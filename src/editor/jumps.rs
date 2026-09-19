//! The jumplist, and finding the buffer for a file.
//!
//! Anything that moves you further than a motion — `gd`, a picker, `gg`/`G`,
//! a search, `:42`, a diagnostic or change jump — leaves the place it left in
//! the jumplist, and `C-o` / `C-i` walk back and forth through it.

use super::*;

/// Jumps remembered; the oldest go first.
const JUMPLIST_LEN: usize = 100;

impl Editor {
    /// Remember the cursor as a place to come back to.
    pub(crate) fn push_jump(&mut self) {
        self.push_jump_at(self.current, self.doc().cursor);
    }

    pub(crate) fn push_jump_at(&mut self, doc: usize, pos: usize) {
        // A new jump forgets the ones you had walked back past.
        self.jumps.truncate(self.jump_idx);
        if self.jumps.last() != Some(&(doc, pos)) {
            self.jumps.push((doc, pos));
        }
        if self.jumps.len() > JUMPLIST_LEN {
            self.jumps.remove(0);
        }
        self.jump_idx = self.jumps.len();
    }

    /// `C-o`
    pub fn jump_back(&mut self) {
        if self.jumps.is_empty() {
            self.set_status("at the oldest jump");
            return;
        }
        if self.jump_idx >= self.jumps.len() {
            // Leaving the present: remember it, so C-i can come back.
            let here = (self.current, self.doc().cursor);
            if self.jumps.last() != Some(&here) {
                self.jumps.push(here);
                if self.jumps.len() > JUMPLIST_LEN {
                    self.jumps.remove(0);
                }
            }
            self.jump_idx = self.jumps.len() - 1;
        }
        if self.jump_idx == 0 {
            self.set_status("at the oldest jump");
            return;
        }
        self.jump_idx -= 1;
        self.goto_jump(self.jumps[self.jump_idx]);
    }

    /// `C-i` (and Tab, which most terminals can't tell apart from it)
    pub fn jump_forward(&mut self) {
        if self.jump_idx + 1 >= self.jumps.len() {
            self.set_status("at the newest jump");
            return;
        }
        self.jump_idx += 1;
        self.goto_jump(self.jumps[self.jump_idx]);
    }

    fn goto_jump(&mut self, (doc, pos): (usize, usize)) {
        if doc >= self.documents.len() {
            return;
        }
        self.leave_terminal_for_edit();
        self.current = doc;
        let doc = self.doc_mut();
        doc.cursor = pos.min(doc.text.len_chars());
        doc.anchor = doc.cursor;
        doc.extra.clear();
        doc.clamp_cursor(false);
        doc.goal_col = None;
    }

    /// The open buffer showing `path`, however it is spelled.
    pub fn open_buffer(&self, path: &Path) -> Option<usize> {
        let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.documents.iter().position(|d| {
            d.path
                .as_ref()
                .is_some_and(|p| p == path || p.canonicalize().is_ok_and(|p| p == canon))
        })
    }

    /// The buffer for `path`, opening the file if it isn't open yet.
    pub(crate) fn buffer_for(&mut self, path: &Path) -> Option<usize> {
        if let Some(i) = self.open_buffer(path) {
            return Some(i);
        }
        match Document::open(path) {
            Ok(doc) => {
                self.documents.push(doc);
                Some(self.documents.len() - 1)
            }
            Err(e) => {
                self.set_status(format!("Error: {e}"));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::tests::{editor_with, press};

    #[test]
    fn ctrl_o_and_ctrl_i_walk_the_jumps() {
        let mut editor = editor_with("a\nb\nc\nd\ne\n");
        press(&mut editor, "j"); // line 1 — a motion, not a jump
        press(&mut editor, "G"); // jump to the end
        assert_eq!(editor.doc().cursor_line(), 4);
        press(&mut editor, "gg");
        assert_eq!(editor.doc().cursor_line(), 0);
        press(&mut editor, "C-o");
        assert_eq!(editor.doc().cursor_line(), 4);
        press(&mut editor, "C-o");
        assert_eq!(editor.doc().cursor_line(), 1);
        press(&mut editor, "C-o");
        assert!(editor.status.contains("oldest"));
        press(&mut editor, "C-i");
        assert_eq!(editor.doc().cursor_line(), 4);
        press(&mut editor, "<tab>");
        assert_eq!(editor.doc().cursor_line(), 0);
        press(&mut editor, "<tab>");
        assert!(editor.status.contains("newest"));
    }

    #[test]
    fn a_new_jump_after_walking_back_drops_the_forward_ones() {
        let mut editor = editor_with("a\nb\nc\nd\n");
        press(&mut editor, "G gg C-o"); // back at the end
        press(&mut editor, ":2 <enter>");
        assert_eq!(editor.doc().cursor_line(), 1);
        press(&mut editor, "C-i");
        assert!(editor.status.contains("newest"));
        press(&mut editor, "C-o");
        assert_eq!(editor.doc().cursor_line(), 3);
    }
}
