//! What the character after `mi`, `ma`, `md`, `mr`, `q` and `@` does.

use super::*;
use crate::transaction::Transaction;

impl Editor {
    /// Finish a command that was waiting for a character.
    pub(crate) fn char_wait(&mut self, wait: CharWait, c: char) {
        match wait {
            CharWait::Inside => self.select_object(c, true),
            CharWait::Around => self.select_object(c, false),
            CharWait::SurroundDelete => self.surround_edit(c, None),
            CharWait::SurroundReplace(None) => {
                self.awaiting_char = Some(CharWait::SurroundReplace(Some(c)));
                self.set_status(format!("replace {c} with…"));
            }
            CharWait::SurroundReplace(Some(old)) => self.surround_edit(old, Some(c)),
            CharWait::MacroRecord => {
                self.macro_rec = Some((c, Vec::new()));
                self.set_status(format!("recording @{c} — q stops"));
            }
            CharWait::MacroPlay(count) => self.play_macro(c, count),
        }
        // The key naming the register runs outside the keymap, so it has to
        // say what it was itself. Without this, the macro's own last command
        // is what `.` sees, and `.` quietly becomes "play @a again".
        match wait {
            CharWait::MacroPlay(_) => self.last_command = Some("macro_play"),
            CharWait::MacroRecord => self.last_command = Some("macro_record"),
            _ => {}
        }
    }

    /// Every selection becomes the object around it. Selections with no such
    /// object stay as they were.
    fn select_object(&mut self, c: char, inside: bool) {
        if matches!(c, 'f' | 't' | 'a' | 'c') {
            self.doc_mut().settle_syntax(); // these read the tree
        }
        let doc = self.doc();
        let tree = doc.syntax.as_ref().and_then(|s| s.tree.as_ref());
        let pick = |(a, cur): (usize, usize)| {
            let sel = (a.min(cur), a.max(cur));
            crate::textobj::find(&doc.text, tree, sel, c, inside)
        };
        let primary = pick((doc.anchor, doc.cursor));
        let extras: Vec<Option<(usize, usize)>> = doc.extra.iter().map(|&s| pick(s)).collect();
        if primary.is_none() && extras.iter().all(Option::is_none) {
            let what = if inside { "inside" } else { "around" };
            self.set_status(format!("nothing to select {what} {c:?} here"));
            return;
        }
        let doc = self.doc_mut();
        if let Some((s, e)) = primary {
            (doc.anchor, doc.cursor) = (s, e);
        }
        for (slot, found) in doc.extra.iter_mut().zip(extras) {
            if let Some(range) = found {
                *slot = range;
            }
        }
        doc.goal_col = None;
        doc.dedupe_cursors();
    }

    /// `md(` deletes the pair around each cursor; `mr([` (with `new`)
    /// swaps it for another. One transaction, one undo step.
    fn surround_edit(&mut self, c: char, new: Option<char>) {
        let Some((open, close)) = crate::textobj::pair_for(c) else {
            self.set_status(format!("{c:?} isn't a pair — try ( [ {{ < \" ' `"));
            return;
        };
        let replacement = match new {
            Some(n) => match crate::textobj::pair_for(n) {
                Some(pair) => Some(pair),
                None => Some((n, n)),
            },
            None => None,
        };
        let doc = self.doc();
        let text = doc.text.slice(..);
        let mut pairs: Vec<(usize, usize)> = std::iter::once((doc.anchor, doc.cursor))
            .chain(doc.extra.iter().copied())
            .filter_map(|(a, cur)| {
                let (from, to) = (a.min(cur), a.max(cur));
                if open == close {
                    // The strict version: `md"` takes the quotes around the
                    // cursor, never the next string along the line.
                    crate::textobj::quote_pair_enclosing(text, (from, to), open)
                } else {
                    crate::textobj::bracket_pair(text, from, to, open, close)
                }
            })
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        if pairs.is_empty() {
            self.set_status(format!("no surrounding {open}{close} here"));
            return;
        }
        let mut changes: Vec<(usize, usize, Option<String>)> = Vec::new();
        for &(o, cl) in &pairs {
            let (ro, rc) = match replacement {
                Some((a, b)) => (Some(a.to_string()), Some(b.to_string())),
                None => (None, None),
            };
            changes.push((o, o + 1, ro));
            changes.push((cl, cl + 1, rc));
        }
        // Nested pairs interleave their delimiters: order all the edits.
        changes.sort_by_key(|c| c.0);
        changes.dedup_by_key(|c| c.0);
        let tx = Transaction::change(&self.doc().text, changes);
        let doc = self.doc_mut();
        let cursor = tx.map_pos(doc.cursor, false);
        doc.apply(tx, cursor);
        doc.clamp_cursor(false);
        doc.commit_undo_group();
        self.set_status(match replacement {
            Some((a, b)) => format!("{open}{close} → {a}{b}"),
            None => format!("removed {open}{close}"),
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::tests::{editor_with, press};

    #[test]
    fn mi_selects_inside_and_d_deletes_it() {
        let mut editor = editor_with("f(alpha, beta)\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (4, 4);
        press(&mut editor, "mi(");
        let doc = editor.doc();
        assert_eq!(
            doc.text.slice(doc.anchor..doc.cursor).to_string(),
            "alpha, beta"
        );
        press(&mut editor, "d");
        assert_eq!(editor.doc().text.to_string(), "f()\n");
    }

    #[test]
    fn ma_works_at_every_cursor() {
        let mut editor = editor_with("[a] x\n[b] y\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (1, 1);
        press(&mut editor, "C");
        press(&mut editor, "ma[");
        press(&mut editor, "d");
        assert_eq!(editor.doc().text.to_string(), " x\n y\n");
    }

    #[test]
    fn md_and_mr_edit_the_surrounding_pair() {
        let mut editor = editor_with("say(\"hi\")\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (6, 6);
        press(&mut editor, "mr\"'");
        assert_eq!(editor.doc().text.to_string(), "say('hi')\n");
        press(&mut editor, "md(");
        assert_eq!(editor.doc().text.to_string(), "say'hi'\n");
        press(&mut editor, "u");
        assert_eq!(editor.doc().text.to_string(), "say('hi')\n");
    }

    /// `md"` and `mr"` act on the string the cursor is in — never on the
    /// next one along the line, and not at all when it is in none.
    #[test]
    fn surround_edits_only_ever_touch_the_pair_around_the_cursor() {
        let mut editor = editor_with("say(\"hi\") and \"bye\"\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (5, 7); // exactly `hi`, as ma" would leave it
        press(&mut editor, "md\"");
        assert_eq!(editor.doc().text.to_string(), "say(hi) and \"bye\"\n");

        // Outside every pair: nothing to delete, and it says so.
        let mut editor = editor_with("abc \"def\"\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (0, 0);
        press(&mut editor, "md\"");
        assert_eq!(editor.doc().text.to_string(), "abc \"def\"\n");
        assert!(editor.status.contains("no surrounding"));
    }

    /// `miw` on a line ending has nothing to select; it must say so rather
    /// than quietly replacing the selection with an empty one.
    #[test]
    fn a_word_object_on_a_line_ending_finds_nothing() {
        let mut editor = editor_with("ab\ncd\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (2, 2); // on the line ending itself
        press(&mut editor, "miw");
        let doc = editor.doc();
        assert_eq!((doc.anchor, doc.cursor), (2, 2), "the cursor is untouched");
        assert!(editor.status.contains("nothing to select"));
    }

    #[test]
    fn dot_repeats_a_text_object_edit_where_the_cursor_is_now() {
        let mut editor = editor_with("(one) (two)\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (1, 1);
        press(&mut editor, "mi( d");
        assert_eq!(editor.doc().text.to_string(), "() (two)\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (4, 4);
        press(&mut editor, ".");
        assert_eq!(editor.doc().text.to_string(), "() ()\n");
    }
}
