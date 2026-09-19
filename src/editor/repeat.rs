//! `.` and macros: recording what was typed, and playing it back.
//!
//! Both replay *inputs*, not edits: a change is re-run through the same key
//! dispatch that ran it the first time, so it acts on the selection and the
//! cursors that are there now — `.` after `mi( d` deletes the inside of the
//! parentheses you are in, not the ones you were in.

use super::*;

/// Selecting inputs kept in front of a change before they are dropped.
const SELECTION_PREFIX_CAP: usize = 32;

/// Replays nested deeper than this are a macro calling itself.
const MAX_REPLAY_DEPTH: usize = 20;

impl Editor {
    /// Note an input for `.` and for the macro being recorded.
    pub(crate) fn record(&mut self, input: Input) {
        if self.change_keys.is_empty() {
            self.change_start = (self.current, self.doc().revision);
            self.change_via_prompt = false;
            self.last_command = None;
        }
        self.change_keys.push(input.clone());
        if let Some((_, inputs)) = self.macro_rec.as_mut() {
            inputs.push(input);
        }
    }

    /// Take back the input just recorded: a key the completion menu used to
    /// move its highlight, which means nothing on replay.
    pub(crate) fn unrecord_last(&mut self) {
        if self.replaying {
            return;
        }
        self.change_keys.pop();
        if let Some((_, inputs)) = self.macro_rec.as_mut() {
            inputs.pop();
        }
    }

    /// Record an effect in place of the keys that caused it.
    pub(crate) fn record_effect(&mut self, input: Input) {
        if !self.replaying {
            self.record(input);
        }
    }

    /// After each input: once the editor is back at rest in normal mode, the
    /// inputs since it last was are one command. Keep it for `.` if it
    /// changed the buffer — and wasn't undo, redo, or a replay itself.
    pub(crate) fn finish_change_record(&mut self) {
        if matches!(self.mode, Mode::Command | Mode::Search | Mode::Picker)
            || self.tree_focused
            || self.terminal_focused()
            || self.help_scroll.is_some()
        {
            self.change_via_prompt = true;
        }
        let at_rest = self.mode == Mode::Normal
            && self.pending.is_empty()
            && self.count.is_none()
            && !self.awaiting_register
            && self.active_register.is_none()
            && !self.awaiting_surround
            && self.awaiting_char.is_none()
            && self.pending_line_op.is_none();
        if !at_rest {
            return;
        }
        let inputs = std::mem::take(&mut self.change_keys);
        let (doc, revision) = self.change_start;
        let changed = doc == self.current
            && self
                .documents
                .get(doc)
                .is_some_and(|d| d.revision != revision);
        let repeatable = !matches!(
            self.last_command,
            Some(
                "undo"
                    | "redo"
                    | "repeat_last_change"
                    | "macro_play"
                    | "macro_record"
                    | "save"
                    | "format_buffer"
            )
        );
        if changed {
            let prefix = std::mem::take(&mut self.selection_prefix);
            if repeatable && !self.change_via_prompt && !inputs.is_empty() {
                self.last_change = prefix.into_iter().chain(inputs).collect();
            }
            return;
        }
        // No change: a selection made from where the cursor is (a word, a
        // line, a text object) belongs to the change that acts on it, so
        // `mi( d` repeats as a whole. Selections that depend on something
        // else — a search's `n`, a click — stay out, which is what makes
        // `n .` walk the matches.
        let doc = self.doc();
        let selected = doc.anchor != doc.cursor || doc.extra.iter().any(|(a, c)| a != c);
        let relative = self.extend
            || matches!(
                self.last_command,
                Some(
                    "select_word_next"
                        | "select_word_prev"
                        | "select_word_end"
                        | "select_line"
                        | "select_inside"
                        | "select_around"
                        | "expand_selection"
                        | "extend_mode"
                )
            );
        if selected && relative && !self.change_via_prompt {
            // Selecting without ever editing (pressing `w` down a file) must
            // not pile up forever; what `.` wants is the last few anyway.
            if self.selection_prefix.len() > SELECTION_PREFIX_CAP {
                self.selection_prefix.clear();
            }
            self.selection_prefix.extend(inputs);
        } else {
            self.selection_prefix.clear();
        }
    }

    /// Feed recorded inputs back through the ordinary input path. False when
    /// the nesting limit stopped it.
    pub(crate) fn replay(&mut self, inputs: &[Input]) -> bool {
        if self.replay_depth >= MAX_REPLAY_DEPTH {
            self.set_status("macro nested too deep — stopped");
            return false;
        }
        self.replay_depth += 1;
        let was = std::mem::replace(&mut self.replaying, true);
        for input in inputs {
            match input {
                Input::Key(key) => self.handle_key(*key),
                Input::Paste(text) => self.handle_paste(text),
                Input::Complete(del, text) => {
                    let doc = self.doc_mut();
                    let from = doc.cursor.saturating_sub(*del);
                    doc.delete_range(from, doc.cursor);
                    self.doc_mut().insert_at_cursor(text);
                }
            }
            if self.should_quit {
                break;
            }
        }
        self.replaying = was;
        self.replay_depth -= 1;
        true
    }

    /// `.` — run the last change again, `count` times.
    pub fn repeat_last_change(&mut self) {
        let count = self.take_count();
        if self.last_change.is_empty() {
            self.set_status("nothing to repeat yet");
            return;
        }
        let inputs = self.last_change.clone();
        for _ in 0..count {
            if !self.replay(&inputs) {
                break;
            }
        }
        // Whatever the change left selected stays selected.
        self.keep_selection = true;
    }

    /// `q` — start recording (the next key names the register), or stop.
    pub fn macro_record(&mut self) {
        self.keep_selection = true;
        match self.macro_rec.take() {
            Some((reg, mut inputs)) => {
                // Drop the keys of the command that stopped the recording:
                // they are the tail of what has been recorded since the
                // editor was last at rest.
                let tail = self.change_keys.len().min(inputs.len());
                inputs.truncate(inputs.len() - tail);
                let n = inputs.len();
                self.macros.insert(reg, inputs);
                self.last_macro = Some(reg);
                self.set_status(format!("recorded @{reg} ({n} inputs)"));
            }
            None => {
                self.awaiting_char = Some(CharWait::MacroRecord);
                self.set_status("record a macro into register…");
            }
        }
    }

    /// `@` — the next key names the macro to play (`@@` the last one).
    pub fn macro_play(&mut self) {
        let count = self.take_count();
        self.keep_selection = true;
        self.awaiting_char = Some(CharWait::MacroPlay(count));
    }

    pub(crate) fn play_macro(&mut self, reg: char, count: usize) {
        let reg = if reg == '@' {
            match self.last_macro {
                Some(r) => r,
                None => {
                    self.set_status("no macro played yet");
                    return;
                }
            }
        } else {
            reg
        };
        if self.macro_rec.as_ref().is_some_and(|(r, _)| *r == reg) {
            self.set_status(format!("@{reg} is still being recorded"));
            return;
        }
        let Some(inputs) = self.macros.get(&reg).cloned() else {
            self.set_status(format!("no macro in register {reg}"));
            return;
        };
        self.last_macro = Some(reg);
        for _ in 0..count {
            if !self.replay(&inputs) {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::tests::{editor_with, press};

    #[test]
    fn dot_repeats_an_insert_session() {
        let mut editor = editor_with("a\nb\nc\n");
        press(&mut editor, "I - <space> <esc>");
        assert_eq!(editor.doc().text.to_string(), "- a\nb\nc\n");
        press(&mut editor, "j .");
        assert_eq!(editor.doc().text.to_string(), "- a\n- b\nc\n");
        press(&mut editor, "j 0 .");
        assert_eq!(editor.doc().text.to_string(), "- a\n- b\n- c\n");
    }

    #[test]
    fn dot_repeats_a_line_delete_with_its_count_and_skips_motions() {
        let mut editor = editor_with("1\n2\n3\n4\n5\n6\n");
        press(&mut editor, "2dd");
        assert_eq!(editor.doc().text.to_string(), "3\n4\n5\n6\n");
        press(&mut editor, "j");
        press(&mut editor, ".");
        assert_eq!(editor.doc().text.to_string(), "3\n6\n");
    }

    #[test]
    fn undo_is_not_what_dot_repeats() {
        let mut editor = editor_with("abc\n");
        press(&mut editor, "x"); // arms the line op…
        press(&mut editor, "<esc>"); // …which Esc cancels
        press(&mut editor, "A ! <esc>");
        press(&mut editor, "u");
        press(&mut editor, ".");
        assert_eq!(editor.doc().text.to_string(), "abc!\n");
    }

    #[test]
    fn a_macro_records_and_replays_with_a_count() {
        let mut editor = editor_with("a\nb\nc\nd\n");
        press(&mut editor, "q a");
        assert!(editor.macro_rec.is_some());
        press(&mut editor, "A ; <esc> j");
        press(&mut editor, "q");
        assert!(editor.macro_rec.is_none());
        assert_eq!(editor.doc().text.to_string(), "a;\nb\nc\nd\n");
        press(&mut editor, "2@a");
        assert_eq!(editor.doc().text.to_string(), "a;\nb;\nc;\nd\n");
        press(&mut editor, "@@");
        assert_eq!(editor.doc().text.to_string(), "a;\nb;\nc;\nd;\n");
    }

    /// `.` repeats the last *change*, not the last macro: after `@a`, `.`
    /// must redo the edit the macro's keys made where the cursor is now,
    /// which for a one-edit macro looks the same — but a second `.` must not
    /// walk the buffer the way replaying the macro again would.
    #[test]
    fn dot_after_a_macro_does_not_become_play_it_again() {
        let mut editor = editor_with("a\nb\nc\nd\n");
        press(&mut editor, "q a A ; <esc> j q"); // @a = append `;`, go down
        press(&mut editor, "@a");
        assert_eq!(editor.doc().text.to_string(), "a;\nb;\nc\nd\n");
        let after_macro = editor.last_change.clone();
        press(&mut editor, ".");
        assert_eq!(
            editor.last_change, after_macro,
            "`.` did not adopt the macro as the change to repeat"
        );
        // The change `.` repeats is the one from before the macro ran.
        assert_eq!(editor.doc().text.to_string(), "a;\nb;\nc;\nd\n");
    }

    #[test]
    fn a_macro_that_calls_itself_stops() {
        let mut editor = editor_with("x\n");
        press(&mut editor, "q a A y <esc> q");
        // Make @a call itself.
        let mut inputs = editor.macros[&'a'].clone();
        inputs.push(crate::editor::Input::Key(crate::keymap::Key::char('@')));
        inputs.push(crate::editor::Input::Key(crate::keymap::Key::char('a')));
        editor.macros.insert('a', inputs);
        press(&mut editor, "@a");
        assert!(editor.status.contains("too deep"));
    }
}
