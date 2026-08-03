//! A single open buffer: its text, its cursor, its undo history, and its
//! scroll position.

use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    /// The file's mtime as we last saw it — stamped when we read it and again
    /// after each of our own writes. `None` means there was no file, or the
    /// filesystem would not say. A write refuses when disk disagrees, which is
    /// how a `git checkout` under an open buffer stops being silent data loss.
    pub disk_mtime: Option<SystemTime>,

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
            disk_mtime: None,
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
        // Stamp after the read, never before: a write that lands in between
        // then looks like a mismatch and gets caught, rather than being read
        // over and silently blessed.
        let mut doc = Document {
            text,
            disk_mtime: disk_mtime(&path),
            path: Some(path),
            ..Document::empty()
        };
        doc.refresh_syntax();
        Ok(doc)
    }

    /// (Re)color the buffer if its language has a grammar or a fallback lexer.
    pub fn refresh_syntax(&mut self) {
        let cached = self.syntax.as_ref().and_then(|s| s.config);
        self.syntax = crate::syntax::highlight(self.path.as_deref(), &self.text, cached);
    }

    /// Write the buffer out, unless the file moved underneath us.
    ///
    /// Every write in the editor funnels through here — `:w`, `:wq`, `<space>w`,
    /// `C-s` — so the staleness check lives here once instead of at each caller.
    /// Format-on-save filters the rope through a subprocess without touching the
    /// file, so our own formatting never trips the guard.
    ///
    /// `force` is the `:w!` escape hatch, for when the user has looked and
    /// decided their buffer wins.
    pub fn save(&mut self, force: bool) -> std::io::Result<()> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| std::io::Error::other("buffer has no filename"))?;
        // Only refuse when disk positively contradicts the stamp: a missing or
        // unreadable file has nothing to clobber, and a false refusal traps a
        // buffer just as badly as a clobber loses one.
        let now = disk_mtime(&path);
        if !force && now.is_some() && now != self.disk_mtime {
            return Err(std::io::Error::other(
                "file changed on disk since read — use :w! to overwrite",
            ));
        }
        // `write_to` only `write_all`s its chunks, and a dropped BufWriter
        // discards its flush error — so a truncated write would report success.
        let mut out = BufWriter::new(File::create(&path)?);
        let wrote = self.text.write_to(&mut out).and_then(|()| out.flush());
        // Re-stamp either way. `File::create` truncated the file, so even a
        // failed write is *our* mark on disk; leaving the old stamp would make
        // every later save blame an external process for our own damage.
        self.disk_mtime = disk_mtime(&path);
        wrote?;
        self.modified = false;
        Ok(())
    }

    /// `:w <path>`. Refusing has to happen *before* the buffer is retargeted:
    /// a rejected save that already moved `path` would leave the buffer pointing
    /// at a file it never wrote, and aim the `:w!` retry at the wrong one.
    pub fn save_as(&mut self, path: impl AsRef<Path>, force: bool) -> std::io::Result<()> {
        let path = path.as_ref().to_path_buf();
        if same_file(self.path.as_deref(), &path) {
            return self.save(force); // `:w ./notes.txt` on notes.txt is just `:w`
        }
        if !force && path.exists() {
            return Err(std::io::Error::other("file exists — use :w! to overwrite"));
        }
        self.path = Some(path);
        self.disk_mtime = None;
        self.refresh_syntax();
        self.save(force)
    }

    /// Re-read the mtime after something we ran rewrote the file in place — an
    /// in-place `[fmt]` command, say. Ours, not an external edit, so the guard
    /// must not flag it.
    pub fn restamp(&mut self) {
        self.disk_mtime = self.path.as_deref().and_then(disk_mtime);
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
        let idx = self
            .text
            .char_to_line(self.cursor.min(self.text.len_chars()));
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

/// What the filesystem currently says about a file, or `None` if it is missing
/// or unwilling to say. Both of those collapse to "we do not know", and the
/// caller treats not-knowing as permission to write.
///
// ponytail: mtime only. Two writes inside one filesystem timestamp tick are
// invisible to this; add `metadata.len()` to the comparison if that ever bites.
fn disk_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

/// The same file spelled two ways — `notes.txt` vs `./notes.txt` vs an absolute
/// path. `Path` compares component-wise, so `:w` on the file already open would
/// otherwise look like a save-as onto an existing file and be refused.
fn same_file(current: Option<&Path>, target: &Path) -> bool {
    let Some(current) = current else {
        return false;
    };
    current == target
        || matches!(
            (current.canonicalize(), target.canonicalize()),
            (Ok(a), Ok(b)) if a == b
        )
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
    fn save_refuses_a_file_that_changed_underneath_us() {
        let path = std::env::temp_dir().join(format!("crow-mtime-test-{}", std::process::id()));
        std::fs::write(&path, "original\n").unwrap();

        // Unchanged file: the guard stays out of the way.
        let mut d = Document::open(&path).unwrap();
        d.insert_at_cursor("mine ");
        assert!(d.save(false).is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine original\n");

        // Somebody else writes it. Faking the stamp rather than sleeping keeps
        // the test off the filesystem's timestamp resolution.
        std::fs::write(&path, "theirs\n").unwrap();
        d.disk_mtime = Some(SystemTime::UNIX_EPOCH);
        d.insert_at_cursor("more ");
        assert!(d.save(false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "theirs\n");
        assert!(d.modified, "a refused write must not look saved");

        // :w! goes through, and re-stamping means the next plain :w does too.
        assert!(d.save(true).is_ok());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "mine more original\n"
        );
        assert!(d.save(false).is_ok());

        // A buffer for a file that does not exist yet writes without a fight.
        let fresh = path.with_extension("new");
        let _ = std::fs::remove_file(&fresh);
        let mut n = Document::open(&fresh).unwrap();
        n.insert_at_cursor("hello");
        assert!(n.save(false).is_ok());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&fresh);
    }

    #[test]
    fn a_refused_save_as_leaves_the_buffer_on_its_own_file() {
        let dir = std::env::temp_dir().join(format!("crow-saveas-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("mine.txt");
        let theirs = dir.join("theirs.txt");
        std::fs::write(&mine, "mine\n").unwrap();
        std::fs::write(&theirs, "theirs\n").unwrap();

        let mut d = Document::open(&mine).unwrap();
        d.insert_at_cursor("edited ");

        // `:w theirs.txt` balks, and — the part that matters — does not drag
        // the buffer onto theirs.txt on the way out.
        assert!(d.save_as(&theirs, false).is_err());
        assert_eq!(d.path.as_deref(), Some(mine.as_path()));
        assert_eq!(std::fs::read_to_string(&theirs).unwrap(), "theirs\n");

        // So a plain `:w` still writes the file we were actually editing.
        assert!(d.save(false).is_ok());
        assert_eq!(std::fs::read_to_string(&mine).unwrap(), "edited mine\n");

        // The same file spelled differently is a save, not a save-as.
        assert!(d.save_as(dir.join(".").join("mine.txt"), false).is_ok());
        assert_eq!(d.path.as_deref(), Some(mine.as_path()));

        let _ = std::fs::remove_dir_all(&dir);
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
