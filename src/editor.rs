//! Editor state and the key dispatch loop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::commands;
use crate::document::Document;
use crate::keymap::{Key, KeyCode, KeyTrie, KeymapResult};
use crate::lsp;

use crate::search;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Search,
    Picker,
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
    /// The user has stepped into the list (Tab/S-Tab/arrows). Until then a
    /// menu that popped up on its own highlights nothing and Enter stays a
    /// newline, so typing is never hijacked; LSP menus start navigated.
    pub navigated: bool,
    /// Signature + docs per label, for the side panel (LSP menus only).
    pub docs: std::collections::HashMap<String, String>,
}

/// The live markdown preview: the window showing it and the rows currently
/// drawn there. Rendered from whichever buffer is focused, so the preview
/// follows you across buffers the way a browser tab follows a save.
pub struct Preview {
    /// The window id the preview owns. Never focused — it is a view, not a
    /// place to edit.
    pub win: usize,
    pub rows: Vec<crate::markdown::Row>,
    /// First row on screen, kept in step with the source's viewport.
    pub scroll: usize,
    /// What the cached rows were rendered from.
    doc: usize,
    revision: u64,
    width: usize,
    theme: &'static str,
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
    pub view_row: usize,
    pub view_col: usize,
}

/// The window tree: leaves are windows, splits divide their rectangle among
/// their children, side by side (`vertical`) or stacked.
pub enum Layout {
    Leaf(Window),
    Split {
        vertical: bool,
        children: Vec<Layout>,
    },
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
            Layout::Split { children, .. } => children.iter_mut().find_map(|c| c.find_mut(id)),
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
            Layout::Split {
                vertical: v,
                children,
            } => {
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
            children.retain(|c| !matches!(c, Layout::Leaf(w) if w.id == id));
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

/// A parsed `:s/pat/repl/flags` ex command (see `parse_substitute`).
pub struct Substitute {
    /// `%s` substitutes in the whole buffer, `s` on the cursor line only.
    pub whole_buffer: bool,
    pub pattern: String,
    pub replacement: String,
    /// `g` flag: every match, not just the first on each line.
    pub global: bool,
    /// `i` flag: case-insensitive matching.
    pub insensitive: bool,
    /// The command looked like `:s` but had no pattern/replacement separator.
    pub malformed: bool,
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
        normal.bind_str("%", "goto_matching_bracket");
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
        normal.bind_str("V", "select_line");
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
        // d/x cut, c copies; doubled (dd, xx, cc) they act on the whole line.
        normal.bind_str("d", "delete_selection");
        normal.bind_str("x", "delete_selection");
        normal.bind_str("c", "copy");
        normal.bind_str("S", "change_selection");
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
        normal.bind_str("<space> g", "grep_text");
        normal.bind_str("<space> r", "recent_files");
        normal.bind_str("<space> e", "tree_toggle");
        normal.bind_str("C-t", "tree_toggle");
        normal.bind_str("C-h", "focus_left");
        normal.bind_str("C-<left>", "focus_left");
        normal.bind_str("C-<bs>", "focus_left"); // terminals that send C-h as ^H
        normal.bind_str("C-l", "focus_right");
        normal.bind_str("C-<right>", "focus_right");
        // j/k deliberately flipped from vim: C-j up, C-k down.
        normal.bind_str("C-j", "focus_up");
        normal.bind_str("C-<down>", "focus_down");
        normal.bind_str("C-k", "focus_down");
        normal.bind_str("C-<up>", "focus_up");
        normal.bind_str("C-w h", "focus_left");
        normal.bind_str("C-w j", "focus_up");
        normal.bind_str("C-w k", "focus_down");
        normal.bind_str("C-w l", "focus_right");
        normal.bind_str("<space> d", "file_explorer");
        normal.bind_str("<space> t", "theme_picker");
        normal.bind_str("<space> m", "markdown_preview");
        normal.bind_str("gc", "toggle_comment");
        normal.bind_str("ms", "surround");
        normal.bind_str("<space> s v", "split_vertical");
        normal.bind_str("<space> s h", "split_horizontal");
        normal.bind_str("<space> w", "save");
        normal.bind_str("<space> q", "quit");
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

/// The defaults with crow.toml's `[keys.*]` layered on top, plus the names of
/// any bindings pointing at a command that doesn't exist.
///
/// Always a full rebuild from `Keymaps::default()`, never a patch of the live
/// maps — that's what makes deleting a line from `[keys.normal]` and
/// reloading restore the default binding instead of leaving your override in
/// place.
fn keymaps_from(config: &crate::config::Config) -> (Keymaps, Vec<String>) {
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
    (keymaps, bad_binds)
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
    /// `ms` has been pressed; the next key is the pair to surround with.
    pub awaiting_surround: bool,
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
    /// One running language server per distinct command; documents are
    /// synced only to their own language's server.
    lsps: Vec<lsp::Client>,
    /// From crow.toml: (file extension, server command).
    lsp_table: Vec<(String, String)>,
    /// Commands that failed to spawn or died, so we don't retry every tick.
    lsp_failed: std::collections::HashSet<String>,
    /// Latest diagnostics per file (canonical paths, as the server sends them).
    pub diagnostics: HashMap<PathBuf, Vec<lsp::Diagnostic>>,
    /// The markdown preview split, when open.
    pub preview: Option<Preview>,
    /// The active popup picker, if any (mode == Picker).
    pub picker: Option<crate::picker::Picker>,
    /// The active completion menu, if any (mode == Insert).
    pub completion: Option<Completion>,
    /// Scroll offset of the `:help` window; None when closed.
    pub help_scroll: Option<usize>,
    /// Highlighted row of the `:` suggestion dropdown; None until Tab/arrows.
    pub command_suggest: Option<usize>,
    /// Started with no files: the empty buffer shows the splash screen.
    pub splash: bool,
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
    /// A completion request owed to the server on the next tick, once the
    /// edit that prompted it has been synced. The tag says which kind:
    /// `completion` for a trigger character the user typed, `completion_typed`
    /// for the ambient one that follows an identifier being typed.
    lsp_completion_pending: Option<&'static str>,
    /// When the last key landed. `didChange` ships the whole buffer, so it
    /// waits for a pause in typing rather than firing on every keystroke.
    last_key_at: std::time::Instant,
    /// A bare `d`, `x` or `c` was pressed (with its count): pressing the same
    /// key again runs that key's line op.
    pub pending_line_op: Option<(char, usize)>,
    /// The hover docs popup: its lines and scroll offset (K to open).
    pub hover: Option<(Vec<String>, usize)>,
    /// A "not installed — run `…`? (y/N)" offer; the next keypress answers it.
    pub pending_install: Option<(String, String)>,
    /// A background install in flight: (program, its result channel).
    install: Option<(String, std::sync::mpsc::Receiver<Result<(), String>>)>,
    /// Manifest badges: (ecosystem, dep name) -> (current version, latest).
    pub dep_info: HashMap<(crate::deps::Kind, String), (Option<String>, Option<String>)>,
    /// All in-flight registry fetches stream over this one channel.
    deps_rx: Option<std::sync::mpsc::Receiver<crate::deps::Info>>,
    deps_tx: Option<std::sync::mpsc::Sender<crate::deps::Info>>,
    /// Manifests already fetched this session.
    deps_fetched: std::collections::HashSet<PathBuf>,
}

impl Editor {
    pub fn new(
        paths: Vec<PathBuf>,
        size: (u16, u16),
        config: &crate::config::Config,
    ) -> std::io::Result<Self> {
        let splash = paths.is_empty();
        let mut documents = Vec::new();
        for path in paths {
            crate::config::record_recent(&path);
            documents.push(Document::open(path)?);
        }
        if documents.is_empty() {
            documents.push(Document::empty());
        }

        let (keymaps, bad_binds) = keymaps_from(config);
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
                view_row: 0,
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
            awaiting_surround: false,
            register_fresh: true,
            last_search: String::new(),
            search_select: false,
            search_origin: (0, 0),
            keep_selection: false,
            extend: false,
            lsps: Vec::new(),
            lsp_failed: std::collections::HashSet::new(),
            diagnostics: HashMap::new(),
            preview: None,
            picker: None,
            completion: None,
            help_scroll: None,
            command_suggest: None,
            splash,
            tree: None,
            tree_focused: false,
            tree_leader: false,
            tree_input: None,
            tree_clipboard: None,
            should_quit: false,
            size,
            keymaps,
            lsp_table: config.lsp.clone(),
            lsp_completion_pending: None,
            // Backdated, so the buffers opened on the command line are
            // coloured for the very first frame rather than one gap later.
            last_key_at: std::time::Instant::now()
                .checked_sub(std::time::Duration::from_secs(1))
                .unwrap_or_else(std::time::Instant::now),
            pending_line_op: None,
            hover: None,
            pending_install: None,
            install: None,
            dep_info: HashMap::new(),
            deps_rx: None,
            deps_tx: None,
            deps_fetched: std::collections::HashSet::new(),
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
    #[allow(clippy::type_complexity)]
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

    /// The splash is pure decoration over the untouched startup buffer; the
    /// moment anything real happens (typing, files, splits) it's gone.
    pub fn show_splash(&self) -> bool {
        self.splash
            && self.mode == Mode::Normal
            && self.window_count() == 1
            && self.doc().path.is_none()
            && self.doc().text.len_chars() == 0
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
        let snap = (
            doc.cursor,
            doc.anchor,
            doc.extra.clone(),
            doc.view_line,
            doc.view_row,
            doc.view_col,
        );
        if let Some(w) = self.layout.find_mut(self.focused) {
            w.doc = current;
            (
                w.cursor,
                w.anchor,
                w.extra,
                w.view_line,
                w.view_row,
                w.view_col,
            ) = snap;
        }
    }

    /// Load the newly focused window's stashed state into its document.
    fn restore_focus_state(&mut self) {
        let Some(w) = self.layout.find(self.focused) else {
            return;
        };
        let (doc_idx, c, a, extra, vl, vr, vc) = (
            w.doc,
            w.cursor,
            w.anchor,
            w.extra.clone(),
            w.view_line,
            w.view_row,
            w.view_col,
        );
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
        doc.view_row = vr;
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
        // Closing the last text window would leave you alone in the preview,
        // which has no cursor. Take the preview down instead.
        if self.window_count() == 2 && self.preview.is_some() {
            self.close_preview();
            return;
        }
        if self.window_count() <= 1 {
            return;
        }
        let closing = self.focused;
        self.focus_next_window();
        // focus_next_window saved into `closing` and restored the next one.
        self.layout.close(closing);
    }

    // ---- markdown preview ---------------------------------------------------

    /// The window id the preview owns, if it is open.
    pub fn preview_win(&self) -> Option<usize> {
        self.preview.as_ref().map(|p| p.win)
    }

    /// `:md` — open the preview beside the buffer, or close it.
    pub fn toggle_preview(&mut self) {
        if self.preview.is_some() {
            self.close_preview();
            return;
        }
        let source = self.focused;
        self.split_window(true);
        let win = self.focused;
        // Hand focus straight back: you edit the markdown, you read the render.
        self.save_focus_state();
        self.focused = source;
        self.restore_focus_state();
        self.preview = Some(Preview {
            win,
            rows: Vec::new(),
            scroll: 0,
            doc: usize::MAX,
            revision: u64::MAX,
            width: 0,
            theme: "",
        });
        self.set_status("markdown preview — :md closes it");
    }

    fn close_preview(&mut self) {
        if let Some(p) = self.preview.take() {
            self.layout.close(p.win);
            if self.layout.find(self.focused).is_none() {
                let mut ids = Vec::new();
                self.layout.leaf_ids(&mut ids);
                self.focused = ids.first().copied().unwrap_or(0);
                self.restore_focus_state();
            }
        }
    }

    /// Re-render the preview when the buffer, the window, or the theme moved
    /// under it, and keep its scroll in step with the source's viewport.
    /// Called once per frame; a frame where nothing changed costs one compare.
    pub fn refresh_preview(&mut self) {
        let Some(win) = self.preview_win() else {
            return;
        };
        let Some((_, (.., w, h))) = self.window_rects().0.into_iter().find(|&(id, _)| id == win)
        else {
            return;
        };
        let (doc, revision, view_line) = {
            let d = self.doc();
            (self.current, d.revision, d.view_line)
        };
        // The theme is part of the key: `:theme` recolors the rows too.
        let theme = crate::theme::current().name;
        let p = self.preview.as_mut().expect("checked above");
        if (p.doc, p.revision, p.width, p.theme) != (doc, revision, w as usize, theme) {
            p.rows = crate::markdown::render(&self.documents[doc].text, w as usize);
            (p.doc, p.revision, p.width, p.theme) = (doc, revision, w as usize, theme);
        }
        // Scroll to the first row that came from the top visible source line,
        // so the two panes stay looking at the same part of the document.
        let at = p
            .rows
            .iter()
            .position(|r| r.src_line >= view_line)
            .unwrap_or(0);
        p.scroll = at.min(p.rows.len().saturating_sub(h as usize));
    }

    /// Move focus to the nearest window in one direction (one of dx/dy is
    /// ±1, the other 0). Returns false when there is none that way.
    pub fn focus_window_dir(&mut self, dx: i32, dy: i32) -> bool {
        let (wins, _) = self.window_rects();
        let (fx, fy, fw, fh) = self.focused_rect();
        let (fcx, fcy) = (fx as i32 + fw as i32 / 2, fy as i32 + fh as i32 / 2);
        let target = wins
            .iter()
            .filter(|&&(id, (x, y, ..))| {
                id != self.focused
                    && Some(id) != self.preview_win()
                    && match (dx, dy) {
                        (-1, _) => x < fx,
                        (1, _) => x > fx,
                        (_, -1) => y < fy,
                        _ => y > fy,
                    }
            })
            .min_by_key(|&&(_, (x, y, w, h))| {
                let cx = x as i32 + w as i32 / 2;
                let cy = y as i32 + h as i32 / 2;
                // Nearest along the axis of travel; ties break by alignment.
                if dx != 0 {
                    ((cx - fcx).abs(), (cy - fcy).abs())
                } else {
                    ((cy - fcy).abs(), (cx - fcx).abs())
                }
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
        ids.retain(|&id| Some(id) != self.preview_win() || id == self.focused);
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

    /// The command a doubled key runs on the current line: `dd`/`xx` cut it,
    /// `cc` copies it.
    fn line_op(c: char) -> Option<&'static str> {
        commands::LINE_OPS
            .iter()
            .find(|(key, _)| *key == c)
            .map(|(_, cmd)| *cmd)
    }

    pub fn handle_key(&mut self, key: Key) {
        self.last_key_at = std::time::Instant::now();
        // An armed install offer eats exactly one key: y runs it, anything
        // else declines and the key is not replayed.
        if let Some((program, cmd)) = self.pending_install.take() {
            self.status.clear();
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                self.start_install(&program, &cmd);
            } else {
                self.set_status(format!(
                    "{program} not installed (:install {program} later)"
                ));
            }
            return;
        }
        self.status.clear();

        // The hover docs popup: j/k scroll, Esc/q/K close; any other key
        // closes it and is handled normally.
        if let Some((lines, scroll)) = self.hover.as_mut() {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    *scroll = (*scroll + 1).min(lines.len().saturating_sub(1));
                    return;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    *scroll = scroll.saturating_sub(1);
                    return;
                }
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('K') => {
                    self.hover = None;
                    return;
                }
                _ => self.hover = None,
            }
        }

        // A bare `d`, `x` or `c` is waiting: pressing it again acts on the
        // whole line; any other key cancels and is handled normally.
        if let Some((armed, count)) = self.pending_line_op.take() {
            if self.mode == Mode::Normal
                && !key.ctrl
                && !key.alt
                && key.code == KeyCode::Char(armed)
            {
                self.count = Some(count);
                self.register_fresh = true;
                (commands::find(Self::line_op(armed).unwrap()).unwrap().func)(self);
                let doc = self.doc_mut();
                doc.anchor = doc.cursor;
                doc.commit_undo_group();
                self.count = None;
                self.active_register = None; // the `"a` prefix was for this op
                return;
            }
        }

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

        // `ms` armed the surround: this key is the pair to wrap with.
        if self.awaiting_surround {
            self.awaiting_surround = false;
            if let KeyCode::Char(c) = key.code {
                if !key.ctrl && !key.alt {
                    commands::surround_with(self, c);
                }
            }
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

        // A bare d/x/c with nothing selected arms the doubled line op instead
        // of eating a character, so dd/xx/cc are two keystrokes with no
        // selection step. With extra cursors it falls through and each cursor
        // acts on its own char.
        // ponytail: keyed by character, so rebinding d/x/c in config leaves
        // the doubles where they are. Move them here too if that ever bites.
        if self.mode == Mode::Normal
            && self.pending.is_empty()
            && !key.ctrl
            && !key.alt
            && self.doc().anchor == self.doc().cursor
            && self.doc().extra.is_empty()
        {
            if let KeyCode::Char(c) = key.code {
                if Self::line_op(c).is_some() {
                    self.pending_line_op = Some((c, self.take_count()));
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
                if !self.doc().extra.is_empty() && commands::PER_CURSOR.contains(&command.name) {
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
        // Minus the box: top border, hint row, separator, bottom border.
        let visible = (h as usize).saturating_sub(4).max(1);
        let max = crate::commands::help_lines(&self.keymaps.normal)
            .len()
            .saturating_sub(visible);
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
                // Exactly what's in the bar — a highlighted suggestion has
                // to be Tab-accepted first, so Enter never runs something
                // you didn't put there.
                self.command_suggest = None;
                let line = std::mem::take(&mut self.command_line);
                self.mode = Mode::Normal;
                self.execute_command(&line);
            }
            // First Tab highlights the top suggestion, a second Tab puts it
            // in the bar (trailing space, ready for an argument); Up/Down
            // pick a different one in between.
            KeyCode::Tab => match self.command_suggest {
                Some(i) => {
                    if let Some(pick) = self.command_suggestions().into_iter().nth(i) {
                        self.command_line = format!("{pick} ");
                    }
                    self.command_suggest = None;
                }
                None => {
                    if !self.command_suggestions().is_empty() {
                        self.command_suggest = Some(0);
                    }
                }
            },
            KeyCode::Down => {
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

    /// The ex-commands `execute_command` handles itself, as opposed to the ones
    /// it forwards to the `commands` registry. Only the primary name of each
    /// arm lives here — the aliases (`write`, `edit`, `format`, …) exist for
    /// muscle memory and would only pad the suggestion list. Hand-typed, and
    /// kept honest by `builtins_match_the_ex_command_dispatch` below.
    const BUILTINS: &'static [&'static str] = &[
        "w",
        "q",
        "q!",
        "wq",
        "e",
        "help",
        "md",
        "wrap",
        "fmt",
        "bn",
        "bp",
        "theme",
        "install",
        "lsp-install",
        "config",
        "config!",
    ];

    /// Fuzzy matches for the command word being typed at the `:` prompt.
    /// Empty once an argument starts — only the command itself completes.
    pub fn command_suggestions(&self) -> Vec<String> {
        let line = &self.command_line;
        if line.is_empty() || line.contains(' ') || line.chars().all(|c| c.is_ascii_digit()) {
            return Vec::new();
        }
        let mut scored: Vec<(i64, String)> = Self::BUILTINS
            .iter()
            .map(|s| s.to_string())
            .chain(crate::commands::COMMANDS.iter().map(|c| c.name.to_string()))
            .filter_map(|name| crate::picker::fuzzy_score(line, &name).map(|score| (score, name)))
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

        // :s and :%s are parsed whole — their arguments are delimited, not
        // whitespace-separated, and the pattern may well contain spaces.
        if let Some(sub) = Self::parse_substitute(line) {
            self.substitute(sub);
            return;
        }

        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next();

        match cmd {
            // The bang on a write means "the file moved under me and I still
            // want my version", the same override `q!` is for unsaved changes.
            "w" | "write" | "w!" | "write!" => {
                let force = cmd.ends_with('!');
                match arg {
                    Some(path) => match self.doc_mut().save_as(path, force) {
                        Ok(()) => self.set_status(format!("\"{path}\" written")),
                        Err(e) => self.set_status(format!("Error: {e}")),
                    },
                    None => commands::save_with(self, force),
                }
            }
            "q" | "quit" => (commands::find("quit").unwrap().func)(self),
            "q!" | "quit!" => self.should_quit = true,
            "wq" | "x" | "wq!" | "x!" => {
                commands::save_with(self, cmd.ends_with('!'));
                // A refused write leaves `modified` set, so a stale-file :wq
                // keeps you in the editor instead of dropping your edits.
                if !self.doc().modified {
                    self.should_quit = true;
                }
            }
            "e" | "edit" => match arg {
                Some(path) => match Document::open(path) {
                    Ok(doc) => {
                        crate::config::record_recent(Path::new(path));
                        self.documents.push(doc);
                        self.current = self.documents.len() - 1;
                        self.set_status(format!("\"{path}\""));
                    }
                    Err(e) => self.set_status(format!("Error: {e}")),
                },
                None => self.set_status("Usage: :e <file>"),
            },
            "help" | "h" => self.help_scroll = Some(0),
            "md" | "preview" => self.toggle_preview(),
            "wrap" => (commands::find("toggle_wrap").unwrap().func)(self),
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
            "install" => match arg {
                Some(name) => self.install_named(name, false),
                None => self.set_status(
                    "Usage: :install <tool or extension>  e.g. :install prettier, :install yaml",
                ),
            },
            "lsp-install" => match arg {
                Some(name) => self.install_named(name, true),
                None => self.set_status("Usage: :lsp-install <extension>  e.g. :lsp-install rs"),
            },
            "config" => match Document::open(crate::config::path()) {
                Ok(doc) => {
                    self.documents.push(doc);
                    self.current = self.documents.len() - 1;
                    self.set_status("editing crow.toml — :config! to reload it");
                }
                Err(e) => self.set_status(format!("Error: {e}")),
            },
            "config!" => self.reload_config(),
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

    /// `:s/pat/repl/flags` parsed into its pieces. Returns `None` when the
    /// line isn't a substitute command at all.
    ///
    /// The `%` prefix widens the scope from the cursor line to the whole
    /// buffer. The delimiter is whatever non-alphanumeric character follows
    /// the `s` (`/`, `#`, …) and can be used literally inside the pattern or
    /// replacement by escaping it (`\/`); a bare `\` elsewhere is left alone
    /// so regex classes like `\d` survive. Flags: `g` replaces every match
    /// (default: the first on each line), `i` ignores case.
    fn parse_substitute(line: &str) -> Option<Substitute> {
        let (whole_buffer, rest) = match line.strip_prefix('%') {
            Some(rest) => (true, rest),
            None => (false, line),
        };
        let rest = rest.strip_prefix('s')?;
        let delim = rest.chars().next()?;
        if delim.is_alphanumeric() || delim.is_whitespace() {
            // ":search"-like words are not :s.
            return None;
        }
        let mut fields: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut chars = rest[delim.len_utf8()..].chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' && chars.peek() == Some(&delim) {
                chars.next();
                cur.push(delim);
            } else if c == delim {
                fields.push(std::mem::take(&mut cur));
            } else {
                cur.push(c);
            }
        }
        // A trailing delimiter is optional: fields is [pat], [pat, repl] or
        // [pat, repl, flags].
        fields.push(cur);
        if fields.len() < 2 {
            return Some(Substitute {
                whole_buffer,
                pattern: String::new(),
                replacement: String::new(),
                global: false,
                insensitive: false,
                malformed: true,
            });
        }
        let flags = fields.get(2).cloned().unwrap_or_default();
        Some(Substitute {
            whole_buffer,
            pattern: fields[0].clone(),
            replacement: fields[1].clone(),
            global: flags.contains('g'),
            insensitive: flags.contains('i'),
            malformed: false,
        })
    }

    /// Run a parsed `:s`/`:%s`: one transaction over every match, so the
    /// whole substitute is a single undo step.
    fn substitute(&mut self, sub: Substitute) {
        if sub.malformed {
            self.set_status("Usage: :s/pat/repl/[g]  (%s for the whole buffer)");
            return;
        }
        // An empty pattern repeats the last search, as in vim.
        let pat = if sub.pattern.is_empty() {
            self.last_search.clone()
        } else {
            sub.pattern
        };
        if pat.is_empty() {
            self.set_status("no previous search pattern");
            return;
        }
        let doc = self.doc_mut();
        let scope = if sub.whole_buffer {
            0..doc.text.len_chars()
        } else {
            let line = doc.cursor_line();
            doc.line_start(line)..doc.line_end(line)
        };
        let changes = search::substitutions(
            &doc.text,
            scope,
            &pat,
            &sub.replacement,
            sub.global,
            sub.insensitive,
        );
        if changes.is_empty() {
            self.set_status(format!("pattern not found: {pat}"));
            return;
        }
        let n = changes.len();
        let tx = crate::transaction::Transaction::change(
            &doc.text,
            changes.into_iter().map(|(f, t, r)| (f, t, Some(r))),
        );
        let cursor = tx.map_pos(doc.cursor, false);
        doc.apply(tx, cursor);
        doc.clamp_cursor(false);
        doc.commit_undo_group();
        self.last_search = pat;
        self.set_status(format!(
            "{n} substitution{}",
            if n == 1 { "" } else { "s" }
        ));
    }

    /// `:config!` — re-read crow.toml and install as much of it as can be
    /// installed into a running editor.
    ///
    /// Language servers are the one thing a reload can't do politely. A
    /// client is matched to a buffer by its command string, so an `[lsp]`
    /// entry you edited or deleted would otherwise leave its old server
    /// running and writing diagnostics for the rest of the session. When the
    /// table actually changed we kill them all and let `lsp_sync` respawn
    /// what's still wanted on the next tick — which means a rust-analyzer
    /// reindex, so the status line says so instead of pretending the reload
    /// was free. Leaving `[lsp]` alone costs nothing.
    ///
    /// ponytail: shutdown-and-respawn is the blunt version; teach lsp::Client
    /// to compare command lines and restart only the entries that moved if
    /// the reindex ever becomes annoying.
    fn reload_config(&mut self) {
        let config = crate::config::load();
        let theme_ok = crate::config::apply(&config);
        let (keymaps, bad_binds) = keymaps_from(&config);
        self.keymaps = keymaps;
        // A command whose typo you just fixed deserves another spawn attempt.
        self.lsp_failed.clear();
        let lsp_changed = self.lsp_table != config.lsp;
        let lsps_restarted = lsp_changed && !self.lsps.is_empty();
        if lsp_changed {
            self.shutdown_lsps();
            self.diagnostics.clear();
            self.lsp_table = config.lsp;
        }

        let mut status = String::from("crow.toml reloaded");
        if !theme_ok {
            status.push_str(&format!("; unknown theme {:?}", config.theme));
        }
        if !bad_binds.is_empty() {
            status.push_str(&format!("; unknown command(s): {}", bad_binds.join(", ")));
        }
        if lsps_restarted {
            status.push_str("; language servers restarting");
        }
        self.set_status(status);
    }

    // ---- tool installs -----------------------------------------------------

    /// `:install x` — x is a tool name, or a file extension whose formatter
    /// (or, with `lsp_only`, language server) gets resolved and installed.
    fn install_named(&mut self, name: &str, lsp_only: bool) {
        if self.install.is_some() {
            self.set_status("an install is already running");
            return;
        }
        let lsp_program = |table: &[(String, String)]| {
            table
                .iter()
                .find(|(e, _)| e == name)
                .map(|(_, c)| c.as_str())
                .or_else(|| crate::config::builtin_lsp(name))
                .and_then(|c| c.split_whitespace().next().map(str::to_string))
        };
        let program = if crate::config::installer(name).is_some() {
            Some(name.to_string())
        } else if lsp_only {
            lsp_program(&self.lsp_table)
        } else {
            crate::config::formatter(name)
                .and_then(|c| c.split_whitespace().next().map(str::to_string))
                .or_else(|| lsp_program(&self.lsp_table))
        };
        match program {
            Some(p) => match crate::config::installer(&p) {
                Some(cmd) => self.start_install(&p, cmd),
                None => self.set_status(format!("don't know how to install {p}")),
            },
            None => self.set_status(format!(
                "nothing known for {name:?} — use a tool name or a file extension"
            )),
        }
    }

    /// Offer to install a missing `program` if we know how: arms the (y/N)
    /// prompt and puts it in the status line. False when we can't help.
    pub fn offer_install(&mut self, program: &str) -> bool {
        let Some(cmd) = crate::config::installer(program) else {
            return false;
        };
        if self.install.is_some() {
            return false;
        }
        self.set_status(format!("{program} not installed — run `{cmd}`? (y/N)"));
        self.pending_install = Some((program.to_string(), cmd.to_string()));
        true
    }

    /// Run `cmd` in a background thread; `install_tick` picks up the result.
    fn start_install(&mut self, program: &str, cmd: &str) {
        let (tx, rx) = std::sync::mpsc::channel();
        let shell_cmd = cmd.to_string();
        std::thread::spawn(move || {
            let result = match std::process::Command::new("sh")
                .args(["-c", &shell_cmd])
                .output()
            {
                Ok(o) if o.status.success() => Ok(()),
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    Err(err
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("failed")
                        .to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(result);
        });
        self.set_status(format!("installing {program}… (`{cmd}`)"));
        self.install = Some((program.to_string(), rx));
    }

    /// Poll the background install from the main loop. True when the status
    /// changed and a redraw is due.
    pub fn install_tick(&mut self) -> bool {
        let Some((_, rx)) = self.install.as_ref() else {
            return false;
        };
        let result = match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(_) => Err("install process vanished".to_string()),
            Ok(r) => r,
        };
        let (program, _) = self.install.take().unwrap();
        match result {
            Ok(()) => {
                // A server that failed to spawn earlier can be retried now.
                self.lsp_failed.clear();
                self.set_status(format!("{program} installed"));
                // If it formats the current buffer, finish what :fmt started.
                let formats_this = self
                    .doc()
                    .path
                    .as_ref()
                    .and_then(|p| p.extension())
                    .and_then(|e| e.to_str())
                    .and_then(crate::config::formatter)
                    .is_some_and(|c| c.split_whitespace().next() == Some(program.as_str()));
                if formats_this {
                    (commands::find("format_buffer").unwrap().func)(self);
                }
            }
            Err(e) => self.set_status(format!("{program}: install failed — {e}")),
        }
        true
    }

    // ---- dependency versions -------------------------------------------------

    /// Poll the registry fetches from the main loop, starting one for any
    /// open package manifest not yet fetched. True on new badges.
    pub fn deps_tick(&mut self) -> bool {
        let pending: Vec<(crate::deps::Kind, PathBuf, String)> = self
            .documents
            .iter()
            .filter_map(|d| {
                let path = d.path.as_ref()?;
                let kind = crate::deps::manifest_kind(path.file_name()?.to_str()?)?;
                (!self.deps_fetched.contains(path))
                    .then(|| (kind, path.clone(), d.text.to_string()))
            })
            .collect();
        for (kind, path, text) in pending {
            self.deps_fetched.insert(path.clone());
            let tx = match &self.deps_tx {
                Some(tx) => tx.clone(),
                None => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.deps_rx = Some(rx);
                    self.deps_tx = Some(tx.clone());
                    tx
                }
            };
            crate::deps::fetch(kind, path, text, tx);
        }
        let Some(rx) = self.deps_rx.as_ref() else {
            return false;
        };
        let mut changed = false;
        while let Ok((kind, name, current, latest)) = rx.try_recv() {
            self.dep_info.insert((kind, name), (current, latest));
            changed = true;
        }
        changed
    }

    // ---- paste -------------------------------------------------------------

    /// Bracketed paste: the text goes in verbatim — no auto-indent, no
    /// autoclose, no per-key replay. That's the whole point of the bracket.
    pub fn handle_paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        match self.mode {
            Mode::Command | Mode::Search => {
                // A path or a pattern: only the first line makes sense.
                self.command_suggest = None;
                self.command_line
                    .push_str(text.lines().next().unwrap_or(""));
                if self.mode == Mode::Search {
                    self.update_search_preview();
                }
            }
            Mode::Picker => {} // ponytail: paste into pickers when someone misses it
            Mode::Insert | Mode::Normal => {
                if self.tree_focused || self.help_scroll.is_some() {
                    return;
                }
                self.doc_mut().insert_at_cursor(&text);
                if self.mode == Mode::Normal {
                    let doc = self.doc_mut();
                    doc.anchor = doc.cursor;
                    doc.commit_undo_group();
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
                    picker.requery();
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
                picker.requery();
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
            Kind::Grep { root, .. } => {
                if let Some((path, line)) = label.rsplit_once(':') {
                    let line = line.parse::<usize>().unwrap_or(1).saturating_sub(1);
                    self.jump_to(root.join(path), line, 0);
                }
            }
            Kind::Recent => {
                let path = match label.strip_prefix("~/") {
                    Some(rest) => {
                        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest)
                    }
                    None => PathBuf::from(label),
                };
                self.jump_to(path, 0, 0);
            }
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
            KeyCode::Char('t') if key.ctrl => self.tree_toggle(),
            KeyCode::Esc => self.tree_focused = false,
            KeyCode::Char('q') => {
                self.tree = None;
                self.tree_focused = false;
            }
            KeyCode::Up | KeyCode::Char('k') => tree.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => tree.move_selection(1),
            KeyCode::Char('h') | KeyCode::Left => tree.collapse_or_parent(),
            KeyCode::Char('R') => tree.rebuild(),
            KeyCode::Char('.') => {
                (crate::commands::find("toggle_hidden").unwrap().func)(self);
            }
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
                    Some(row) => row.path.parent().unwrap_or(&tree.root).to_path_buf(),
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
        let consumed = self.completion_key_inner(key);
        if consumed {
            self.maybe_resolve_completion();
        }
        consumed
    }

    fn completion_key_inner(&mut self, key: Key) -> bool {
        let Some(completion) = self.completion.as_mut() else {
            return false;
        };
        let next = |c: &mut Completion| {
            c.selected = (c.selected + 1) % c.items.len();
        };
        let prev = |c: &mut Completion| {
            c.selected = (c.selected + c.items.len() - 1) % c.items.len();
        };
        match key.code {
            KeyCode::Enter if !completion.navigated => {
                self.completion = None;
                false // the newline happens normally
            }
            KeyCode::Enter => {
                self.completion_accept();
                true
            }
            KeyCode::Esc => {
                self.completion = None;
                // Not consumed: one Esc both closes the menu and leaves
                // insert mode via the keymap, not two.
                false
            }
            // Tab steps into the list (first press selects the top item),
            // then Tab/S-Tab cycle; Enter accepts.
            KeyCode::Tab => {
                if completion.navigated {
                    next(completion);
                } else {
                    completion.navigated = true;
                }
                true
            }
            KeyCode::BackTab => {
                if completion.navigated {
                    prev(completion);
                } else {
                    completion.navigated = true;
                }
                true
            }
            KeyCode::Down => {
                completion.navigated = true;
                next(completion);
                true
            }
            KeyCode::Up => {
                completion.navigated = true;
                prev(completion);
                true
            }
            KeyCode::Char('n') if key.ctrl => {
                completion.navigated = true;
                next(completion);
                true
            }
            KeyCode::Char('p') if key.ctrl => {
                completion.navigated = true;
                prev(completion);
                true
            }
            KeyCode::Char('.' | ':') if !key.ctrl && !key.alt => {
                // Member access ends this menu; `insert_typed` sees the key
                // and asks the language server for the members.
                self.completion = None;
                false
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
                completion
                    .items
                    .retain(|(label, _)| label.to_lowercase().starts_with(&prefix));
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

    /// The highlighted completion has no docs yet: ask the server to
    /// resolve them, once. rust-analyzer and friends defer documentation to
    /// `completionItem/resolve` so the initial list stays fast.
    fn maybe_resolve_completion(&mut self) {
        let Some(c) = self.completion.as_ref() else {
            return;
        };
        if !c.navigated {
            return;
        }
        let Some((label, _)) = c.items.get(c.selected) else {
            return;
        };
        if c.docs.contains_key(label) {
            return;
        }
        let label = label.clone();
        let Some(lsp) = self.current_client() else {
            return;
        };
        lsp.resolve_completion(&label);
        // A placeholder, so cycling back over the item doesn't re-request;
        // the resolve response overwrites it.
        if let Some(c) = self.completion.as_mut() {
            c.docs.insert(label, String::new());
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
        if text
            .to_lowercase()
            .starts_with(&completion.prefix.to_lowercase())
        {
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

    /// The configured server command for the current buffer, if any.
    fn current_server_command(&self) -> Option<String> {
        let path = self.doc().path.as_deref()?;
        server_for(&self.lsp_table, path).map(str::to_string)
    }

    /// True when a language server is up for the current buffer (status line).
    pub fn lsp_active(&self) -> bool {
        self.current_server_command()
            .is_some_and(|cmd| self.lsps.iter().any(|c| c.command() == cmd))
    }

    /// The running client that serves the current buffer's language.
    pub fn current_client(&mut self) -> Option<&mut lsp::Client> {
        let command = self.current_server_command()?;
        self.lsps.iter_mut().find(|c| c.command() == command)
    }

    /// Shut every language server down (quit path).
    pub fn shutdown_lsps(&mut self) {
        for lsp in self.lsps.drain(..) {
            lsp.shutdown();
        }
    }

    /// Called from the main loop between keystrokes: keep the server in sync
    /// with edited buffers and apply anything it sent back.
    /// Drains language-server messages. Returns true when anything on screen
    /// may have changed, so the main loop knows a redraw is needed.
    pub fn lsp_tick(&mut self) -> bool {
        // `didChange` serialises the entire buffer to JSON and writes it down a
        // pipe. Hold it until typing pauses — unless a completion is owed, and
        // then the server has to see the edit that prompted it first.
        if self.lsp_completion_pending.is_some()
            || self.last_key_at.elapsed() >= std::time::Duration::from_millis(120)
        {
            self.lsp_sync();
        }
        if let Some(tag) = self.lsp_completion_pending.take() {
            if self.mode == Mode::Insert {
                if let Some(path) = self.doc().path.clone() {
                    let (line, col) = self.doc().cursor_line_col();
                    let utf16_col = crate::position::char_to_utf16(self.doc().line(line), col);
                    if let Some(lsp) = self.current_client() {
                        lsp.request_position(
                            tag,
                            "textDocument/completion",
                            &path,
                            line,
                            utf16_col,
                        );
                    }
                }
            }
        }
        let mut events = Vec::new();
        let mut i = 0;
        while i < self.lsps.len() {
            events.extend(self.lsps[i].poll());
            if self.lsps[i].is_dead() {
                // The server exited (crashed, or was a broken shim): stop
                // syncing it and don't respawn every tick.
                let dead = self.lsps.remove(i);
                self.lsp_failed.insert(dead.command().to_string());
            } else {
                i += 1;
            }
        }
        let changed = !events.is_empty();
        for event in events {
            match event {
                lsp::Event::Definition(path, line, col) => self.jump_to(path, line, col),
                lsp::Event::Hover(text) => self.open_hover(&text),
                lsp::Event::Status(text) => self.set_status(text),
                lsp::Event::Diagnostics(path, diags) => {
                    self.diagnostics.insert(path, diags);
                }
                lsp::Event::Completions(items, typed) => self.show_completions(items, typed),
                lsp::Event::CompletionResolved(label, info) => {
                    if let Some(c) = self.completion.as_mut() {
                        if !info.is_empty() {
                            c.docs.insert(label, info);
                        }
                    }
                }
            }
        }
        changed
    }

    /// Open the hover docs popup — signature, description, examples — or
    /// fall back to the status line when the buffer isn't in normal mode.
    pub fn open_hover(&mut self, text: &str) {
        if self.mode != Mode::Normal {
            self.set_status(text.lines().next().unwrap_or("").to_string());
            return;
        }
        self.hover = Some((text.lines().map(str::to_string).collect(), 0));
    }

    /// `typed` marks the list crow asked for on its own while an identifier
    /// was being typed, rather than one the user asked for.
    fn show_completions(&mut self, items: Vec<(String, String, String)>, typed: bool) {
        if self.mode != Mode::Insert {
            return; // the answer arrived after insert mode ended
        }
        let prefix = self.word_prefix();
        let lower = prefix.to_lowercase();
        let mut docs = std::collections::HashMap::new();
        let mut items: Vec<(String, String)> = items
            .into_iter()
            .filter(|(label, _, _)| label.to_lowercase().starts_with(&lower))
            .map(|(label, text, info)| {
                if !info.is_empty() {
                    docs.insert(label.clone(), info);
                }
                (label, text)
            })
            .collect();
        items.truncate(50);
        if items.is_empty() {
            // An unasked-for list that no longer matches what has been typed
            // since must not close the popup that is up, nor say anything.
            if !typed {
                self.set_status("no completions");
                self.completion = None;
            }
            return;
        }
        self.completion = Some(Completion {
            items,
            selected: 0,
            prefix,
            // Asked for (C-space, or a `.`/`<`/`::` trigger): the list is the
            // point, so Enter accepts. Offered while typing: Enter stays
            // Enter and Tab accepts, like the buffer-word popup it replaced.
            navigated: !typed,
            docs,
        });
        // Docs for the item highlighted on open, if the server defers them.
        self.maybe_resolve_completion();
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

        let prev = {
            let doc = self.doc();
            (doc.cursor > 0).then(|| doc.text.char(doc.cursor - 1))
        };
        self.doc_mut().insert_at_cursor(&c.to_string());
        if c.is_alphanumeric() || c == '_' || c == '/' {
            self.maybe_autocomplete();
            // With a server up, ask it as well: buffer words can only offer
            // what the file already says, so `println` is invisible until
            // something types it first. Its answer lands in the same popup a
            // tick later, filtered by whatever the prefix is by then.
            //
            // ponytail: one request per identifier keystroke, held to one in
            // flight by the single slot; a real debounce timer if a server
            // ever starts falling behind.
            if self.lsp_completion_pending.is_none()
                && self.word_prefix().chars().count() >= 2
                && self.current_server_command().is_some()
            {
                self.lsp_completion_pending = Some("completion_typed");
            }
        }
        // Member access: `.` or a second `:` asks the server what's inside.
        // Deferred to `lsp_tick` so the request follows this edit's didChange.
        // A digit before the dot is a float literal, not member access.
        let member_dot = c == '.' && !prev.is_some_and(|p| p.is_ascii_digit());
        // Plus whatever else the server itself calls a trigger — `<` opens
        // Oxigen's type list, and nothing but the server knows that.
        let declared = c != '.'
            && self
                .current_client()
                .is_some_and(|lsp| lsp.triggers_completion(c));
        if (member_dot || declared || (c == ':' && prev == Some(':')))
            && self.current_server_command().is_some()
        {
            self.completion = None;
            self.lsp_completion_pending = Some("completion");
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
                navigated: false,
                docs: std::collections::HashMap::new(),
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
            navigated: false,
            docs: std::collections::HashMap::new(),
        })
    }

    fn lsp_sync(&mut self) {
        // One client per distinct server command among the open files.
        let needed: Vec<String> = self
            .documents
            .iter()
            .filter_map(|d| d.path.as_deref())
            .filter_map(|p| server_for(&self.lsp_table, p))
            .map(str::to_string)
            .collect();
        for command in needed {
            if self.lsps.iter().any(|c| c.command() == command)
                || self.lsp_failed.contains(&command)
            {
                continue;
            }
            let root = std::env::current_dir().unwrap_or_default();
            match lsp::Client::spawn(&root, &command) {
                Some(client) => self.lsps.push(client),
                None => {
                    let program = command.split_whitespace().next().unwrap_or("").to_string();
                    self.lsp_failed.insert(command.clone());
                    if !self.offer_install(&program) {
                        self.set_status(format!("could not start {command:?} — no LSP"));
                    }
                }
            }
        }
        // Sync every document to its own language's server — never another's
        // (taplo getting a .rs file marks it "excluded", and worse).
        for doc in &self.documents {
            let Some(path) = doc.path.as_ref() else {
                continue;
            };
            let Some(command) = server_for(&self.lsp_table, path) else {
                continue;
            };
            let Some(lsp) = self.lsps.iter_mut().find(|c| c.command() == command) else {
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
        crate::config::record_recent(&canon);
        self.current = idx;
        let doc = self.doc_mut();
        let line = line.min(doc.line_count().saturating_sub(1));
        let col = crate::position::utf16_to_char(doc.line(line), utf16_col);
        doc.cursor = doc.line_start(line) + col;
        doc.anchor = doc.cursor;
        doc.clamp_cursor(false);
        doc.goal_col = None;
    }

    /// Whether any buffer is owed a reparse.
    pub fn needs_reparse(&self) -> bool {
        self.documents.iter().any(Document::syntax_stale)
    }

    /// Whether nothing has been typed for `gap`.
    pub fn idle_for(&self, gap: std::time::Duration) -> bool {
        self.last_key_at.elapsed() >= gap
    }

    /// Recolor the stale buffers. A reparse is a whole-file tree-sitter pass —
    /// tens of milliseconds on a big file — so the main loop holds this until
    /// typing pauses, and draws the edits with the old spans slid through them
    /// in the meantime.
    pub fn settle(&mut self) {
        for doc in &mut self.documents {
            doc.settle_syntax();
        }
    }

    // ---- scrolling ---------------------------------------------------------

    /// Adjust the viewport so the cursor is on screen, keeping a few lines of
    /// context above and below where possible.
    /// Soft-wrap width for the focused window's text area, or `None` when
    /// wrapping is off and long lines scroll sideways instead.
    pub fn wrap_width(&self) -> Option<usize> {
        crate::config::soft_wrap()
            .then(|| self.text_width())
            .filter(|w| *w > 0)
    }

    pub fn ensure_cursor_visible(&mut self) {
        let height = self.text_height();
        let width = self.text_width();
        if height == 0 || width == 0 {
            return;
        }
        let scrolloff = crate::config::scrolloff().min(height.saturating_sub(1) / 2);
        let wrap = self.wrap_width();

        let doc = self.doc_mut();
        let (line, row, col) = doc.cursor_visual(wrap);

        let Some(_) = wrap else {
            doc.view_row = 0;
            if line < doc.view_line + scrolloff {
                doc.view_line = line.saturating_sub(scrolloff);
            }
            if line + scrolloff >= doc.view_line + height {
                doc.view_line = (line + scrolloff + 1).saturating_sub(height);
            }
            doc.view_line = doc.view_line.min(doc.line_count().saturating_sub(1));
            if col < doc.view_col {
                doc.view_col = col;
            }
            if col >= doc.view_col + width {
                doc.view_col = col - width + 1;
            }
            return;
        };

        // Wrapping makes "rows" and "lines" different units, so the viewport
        // is a (line, row) pair and scrolling counts rows.
        doc.view_col = 0; // nothing scrolls sideways while it wraps
        doc.view_line = doc.view_line.min(doc.line_count().saturating_sub(1));
        // A jump (`G`, a goto) can leave the viewport a whole file away. Land
        // near the cursor first, so the row walk below stays bounded by the
        // screen instead of the file.
        if line >= doc.view_line + height || line + height < doc.view_line {
            doc.view_line = line.saturating_sub(height / 2);
            doc.view_row = 0;
        }

        let top = (doc.view_line, doc.view_row);
        if (line, row) < top {
            (doc.view_line, doc.view_row) = (line, row);
            doc.scroll_view(wrap, -(scrolloff as isize));
        } else {
            let over = doc.rows_forward(wrap, top, (line, row)) as isize + scrolloff as isize + 1
                - height as isize;
            if over > 0 {
                doc.scroll_view(wrap, over);
            }
        }
    }

    /// Collect highlight spans for the lines about to be drawn, and nothing
    /// else. Every other path only marks them stale, so the cost of coloring
    /// is a screenful per frame rather than a whole file per keystroke.
    pub fn refresh_highlights(&mut self) {
        let (wins, _) = self.window_rects();
        // Two windows on one document share its spans, so take the union of
        // what they need rather than letting them fight over the range.
        let mut ranges: HashMap<usize, (usize, usize)> = HashMap::new();
        for (id, (.., h)) in wins {
            let (doc, first) = if id == self.focused {
                (self.current, self.documents[self.current].view_line)
            } else {
                match self.layout.find(id) {
                    Some(w) => (w.doc.min(self.documents.len() - 1), w.view_line),
                    None => continue,
                }
            };
            let want = (first, first + h as usize);
            ranges
                .entry(doc)
                .and_modify(|r| *r = (r.0.min(want.0), r.1.max(want.1)))
                .or_insert(want);
        }
        for (doc, (first, last)) in ranges {
            self.documents[doc].highlight_range(first, last);
        }
    }

    /// Cursor position on screen as (column, row), or `None` if off-screen.
    pub fn screen_cursor(&self) -> Option<(u16, u16)> {
        if self.help_scroll.is_some() {
            return None; // the help window has no cursor
        }
        if self.show_splash() {
            return None; // nothing to edit yet
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
        let wrap = self.wrap_width();
        let doc = self.doc();
        let (line, row, col) = doc.cursor_visual(wrap);
        if line < doc.view_line || line > doc.view_line + rh as usize {
            return None;
        }
        if (line, row) < (doc.view_line, doc.view_row) || col < doc.view_col {
            return None;
        }

        let screen_row = doc.rows_forward(wrap, (doc.view_line, doc.view_row), (line, row));
        if screen_row >= rh as usize {
            return None;
        }
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
        let display =
            crate::position::char_to_display_col(doc.line(line), col, crate::config::tab_width());
        format!("{}:{}", line + 1, display + 1)
    }
}

/// The server command for a file, by extension: crow.toml's [lsp] entries
/// first, then the built-in table.
fn server_for<'a>(table: &'a [(String, String)], path: &Path) -> Option<&'a str> {
    let ext = path.extension()?.to_str()?;
    table
        .iter()
        .find(|(e, _)| e == ext)
        .map(|(_, cmd)| cmd.as_str())
        .or_else(|| crate::config::builtin_lsp(ext))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::keymap::Key;

    pub(crate) fn editor_with(text: &str) -> Editor {
        let mut editor = Editor::new(vec![], (80, 24), &crate::config::Config::default()).unwrap();
        editor.doc_mut().text = ropey::Rope::from_str(text);
        editor
    }

    pub(crate) fn press(editor: &mut Editor, keys: &str) {
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
    fn paste_goes_in_verbatim_no_autoindent_no_autoclose() {
        // The ci.yml bug: pasted YAML must not pick up cumulative indent,
        // and pasted brackets must not auto-close.
        let mut editor = editor_with("    indented\n");
        press(&mut editor, "i");
        editor.doc_mut().cursor = 13; // after the indented line
        editor.handle_paste("on:\r\n  push:\r\n    branches: [main]\n");
        assert_eq!(
            editor.doc().text.to_string(),
            "    indented\non:\n  push:\n    branches: [main]\n"
        );
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
        press(&mut editor, "v3ld");
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
        press(&mut editor, "v10ld");
        assert_eq!(editor.doc().text.to_string(), "klm");
    }

    #[test]
    fn select_line_then_d_deletes_the_line() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "Vd");
        assert_eq!(editor.doc().text.to_string(), "two\nthree");
    }

    #[test]
    fn repeated_line_select_extends_the_selection() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "VVd");
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
        // w selects "foo ", h collapses onto the space; vl reselects just it.
        press(&mut editor, "wh");
        press(&mut editor, "vld");
        assert_eq!(editor.doc().text.to_string(), "foobar");
    }

    #[test]
    fn change_replaces_the_selection() {
        let mut editor = editor_with("foo bar");
        press(&mut editor, "wS");
        press(&mut editor, "x");
        assert_eq!(editor.doc().text.to_string(), "xbar");
    }

    #[test]
    fn linewise_copy_then_paste_duplicates_the_line() {
        let mut editor = editor_with("one\ntwo");
        press(&mut editor, "Vcp");
        assert_eq!(editor.doc().text.to_string(), "one\none\ntwo");
    }

    #[test]
    fn delete_then_paste_moves_a_line() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "Vdjp");
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
        press(&mut editor, "SX");
        press(&mut editor, "<esc>");
        assert_eq!(editor.doc().text.to_string(), "X bar X baz X");
    }

    #[test]
    fn select_matches_is_scoped_by_the_selection() {
        let mut editor = editor_with("foo\nfoo\nfoo");
        press(&mut editor, "VV"); // select first two lines
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
        // The delete dropped extend mode: l is a plain motion again, so vl
        // selects exactly one char.
        press(&mut editor, "lvld");
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
        // With extend off, l collapses and w selects from there only.
        press(&mut editor, "lwd");
        assert_eq!(editor.doc().text.to_string(), "a");
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
        press(&mut editor, "w"); // select "foo "
        press(&mut editor, "\"ac"); // into register a; cursor back to 0
        press(&mut editor, "vld"); // select "f", delete: unnamed register = "f"
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

    /// The cursor has to land on the *visual* row its character wrapped onto,
    /// not on its line's row — otherwise it drifts further off with every fold.
    #[test]
    fn soft_wrap_puts_the_cursor_on_its_wrapped_row() {
        let mut editor = editor_with(&format!("{}\nnext\n", "x".repeat(200)));
        // 80 columns less a 4-column gutter: rows start at chars 0, 76, 152.
        editor.doc_mut().cursor = 100;
        editor.ensure_cursor_visible();
        assert_eq!(editor.screen_cursor(), Some((4 + 24, 1)));
        editor.doc_mut().cursor = 160;
        editor.ensure_cursor_visible();
        assert_eq!(editor.screen_cursor(), Some((4 + 8, 2)));
    }

    /// Scrolling has to count rows, not lines: with every line folding in two,
    /// a line-counting viewport thinks the cursor is on screen when it is a
    /// screen and a half below it.
    #[test]
    fn scrolling_counts_wrapped_rows_not_lines() {
        let long = "y".repeat(100);
        let text = std::iter::repeat_n(long.as_str(), 30)
            .collect::<Vec<_>>()
            .join("\n");
        let mut editor = editor_with(&text);
        let at = editor.doc().line_start(20);
        editor.doc_mut().cursor = at;
        editor.ensure_cursor_visible();
        assert!(
            editor.doc().view_line > 8,
            "viewport still counting lines: {}",
            editor.doc().view_line
        );
        assert!(
            editor.screen_cursor().is_some(),
            "cursor scrolled off screen"
        );
    }

    /// A line taller than the whole window means the viewport has to be able
    /// to start partway into a line, not just at one.
    #[test]
    fn the_viewport_can_park_inside_one_very_long_line() {
        let mut editor = editor_with(&"z".repeat(4000));
        editor.doc_mut().cursor = 3900;
        editor.ensure_cursor_visible();
        assert!(editor.doc().view_row > 0);
        assert!(editor.screen_cursor().is_some());
        editor.doc_mut().cursor = 0;
        editor.ensure_cursor_visible();
        assert_eq!((editor.doc().view_line, editor.doc().view_row), (0, 0));
    }

    #[test]
    fn md_opens_a_live_preview_beside_the_text_and_closes_again() {
        let mut editor = editor_with("# Title\n\nsome **bold** words\n");
        press(&mut editor, "<space> m");
        assert_eq!(editor.window_count(), 2);
        assert!(editor.preview_win().is_some());
        assert_ne!(
            editor.preview_win(),
            Some(editor.focused),
            "focus belongs to the text, not the render"
        );

        editor.refresh_preview();
        let rendered: String = editor
            .preview
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .flat_map(|r| r.spans.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(rendered.contains("TITLE"), "{rendered:?}");
        assert!(rendered.contains("bold") && !rendered.contains('*'));

        // Editing the buffer re-renders it.
        press(&mut editor, "G");
        press(&mut editor, "o");
        for c in "## Added".chars() {
            editor.handle_key(Key::char(c));
        }
        press(&mut editor, "<esc>");
        editor.refresh_preview();
        let rendered: String = editor
            .preview
            .as_ref()
            .unwrap()
            .rows
            .iter()
            .flat_map(|r| r.spans.iter().map(|s| s.text.as_str()))
            .collect();
        assert!(rendered.contains("Added"), "{rendered:?}");

        press(&mut editor, "<space> m");
        assert_eq!(editor.window_count(), 1);
        assert!(editor.preview.is_none());
    }

    /// Focus must never land in the preview — it has no cursor to put there.
    #[test]
    fn window_cycling_skips_the_preview() {
        let mut editor = editor_with("hi\n");
        press(&mut editor, "<space> m");
        let source = editor.focused;
        editor.focus_next_window();
        assert_eq!(editor.focused, source);
        assert!(!editor.focus_window_dir(1, 0));
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
        editor.show_completions(
            vec![
                ("println!".into(), "println!".into(), String::new()),
                ("print!".into(), "print!".into(), String::new()),
            ],
            false,
        );
        assert_eq!(editor.completion.as_ref().unwrap().items.len(), 2);
        press(&mut editor, "<enter>");
        assert_eq!(editor.doc().text.to_string(), "println!");
        assert!(editor.completion.is_none());
    }

    #[test]
    fn esc_closes_completion_and_leaves_insert_mode_in_one_press() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "pri");
        editor.show_completions(
            vec![("println!".into(), "println!".into(), String::new())],
            false,
        );
        press(&mut editor, "<esc>");
        assert!(editor.completion.is_none());
        assert_eq!(editor.mode, Mode::Normal);
    }

    #[test]
    fn h_wraps_onto_the_previous_line() {
        let mut editor = editor_with("ab\ncd\nef");
        press(&mut editor, "j");
        press(&mut editor, "0");
        assert_eq!(editor.doc().cursor, 3);
        // One h from the start of "cd" lands on the last char of "ab".
        press(&mut editor, "h");
        assert_eq!(editor.doc().cursor, 1);
        // Repeated h keeps wrapping through an empty line to the top.
        let mut editor = editor_with("ab\n\ncd");
        editor.doc_mut().cursor = 4;
        press(&mut editor, "h");
        assert_eq!(editor.doc().cursor, 3); // the empty line
        press(&mut editor, "h");
        assert_eq!(editor.doc().cursor, 1);
        press(&mut editor, "h");
        assert_eq!(editor.doc().cursor, 0);
        // At the very start there is nowhere left to go.
        press(&mut editor, "h");
        assert_eq!(editor.doc().cursor, 0);
    }

    #[test]
    fn l_wraps_onto_the_next_line() {
        let mut editor = editor_with("ab\ncd\nef");
        // From the last char of "ab", l lands on the first char of "cd".
        editor.doc_mut().cursor = 1;
        press(&mut editor, "l");
        assert_eq!(editor.doc().cursor, 3);
        // Repeated l keeps wrapping through an empty line.
        let mut editor = editor_with("ab\n\ncd");
        editor.doc_mut().cursor = 1;
        press(&mut editor, "l");
        assert_eq!(editor.doc().cursor, 3); // the empty line
        press(&mut editor, "l");
        assert_eq!(editor.doc().cursor, 4);
        // On the last char of the last line there is nowhere right to go.
        press(&mut editor, "l");
        assert_eq!(editor.doc().cursor, 5);
        press(&mut editor, "l");
        assert_eq!(editor.doc().cursor, 5);
    }

    #[test]
    fn percent_s_substitutes_in_the_whole_buffer_as_one_undo_step() {
        let mut editor = editor_with("foo bar\nfoo baz\nfoo");
        press(&mut editor, ":%s/foo/quux/g <enter>");
        assert_eq!(editor.doc().text.to_string(), "quux bar\nquux baz\nquux");
        assert_eq!(editor.status, "3 substitutions");
        press(&mut editor, "u");
        assert_eq!(editor.doc().text.to_string(), "foo bar\nfoo baz\nfoo");
    }

    #[test]
    fn s_without_percent_or_g_replaces_the_first_match_on_the_cursor_line() {
        let mut editor = editor_with("foo foo\nfoo foo");
        editor.doc_mut().cursor = 8; // line 1
        press(&mut editor, ":s/foo/bar <enter>");
        assert_eq!(editor.doc().text.to_string(), "foo foo\nbar foo");
        // With g, every match on the line goes.
        let mut editor = editor_with("foo foo");
        press(&mut editor, ":s/foo/bar/g <enter>");
        assert_eq!(editor.doc().text.to_string(), "bar bar");
    }

    #[test]
    fn s_supports_capture_groups_and_reports_missing_patterns() {
        let mut editor = editor_with("ab");
        press(&mut editor, ":%s/(a)(b)/\\2\\1/ <enter>");
        assert_eq!(editor.doc().text.to_string(), "ba");
        let mut editor = editor_with("hello");
        press(&mut editor, ":%s/zzz/x/g <enter>");
        assert_eq!(editor.doc().text.to_string(), "hello");
        assert_eq!(editor.status, "pattern not found: zzz");
    }

    #[test]
    fn search_like_words_are_not_substitute_commands() {
        // ":s" must be followed by a delimiter; ":set" is not :s.
        assert!(Editor::parse_substitute("set number").is_none());
        assert!(Editor::parse_substitute("search").is_none());
        assert!(Editor::parse_substitute("s").is_none());
        let sub = Editor::parse_substitute("%s/a/b/g").unwrap();
        assert!(sub.whole_buffer && sub.global && !sub.insensitive);
        assert_eq!(sub.pattern, "a");
        assert_eq!(sub.replacement, "b");
        // A different delimiter works, and an escaped delimiter is literal.
        let sub = Editor::parse_substitute("s#a#b#").unwrap();
        assert!(!sub.whole_buffer && !sub.global);
        let sub = Editor::parse_substitute("s/a\\/b/c/").unwrap();
        assert_eq!(sub.pattern, "a/b");
    }

    #[test]
    fn percent_jumps_between_matching_brackets() {
        let mut editor = editor_with("fn main() {\n    if x {\n    }\n}");
        press(&mut editor, "%"); // on the 'f': not a bracket, stays put
        assert_eq!(editor.doc().cursor, 0);
        editor.doc_mut().cursor = 10; // the '{'
        press(&mut editor, "%");
        assert_eq!(editor.doc().cursor, 29); // the final '}'
        press(&mut editor, "%");
        assert_eq!(editor.doc().cursor, 10);
    }

    #[test]
    fn dd_deletes_lines_into_the_register() {
        let mut editor = editor_with("one\ntwo\nthree\nfour\n");
        press(&mut editor, "dd");
        assert_eq!(editor.doc().text.to_string(), "two\nthree\nfour\n");
        assert_eq!(editor.register, "one\n");
        // The register is linewise, so p pastes the line back below.
        press(&mut editor, "p");
        assert_eq!(editor.doc().text.to_string(), "two\none\nthree\nfour\n");
        // A count deletes that many lines; u undoes the whole delete.
        press(&mut editor, "gg 2dd");
        assert_eq!(editor.doc().text.to_string(), "three\nfour\n");
        press(&mut editor, "u");
        assert_eq!(editor.doc().text.to_string(), "two\none\nthree\nfour\n");
        // d followed by anything else cancels — nothing deleted.
        press(&mut editor, "dj");
        assert_eq!(editor.doc().text.to_string(), "two\none\nthree\nfour\n");
    }

    #[test]
    fn hover_popup_scrolls_and_any_key_falls_through() {
        let mut editor = editor_with("hello\n");
        editor.open_hover("fn foo()\n\nDoes the thing.\n\nExample:\n    foo();");
        assert!(editor.hover.is_some());
        press(&mut editor, "j");
        assert_eq!(editor.hover.as_ref().unwrap().1, 1);
        press(&mut editor, "k");
        assert_eq!(editor.hover.as_ref().unwrap().1, 0);
        press(&mut editor, "<esc>");
        assert!(editor.hover.is_none());
        // Any non-scroll key closes the popup and still does its job.
        editor.open_hover("docs");
        press(&mut editor, "i");
        assert!(editor.hover.is_none());
        assert_eq!(editor.mode, Mode::Insert);
    }

    #[test]
    fn dep_upgrade_rewrites_the_cursor_lines_version() {
        let mut editor = editor_with("[dependencies]\ncrossterm = \"0.27\"\n");
        editor.doc_mut().path = Some("Cargo.toml".into());
        editor.dep_info.insert(
            (crate::deps::Kind::Cargo, "crossterm".into()),
            (Some("0.27.0".into()), Some("0.29.0".into())),
        );
        press(&mut editor, "j"); // onto the crossterm line
        (crate::commands::find("dep_upgrade").unwrap().func)(&mut editor);
        assert_eq!(
            editor.doc().text.to_string(),
            "[dependencies]\ncrossterm = \"0.29.0\"\n"
        );
    }

    #[test]
    fn tab_and_shift_tab_cycle_the_completion_menu() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "p");
        editor.show_completions(
            vec![
                ("print!".into(), "print!".into(), String::new()),
                ("push".into(), "push".into(), "Appends an element.".into()),
            ],
            false,
        );
        // An LSP menu starts navigated: Tab advances, S-Tab goes back.
        press(&mut editor, "<tab>");
        assert_eq!(editor.completion.as_ref().unwrap().selected, 1);
        press(&mut editor, "<backtab>");
        assert_eq!(editor.completion.as_ref().unwrap().selected, 0);
        press(&mut editor, "<tab> <enter>");
        assert_eq!(editor.doc().text.to_string(), "push");
    }

    #[test]
    fn typing_narrows_the_completion_menu() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "p");
        editor.show_completions(
            vec![
                ("print!".into(), "print!".into(), String::new()),
                ("push".into(), "push".into(), String::new()),
            ],
            false,
        );
        press(&mut editor, "u"); // types through the menu
        assert_eq!(editor.doc().text.to_string(), "pu");
        assert_eq!(editor.completion.as_ref().unwrap().items.len(), 1);
        press(&mut editor, "<enter>");
        assert_eq!(editor.doc().text.to_string(), "push");
    }

    /// The list crow asks the server for while you type replaces the
    /// buffer-word popup, so it has to keep that popup's manners.
    #[test]
    fn a_typed_completion_list_keeps_enter_as_enter() {
        let mut editor = editor_with("");
        press(&mut editor, "i");
        press(&mut editor, "pri");
        editor.show_completions(
            vec![("println".into(), "println".into(), String::new())],
            true,
        );
        assert_eq!(editor.completion.as_ref().unwrap().items.len(), 1);
        press(&mut editor, "<enter>");
        assert_eq!(editor.doc().text.to_string(), "pri\n");
        assert!(editor.completion.is_none());

        // And an answer that arrives too late to match what has been typed
        // since leaves the popup that is up alone instead of closing it.
        press(&mut editor, "pri");
        editor.show_completions(
            vec![("println".into(), "println".into(), String::new())],
            true,
        );
        editor.show_completions(vec![("zzz".into(), "zzz".into(), String::new())], true);
        assert!(
            editor.completion.is_some(),
            "a stale typed list closed the popup"
        );
        assert_eq!(editor.status, "");
    }

    #[test]
    fn palette_rows_carry_the_shortest_binding() {
        let editor = editor_with("");
        let keymap = &editor.keymaps.normal;
        assert_eq!(
            keymap.binding_of("find_files").as_deref(),
            Some("<space> f")
        );
        assert_eq!(keymap.binding_of("save").as_deref(), Some("Ctrl-s")); // not <space> w
        assert_eq!(keymap.binding_of("format_buffer"), None); // `:fmt` only
        let picker = crate::picker::Picker::commands(keymap);
        let item = picker
            .items
            .iter()
            .find(|i| i.label == "find_files")
            .unwrap();
        assert!(item.detail.starts_with("<space> f  ·  "));
    }

    #[test]
    fn leader_shows_continuations_and_space_s_v_splits() {
        let mut editor = editor_with("hello");
        press(&mut editor, "<space>");
        let entries = editor.keymaps.normal.continuations(&editor.pending);
        assert!(entries.iter().any(|(k, n)| k == "e" && n == "tree_toggle"));
        assert!(
            entries.iter().any(|(k, n)| k == "s" && n == "…"),
            "s is a group"
        );
        assert!(entries.iter().any(|(k, n)| k == "w" && n == "save"));
        assert!(entries.iter().any(|(k, n)| k == "q" && n == "quit"));
        press(&mut editor, "s v");
        assert_eq!(editor.window_count(), 2);
        assert!(editor.pending.is_empty());
    }

    #[test]
    fn ctrl_h_closes_the_picker_and_moves_focus() {
        let mut editor = editor_with("hello");
        press(&mut editor, "C-t C-l"); // open the sidebar, back to the text
        press(&mut editor, "<space> f");
        assert_eq!(editor.mode, Mode::Picker);
        press(&mut editor, "C-h");
        assert!(editor.picker.is_none());
        assert!(
            editor.tree_focused,
            "C-h out of the picker crosses to the open sidebar"
        );
    }

    /// `BUILTINS` is a second copy of the `match cmd` in `execute_command`, and
    /// the compiler has no opinion about the two agreeing: add an arm and the
    /// command silently stops Tab-completing, delete one and Tab-completion
    /// offers a name that answers "Not a command". Rather than generate the
    /// arms from a macro — which would have to thread `self` and `arg` through
    /// macro hygiene to buy this one assertion — read our own source back and
    /// diff the two lists. Only each arm's first name counts; the aliases after
    /// `|` are deliberately absent from `BUILTINS`.
    // ponytail: text-scrapes the arms, so it only sees `"name" | ... =>` written
    // on one line. If an arm ever needs a real parse, that is the day for one.
    #[test]
    fn builtins_match_the_ex_command_dispatch() {
        let arms = include_str!("editor.rs")
            .split_once("        match cmd {")
            .expect("execute_command's dispatch")
            .1
            .split_once("other => {")
            .expect("the fallback arm ends the literal ones")
            .0;
        let mut dispatched: Vec<&str> = arms
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with('"') && line.contains("=>"))
            .map(|line| line[1..].split('"').next().unwrap())
            .collect();
        let mut suggested: Vec<&str> = Editor::BUILTINS.to_vec();
        dispatched.sort_unstable();
        suggested.sort_unstable();
        assert_eq!(
            dispatched, suggested,
            "BUILTINS has drifted from the `match cmd` arms in execute_command"
        );
    }

    #[test]
    fn command_bar_tab_completes_into_the_bar_and_enter_submits() {
        let mut editor = editor_with("hello");
        press(&mut editor, ":");
        press(&mut editor, "qui");
        let suggestions = editor.command_suggestions();
        assert_eq!(suggestions.first().map(String::as_str), Some("quit"));
        assert_eq!(
            editor.command_suggest, None,
            "nothing highlighted until Tab"
        );
        press(&mut editor, "<tab>");
        assert_eq!(editor.command_suggest, Some(0));
        assert!(!editor.should_quit, "highlighting never runs anything");
        press(&mut editor, "<tab>");
        assert_eq!(editor.command_line, "quit ", "second Tab fills the bar");
        assert_eq!(editor.command_suggest, None);
        press(&mut editor, "<enter>");
        assert!(editor.should_quit, "Enter submits what's in the bar");
    }

    #[test]
    fn command_bar_completion_leaves_room_for_an_argument() {
        let mut editor = editor_with("hello");
        press(&mut editor, ":");
        for c in "lsp-inst".chars() {
            editor.handle_key(Key::char(c));
        }
        // Down cycles the highlight; Tab accepts it into the bar.
        press(&mut editor, "<down> <tab>");
        assert_eq!(editor.command_line, "lsp-install ");
        assert_eq!(editor.mode, Mode::Command, "still editing, not submitted");
        press(&mut editor, "rs");
        assert_eq!(editor.command_line, "lsp-install rs");
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

        // C-h closes the window in one press (no sidebar: focus stays put).
        press(&mut editor, ": help <enter>");
        press(&mut editor, "C-h");
        assert_eq!(editor.help_scroll, None);
        assert!(editor.tree.is_none() && !editor.tree_focused);
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
        // …at the leftmost window it stops — never opening the sidebar…
        press(&mut editor, "C-h");
        assert!(editor.tree.is_none() && !editor.tree_focused);
        // …but crosses into it when it's open.
        press(&mut editor, "C-t");
        press(&mut editor, "C-l"); // back to the editor
        assert!(!editor.tree_focused);
        press(&mut editor, "C-h");
        assert!(editor.tree_focused);
        press(&mut editor, "C-l");
        press(&mut editor, "C-l"); // and across to the right window
        assert_eq!(editor.focused, rightmost);
    }

    #[test]
    fn ctrl_j_k_move_between_stacked_splits() {
        let mut editor = editor_with("hello");
        press(&mut editor, "<space> s h"); // stacked split
        let (wins, _) = editor.window_rects();
        assert_eq!(wins.len(), 2);
        let top = wins.iter().min_by_key(|&&(_, (_, y, ..))| y).unwrap().0;
        let bottom = wins.iter().max_by_key(|&&(_, (_, y, ..))| y).unwrap().0;
        // j/k are flipped by request: C-k descends, C-j ascends.
        editor.focused = top;
        press(&mut editor, "C-k");
        assert_eq!(editor.focused, bottom);
        press(&mut editor, "C-j");
        assert_eq!(editor.focused, top);
        press(&mut editor, "C-j"); // topmost already: stays put
        assert_eq!(editor.focused, top);
        assert!(!editor.tree_focused);
    }

    #[test]
    fn splash_shows_on_empty_start_and_leaves_when_work_begins() {
        let mut editor = Editor::new(vec![], (80, 24), &crate::config::Config::default()).unwrap();
        assert!(editor.show_splash());
        press(&mut editor, "<space>"); // a pending leader doesn't dismiss it
        assert!(editor.show_splash());
        press(&mut editor, "<esc>");
        press(&mut editor, "i");
        assert!(!editor.show_splash(), "insert mode hides it");
        press(&mut editor, "hi");
        press(&mut editor, "<esc>");
        assert!(!editor.show_splash(), "text in the buffer hides it");
    }

    #[test]
    fn ctrl_t_toggles_the_tree_and_ctrl_h_l_only_navigate() {
        let mut editor = editor_with("hello");
        press(&mut editor, "C-h"); // navigation never opens the sidebar
        assert!(editor.tree.is_none() && !editor.tree_focused);
        press(&mut editor, "C-t");
        assert!(editor.tree.is_some() && editor.tree_focused);
        press(&mut editor, "C-l");
        assert!(!editor.tree_focused);
        assert!(editor.tree.is_some(), "tree stays open, just unfocused");
        press(&mut editor, "C-h"); // open sidebar: C-h crosses into it
        assert!(editor.tree_focused);
        press(&mut editor, "C-t"); // toggle from inside closes it
        assert!(editor.tree.is_none());
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
        assert!(completion
            .items
            .iter()
            .any(|(label, _)| label == "notes.txt"));
        press(&mut editor, "n");
        press(&mut editor, "<tab> <enter>");
        assert!(editor.doc().text.to_string().ends_with("/notes.txt"));
    }

    #[test]
    fn typing_pops_intellisense_from_buffer_words() {
        let mut editor = editor_with("printer value");
        press(&mut editor, "A");
        press(&mut editor, "<space>");
        press(&mut editor, "pri"); // two identifier chars trigger the menu
        let completion = editor.completion.as_ref().expect("menu popped");
        assert!(!completion.navigated);
        assert_eq!(completion.items[0].0, "printer");
        // Tab steps into the list, Enter accepts.
        press(&mut editor, "<tab> <enter>");
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
        config
            .keys_normal
            .push(("Z".into(), "not_a_command".into()));
        let mut editor = Editor::new(vec![], (80, 24), &config).unwrap();
        assert!(editor.status.contains("not_a_command")); // bad bind reported
        press(&mut editor, "Q");
        assert!(editor.should_quit);
    }

    #[test]
    fn each_delete_is_its_own_undo_step() {
        let mut editor = editor_with("a\nb\nc\n");
        press(&mut editor, "dd dd");
        assert_eq!(editor.doc().text.to_string(), "c\n");
        press(&mut editor, "u");
        assert_eq!(editor.doc().text.to_string(), "b\nc\n");
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
