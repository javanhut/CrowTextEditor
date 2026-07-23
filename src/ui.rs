//! Drawing the editor.
//!
//! The whole screen is redrawn each frame. That is fast enough at terminal
//! sizes and avoids a class of stale-cell bugs; a damage-tracking renderer is
//! an optimisation to make later, once there is something to measure.

use std::io::Write;

use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType};
use crossterm::{cursor, queue};
use ropey::RopeSlice;

use crate::editor::{Editor, Mode};
use crate::position::{self, TAB_WIDTH};

pub fn render(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    queue!(out, cursor::Hide, cursor::MoveTo(0, 0))?;

    render_text(editor, out)?;
    render_status_line(editor, out)?;
    render_command_line(editor, out)?;

    match editor.screen_cursor() {
        Some((col, row)) => queue!(out, cursor::MoveTo(col, row), cursor::Show)?,
        None => queue!(out, cursor::Hide)?,
    }

    // The cursor shape is the clearest signal of which mode you're in.
    match editor.mode {
        Mode::Insert => queue!(out, cursor::SetCursorStyle::SteadyBar)?,
        _ => queue!(out, cursor::SetCursorStyle::SteadyBlock)?,
    }

    out.flush()
}

/// Cell styling: an overlay (selection or extra cursor) plus a syntax color.
/// (The primary cursor is the terminal's own.)
const ST_NONE: u8 = 0;
const ST_SEL: u8 = 1;
const ST_CURSOR: u8 = 2;

type Style = (u8, Option<Color>);

fn render_text(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let (wins, seps) = editor.window_rects();
    for &(id, rect) in &wins {
        render_window(editor, id, rect, out)?;
    }
    for &((x, y, w, h), vertical) in &seps {
        queue!(out, SetForegroundColor(Color::DarkGrey))?;
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

    for row in 0..rh as usize {
        let line_idx = view_line + row;
        queue!(out, cursor::MoveTo(rx, ry + row as u16))?;

        if line_idx >= line_count {
            queue!(
                out,
                SetForegroundColor(Color::DarkGrey),
                Print("~"),
                ResetColor,
                Print(" ".repeat((rw as usize).saturating_sub(1)))
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
                Color::Yellow
            } else {
                Color::DarkGrey
            })),
            Print(number),
            ResetColor
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
            let fg = crate::syntax::color(crate::syntax::group_at(spans, p));
            (overlay, fg)
        };

        let runs = styled_visible(doc.line(line_idx), line_len, view_col, width, style_of);
        let mut printed = 0usize;
        for ((overlay, fg), s) in runs {
            printed += display_width(&s);
            if let Some(color) = fg {
                queue!(out, SetForegroundColor(color))?;
            }
            match overlay {
                ST_SEL => queue!(out, SetBackgroundColor(Color::DarkGrey))?,
                ST_CURSOR => queue!(out, SetAttribute(Attribute::Reverse))?,
                _ => {}
            }
            queue!(out, Print(s))?;
            if fg.is_some() || overlay != ST_NONE {
                queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
            }
        }
        // Pad to the window edge; UntilNewLine would bleed into a neighbour.
        if printed < width {
            queue!(out, Print(" ".repeat(width - printed)))?;
        }
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
        let w = position::char_width(c, col, TAB_WIDTH);
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

pub fn search_prompt(editor: &Editor) -> &'static str {
    if editor.search_select {
        "select/"
    } else {
        "/"
    }
}

fn render_command_line(editor: &Editor, out: &mut impl Write) -> std::io::Result<()> {
    let row = editor.size.1.saturating_sub(1);
    let width = editor.size.0 as usize;

    let mut content = match editor.mode {
        Mode::Command => format!(":{}", editor.command_line),
        Mode::Search => format!("{}{}", search_prompt(editor), editor.command_line),
        _ => editor.status.clone(),
    };
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

    queue!(
        out,
        cursor::MoveTo(0, row),
        Print(content),
        Clear(ClearType::UntilNewLine)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    fn plain(line: RopeSlice, view_col: usize, width: usize) -> String {
        let len = position::line_len_without_newline(line);
        styled_visible(line, len, view_col, width, |_| (ST_NONE, None))
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
                (ST_SEL, None)
            } else {
                (ST_NONE, None)
            }
        });
        assert_eq!(
            runs,
            vec![
                ((ST_NONE, None), "ab".to_string()),
                ((ST_SEL, None), "cd".to_string()),
                ((ST_NONE, None), "ef".to_string()),
            ]
        );
    }

    #[test]
    fn styled_newline_shows_as_one_column() {
        let rope = Rope::from_str("ab\ncd");
        // The whole line plus its newline is selected.
        let runs = styled_visible(rope.line(0), 2, 0, 10, |_| (ST_SEL, None));
        assert_eq!(runs, vec![((ST_SEL, None), "ab ".to_string())]);
    }
}
