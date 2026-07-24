//! Drawing the editor.
//!
//! The whole screen is redrawn each frame. That is fast enough at terminal
//! sizes and avoids a class of stale-cell bugs; a damage-tracking renderer is
//! an optimisation to make later, once there is something to measure.

use std::io::Write;

use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{cursor, queue};
use ropey::RopeSlice;

use crate::config::tab_width;
use crate::editor::{Editor, Mode};
use crate::position::{self};

pub fn render(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    // Synchronized updates make the terminal present the frame atomically
    // instead of painting it cell by cell as the bytes stream in; ignored by
    // terminals that don't support it.
    queue!(
        out,
        BeginSynchronizedUpdate,
        cursor::Hide,
        cursor::MoveTo(0, 0)
    )?;

    render_tree(editor, out)?;
    render_text(editor, out)?;
    render_splash(editor, out)?;
    render_status_line(editor, out)?;
    render_command_line(editor, out)?;
    render_prompt_popup(editor, out)?;
    render_tree_prompt(editor, out)?;
    render_picker(editor, out)?;
    render_completion(editor, out)?;
    render_hover(editor, out)?;
    render_pending_keys(editor, out)?;
    render_help(editor, out)?;

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
        if editor.show_splash() {
            // The splash owns the whole area: no gutter, no `~` rows.
            let (x, y, w, h) = rect;
            let theme = crate::theme::current();
            queue!(out, SetBackgroundColor(theme.bg.unwrap_or(Color::Reset)))?;
            for row in y..y + h {
                queue!(out, cursor::MoveTo(x, row), Print(" ".repeat(w as usize)))?;
            }
            queue!(out, ResetColor)?;
        } else {
            render_window(editor, id, rect, out)?;
        }
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
        (
            d,
            d.cursor,
            d.anchor,
            d.extra.clone(),
            d.view_line,
            d.view_col,
        )
    } else {
        let d = &editor.documents[win.doc.min(editor.documents.len() - 1)];
        (
            d,
            win.cursor,
            win.anchor,
            win.extra.clone(),
            win.view_line,
            win.view_col,
        )
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

    let manifest_kind = (!editor.dep_info.is_empty())
        .then(|| {
            doc.path
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .and_then(crate::deps::manifest_kind)
        })
        .flatten();

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
        // Package manifests: the dependency's current version, and the newer
        // one on its registry when there is one — like the diagnostics,
        // virtual text.
        if let Some(kind) = manifest_kind {
            let head: String = doc.line(line_idx).chars().take(200).collect();
            let dep = crate::deps::line_dep(kind, &head)
                .and_then(|name| editor.dep_info.get(&(kind, name)));
            if let Some((locked, latest)) = dep {
                let mut avail = width.saturating_sub(printed);
                let mut badge = |text: String, color: Color| -> std::io::Result<()> {
                    let w = display_width(&text);
                    if w < avail {
                        avail -= w;
                        printed += w;
                        queue!(out, SetForegroundColor(color), Print(text))?;
                    }
                    Ok(())
                };
                if let Some(l) = locked {
                    badge(format!("  ✓ {l}"), Color::Cyan)?;
                }
                let newer = match (locked, latest) {
                    (Some(l), Some(n)) => crate::deps::semver_key(n) > crate::deps::semver_key(l),
                    (None, Some(_)) => true,
                    _ => false,
                };
                if newer {
                    badge(format!("  ↑ {}", latest.as_deref().unwrap()), Color::Yellow)?;
                }
            }
        }

        // The line's worst diagnostic, inline after the text (virtual text).
        let inline = diags
            .iter()
            .filter(|d| d.line == line_idx)
            .min_by_key(|d| d.severity);
        if let Some(d) = inline {
            let avail = width.saturating_sub(printed);
            if avail > 8 {
                let mut text = String::from("  ■ ");
                let mut w = display_width(&text);
                for ch in d.message.chars() {
                    let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                    if w + cw > avail {
                        break;
                    }
                    text.push(ch);
                    w += cw;
                }
                printed += w;
                let color = if d.severity == 1 {
                    Color::Red
                } else {
                    Color::Yellow
                };
                queue!(out, SetForegroundColor(color), Print(text))?;
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

/// The status line, NvCrow-style: a colored mode segment with a powerline
/// chevron on the left, the file beside it, and chevroned pills for cursor
/// position and progress on the right. Without icons the chevrons vanish
/// and the segments stand on their colors alone.
fn render_status_line(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let row = editor.size.1.saturating_sub(2);
    let width = editor.size.0 as usize;
    let doc = editor.doc();
    let theme = crate::theme::current();

    let (label, accent) = if editor.extend && editor.mode == Mode::Normal {
        ("SELECT", Color::Magenta)
    } else {
        match editor.mode {
            Mode::Normal => ("NORMAL", Color::Blue),
            Mode::Insert => ("INSERT", Color::Green),
            Mode::Command => ("COMMAND", Color::Yellow),
            Mode::Search => ("SEARCH", Color::Yellow),
            Mode::Picker => ("PICKER", Color::Yellow),
        }
    };
    let (lsep, rsep) = if crate::config::icons() {
        ("\u{e0b0}", "\u{e0b2}")
    } else {
        ("", "")
    };
    let bar_bg = theme.popup_bg;
    let fg = theme.fg.unwrap_or(Color::Reset);

    // Base coat, then segments over it.
    queue!(
        out,
        cursor::MoveTo(0, row),
        ResetColor,
        SetBackgroundColor(bar_bg),
        Print(" ".repeat(width))
    )?;

    let file = format!(" {}{}", doc.name(), if doc.modified { " [+]" } else { "" });
    queue!(
        out,
        cursor::MoveTo(0, row),
        SetAttribute(Attribute::Bold),
        SetBackgroundColor(accent),
        SetForegroundColor(Color::Black),
        Print(format!(" {label} ")),
        SetAttribute(Attribute::Reset),
        SetBackgroundColor(bar_bg),
        SetForegroundColor(accent),
        Print(lsep),
        SetForegroundColor(fg),
        Print(&file)
    )?;

    // Diagnostic counts for this buffer: ● errors, ▲ warnings.
    let (errs, warns) = doc
        .path
        .as_ref()
        .and_then(|p| p.canonicalize().ok())
        .and_then(|p| editor.diagnostics.get(&p))
        .map(|ds| {
            ds.iter().fold((0usize, 0usize), |(e, w), d| {
                if d.severity == 1 {
                    (e + 1, w)
                } else {
                    (e, w + 1)
                }
            })
        })
        .unwrap_or((0, 0));
    let mut diag_w = 0;
    if errs > 0 {
        let s = format!(" ● {errs}");
        diag_w += s.chars().count();
        queue!(out, SetForegroundColor(Color::Red), Print(s))?;
    }
    if warns > 0 {
        let s = format!(" ▲ {warns}");
        diag_w += s.chars().count();
        queue!(out, SetForegroundColor(Color::Yellow), Print(s))?;
    }

    // Right side: transient input state, then the pills.
    let reg = match (editor.awaiting_register, editor.active_register) {
        (true, _) => "\"".to_string(),
        (_, Some(c)) => format!("\"{c}"),
        _ => String::new(),
    };
    let pending: String = editor
        .pending
        .iter()
        .map(|k| k.display())
        .collect::<Vec<_>>()
        .join(" ");
    let count = editor.count.map(|n| n.to_string()).unwrap_or_default();
    let cursors = match doc.extra.len() {
        0 => String::new(),
        n => format!("{} cursors  ", n + 1),
    };
    let mut info = format!("{cursors}{reg}{count}{pending}");
    if !info.is_empty() {
        info.push_str("  ");
    }

    let pos = format!(
        " {}  {}/{} ",
        editor.cursor_indicator(),
        editor.current + 1,
        editor.documents.len()
    );
    let (line, _) = doc.cursor_line_col();
    let pct = format!(
        " {}% ",
        ((line + 1) * 100 / doc.line_count().max(1)).min(100)
    );

    let left_w = 2 + label.chars().count() + lsep.chars().count() + file.chars().count() + diag_w;
    let right_w =
        info.chars().count() + rsep.chars().count() * 2 + pos.chars().count() + pct.chars().count();
    if left_w + right_w < width {
        queue!(
            out,
            cursor::MoveTo((width - right_w) as u16, row),
            SetBackgroundColor(bar_bg),
            SetForegroundColor(fg),
            Print(&info),
            SetForegroundColor(theme.selection),
            Print(rsep),
            SetBackgroundColor(theme.selection),
            SetForegroundColor(fg),
            Print(&pos),
            SetForegroundColor(accent),
            Print(rsep),
            SetAttribute(Attribute::Bold),
            SetBackgroundColor(accent),
            SetForegroundColor(Color::Black),
            Print(&pct),
            SetAttribute(Attribute::Reset)
        )?;
    }
    queue!(out, ResetColor)
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
        let mut printed = 0usize;
        if let Some(r) = tree.rows.get(start + row) {
            let idx = start + row;
            let indent = " ".repeat(r.depth);
            let marker = if r.is_dir {
                match (crate::config::icons(), r.expanded) {
                    (true, true) => "\u{f107} ",  //  angle-down
                    (true, false) => "\u{f105} ", //  angle-right
                    (false, true) => "▾ ",
                    (false, false) => "▸ ",
                }
            } else {
                "  "
            };
            let (icon, icon_color) = file_icon(&r.name, r.is_dir, r.expanded);
            let icon = if crate::config::icons() {
                format!("{icon} ")
            } else {
                String::new()
            };
            if idx == tree.selected && editor.tree_focused {
                let line: String = format!("{indent}{marker}{icon}{}", r.name)
                    .chars()
                    .take(inner)
                    .collect();
                queue!(
                    out,
                    SetAttribute(Attribute::Reverse),
                    Print(format!("{line:<inner$}")),
                    SetAttribute(Attribute::Reset),
                    SetBackgroundColor(theme.bg.unwrap_or(Color::Reset))
                )?;
                printed = inner;
            } else {
                // Root header in the accent, directories highlighted, files
                // plain — with the icon in its filetype color.
                let name_color = if idx == 0 {
                    theme.border
                } else if r.is_dir {
                    theme.gutter_cursor
                } else {
                    theme.fg.unwrap_or(Color::Reset)
                };
                let mut budget = inner;
                let take = |s: &str, budget: &mut usize| -> String {
                    let t: String = s.chars().take(*budget).collect();
                    *budget -= t.chars().count();
                    t
                };
                let lead = take(&format!("{indent}{marker}"), &mut budget);
                let icon = take(&icon, &mut budget);
                let name = take(&r.name, &mut budget);
                queue!(
                    out,
                    SetForegroundColor(theme.gutter),
                    Print(lead),
                    SetForegroundColor(icon_color),
                    Print(icon),
                    SetForegroundColor(name_color),
                    Print(name)
                )?;
                printed = inner - budget;
            }
        }
        queue!(
            out,
            Print(" ".repeat(inner - printed)),
            SetForegroundColor(theme.gutter),
            Print("│"),
            ResetColor
        )?;
    }
    Ok(())
}

/// The popup picker: a reversed title/query row, then the filtered list.
/// Pickers share the prompt popup's bordered look: title in the top border,
/// the query where the command line sits, then the list in the same walls.
fn render_picker(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let Some(picker) = &editor.picker else {
        return Ok(());
    };
    let (x, y, w, h) = editor.picker_rect();
    draw_popup(
        out,
        x,
        y,
        w,
        &format!(" {} ", picker.title),
        &format!(" ▸ {}", picker.query),
    )?;

    let list_h = (h as usize).saturating_sub(4).max(1);
    let start = picker
        .selected
        .saturating_sub(list_h.saturating_sub(1).min(picker.selected));
    let start = start.min(picker.filtered.len().saturating_sub(list_h));
    let end = (start + list_h).min(picker.filtered.len());
    let rows: Vec<String> = picker.filtered[start..end]
        .iter()
        .map(|&idx| {
            let item = &picker.items[idx];
            if item.detail.is_empty() {
                format!(" {}", item.label)
            } else {
                format!(" {}  · {}", item.label, item.detail)
            }
        })
        .collect();
    let selected = (!picker.filtered.is_empty()).then(|| picker.selected - start);
    draw_box_list(out, x, y + 2, w, &rows, selected)
}

/// The start screen: a logo and the keys worth knowing, drawn over the
/// untouched startup buffer. Purely informative — every key acts normally.
fn render_splash(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    if !editor.show_splash() {
        return Ok(());
    }
    const LOGO: &[&str] = &[
        " ██████╗██████╗  ██████╗ ██╗    ██╗",
        "██╔════╝██╔══██╗██╔═══██╗██║    ██║",
        "██║     ██████╔╝██║   ██║██║ █╗ ██║",
        "██║     ██╔══██╗██║   ██║██║███╗██║",
        "╚██████╗██║  ██║╚██████╔╝╚███╔███╔╝",
        " ╚═════╝╚═╝  ╚═╝ ╚═════╝  ╚══╝╚══╝ ",
    ];
    const ENTRIES: &[(&str, &str)] = &[
        ("Find file", "find_files"),
        ("Recent files", "recent_files"),
        ("Grep text", "grep_text"),
        ("File tree", "tree_toggle"),
        ("Command palette", "command_palette"),
        ("Pick theme", "theme_picker"),
        ("Quit", "quit"),
    ];
    let tagline = concat!(
        "crow v",
        env!("CARGO_PKG_VERSION"),
        "  ·  :help for everything"
    );

    let (wx, wy, ww, wh) = editor.focused_rect();
    let total = LOGO.len() + 1 + ENTRIES.len() + 1 + 1;
    let logo_w = LOGO[0].chars().count();
    if (wh as usize) < total + 2 || (ww as usize) < logo_w + 2 {
        return Ok(()); // window too small for decoration
    }
    let theme = crate::theme::current();
    // Text cells must sit on the same background the area was filled with.
    queue!(out, SetBackgroundColor(theme.bg.unwrap_or(Color::Reset)))?;
    let mut y = wy + ((wh as usize - total) / 2) as u16;

    for row in LOGO {
        let x = wx + ((ww as usize - logo_w) / 2) as u16;
        queue!(
            out,
            cursor::MoveTo(x, y),
            SetForegroundColor(theme.border),
            Print(row)
        )?;
        y += 1;
    }
    y += 1;

    let entry_w = 32;
    let x = wx + ((ww as usize).saturating_sub(entry_w) / 2) as u16;
    for (label, command) in ENTRIES {
        let keys = editor
            .keymaps
            .normal
            .binding_of(command)
            .unwrap_or_default();
        let (icon, color) = leader_hint(command);
        let icon = if crate::config::icons() {
            format!("{icon} ")
        } else {
            String::new()
        };
        queue!(
            out,
            cursor::MoveTo(x, y),
            SetForegroundColor(color),
            Print(icon),
            SetForegroundColor(theme.fg.unwrap_or(Color::Reset)),
            Print(format!("{label:<17}")),
            SetForegroundColor(color),
            Print(format!("{keys:>12}"))
        )?;
        y += 1;
    }
    y += 1;

    let x = wx + ((ww as usize).saturating_sub(tagline.chars().count()) / 2) as u16;
    queue!(
        out,
        cursor::MoveTo(x, y),
        SetForegroundColor(theme.gutter),
        SetAttribute(Attribute::Italic),
        Print(tagline),
        SetAttribute(Attribute::Reset),
        ResetColor
    )
}

/// Which-key: a pending multi-key sequence (the space leader, C-w, g…)
/// shows its continuations in a panel above the status line, column-major
/// like which-key: `key → command`.
fn render_pending_keys(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    if editor.mode != Mode::Normal || editor.pending.is_empty() {
        return Ok(());
    }
    let entries = editor.keymaps.normal.continuations(&editor.pending);
    if entries.is_empty() {
        return Ok(());
    }
    let (w, h) = editor.size;
    let rows = entries.len().div_ceil(3);
    let cols = entries.len().div_ceil(rows);
    let col_w = (w as usize) / cols.max(1);
    let icons = crate::config::icons();
    let name_w = col_w.saturating_sub(if icons { 15 } else { 13 });
    let theme = crate::theme::current();
    let top = h.saturating_sub(2).saturating_sub(rows as u16);
    for r in 0..rows {
        let y = top + r as u16;
        queue!(
            out,
            cursor::MoveTo(0, y),
            SetBackgroundColor(theme.popup_bg),
            Print(" ".repeat(w as usize))
        )?;
        for c in 0..cols {
            let Some((key, name)) = entries.get(c * rows + r) else {
                continue;
            };
            let (icon, color) = leader_hint(name);
            let icon = if icons {
                format!("{icon} ")
            } else {
                String::new()
            };
            let name: String = name.chars().take(name_w).collect();
            queue!(
                out,
                cursor::MoveTo((c * col_w) as u16 + 1, y),
                SetAttribute(Attribute::Bold),
                SetForegroundColor(color),
                Print(format!("{key:>8}")),
                SetAttribute(Attribute::Reset),
                SetBackgroundColor(theme.popup_bg),
                SetForegroundColor(theme.gutter),
                Print(" → "),
                SetForegroundColor(color),
                Print(icon),
                SetForegroundColor(theme.fg.unwrap_or(Color::Reset)),
                Print(name)
            )?;
        }
        queue!(out, ResetColor)?;
    }
    Ok(())
}

/// Icon and accent color for a leader-bar entry. Colors come from the
/// current theme's syntax palette so every theme keeps its own look.
fn leader_hint(name: &str) -> (&'static str, Color) {
    let theme = crate::theme::current();
    let syn = |i: usize, fallback: Color| theme.syntax[i].unwrap_or(fallback);
    match name {
        "…" => ("\u{f101}", theme.border),                      //  group
        "save" => ("\u{f0c7}", syn(2, Color::Green)),           //  floppy
        "quit" => ("\u{f011}", Color::Red),                     //  power
        "find_files" => ("\u{f002}", syn(4, Color::Cyan)),      //  search
        "grep_text" => ("\u{f15c}", syn(2, Color::Green)),      //  file-text
        "recent_files" => ("\u{f017}", syn(7, Color::Blue)),    //  clock
        "file_explorer" => ("\u{f07b}", syn(5, Color::Yellow)), //  folder
        "tree_toggle" => ("\u{f07c}", syn(5, Color::Yellow)),   //  open folder
        "command_palette" => ("\u{f489}", syn(3, Color::Magenta)), //  terminal
        "theme_picker" => ("\u{f043}", syn(7, Color::Blue)),    //  tint
        n if n.starts_with("split") => ("\u{f0db}", syn(4, Color::Cyan)), //  columns
        _ => ("\u{f013}", syn(6, Color::DarkYellow)),           //  gear
    }
}

/// The `:help` window: the shared bordered box, with keys, command, and
/// description in aligned, individually colored columns.
fn render_help(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    use crate::commands::HelpLine;
    let Some(scroll) = editor.help_scroll else {
        return Ok(());
    };
    let (x, y, w, h) = editor.help_rect();
    let lines = crate::commands::help_lines(&editor.keymaps.normal);
    let visible = (h as usize).saturating_sub(4).max(1);
    let hint = format!(
        " j/k scroll · esc close · {}–{}/{}",
        scroll + 1,
        (scroll + visible).min(lines.len()),
        lines.len()
    );
    draw_popup(out, x, y, w, " Help ", &hint)?;

    let inner = (w as usize).saturating_sub(2);
    let theme = crate::theme::current();
    let fg = theme.fg.unwrap_or(Color::Reset);
    queue!(
        out,
        cursor::MoveTo(x, y + 2),
        SetBackgroundColor(theme.popup_bg),
        SetForegroundColor(theme.border),
        Print(format!("├{}┤", "─".repeat(inner)))
    )?;
    for (i, line) in lines.iter().skip(scroll).take(visible).enumerate() {
        queue!(
            out,
            cursor::MoveTo(x, y + 3 + i as u16),
            SetForegroundColor(theme.border),
            Print("│")
        )?;
        match line {
            HelpLine::Header(title) => {
                let text: String = format!(" {title}").chars().take(inner).collect();
                queue!(
                    out,
                    SetAttribute(Attribute::Bold),
                    SetForegroundColor(theme.border),
                    Print(format!("{text:<inner$}")),
                    SetAttribute(Attribute::Reset),
                    SetBackgroundColor(theme.popup_bg)
                )?;
            }
            HelpLine::Entry { keys, name, doc } => {
                let mut budget = inner;
                let take = |s: String, budget: &mut usize| -> String {
                    let t: String = s.chars().take(*budget).collect();
                    *budget -= t.chars().count();
                    t
                };
                let keys = take(format!(" {keys:<14} "), &mut budget);
                let name = take(format!("{name:<22} "), &mut budget);
                let doc = take(doc.clone(), &mut budget);
                queue!(
                    out,
                    SetForegroundColor(theme.gutter_cursor),
                    Print(keys),
                    SetForegroundColor(fg),
                    Print(name),
                    SetForegroundColor(theme.gutter),
                    Print(doc),
                    Print(" ".repeat(budget))
                )?;
            }
        }
        queue!(out, SetForegroundColor(theme.border), Print("│"))?;
    }
    queue!(
        out,
        cursor::MoveTo(
            x,
            y + 3 + visible.min(lines.len().saturating_sub(scroll)) as u16
        ),
        Print(format!("╰{}╯", "─".repeat(inner))),
        ResetColor
    )
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
        if completion.navigated && start + row == completion.selected {
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

    // Docs for the highlighted item, in a panel beside the menu — signature
    // and description straight from the language server.
    if completion.navigated {
        let doc = completion
            .items
            .get(completion.selected)
            .and_then(|(label, _)| completion.docs.get(label))
            .filter(|d| !d.is_empty()); // empty = resolve still in flight
        if let Some(doc) = doc {
            let text_w = 44;
            let mut rows = Vec::new();
            for line in doc.lines() {
                wrap_line(line, text_w, &mut rows);
            }
            rows.truncate(12);
            let panel_w = (text_w + 4) as u16;
            let right = x + width as u16;
            let px = if right + panel_w <= editor.size.0 {
                Some(right)
            } else if x >= panel_w {
                Some(x - panel_w)
            } else {
                None // no room on either side
            };
            if let Some(px) = px {
                let top = top.min(editor.size.1.saturating_sub(rows.len() as u16 + 2));
                draw_text_popup(out, px, top, text_w, &rows)?;
            }
        }
    }
    Ok(())
}

/// Word-wrap `line` onto `out` at `width` columns; lines that fit (code
/// examples with their indentation) pass through untouched.
fn wrap_line(line: &str, width: usize, out: &mut Vec<String>) {
    if line.chars().count() <= width {
        out.push(line.to_string());
        return;
    }
    let mut cur = String::new();
    for word in line.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
}

/// A bordered popup of text rows with its top-left corner at (x, y).
fn draw_text_popup(
    out: &mut impl Write,
    x: u16,
    y: u16,
    inner_w: usize,
    rows: &[String],
) -> std::io::Result<()> {
    let theme = crate::theme::current();
    queue!(
        out,
        cursor::MoveTo(x, y),
        SetBackgroundColor(theme.popup_bg),
        SetForegroundColor(theme.border),
        Print(format!("╭{}╮", "─".repeat(inner_w + 2)))
    )?;
    for (i, row) in rows.iter().enumerate() {
        let line: String = row.chars().take(inner_w).collect();
        queue!(
            out,
            cursor::MoveTo(x, y + 1 + i as u16),
            SetForegroundColor(theme.border),
            Print("│ "),
            SetForegroundColor(theme.fg.unwrap_or(Color::Reset)),
            Print(format!("{line:<inner_w$}")),
            SetForegroundColor(theme.border),
            Print(" │")
        )?;
    }
    queue!(
        out,
        cursor::MoveTo(x, y + 1 + rows.len() as u16),
        SetForegroundColor(theme.border),
        Print(format!("╰{}╯", "─".repeat(inner_w + 2))),
        ResetColor
    )?;
    Ok(())
}

/// The hover docs popup (K): signature, description, and examples from the
/// language server, wrapped and scrollable with j/k.
fn render_hover(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let Some((lines, scroll)) = &editor.hover else {
        return Ok(());
    };
    let Some((cx, cy)) = editor.screen_cursor() else {
        return Ok(());
    };
    let text_w = lines
        .iter()
        .map(|l| l.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(24, 76)
        .min(editor.size.0.saturating_sub(4) as usize);

    let mut rows = Vec::new();
    for line in lines {
        wrap_line(line, text_w, &mut rows);
    }
    let h = rows.len().min(14);
    let start = (*scroll).min(rows.len().saturating_sub(h));
    let rows = &rows[start..start + h];

    let w = (text_w + 4) as u16;
    // Above the cursor when there is room — the code under discussion stays
    // visible — otherwise below.
    let box_h = h as u16 + 2;
    let top = if cy >= box_h {
        cy - box_h
    } else {
        (cy + 1).min(editor.size.1.saturating_sub(box_h))
    };
    let x = cx.min(editor.size.0.saturating_sub(w));
    draw_text_popup(out, x, top, text_w, rows)
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
/// Nerd Font icon and filetype color for a tree entry, nvim-tree-style.
fn file_icon(name: &str, is_dir: bool, expanded: bool) -> (&'static str, Color) {
    const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color::Rgb { r, g, b }
    }
    if is_dir {
        let icon = if expanded { "\u{f07c}" } else { "\u{f07b}" }; // open/closed folder
        return (icon, rgb(122, 162, 247));
    }
    match name.rsplit('.').next().unwrap_or("") {
        "rs" => ("\u{e7a8}", rgb(222, 165, 132)),
        "toml" => ("\u{e615}", rgb(228, 104, 118)),
        "md" => ("\u{f48a}", rgb(130, 170, 255)),
        "json" => ("\u{e60b}", rgb(224, 175, 104)),
        "sh" | "bash" | "zsh" => ("\u{f489}", rgb(158, 206, 106)),
        "py" => ("\u{e73c}", rgb(224, 175, 104)),
        "js" | "jsx" | "mjs" => ("\u{e74e}", rgb(224, 175, 104)),
        "ts" | "tsx" => ("\u{e628}", rgb(86, 156, 214)),
        "c" | "h" => ("\u{e61e}", rgb(86, 156, 214)),
        "cc" | "cpp" | "hpp" => ("\u{e61d}", rgb(86, 156, 214)),
        "go" => ("\u{e626}", rgb(86, 192, 230)),
        "html" => ("\u{e736}", rgb(225, 140, 84)),
        "css" => ("\u{e749}", rgb(86, 156, 214)),
        "yml" | "yaml" => ("\u{e615}", rgb(187, 154, 247)),
        "lock" => ("\u{f023}", rgb(150, 150, 150)),
        "txt" => ("\u{f15c}", rgb(160, 170, 190)),
        _ => ("\u{f15b}", rgb(160, 170, 190)), // generic file
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
    let prefix = if editor.mode == Mode::Command {
        ':'
    } else {
        '/'
    };
    draw_popup(
        out,
        x,
        y,
        w,
        title,
        &format!(" {prefix}{}", editor.command_line),
    )?;

    // Fuzzy command suggestions extend the prompt's box; Tab/arrows highlight.
    if editor.mode == Mode::Command {
        let suggestions = editor.command_suggestions();
        if !suggestions.is_empty() {
            let rows: Vec<String> = suggestions
                .iter()
                .map(|name| {
                    let doc = crate::commands::find(name).map(|c| c.doc).unwrap_or("");
                    format!(" {name:<18} {doc}")
                })
                .collect();
            draw_box_list(out, x, y + 2, w, &rows, editor.command_suggest)?;
        }
    }
    Ok(())
}

/// Rows continuing an open `draw_popup` box: a `├─┤` separator (drawn over
/// the popup's bottom border at `y`), walled rows with the selected one
/// reversed, and the closing `╰─╯`.
fn draw_box_list(
    out: &mut impl Write,
    x: u16,
    y: u16,
    w: u16,
    rows: &[String],
    selected: Option<usize>,
) -> std::io::Result<()> {
    let inner = (w as usize).saturating_sub(2);
    let theme = crate::theme::current();
    queue!(
        out,
        cursor::MoveTo(x, y),
        SetBackgroundColor(theme.popup_bg),
        SetForegroundColor(theme.border),
        Print(format!("├{}┤", "─".repeat(inner)))
    )?;
    for (i, row) in rows.iter().enumerate() {
        let line: String = row.chars().take(inner).collect();
        queue!(
            out,
            cursor::MoveTo(x, y + 1 + i as u16),
            SetForegroundColor(theme.border),
            Print("│"),
            SetForegroundColor(theme.fg.unwrap_or(Color::Reset))
        )?;
        if selected == Some(i) {
            queue!(
                out,
                SetAttribute(Attribute::Reverse),
                Print(format!("{line:<inner$}")),
                SetAttribute(Attribute::Reset),
                SetBackgroundColor(theme.popup_bg)
            )?;
        } else {
            queue!(out, Print(format!("{line:<inner$}")))?;
        }
        queue!(out, SetForegroundColor(theme.border), Print("│"))?;
    }
    queue!(
        out,
        cursor::MoveTo(x, y + 1 + rows.len() as u16),
        Print(format!("╰{}╯", "─".repeat(inner))),
        ResetColor
    )
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
