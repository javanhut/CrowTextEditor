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

/// One edit as the language server wants to hear about it: a range in the
/// text as the server last saw it, in (line, UTF-16 column), and what replaces
/// it. Changes are logged in the order they must be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LspChange {
    pub start: (usize, usize),
    pub end: (usize, usize),
    pub text: String,
}

/// More logged changes than this and a full resync is cheaper to send.
const LSP_LOG_CAP: usize = 2000;

/// Undo entries kept in the persistent undo file; older ones are dropped.
const UNDO_FILE_ENTRIES: usize = 1000;

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
    /// `syntax` is stale and must be rebuilt before anything reads it.
    syntax_dirty: bool,
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
    /// An mtime we've already told the user about, so a modified buffer whose
    /// file changed on disk is flagged once rather than on every check.
    pub disk_conflict: Option<SystemTime>,

    /// Edits since the language server was last synced (see `LspChange`),
    /// starting from revision `lsp_base`. `lsp_overflow` means the log was
    /// abandoned and the next sync must send the whole text.
    lsp_log: Vec<LspChange>,
    lsp_base: u64,
    lsp_overflow: bool,

    /// The revision last written to this buffer's swap file.
    pub swap_revision: u64,
    /// An earlier session left a swap file here that differs from the file.
    pub swap_found: bool,
    /// The user has been told about `swap_found`.
    pub swap_notified: bool,

    /// Change markers against the last ivaldi seal.
    pub vcs: crate::vcs::DocState,

    /// First visible line.
    pub view_line: usize,
    /// Visual row of `view_line` drawn first. Always 0 without soft wrap; with
    /// it, a single line can be taller than the screen (one markdown paragraph
    /// on one line), so the viewport has to be able to start inside a line.
    pub view_row: usize,
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
            syntax_dirty: false,
            revision: 0,
            goal_col: None,
            modified: false,
            disk_mtime: None,
            disk_conflict: None,
            lsp_log: Vec::new(),
            lsp_base: 0,
            lsp_overflow: false,
            swap_revision: 0,
            swap_found: false,
            swap_notified: false,
            vcs: crate::vcs::DocState::default(),
            view_line: 0,
            view_row: 0,
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
        if crate::config::persistent_undo() {
            doc.load_undo();
        }
        if crate::config::swap_files() {
            match doc.read_swap() {
                Some(swapped) if doc.text != swapped.as_str() => doc.swap_found = true,
                Some(_) => doc.remove_swap(), // nothing it could restore
                None => {}
            }
        }
        Ok(doc)
    }

    /// Re-read the file from disk as one undoable edit, so a reload you
    /// didn't want is a `u` away and cursors outside the changed stretch stay
    /// where they were. False when disk and buffer already agree.
    pub fn reload(&mut self) -> std::io::Result<bool> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| std::io::Error::other("buffer has no filename"))?;
        let new = std::fs::read_to_string(&path)?;
        let mtime = disk_mtime(&path);
        let changed = self.replace_all(&new);
        self.modified = false;
        self.disk_mtime = mtime;
        self.disk_conflict = None;
        Ok(changed)
    }

    /// Make the buffer read `new`, as one undo step touching only the stretch
    /// that differs. False when it already did.
    pub fn replace_all(&mut self, new: &str) -> bool {
        // Both sides are walked as iterators — forwards from the start, then
        // backwards from the end — so a reload costs one pass over the text
        // rather than a `Vec<char>` copy of the whole file on each side.
        let old_len = self.text.len_chars();
        let new_len = new.chars().count();
        let prefix = self
            .text
            .chars()
            .zip(new.chars())
            .take_while(|&(a, b)| a == b)
            .count();
        if prefix == old_len && prefix == new_len {
            return false;
        }
        // The shared tail, stopping before it can overlap the shared head.
        let mut suffix = 0;
        let longest = (old_len - prefix).min(new_len - prefix);
        let mut old_back = self.text.chars_at(old_len);
        let mut new_back = new.chars().rev();
        while suffix < longest {
            match (old_back.prev(), new_back.next()) {
                (Some(a), Some(b)) if a == b => suffix += 1,
                _ => break,
            }
        }
        let inserted: String = new
            .chars()
            .skip(prefix)
            .take(new_len - suffix - prefix)
            .collect();
        self.commit_undo_group();
        let tx = Transaction::change(&self.text, [(prefix, old_len - suffix, Some(inserted))]);
        // Positions at the start of the changed stretch stay there rather
        // than being pushed past the new text.
        let (cursor, anchor) = (tx.map_pos(self.cursor, true), tx.map_pos(self.anchor, true));
        self.apply(tx, cursor);
        self.anchor = anchor.min(self.text.len_chars());
        self.clamp_cursor(false);
        self.commit_undo_group();
        true
    }

    /// Mark the buffer's coloring stale. The parse itself is deferred to
    /// `settle_syntax`, because it is a full-file tree-sitter reparse and
    /// doing one per keystroke is what made typing lag behind the keyboard.
    pub fn refresh_syntax(&mut self) {
        self.syntax_dirty = true;
    }

    /// Whether a reparse is owed.
    pub fn syntax_stale(&self) -> bool {
        self.syntax_dirty
    }

    /// Rebuild the coloring if it is stale. Called once per frame, and by the
    /// one command that reads the tree rather than the spans.
    pub fn settle_syntax(&mut self) {
        if !self.syntax_dirty {
            return;
        }
        self.syntax_dirty = false;
        let cached = self.syntax.as_ref().and_then(|s| s.config);
        self.syntax = crate::syntax::highlight(self.path.as_deref(), &self.text, cached);
    }

    /// Reparse after an edit, reusing the tree we already have.
    ///
    /// This is the difference between typing in a big file feeling instant and
    /// feeling like a modem: a full reparse is O(file), an edited one is
    /// O(change). The spans are only marked stale here — they get recollected
    /// for whatever is on screen, in `highlight_range`.
    fn edit_syntax(&mut self, before: &Rope, range: Option<(usize, usize, usize)>) {
        let Some((start, old_end, new_end)) = range else {
            return;
        };
        let mut syntax = self.syntax.take();
        match syntax.as_mut() {
            Some(s) => {
                s.dirty = true;
                if let (Some(config), Some(tree)) = (s.config, s.tree.as_mut()) {
                    tree.edit(&crate::syntax::input_edit(
                        before, &self.text, start, old_end, new_end,
                    ));
                    // A failed reparse leaves the edited tree in place: its
                    // ranges are still right, only its shape is stale.
                    if let Some(new) = crate::syntax::parse_tree(config, &self.text, Some(tree)) {
                        s.tree = Some(new);
                    }
                }
                self.syntax = syntax;
            }
            // No syntax yet (a scratch buffer that just got a path): try again.
            None => self.refresh_syntax(),
        }
    }

    /// Collect highlight spans for the lines `first..=last`, if they aren't
    /// covered already. Called once per frame, before drawing.
    pub fn highlight_range(&mut self, first_line: usize, last_line: usize) {
        let Some(mut syntax) = self.syntax.take() else {
            return;
        };
        let lines = self.text.len_lines();
        let from = self.text.line_to_char(first_line.min(lines - 1));
        // `min(lines)` would be out of range; the text's end is the same
        // thing and keeps the last line — which has no line after it to
        // borrow an end from — inside the range that gets coloured.
        let to = if last_line + 1 >= lines {
            self.text.len_chars()
        } else {
            self.text.line_to_char(last_line + 1)
        };
        syntax.update(&self.text, from, to.max(from));
        self.syntax = Some(syntax);
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
        self.disk_conflict = None;
        // Our own text is on disk now, so our swap file has done its job —
        // but one holding an earlier session's work keeps it until `:recover`
        // or `:recover!` says what to do with it.
        if !self.swap_found {
            self.remove_swap();
        }
        if crate::config::persistent_undo() {
            self.save_undo();
        }
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
        // The swap file belongs to the file we are leaving, not the one we
        // are about to write; `save` below would remove the new path's.
        self.remove_swap();
        self.path = Some(path);
        self.disk_mtime = None;
        self.swap_revision = 0;
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

    /// Screen rows a line takes. `wrap` is the soft-wrap width, or `None` when
    /// long lines scroll sideways instead — then every line is one row.
    pub fn visual_rows(&self, line: usize, wrap: Option<usize>) -> usize {
        match wrap {
            Some(w) => position::wrap_offsets(self.line(line), w, tab_width()).len(),
            None => 1,
        }
    }

    /// The cursor as (line, visual row within that line, display column within
    /// that row). Without wrapping the row is always 0 and the column is the
    /// line's own display column.
    pub fn cursor_visual(&self, wrap: Option<usize>) -> (usize, usize, usize) {
        self.position_visual(self.cursor, wrap)
    }

    /// Any document position in the same coordinates as `cursor_visual`.
    pub fn position_visual(&self, pos: usize, wrap: Option<usize>) -> (usize, usize, usize) {
        let pos = pos.min(self.text.len_chars());
        let line = self.text.char_to_line(pos);
        let col = pos - self.line_start(line);
        let slice = self.line(line);
        let Some(width) = wrap else {
            return (
                line,
                0,
                position::char_to_display_col(slice, col, tab_width()),
            );
        };
        let offsets = position::wrap_offsets(slice, width, tab_width());
        let row = offsets.partition_point(|&o| o <= col).saturating_sub(1);
        (
            line,
            row,
            position::display_col_between(slice, offsets[row], col, tab_width()),
        )
    }

    /// Visual rows from `(line, row)` `a` forward to `b`, where `a <= b`.
    pub fn rows_forward(&self, wrap: Option<usize>, a: (usize, usize), b: (usize, usize)) -> usize {
        let mut n = 0isize;
        for l in a.0..b.0.min(self.line_count()) {
            n += self.visual_rows(l, wrap) as isize;
        }
        (n + b.1 as isize - a.1 as isize).max(0) as usize
    }

    /// Step the viewport `n` visual rows: down when positive, up when negative.
    pub fn scroll_view(&mut self, wrap: Option<usize>, n: isize) {
        let last = self.line_count().saturating_sub(1);
        for _ in 0..n.unsigned_abs() {
            if n > 0 {
                if self.view_row + 1 < self.visual_rows(self.view_line, wrap) {
                    self.view_row += 1;
                } else if self.view_line < last {
                    self.view_line += 1;
                    self.view_row = 0;
                } else {
                    break;
                }
            } else if self.view_row > 0 {
                self.view_row -= 1;
            } else if self.view_line > 0 {
                self.view_line -= 1;
                self.view_row = self.visual_rows(self.view_line, wrap) - 1;
            } else {
                break;
            }
        }
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
        let range = tx.changed_range();
        let before = self.text.clone(); // ropes are copy-on-write: this is cheap
        self.log_lsp(&tx);

        tx.apply(&mut self.text);
        self.cursor = new_cursor.min(self.text.len_chars());
        self.anchor = self.cursor;
        // Every other cursor rides through the edit via position mapping.
        for (a, c) in &mut self.extra {
            *a = tx.map_pos(*a, false);
            *c = tx.map_pos(*c, false);
        }
        // Slide the existing highlight spans through the edit, exactly as the
        // cursors just were. The reparse that replaces them waits for a gap in
        // typing, and until it lands these keep every colour on the text it
        // belongs to instead of one character behind it.
        if let Some(syntax) = &mut self.syntax {
            for (start, end, _) in &mut syntax.spans {
                *start = tx.map_pos(*start, true);
                *end = tx.map_pos(*end, false);
            }
            syntax.spans.retain(|&(start, end, _)| start < end);
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
        self.edit_syntax(&before, range);
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
            let before = self.text.clone();
            let range = entry.inverse.changed_range();
            self.log_lsp(&entry.inverse);
            entry.inverse.apply(&mut self.text);
            self.edit_syntax(&before, range);
            self.cursor = entry.cursor_before.min(self.text.len_chars());
            self.anchor = self.cursor;
            self.redo_stack.push(entry);
        }

        self.modified = true;
        self.goal_col = None;
        // Any further edit must not join the group we just undid.
        self.group += 1;
        self.revision += 1;
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
            let before = self.text.clone();
            let range = entry.forward.changed_range();
            self.log_lsp(&entry.forward);
            entry.forward.apply(&mut self.text);
            self.edit_syntax(&before, range);
            self.cursor = entry.cursor_after.min(self.text.len_chars());
            self.anchor = self.cursor;
            self.history.push(entry);
        }

        self.modified = true;
        self.goal_col = None;
        self.revision += 1;
        true
    }

    // ---- language-server sync ----------------------------------------------

    /// Record `tx` in the change log, in the coordinates of the text it is
    /// about to be applied to. A multi-cursor transaction logs its changes
    /// last-first: each one then only moves text after the ones still to come,
    /// so all of them can be stated against the same original text.
    fn log_lsp(&mut self, tx: &Transaction) {
        if self.lsp_overflow {
            return;
        }
        let changes = tx.changes();
        if self.lsp_log.len() + changes.len() > LSP_LOG_CAP {
            self.lsp_overflow = true;
            self.lsp_log.clear();
            return;
        }
        let text = &self.text;
        let at = |pos: usize| {
            let line = text.char_to_line(pos);
            let col = text.char_to_utf16_cu(pos) - text.char_to_utf16_cu(text.line_to_char(line));
            (line, col)
        };
        for (from, to, inserted) in changes.into_iter().rev() {
            self.lsp_log.push(LspChange {
                start: at(from),
                end: at(to),
                text: inserted,
            });
        }
    }

    /// The edits since the server was synced at `synced_revision`, if the log
    /// covers exactly that span; `None` means send the whole text. Either way
    /// the log restarts from the current revision.
    pub fn take_lsp_changes(&mut self, synced_revision: u64) -> Option<Vec<LspChange>> {
        let usable = !self.lsp_overflow && self.lsp_base == synced_revision;
        let log = std::mem::take(&mut self.lsp_log);
        self.reset_lsp_log();
        usable.then_some(log)
    }

    /// The server has just been sent the whole text.
    pub fn reset_lsp_log(&mut self) {
        self.lsp_log.clear();
        self.lsp_overflow = false;
        self.lsp_base = self.revision;
    }

    // ---- persistent undo ----------------------------------------------------

    /// Write the undo history beside nothing: into the state dir, keyed by
    /// the file's path and stamped with a hash of the text it applies to.
    fn save_undo(&self) {
        let Some(file) = self.path.as_deref().and_then(state_file("undo", "")) else {
            return;
        };
        if self.history.is_empty() && self.redo_stack.is_empty() {
            let _ = std::fs::remove_file(&file);
            return;
        }
        let entry = |e: &HistoryEntry| {
            serde_json::json!({
                "f": e.forward.to_json(),
                "i": e.inverse.to_json(),
                "cb": e.cursor_before,
                "ca": e.cursor_after,
                "g": e.group,
            })
        };
        let keep = self.history.len().saturating_sub(UNDO_FILE_ENTRIES);
        let body = serde_json::json!({
            "version": 1,
            "hash": format!("{:016x}", rope_hash(&self.text)),
            "len": self.text.len_chars(),
            "group": self.group,
            "history": self.history[keep..].iter().map(entry).collect::<Vec<_>>(),
            "redo": self.redo_stack.iter().map(entry).collect::<Vec<_>>(),
        });
        write_atomically(&file, body.to_string().as_bytes());
    }

    /// Bring back the history `save_undo` wrote — only if the file on disk is
    /// the very text it was written for. Anything else (edited elsewhere, a
    /// checkout) and the history would replay onto the wrong text.
    fn load_undo(&mut self) {
        let Some(file) = self.path.as_deref().and_then(state_file("undo", "")) else {
            return;
        };
        let Ok(raw) = std::fs::read_to_string(&file) else {
            return;
        };
        let Ok(body) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return;
        };
        let len = self.text.len_chars();
        if body["hash"].as_str() != Some(format!("{:016x}", rope_hash(&self.text)).as_str())
            || body["len"].as_u64() != Some(len as u64)
        {
            return;
        }
        let entry = |v: &serde_json::Value| {
            Some(HistoryEntry {
                forward: Transaction::from_json(&v["f"])?,
                inverse: Transaction::from_json(&v["i"])?,
                cursor_before: v["cb"].as_u64()? as usize,
                cursor_after: v["ca"].as_u64()? as usize,
                group: v["g"].as_u64()? as usize,
            })
        };
        let list = |key: &str| -> Option<Vec<HistoryEntry>> {
            body[key].as_array()?.iter().map(entry).collect()
        };
        let (Some(history), Some(redo)) = (list("history"), list("redo")) else {
            return;
        };
        // Every entry has to fit the text the one before it leaves behind,
        // walking each stack from the top down. A single bad length anywhere
        // would otherwise panic ropey on the undo that reaches it.
        let chain_ok = |entries: &[HistoryEntry], undoing: bool| {
            let mut expect = len;
            for e in entries.iter().rev() {
                let (input, output) = if undoing {
                    (&e.inverse, &e.forward)
                } else {
                    (&e.forward, &e.inverse)
                };
                if input.input_len() != expect {
                    return false;
                }
                expect = output.input_len();
            }
            true
        };
        if !chain_ok(&history, true) || !chain_ok(&redo, false) {
            return;
        }
        let top = history
            .iter()
            .chain(&redo)
            .map(|e| e.group)
            .max()
            .unwrap_or(0);
        self.group = (body["group"].as_u64().unwrap_or(0) as usize).max(top + 1);
        self.history = history;
        self.redo_stack = redo;
    }

    // ---- swap files ---------------------------------------------------------

    /// Save the unsaved text aside, if it moved since the last time. True
    /// when something was written.
    pub fn write_swap(&mut self) -> bool {
        // `swap_found` means the file holds an earlier session's unsaved work
        // that has not been recovered or discarded yet. Writing over it would
        // destroy exactly what it exists to keep.
        if !crate::config::swap_files()
            || self.swap_found
            || !self.modified
            || self.revision == self.swap_revision
        {
            return false;
        }
        let Some(path) = self.path.clone() else {
            return false;
        };
        let Some(file) = state_file("swap", ".swp")(&path) else {
            return false;
        };
        let mut body = format!(
            "crow-swap\n{}\n",
            path.canonicalize().unwrap_or(path).display()
        );
        body.push_str(&self.text.to_string());
        write_atomically(&file, body.as_bytes());
        self.swap_revision = self.revision;
        true
    }

    /// The text a swap file holds for this buffer's file, if there is one.
    pub fn read_swap(&self) -> Option<String> {
        let path = self.path.as_deref()?;
        let raw = std::fs::read_to_string(state_file("swap", ".swp")(path)?).ok()?;
        let rest = raw.strip_prefix("crow-swap\n")?;
        let (named, text) = rest.split_once('\n')?;
        // Two paths can hash to one file name; the header says which file the
        // text belongs to, and text from another file must not be offered.
        let mine = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        (named == mine.to_string_lossy()).then(|| text.to_string())
    }

    pub fn remove_swap(&mut self) {
        if let Some(file) = self.path.as_deref().and_then(state_file("swap", ".swp")) {
            let _ = std::fs::remove_file(file);
        }
        self.swap_found = false;
        self.swap_revision = self.revision;
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

/// FNV-1a over the rope's bytes: stable across runs and Rust versions,
/// which `DefaultHasher` does not promise, and that is all it is for.
fn rope_hash(text: &Rope) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for chunk in text.chunks() {
        for b in chunk.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// `state_dir/<kind>/<hash of the file's absolute path><ext>`, as a closure
/// so it reads the same at every call site.
fn state_file(kind: &'static str, ext: &'static str) -> impl Fn(&Path) -> Option<PathBuf> {
    move |path: &Path| {
        let abs = path
            .canonicalize()
            .ok()
            .or_else(|| std::env::current_dir().ok().map(|d| d.join(path)))?;
        let key = rope_hash(&Rope::from_str(&abs.to_string_lossy()));
        Some(
            crate::config::state_dir()
                .join(kind)
                .join(format!("{key:016x}{ext}")),
        )
    }
}

/// Write via a temp file and a rename, so a crash mid-write leaves the old
/// file rather than half a new one. Best effort: these files are a safety net,
/// and failing to write one must never interrupt an edit.
fn write_atomically(file: &Path, bytes: &[u8]) {
    let Some(dir) = file.parent() else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, file).is_err() {
        let _ = std::fs::remove_file(&tmp);
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
    fn editing_defers_the_reparse_until_it_is_settled() {
        let mut d = Document {
            path: Some("t.rs".into()),
            ..doc("")
        };
        d.insert_at_cursor("fn main() {}");
        // The edit only flagged the buffer; nothing has been parsed yet.
        assert!(d.syntax.is_none());
        d.settle_syntax();
        assert!(
            d.syntax.as_ref().is_some_and(|s| !s.spans.is_empty()),
            "settling colors the buffer"
        );
        // Settling again with no edit is a no-op, and keeps the colors.
        d.settle_syntax();
        assert!(d.syntax.as_ref().is_some_and(|s| !s.spans.is_empty()));
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

    /// `replace_all` has to find the smallest middle that differs — the
    /// cursors outside it are the ones that stay put on a reload.
    #[test]
    fn replace_all_edits_only_the_stretch_that_differs() {
        let case = |before: &str, after: &str| {
            let mut d = doc(before);
            d.cursor = before.chars().count();
            d.anchor = d.cursor;
            let changed = d.replace_all(after);
            assert_eq!(d.text.to_string(), after, "{before:?} -> {after:?}");
            changed
        };
        assert!(case("one\ntwo\n", "one\nTWO\nthree\n"));
        assert!(case("", "new"));
        assert!(case("gone", ""));
        assert!(case("head tail", "head middle tail"));
        assert!(case("abab", "ab")); // the tail could be double-counted
        assert!(case("ab", "abab"));
        assert!(case("héllo wörld", "héllo wörld!")); // multi-byte chars
        assert!(!case("same\n", "same\n"), "no edit when they already agree");

        // The change covers the middle only: a cursor before it does not move.
        let mut d = doc("keep this: old\n");
        d.cursor = 4;
        d.anchor = 4;
        d.replace_all("keep this: new\n");
        assert_eq!(d.cursor, 4);
        assert_eq!(d.text.to_string(), "keep this: new\n");
        d.undo();
        assert_eq!(d.text.to_string(), "keep this: old\n");
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

#[cfg(test)]
mod bench {
    use super::*;

    /// The whole point of the incremental path, as a number.
    ///
    /// Self-calibrating rather than an absolute millisecond budget, so it
    /// means the same thing on a slow machine: an edit plus colouring one
    /// screenful must cost a fraction of what re-parsing the file costs.
    /// Ignored by default — it is a benchmark, and it takes a second.
    #[test]
    #[ignore]
    fn editing_beats_reparsing_the_file() {
        let big: String = std::iter::repeat_n(include_str!("editor.rs"), 4).collect();
        let mut doc = Document {
            text: Rope::from_str(&big),
            path: Some(PathBuf::from("big.rs")),
            ..Document::empty()
        };
        doc.refresh_syntax();
        doc.cursor = doc.text.len_chars() / 2;
        eprintln!("{} lines, {} KB", doc.line_count(), big.len() / 1024);

        let start = std::time::Instant::now();
        for _ in 0..100 {
            doc.insert_at_cursor("x");
            let line = doc.cursor_line();
            doc.highlight_range(line, line + 40); // what a frame asks for
        }
        let edit = start.elapsed() / 100;

        let start = std::time::Instant::now();
        for _ in 0..10 {
            doc.insert_at_cursor("x");
            // `refresh_syntax` only marks the buffer stale; the reparse
            // itself is `settle_syntax`, and that is the cost to beat.
            doc.refresh_syntax();
            doc.settle_syntax();
        }
        let full = start.elapsed() / 10;

        eprintln!("edit {edit:?} vs full reparse {full:?}");
        assert!(
            edit * 10 < full,
            "incremental parsing lost its advantage: {edit:?} vs {full:?}"
        );
    }
}
