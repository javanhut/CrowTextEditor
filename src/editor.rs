//! Editor state and the key dispatch loop.

use std::path::PathBuf;

use crate::commands;
use crate::document::Document;
use crate::keymap::{Key, KeyCode, KeyTrie, KeymapResult};
use crate::position::TAB_WIDTH;
use crate::search;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Search,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Insert => "INS",
            Mode::Command => "CMD",
            Mode::Search => "FND",
        }
    }
}

/// A window's saved view state. The focused window's state lives in its
/// `Document`; it is stashed here whenever focus moves away.
///
/// ponytail: stashed positions are clamped, not remapped, when another window
/// edits the same document — remapping would need the transactions replayed.
#[derive(Clone)]
pub struct Window {
    pub id: usize,
    pub doc: usize,
    pub cursor: usize,
    pub anchor: usize,
    pub extra: Vec<(usize, usize)>,
    pub view_line: usize,
    pub view_col: usize,
}

/// The window tree: leaves are windows, splits divide their rectangle among
/// their children, side by side (`vertical`) or stacked.
pub enum Layout {
    Leaf(Window),
    Split { vertical: bool, children: Vec<Layout> },
}

/// (x, y, width, height) in terminal cells.
pub type Rect = (u16, u16, u16, u16);

impl Layout {
    fn leaf_ids(&self, out: &mut Vec<usize>) {
        match self {
            Layout::Leaf(w) => out.push(w.id),
            Layout::Split { children, .. } => {
                for c in children {
                    c.leaf_ids(out);
                }
            }
        }
    }

    pub fn find_mut(&mut self, id: usize) -> Option<&mut Window> {
        match self {
            Layout::Leaf(w) if w.id == id => Some(w),
            Layout::Leaf(_) => None,
            Layout::Split { children, .. } => {
                children.iter_mut().find_map(|c| c.find_mut(id))
            }
        }
    }

    pub fn find(&self, id: usize) -> Option<&Window> {
        match self {
            Layout::Leaf(w) if w.id == id => Some(w),
            Layout::Leaf(_) => None,
            Layout::Split { children, .. } => children.iter().find_map(|c| c.find(id)),
        }
    }

    fn count(&self) -> usize {
        match self {
            Layout::Leaf(_) => 1,
            Layout::Split { children, .. } => children.iter().map(Layout::count).sum(),
        }
    }

    /// Insert `new` as the sibling after the leaf `id`, splitting in the given
    /// direction. Returns true if the leaf was found.
    fn split(&mut self, id: usize, vertical: bool, new: Window) -> bool {
        match self {
            Layout::Leaf(w) if w.id == id => {
                let old = std::mem::replace(
                    self,
                    Layout::Split {
                        vertical,
                        children: Vec::new(),
                    },
                );
                if let Layout::Split { children, .. } = self {
                    children.push(old);
                    children.push(Layout::Leaf(new));
                }
                true
            }
            Layout::Leaf(_) => false,
            Layout::Split { vertical: v, children } => {
                // Same-direction split of a direct child joins this row/column
                // instead of nesting.
                if *v == vertical {
                    if let Some(i) = children
                        .iter()
                        .position(|c| matches!(c, Layout::Leaf(w) if w.id == id))
                    {
                        children.insert(i + 1, Layout::Leaf(new));
                        return true;
                    }
                }
                for c in children.iter_mut() {
                    if c.split(id, vertical, new.clone()) {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Remove the leaf `id`, collapsing single-child splits.
    fn close(&mut self, id: usize) {
        if let Layout::Split { children, .. } = self {
            children.retain(
                |c| !matches!(c, Layout::Leaf(w) if w.id == id),
            );
            for c in children.iter_mut() {
                c.close(id);
            }
            if children.len() == 1 {
                *self = children.pop().unwrap();
            }
        }
    }

    /// Compute every leaf's rectangle plus the separators between children.
    fn rects(&self, rect: Rect, wins: &mut Vec<(usize, Rect)>, seps: &mut Vec<(Rect, bool)>) {
        match self {
            Layout::Leaf(w) => wins.push((w.id, rect)),
            Layout::Split { vertical, children } => {
                let n = children.len() as u16;
                let (x, y, w, h) = rect;
                if *vertical {
                    let each = w.saturating_sub(n - 1) / n;
                    let mut cx = x;
                    for (i, c) in children.iter().enumerate() {
                        let cw = if i as u16 == n - 1 {
                            (x + w).saturating_sub(cx)
                        } else {
                            each
                        };
                        c.rects((cx, y, cw, h), wins, seps);
                        cx += cw;
                        if (i as u16) < n - 1 {
                            seps.push(((cx, y, 1, h), true));
                            cx += 1;
                        }
                    }
                } else {
                    let each = h.saturating_sub(n - 1) / n;
                    let mut cy = y;
                    for (i, c) in children.iter().enumerate() {
                        let ch = if i as u16 == n - 1 {
                            (y + h).saturating_sub(cy)
                        } else {
                            each
                        };
                        c.rects((x, cy, w, ch), wins, seps);
                        cy += ch;
                        if (i as u16) < n - 1 {
                            seps.push(((x, cy, w, 1), false));
                            cy += 1;
                        }
                    }
                }
            }
        }
    }
}

pub struct Keymaps {
    pub normal: KeyTrie,
    pub insert: KeyTrie,
}

impl Default for Keymaps {
    fn default() -> Self {
        let mut normal = KeyTrie::new();

        // motion
        normal.bind_str("h", "move_left");
        normal.bind_str("<left>", "move_left");
        normal.bind_str("l", "move_right");
        normal.bind_str("<right>", "move_right");
        normal.bind_str("k", "move_up");
        normal.bind_str("<up>", "move_up");
        normal.bind_str("j", "move_down");
        normal.bind_str("<down>", "move_down");
        normal.bind_str("0", "move_line_start");
        normal.bind_str("<home>", "move_line_start");
        normal.bind_str("^", "move_line_first_nonblank");
        normal.bind_str("$", "move_line_end");
        normal.bind_str("<end>", "move_line_end");
        normal.bind_str("w", "select_word_next");
        normal.bind_str("b", "select_word_prev");
        normal.bind_str("e", "select_word_end");
        normal.bind_str("gg", "goto_file_start");
        normal.bind_str("G", "goto_file_end");
        normal.bind_str("C-d", "half_page_down");
        normal.bind_str("C-u", "half_page_up");
        normal.bind_str("C-f", "page_down");
        normal.bind_str("C-b", "page_up");
        normal.bind_str("<pagedown>", "page_down");
        normal.bind_str("<pageup>", "page_up");

        // entering insert mode
        normal.bind_str("i", "insert_mode");
        normal.bind_str("I", "insert_at_line_start");
        normal.bind_str("a", "append");
        normal.bind_str("A", "append_at_line_end");
        normal.bind_str("o", "open_below");
        normal.bind_str("O", "open_above");

        // selection and edits
        normal.bind_str("x", "select_line");
        normal.bind_str("v", "extend_mode");
        normal.bind_str("A-o", "expand_selection");
        normal.bind_str(";", "collapse_selection");
        normal.bind_str("C", "add_cursor_below");
        normal.bind_str("A-C", "add_cursor_above");
        normal.bind_str(",", "remove_extra_cursors");
        normal.bind_str("/", "search");
        normal.bind_str("s", "select_matches");
        normal.bind_str("n", "search_next");
        normal.bind_str("N", "search_prev");
        normal.bind_str("d", "delete_selection");
        normal.bind_str("c", "change_selection");
        normal.bind_str("y", "yank");
        normal.bind_str("p", "paste_after");
        normal.bind_str("P", "paste_before");
        normal.bind_str("D", "delete_to_line_end");
        normal.bind_str("J", "join_lines");
        normal.bind_str("u", "undo");
        normal.bind_str("C-r", "redo");

        // windows, buffers, files, lifecycle
        normal.bind_str("C-w v", "split_vertical");
        normal.bind_str("C-w s", "split_horizontal");
        normal.bind_str("C-w w", "next_window");
        normal.bind_str("C-w q", "quit");
        normal.bind_str("gn", "next_buffer");
        normal.bind_str("gp", "prev_buffer");
        normal.bind_str("C-s", "save");
        normal.bind_str(":", "command_mode");
        normal.bind_str("<esc>", "normal_mode");

        let mut insert = KeyTrie::new();
        insert.bind_str("<esc>", "normal_mode");
        insert.bind_str("<enter>", "insert_newline");
        insert.bind_str("<tab>", "insert_tab");
        insert.bind_str("<bs>", "delete_backward");
        insert.bind_str("<del>", "delete_forward");
        insert.bind_str("<left>", "move_left");
        insert.bind_str("<right>", "move_right");
        insert.bind_str("<up>", "move_up");
        insert.bind_str("<down>", "move_down");
        insert.bind_str("<home>", "move_line_start");
        insert.bind_str("<end>", "move_line_end");
        insert.bind_str("C-s", "save");

        Keymaps { normal, insert }
    }
}

pub struct Editor {
    pub documents: Vec<Document>,
    pub current: usize,
    pub layout: Layout,
    /// Id of the focused window (a leaf of `layout`).
    pub focused: usize,
    next_window_id: usize,
    pub mode: Mode,
    /// Keys received so far that form a prefix of some binding.
    pub pending: Vec<Key>,
    /// Numeric prefix, e.g. the `3` in `3dd`.
    pub count: Option<usize>,
    pub command_line: String,
    pub status: String,
    /// The unnamed register: what d/c/y last captured, what p/P paste.
    pub register: String,
    /// Named registers, selected for one command by the `"x` prefix.
    pub registers: std::collections::HashMap<char, String>,
    /// Register the next capture/paste should use instead of the unnamed one.
    pub active_register: Option<char>,
    /// A `"` has been pressed; the next key names the register.
    pub awaiting_register: bool,
    /// True until the first register capture of the current keypress, so the
    /// captures of one multi-cursor edit accumulate instead of overwriting.
    pub register_fresh: bool,
    /// The last committed search pattern, reused by n/N.
    pub last_search: String,
    /// True while the prompt belongs to `s` (select every match) rather
    /// than `/` (jump to the next match).
    pub search_select: bool,
    /// (anchor, cursor) when the search prompt opened, restored on Esc.
    pub search_origin: (usize, usize),
    /// Set by selecting commands; any command that leaves it false has its
    /// selection collapsed after it runs.
    pub keep_selection: bool,
    /// Extend mode (`v`): motions grow the selection instead of replacing it.
    pub extend: bool,
    pub should_quit: bool,
    /// Terminal size as (columns, rows).
    pub size: (u16, u16),
    pub keymaps: Keymaps,
}

impl Editor {
    pub fn new(paths: Vec<PathBuf>, size: (u16, u16)) -> std::io::Result<Self> {
        let mut documents = Vec::new();
        for path in paths {
            documents.push(Document::open(path)?);
        }
        if documents.is_empty() {
            documents.push(Document::empty());
        }

        Ok(Editor {
            documents,
            current: 0,
            layout: Layout::Leaf(Window {
                id: 0,
                doc: 0,
                cursor: 0,
                anchor: 0,
                extra: Vec::new(),
                view_line: 0,
                view_col: 0,
            }),
            focused: 0,
            next_window_id: 1,
            mode: Mode::Normal,
            pending: Vec::new(),
            count: None,
            command_line: String::new(),
            status: String::new(),
            register: String::new(),
            registers: std::collections::HashMap::new(),
            active_register: None,
            awaiting_register: false,
            register_fresh: true,
            last_search: String::new(),
            search_select: false,
            search_origin: (0, 0),
            keep_selection: false,
            extend: false,
            should_quit: false,
            size,
            keymaps: Keymaps::default(),
        })
    }

    pub fn doc(&self) -> &Document {
        &self.documents[self.current]
    }

    pub fn doc_mut(&mut self) -> &mut Document {
        &mut self.documents[self.current]
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.status = msg.into();
    }

    pub fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1)
    }

    // ---- windows -----------------------------------------------------------

    /// Every window's rectangle plus the separators between them. The text
    /// area is everything but the status and command lines.
    pub fn window_rects(&self) -> (Vec<(usize, Rect)>, Vec<(Rect, bool)>) {
        let area = (0, 0, self.size.0, self.size.1.saturating_sub(2));
        let mut wins = Vec::new();
        let mut seps = Vec::new();
        self.layout.rects(area, &mut wins, &mut seps);
        (wins, seps)
    }

    pub fn focused_rect(&self) -> Rect {
        let (wins, _) = self.window_rects();
        wins.iter()
            .find(|(id, _)| *id == self.focused)
            .map(|&(_, r)| r)
            .unwrap_or((0, 0, self.size.0, self.size.1.saturating_sub(2)))
    }

    /// Stash the live view state into the focused window before focus moves.
    fn save_focus_state(&mut self) {
        let current = self.current;
        let doc = &self.documents[current];
        let snap = (doc.cursor, doc.anchor, doc.extra.clone(), doc.view_line, doc.view_col);
        if let Some(w) = self.layout.find_mut(self.focused) {
            w.doc = current;
            (w.cursor, w.anchor, w.extra, w.view_line, w.view_col) = snap;
        }
    }

    /// Load the newly focused window's stashed state into its document.
    fn restore_focus_state(&mut self) {
        let Some(w) = self.layout.find(self.focused) else {
            return;
        };
        let (doc_idx, c, a, extra, vl, vc) =
            (w.doc, w.cursor, w.anchor, w.extra.clone(), w.view_line, w.view_col);
        self.current = doc_idx.min(self.documents.len() - 1);
        let doc = &mut self.documents[self.current];
        let len = doc.text.len_chars();
        doc.cursor = c.min(len);
        doc.anchor = a.min(len);
        doc.extra = extra
            .into_iter()
            .map(|(a, c)| (a.min(len), c.min(len)))
            .collect();
        doc.view_line = vl.min(doc.line_count().saturating_sub(1));
        doc.view_col = vc;
        doc.clamp_cursor(false);
        doc.dedupe_cursors();
    }

    pub fn split_window(&mut self, vertical: bool) {
        self.save_focus_state();
        let mut new = self
            .layout
            .find(self.focused)
            .expect("focused window exists")
            .clone();
        new.id = self.next_window_id;
        self.next_window_id += 1;
        let new_id = new.id;
        self.layout.split(self.focused, vertical, new);
        self.focused = new_id;
        self.restore_focus_state();
    }

    pub fn window_count(&self) -> usize {
        self.layout.count()
    }

    pub fn close_focused_window(&mut self) {
        if self.window_count() <= 1 {
            return;
        }
        let closing = self.focused;
        self.focus_next_window();
        // focus_next_window saved into `closing` and restored the next one.
        self.layout.close(closing);
    }

    pub fn focus_next_window(&mut self) {
        let mut ids = Vec::new();
        self.layout.leaf_ids(&mut ids);
        if ids.len() <= 1 {
            return;
        }
        let pos = ids.iter().position(|&i| i == self.focused).unwrap_or(0);
        self.save_focus_state();
        self.focused = ids[(pos + 1) % ids.len()];
        self.restore_focus_state();
    }

    // ---- geometry ----------------------------------------------------------

    /// Rows of document text in the focused window.
    pub fn text_height(&self) -> usize {
        self.focused_rect().3 as usize
    }

    /// Width of the line-number gutter, including its trailing space.
    pub fn gutter_width(&self) -> usize {
        let digits = self.doc().line_count().to_string().len();
        digits.max(3) + 1
    }

    /// Columns available for document text in the focused window.
    pub fn text_width(&self) -> usize {
        (self.focused_rect().2 as usize).saturating_sub(self.gutter_width())
    }

    pub fn set_mode(&mut self, mode: Mode) {
        if self.mode == Mode::Insert && mode != Mode::Insert {
            let doc = self.doc_mut();
            // A typing burst becomes one undo step.
            doc.commit_undo_group();
            // vi convention: the cursor steps back off the insertion point.
            let start = doc.line_start(doc.cursor_line());
            if doc.cursor > start {
                doc.cursor -= 1;
            }
            doc.clamp_cursor(false);
        }
        self.mode = mode;
        self.pending.clear();
    }

    // ---- key handling ------------------------------------------------------

    pub fn handle_key(&mut self, key: Key) {
        self.status.clear();

        if self.mode == Mode::Command {
            self.handle_command_key(key);
            return;
        }
        if self.mode == Mode::Search {
            self.handle_search_key(key);
            return;
        }

        // `"x` names the register for the next capture or paste.
        if self.mode == Mode::Normal && self.pending.is_empty() {
            if self.awaiting_register {
                self.awaiting_register = false;
                if let KeyCode::Char(c) = key.code {
                    if !key.ctrl && !key.alt {
                        self.active_register = Some(c);
                        return;
                    }
                }
                self.active_register = None;
                return;
            }
            if key.code == KeyCode::Char('"') && !key.ctrl && !key.alt {
                self.awaiting_register = true;
                return;
            }
        }

        // A digit typed with no pending sequence builds a count, except a
        // leading `0`, which is the line-start motion.
        if self.mode == Mode::Normal && self.pending.is_empty() && !key.ctrl && !key.alt {
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_digit() && !(c == '0' && self.count.is_none()) {
                    let digit = c.to_digit(10).unwrap() as usize;
                    self.count = Some(self.count.unwrap_or(0).saturating_mul(10) + digit);
                    return;
                }
            }
        }

        self.pending.push(key);

        // The lookup borrows the keymap, but its result is `'static`, so the
        // borrow ends here and the command is free to mutate the editor.
        let result = {
            let map = match self.mode {
                Mode::Insert => &self.keymaps.insert,
                _ => &self.keymaps.normal,
            };
            map.lookup(&self.pending)
        };

        match result {
            KeymapResult::Pending => {}
            KeymapResult::Matched(command) => {
                self.pending.clear();
                self.keep_selection = false;
                self.register_fresh = true;
                if !self.doc().extra.is_empty()
                    && commands::PER_CURSOR.contains(&command.name)
                {
                    self.dispatch_per_cursor(command);
                } else {
                    (command.func)(self);
                }
                if !self.keep_selection && !(self.extend && self.mode == Mode::Normal) {
                    let doc = self.doc_mut();
                    doc.anchor = doc.cursor;
                    for (a, c) in &mut doc.extra {
                        *a = *c;
                    }
                }
                self.doc_mut().dedupe_cursors();
                if self.mode == Mode::Normal {
                    // Each normal-mode edit is its own undo step; insert-mode
                    // bursts stay grouped because the mode is no longer Normal
                    // by the time the entering command finishes.
                    self.doc_mut().commit_undo_group();
                }
                self.count = None;
                self.active_register = None;
            }
            KeymapResult::NotFound => {
                // In insert mode an unbound printable key is literal text.
                if self.mode == Mode::Insert && self.pending.len() == 1 && !key.ctrl && !key.alt {
                    if let KeyCode::Char(c) = key.code {
                        let s = c.to_string();
                        self.doc_mut().insert_at_cursor(&s);
                    }
                }
                self.pending.clear();
                self.count = None;
                self.active_register = None;
            }
        }
    }

    /// Run a command once per cursor.
    ///
    /// Each extra selection is swapped into the primary slot, the command runs,
    /// and the result is swapped back. While one cursor is being processed,
    /// every other cursor — including the stashed primary — sits in `extra`,
    /// where `Document::apply` remaps it through any edit the command makes.
    /// All the edits share one undo group, so a multi-cursor edit is one undo.
    fn dispatch_per_cursor(&mut self, command: &'static crate::commands::Command) {
        let count = self.count;
        let n = self.doc().extra.len();
        for i in 0..n {
            {
                let doc = self.doc_mut();
                let stash = (doc.anchor, doc.cursor);
                (doc.anchor, doc.cursor) = doc.extra[i];
                doc.extra[i] = stash;
            }
            self.count = count;
            (command.func)(self);
            let doc = self.doc_mut();
            let stash = (doc.anchor, doc.cursor);
            (doc.anchor, doc.cursor) = doc.extra[i];
            doc.extra[i] = stash;
        }
        self.count = count;
        (command.func)(self);
    }

    fn handle_command_key(&mut self, key: Key) {
        match key.code {
            KeyCode::Esc => {
                self.command_line.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let line = std::mem::take(&mut self.command_line);
                self.mode = Mode::Normal;
                self.execute_command(&line);
            }
            KeyCode::Backspace => {
                if self.command_line.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Char(c) if !key.ctrl && !key.alt => self.command_line.push(c),
            _ => {}
        }
    }

    // ---- search ------------------------------------------------------------

    fn handle_search_key(&mut self, key: Key) {
        match key.code {
            KeyCode::Esc => {
                self.command_line.clear();
                self.restore_search_origin();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let query = std::mem::take(&mut self.command_line);
                self.mode = Mode::Normal;
                if !query.is_empty() {
                    self.last_search = query;
                }
                if self.search_select {
                    self.select_all_matches();
                }
                // For `/`, the incremental preview already put the selection
                // on the match; Enter just keeps it.
            }
            KeyCode::Backspace => {
                if self.command_line.pop().is_none() {
                    self.restore_search_origin();
                    self.mode = Mode::Normal;
                } else {
                    self.update_search_preview();
                }
            }
            KeyCode::Char(c) if !key.ctrl && !key.alt => {
                self.command_line.push(c);
                self.update_search_preview();
            }
            _ => {}
        }
    }

    fn restore_search_origin(&mut self) {
        let (a, c) = self.search_origin;
        let doc = self.doc_mut();
        doc.anchor = a;
        doc.cursor = c;
    }

    /// Live feedback while typing a `/` pattern: select the first match at or
    /// after where the search started. The main loop's scrolling brings it on
    /// screen for free.
    fn update_search_preview(&mut self) {
        if self.search_select {
            // The current selection scopes `s`; don't disturb it while typing.
            return;
        }
        let (_, oc) = self.search_origin;
        let all = search::matches(&self.doc().text, &self.command_line);
        let hit = all
            .iter()
            .copied()
            .find(|&(p, _)| p >= oc)
            .or(all.first().copied());
        match hit {
            Some((p, e)) => {
                let doc = self.doc_mut();
                doc.anchor = p;
                doc.cursor = e;
            }
            None => self.restore_search_origin(),
        }
    }

    /// Put a selection on every match of the last search — within the current
    /// selection if there is one, otherwise the whole buffer. Search becomes
    /// multi-cursor: follow with `c`, `d`, or `y`.
    fn select_all_matches(&mut self) {
        if self.last_search.is_empty() {
            return;
        }
        let (from, to) = {
            let doc = self.doc();
            if doc.anchor == doc.cursor {
                (0, doc.text.len_chars())
            } else {
                (doc.anchor.min(doc.cursor), doc.anchor.max(doc.cursor))
            }
        };
        let all: Vec<(usize, usize)> = search::matches(&self.doc().text, &self.last_search)
            .into_iter()
            .filter(|&(p, e)| p >= from && e <= to)
            .collect();
        match all.split_first() {
            None => {
                let q = self.last_search.clone();
                self.restore_search_origin();
                self.set_status(format!("no match: {q}"));
            }
            Some((&(first, first_end), rest)) => {
                // A match range is an (anchor, cursor) pair; the extras are
                // exactly the remaining ranges.
                let doc = self.doc_mut();
                doc.anchor = first;
                doc.cursor = first_end;
                doc.extra = rest.to_vec();
                let n = rest.len() + 1;
                self.set_status(format!("{n} matches"));
            }
        }
    }

    // ---- ex commands -------------------------------------------------------

    fn execute_command(&mut self, line: &str) {
        self.register_fresh = true;
        let line = line.trim();
        if line.is_empty() {
            return;
        }

        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next();

        match cmd {
            "w" | "write" => match arg {
                Some(path) => match self.doc_mut().save_as(path) {
                    Ok(()) => self.set_status(format!("\"{path}\" written")),
                    Err(e) => self.set_status(format!("Error: {e}")),
                },
                None => (commands::find("save").unwrap().func)(self),
            },
            "q" | "quit" => (commands::find("quit").unwrap().func)(self),
            "q!" | "quit!" => self.should_quit = true,
            "wq" | "x" => {
                (commands::find("save").unwrap().func)(self);
                if !self.doc().modified {
                    self.should_quit = true;
                }
            }
            "e" | "edit" => match arg {
                Some(path) => match Document::open(path) {
                    Ok(doc) => {
                        self.documents.push(doc);
                        self.current = self.documents.len() - 1;
                        self.set_status(format!("\"{path}\""));
                    }
                    Err(e) => self.set_status(format!("Error: {e}")),
                },
                None => self.set_status("Usage: :e <file>"),
            },
            "bn" => (commands::find("next_buffer").unwrap().func)(self),
            "bp" => (commands::find("prev_buffer").unwrap().func)(self),
            other => {
                // `:42` jumps to a line.
                if let Ok(n) = other.parse::<usize>() {
                    let doc = self.doc_mut();
                    let target = n.saturating_sub(1).min(doc.line_count().saturating_sub(1));
                    doc.cursor = doc.line_start(target);
                    doc.clamp_cursor(false);
                    doc.goal_col = None;
                } else if let Some(command) = commands::find(other) {
                    // Anything in the registry is also callable by name.
                    (command.func)(self);
                } else {
                    self.set_status(format!("Not a command: {other}"));
                }
            }
        }
    }

    // ---- scrolling ---------------------------------------------------------

    /// Adjust the viewport so the cursor is on screen, keeping a few lines of
    /// context above and below where possible.
    pub fn ensure_cursor_visible(&mut self) {
        let height = self.text_height();
        let width = self.text_width();
        if height == 0 || width == 0 {
            return;
        }
        let scrolloff = 3.min(height.saturating_sub(1) / 2);

        let doc = self.doc_mut();
        let (line, col) = doc.cursor_display();

        if line < doc.view_line + scrolloff {
            doc.view_line = line.saturating_sub(scrolloff);
        }
        if line + scrolloff >= doc.view_line + height {
            doc.view_line = (line + scrolloff + 1).saturating_sub(height);
        }

        let max_view_line = doc.line_count().saturating_sub(1);
        if doc.view_line > max_view_line {
            doc.view_line = max_view_line;
        }

        if col < doc.view_col {
            doc.view_col = col;
        }
        if col >= doc.view_col + width {
            doc.view_col = col - width + 1;
        }
    }

    /// Cursor position on screen as (column, row), or `None` if off-screen.
    pub fn screen_cursor(&self) -> Option<(u16, u16)> {
        let prompt_len = match self.mode {
            Mode::Command => Some(1),
            Mode::Search => Some(crate::ui::search_prompt(self).chars().count()),
            _ => None,
        };
        if let Some(n) = prompt_len {
            let col = self.command_line.chars().count() + n;
            return Some((col as u16, self.size.1.saturating_sub(1)));
        }

        let (rx, ry, rw, rh) = self.focused_rect();
        let doc = self.doc();
        let (line, col) = doc.cursor_display();
        if line < doc.view_line || line >= doc.view_line + rh as usize {
            return None;
        }
        if col < doc.view_col {
            return None;
        }

        let screen_row = line - doc.view_line;
        let screen_col = self.gutter_width() + (col - doc.view_col);
        if screen_col >= rw as usize {
            return None;
        }
        Some((rx + screen_col as u16, ry + screen_row as u16))
    }

    /// Human-readable cursor position for the status line, 1-indexed.
    pub fn cursor_indicator(&self) -> String {
        let doc = self.doc();
        let (line, col) = doc.cursor_line_col();
        let display = crate::position::char_to_display_col(doc.line(line), col, TAB_WIDTH);
        format!("{}:{}", line + 1, display + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Key;

    fn editor_with(text: &str) -> Editor {
        let mut editor = Editor::new(vec![], (80, 24)).unwrap();
        editor.doc_mut().text = ropey::Rope::from_str(text);
        editor
    }

    fn press(editor: &mut Editor, keys: &str) {
        for token in keys.split(' ').filter(|t| !t.is_empty()) {
            if token.len() > 1 && !token.contains('-') && !token.starts_with('<') {
                for c in token.chars() {
                    editor.handle_key(Key::char(c));
                }
            } else {
                editor.handle_key(Key::parse(token).unwrap());
            }
        }
    }

    #[test]
    fn typing_in_insert_mode_inserts_text() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "hello");
        assert_eq!(editor.doc().text.to_string(), "hello");
    }

    #[test]
    fn escape_commits_one_undo_step() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "hello");
        press(&mut editor, "<esc>");
        press(&mut editor, "u");
        assert_eq!(editor.doc().text.to_string(), "");
    }

    #[test]
    fn count_prefix_repeats_a_command() {
        let mut editor = editor_with("abcdef");
        press(&mut editor, "3d");
        assert_eq!(editor.doc().text.to_string(), "def");
    }

    #[test]
    fn zero_is_a_motion_not_a_count() {
        let mut editor = editor_with("hello");
        press(&mut editor, "$");
        assert_eq!(editor.doc().cursor, 4);
        press(&mut editor, "0");
        assert_eq!(editor.doc().cursor, 0);
    }

    #[test]
    fn zero_is_a_count_digit_after_another_digit() {
        let mut editor = editor_with("abcdefghijklm");
        press(&mut editor, "10d");
        assert_eq!(editor.doc().text.to_string(), "klm");
    }

    #[test]
    fn select_line_then_d_deletes_the_line() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "xd");
        assert_eq!(editor.doc().text.to_string(), "two\nthree");
    }

    #[test]
    fn repeated_x_extends_the_selection() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "xxd");
        assert_eq!(editor.doc().text.to_string(), "three");
    }

    #[test]
    fn w_selects_what_it_crosses() {
        let mut editor = editor_with("foo bar baz");
        press(&mut editor, "wd");
        assert_eq!(editor.doc().text.to_string(), "bar baz");
    }

    #[test]
    fn motions_collapse_the_selection() {
        let mut editor = editor_with("foo bar");
        press(&mut editor, "wh");
        press(&mut editor, "d");
        assert_eq!(editor.doc().text.to_string(), "foobar");
    }

    #[test]
    fn change_replaces_the_selection() {
        let mut editor = editor_with("foo bar");
        press(&mut editor, "wc");
        press(&mut editor, "x");
        assert_eq!(editor.doc().text.to_string(), "xbar");
    }

    #[test]
    fn linewise_yank_then_paste_duplicates_the_line() {
        let mut editor = editor_with("one\ntwo");
        press(&mut editor, "xyp");
        assert_eq!(editor.doc().text.to_string(), "one\none\ntwo");
    }

    #[test]
    fn delete_then_paste_moves_a_line() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "xdjp");
        assert_eq!(editor.doc().text.to_string(), "two\nthree\none");
    }

    #[test]
    fn multi_cursor_typing_inserts_at_every_cursor() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "C");
        press(&mut editor, "i");
        press(&mut editor, "x");
        assert_eq!(editor.doc().text.to_string(), "xone\nxtwo\nthree");
    }

    #[test]
    fn multi_cursor_word_delete() {
        let mut editor = editor_with("foo bar\nfoo baz");
        press(&mut editor, "C");
        press(&mut editor, "wd");
        assert_eq!(editor.doc().text.to_string(), "bar\nbaz");
        // Both captures land in the register.
        assert_eq!(editor.register, "foo \nfoo ");
    }

    #[test]
    fn multi_cursor_edit_is_one_undo_step() {
        let mut editor = editor_with("foo bar\nfoo baz");
        press(&mut editor, "C");
        press(&mut editor, "wd");
        press(&mut editor, "u");
        assert_eq!(editor.doc().text.to_string(), "foo bar\nfoo baz");
    }

    #[test]
    fn colliding_cursors_merge() {
        let mut editor = editor_with("abc\ndef");
        press(&mut editor, "C");
        assert_eq!(editor.doc().extra.len(), 1);
        press(&mut editor, "gg");
        assert_eq!(editor.doc().extra.len(), 0);
    }

    #[test]
    fn comma_drops_extra_cursors_and_esc_clears_in_normal_mode() {
        let mut editor = editor_with("abc\ndef\nghi");
        press(&mut editor, "CC");
        assert_eq!(editor.doc().extra.len(), 2);
        press(&mut editor, ",");
        assert_eq!(editor.doc().extra.len(), 0);

        press(&mut editor, "C");
        assert_eq!(editor.doc().extra.len(), 1);
        press(&mut editor, "<esc>");
        assert_eq!(editor.doc().extra.len(), 0);
    }

    #[test]
    fn search_selects_the_match_so_edits_compose() {
        let mut editor = editor_with("one two three");
        press(&mut editor, "/two");
        press(&mut editor, "<enter>");
        press(&mut editor, "d");
        assert_eq!(editor.doc().text.to_string(), "one  three");
    }

    #[test]
    fn n_walks_matches_and_wraps() {
        let mut editor = editor_with("foo x foo y foo");
        press(&mut editor, "/foo");
        press(&mut editor, "<enter>");
        assert_eq!(editor.doc().anchor, 0);
        press(&mut editor, "n");
        assert_eq!(editor.doc().anchor, 6);
        press(&mut editor, "n");
        assert_eq!(editor.doc().anchor, 12);
        press(&mut editor, "n");
        assert_eq!(editor.doc().anchor, 0);
        press(&mut editor, "N");
        assert_eq!(editor.doc().anchor, 12);
    }

    #[test]
    fn esc_cancels_search_and_restores_the_cursor() {
        let mut editor = editor_with("abc def");
        press(&mut editor, "/def");
        assert_eq!(editor.doc().anchor, 4); // preview moved
        press(&mut editor, "<esc>");
        assert_eq!(editor.doc().cursor, 0);
        assert_eq!(editor.doc().anchor, 0);
    }

    #[test]
    fn select_matches_is_search_as_multi_cursor() {
        let mut editor = editor_with("foo bar foo baz foo");
        press(&mut editor, "sfoo");
        press(&mut editor, "<enter>");
        assert_eq!(editor.doc().extra.len(), 2);
        // Interactive replace-all: change every match.
        press(&mut editor, "cX");
        press(&mut editor, "<esc>");
        assert_eq!(editor.doc().text.to_string(), "X bar X baz X");
    }

    #[test]
    fn select_matches_is_scoped_by_the_selection() {
        let mut editor = editor_with("foo\nfoo\nfoo");
        press(&mut editor, "xx"); // select first two lines
        press(&mut editor, "sfoo");
        press(&mut editor, "<enter>");
        press(&mut editor, "d");
        assert_eq!(editor.doc().text.to_string(), "\n\nfoo");
    }

    #[test]
    fn count_gg_and_g_jump_to_a_line() {
        let mut editor = editor_with("a\nb\nc\nd");
        press(&mut editor, "3gg");
        assert_eq!(editor.doc().cursor_line(), 2);
        press(&mut editor, "2G");
        assert_eq!(editor.doc().cursor_line(), 1);
        press(&mut editor, "99G");
        assert_eq!(editor.doc().cursor_line(), 3);
        press(&mut editor, "gg");
        assert_eq!(editor.doc().cursor_line(), 0);
    }

    #[test]
    fn v_makes_motions_extend_the_selection() {
        let mut editor = editor_with("foo bar baz");
        press(&mut editor, "vwwd");
        assert_eq!(editor.doc().text.to_string(), "baz");
        // The delete dropped extend mode: plain motion, single-char delete.
        press(&mut editor, "ld");
        assert_eq!(editor.doc().text.to_string(), "bz");
    }

    #[test]
    fn plain_motions_extend_too_in_extend_mode() {
        let mut editor = editor_with("abcd");
        press(&mut editor, "vlld");
        assert_eq!(editor.doc().text.to_string(), "cd");
    }

    #[test]
    fn esc_leaves_extend_mode() {
        let mut editor = editor_with("abcd");
        press(&mut editor, "v");
        press(&mut editor, "<esc>");
        press(&mut editor, "ld");
        assert_eq!(editor.doc().text.to_string(), "acd");
    }

    #[test]
    fn regex_search_matches_variable_lengths() {
        let mut editor = editor_with("id42 and id777 here");
        press(&mut editor, "sid");
        press(&mut editor, "<enter>");
        assert_eq!(editor.doc().extra.len(), 1);
        press(&mut editor, ",");
        press(&mut editor, "gg");
        press(&mut editor, "s");
        press(&mut editor, "id\\d+");
        press(&mut editor, "<enter>");
        press(&mut editor, "d");
        assert_eq!(editor.doc().text.to_string(), " and  here");
    }

    #[test]
    fn named_registers_are_independent() {
        let mut editor = editor_with("foo bar");
        press(&mut editor, "w");    // select "foo "
        press(&mut editor, "\"ay"); // into register a; cursor back to 0
        press(&mut editor, "d");    // unnamed register = "f"
        assert_eq!(editor.doc().text.to_string(), "oo bar");
        assert_eq!(editor.register, "f");
        press(&mut editor, "\"aP");
        assert_eq!(editor.doc().text.to_string(), "foo oo bar");
        // The prefix applied to one command only; plain P uses unnamed again.
        press(&mut editor, "P");
        assert!(editor.doc().text.to_string().contains("foo"));
        assert_eq!(editor.register, "f");
    }

    #[test]
    fn split_windows_have_independent_cursors() {
        let mut editor = editor_with("a\nb\nc\nd");
        press(&mut editor, "C-w v");
        assert_eq!(editor.window_count(), 2);
        press(&mut editor, "jj"); // move in the new window
        assert_eq!(editor.doc().cursor_line(), 2);
        press(&mut editor, "C-w w"); // back to the first window
        assert_eq!(editor.doc().cursor_line(), 0);
        press(&mut editor, "C-w w");
        assert_eq!(editor.doc().cursor_line(), 2);
    }

    #[test]
    fn quit_closes_windows_before_the_editor() {
        let mut editor = editor_with("hello");
        press(&mut editor, "C-w s");
        assert_eq!(editor.window_count(), 2);
        press(&mut editor, ":q");
        press(&mut editor, "<enter>");
        assert_eq!(editor.window_count(), 1);
        assert!(!editor.should_quit);
        press(&mut editor, ":q");
        press(&mut editor, "<enter>");
        assert!(editor.should_quit);
    }

    #[test]
    fn expand_selection_climbs_the_syntax_tree() {
        let mut editor = editor_with("fn main() { let x = 1; }");
        editor.doc_mut().path = Some("test.rs".into());
        editor.doc_mut().refresh_syntax();
        press(&mut editor, "/x");
        press(&mut editor, "<enter>"); // select the identifier x
        press(&mut editor, "A-o"); // let-declaration
        let (a, c) = (editor.doc().anchor, editor.doc().cursor);
        let sel = editor.doc().text.slice(a.min(c)..a.max(c)).to_string();
        assert_eq!(sel, "let x = 1;");
        press(&mut editor, "A-o"); // block
        press(&mut editor, "A-o"); // whole function
        let (a, c) = (editor.doc().anchor, editor.doc().cursor);
        let sel = editor.doc().text.slice(a.min(c)..a.max(c)).to_string();
        assert_eq!(sel, "fn main() { let x = 1; }");
    }

    #[test]
    fn each_delete_is_its_own_undo_step() {
        let mut editor = editor_with("abc");
        press(&mut editor, "dd");
        assert_eq!(editor.doc().text.to_string(), "c");
        press(&mut editor, "u");
        assert_eq!(editor.doc().text.to_string(), "bc");
    }

    #[test]
    fn goal_column_survives_a_short_line() {
        let mut editor = editor_with("abcdefgh\nxy\nabcdefgh");
        press(&mut editor, "$");
        assert_eq!(editor.doc().cursor_line_col(), (0, 7));
        press(&mut editor, "j");
        assert_eq!(editor.doc().cursor_line_col(), (1, 1));
        press(&mut editor, "j");
        assert_eq!(editor.doc().cursor_line_col(), (2, 7));
    }

    #[test]
    fn ex_command_jumps_to_line() {
        let mut editor = editor_with("a\nb\nc\nd");
        press(&mut editor, ":");
        press(&mut editor, "3");
        press(&mut editor, "<enter>");
        assert_eq!(editor.doc().cursor_line(), 2);
    }

    #[test]
    fn open_below_preserves_indent() {
        let mut editor = editor_with("    hello");
        press(&mut editor, "o");
        press(&mut editor, "x");
        assert_eq!(editor.doc().text.to_string(), "    hello\n    x");
    }
}
