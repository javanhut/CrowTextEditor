//! Editor state and the key dispatch loop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::commands;
use crate::lsp;
use crate::document::Document;
use crate::keymap::{Key, KeyCode, KeyTrie, KeymapResult};

use crate::search;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Search,
    Picker,
}

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "NOR",
            Mode::Insert => "INS",
            Mode::Command => "CMD",
            Mode::Search => "FND",
            Mode::Picker => "PCK",
        }
    }
}

/// A pending file operation started from the tree sidebar.
pub enum TreeInput {
    /// Typing a name for a new entry inside `dir`; a trailing `/` makes a
    /// directory.
    Create { dir: PathBuf, name: String },
    /// Waiting for y/n on deleting `path`.
    Delete { path: PathBuf },
    /// Editing a new name for `path`, prefilled with the current one.
    Rename { path: PathBuf, name: String },
}

/// An active completion menu, shown while in insert mode.
pub struct Completion {
    /// (label, insert text) pairs, already filtered to the typed prefix.
    pub items: Vec<(String, String)>,
    pub selected: usize,
    /// The word before the cursor when the menu appeared.
    pub prefix: String,
    /// Popped up on its own while typing (buffer words). Auto menus accept
    /// with Tab only — Enter stays a newline, so typing is never hijacked.
    pub auto: bool,
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
        normal.bind_str("gd", "goto_definition");
        normal.bind_str("K", "hover");

        // space leader: pickers
        normal.bind_str("<space> c", "command_palette");
        normal.bind_str("<space> f", "find_files");
        normal.bind_str("<space> e", "tree_toggle");
        normal.bind_str("C-h", "focus_left");
        normal.bind_str("C-<left>", "focus_left");
        normal.bind_str("C-l", "focus_right");
        normal.bind_str("C-<right>", "focus_right");
        normal.bind_str("<space> d", "file_explorer");
        normal.bind_str("<space> t", "theme_picker");
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
        insert.bind_str("C-<space>", "complete");
        insert.bind_str("C-n", "complete");

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
    /// The language server, once one has started.
    pub lsp: Option<lsp::Client>,
    /// From crow.toml: (file extension, server command).
    lsp_table: Vec<(String, String)>,
    /// Set after a failed spawn so we don't retry every tick.
    lsp_failed: bool,
    /// Latest diagnostics per file (canonical paths, as the server sends them).
    pub diagnostics: HashMap<PathBuf, Vec<lsp::Diagnostic>>,
    /// The active popup picker, if any (mode == Picker).
    pub picker: Option<crate::picker::Picker>,
    /// The active completion menu, if any (mode == Insert).
    pub completion: Option<Completion>,
    /// Scroll offset of the `:help` window; None when closed.
    pub help_scroll: Option<usize>,
    /// Highlighted row of the `:` suggestion dropdown; None until Tab/arrows.
    pub command_suggest: Option<usize>,
    /// The file tree sidebar, when visible.
    pub tree: Option<crate::filetree::FileTree>,
    /// Keys go to the tree instead of the buffer.
    pub tree_focused: bool,
    /// A `space` was pressed while the tree had focus; `e` completes the
    /// toggle sequence there too.
    tree_leader: bool,
    /// An in-progress create/delete started from the tree.
    pub tree_input: Option<TreeInput>,
    /// The tree's clipboard: a path and whether the paste should move it.
    pub tree_clipboard: Option<(PathBuf, bool)>,
    pub should_quit: bool,
    /// Terminal size as (columns, rows).
    pub size: (u16, u16),
    pub keymaps: Keymaps,
}

impl Editor {
    pub fn new(
        paths: Vec<PathBuf>,
        size: (u16, u16),
        config: &crate::config::Config,
    ) -> std::io::Result<Self> {
        let mut documents = Vec::new();
        for path in paths {
            documents.push(Document::open(path)?);
        }
        if documents.is_empty() {
            documents.push(Document::empty());
        }

        let mut keymaps = Keymaps::default();
        let mut bad_binds = Vec::new();
        for (mode_keys, trie) in [
            (&config.keys_normal, &mut keymaps.normal),
            (&config.keys_insert, &mut keymaps.insert),
        ] {
            for (seq, command) in mode_keys {
                if commands::find(command).is_some() {
                    trie.bind_str(seq, command);
                } else {
                    bad_binds.push(command.clone());
                }
            }
        }
        let status = if bad_binds.is_empty() {
            String::new()
        } else {
            format!("crow.toml: unknown command(s): {}", bad_binds.join(", "))
        };

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
            status,
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
            lsp: None,
            lsp_failed: false,
            diagnostics: HashMap::new(),
            picker: None,
            completion: None,
            help_scroll: None,
            command_suggest: None,
            tree: None,
            tree_focused: false,
            tree_leader: false,
            tree_input: None,
            tree_clipboard: None,
            should_quit: false,
            size,
            keymaps,
            lsp_table: config.lsp.clone(),
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

    /// Width of the file tree sidebar, 0 when hidden.
    pub fn tree_width(&self) -> u16 {
        if self.tree.is_some() {
            30.min(self.size.0 / 3)
        } else {
            0
        }
    }

    /// Every window's rectangle plus the separators between them. The text
    /// area is everything but the status and command lines, minus the tree
    /// sidebar when it is visible.
    pub fn window_rects(&self) -> (Vec<(usize, Rect)>, Vec<(Rect, bool)>) {
        let tree_w = self.tree_width();
        let area = (
            tree_w,
            0,
            self.size.0.saturating_sub(tree_w),
            self.size.1.saturating_sub(2),
        );
        let mut wins = Vec::new();
        let mut seps = Vec::new();
        self.layout.rects(area, &mut wins, &mut seps);
        (wins, seps)
    }

    /// The `:help` window: centered, most of the screen.
    pub fn help_rect(&self) -> Rect {
        let w = ((self.size.0 as usize) * 3 / 4).clamp(30, 90) as u16;
        let w = w.min(self.size.0);
        let h = self.size.1.saturating_sub(6).max(3);
        let x = (self.size.0.saturating_sub(w)) / 2;
        (x, 2, w, h)
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

    /// Move focus to the nearest window left/right of the current one.
    /// Returns false when there is none in that direction.
    pub fn focus_window_horizontal(&mut self, left: bool) -> bool {
        let (wins, _) = self.window_rects();
        let (fx, fy, _, fh) = self.focused_rect();
        let fmid = fy as i32 + fh as i32 / 2;
        let target = wins
            .iter()
            .filter(|&&(id, (x, ..))| id != self.focused && if left { x < fx } else { x > fx })
            .min_by_key(|&&(_, (x, y, _, h))| {
                let dx = if left { fx - x } else { x - fx };
                (dx, (y as i32 + h as i32 / 2 - fmid).abs())
            })
            .map(|&(id, _)| id);
        let Some(id) = target else {
            return false;
        };
        self.save_focus_state();
        self.focused = id;
        self.restore_focus_state();
        true
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

        if let Some(scroll) = self.help_scroll {
            self.handle_help_key(key, scroll);
            return;
        }
        if self.mode == Mode::Command {
            self.handle_command_key(key);
            return;
        }
        if self.mode == Mode::Search {
            self.handle_search_key(key);
            return;
        }
        if self.mode == Mode::Picker {
            self.handle_picker_key(key);
            return;
        }
        if self.tree_focused {
            self.handle_tree_key(key);
            return;
        }
        if self.mode == Mode::Insert && self.completion.is_some() && self.handle_completion_key(key)
        {
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
                        self.insert_typed(c);
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

    /// Keys while the `:help` window is open: scroll or close.
    fn handle_help_key(&mut self, key: Key, scroll: usize) {
        let (_, _, _, h) = self.help_rect();
        let visible = (h as usize).saturating_sub(1); // minus the title row
        let max = crate::commands::help_lines().len().saturating_sub(visible);
        let clamp = |s: usize| Some(s.min(max));
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.help_scroll = None,
            // The focus keys work from here too: close, then move.
            KeyCode::Char('h') | KeyCode::Left if key.ctrl => {
                self.help_scroll = None;
                (crate::commands::find("focus_left").unwrap().func)(self);
            }
            KeyCode::Char('l') | KeyCode::Right if key.ctrl => {
                self.help_scroll = None;
                (crate::commands::find("focus_right").unwrap().func)(self);
            }
            KeyCode::Up | KeyCode::Char('k') => self.help_scroll = clamp(scroll.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.help_scroll = clamp(scroll + 1),
            KeyCode::Char('d') if key.ctrl => self.help_scroll = clamp(scroll + visible / 2),
            KeyCode::Char('u') if key.ctrl => {
                self.help_scroll = clamp(scroll.saturating_sub(visible / 2))
            }
            KeyCode::PageDown => self.help_scroll = clamp(scroll + visible),
            KeyCode::PageUp => self.help_scroll = clamp(scroll.saturating_sub(visible)),
            KeyCode::Char('g') => self.help_scroll = Some(0),
            KeyCode::Char('G') => self.help_scroll = Some(max),
            _ => {}
        }
    }

    fn handle_command_key(&mut self, key: Key) {
        match key.code {
            KeyCode::Esc => {
                self.command_line.clear();
                self.command_suggest = None;
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                // A highlighted suggestion wins; otherwise the line as typed.
                let line = match self.command_suggest.take() {
                    Some(i) => self
                        .command_suggestions()
                        .into_iter()
                        .nth(i)
                        .unwrap_or_else(|| std::mem::take(&mut self.command_line)),
                    None => std::mem::take(&mut self.command_line),
                };
                self.command_line.clear();
                self.mode = Mode::Normal;
                self.execute_command(&line);
            }
            KeyCode::Tab | KeyCode::Down => {
                let n = self.command_suggestions().len();
                if n > 0 {
                    self.command_suggest = Some(self.command_suggest.map_or(0, |i| (i + 1) % n));
                }
            }
            KeyCode::Up => {
                let n = self.command_suggestions().len();
                if n > 0 {
                    self.command_suggest =
                        Some(self.command_suggest.map_or(n - 1, |i| (i + n - 1) % n));
                }
            }
            KeyCode::Backspace => {
                self.command_suggest = None;
                if self.command_line.pop().is_none() {
                    self.mode = Mode::Normal;
                }
            }
            KeyCode::Char(c) if !key.ctrl && !key.alt => {
                self.command_suggest = None;
                self.command_line.push(c);
            }
            _ => {}
        }
    }

    /// Fuzzy matches for the command word being typed at the `:` prompt.
    /// Empty once an argument starts — only the command itself completes.
    pub fn command_suggestions(&self) -> Vec<String> {
        let line = &self.command_line;
        if line.is_empty() || line.contains(' ') || line.chars().all(|c| c.is_ascii_digit()) {
            return Vec::new();
        }
        const BUILTINS: &[&str] =
            &["w", "q", "q!", "wq", "e", "fmt", "bn", "bp", "theme", "config", "help"];
        let mut scored: Vec<(i64, String)> = BUILTINS
            .iter()
            .map(|s| s.to_string())
            .chain(crate::commands::COMMANDS.iter().map(|c| c.name.to_string()))
            .filter_map(|name| {
                crate::picker::fuzzy_score(line, &name).map(|score| (score, name))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        scored.truncate(8);
        scored.into_iter().map(|(_, name)| name).collect()
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
            "help" | "h" => self.help_scroll = Some(0),
            "fmt" | "format" => (commands::find("format_buffer").unwrap().func)(self),
            "bn" => (commands::find("next_buffer").unwrap().func)(self),
            "bp" => (commands::find("prev_buffer").unwrap().func)(self),
            "theme" => match arg {
                Some(name) => {
                    if crate::theme::set(name) {
                        self.set_status(format!("theme: {name}"));
                    } else {
                        self.set_status(format!(
                            "Unknown theme {name:?}. Available: {}",
                            crate::theme::names()
                        ));
                    }
                }
                None => self.set_status(format!("Themes: {}", crate::theme::names())),
            },
            "config" => match Document::open(crate::config::path()) {
                Ok(doc) => {
                    self.documents.push(doc);
                    self.current = self.documents.len() - 1;
                    self.set_status("editing crow.toml — changes apply on restart");
                }
                Err(e) => self.set_status(format!("Error: {e}")),
            },
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

    // ---- picker ------------------------------------------------------------

    pub fn open_picker(&mut self, picker: crate::picker::Picker) {
        self.picker = Some(picker);
        self.set_mode(Mode::Picker);
    }

    fn close_picker(&mut self) {
        self.picker = None;
        self.set_mode(Mode::Normal);
    }

    fn handle_picker_key(&mut self, key: Key) {
        use crate::picker::Kind;
        let Some(picker) = self.picker.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        match key.code {
            KeyCode::Esc => self.picker_cancel(),
            // The focus keys work from here too: cancel, then move.
            KeyCode::Char('h') | KeyCode::Left if key.ctrl => {
                self.picker_cancel();
                (crate::commands::find("focus_left").unwrap().func)(self);
            }
            KeyCode::Char('l') | KeyCode::Right if key.ctrl => {
                self.picker_cancel();
                (crate::commands::find("focus_right").unwrap().func)(self);
            }
            KeyCode::Enter => self.picker_accept(),
            KeyCode::Down => self.picker_move(1),
            KeyCode::Up => self.picker_move(-1),
            KeyCode::Char('n') if key.ctrl => self.picker_move(1),
            KeyCode::Char('p') if key.ctrl => self.picker_move(-1),
            KeyCode::Backspace => {
                if picker.query.pop().is_some() {
                    picker.refilter();
                    self.picker_preview();
                } else if let Kind::Explorer { dir } = &picker.kind {
                    // Empty query: backspace climbs to the parent directory.
                    let parent = dir.parent().map(Path::to_path_buf);
                    if let Some(parent) = parent {
                        *picker = crate::picker::Picker::explorer(parent);
                    }
                }
            }
            KeyCode::Char(c) if !key.ctrl && !key.alt => {
                picker.query.push(c);
                picker.refilter();
                self.picker_preview();
            }
            _ => {}
        }
    }

    /// Close the picker without accepting, undoing any live theme preview.
    fn picker_cancel(&mut self) {
        if let Some(picker) = &self.picker {
            if let crate::picker::Kind::Theme { original } = &picker.kind {
                crate::theme::set(original);
            }
        }
        self.close_picker();
    }

    fn picker_move(&mut self, delta: isize) {
        if let Some(picker) = self.picker.as_mut() {
            picker.move_selection(delta);
        }
        self.picker_preview();
    }

    /// Theme picking previews live: the highlighted theme is applied at once.
    fn picker_preview(&mut self) {
        let Some(picker) = &self.picker else {
            return;
        };
        if matches!(picker.kind, crate::picker::Kind::Theme { .. }) {
            if let Some(item) = picker.selected_item() {
                let name = item.label.clone();
                crate::theme::set(&name);
            }
        }
    }

    fn picker_accept(&mut self) {
        use crate::picker::Kind;
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some(item) = picker.selected_item() else {
            self.close_picker();
            return;
        };
        let label = item.label.clone();
        self.close_picker();
        match picker.kind {
            Kind::Command => {
                if let Some(command) = commands::find(&label) {
                    self.register_fresh = true;
                    (command.func)(self);
                }
            }
            Kind::Theme { .. } => {
                crate::theme::set(&label);
                self.set_status(format!("theme: {label}"));
            }
            Kind::Files { root } => self.jump_to(root.join(label), 0, 0),
            Kind::Explorer { dir } => {
                if label == "../" {
                    if let Some(parent) = dir.parent() {
                        self.open_picker(crate::picker::Picker::explorer(parent.to_path_buf()));
                    }
                } else if let Some(subdir) = label.strip_suffix('/') {
                    self.open_picker(crate::picker::Picker::explorer(dir.join(subdir)));
                } else {
                    self.jump_to(dir.join(label), 0, 0);
                }
            }
        }
    }

    /// The floating `:`/`/` prompt: (x, y, width), centered near the top.
    pub fn prompt_rect(&self) -> (u16, u16, u16) {
        let w = ((self.size.0 as usize) * 3 / 5).clamp(20, 70) as u16;
        let x = (self.size.0.saturating_sub(w)) / 2;
        (x, 1, w)
    }

    /// Overlay rectangle for the picker, centered near the top.
    pub fn picker_rect(&self) -> Rect {
        let w = ((self.size.0 as usize) * 3 / 4).clamp(20, 80) as u16;
        let h = 12.min(self.size.1.saturating_sub(4)).max(2);
        let x = (self.size.0.saturating_sub(w)) / 2;
        (x, 1, w, h)
    }

    // ---- file tree ---------------------------------------------------------

    /// `space e` from anywhere: hidden -> shown+focused, unfocused -> focused,
    /// focused -> closed. Every state reaches every other with the same key.
    pub fn tree_toggle(&mut self) {
        match (&self.tree, self.tree_focused) {
            (None, _) => {
                let root = std::env::current_dir().unwrap_or_default();
                self.tree = Some(crate::filetree::FileTree::new(root));
                self.tree_focused = true;
            }
            (Some(_), false) => self.tree_focused = true,
            (Some(_), true) => {
                self.tree = None;
                self.tree_focused = false;
            }
        }
    }

    fn handle_tree_key(&mut self, key: Key) {
        if self.tree_input.is_some() {
            self.handle_tree_input_key(key);
            return;
        }
        // The tree owns the keyboard while focused, so it must recognize the
        // `space e` toggle itself — otherwise the sidebar could never close.
        if self.tree_leader {
            self.tree_leader = false;
            if key.code == KeyCode::Char('e') && !key.ctrl && !key.alt {
                self.tree_toggle();
            }
            return;
        }
        if key.code == KeyCode::Char(' ') && !key.ctrl && !key.alt {
            self.tree_leader = true;
            return;
        }
        let Some(tree) = self.tree.as_mut() else {
            self.tree_focused = false;
            return;
        };
        match key.code {
            KeyCode::Char('l') | KeyCode::Right if key.ctrl => self.tree_focused = false,
            KeyCode::Esc => self.tree_focused = false,
            KeyCode::Char('q') => {
                self.tree = None;
                self.tree_focused = false;
            }
            KeyCode::Up | KeyCode::Char('k') => tree.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => tree.move_selection(1),
            KeyCode::Char('h') | KeyCode::Left => tree.collapse_or_parent(),
            KeyCode::Char('R') => tree.rebuild(),
            KeyCode::Char('r') => {
                if let Some(row) = tree.selected_row() {
                    if row.path == tree.root {
                        self.set_status("the root can't be renamed from here");
                    } else {
                        self.tree_input = Some(TreeInput::Rename {
                            path: row.path.clone(),
                            name: row.name.clone(),
                        });
                    }
                }
            }
            KeyCode::Char('a') => {
                // New entries go into the selected directory, or beside the
                // selected file.
                let dir = match tree.selected_row() {
                    Some(row) if row.is_dir => row.path.clone(),
                    Some(row) => row
                        .path
                        .parent()
                        .unwrap_or(&tree.root)
                        .to_path_buf(),
                    None => tree.root.clone(),
                };
                self.tree_input = Some(TreeInput::Create {
                    dir,
                    name: String::new(),
                });
            }
            KeyCode::Char('d') => {
                if let Some(row) = tree.selected_row() {
                    if row.path == tree.root {
                        self.set_status("not deleting the project root");
                    } else {
                        self.tree_input = Some(TreeInput::Delete {
                            path: row.path.clone(),
                        });
                    }
                }
            }
            KeyCode::Char('x') | KeyCode::Char('c') => {
                if let Some(row) = tree.selected_row() {
                    if row.path == tree.root {
                        self.set_status("the root can't be cut or copied");
                        return;
                    }
                    let cut = key.code == KeyCode::Char('x');
                    let name = row.name.clone();
                    self.tree_clipboard = Some((row.path.clone(), cut));
                    self.set_status(format!(
                        "{} {name} — p pastes",
                        if cut { "cut" } else { "copied" }
                    ));
                }
            }
            KeyCode::Char('p') => self.tree_paste(),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                let Some(row) = tree.selected_row() else {
                    return;
                };
                if row.is_dir {
                    tree.toggle_selected();
                } else {
                    let path = row.path.clone();
                    self.tree_focused = false;
                    self.jump_to(path, 0, 0);
                }
            }
            _ => {}
        }
    }

    /// Keys while a tree create/delete prompt is open. `take` + re-store
    /// keeps the borrow checker out of the way.
    fn handle_tree_input_key(&mut self, key: Key) {
        let Some(input) = self.tree_input.take() else {
            return;
        };
        match input {
            TreeInput::Create { dir, mut name } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.tree_create(&dir, name.trim()),
                KeyCode::Backspace => {
                    name.pop();
                    self.tree_input = Some(TreeInput::Create { dir, name });
                }
                KeyCode::Char(c) if !key.ctrl && !key.alt => {
                    name.push(c);
                    self.tree_input = Some(TreeInput::Create { dir, name });
                }
                _ => self.tree_input = Some(TreeInput::Create { dir, name }),
            },
            TreeInput::Delete { path } => {
                if key.code == KeyCode::Char('y') {
                    self.tree_delete(&path);
                }
            }
            TreeInput::Rename { path, mut name } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.tree_rename(&path, name.trim()),
                KeyCode::Backspace => {
                    name.pop();
                    self.tree_input = Some(TreeInput::Rename { path, name });
                }
                KeyCode::Char(c) if !key.ctrl && !key.alt => {
                    name.push(c);
                    self.tree_input = Some(TreeInput::Rename { path, name });
                }
                _ => self.tree_input = Some(TreeInput::Rename { path, name }),
            },
        }
    }

    fn tree_create(&mut self, dir: &Path, name: &str) {
        if name.is_empty() {
            return;
        }
        let target = dir.join(name);
        let result = if name.ends_with('/') {
            std::fs::create_dir_all(&target)
        } else {
            // `a src/deep/new.rs` works: intermediate directories appear too.
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // create_new: never truncate something that already exists.
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map(|_| ())
        };
        match result {
            Ok(()) => {
                if let Some(tree) = self.tree.as_mut() {
                    tree.reveal(&target);
                }
                self.set_status(format!("created {name}"));
            }
            Err(e) => self.set_status(format!("Error: {e}")),
        }
    }

    fn tree_rename(&mut self, path: &Path, name: &str) {
        if name.is_empty() {
            return;
        }
        let target = path.parent().unwrap_or(Path::new("")).join(name);
        if target == path {
            return;
        }
        if target.exists() {
            self.set_status(format!("Error: {name:?} already exists"));
            return;
        }
        // A name with slashes is a move; the intermediate directories appear.
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(path, &target) {
            Ok(()) => {
                self.retarget_buffers(path, &target);
                if let Some(tree) = self.tree.as_mut() {
                    tree.reveal(&target);
                }
                self.set_status(format!("renamed to {name}"));
            }
            Err(e) => self.set_status(format!("Error: {e}")),
        }
    }

    fn tree_paste(&mut self) {
        let Some((src, cut)) = self.tree_clipboard.clone() else {
            self.set_status("nothing cut or copied");
            return;
        };
        let Some(tree) = self.tree.as_ref() else {
            return;
        };
        // Same landing rule as `a`: the selected directory, or beside the
        // selected file.
        let dir = match tree.selected_row() {
            Some(row) if row.is_dir => row.path.clone(),
            Some(row) => row.path.parent().unwrap_or(&tree.root).to_path_buf(),
            None => tree.root.clone(),
        };
        let Some(name) = src.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };
        let target = dir.join(&name);
        if target == src {
            self.set_status("already there");
            return;
        }
        if target.exists() {
            self.set_status(format!("Error: {name:?} already exists here"));
            return;
        }
        if src.is_dir() && dir.starts_with(&src) {
            self.set_status("Error: can't paste a directory into itself");
            return;
        }

        let result = if cut {
            std::fs::rename(&src, &target)
        } else {
            crate::filetree::copy_recursively(&src, &target)
        };
        match result {
            Ok(()) => {
                if cut {
                    // The move is done: open buffers follow, and a second
                    // paste would be meaningless.
                    self.retarget_buffers(&src, &target);
                    self.tree_clipboard = None;
                }
                if let Some(tree) = self.tree.as_mut() {
                    tree.reveal(&target);
                }
                self.set_status(format!("{} {name}", if cut { "moved" } else { "copied" }));
            }
            Err(e) => self.set_status(format!("Error: {e}")),
        }
    }

    /// Point any open buffer at a file's new location after a move.
    fn retarget_buffers(&mut self, old: &Path, new: &Path) {
        for doc in &mut self.documents {
            if doc.path.as_deref() == Some(old) {
                doc.path = Some(new.to_path_buf());
                doc.refresh_syntax();
            }
        }
    }

    fn tree_delete(&mut self, path: &Path) {
        // ponytail: a real delete, not a trash can — the y/n prompt names
        // exactly what goes.
        let result = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match result {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Some(tree) = self.tree.as_mut() {
                    tree.rebuild();
                }
                self.set_status(format!("deleted {name}"));
            }
            Err(e) => self.set_status(format!("Error: {e}")),
        }
    }

    // ---- completion --------------------------------------------------------

    /// Handle a key while the completion menu is open. Returns true if the
    /// key was consumed.
    fn handle_completion_key(&mut self, key: Key) -> bool {
        let Some(completion) = self.completion.as_mut() else {
            return false;
        };
        match key.code {
            KeyCode::Enter if completion.auto => {
                self.completion = None;
                false // the newline happens normally
            }
            KeyCode::Tab | KeyCode::Enter => {
                self.completion_accept();
                true
            }
            KeyCode::Esc => {
                self.completion = None;
                true
            }
            KeyCode::Down => {
                completion.selected = (completion.selected + 1) % completion.items.len();
                true
            }
            KeyCode::Up => {
                completion.selected =
                    (completion.selected + completion.items.len() - 1) % completion.items.len();
                true
            }
            KeyCode::Char('n') if key.ctrl => {
                completion.selected = (completion.selected + 1) % completion.items.len();
                true
            }
            KeyCode::Char('p') if key.ctrl => {
                completion.selected =
                    (completion.selected + completion.items.len() - 1) % completion.items.len();
                true
            }
            KeyCode::Char('/') if !key.ctrl && !key.alt => {
                // A slash ends the current word; for paths it descends, so
                // start over and list the next directory.
                self.completion = None;
                self.doc_mut().insert_at_cursor("/");
                self.maybe_autocomplete();
                true
            }
            KeyCode::Char(c) if !key.ctrl && !key.alt => {
                // Type through the menu: insert the char and narrow the list.
                completion.prefix.push(c);
                let prefix = completion.prefix.to_lowercase();
                completion.items.retain(|(label, _)| label.to_lowercase().starts_with(&prefix));
                completion.selected = 0;
                let empty = completion.items.is_empty();
                self.doc_mut().insert_at_cursor(&c.to_string());
                if empty {
                    self.completion = None;
                }
                true
            }
            _ => {
                // Anything else (backspace, arrows, escape sequences…) closes
                // the menu and is handled normally.
                self.completion = None;
                false
            }
        }
    }

    fn completion_accept(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some((_, text)) = completion.items.get(completion.selected) else {
            return;
        };
        let prefix_chars = completion.prefix.chars().count();
        let entered_dir = text.ends_with('/');
        if text.to_lowercase().starts_with(&completion.prefix.to_lowercase()) {
            // The typed prefix stands; append the rest at every cursor.
            let suffix: String = text.chars().skip(prefix_chars).collect();
            self.doc_mut().insert_at_cursor(&suffix);
        } else {
            // ponytail: replacement completions rewrite the primary cursor
            // only; per-cursor replacement when multi-cursor completion itches.
            let doc = self.doc_mut();
            let from = doc.cursor.saturating_sub(prefix_chars);
            doc.delete_range(from, doc.cursor);
            let text = text.clone();
            self.doc_mut().insert_at_cursor(&text);
        }
        // Accepting a directory rolls straight into listing its contents.
        if entered_dir {
            self.maybe_autocomplete();
        }
    }

    /// The identifier fragment just before the cursor.
    fn word_prefix(&self) -> String {
        let doc = self.doc();
        let (line, col) = doc.cursor_line_col();
        let slice = doc.line(line);
        let mut start = col;
        while start > 0 {
            let c = slice.char(start - 1);
            if c.is_alphanumeric() || c == '_' {
                start -= 1;
            } else {
                break;
            }
        }
        (start..col).map(|i| slice.char(i)).collect()
    }

    // ---- lsp ---------------------------------------------------------------

    /// Called from the main loop between keystrokes: keep the server in sync
    /// with edited buffers and apply anything it sent back.
    /// Drains language-server messages. Returns true when anything on screen
    /// may have changed, so the main loop knows a redraw is needed.
    pub fn lsp_tick(&mut self) -> bool {
        self.lsp_sync();
        let Some(lsp) = self.lsp.as_mut() else {
            return false;
        };
        let events = lsp.poll();
        if lsp.is_dead() {
            // The server exited (crashed, or was a broken shim): stop syncing
            // and don't respawn every tick.
            self.lsp = None;
            self.lsp_failed = true;
        }
        let changed = !events.is_empty();
        for event in events {
            match event {
                lsp::Event::Definition(path, line, col) => self.jump_to(path, line, col),
                lsp::Event::Hover(text) => self.set_status(text),
                lsp::Event::Status(text) => self.set_status(text),
                lsp::Event::Diagnostics(path, diags) => {
                    self.diagnostics.insert(path, diags);
                }
                lsp::Event::Completions(items) => self.show_completions(items),
            }
        }
        changed
    }

    fn show_completions(&mut self, items: Vec<(String, String)>) {
        if self.mode != Mode::Insert {
            return; // the answer arrived after insert mode ended
        }
        let prefix = self.word_prefix();
        let lower = prefix.to_lowercase();
        let mut items: Vec<(String, String)> = items
            .into_iter()
            .filter(|(label, _)| label.to_lowercase().starts_with(&lower))
            .collect();
        items.truncate(50);
        if items.is_empty() {
            self.set_status("no completions");
            self.completion = None;
        } else {
            self.completion = Some(Completion {
                items,
                selected: 0,
                prefix,
                auto: false,
            });
        }
    }

    /// One typed character in insert mode: bracket/quote pairs close
    /// themselves, retyping a closer steps over it, and identifier chars
    /// feed the intellisense popup.
    fn insert_typed(&mut self, c: char) {
        // A closer typed on a whitespace-only line dedents it one level first.
        if matches!(c, ')' | ']' | '}') && self.doc().extra.is_empty() {
            let doc = self.doc();
            let (line, col) = doc.cursor_line_col();
            let slice = doc.line(line);
            if col > 0 && (0..col).all(|i| matches!(slice.char(i), ' ' | '\t')) {
                let take = if slice.char(col - 1) == '\t' {
                    1
                } else {
                    crate::config::tab_width().min(col)
                };
                let to = doc.cursor;
                self.doc_mut().delete_range(to - take, to);
            }
        }

        if crate::config::autoclose() {
            let doc = self.doc();
            let next = (doc.cursor < doc.text.len_chars()).then(|| doc.text.char(doc.cursor));
            let prev = (doc.cursor > 0).then(|| doc.text.char(doc.cursor - 1));

            // Retyping the closer that's already there steps over it.
            if matches!(c, ')' | ']' | '}' | '"' | '\'') && next == Some(c) {
                let doc = self.doc_mut();
                let len = doc.text.len_chars();
                doc.cursor = (doc.cursor + 1).min(len);
                doc.anchor = doc.cursor;
                for (a, cur) in &mut doc.extra {
                    *cur = (*cur + 1).min(len);
                    *a = *cur;
                }
                return;
            }

            // Openers bring their closer; quotes only where a pair reads as
            // one (not right after a word: don't, can't…).
            let close = match c {
                '(' => Some(')'),
                '[' => Some(']'),
                '{' => Some('}'),
                '"' | '\'' if !prev.is_some_and(|p| p.is_alphanumeric() || p == '_') => Some(c),
                _ => None,
            };
            if let Some(close) = close {
                let pair: String = [c, close].iter().collect();
                self.doc_mut().insert_at_cursor(&pair);
                // Every cursor steps back between its pair.
                let doc = self.doc_mut();
                doc.cursor = doc.cursor.saturating_sub(1);
                doc.anchor = doc.cursor;
                for (a, cur) in &mut doc.extra {
                    *cur = cur.saturating_sub(1);
                    *a = *cur;
                }
                return;
            }
        }

        self.doc_mut().insert_at_cursor(&c.to_string());
        if c.is_alphanumeric() || c == '_' || c == '/' {
            self.maybe_autocomplete();
        }
    }

    /// Intellisense while typing: once two identifier chars are down, offer
    /// matching words from every open buffer. Instant and offline; `C-space`
    /// still asks the language server for the smart list.
    fn maybe_autocomplete(&mut self) {
        if self.completion.is_some() {
            return;
        }
        if let Some(completion) = self.path_completion() {
            self.completion = Some(completion);
            return;
        }
        let prefix = self.word_prefix();
        if prefix.chars().count() < 2 {
            return;
        }
        let items = self.buffer_words(&prefix);
        if !items.is_empty() {
            self.completion = Some(Completion {
                items,
                selected: 0,
                prefix,
                auto: true,
            });
        }
    }

    /// Words from all open buffers matching `prefix`, excluding the prefix
    /// itself. ponytail: a full scan per popup; a word index maintained on
    /// edit when huge buffers itch.
    fn buffer_words(&self, prefix: &str) -> Vec<(String, String)> {
        let lower = prefix.to_lowercase();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for doc in &self.documents {
            let mut word = String::new();
            for c in doc.text.chars().chain(std::iter::once(' ')) {
                if c.is_alphanumeric() || c == '_' {
                    word.push(c);
                    continue;
                }
                if word.chars().count() >= 3
                    && word.to_lowercase().starts_with(&lower)
                    && word != prefix
                    && seen.insert(word.clone())
                {
                    out.push((word.clone(), word.clone()));
                }
                word.clear();
            }
            if out.len() >= 50 {
                break;
            }
        }
        out.sort();
        out.truncate(50);
        out
    }

    /// Filesystem completion for a `./`, `../` or absolute path being typed.
    /// Returns None when the text before the cursor doesn't look like one.
    fn path_completion(&self) -> Option<Completion> {
        let doc = self.doc();
        let (line, col) = doc.cursor_line_col();
        let slice = doc.line(line);
        let mut start = col;
        while start > 0 {
            let c = slice.char(start - 1);
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~') {
                start -= 1;
            } else {
                break;
            }
        }
        let token: String = (start..col).map(|i| slice.char(i)).collect();
        // A bare "/" (division, say) doesn't count; "./", "../", "~/" and "/usr" do.
        let looks_like_path = token.starts_with("./")
            || token.starts_with("../")
            || token.starts_with("~/")
            || (token.starts_with('/') && token.len() > 1);
        if !looks_like_path {
            return None;
        }
        let (dir, prefix) = token.rsplit_once('/')?;
        let dir = match dir.strip_prefix('~') {
            Some(rest) => format!("{}{rest}", std::env::var("HOME").ok()?),
            None => dir.to_string(),
        };
        let lower = prefix.to_lowercase();
        let mut items: Vec<(String, String)> = std::fs::read_dir(format!("{dir}/"))
            .ok()?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let mut name = entry.file_name().into_string().ok()?;
                // Hidden entries only once the prefix opts in with a dot.
                if name.starts_with('.') && !prefix.starts_with('.') {
                    return None;
                }
                if !name.to_lowercase().starts_with(&lower) || name == prefix {
                    return None;
                }
                if entry.file_type().ok()?.is_dir() {
                    name.push('/');
                }
                Some((name.clone(), name))
            })
            .collect();
        items.sort();
        items.truncate(50);
        (!items.is_empty()).then(|| Completion {
            items,
            selected: 0,
            prefix: prefix.to_string(),
            auto: true,
        })
    }

    fn lsp_sync(&mut self) {
        if self.lsp.is_none() {
            if self.lsp_failed {
                return;
            }
            // ponytail: one server per session — the first open file with a
            // configured server picks it; a client per language when
            // multi-language sessions itch.
            let command = self
                .documents
                .iter()
                .filter_map(|d| d.path.as_deref())
                .find_map(|p| server_for(&self.lsp_table, p))
                .map(str::to_string);
            let Some(command) = command else {
                return;
            };
            let root = std::env::current_dir().unwrap_or_default();
            match lsp::Client::spawn(&root, &command) {
                Some(client) => self.lsp = Some(client),
                None => {
                    self.lsp_failed = true;
                    self.set_status(format!("could not start {command:?} — no LSP"));
                    return;
                }
            }
        }
        let table = &self.lsp_table;
        let lsp = self.lsp.as_mut().unwrap();
        for doc in &self.documents {
            let Some(path) = doc.path.as_ref().filter(|p| server_for(table, p).is_some())
            else {
                continue;
            };
            match lsp.synced.get(path.as_path()) {
                None => lsp.did_open(path, doc.text.to_string(), doc.revision),
                Some(&(_, revision)) if revision != doc.revision => {
                    lsp.did_change(path, doc.text.to_string(), doc.revision)
                }
                Some(_) => {}
            }
        }
    }

    /// Jump to a position given as (path, line, UTF-16 column) — reusing an
    /// open buffer for the file when there is one.
    pub fn jump_to(&mut self, path: PathBuf, line: usize, utf16_col: usize) {
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        let existing = self.documents.iter().position(|d| {
            d.path
                .as_ref()
                .and_then(|p| p.canonicalize().ok())
                .is_some_and(|p| p == canon)
        });
        let idx = match existing {
            Some(i) => i,
            None => match Document::open(&path) {
                Ok(doc) => {
                    self.documents.push(doc);
                    self.documents.len() - 1
                }
                Err(e) => {
                    self.set_status(format!("Error: {e}"));
                    return;
                }
            },
        };
        self.current = idx;
        let doc = self.doc_mut();
        let line = line.min(doc.line_count().saturating_sub(1));
        let col = crate::position::utf16_to_char(doc.line(line), utf16_col);
        doc.cursor = doc.line_start(line) + col;
        doc.anchor = doc.cursor;
        doc.clamp_cursor(false);
        doc.goal_col = None;
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
        let scrolloff = crate::config::scrolloff().min(height.saturating_sub(1) / 2);

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
        if self.help_scroll.is_some() {
            return None; // the help window has no cursor
        }
        if let Some(TreeInput::Create { name, .. } | TreeInput::Rename { name, .. }) =
            &self.tree_input
        {
            let (x, y, w) = self.prompt_rect();
            let col = 2 + name.chars().count();
            return Some((x + (col as u16).min(w.saturating_sub(2)), y + 1));
        }
        if self.tree_focused {
            return None; // the tree's reversed row is the focus indicator
        }
        if self.mode == Mode::Picker {
            let picker = self.picker.as_ref()?;
            let (x, y, w, _) = self.picker_rect();
            let col = 4 + picker.query.chars().count(); // after "│ ▸ "
            return Some((x + (col as u16).min(w.saturating_sub(2)), y + 1));
        }
        if matches!(self.mode, Mode::Command | Mode::Search) {
            // Inside the floating prompt: after the border, space, and prefix.
            let (x, y, w) = self.prompt_rect();
            let col = 3 + self.command_line.chars().count();
            return Some((x + (col as u16).min(w.saturating_sub(2)), y + 1));
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
        let display = crate::position::char_to_display_col(doc.line(line), col, crate::config::tab_width());
        format!("{}:{}", line + 1, display + 1)
    }
}

/// The configured server command for a file, by extension.
fn server_for<'a>(table: &'a [(String, String)], path: &Path) -> Option<&'a str> {
    let ext = path.extension()?.to_str()?;
    table
        .iter()
        .find(|(e, _)| e == ext)
        .map(|(_, cmd)| cmd.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::Key;

    fn editor_with(text: &str) -> Editor {
        let mut editor =
            Editor::new(vec![], (80, 24), &crate::config::Config::default()).unwrap();
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
    fn command_palette_runs_the_picked_command() {
        let mut editor = editor_with("hello");
        press(&mut editor, "<space> c");
        assert_eq!(editor.mode, Mode::Picker);
        press(&mut editor, "quit");
        press(&mut editor, "<enter>");
        assert!(editor.should_quit);
    }

    #[test]
    fn theme_picker_previews_live_and_esc_restores() {
        let _guard = crate::theme::TEST_LOCK.lock().unwrap();
        crate::theme::set("default");
        let mut editor = editor_with("hello");
        press(&mut editor, "<space> t");
        press(&mut editor, "<down>"); // move to the second theme: live preview
        assert_ne!(crate::theme::current().name, "default");
        press(&mut editor, "<esc>");
        assert_eq!(crate::theme::current().name, "default");
        assert_eq!(editor.mode, Mode::Normal);
    }

    #[test]
    fn completion_accepts_by_appending_the_suffix() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "pri");
        editor.show_completions(vec![
            ("println!".into(), "println!".into()),
            ("print!".into(), "print!".into()),
        ]);
        assert_eq!(editor.completion.as_ref().unwrap().items.len(), 2);
        press(&mut editor, "<tab>");
        assert_eq!(editor.doc().text.to_string(), "println!");
        assert!(editor.completion.is_none());
    }

    #[test]
    fn typing_narrows_the_completion_menu() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "p");
        editor.show_completions(vec![
            ("print!".into(), "print!".into()),
            ("push".into(), "push".into()),
        ]);
        press(&mut editor, "u"); // types through the menu
        assert_eq!(editor.doc().text.to_string(), "pu");
        assert_eq!(editor.completion.as_ref().unwrap().items.len(), 1);
        press(&mut editor, "<tab>");
        assert_eq!(editor.doc().text.to_string(), "push");
    }

    #[test]
    fn ctrl_h_closes_the_picker_and_moves_focus() {
        let mut editor = editor_with("hello");
        press(&mut editor, "<space> f");
        assert_eq!(editor.mode, Mode::Picker);
        press(&mut editor, "C-h");
        assert!(editor.picker.is_none());
        assert!(editor.tree_focused);
    }

    #[test]
    fn command_bar_suggests_and_tab_enter_runs_the_pick() {
        let mut editor = editor_with("hello");
        press(&mut editor, ":");
        press(&mut editor, "qui");
        let suggestions = editor.command_suggestions();
        assert_eq!(suggestions.first().map(String::as_str), Some("quit"));
        assert_eq!(editor.command_suggest, None, "nothing highlighted until Tab");
        press(&mut editor, "<tab>");
        assert_eq!(editor.command_suggest, Some(0));
        press(&mut editor, "<enter>");
        assert!(editor.should_quit, "Enter runs the highlighted suggestion");
    }

    #[test]
    fn plain_enter_runs_the_typed_line_not_a_suggestion() {
        let mut editor = editor_with("hello");
        press(&mut editor, ":");
        press(&mut editor, "42 <enter>"); // line jump: digits never suggest
        assert!(!editor.should_quit);
        assert_eq!(editor.mode, Mode::Normal);
    }

    #[test]
    fn colon_help_opens_a_scrollable_window() {
        let mut editor = editor_with("hello");
        press(&mut editor, ": help <enter>");
        assert_eq!(editor.help_scroll, Some(0));
        press(&mut editor, "j j k");
        assert_eq!(editor.help_scroll, Some(1));
        press(&mut editor, "G");
        let bottom = editor.help_scroll.unwrap();
        assert!(bottom > 1, "G jumps to the end");
        press(&mut editor, "j");
        assert_eq!(editor.help_scroll, Some(bottom), "scroll clamps at the end");
        press(&mut editor, "<esc>");
        assert_eq!(editor.help_scroll, None);
        assert_eq!(editor.mode, Mode::Normal);

        // C-h closes the window and moves focus in one press.
        press(&mut editor, ": help <enter>");
        press(&mut editor, "C-h");
        assert_eq!(editor.help_scroll, None);
        assert!(editor.tree_focused);
    }

    #[test]
    fn ctrl_h_l_move_between_splits_before_the_tree() {
        let mut editor = editor_with("hello");
        editor.split_window(true);
        let (wins, _) = editor.window_rects();
        assert_eq!(wins.len(), 2);
        let rightmost = wins.iter().max_by_key(|&&(_, (x, ..))| x).unwrap().0;
        // Start from the rightmost window: C-h crosses the split first…
        editor.focused = rightmost;
        press(&mut editor, "C-h");
        assert!(!editor.tree_focused, "split comes before the tree");
        assert_ne!(editor.focused, rightmost);
        // …and only the leftmost window opens the tree.
        press(&mut editor, "C-h");
        assert!(editor.tree_focused);
        press(&mut editor, "C-l"); // back to the editor
        assert!(!editor.tree_focused);
        press(&mut editor, "C-l"); // and across to the right window
        assert_eq!(editor.focused, rightmost);
    }

    #[test]
    fn ctrl_h_focuses_the_tree_and_ctrl_l_returns() {
        let mut editor = editor_with("hello");
        press(&mut editor, "C-h");
        assert!(editor.tree.is_some() && editor.tree_focused);
        press(&mut editor, "C-l");
        assert!(!editor.tree_focused);
        assert!(editor.tree.is_some(), "tree stays open, just unfocused");
        press(&mut editor, "C-h"); // already open: just refocus
        assert!(editor.tree_focused);
    }

    #[test]
    fn typing_a_path_pops_directory_completions() {
        let dir = std::env::temp_dir().join("crow-path-completion-test");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        let mut editor = editor_with("");
        press(&mut editor, "i");
        for c in format!("{}/", dir.display()).chars() {
            editor.handle_key(Key::char(c));
        }
        let completion = editor.completion.as_ref().expect("path menu popped");
        assert!(completion.items.iter().any(|(label, _)| label == "sub/"));
        assert!(completion.items.iter().any(|(label, _)| label == "notes.txt"));
        press(&mut editor, "n");
        press(&mut editor, "<tab>");
        assert!(editor.doc().text.to_string().ends_with("/notes.txt"));
    }

    #[test]
    fn typing_pops_intellisense_from_buffer_words() {
        let mut editor = editor_with("printer value");
        press(&mut editor, "A");
        press(&mut editor, "<space>");
        press(&mut editor, "pri"); // two identifier chars trigger the menu
        let completion = editor.completion.as_ref().expect("menu popped");
        assert!(completion.auto);
        assert_eq!(completion.items[0].0, "printer");
        press(&mut editor, "<tab>");
        assert_eq!(editor.doc().text.to_string(), "printer value printer");
    }

    #[test]
    fn enter_stays_a_newline_when_the_menu_popped_itself() {
        let mut editor = editor_with("printer value");
        press(&mut editor, "A");
        press(&mut editor, "<space>");
        press(&mut editor, "pr");
        assert!(editor.completion.is_some());
        press(&mut editor, "<enter>");
        assert!(editor.completion.is_none());
        assert_eq!(editor.doc().text.to_string(), "printer value pr\n");
    }

    #[test]
    fn tree_sidebar_toggles_focuses_and_opens_files() {
        let mut editor = editor_with("");
        press(&mut editor, "<space> e");
        assert!(editor.tree.is_some() && editor.tree_focused);
        // Windows shift right to make room for the sidebar.
        let (wins, _) = editor.window_rects();
        assert_eq!(wins[0].1 .0, editor.tree_width());
        // The tree intercepts keys: `j` moves selection, not the cursor.
        press(&mut editor, "j");
        assert_eq!(editor.doc().cursor, 0);
        press(&mut editor, "<esc>");
        assert!(!editor.tree_focused && editor.tree.is_some());
        press(&mut editor, "<space> e"); // refocus
        assert!(editor.tree_focused);
        // The same toggle closes it even though the tree has the keyboard.
        press(&mut editor, "<space> e");
        assert!(editor.tree.is_none());
        assert_eq!(editor.window_rects().0[0].1 .0, 0);
        // And a plain open-then-toggle round trip closes it too.
        press(&mut editor, "<space> e");
        assert!(editor.tree.is_some() && editor.tree_focused);
        press(&mut editor, "<space> e");
        assert!(editor.tree.is_none());
    }

    #[test]
    fn tree_a_creates_and_d_deletes_files() {
        let dir = std::env::temp_dir().join(format!("crow-tree-ops-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let mut editor = editor_with("");
        editor.tree = Some(crate::filetree::FileTree::new(dir.clone()));
        editor.tree_focused = true;

        // a, type a name, Enter -> the file exists and is selected.
        press(&mut editor, "a");
        assert!(editor.tree_input.is_some());
        press(&mut editor, "notes.txt");
        press(&mut editor, "<enter>");
        assert!(dir.join("notes.txt").is_file());
        let tree = editor.tree.as_ref().unwrap();
        assert_eq!(tree.selected_row().unwrap().name, "notes.txt");

        // Nested path: intermediate directories appear too.
        press(&mut editor, "a");
        press(&mut editor, "sub/deep.txt");
        press(&mut editor, "<enter>");
        assert!(dir.join("sub/deep.txt").is_file());

        // r renames, prefilled with the old name; buffers follow.
        editor.tree.as_mut().unwrap().reveal(&dir.join("notes.txt"));
        press(&mut editor, "r");
        assert!(matches!(
            editor.tree_input,
            Some(crate::editor::TreeInput::Rename { .. })
        ));
        press(&mut editor, "<bs> <bs> <bs>"); // "notes.txt" -> "notes."
        press(&mut editor, "md");
        press(&mut editor, "<enter>"); // -> "notes.md"
        assert!(dir.join("notes.md").is_file());
        assert!(!dir.join("notes.txt").exists());
        std::fs::rename(dir.join("notes.md"), dir.join("notes.txt")).unwrap();
        editor.tree.as_mut().unwrap().rebuild();

        // d + n leaves the file alone; d + y removes it.
        editor.tree.as_mut().unwrap().reveal(&dir.join("notes.txt"));
        press(&mut editor, "d");
        press(&mut editor, "n");
        assert!(dir.join("notes.txt").is_file());
        press(&mut editor, "d");
        press(&mut editor, "y");
        assert!(!dir.join("notes.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tree_cut_copy_paste_move_files() {
        let dir = std::env::temp_dir().join(format!("crow-tree-clip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hi").unwrap();

        let mut editor = editor_with("");
        editor.tree = Some(crate::filetree::FileTree::new(dir.clone()));
        editor.tree_focused = true;

        // Copy a.txt into sub/: original stays.
        editor.tree.as_mut().unwrap().reveal(&dir.join("a.txt"));
        press(&mut editor, "c");
        editor.tree.as_mut().unwrap().reveal(&dir.join("sub"));
        press(&mut editor, "p");
        assert!(dir.join("a.txt").is_file());
        assert!(dir.join("sub/a.txt").is_file());
        // Copy clipboard survives; pasting where it exists errors, not clobbers.
        press(&mut editor, "p");
        assert!(editor.status.contains("already exists"));

        // Cut sub/a.txt back to the root as a move (pasting beside b.txt).
        std::fs::remove_file(dir.join("a.txt")).unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();
        editor.tree.as_mut().unwrap().reveal(&dir.join("sub/a.txt"));
        press(&mut editor, "x");
        editor.tree.as_mut().unwrap().reveal(&dir.join("b.txt"));
        press(&mut editor, "p");
        assert!(dir.join("a.txt").is_file());
        assert!(!dir.join("sub/a.txt").exists());
        // Cut clipboard is spent.
        press(&mut editor, "p");
        assert!(editor.status.contains("nothing"));

        // The root header row is a valid paste target.
        std::fs::write(dir.join("sub/c.txt"), "").unwrap();
        editor.tree.as_mut().unwrap().reveal(&dir.join("sub/c.txt"));
        press(&mut editor, "x");
        editor.tree.as_mut().unwrap().selected = 0; // the root row
        press(&mut editor, "p");
        assert!(dir.join("c.txt").is_file());
        assert!(!dir.join("sub/c.txt").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn brackets_autoclose_step_over_and_backspace_as_pairs() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "(x");
        assert_eq!(editor.doc().text.to_string(), "(x)");
        press(&mut editor, ")"); // retyping the closer steps over it
        assert_eq!(editor.doc().text.to_string(), "(x)");
        assert_eq!(editor.doc().cursor, 3);
        press(&mut editor, "[");
        press(&mut editor, "<bs>"); // backspace eats the empty pair
        assert_eq!(editor.doc().text.to_string(), "(x)");
    }

    #[test]
    fn quotes_pair_except_after_word_chars() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "\"hi");
        assert_eq!(editor.doc().text.to_string(), "\"hi\"");
        press(&mut editor, "\""); // step over
        press(&mut editor, "<space>");
        press(&mut editor, "don't");
        assert_eq!(editor.doc().text.to_string(), "\"hi\" don't");
    }

    #[test]
    fn autoclose_works_at_every_cursor() {
        let mut editor = editor_with("a\nb");
        press(&mut editor, "C"); // cursor on both lines
        press(&mut editor, "i");
        press(&mut editor, "(");
        assert_eq!(editor.doc().text.to_string(), "()a\n()b");
        press(&mut editor, "x"); // typing lands inside both pairs
        assert_eq!(editor.doc().text.to_string(), "(x)a\n(x)b");
    }

    #[test]
    fn extend_mode_selects_across_lines() {
        let mut editor = editor_with("abc\ndef\nghi");
        press(&mut editor, "vjd"); // grow the selection down a line, delete
        assert_eq!(editor.doc().text.to_string(), "def\nghi");
        press(&mut editor, "vjd");
        assert_eq!(editor.doc().text.to_string(), "ghi");
    }

    #[test]
    fn config_keys_bind_registry_commands() {
        let mut config = crate::config::Config::default();
        config.keys_normal.push(("Q".into(), "quit".into()));
        config.keys_normal.push(("Z".into(), "not_a_command".into()));
        let mut editor = Editor::new(vec![], (80, 24), &config).unwrap();
        assert!(editor.status.contains("not_a_command")); // bad bind reported
        press(&mut editor, "Q");
        assert!(editor.should_quit);
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

    #[test]
    fn newline_after_opener_indents_and_places_the_closer() {
        // Autoclose gives `{}`; Enter between them opens an indented block.
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "{");
        press(&mut editor, "<enter>");
        press(&mut editor, "x");
        assert_eq!(editor.doc().text.to_string(), "{\n    x\n}");
    }

    #[test]
    fn newline_after_opener_without_closer_just_indents() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "( <enter>");
        // Autoclose put `)` after the cursor, so this is the block case…
        assert_eq!(editor.doc().text.to_string(), "(\n    \n)");
        // …and a tabbed file indents with a tab.
        let mut editor = editor_with("\tif x {");
        press(&mut editor, "A");
        press(&mut editor, "<enter>");
        press(&mut editor, "y");
        assert_eq!(editor.doc().text.to_string(), "\tif x {\n\t\ty");
    }

    #[test]
    fn closer_on_a_blank_line_dedents() {
        let mut editor = editor_with("{\n        ");
        press(&mut editor, "j $ a");
        press(&mut editor, "}");
        assert_eq!(editor.doc().text.to_string(), "{\n    }");
    }

    #[test]
    fn tab_indents_with_the_line_style() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "<tab>");
        assert_eq!(editor.doc().text.to_string(), "    ");
        let mut editor = editor_with("\tx");
        press(&mut editor, "A");
        press(&mut editor, "<tab>");
        assert_eq!(editor.doc().text.to_string(), "\tx\t");
    }
}
