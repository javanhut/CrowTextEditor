//! Drawing the editor.
//!
//! The whole screen is redrawn each frame. That is fast enough at terminal
//! sizes and avoids a class of stale-cell bugs; a damage-tracking renderer is
//! an optimisation to make later, once there is something to measure.

use std::io::Write;

use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate,
};
use crossterm::{cursor, queue};
use ropey::RopeSlice;

use crate::editor::{Editor, Mode};
use crate::config::tab_width;
use crate::position::{self};

pub fn render(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    // Synchronized updates make the terminal present the frame atomically
    // instead of painting it cell by cell as the bytes stream in; ignored by
    // terminals that don't support it.
    queue!(out, BeginSynchronizedUpdate, cursor::Hide, cursor::MoveTo(0, 0))?;

    render_tree(editor, out)?;
    render_text(editor, out)?;
    render_status_line(editor, out)?;
    render_command_line(editor, out)?;
    render_prompt_popup(editor, out)?;
    render_tree_prompt(editor, out)?;
    render_picker(editor, out)?;
    render_completion(editor, out)?;

    match editor.screen_cursor() {
        Some((col, row)) => queue!(out, cursor::MoveTo(col, row), cursor::Show)?,
        None => queue!(out, cursor::Hide)?,
    }

    // The cursor shape is the clearest signal of which mode you're in. Some
    // terminals visibly blink the cursor every time the style is set, so only
    // send it when it actually changed.
    let insert = editor.mode == Mode::Insert;
    let style = if insert {
        cursor::SetCursorStyle::SteadyBar
    } else {
        cursor::SetCursorStyle::SteadyBlock
    };
    LAST_CURSOR_INSERT.with(|last| {
        if last.get() != Some(insert) {
            last.set(Some(insert));
            let _ = queue!(out, style);
        }
    });

    queue!(out, EndSynchronizedUpdate)?;
    out.flush()
}

thread_local! {
    /// Whether the last cursor style we sent was the insert-mode bar.
    static LAST_CURSOR_INSERT: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Cell styling: an overlay (selection or extra cursor) plus a syntax color.
/// (The primary cursor is the terminal's own.)
const ST_NONE: u8 = 0;
const ST_SEL: u8 = 1;
const ST_CURSOR: u8 = 2;

/// (overlay, color, BOLD/ITALIC flags) for one cell.
type Style = (u8, Option<Color>, u8);

fn render_text(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let (wins, seps) = editor.window_rects();
    for &(id, rect) in &wins {
        render_window(editor, id, rect, out)?;
    }
    for &((x, y, w, h), vertical) in &seps {
        let theme = crate::theme::current();
        queue!(
            out,
            SetBackgroundColor(theme.bg.unwrap_or(Color::Reset)),
            SetForegroundColor(theme.gutter)
        )?;
        if vertical {
            for row in y..y + h {
                queue!(out, cursor::MoveTo(x, row), Print("│"))?;
            }
        } else {
            queue!(out, cursor::MoveTo(x, y), Print("─".repeat(w as usize)))?;
        }
        queue!(out, ResetColor)?;
    }
    Ok(())
}

fn render_window(
    editor: &Editor,
    id: usize,
    rect: crate::editor::Rect,
    out: &mut impl Write,
) -> std::io::Result<()> {
    let (rx, ry, rw, rh) = rect;
    if rw == 0 || rh == 0 {
        return Ok(());
    }

    let focused = id == editor.focused;
    let win = editor.layout.find(id).expect("window exists");
    // The focused window's live state is in its document; other windows render
    // from their stashed state.
    let (doc, cursor, anchor, extra, view_line, view_col) = if focused {
        let d = editor.doc();
        (d, d.cursor, d.anchor, d.extra.clone(), d.view_line, d.view_col)
    } else {
        let d = &editor.documents[win.doc.min(editor.documents.len() - 1)];
        (d, win.cursor, win.anchor, win.extra.clone(), win.view_line, win.view_col)
    };

    let diags: &[crate::lsp::Diagnostic] = doc
        .path
        .as_ref()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| editor.diagnostics.get(&p))
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    let len = doc.text.len_chars();
    let line_count = doc.line_count();
    let gutter = doc.line_count().to_string().len().max(3) + 1;
    let width = (rw as usize).saturating_sub(gutter);
    let cursor_line = {
        let l = doc.text.char_to_line(cursor.min(len));
        l.min(line_count.saturating_sub(1))
    };

    // Selections and extra cursors only make sense in the focused window.
    let mut sels: Vec<(usize, usize)> = Vec::new();
    let mut curs: Vec<usize> = Vec::new();
    if focused {
        for &(a, c) in std::iter::once(&(anchor, cursor)).chain(extra.iter()) {
            if a != c {
                sels.push((a.min(c).min(len), a.max(c).min(len)));
            }
        }
        curs = extra.iter().map(|&(_, c)| c.min(len)).collect();
    }

    let theme = crate::theme::current();
    // Reset foreground only (`Color::Reset`) so the theme background, once
    // set for a row, survives every color change within it.
    let base_bg = theme.bg.unwrap_or(Color::Reset);
    let base_fg = theme.fg.unwrap_or(Color::Reset);

    for row in 0..rh as usize {
        let line_idx = view_line + row;
        queue!(
            out,
            cursor::MoveTo(rx, ry + row as u16),
            ResetColor,
            SetBackgroundColor(base_bg)
        )?;

        if line_idx >= line_count {
            queue!(
                out,
                SetForegroundColor(theme.gutter),
                Print("~"),
                Print(" ".repeat((rw as usize).saturating_sub(1))),
                ResetColor
            )?;
            continue;
        }

        let number = format!("{:>width$} ", line_idx + 1, width = gutter - 1);
        // A diagnostic on the line colors its number: red error, yellow warning.
        let diag_color = diags
            .iter()
            .filter(|d| d.line == line_idx)
            .map(|d| d.severity)
            .min()
            .map(|s| if s == 1 { Color::Red } else { Color::Yellow });
        queue!(
            out,
            SetForegroundColor(diag_color.unwrap_or(if line_idx == cursor_line && focused {
                theme.gutter_cursor
            } else {
                theme.gutter
            })),
            Print(number)
        )?;

        let ls = doc.line_start(line_idx);
        let line_len = doc.line_len(line_idx);
        let spans = doc
            .syntax
            .as_ref()
            .map(|s| s.spans.as_slice())
            .unwrap_or(&[]);
        let style_of = |off: usize| -> Style {
            let p = ls + off;
            let overlay = if curs.contains(&p) {
                ST_CURSOR
            } else if sels.iter().any(|&(f, t)| f <= p && p < t) {
                ST_SEL
            } else {
                ST_NONE
            };
            let group = crate::syntax::group_at(spans, p);
            let fg = crate::syntax::color(group);
            (overlay, fg, crate::syntax::attrs(group))
        };

        let runs = styled_visible(doc.line(line_idx), line_len, view_col, width, style_of);
        let mut printed = 0usize;
        for ((overlay, fg, attrs), s) in runs {
            printed += display_width(&s);
            queue!(out, SetForegroundColor(fg.unwrap_or(base_fg)))?;
            if attrs & crate::syntax::BOLD != 0 {
                queue!(out, SetAttribute(Attribute::Bold))?;
            }
            if attrs & crate::syntax::ITALIC != 0 {
                queue!(out, SetAttribute(Attribute::Italic))?;
            }
            match overlay {
                ST_SEL => queue!(out, SetBackgroundColor(theme.selection))?,
                ST_CURSOR => queue!(out, SetAttribute(Attribute::Reverse))?,
                _ => {}
            }
            queue!(out, Print(s))?;
            match overlay {
                ST_SEL => queue!(out, SetBackgroundColor(base_bg))?,
                ST_CURSOR => queue!(out, SetAttribute(Attribute::NoReverse))?,
                _ => {}
            }
            if attrs & crate::syntax::BOLD != 0 {
                queue!(out, SetAttribute(Attribute::NormalIntensity))?;
            }
            if attrs & crate::syntax::ITALIC != 0 {
                queue!(out, SetAttribute(Attribute::NoItalic))?;
            }
        }
        // Pad to the window edge; UntilNewLine would bleed into a neighbour.
        if printed < width {
            queue!(out, Print(" ".repeat(width - printed)))?;
        }
        queue!(out, ResetColor)?;
    }

    Ok(())
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

/// Render the portion of a line inside the horizontal viewport as runs of
/// equally-styled text, where `style_of` maps a char offset within the line to
/// its style.
///
/// Tabs expand to spaces, and wide characters straddling either edge of the
/// viewport are replaced with spaces rather than being cut in half — a
/// half-drawn wide character corrupts every column after it. The newline gets
/// one extra column when styled, so a selected line ending or a cursor sitting
/// on it is visible.
fn styled_visible(
    line: RopeSlice,
    line_len: usize,
    view_col: usize,
    width: usize,
    style_of: impl Fn(usize) -> Style,
) -> Vec<(Style, String)> {
    let mut runs: Vec<(Style, String)> = Vec::new();
    let push = |runs: &mut Vec<(Style, String)>, style: Style, piece: char| match runs.last_mut() {
        Some((last, buf)) if *last == style => buf.push(piece),
        _ => runs.push((style, piece.to_string())),
    };

    let right_edge = view_col + width;
    let mut col = 0usize;

    for (i, c) in line.chars().enumerate() {
        if c == '\n' || c == '\r' {
            break;
        }

        let start = col;
        let w = position::char_width(c, col, tab_width());
        col += w;

        if col <= view_col {
            continue; // entirely left of the viewport
        }
        if start >= right_edge {
            break; // entirely right of it
        }

        let style = style_of(i);
        if c == '\t' || start < view_col || col > right_edge {
            let from = start.max(view_col);
            let to = col.min(right_edge);
            for _ in from..to {
                push(&mut runs, style, ' ');
            }
        } else {
            push(&mut runs, style, c);
        }
    }

    if col >= view_col && col < right_edge {
        let style = style_of(line_len);
        if style.0 != ST_NONE {
            push(&mut runs, style, ' ');
        }
    }

    runs
}

fn render_status_line(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let row = editor.size.1.saturating_sub(2);
    let width = editor.size.0 as usize;
    let doc = editor.doc();

    let label = if editor.extend && editor.mode == Mode::Normal {
        "SEL"
    } else {
        editor.mode.label()
    };
    let left = format!(
        " {}  {}{} ",
        label,
        doc.name(),
        if doc.modified { " [+]" } else { "" }
    );

    let reg = match (editor.awaiting_register, editor.active_register) {
        (true, _) => "\"".to_string(),
        (_, Some(c)) => format!("\"{c}"),
        _ => String::new(),
    };
    let pending: String = editor.pending.iter().map(|k| k.display()).collect();
    let count = format!("{}{}", reg, editor.count.map(|n| n.to_string()).unwrap_or_default());
    let cursors = match doc.extra.len() {
        0 => String::new(),
        n => format!("{} cursors  ", n + 1),
    };

    let right = format!(
        " {}{}  {}{}  {}/{} ",
        count,
        pending,
        cursors,
        editor.cursor_indicator(),
        editor.current + 1,
        editor.documents.len()
    );

    let padding = width.saturating_sub(left.chars().count() + right.chars().count());
    let line = format!("{left}{}{right}", " ".repeat(padding));
    let line: String = line.chars().take(width).collect();

    queue!(
        out,
        cursor::MoveTo(0, row),
        SetAttribute(Attribute::Reverse),
        Print(line),
        SetAttribute(Attribute::Reset),
        Clear(ClearType::UntilNewLine)
    )
}

/// The file tree sidebar: indented rows, `▸`/`▾` on directories, the
/// selected row reversed while the tree has focus.
fn render_tree(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let Some(tree) = &editor.tree else {
        return Ok(());
    };
    let w = editor.tree_width() as usize;
    if w < 4 {
        return Ok(());
    }
    let inner = w - 1; // last column is the separator
    let height = editor.size.1.saturating_sub(2) as usize;
    let theme = crate::theme::current();

    let start = tree
        .selected
        .saturating_sub(height.saturating_sub(1).min(tree.selected));
    for row in 0..height {
        queue!(
            out,
            cursor::MoveTo(0, row as u16),
            ResetColor,
            SetBackgroundColor(theme.bg.unwrap_or(Color::Reset))
        )?;
        let line = match tree.rows.get(start + row) {
            Some(r) => {
                let marker = if r.is_dir {
                    if r.expanded {
                        "▾ "
                    } else {
                        "▸ "
                    }
                } else {
                    "  "
                };
                let icon = if crate::config::icons() {
                    format!("{} ", file_icon(&r.name, r.is_dir, r.expanded))
                } else {
                    String::new()
                };
                format!("{}{}{}{}", " ".repeat(r.depth), marker, icon, r.name)
            }
            None => String::new(),
        };
        let line: String = line.chars().take(inner).collect();
        if start + row == tree.selected && editor.tree_focused {
            queue!(
                out,
                SetAttribute(Attribute::Reverse),
                Print(format!("{line:<inner$}")),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            // The root header gets the accent color; other dirs the gutter
            // highlight.
            if start + row == 0 {
                queue!(out, SetForegroundColor(theme.border))?;
            } else if tree.rows.get(start + row).is_some_and(|r| r.is_dir) {
                queue!(out, SetForegroundColor(theme.gutter_cursor))?;
            }
            queue!(out, Print(format!("{line:<inner$}")), ResetColor)?;
        }
        queue!(
            out,
            SetBackgroundColor(theme.bg.unwrap_or(Color::Reset)),
            SetForegroundColor(theme.gutter),
            Print("│"),
            ResetColor
        )?;
    }
    Ok(())
}

/// The popup picker: a reversed title/query row, then the filtered list.
fn render_picker(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let Some(picker) = &editor.picker else {
        return Ok(());
    };
    let (x, y, w, h) = editor.picker_rect();
    let w = w as usize;

    let title = format!(" {} ▸ {}", picker.title, picker.query);
    let title: String = title.chars().take(w).collect();
    queue!(
        out,
        cursor::MoveTo(x, y),
        SetAttribute(Attribute::Reverse),
        Print(format!("{title:<w$}")),
        SetAttribute(Attribute::Reset)
    )?;

    let list_h = (h as usize).saturating_sub(1);
    let start = picker
        .selected
        .saturating_sub(list_h.saturating_sub(1).min(picker.selected));
    let start = start.min(picker.filtered.len().saturating_sub(list_h).max(0));
    let popup_bg = crate::theme::current().popup_bg;

    for row in 0..list_h {
        queue!(out, cursor::MoveTo(x, y + 1 + row as u16))?;
        let line = match picker.filtered.get(start + row) {
            Some(&idx) => {
                let item = &picker.items[idx];
                let mut line = format!(" {}", item.label);
                if !item.detail.is_empty() {
                    line.push_str("  · ");
                    line.push_str(&item.detail);
                }
                line
            }
            None => String::new(),
        };
        let line: String = line.chars().take(w).collect();
        if start + row == picker.selected && !picker.filtered.is_empty() {
            queue!(
                out,
                SetAttribute(Attribute::Reverse),
                Print(format!("{line:<w$}")),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(
                out,
                SetBackgroundColor(popup_bg),
                Print(format!("{line:<w$}")),
                ResetColor
            )?;
        }
    }
    Ok(())
}

/// The completion menu: a small list anchored at the cursor.
fn render_completion(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let Some(completion) = &editor.completion else {
        return Ok(());
    };
    if editor.mode != Mode::Insert {
        return Ok(());
    }
    let Some((cx, cy)) = editor.screen_cursor() else {
        return Ok(());
    };

    let shown = completion.items.len().min(8);
    let width = completion
        .items
        .iter()
        .map(|(label, _)| label.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(8, 40)
        + 2;
    let text_rows = editor.size.1.saturating_sub(2);
    // Below the cursor when there is room, above otherwise.
    let top = if cy + 1 + (shown as u16) <= text_rows {
        cy + 1
    } else {
        cy.saturating_sub(shown as u16)
    };
    let x = cx.min(editor.size.0.saturating_sub(width as u16));

    let start = completion
        .selected
        .saturating_sub(shown.saturating_sub(1).min(completion.selected));
    let popup_bg = crate::theme::current().popup_bg;

    for row in 0..shown {
        let Some((label, _)) = completion.items.get(start + row) else {
            break;
        };
        let line: String = format!(" {label}").chars().take(width).collect();
        queue!(out, cursor::MoveTo(x, top + row as u16))?;
        if start + row == completion.selected {
            queue!(
                out,
                SetAttribute(Attribute::Reverse),
                Print(format!("{line:<width$}")),
                SetAttribute(Attribute::Reset)
            )?;
        } else {
            queue!(
                out,
                SetBackgroundColor(popup_bg),
                Print(format!("{line:<width$}")),
                ResetColor
            )?;
        }
    }
    Ok(())
}

/// The bottom line: status messages and the cursor line's diagnostic. The
/// `:` and `/` prompts live in the floating popup, not here.
fn render_command_line(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let row = editor.size.1.saturating_sub(1);
    let width = editor.size.0 as usize;

    let mut content = editor.status.clone();
    // With nothing else to say, surface the diagnostic under the cursor.
    if content.is_empty() {
        let doc = editor.doc();
        if let Some(d) = doc
            .path
            .as_ref()
            .and_then(|p| p.canonicalize().ok())
            .and_then(|p| editor.diagnostics.get(&p))
            .and_then(|v| v.iter().find(|d| d.line == doc.cursor_line()))
        {
            content = format!("● {}", d.message);
        }
    }
    let content: String = content.chars().take(width).collect();
    let theme = crate::theme::current();

    queue!(
        out,
        cursor::MoveTo(0, row),
        ResetColor,
        SetBackgroundColor(theme.bg.unwrap_or(Color::Reset)),
        SetForegroundColor(theme.fg.unwrap_or(Color::Reset)),
        Print(format!("{content:<width$}")),
        ResetColor
    )
}

/// Nerd Font glyph for a tree row. Needs a Nerd Font in the terminal
/// (`icons = false` in crow.toml otherwise).
fn file_icon(name: &str, is_dir: bool, expanded: bool) -> &'static str {
    if is_dir {
        return if expanded { "\u{f07c}" } else { "\u{f07b}" }; // open/closed folder
    }
    match name.rsplit('.').next().unwrap_or("") {
        "rs" => "\u{e7a8}",
        "toml" => "\u{e615}",
        "md" => "\u{f48a}",
        "json" => "\u{e60b}",
        "sh" | "bash" | "zsh" => "\u{f489}",
        "py" => "\u{e73c}",
        "js" | "jsx" | "mjs" => "\u{e74e}",
        "ts" | "tsx" => "\u{e628}",
        "c" | "h" => "\u{e61e}",
        "cc" | "cpp" | "hpp" => "\u{e61d}",
        "go" => "\u{e626}",
        "html" => "\u{e736}",
        "css" => "\u{e749}",
        "yml" | "yaml" => "\u{e615}",
        "lock" => "\u{f023}",
        "txt" => "\u{f15c}",
        _ => "\u{f15b}", // generic file
    }
}

/// A rounded, bordered one-line popup with its title in the top border —
/// the NvCrow-style floating bar.
fn draw_popup(
    out: &mut impl Write,
    x: u16,
    y: u16,
    w: u16,
    title: &str,
    content: &str,
) -> std::io::Result<()> {
    let inner = (w as usize).saturating_sub(2);
    let theme = crate::theme::current();
    let content: String = content.chars().take(inner).collect();
    queue!(
        out,
        cursor::MoveTo(x, y),
        ResetColor,
        SetBackgroundColor(theme.popup_bg),
        SetForegroundColor(theme.border),
        Print(format!("╭{title:─^inner$}╮")),
        cursor::MoveTo(x, y + 1),
        Print("│"),
        SetForegroundColor(theme.fg.unwrap_or(Color::Reset)),
        Print(format!("{content:<inner$}")),
        SetForegroundColor(theme.border),
        Print("│"),
        cursor::MoveTo(x, y + 2),
        Print(format!("╰{}╯", "─".repeat(inner))),
        ResetColor
    )
}

/// The `:` and `/` prompts as a floating popup.
fn render_prompt_popup(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let title = match editor.mode {
        Mode::Command => " Command ",
        Mode::Search if editor.search_select => " Select ",
        Mode::Search => " Search ",
        _ => return Ok(()),
    };
    let (x, y, w) = editor.prompt_rect();
    let prefix = if editor.mode == Mode::Command { ':' } else { '/' };
    draw_popup(out, x, y, w, title, &format!(" {prefix}{}", editor.command_line))
}

/// The tree's create/delete prompts, in the same popup style.
fn render_tree_prompt(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let Some(input) = &editor.tree_input else {
        return Ok(());
    };
    let (x, y, w) = editor.prompt_rect();
    match input {
        crate::editor::TreeInput::Create { dir, name } => {
            let where_ = dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| dir.to_string_lossy().into_owned());
            draw_popup(
                out,
                x,
                y,
                w,
                &format!(" New in {where_}/ "),
                &format!(" {name}"),
            )
        }
        crate::editor::TreeInput::Rename { name, .. } => {
            draw_popup(out, x, y, w, " Rename ", &format!(" {name}"))
        }
        crate::editor::TreeInput::Delete { path } => {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let kind = if path.is_dir() {
                "directory and contents"
            } else {
                "file"
            };
            draw_popup(
                out,
                x,
                y,
                w,
                " Delete ",
                &format!(" delete {kind} {name:?}? (y/n)"),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    fn plain(line: RopeSlice, view_col: usize, width: usize) -> String {
        let len = position::line_len_without_newline(line);
        styled_visible(line, len, view_col, width, |_| (ST_NONE, None, 0))
            .into_iter()
            .map(|(_, s)| s)
            .collect()
    }

    #[test]
    fn horizontal_scroll_drops_text_left_of_the_viewport() {
        let rope = Rope::from_str("abcdefghij");
        assert_eq!(plain(rope.line(0), 3, 4), "defg");
    }

    #[test]
    fn tabs_render_as_spaces() {
        let rope = Rope::from_str("\tx");
        assert_eq!(plain(rope.line(0), 0, 10), "    x");
    }

    #[test]
    fn wide_char_split_by_the_edge_becomes_a_space() {
        // The viewport ends mid-character, so a space is drawn instead of half
        // of a double-width glyph.
        let rope = Rope::from_str("日本");
        assert_eq!(plain(rope.line(0), 0, 3), "日 ");
    }

    #[test]
    fn newline_is_not_rendered() {
        let rope = Rope::from_str("ab\ncd");
        assert_eq!(plain(rope.line(0), 0, 10), "ab");
    }

    #[test]
    fn styles_split_a_line_into_runs() {
        let rope = Rope::from_str("abcdef");
        // Select chars 2..4.
        let runs = styled_visible(rope.line(0), 6, 0, 10, |i| {
            if (2..4).contains(&i) {
                (ST_SEL, None, 0)
            } else {
                (ST_NONE, None, 0)
            }
        });
        assert_eq!(
            runs,
            vec![
                ((ST_NONE, None, 0), "ab".to_string()),
                ((ST_SEL, None, 0), "cd".to_string()),
                ((ST_NONE, None, 0), "ef".to_string()),
            ]
        );
    }

    #[test]
    fn styled_newline_shows_as_one_column() {
        let rope = Rope::from_str("ab\ncd");
        // The whole line plus its newline is selected.
        let runs = styled_visible(rope.line(0), 2, 0, 10, |_| (ST_SEL, None, 0));
        assert_eq!(runs, vec![((ST_SEL, None, 0), "ab ".to_string())]);
    }
}
