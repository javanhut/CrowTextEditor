//! Operations on the whole set of selections: split, filter, align, rotate,
//! trim. Every selection is a half-open (anchor, cursor) char range; these
//! hand back forward ones (anchor at the start).

use super::*;
use crate::config::tab_width;
use crate::position;
use crate::transaction::Transaction;

/// `C-v`'s rectangle: two opposite corners as (line, display column). The
/// origin is where it was started; the corner is the one motions move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    origin: (usize, usize),
    corner: (usize, usize),
}

impl Editor {
    /// Are selections drawn with the cursor on their last character, not
    /// past it? True while one is being stretched, by `v` or by `C-v`.
    pub fn inclusive(&self) -> bool {
        self.extend || self.block.is_some()
    }

    /// `C-v`: start a block at the cursor, or stop stretching the one there
    /// is and keep its selections.
    pub fn toggle_block(&mut self) {
        self.keep_selection = true;
        if self.block.take().is_some() {
            return;
        }
        self.extend = false;
        let doc = self.doc_mut();
        doc.extra.clear();
        let (line, col) = doc.cursor_line_col();
        let col = position::char_to_display_col(doc.line(line), col, tab_width());
        self.block = Some(Block {
            origin: (line, col),
            corner: (line, col),
        });
        self.block_select();
    }

    /// Before a motion in block mode: one bare cursor, on the corner, holding
    /// the corner's column through lines too short to reach it.
    pub(crate) fn block_to_corner(&mut self) {
        let Some(Block { corner, .. }) = self.block else {
            return;
        };
        let doc = self.doc_mut();
        let line = corner.0.min(doc.line_count().saturating_sub(1));
        let at = doc.line_start(line)
            + position::display_col_to_char(doc.line(line), corner.1, tab_width());
        doc.extra.clear();
        (doc.anchor, doc.cursor) = (at, at);
        doc.clamp_cursor(false);
        doc.goal_col = Some(corner.1);
    }

    /// After it: the corner is wherever the cursor went. A vertical motion
    /// leaves its column in `goal_col`; a horizontal one clears that, and
    /// the cursor's own column is the new one.
    pub(crate) fn block_stretch(&mut self) {
        let doc = self.doc();
        let (line, col) = doc.cursor_line_col();
        let col = doc
            .goal_col
            .unwrap_or_else(|| position::char_to_display_col(doc.line(line), col, tab_width()));
        if let Some(block) = self.block.as_mut() {
            block.corner = (line, col);
        }
        self.block_select();
    }

    /// The block's selections, handed to the command that ends it. `i` and
    /// `a` type at the cursor, so the cursors go to the rectangle's left
    /// edge, or onto its right one. Insert and comment want the lines the
    /// rectangle only touches; a delete there would eat the line break.
    pub(crate) fn block_finish(&mut self, command: &str) {
        let doc = self.doc_mut();
        let text = doc.text.clone();
        let mut sels: Vec<&mut (usize, usize)> = doc.extra.iter_mut().collect();
        let mut primary = (doc.anchor, doc.cursor);
        sels.push(&mut primary);
        for sel in sels {
            let (a, c) = *sel;
            match command {
                "insert_mode" => *sel = (a, a),
                "append" if a < c => {
                    let last = position::prev_grapheme_boundary(text.slice(..), c);
                    *sel = (last, last);
                }
                _ => {}
            }
        }
        (doc.anchor, doc.cursor) = primary;
        if matches!(command, "delete_selection" | "change_selection") {
            doc.extra.retain(|(a, c)| a != c);
        }
    }

    /// One selection per line of the rectangle. A line that ends before the
    /// rectangle begins has nothing in it and gets none — except the corner's
    /// own, which is where the cursor has to be.
    fn block_select(&mut self) {
        let Some(Block { origin, corner }) = self.block else {
            return;
        };
        let doc = self.doc();
        let (left, right) = (origin.1.min(corner.1), origin.1.max(corner.1));
        let mut sels = Vec::new();
        let mut primary = 0;
        for line in origin.0.min(corner.0)..=origin.0.max(corner.0) {
            let text = doc.line(line);
            let len = doc.line_len(line);
            let from = position::display_col_to_char(text, left, tab_width()).min(len);
            let width = position::char_to_display_col(text, len, tab_width());
            if width < left && line != corner.0 {
                continue;
            }
            let to = position::display_col_to_char(text, right + 1, tab_width()).min(len);
            if line == corner.0 {
                primary = sels.len();
            }
            let start = doc.line_start(line);
            sels.push((start + from, start + to.max(from)));
        }
        let goal = self.doc().goal_col;
        self.set_selections(sels, primary);
        self.doc_mut().goal_col = goal;
    }

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

    /// `C-v` and motions draw a rectangle; what comes next runs on every
    /// line of it, as one undo step.
    #[test]
    fn a_block_is_one_selection_per_line() {
        let mut editor = editor_with("abcdef\nabcdef\nabcdef\n");
        press(&mut editor, "l C-v j j l");
        assert_eq!(texts(&editor), ["bc", "bc", "bc"]);
        // Stretching back the other way shrinks it, and crosses the origin.
        press(&mut editor, "k h h");
        assert_eq!(texts(&editor), ["ab", "ab"]);
        press(&mut editor, "d");
        assert!(editor.block.is_none(), "an edit is what the block was for");
        assert_eq!(editor.doc().text.to_string(), "cdef\ncdef\nabcdef\n");
        press(&mut editor, "u");
        assert_eq!(editor.doc().text.to_string(), "abcdef\nabcdef\nabcdef\n");
    }

    #[test]
    fn a_block_inserts_and_appends_on_every_line() {
        let mut editor = editor_with("one\ntwo\nthree\n");
        press(&mut editor, "C-v j j i <space> <space> <esc>");
        assert_eq!(editor.doc().text.to_string(), "  one\n  two\n  three\n");
        press(&mut editor, "<esc> gg C-v j j a - <esc>");
        assert_eq!(editor.doc().text.to_string(), " - one\n - two\n - three\n");
    }

    /// The column is the rectangle's, not whatever a short line clamps the
    /// cursor to on the way through; and a line the rectangle misses is left
    /// alone — but an insert at column 0 still reaches an empty one.
    #[test]
    fn a_block_keeps_its_column_through_short_lines() {
        let mut editor = editor_with("abcdef\nab\n\nabcdef\n");
        press(&mut editor, "l l l C-v j j j");
        assert_eq!(texts(&editor), ["d", "d"]);
        press(&mut editor, "d");
        assert_eq!(editor.doc().text.to_string(), "abcef\nab\n\nabcef\n");

        let mut editor = editor_with("a\n\nb\n");
        press(&mut editor, "C-v j j d");
        assert_eq!(editor.doc().text.to_string(), "\n\n\n", "the empty line survives");
        let mut editor = editor_with("a\n\nb\n");
        press(&mut editor, "C-v j j i # <esc>");
        assert_eq!(editor.doc().text.to_string(), "#a\n#\n#b\n");
    }

    /// `I` and `A` go to each line's own start and end, however ragged.
    #[test]
    fn line_start_and_end_inserts_reach_every_cursor() {
        let mut editor = editor_with("  one\n    three\n");
        press(&mut editor, "C-v j I / / <space> <esc>");
        assert_eq!(editor.doc().text.to_string(), "  // one\n    // three\n");
        press(&mut editor, "<esc> gg C-v j A ; <esc>");
        assert_eq!(editor.doc().text.to_string(), "  // one;\n    // three;\n");
    }

    #[test]
    fn leaving_a_block() {
        let mut editor = editor_with("abc\nabc\n");
        // C-v again keeps the cursors for plain multi-cursor work…
        press(&mut editor, "C-v j C-v");
        assert!(editor.block.is_none());
        assert_eq!(editor.doc().extra.len(), 1);
        // …and Esc drops them.
        press(&mut editor, "C-v j <esc>");
        assert!(editor.block.is_none());
        assert!(editor.doc().extra.is_empty());
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
