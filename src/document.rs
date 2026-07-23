//! A single open buffer: its text, its cursor, its undo history, and its
//! scroll position.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use ropey::{Rope, RopeSlice};

use crate::config::tab_width;
use crate::position::{self};
use crate::transaction::Transaction;

/// One reversible step in the undo history.
///
/// Entries carry a `group` id. Undo pops every entry sharing the id of the
/// entry on top, which is how a whole insert-mode session collapses into a
/// single undo step without having to compose transactions.
struct HistoryEntry {
    forward: Transaction,
    inverse: Transaction,
    cursor_before: usize,
    cursor_after: usize,
    group: usize,
}

pub struct Document {
    pub text: Rope,
    pub path: Option<PathBuf>,
    /// Canonical cursor position: a char index into `text`.
    pub cursor: usize,
    /// Other end of the selection. Equal to `cursor` when nothing is selected;
    /// selecting motions leave it behind, everything else drags it along.
    pub anchor: usize,
    /// Non-primary selections, as (anchor, cursor) pairs. Every transaction
    /// remaps them in `apply`, so they survive edits made by any cursor.
    pub extra: Vec<(usize, usize)>,
    /// Tree-sitter state, when the file's language has a grammar.
    pub syntax: Option<crate::syntax::Syntax>,
    /// Bumped on every text change; lets the LSP layer notice edits.
    pub revision: u64,
    /// Sticky display column for vertical motion, so moving down through a
    /// short line and back out does not lose the original column.
    pub goal_col: Option<usize>,
    pub modified: bool,

    /// First visible line.
    pub view_line: usize,
    /// Horizontal scroll, in display columns.
    pub view_col: usize,

    history: Vec<HistoryEntry>,
    redo_stack: Vec<HistoryEntry>,
    group: usize,
}

impl Document {
    pub fn empty() -> Self {
        Document {
            text: Rope::new(),
            path: None,
            cursor: 0,
            anchor: 0,
            extra: Vec::new(),
            syntax: None,
            revision: 0,
            goal_col: None,
            modified: false,
            view_line: 0,
            view_col: 0,
            history: Vec::new(),
            redo_stack: Vec::new(),
            group: 0,
        }
    }

    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let text = if path.exists() {
            Rope::from_reader(BufReader::new(File::open(&path)?))?
        } else {
            Rope::new()
        };
        let mut doc = Document {
            text,
            path: Some(path),
            ..Document::empty()
        };
        doc.refresh_syntax();
        Ok(doc)
    }

    /// (Re)parse the buffer if its language has a grammar.
    pub fn refresh_syntax(&mut self) {
        let config = self
            .syntax
            .as_ref()
            .map(|s| s.config)
            .or_else(|| crate::syntax::config_for(self.path.as_deref()));
        self.syntax = config.and_then(|c| crate::syntax::parse(c, &self.text));
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        let path = self.path.clone().ok_or_else(|| {
            std::io::Error::other("buffer has no filename")
        })?;
        self.text.write_to(BufWriter::new(File::create(&path)?))?;
        self.modified = false;
        Ok(())
    }

    pub fn save_as(&mut self, path: impl AsRef<Path>) -> std::io::Result<()> {
        self.path = Some(path.as_ref().to_path_buf());
        self.refresh_syntax();
        self.save()
    }

    pub fn name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[no name]".to_string())
    }

    // ---- line geometry -----------------------------------------------------

    /// Number of lines, not counting the phantom empty line ropey reports after
    /// a trailing newline.
    pub fn line_count(&self) -> usize {
        let n = self.text.len_lines();
        if n > 1 && self.text.line(n - 1).len_chars() == 0 {
            n - 1
        } else {
            n
        }
    }

    pub fn line(&self, idx: usize) -> RopeSlice<'_> {
        self.text.line(idx)
    }

    /// Length of a line in chars, excluding its line ending.
    pub fn line_len(&self, idx: usize) -> usize {
        if idx >= self.text.len_lines() {
            return 0;
        }
        position::line_len_without_newline(self.text.line(idx))
    }

    pub fn cursor_line(&self) -> usize {
        let idx = self.text.char_to_line(self.cursor.min(self.text.len_chars()));
        // A cursor sitting just past a trailing newline lands on the phantom
        // final line ropey reports; pull it back onto a real one.
        idx.min(self.line_count().saturating_sub(1))
    }

    /// Cursor as (line, char offset within line).
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let line = self.cursor_line();
        let line_start = self.text.line_to_char(line);
        (line, self.cursor - line_start)
    }

    /// Cursor as (line, display column).
    pub fn cursor_display(&self) -> (usize, usize) {
        let (line, col) = self.cursor_line_col();
        (
            line,
            position::char_to_display_col(self.text.line(line), col, tab_width()),
        )
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.text.line_to_char(line)
    }

    pub fn line_end(&self, line: usize) -> usize {
        self.line_start(line) + self.line_len(line)
    }

    /// Clamp the cursor into a valid position.
    ///
    /// In normal mode the cursor sits *on* a character, so it stops one short
    /// of the line end. In insert mode it sits *between* characters and may sit
    /// past the last one.
    pub fn clamp_cursor(&mut self, allow_past_end: bool) {
        let max = self.text.len_chars();
        if self.cursor > max {
            self.cursor = max;
        }
        let line = self.cursor_line();
        let start = self.line_start(line);
        let len = self.line_len(line);
        let limit = if allow_past_end || len == 0 {
            len
        } else {
            len - 1
        };
        if self.cursor > start + limit {
            self.cursor = start + limit;
        }
    }

    // ---- editing -----------------------------------------------------------

    /// Apply a transaction and record it in the undo history.
    pub fn apply(&mut self, tx: Transaction, new_cursor: usize) {
        if tx.is_empty() {
            self.cursor = new_cursor;
            return;
        }

        let inverse = tx.invert(&self.text);
        let cursor_before = self.cursor;

        tx.apply(&mut self.text);
        self.cursor = new_cursor.min(self.text.len_chars());
        self.anchor = self.cursor;
        // Every other cursor rides through the edit via position mapping.
        for (a, c) in &mut self.extra {
            *a = tx.map_pos(*a, false);
            *c = tx.map_pos(*c, false);
        }
        self.modified = true;
        self.goal_col = None;

        self.history.push(HistoryEntry {
            forward: tx,
            inverse,
            cursor_before,
            cursor_after: self.cursor,
            group: self.group,
        });
        self.redo_stack.clear();
        self.revision += 1;
        self.refresh_syntax();
    }

    /// Close the current undo group. The next edit starts a new one.
    ///
    /// Called when leaving insert mode, so a burst of typing undoes as a unit.
    pub fn commit_undo_group(&mut self) {
        if self
            .history
            .last()
            .is_some_and(|entry| entry.group == self.group)
        {
            self.group += 1;
        }
    }

    pub fn undo(&mut self) -> bool {
        let target = match self.history.last() {
            Some(entry) => entry.group,
            None => return false,
        };
        // ponytail: undo restores one cursor, not the fleet; per-entry cursor
        // sets would need recording extras in HistoryEntry.
        self.extra.clear();

        while self
            .history
            .last()
            .is_some_and(|entry| entry.group == target)
        {
            let entry = self.history.pop().unwrap();
            entry.inverse.apply(&mut self.text);
            self.cursor = entry.cursor_before.min(self.text.len_chars());
            self.anchor = self.cursor;
            self.redo_stack.push(entry);
        }

        self.modified = true;
        self.goal_col = None;
        // Any further edit must not join the group we just undid.
        self.group += 1;
        self.revision += 1;
        self.refresh_syntax();
        true
    }

    pub fn redo(&mut self) -> bool {
        let target = match self.redo_stack.last() {
            Some(entry) => entry.group,
            None => return false,
        };
        self.extra.clear();

        while self
            .redo_stack
            .last()
            .is_some_and(|entry| entry.group == target)
        {
            let entry = self.redo_stack.pop().unwrap();
            entry.forward.apply(&mut self.text);
            self.cursor = entry.cursor_after.min(self.text.len_chars());
            self.anchor = self.cursor;
            self.history.push(entry);
        }

        self.modified = true;
        self.goal_col = None;
        self.revision += 1;
        self.refresh_syntax();
        true
    }

    // ---- convenience edits -------------------------------------------------

    /// Insert `s` at every cursor, as one transaction.
    ///
    /// This is what makes multi-cursor typing work: insert mode calls this per
    /// keystroke and every cursor gets the text in a single history entry.
    pub fn insert_at_cursor(&mut self, s: &str) {
        if self.extra.is_empty() {
            let tx = Transaction::insert(&self.text, self.cursor, s);
            let new_cursor = self.cursor + s.chars().count();
            self.apply(tx, new_cursor);
            return;
        }

        let mut points: Vec<usize> = std::iter::once(self.cursor)
            .chain(self.extra.iter().map(|&(_, c)| c))
            .collect();
        points.sort_unstable();
        points.dedup();
        let tx = Transaction::change(
            &self.text,
            points.iter().map(|&p| (p, p, Some(s.to_string()))),
        );
        let new_cursor = tx.map_pos(self.cursor, false);
        self.apply(tx, new_cursor);
    }

    /// Drop extra selections that duplicate the primary or each other.
    pub fn dedupe_cursors(&mut self) {
        let primary = (self.anchor, self.cursor);
        self.extra.retain(|&e| e != primary);
        self.extra.sort_unstable();
        self.extra.dedup();
    }

    pub fn delete_range(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let to = to.min(self.text.len_chars());
        let tx = Transaction::delete(&self.text, from, to);
        self.apply(tx, from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> Document {
        Document {
            text: Rope::from_str(s),
            ..Document::empty()
        }
    }

    #[test]
    fn trailing_newline_does_not_add_a_line() {
        assert_eq!(doc("a\nb\n").line_count(), 2);
        assert_eq!(doc("a\nb").line_count(), 2);
        assert_eq!(doc("").line_count(), 1);
    }

    #[test]
    fn undo_groups_collapse_a_typing_burst() {
        let mut d = doc("");
        for c in "hello".chars() {
            d.insert_at_cursor(&c.to_string());
        }
        assert_eq!(d.text.to_string(), "hello");

        // One undo, because every keystroke shared a group.
        d.undo();
        assert_eq!(d.text.to_string(), "");
    }

    #[test]
    fn committing_starts_a_new_undo_group() {
        let mut d = doc("");
        d.insert_at_cursor("abc");
        d.commit_undo_group();
        d.insert_at_cursor("def");
        assert_eq!(d.text.to_string(), "abcdef");

        d.undo();
        assert_eq!(d.text.to_string(), "abc");
        d.undo();
        assert_eq!(d.text.to_string(), "");
    }

    #[test]
    fn redo_replays_a_whole_group() {
        let mut d = doc("");
        d.insert_at_cursor("abc");
        d.commit_undo_group();
        d.undo();
        assert_eq!(d.text.to_string(), "");
        d.redo();
        assert_eq!(d.text.to_string(), "abc");
    }

    #[test]
    fn editing_after_undo_clears_redo() {
        let mut d = doc("");
        d.insert_at_cursor("abc");
        d.commit_undo_group();
        d.undo();
        d.insert_at_cursor("xyz");
        assert!(!d.redo());
        assert_eq!(d.text.to_string(), "xyz");
    }

    #[test]
    fn undo_restores_cursor() {
        let mut d = doc("hello");
        d.cursor = 5;
        d.insert_at_cursor(" world");
        assert_eq!(d.cursor, 11);
        d.undo();
        assert_eq!(d.cursor, 5);
    }

    #[test]
    fn normal_mode_cursor_stops_before_line_end() {
        let mut d = doc("abc\ndef");
        d.cursor = 3;
        d.clamp_cursor(false);
        assert_eq!(d.cursor, 2);

        d.cursor = 3;
        d.clamp_cursor(true);
        assert_eq!(d.cursor, 3);
    }
}
