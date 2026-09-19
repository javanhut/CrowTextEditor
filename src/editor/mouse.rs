//! The mouse: click to place the cursor (and focus the window clicked in),
//! drag to select, wheel to scroll. Screen cells are mapped back to buffer
//! positions through the same wrap and gutter geometry the renderer uses.

use super::*;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// Lines per wheel notch.
const WHEEL_LINES: isize = 3;

impl Editor {
    pub fn handle_mouse(&mut self, ev: MouseEvent) {
        let (x, y) = (ev.column, ev.row);
        let wheel = match ev.kind {
            MouseEventKind::ScrollDown => Some(WHEEL_LINES),
            MouseEventKind::ScrollUp => Some(-WHEEL_LINES),
            _ => None,
        };
        // Overlays own the mouse while they are up: the wheel scrolls them,
        // clicks do nothing underneath.
        if let Some(scroll) = self.help_scroll {
            if let Some(n) = wheel {
                self.help_scroll = Some((scroll as isize + n).max(0) as usize);
            }
            return;
        }
        if self.mode == Mode::Picker {
            if let (Some(n), Some(picker)) = (wheel, self.picker.as_mut()) {
                picker.move_selection(n.signum());
            }
            return;
        }
        if matches!(self.mode, Mode::Command | Mode::Search) || self.tree_input.is_some() {
            return;
        }
        self.hover = None;
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let extend = ev.modifiers.contains(KeyModifiers::SHIFT);
                self.mouse_click(x, y, extend);
            }
            MouseEventKind::Drag(MouseButton::Left) => self.mouse_drag_to(x, y),
            MouseEventKind::Up(MouseButton::Left) => self.mouse_drag = false,
            _ => {
                if let Some(n) = wheel {
                    self.mouse_scroll(x, y, n);
                }
            }
        }
    }

    /// The window under a screen cell.
    fn window_at(&self, x: u16, y: u16) -> Option<(usize, Rect)> {
        self.window_rects()
            .0
            .into_iter()
            .find(|&(_, (rx, ry, rw, rh))| x >= rx && x < rx + rw && y >= ry && y < ry + rh)
    }

    fn focus_window(&mut self, id: usize) {
        if id != self.focused {
            self.save_focus_state();
            self.focused = id;
            self.restore_focus_state();
            self.sync_focus_mode();
        }
        self.tree_focused = false;
    }

    fn mouse_click(&mut self, x: u16, y: u16, extend: bool) {
        if x < self.tree_width() {
            self.tree_click(y);
            return;
        }
        let Some((id, _)) = self.window_at(x, y) else {
            return;
        };
        if Some(id) == self.preview_win() {
            return; // a view, not a place to put a cursor
        }
        self.focus_window(id);
        if self.terminal_focused() {
            return;
        }
        let Some(pos) = self.position_at(x, y) else {
            return;
        };
        self.completion = None;
        let insert = self.mode == Mode::Insert;
        let doc = self.doc_mut();
        doc.extra.clear();
        doc.cursor = pos;
        doc.clamp_cursor(insert);
        if !extend {
            doc.anchor = doc.cursor;
        }
        doc.goal_col = None;
        self.extend = false;
        self.mouse_drag = true;
    }

    fn mouse_drag_to(&mut self, x: u16, y: u16) {
        if !self.mouse_drag || self.terminal_focused() {
            return;
        }
        let (rx, ry, rw, rh) = self.focused_rect();
        // Past the window's edge the selection keeps going to its last row.
        let x = x.clamp(rx, rx + rw.saturating_sub(1));
        let y = y.clamp(ry, ry + rh.saturating_sub(1));
        let Some(pos) = self.position_at(x, y) else {
            return;
        };
        let doc = self.doc_mut();
        // Selections are half-open: dragging forward has to take in the
        // character under the pointer.
        doc.cursor = if pos >= doc.anchor {
            crate::position::next_grapheme_boundary(doc.text.slice(..), pos)
        } else {
            pos
        };
        doc.goal_col = None;
    }

    fn mouse_scroll(&mut self, x: u16, y: u16, lines: isize) {
        if x < self.tree_width() {
            if let Some(tree) = self.tree.as_mut() {
                tree.move_selection(lines);
            }
            return;
        }
        let Some((id, _)) = self.window_at(x, y) else {
            return;
        };
        if Some(id) == self.preview_win() {
            return; // it follows its source's scroll
        }
        if Some(id) == self.terminal_win() {
            if let Some(t) = self.terminal.as_mut() {
                t.scroll_by(-lines);
            }
            return;
        }
        self.focus_window(id);
        self.scroll_lines(lines);
    }

    /// Scroll the focused window's view, dragging the cursor along only as
    /// far as it takes to keep it on screen.
    ///
    /// Counted in visual rows, not lines: with soft wrap one line can be
    /// taller than the window, and a cursor moved a whole line at a time
    /// could never stay inside it — `ensure_cursor_visible` would pull the
    /// view straight back and the wheel would do nothing at all.
    pub(crate) fn scroll_lines(&mut self, lines: isize) {
        let height = self.text_height();
        if height == 0 {
            return;
        }
        let wrap = self.wrap_width();
        let so = crate::config::scrolloff().min(height.saturating_sub(1) / 2);
        let insert = self.mode == Mode::Insert;
        let (rx, ry, rw, _) = self.focused_rect();
        let gutter = self.gutter_width();

        let doc = self.doc_mut();
        // A resize can leave the viewport parked on a row its line no longer
        // has; every walk from here on assumes it doesn't.
        let rows_here = doc.visual_rows(doc.view_line, wrap);
        doc.view_row = doc.view_row.min(rows_here.saturating_sub(1));
        doc.scroll_view(wrap, lines);

        // Where the cursor now sits relative to the top of the view.
        let (cline, crow, ccol) = doc.cursor_visual(wrap);
        let top = (doc.view_line, doc.view_row);
        let goal = doc.goal_col.unwrap_or(ccol);
        // The cursor has to end up at least `scrolloff` rows inside the view
        // at both ends, or `ensure_cursor_visible` will scroll the view back
        // to it and undo the wheel.
        let screen_row = if (cline, crow) < top {
            Some(so)
        } else {
            let below = doc.rows_forward(wrap, top, (cline, crow));
            if below < so {
                Some(so)
            } else if below + so + 1 > height {
                Some(height.saturating_sub(so + 1))
            } else {
                None
            }
        };
        let Some(screen_row) = screen_row.filter(|&r| r < height) else {
            return; // still on screen: the cursor stays where it is
        };
        // The same mapping a click uses, so the cursor lands on the character
        // drawn at that row and the column it was aiming for.
        let x = rx + ((gutter + goal) as u16).min(rw.saturating_sub(1));
        let Some(pos) = self.position_at(x, ry + screen_row as u16) else {
            return;
        };
        let doc = self.doc_mut();
        doc.cursor = pos;
        doc.clamp_cursor(insert);
        doc.anchor = doc.cursor;
        doc.goal_col = Some(goal);
    }

    /// The buffer position drawn at screen cell (x, y) of the focused window.
    pub(crate) fn position_at(&self, x: u16, y: u16) -> Option<usize> {
        let (rx, ry, _, _) = self.focused_rect();
        let wrap = self.wrap_width();
        let tab = crate::config::tab_width();
        let doc = self.doc();
        let mut rows_left = y.checked_sub(ry)? as usize;
        let (mut line, mut sub) = (doc.view_line, doc.view_row);
        let last_line = doc.line_count().saturating_sub(1);
        // Walk down the visual rows to the one clicked.
        let offsets = loop {
            let offsets = match wrap {
                Some(w) => crate::position::wrap_offsets(doc.line(line), w, tab),
                None => vec![0],
            };
            let here = offsets.len().saturating_sub(sub);
            if rows_left < here || line >= last_line {
                sub = (sub + rows_left).min(offsets.len() - 1);
                break offsets;
            }
            rows_left -= here;
            line += 1;
            sub = 0;
        };
        let col = (x.checked_sub(rx)? as usize).saturating_sub(self.gutter_width());
        let slice = doc.line(line);
        let len = doc.line_len(line);
        let offset = match wrap {
            None => crate::position::display_col_to_char(slice, col + doc.view_col, tab),
            Some(_) => {
                let start = offsets[sub];
                let end = offsets.get(sub + 1).copied().unwrap_or(len);
                let mut at =
                    crate::position::display_col_to_char_between(slice, start, end, col, tab);
                // Past the end of a wrapped row lands on its last character,
                // not the first one of the row below — and on the character's
                // start, never inside a cluster.
                if at == end && end < len {
                    let line_start = doc.line_start(line);
                    let back = crate::position::prev_grapheme_boundary(
                        doc.text.slice(..),
                        line_start + end,
                    );
                    at = back.saturating_sub(line_start).max(start);
                }
                at
            }
        };
        Some(doc.line_start(line) + offset.min(len))
    }

    /// A click in the sidebar picks the row, and opens it as Enter would.
    fn tree_click(&mut self, y: u16) {
        let height = self.size.1.saturating_sub(2) as usize;
        // The sidebar stops above the status and command lines; a click on
        // those is not a click on the row that would have been there.
        if y as usize >= height {
            return;
        }
        let Some(tree) = self.tree.as_mut() else {
            return;
        };
        // The same window onto the rows that `render_tree` draws.
        let start = tree
            .selected
            .saturating_sub(height.saturating_sub(1).min(tree.selected));
        let idx = start + y as usize;
        if idx >= tree.rows.len() {
            return;
        }
        tree.selected = idx;
        self.tree_focused = true;
        let row = &tree.rows[idx];
        if row.is_dir {
            tree.toggle_selected();
        } else {
            let path = row.path.clone();
            self.tree_focused = false;
            self.jump_to(path, 0, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::tests::editor_with;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn click_places_the_cursor_and_drag_selects() {
        let mut editor = editor_with("hello world\nsecond line\n");
        let gutter = editor.gutter_width() as u16;
        editor.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            gutter + 6,
            0,
        ));
        assert_eq!(editor.doc().cursor, 6);
        editor.handle_mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            gutter + 5,
            1,
        ));
        editor.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), gutter + 5, 1));
        let doc = editor.doc();
        assert_eq!(
            doc.text.slice(doc.anchor..doc.cursor).to_string(),
            "world\nsecond"
        );
    }

    #[test]
    fn clicking_past_a_line_end_lands_on_it() {
        let mut editor = editor_with("ab\nlonger line\n");
        let gutter = editor.gutter_width() as u16;
        editor.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            gutter + 20,
            0,
        ));
        assert_eq!(editor.doc().cursor, 1, "normal mode sits on the last char");
    }

    /// One line taller than the window: the wheel has to move the view in
    /// rows and bring the cursor with it, or `ensure_cursor_visible` pulls
    /// the view straight back and the wheel does nothing at all.
    #[test]
    fn the_wheel_works_inside_one_very_long_wrapped_line() {
        let long: String = std::iter::repeat_n("word ", 900).collect();
        let mut editor = editor_with(&format!("{long}\nafter\n"));
        assert!(
            editor.doc().visual_rows(0, editor.wrap_width()) > editor.text_height(),
            "the first line has to be taller than the window for this test"
        );
        for _ in 0..3 {
            editor.handle_mouse(mouse(MouseEventKind::ScrollDown, 10, 5));
            editor.ensure_cursor_visible(); // what the main loop does each frame
        }
        assert_eq!(editor.doc().view_line, 0);
        assert!(
            editor.doc().view_row >= 9,
            "the view stayed put: view_row {}",
            editor.doc().view_row
        );
    }

    /// A click on the status or command line is not a click on the row the
    /// sidebar would have drawn there.
    #[test]
    fn clicks_below_the_sidebar_do_not_pick_a_row() {
        let mut editor = editor_with("x\n");
        editor.tree_toggle();
        let before = editor.tree.as_ref().map(|t| t.selected);
        editor.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            1,
            editor.size.1 - 1,
        ));
        assert_eq!(editor.tree.as_ref().map(|t| t.selected), before);
    }

    #[test]
    fn the_wheel_scrolls_and_keeps_the_cursor_on_screen() {
        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let mut editor = editor_with(&text);
        editor.handle_mouse(mouse(MouseEventKind::ScrollDown, 10, 5));
        editor.handle_mouse(mouse(MouseEventKind::ScrollDown, 10, 5));
        assert_eq!(editor.doc().view_line, 6);
        let so = crate::config::scrolloff();
        assert_eq!(editor.doc().cursor_line(), 6 + so);
    }
}
