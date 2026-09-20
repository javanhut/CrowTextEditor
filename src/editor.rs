//! Editor state and the key dispatch loop.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::commands;
use crate::document::Document;
use crate::keymap::{Key, KeyCode, KeyTrie, KeymapResult};
use crate::lsp;
use crate::terminal::{Terminal, Wake};

use crate::search;

mod completion;
mod jumps;
mod lsp_features;
mod lsp_glue;
mod mouse;
mod objects;
mod picker_keys;
mod repeat;
mod selections;
mod terminal_split;
mod tools;
mod tree;
mod watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Command,
    Search,
    Picker,
    /// The terminal split has focus and keys go to the shell.
    Terminal,
}

/// One recorded input, for `.` and macros.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    Key(Key),
    Paste(String),
    /// A completion was accepted: delete this many chars before the cursor,
    /// then insert the text. Recorded as its effect, because replaying the
    /// keys would depend on what the menu happened to offer at the time.
    Complete(usize, String),
}

/// What the next typed character is for, after a key that asks for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharWait {
    /// `mi` / `ma`: select inside / around the object named by the char.
    Inside,
    Around,
    /// `md`: delete the surrounding pair.
    SurroundDelete,
    /// `mr`: replace the surrounding pair — first the old, then the new.
    SurroundReplace(Option<char>),
    /// `q`: start recording a macro into the named register.
    MacroRecord,
    /// `@`: replay the named macro this many times.
    MacroPlay(usize),
}

/// Which selection operation the regex prompt is collecting a pattern for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectPrompt {
    /// Split every selection on the pattern's matches.
    Split,
    /// Keep only the selections that contain a match.
    Keep,
    /// Drop the selections that contain a match.
    Remove,
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
    pub(crate) fn leaf_ids(&self, out: &mut Vec<usize>) {
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
        normal.bind_str("C-v", "block_mode");
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
        normal.bind_str(".", "repeat_last_change");
        normal.bind_str("q", "macro_record");
        normal.bind_str("@", "macro_play");

        // text objects and surround
        normal.bind_str("mi", "select_inside");
        normal.bind_str("ma", "select_around");
        normal.bind_str("md", "surround_delete");
        normal.bind_str("mr", "surround_replace");

        // selections
        normal.bind_str("A-s", "split_selection_lines");
        normal.bind_str("A-S", "split_selection");
        normal.bind_str("A-k", "keep_selections");
        normal.bind_str("A-K", "remove_selections");
        normal.bind_str("&", "align_selections");
        normal.bind_str(")", "rotate_selections_forward");
        normal.bind_str("(", "rotate_selections_backward");
        normal.bind_str("_", "trim_selections");

        // jumps
        normal.bind_str("C-o", "jump_back");
        normal.bind_str("C-i", "jump_forward");
        // Most terminals can't tell C-i from Tab.
        normal.bind_str("<tab>", "jump_forward");
        normal.bind_str("]d", "next_diagnostic");
        normal.bind_str("[d", "prev_diagnostic");
        normal.bind_str("]g", "next_change");
        normal.bind_str("[g", "prev_change");

        // windows, buffers, files, lifecycle
        normal.bind_str("C-w v", "split_vertical");
        normal.bind_str("C-w s", "split_horizontal");
        normal.bind_str("C-w w", "next_window");
        normal.bind_str("C-w q", "quit");
        normal.bind_str("gn", "next_buffer");
        normal.bind_str("gp", "prev_buffer");
        normal.bind_str("gd", "goto_definition");
        normal.bind_str("gr", "goto_references");
        normal.bind_str("K", "hover");
        normal.bind_str("<space> a", "code_action");
        normal.bind_str("<space> R", "rename");
        normal.bind_str("<space> s s", "document_symbols");
        normal.bind_str("<space> S", "workspace_symbols");
        normal.bind_str("<space> x", "diagnostics");
        normal.bind_str("gl", "diagnostic_detail");

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
        normal.bind_str("<space> t", "terminal");
        normal.bind_str("<space> T", "theme_picker");
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
    /// Another key is waiting for a character (text objects, macros…).
    pub awaiting_char: Option<CharWait>,
    /// The search prompt is collecting a pattern for a selection operation.
    pub select_prompt: Option<SelectPrompt>,
    /// `.`: the inputs of the last change, and of the command in progress.
    pub last_change: Vec<Input>,
    change_keys: Vec<Input>,
    /// Selecting commands since the last change, kept to prefix the next one.
    selection_prefix: Vec<Input>,
    /// (buffer, revision) when the command in progress began.
    change_start: (usize, u64),
    /// The command in progress went through a prompt or picker, so it isn't
    /// a change `.` can repeat.
    change_via_prompt: bool,
    /// The last command a keymap dispatched, so `.` can skip undo and itself.
    last_command: Option<&'static str>,
    /// What accepting a completion did, for the recorder to note in place
    /// of the key that accepted it.
    completion_effect: Option<Input>,
    /// Replaying `.` or a macro: nothing new is recorded meanwhile.
    pub replaying: bool,
    /// How deep replays are nested (a macro calling a macro), to stop runaways.
    replay_depth: usize,
    /// The macro being recorded: its register and the inputs so far.
    pub macro_rec: Option<(char, Vec<Input>)>,
    pub macros: HashMap<char, Vec<Input>>,
    last_macro: Option<char>,
    /// Places jumped from, as (buffer, char offset); `C-o`/`C-i` walk them.
    jumps: Vec<(usize, usize)>,
    jump_idx: usize,
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
    /// Block mode (`C-v`): motions stretch a rectangle, one selection per
    /// line of it. See `selections.rs`.
    pub block: Option<selections::Block>,
    /// One running language server per distinct command; documents are
    /// synced only to their own language's server.
    lsps: Vec<lsp::Client>,
    /// From crow.toml: (file extension, server command).
    lsp_table: Vec<(String, String)>,
    /// Commands that failed to spawn or died, so we don't retry every tick.
    lsp_failed: std::collections::HashSet<String>,
    /// The (file, revision) autosave last failed to write, so it isn't retried
    /// and re-reported every tick.
    pub(crate) autosave_failed: Option<(PathBuf, u64)>,
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
    /// Signature help owed to the server on the next tick.
    lsp_signature_pending: bool,
    /// The signature being typed, with its active parameter's char range.
    pub signature: Option<(String, Option<(usize, usize)>)>,
    /// A `textDocument/formatting` in flight: (buffer, its revision then).
    pending_format: Option<(usize, u64)>,
    /// The file a document-symbol request was made for.
    symbols_path: Option<PathBuf>,
    /// Sealed-text lookups from ivaldi, answered from background threads,
    /// as (the buffer asked about, the path asked about, the answer).
    vcs_tx: std::sync::mpsc::Sender<(usize, PathBuf, crate::vcs::Base)>,
    vcs_rx: std::sync::mpsc::Receiver<(usize, PathBuf, crate::vcs::Base)>,
    /// When the change markers were last refreshed from ivaldi.
    vcs_refreshed: std::time::Instant,
    /// When open files were last checked for changes on disk.
    disk_checked: std::time::Instant,
    /// When swap files were last written.
    swap_written: std::time::Instant,
    /// A mouse drag is extending the selection in the focused window.
    mouse_drag: bool,
    /// `:q!` — leave without keeping anything, swap files included.
    pub quit_discarding: bool,
    /// A "not installed — run `…`? (y/N)" offer; the next keypress answers it.
    pub pending_install: Option<(String, String)>,
    /// A background install in flight: (program, its result channel).
    install: Option<(String, std::sync::mpsc::Receiver<Result<(), String>>)>,
    /// An install running in the terminal split, where sudo can ask for a
    /// password: (program, whether the terminal is ours — spawned for the
    /// install and closed when it ends — or the user's own shell, which was
    /// handed the command and is watched for the program to appear).
    terminal_install: Option<(String, bool)>,
    /// Manifest badges: (ecosystem, dep name) -> (current version, latest).
    pub dep_info: HashMap<(crate::deps::Kind, String), (Option<String>, Option<String>)>,
    /// All in-flight registry fetches stream over this one channel.
    deps_rx: Option<std::sync::mpsc::Receiver<crate::deps::Info>>,
    deps_tx: Option<std::sync::mpsc::Sender<crate::deps::Info>>,
    /// Manifests already fetched this session.
    deps_fetched: std::collections::HashSet<PathBuf>,
    /// The shell split (`space t`), running whether or not it is shown.
    pub terminal: Option<Terminal>,
    /// Counts terminals ever started, so output from a closed one is dropped.
    terminal_generation: u64,
    /// A `C-w` or `C-\` typed into the terminal, waiting for its second key.
    term_pending: Option<Key>,
    /// Everything that wakes the main loop: input, shell output.
    pub wake_tx: std::sync::mpsc::Sender<Wake>,
    /// The receiving end, taken by the main loop at startup.
    pub wake_rx: Option<std::sync::mpsc::Receiver<Wake>>,
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
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let (vcs_tx, vcs_rx) = std::sync::mpsc::channel();
        let long_ago = std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(3600))
            .unwrap_or_else(std::time::Instant::now);
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
            awaiting_char: None,
            select_prompt: None,
            last_change: Vec::new(),
            change_keys: Vec::new(),
            selection_prefix: Vec::new(),
            change_start: (0, 0),
            change_via_prompt: false,
            last_command: None,
            completion_effect: None,
            replaying: false,
            replay_depth: 0,
            macro_rec: None,
            macros: HashMap::new(),
            last_macro: None,
            jumps: Vec::new(),
            jump_idx: 0,
            register_fresh: true,
            last_search: String::new(),
            search_select: false,
            search_origin: (0, 0),
            keep_selection: false,
            extend: false,
            block: None,
            lsps: Vec::new(),
            lsp_failed: std::collections::HashSet::new(),
            autosave_failed: None,
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
            lsp_signature_pending: false,
            signature: None,
            pending_format: None,
            symbols_path: None,
            vcs_tx,
            vcs_rx,
            vcs_refreshed: std::time::Instant::now(),
            disk_checked: std::time::Instant::now(),
            swap_written: long_ago,
            mouse_drag: false,
            quit_discarding: false,
            pending_install: None,
            install: None,
            terminal_install: None,
            dep_info: HashMap::new(),
            deps_rx: None,
            deps_tx: None,
            deps_fetched: std::collections::HashSet::new(),
            terminal: None,
            terminal_generation: 0,
            term_pending: None,
            wake_tx,
            wake_rx: Some(wake_rx),
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
        self.sync_focus_mode();
    }

    pub fn window_count(&self) -> usize {
        self.layout.count()
    }

    pub fn close_focused_window(&mut self) {
        // The shell keeps running: closing its window hides it, and
        // `space t` brings it back.
        if self.terminal_focused() {
            self.hide_terminal();
            return;
        }
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
        self.sync_focus_mode();
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
        self.sync_focus_mode();
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
            self.signature = None;
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

    /// One keypress: handled, and recorded for `.` and any macro being
    /// recorded — unless it is itself part of a replay.
    pub fn handle_key(&mut self, key: Key) {
        let recording = !self.replaying;
        if recording {
            self.record(Input::Key(key));
        }
        self.handle_key_inner(key);
        if recording {
            self.finish_change_record();
        }
    }

    fn handle_key_inner(&mut self, key: Key) {
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

        if self.mode == Mode::Terminal {
            self.handle_terminal_key(key);
            return;
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
        if self.terminal_focused() {
            self.handle_terminal_normal_key(key);
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

        // Text objects, surround edits and macros name their target with
        // one more character; anything else (Esc) cancels.
        if let Some(wait) = self.awaiting_char.take() {
            self.status.clear();
            if let KeyCode::Char(c) = key.code {
                if !key.ctrl && !key.alt {
                    self.char_wait(wait, c);
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
            && !self.extend
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
                // A motion in block mode moves the rectangle's corner, alone;
                // anything else is what the block was built for, and runs on
                // its selections like on any others.
                let stretching = self.block.is_some() && commands::BLOCK_MOTIONS.contains(&command.name);
                if stretching {
                    self.block_to_corner();
                } else if command.name != "block_mode" && self.block.take().is_some() {
                    self.block_finish(command.name);
                }
                if !self.doc().extra.is_empty() && commands::PER_CURSOR.contains(&command.name) {
                    self.dispatch_per_cursor(command);
                } else {
                    (command.func)(self);
                }
                // After the call: a replay (`.`, `@`) dispatches commands of
                // its own, and the one to remember is the replay itself.
                self.last_command = Some(command.name);
                if !self.keep_selection && !(self.extend && self.mode == Mode::Normal) {
                    let doc = self.doc_mut();
                    doc.anchor = doc.cursor;
                    for (a, c) in &mut doc.extra {
                        *a = *c;
                    }
                }
                self.doc_mut().dedupe_cursors();
                if stretching {
                    self.block_stretch();
                }
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
        "term",
        "wrap",
        "fmt",
        "bn",
        "bp",
        "theme",
        "install",
        "lsp-install",
        "config",
        "config!",
        "wa",
        "e!",
        "rename",
        "recover",
        "recover!",
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
                self.select_prompt = None;
                self.restore_search_origin();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let query = std::mem::take(&mut self.command_line);
                self.mode = Mode::Normal;
                if let Some(kind) = self.select_prompt.take() {
                    self.restore_search_origin();
                    self.apply_select_prompt(kind, &query);
                    return;
                }
                if !query.is_empty() {
                    self.last_search = query;
                }
                if self.search_select {
                    self.select_all_matches();
                } else if (self.doc().anchor, self.doc().cursor) != self.search_origin {
                    // `/` moved us: the place we left is worth a C-o.
                    self.push_jump_at(self.current, self.search_origin.1);
                }
                // For `/`, the incremental preview already put the selection
                // on the match; Enter just keeps it.
            }
            KeyCode::Backspace => {
                if self.command_line.pop().is_none() {
                    self.select_prompt = None;
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
            "wa" | "wall" => self.write_all(),
            "q" | "quit" => (commands::find("quit").unwrap().func)(self),
            "q!" | "quit!" => {
                self.quit_discarding = true;
                self.should_quit = true;
            }
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
                        self.leave_terminal_for_edit();
                        crate::config::record_recent(Path::new(path));
                        self.documents.push(doc);
                        self.current = self.documents.len() - 1;
                        self.set_status(format!("\"{path}\""));
                    }
                    Err(e) => self.set_status(format!("Error: {e}")),
                },
                None => self.set_status("Usage: :e <file>"),
            },
            "e!" | "edit!" => match self.doc_mut().reload() {
                Ok(true) => self.set_status("reloaded from disk (u undoes)"),
                Ok(false) => self.set_status("already the same as the file on disk"),
                Err(e) => self.set_status(format!("Error: {e}")),
            },
            "rename" => match arg {
                Some(name) => self.rename_symbol(name),
                None => self.set_status("Usage: :rename <new name>"),
            },
            "recover" => self.recover_swap(),
            "recover!" => {
                self.doc_mut().remove_swap();
                self.set_status("swap file discarded");
            }
            "help" | "h" => self.help_scroll = Some(0),
            "md" | "preview" => self.toggle_preview(),
            "term" | "terminal" => self.toggle_terminal(),
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
                    self.leave_terminal_for_edit();
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
                    self.push_jump();
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
        self.set_status(format!("{n} substitution{}", if n == 1 { "" } else { "s" }));
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
        let was_mouse = crate::config::mouse();
        let theme_ok = crate::config::apply(&config);
        if config.mouse != was_mouse {
            crate::ui::set_mouse_capture(config.mouse);
        }
        // Markers turned off go away; turned on, they are fetched again.
        for i in 0..self.documents.len() {
            if config.vcs_gutter {
                self.documents[i].vcs.base = crate::vcs::Base::Unknown;
            } else {
                self.documents[i].vcs = crate::vcs::DocState::default();
            }
        }
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

    // ---- paste -------------------------------------------------------------

    /// Bracketed paste: the text goes in verbatim — no auto-indent, no
    /// autoclose, no per-key replay. That's the whole point of the bracket.
    pub fn handle_paste(&mut self, text: &str) {
        let recording = !self.replaying;
        if recording {
            self.record(Input::Paste(text.to_string()));
        }
        self.handle_paste_inner(text);
        if recording {
            self.finish_change_record();
        }
    }

    fn handle_paste_inner(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        let pasted_something = !text.is_empty();
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
            Mode::Terminal => {
                if let Some(t) = self.terminal.as_mut() {
                    t.paste(&text);
                }
            }
            Mode::Insert | Mode::Normal => {
                if self.tree_focused || self.help_scroll.is_some() {
                    return;
                }
                if self.mode == Mode::Normal && self.doc().anchor != self.doc().cursor {
                    let from = crate::position::grapheme_floor(
                        self.doc().text.slice(..),
                        self.doc().anchor.min(self.doc().cursor),
                    );
                    let to = crate::position::grapheme_ceil(
                        self.doc().text.slice(..),
                        self.doc().anchor.max(self.doc().cursor),
                    );
                    let pasted_len = text.chars().count();
                    let tx = crate::transaction::Transaction::change(
                        &self.doc().text,
                        std::iter::once((from, to, Some(text))),
                    );
                    self.extend = false;
                    self.doc_mut().apply(tx, from + pasted_len);
                } else {
                    self.doc_mut().insert_at_cursor(&text);
                }
                if self.mode == Mode::Normal {
                    let doc = self.doc_mut();
                    if pasted_something {
                        doc.cursor =
                            crate::position::prev_grapheme_boundary(doc.text.slice(..), doc.cursor);
                    }
                    doc.anchor = doc.cursor;
                    doc.commit_undo_group();
                }
            }
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
        // A resize changes the wrap width under the viewport: the row it is
        // parked on may no longer exist.
        let rows = doc.visual_rows(doc.view_line, wrap);
        doc.view_row = doc.view_row.min(rows.saturating_sub(1));
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
        if self.terminal_focused() {
            // The shell's own cursor, unless you've scrolled away from it.
            let t = self.terminal.as_ref()?;
            if t.scroll != 0 || !t.screen.cursor_visible {
                return None;
            }
            let (rx, ry, rw, rh) = self.focused_rect();
            let (row, col) = t.screen.cursor();
            if row >= rh as usize || col >= rw as usize {
                return None;
            }
            return Some((rx + col as u16, ry + row as u16));
        }

        let (rx, ry, rw, rh) = self.focused_rect();
        let wrap = self.wrap_width();
        let doc = self.doc();
        let cursor = if self.inclusive() && doc.anchor < doc.cursor {
            crate::position::prev_grapheme_boundary(doc.text.slice(..), doc.cursor)
        } else {
            doc.cursor
        };
        let (line, row, col) = doc.position_visual(cursor, wrap);
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
        let cursor = if self.inclusive() && doc.anchor < doc.cursor {
            crate::position::prev_grapheme_boundary(doc.text.slice(..), doc.cursor)
        } else {
            doc.cursor
        };
        let line = doc.text.char_to_line(cursor.min(doc.text.len_chars()));
        let col = cursor - doc.line_start(line);
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
        assert_eq!(editor.doc().text.to_string(), "ef");
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
        assert_eq!(editor.doc().text.to_string(), "lm");
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
        // w selects "foo ", h collapses onto the space; v selects it.
        press(&mut editor, "wh");
        press(&mut editor, "vd");
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
    fn paste_replaces_a_characterwise_selection() {
        let mut editor = editor_with("abc");
        editor.register = "XY".into();
        press(&mut editor, "vlp");
        assert_eq!(editor.doc().text.to_string(), "XYc");
        assert_eq!(editor.doc().cursor, 1);
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
        press(&mut editor, "lvd");
        assert_eq!(editor.doc().text.to_string(), "bz");
    }

    #[test]
    fn v_immediately_selects_the_character_under_the_cursor() {
        let mut editor = editor_with("abc");
        press(&mut editor, "vd");
        assert_eq!(editor.doc().text.to_string(), "bc");
    }

    #[test]
    fn characterwise_selection_is_inclusive_in_both_directions() {
        let mut editor = editor_with("abcd");
        press(&mut editor, "lvhd");
        assert_eq!(editor.doc().text.to_string(), "cd");

        let mut editor = editor_with("abcd");
        press(&mut editor, "vld");
        assert_eq!(editor.doc().text.to_string(), "cd");
    }

    #[test]
    fn characterwise_selection_wraps_across_lines_in_both_directions() {
        let mut editor = editor_with("ab\ncd");
        press(&mut editor, "$vld");
        assert_eq!(editor.doc().text.to_string(), "ad");

        let mut editor = editor_with("ab\ncd");
        press(&mut editor, "jvhd");
        assert_eq!(editor.doc().text.to_string(), "ad");
    }

    #[test]
    fn characterwise_selection_keeps_a_grapheme_whole() {
        let mut editor = editor_with("👨\u{200d}👩\u{200d}👧x");
        press(&mut editor, "vd");
        assert_eq!(editor.doc().text.to_string(), "x");
    }

    #[test]
    fn plain_motions_extend_too_in_extend_mode() {
        let mut editor = editor_with("abcd");
        press(&mut editor, "vlld");
        assert_eq!(editor.doc().text.to_string(), "d");
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
    fn a_second_v_collapses_and_leaves_select_mode() {
        let mut editor = editor_with("abc");
        press(&mut editor, "vlv");
        assert!(!editor.extend);
        assert_eq!(editor.doc().anchor, editor.doc().cursor);
        assert_eq!(editor.doc().cursor, 1);
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
        press(&mut editor, "vd"); // select "f", delete: unnamed register = "f"
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
    fn space_t_opens_a_shell_below_and_toggles_it_away_and_back() {
        let mut editor = editor_with("text\n");
        press(&mut editor, "<space> t");
        assert_eq!(editor.window_count(), 2);
        assert!(editor.terminal_focused());
        assert_eq!(editor.mode, Mode::Terminal);
        let term_win = editor.focused;
        let (wins, _) = editor.window_rects();
        let (_, (_, ty, ..)) = wins.iter().find(|(id, _)| *id == term_win).unwrap();
        assert!(*ty > 0, "the shell sits below the text");
        assert!(editor.screen_cursor().is_some());

        // C-\ C-n drops to normal mode in the same window; i goes back.
        press(&mut editor, "C-\\ C-n");
        assert_eq!(editor.mode, Mode::Normal);
        assert!(editor.terminal_focused());
        press(&mut editor, "i");
        assert_eq!(editor.mode, Mode::Terminal);

        // Hiding keeps the shell; showing brings it back into the shell.
        press(&mut editor, "C-w N");
        press(&mut editor, "<space> t");
        assert_eq!(editor.window_count(), 1);
        assert!(editor.terminal.is_some());
        assert!(!editor.terminal_focused());
        assert_eq!(editor.mode, Mode::Normal);
        press(&mut editor, "<space> t");
        assert_eq!(editor.window_count(), 2);
        assert_eq!(editor.mode, Mode::Terminal);

        // Moving focus out and back in switches modes with it.
        press(&mut editor, "C-w j");
        assert!(!editor.terminal_focused());
        assert_eq!(editor.mode, Mode::Normal);
        press(&mut editor, "<space> t");
        assert!(editor.terminal_focused());
        assert_eq!(editor.mode, Mode::Terminal);

        // :q on the shell hides it rather than killing it; the buffer's
        // window is untouched.
        press(&mut editor, "C-w :");
        assert_eq!(editor.mode, Mode::Command);
        press(&mut editor, "q <enter>");
        assert_eq!(editor.window_count(), 1);
        assert!(editor.terminal.is_some());
        assert_eq!(editor.doc().text.to_string(), "text\n");
        editor.close_terminal();
        assert!(editor.terminal.is_none());
    }

    /// Wait for the install terminal to finish, the way the main loop would.
    fn settle_install(editor: &mut Editor) {
        for _ in 0..200 {
            if editor.terminal_tick() || editor.install_tick() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("the install never finished: {:?}", editor.status);
    }

    /// A root install runs in a terminal of its own, so sudo has somewhere
    /// to ask; when it succeeds the terminal goes away and the tool counts
    /// as installed.
    #[test]
    fn a_root_install_runs_in_its_own_terminal_and_closes_on_success() {
        let mut editor = editor_with("text\n");
        // `true` stands in for the package manager: it is on every PATH.
        editor.start_install_in_terminal("true", "true");
        assert_eq!(editor.window_count(), 2);
        assert!(editor.terminal_focused(), "the password prompt needs focus");
        assert_eq!(editor.mode, Mode::Terminal);
        assert!(editor.install_running());
        settle_install(&mut editor);
        assert_eq!(editor.status, "true installed");
        assert!(editor.terminal.is_none());
        assert_eq!(editor.window_count(), 1);
        assert!(!editor.install_running());
    }

    /// A failed install holds its terminal until Enter, so the error can be
    /// read, then reports the failure.
    #[test]
    fn a_failed_root_install_waits_for_enter_then_reports() {
        let mut editor = editor_with("text\n");
        editor.start_install_in_terminal("crow-no-such-tool", "false");
        // Still there, waiting on the read.
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert!(!editor.terminal_tick());
        assert!(editor.terminal.is_some());
        editor.terminal.as_mut().unwrap().write(b"\n");
        settle_install(&mut editor);
        assert!(editor.terminal.is_none());
        assert!(
            editor
                .status
                .starts_with("crow-no-such-tool: install failed"),
            "{}",
            editor.status
        );
    }

    /// With a shell already open the command is typed into it instead, and
    /// the tool showing up on PATH is what marks the install done.
    #[test]
    fn a_root_install_uses_the_open_shell_and_watches_path() {
        let mut editor = editor_with("text\n");
        press(&mut editor, "<space> t");
        assert!(editor.focus_window_dir(0, -1));
        editor.sync_focus_mode();
        assert!(!editor.terminal_focused());
        editor.start_install_in_terminal("true", ": would be sudo");
        assert!(editor.terminal_focused(), "the password prompt needs focus");
        assert_eq!(editor.terminal_install, Some(("true".to_string(), false)));
        assert!(
            !editor.install_running(),
            "the user's shell is not ours to wait on"
        );
        assert!(editor.install_tick());
        assert_eq!(editor.status, "true installed");
        assert!(editor.terminal.is_some(), "the user's shell stays");
        editor.close_terminal();
    }

    #[test]
    fn editing_keys_in_the_terminal_never_touch_the_buffer() {
        let mut editor = editor_with("keep me\n");
        press(&mut editor, "<space> t");
        press(&mut editor, "C-\\ C-n");
        press(&mut editor, "dd");
        press(&mut editor, "x");
        press(&mut editor, "p");
        assert_eq!(editor.doc().text.to_string(), "keep me\n");
        assert_eq!(editor.mode, Mode::Normal);
        editor.close_terminal();
    }

    #[test]
    fn opening_a_file_from_the_terminal_lands_in_a_text_window() {
        let dir = std::env::temp_dir().join(format!("crow-term-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "opened\n").unwrap();
        let mut editor = editor_with("");
        press(&mut editor, "<space> t");
        press(&mut editor, "C-w :");
        for c in format!("e {}", path.display()).chars() {
            editor.handle_key(Key::char(c));
        }
        press(&mut editor, "<enter>");
        assert!(!editor.terminal_focused());
        assert_eq!(editor.mode, Mode::Normal);
        assert_eq!(editor.doc().text.to_string(), "opened\n");
        editor.close_terminal();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn theme_picker_previews_live_and_esc_restores() {
        let _guard = crate::theme::TEST_LOCK.lock().unwrap();
        crate::theme::set("default");
        let mut editor = editor_with("hello");
        press(&mut editor, "<space> T");
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
    fn autoclose_still_fires_with_the_completion_popup_open() {
        // `main` in the buffer means typing `ma` opens the word popup; the
        // bracket that follows must still bring its closer instead of being
        // typed raw through the menu.
        let mut editor = editor_with(
            "mainly
",
        );
        press(&mut editor, "i");
        press(&mut editor, "ma");
        assert!(editor.completion.is_some());
        press(&mut editor, "(");
        assert!(editor.completion.is_none());
        assert_eq!(editor.doc().text.to_string(), "ma()mainly\n");

        // Same for quotes: one `"` makes the pair, it does not take three.
        let mut editor = editor_with("mainly\n");
        press(&mut editor, "i");
        press(&mut editor, "ma");
        press(&mut editor, "\"");
        assert_eq!(editor.doc().text.to_string(), "ma\"\"mainly\n");
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
        assert_eq!(editor.doc().text.to_string(), "ef\nghi");
        press(&mut editor, "vjd");
        assert_eq!(editor.doc().text.to_string(), "hi");
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
