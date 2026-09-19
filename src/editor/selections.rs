//! Operations on the whole set of selections: split, filter, align, rotate,
//! trim. Every selection is a half-open (anchor, cursor) char range; these
//! hand back forward ones (anchor at the start).

use super::*;
use crate::transaction::Transaction;

impl Editor {
    /// Every selection, the primary first, as (start, end).
    pub(crate) fn selections(&self) -> Vec<(usize, usize)> {
        let doc = self.doc();
        std::iter::once((doc.anchor, doc.cursor))
            .chain(doc.extra.iter().copied())
            .map(|(a, c)| (a.min(c), a.max(c)))
            .collect()
    }

    /// Replace the selections; `primary` indexes into `sels`.
    pub(crate) fn set_selections(&mut self, mut sels: Vec<(usize, usize)>, primary: usize) {
        if sels.is_empty() {
            return;
        }
        let p = sels.remove(primary.min(sels.len() - 1));
        let doc = self.doc_mut();
        (doc.anchor, doc.cursor) = p;
        doc.extra = sels;
        doc.goal_col = None;
        doc.dedupe_cursors();
        self.keep_selection = true;
    }

    /// `A-s`: one selection per line of every selection, newlines left out.
    pub fn split_selection_lines(&mut self) {
        let doc = self.doc();
        let mut out = Vec::new();
        for (from, to) in self.selections() {
            let first = doc.text.char_to_line(from);
            let last = doc.text.char_to_line(to.saturating_sub(1).max(from));
            for line in first..=last {
                let s = doc.line_start(line).max(from);
                let e = doc.line_end(line).min(to);
                if s < e {
                    out.push((s, e));
                }
            }
        }
        if out.is_empty() {
            self.set_status("nothing selected to split");
            self.keep_selection = true;
            return;
        }
        let n = out.len();
        self.set_selections(out, 0);
        self.set_status(format!("{n} selections"));
    }

    /// `A-S`, `A-k`, `A-K`: ask for the pattern in the search prompt.
    pub fn open_select_prompt(&mut self, kind: SelectPrompt) {
        self.keep_selection = true;
        self.select_prompt = Some(kind);
        self.command_line.clear();
        // The select flavour of the prompt: no live preview moving things.
        self.search_select = true;
        self.search_origin = (self.doc().anchor, self.doc().cursor);
        self.set_mode(Mode::Search);
    }

    /// The pattern is in: split, keep or remove.
    pub(crate) fn apply_select_prompt(&mut self, kind: SelectPrompt, pattern: &str) {
        if pattern.is_empty() {
            return;
        }
        let matches = crate::search::matches(&self.doc().text, pattern);
        let sels = self.selections();
        let inside = |(from, to): (usize, usize)| {
            matches
                .iter()
                .copied()
                .filter(move |&(s, e)| s >= from && e <= to && s < e)
        };
        let out: Vec<(usize, usize)> = match kind {
            SelectPrompt::Split => sels
                .iter()
                .flat_map(|&(from, to)| {
                    let mut pieces = Vec::new();
                    let mut at = from;
                    for (s, e) in inside((from, to)) {
                        if s > at {
                            pieces.push((at, s));
                        }
                        at = e;
                    }
                    if to > at {
                        pieces.push((at, to));
                    }
                    pieces
                })
                .collect(),
            SelectPrompt::Keep | SelectPrompt::Remove => {
                let keep = kind == SelectPrompt::Keep;
                sels.iter()
                    .copied()
                    .filter(|&sel| inside(sel).next().is_some() == keep)
                    .collect()
            }
        };
        if out.is_empty() {
            self.set_status(match kind {
                SelectPrompt::Split => "nothing left after splitting".to_string(),
                SelectPrompt::Keep => format!("no selection matches {pattern}"),
                SelectPrompt::Remove => format!("every selection matches {pattern}"),
            });
            self.keep_selection = true;
            return;
        }
        let n = out.len();
        self.set_selections(out, 0);
        self.set_status(format!("{n} selection{}", if n == 1 { "" } else { "s" }));
    }

    /// `&`: pad each selection's start with spaces so they all begin in the
    /// same column. One selection per line; one undo step.
    pub fn align_selections(&mut self) {
        self.keep_selection = true;
        let sels = self.selections();
        if sels.len() < 2 {
            self.set_status("align needs more than one selection");
            return;
        }
        let doc = self.doc();
        let tab = crate::config::tab_width();
        let mut starts: Vec<(usize, usize, usize)> = sels
            .iter()
            .map(|&(s, _)| {
                let line = doc.text.char_to_line(s);
                let col = crate::position::char_to_display_col(
                    doc.line(line),
                    s - doc.line_start(line),
                    tab,
                );
                (line, s, col)
            })
            .collect();
        starts.sort_unstable();
        if starts.windows(2).any(|w| w[0].0 == w[1].0) {
            self.set_status("align needs one selection per line");
            return;
        }
        let target = starts.iter().map(|s| s.2).max().unwrap_or(0);
        let changes: Vec<(usize, usize, Option<String>)> = starts
            .iter()
            .filter(|s| s.2 < target)
            .map(|&(_, at, col)| (at, at, Some(" ".repeat(target - col))))
            .collect();
        if changes.is_empty() {
            return;
        }
        let tx = Transaction::change(&doc.text, changes);
        let mapped: Vec<(usize, usize)> = std::iter::once((doc.anchor, doc.cursor))
            .chain(doc.extra.iter().copied())
            .map(|(a, c)| (tx.map_pos(a, false), tx.map_pos(c, false)))
            .collect();
        let doc = self.doc_mut();
        doc.apply(tx, mapped[0].1);
        doc.anchor = mapped[0].0;
        doc.extra = mapped[1..].to_vec();
        doc.commit_undo_group();
    }

    /// `)` / `(`: make the next (or previous) selection in document order
    /// the primary one.
    pub fn rotate_selections(&mut self, forward: bool) {
        self.keep_selection = true;
        let doc = self.doc();
        let mut all: Vec<(usize, usize)> = std::iter::once((doc.anchor, doc.cursor))
            .chain(doc.extra.iter().copied())
            .collect();
        if all.len() < 2 {
            return;
        }
        let primary = all[0];
        all.sort_unstable_by_key(|&(a, c)| (a.min(c), a.max(c)));
        let at = all.iter().position(|&s| s == primary).unwrap_or(0);
        let n = all.len();
        let next = if forward {
            (at + 1) % n
        } else {
            (at + n - 1) % n
        };
        let p = all.remove(next);
        let doc = self.doc_mut();
        (doc.anchor, doc.cursor) = p;
        doc.extra = all;
    }

    /// `_`: shrink every selection to leave out whitespace at its ends.
    pub fn trim_selections(&mut self) {
        let doc = self.doc();
        let out: Vec<(usize, usize)> = self
            .selections()
            .into_iter()
            .map(|(mut s, mut e)| {
                while s < e && doc.text.char(s).is_whitespace() {
                    s += 1;
                }
                while e > s && doc.text.char(e - 1).is_whitespace() {
                    e -= 1;
                }
                (s, e)
            })
            .collect();
        self.set_selections(out, 0);
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::tests::{editor_with, press};

    fn texts(editor: &crate::editor::Editor) -> Vec<String> {
        let doc = editor.doc();
        let mut v: Vec<String> = editor
            .selections()
            .iter()
            .map(|&(s, e)| doc.text.slice(s..e).to_string())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn alt_s_splits_a_selection_into_lines() {
        let mut editor = editor_with("one\ntwo\nthree\n");
        press(&mut editor, "V V V");
        press(&mut editor, "A-s");
        assert_eq!(texts(&editor), vec!["one", "three", "two"]);
    }

    #[test]
    fn split_keep_and_remove_by_pattern() {
        let mut editor = editor_with("a,b,c\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (0, 5);
        editor.open_select_prompt(crate::editor::SelectPrompt::Split);
        press(&mut editor, ", <enter>");
        assert_eq!(texts(&editor), vec!["a", "b", "c"]);
        editor.open_select_prompt(crate::editor::SelectPrompt::Remove);
        press(&mut editor, "b <enter>");
        assert_eq!(texts(&editor), vec!["a", "c"]);
        editor.open_select_prompt(crate::editor::SelectPrompt::Keep);
        press(&mut editor, "c <enter>");
        assert_eq!(texts(&editor), vec!["c"]);
    }

    #[test]
    fn ampersand_aligns_selection_starts() {
        let mut editor = editor_with("x = 1\nlonger = 2\n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (2, 2); // the `=` on line 0
        doc.extra = vec![(13, 13)]; // the `=` on line 1
        press(&mut editor, "&");
        assert_eq!(editor.doc().text.to_string(), "x      = 1\nlonger = 2\n");
    }

    #[test]
    fn rotate_and_trim() {
        let mut editor = editor_with("  a  \n  b  \n");
        let doc = editor.doc_mut();
        (doc.anchor, doc.cursor) = (0, 5);
        doc.extra = vec![(6, 11)];
        press(&mut editor, "_");
        assert_eq!(texts(&editor), vec!["a", "b"]);
        let before = (editor.doc().anchor, editor.doc().cursor);
        press(&mut editor, ")");
        assert_ne!((editor.doc().anchor, editor.doc().cursor), before);
        press(&mut editor, "(");
        assert_eq!((editor.doc().anchor, editor.doc().cursor), before);
    }
}
