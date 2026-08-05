//! Every editor action is a named value, not an arm of a `match`.
//!
//! Commands are `&'static` so a keymap can hold references to them without
//! borrowing the editor, and so bindings can be looked up by name — which is
//! what a config file will need.

use crate::config::tab_width;
use crate::document::Document;
use crate::editor::{Editor, Mode};
use crate::position::{self, CharClass};
use crate::transaction::Transaction;

pub struct Command {
    pub name: &'static str,
    pub func: fn(&mut Editor),
    pub doc: &'static str,
}

pub fn find(name: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.name == name)
}

macro_rules! commands {
    ($($name:ident => $doc:literal),* $(,)?) => {
        pub static COMMANDS: &[Command] = &[
            $(Command { name: stringify!($name), func: $name, doc: $doc },)*
        ];
    };
}

commands! {
    move_left => "move one character left",
    move_right => "move one character right",
    move_up => "move one line up",
    move_down => "move one line down",
    move_line_start => "move to the start of the line",
    move_line_first_nonblank => "move to the first non-blank character",
    move_line_end => "move to the end of the line",
    select_word_next => "select to the start of the next word",
    select_word_prev => "select back to the start of the previous word",
    select_word_end => "select through the end of the current word",
    select_line => "select the current line; repeat to extend",
    expand_selection => "expand the selection to the enclosing syntax node",
    collapse_selection => "collapse the selection to the cursor",
    add_cursor_below => "add a cursor on the next line",
    add_cursor_above => "add a cursor on the previous line",
    remove_extra_cursors => "keep only the primary cursor",
    goto_definition => "jump to the definition under the cursor (LSP)",
    hover => "pop up docs and examples for the symbol under the cursor (LSP)",
    complete => "open the completion menu (LSP)",
    command_palette => "fuzzy-pick any command by name",
    find_files => "fuzzy-find a file under the current directory",
    grep_text => "search file contents across the project",
    recent_files => "reopen a recently opened file",
    file_explorer => "browse the current directory in a picker",
    tree_toggle => "toggle the file tree sidebar",
    toggle_hidden => "show or hide dotfiles and build dirs in the tree, finder, and grep",
    focus_left => "focus the window to the left, or the open file tree",
    focus_right => "focus the window to the right",
    focus_down => "focus the window below",
    focus_up => "focus the window above",
    theme_picker => "pick a theme with live preview",
    search => "search forward, selecting the match",
    select_matches => "select every match of a pattern",
    search_next => "select the next match of the last search",
    search_prev => "select the previous match of the last search",
    goto_file_start => "move to the first line, or with a count to that line",
    goto_file_end => "move to the last line, or with a count to that line",
    extend_mode => "toggle extending selections with every motion",
    half_page_down => "scroll down half a screen",
    half_page_up => "scroll up half a screen",
    page_down => "scroll down a screen",
    page_up => "scroll up a screen",

    insert_mode => "insert before the cursor",
    insert_at_line_start => "insert at the first non-blank character",
    append => "insert after the cursor",
    append_at_line_end => "insert at the end of the line",
    open_below => "open a new line below and insert",
    open_above => "open a new line above and insert",
    normal_mode => "return to normal mode",
    command_mode => "enter a command",

    delete_selection => "delete the selection, or the character under the cursor",
    change_selection => "delete the selection and insert",
    copy => "copy the selection to the register",
    copy_line => "copy the current line to the register (cc)",
    paste_after => "paste the register after the cursor",
    paste_before => "paste the register before the cursor",
    delete_char => "delete the character under the cursor",
    delete_line => "delete the current line into the register (dd, xx)",
    delete_to_line_end => "delete to the end of the line",
    join_lines => "join this line with the next",
    undo => "undo the last change",
    redo => "redo the last undone change",

    insert_newline => "insert a line break",
    insert_tab => "insert a tab",
    delete_backward => "delete the character before the cursor",
    delete_forward => "delete the character under the cursor",

    format_buffer => "run the file's formatter over the buffer (:fmt)",
    dep_upgrade => "rewrite this line's dependency to its latest version (package manifests)",
    next_buffer => "switch to the next buffer",
    prev_buffer => "switch to the previous buffer",
    split_vertical => "split the window side by side",
    split_horizontal => "split the window stacked",
    next_window => "focus the next window",
    save => "write the buffer to disk",
    quit => "close the window, or the editor with the last one",
}

/// Keys that act on the whole line when pressed twice with nothing selected.
/// `handle_key` arms them by character rather than through the keymap, so this
/// is also where `:help` gets their keys column.
pub static LINE_OPS: &[(char, &str)] = &[
    ('d', "delete_line"),
    ('x', "delete_line"),
    ('c', "copy_line"),
];

/// The doubled keys bound to `name`, e.g. `"dd  xx"` — empty if it has none.
fn line_op_keys(name: &str) -> String {
    LINE_OPS
        .iter()
        .filter(|(_, cmd)| *cmd == name)
        .map(|(c, _)| format!("{c}{c}"))
        .collect::<Vec<_>>()
        .join("  ")
}

/// One row of the `:help` window.
pub enum HelpLine {
    Header(&'static str),
    Entry {
        keys: String,
        name: String,
        doc: String,
    },
}

/// The `:help` window's rows: the `:` commands, then every named command
/// with its live key binding.
pub fn help_lines(keymap: &crate::keymap::KeyTrie) -> Vec<HelpLine> {
    let mut out = vec![HelpLine::Header("Command line — press :")];
    for (cmd, doc) in [
        (":w [path]", "write the buffer (:write; to a path if given)"),
        (":q  :q!", "close the window / quit without saving"),
        (":wq  :x", "write, then quit"),
        (":e <file>", "open a file (:edit)"),
        (":fmt", "run the file's formatter over the buffer"),
        (
            ":install <x>",
            "install the tool for a name or extension (:lsp-install <ext> for servers)",
        ),
        (":bn  :bp", "next / previous buffer"),
        (":theme [name]", "list themes, or switch to one"),
        (
            ":config  :config!",
            "edit crow.toml / reload it (restarts language servers if [lsp] changed)",
        ),
        (":<number>", "jump to that line"),
        (":help  :h", "this window"),
        (":<command>", "run any command below by name"),
    ] {
        out.push(HelpLine::Entry {
            keys: cmd.to_string(),
            name: String::new(),
            doc: doc.to_string(),
        });
    }
    out.push(HelpLine::Header(
        "Commands — also in the palette (<space> c)",
    ));
    for c in COMMANDS {
        out.push(HelpLine::Entry {
            keys: keymap
                .binding_of(c.name)
                .unwrap_or_else(|| line_op_keys(c.name)),
            name: c.name.to_string(),
            doc: c.doc.to_string(),
        });
    }
    out
}

/// Commands that run once per cursor when extra cursors exist.
///
/// `handle_key` swaps each extra selection into the primary slot, runs the
/// command, and swaps it back; `Document::apply` remaps every other cursor
/// through each edit's transaction, so per-cursor edits compose correctly.
/// Everything else (scrolling, buffers, undo, save…) runs once.
pub static PER_CURSOR: &[&str] = &[
    "move_left",
    "move_right",
    "move_up",
    "move_down",
    "move_line_start",
    "move_line_first_nonblank",
    "move_line_end",
    "goto_file_start",
    "goto_file_end",
    "select_word_next",
    "select_word_prev",
    "select_word_end",
    "select_line",
    "expand_selection",
    "delete_selection",
    "change_selection",
    "copy",
    "paste_after",
    "paste_before",
    "delete_backward",
    "delete_forward",
];

// ---- motions ---------------------------------------------------------------

fn move_left(editor: &mut Editor) {
    let count = editor.take_count();
    let doc = editor.doc_mut();
    let line_start = doc.line_start(doc.cursor_line());
    for _ in 0..count {
        if doc.cursor <= line_start {
            break;
        }
        doc.cursor = position::prev_grapheme_boundary(doc.text.slice(..), doc.cursor);
    }
    doc.cursor = doc.cursor.max(line_start);
    doc.goal_col = None;
}

fn move_right(editor: &mut Editor) {
    let count = editor.take_count();
    let past_end = editor.mode == Mode::Insert;
    let doc = editor.doc_mut();
    for _ in 0..count {
        doc.cursor = position::next_grapheme_boundary(doc.text.slice(..), doc.cursor);
    }
    doc.clamp_cursor(past_end);
    doc.goal_col = None;
}

fn move_vertical(doc: &mut Document, delta: isize, past_end: bool) {
    let (line, display_col) = doc.cursor_display();
    let goal = doc.goal_col.unwrap_or(display_col);

    let last = doc.line_count().saturating_sub(1) as isize;
    let target = (line as isize + delta).clamp(0, last) as usize;

    let offset = position::display_col_to_char(doc.line(target), goal, tab_width());
    doc.cursor = doc.line_start(target) + offset;
    doc.clamp_cursor(past_end);
    doc.goal_col = Some(goal);
}

fn move_up(editor: &mut Editor) {
    let count = editor.take_count() as isize;
    let past_end = editor.mode == Mode::Insert;
    move_vertical(editor.doc_mut(), -count, past_end);
}

fn move_down(editor: &mut Editor) {
    let count = editor.take_count() as isize;
    let past_end = editor.mode == Mode::Insert;
    move_vertical(editor.doc_mut(), count, past_end);
}

fn move_line_start(editor: &mut Editor) {
    let doc = editor.doc_mut();
    doc.cursor = doc.line_start(doc.cursor_line());
    doc.goal_col = None;
}

fn move_line_first_nonblank(editor: &mut Editor) {
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    let start = doc.line_start(line);
    let len = doc.line_len(line);
    let slice = doc.line(line);

    let mut offset = 0;
    while offset < len && slice.char(offset).is_whitespace() {
        offset += 1;
    }
    doc.cursor = start + offset.min(len.saturating_sub(1));
    doc.goal_col = None;
}

fn move_line_end(editor: &mut Editor) {
    let past_end = editor.mode == Mode::Insert;
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    doc.cursor = doc.line_end(line);
    doc.clamp_cursor(past_end);
    doc.goal_col = None;
}

// ---- selection -------------------------------------------------------------
//
// The Helix model, not the vim model: motions select the text they cross, and
// d/c/y act on the selection. What you are about to change is always on
// screen — there is no operator-pending state to hold in your head.
//
// The selection is `anchor..cursor`, half-open at whichever end is greater.
// Selecting commands set `editor.keep_selection`; any command that doesn't is
// collapsed by `handle_key` afterwards, so plain motions clear the selection.

fn extend_mode(editor: &mut Editor) {
    editor.keep_selection = true;
    editor.extend = !editor.extend;
}

fn select_word_next(editor: &mut Editor) {
    let count = editor.take_count();
    editor.keep_selection = true;
    let extend = editor.extend;
    let doc = editor.doc_mut();
    if !extend {
        doc.anchor = doc.cursor;
    }
    for _ in 0..count {
        doc.cursor = next_word_start(doc, doc.cursor);
    }
    doc.goal_col = None;
}

fn select_word_prev(editor: &mut Editor) {
    let count = editor.take_count();
    editor.keep_selection = true;
    let extend = editor.extend;
    let doc = editor.doc_mut();
    if !extend {
        doc.anchor = doc.cursor;
    }
    for _ in 0..count {
        doc.cursor = prev_word_start(doc, doc.cursor);
    }
    doc.goal_col = None;
}

fn select_word_end(editor: &mut Editor) {
    let count = editor.take_count();
    editor.keep_selection = true;
    let extend = editor.extend;
    let doc = editor.doc_mut();
    if !extend {
        doc.anchor = doc.cursor;
    }
    for _ in 0..count {
        doc.cursor = word_end(doc, doc.cursor);
    }
    // Selections are exclusive at the cursor end; step past the last word char
    // so `ed` deletes the whole word.
    doc.cursor = (doc.cursor + 1).min(doc.text.len_chars());
    doc.goal_col = None;
}

fn select_line(editor: &mut Editor) {
    let count = editor.take_count();
    editor.keep_selection = true;
    let extend = editor.extend;
    let doc = editor.doc_mut();
    for _ in 0..count {
        let line = doc.cursor_line();
        let anchor_line = doc.text.char_to_line(doc.anchor.min(doc.text.len_chars()));
        // A previous `x` leaves the anchor at a line start and the cursor at
        // the start of the next line (or EOF); in that case extend downwards
        // instead of re-selecting.
        let extending = doc.anchor == doc.line_start(anchor_line)
            && doc.anchor < doc.cursor
            && (doc.cursor == doc.line_start(line) || doc.cursor == doc.text.len_chars());
        if !extending && !extend {
            doc.anchor = doc.line_start(line);
        }
        doc.cursor = if line + 1 < doc.line_count() {
            doc.line_start(line + 1)
        } else {
            doc.text.len_chars()
        };
    }
    doc.goal_col = None;
}

/// Grow the selection to the smallest syntax node strictly containing it:
/// token, expression, statement, block, item — one keypress per level.
fn expand_selection(editor: &mut Editor) {
    editor.keep_selection = true;
    let range = {
        let doc = editor.doc();
        let len = doc.text.len_chars();
        let (from, to) = {
            let a = doc.anchor.min(len);
            let c = doc.cursor.min(len);
            (a.min(c), a.max(c))
        };
        doc.syntax
            .as_ref()
            .and_then(|s| s.tree.as_ref())
            .map(|tree| {
                let b0 = doc.text.char_to_byte(from);
                let b1 = doc.text.char_to_byte(to);
                let mut node = tree.root_node().descendant_for_byte_range(b0, b1)?;
                while node.start_byte() == b0 && node.end_byte() == b1 {
                    node = node.parent()?;
                }
                Some((
                    doc.text.byte_to_char(node.start_byte()),
                    doc.text.byte_to_char(node.end_byte()),
                ))
            })
    };
    match range {
        None => editor.set_status("no syntax tree for this buffer"),
        Some(None) => {}
        Some(Some((s, e))) => {
            let doc = editor.doc_mut();
            doc.anchor = s;
            doc.cursor = e;
            doc.goal_col = None;
        }
    }
}

fn collapse_selection(_editor: &mut Editor) {
    // Not setting `keep_selection` is the whole implementation: the
    // post-command collapse in `handle_key` does the work.
}

// ---- multiple cursors ------------------------------------------------------

fn add_cursor_below(editor: &mut Editor) {
    add_cursor(editor, 1);
}

fn add_cursor_above(editor: &mut Editor) {
    add_cursor(editor, -1);
}

/// Copy the outermost selection to the adjacent line, column-for-column.
/// A selection keeps its shape only when it fits on one line; otherwise the
/// new cursor is collapsed.
fn add_cursor(editor: &mut Editor, dir: isize) {
    let count = editor.take_count();
    editor.keep_selection = true;
    let doc = editor.doc_mut();
    for _ in 0..count {
        let len = doc.text.len_chars();
        let last_line = doc.line_count().saturating_sub(1);
        let (a, c) = std::iter::once((doc.anchor, doc.cursor))
            .chain(doc.extra.iter().copied())
            .max_by_key(|&(_, c)| if dir > 0 { c as isize } else { -(c as isize) })
            .unwrap();

        let cline = doc.text.char_to_line(c.min(len)).min(last_line);
        let target = cline as isize + dir;
        if target < 0 || target as usize > last_line {
            return;
        }
        let target = target as usize;

        let col_on = |doc: &Document, line: usize, pos: usize| {
            position::char_to_display_col(doc.line(line), pos - doc.line_start(line), tab_width())
        };
        let to_target = |doc: &Document, col: usize| {
            doc.line_start(target)
                + position::display_col_to_char(doc.line(target), col, tab_width())
        };

        let new_c = to_target(doc, col_on(doc, cline, c));
        let aline = doc.text.char_to_line(a.min(len)).min(last_line);
        let new_a = if aline == cline && a != c {
            to_target(doc, col_on(doc, cline, a))
        } else {
            new_c
        };
        doc.extra.push((new_a, new_c));
        doc.dedupe_cursors();
    }
}

fn remove_extra_cursors(editor: &mut Editor) {
    editor.keep_selection = true;
    editor.doc_mut().extra.clear();
}

// ---- pickers ---------------------------------------------------------------

fn command_palette(editor: &mut Editor) {
    editor.open_picker(crate::picker::Picker::commands(&editor.keymaps.normal));
}

fn theme_picker(editor: &mut Editor) {
    editor.open_picker(crate::picker::Picker::themes());
}

fn find_files(editor: &mut Editor) {
    let root = std::env::current_dir().unwrap_or_default();
    editor.open_picker(crate::picker::Picker::files(&root));
}

fn recent_files(editor: &mut Editor) {
    editor.open_picker(crate::picker::Picker::recent());
}

fn grep_text(editor: &mut Editor) {
    let root = std::env::current_dir().unwrap_or_default();
    editor.open_picker(crate::picker::Picker::grep(&root));
}

fn file_explorer(editor: &mut Editor) {
    let dir = std::env::current_dir().unwrap_or_default();
    editor.open_picker(crate::picker::Picker::explorer(dir));
}

fn tree_toggle(editor: &mut Editor) {
    editor.tree_toggle();
}

fn toggle_hidden(editor: &mut Editor) {
    let shown = crate::config::toggle_hidden();
    if let Some(tree) = editor.tree.as_mut() {
        tree.rebuild();
    }
    editor.set_status(if shown {
        "dotfiles shown"
    } else {
        "dotfiles hidden"
    });
}

fn focus_left(editor: &mut Editor) {
    if editor.focus_window_dir(-1, 0) {
        return;
    }
    // Leftmost window: the sidebar is next — but navigation never opens it.
    if editor.tree.is_some() {
        editor.tree_focused = true;
    }
}

fn focus_right(editor: &mut Editor) {
    editor.focus_window_dir(1, 0);
}

fn focus_down(editor: &mut Editor) {
    editor.focus_window_dir(0, 1);
}

fn focus_up(editor: &mut Editor) {
    editor.focus_window_dir(0, -1);
}

// ---- lsp -------------------------------------------------------------------

fn complete(editor: &mut Editor) {
    lsp_position_request(editor, "completion", "textDocument/completion");
}

fn goto_definition(editor: &mut Editor) {
    lsp_position_request(editor, "definition", "textDocument/definition");
}

fn hover(editor: &mut Editor) {
    lsp_position_request(editor, "hover", "textDocument/hover");
}

fn lsp_position_request(editor: &mut Editor, tag: &'static str, method: &str) {
    let Some(path) = editor.doc().path.clone() else {
        editor.set_status("buffer has no file");
        return;
    };
    let (line, col) = editor.doc().cursor_line_col();
    let utf16_col = position::char_to_utf16(editor.doc().line(line), col);
    match editor.current_client() {
        Some(lsp) => lsp.request_position(tag, method, &path, line, utf16_col),
        None => editor.set_status("language server not running"),
    }
}

// ---- search ----------------------------------------------------------------

fn search(editor: &mut Editor) {
    open_search_prompt(editor, false);
}

fn select_matches(editor: &mut Editor) {
    open_search_prompt(editor, true);
}

fn open_search_prompt(editor: &mut Editor, select_all: bool) {
    editor.keep_selection = true;
    editor.command_line.clear();
    editor.search_select = select_all;
    editor.search_origin = (editor.doc().anchor, editor.doc().cursor);
    editor.set_mode(Mode::Search);
}

fn search_next(editor: &mut Editor) {
    search_step(editor, true);
}

fn search_prev(editor: &mut Editor) {
    search_step(editor, false);
}

fn search_step(editor: &mut Editor, forward: bool) {
    if editor.last_search.is_empty() {
        editor.set_status("no previous search");
        return;
    }
    editor.keep_selection = true;
    let all = crate::search::matches(&editor.doc().text, &editor.last_search);
    let hit = if forward {
        let from = editor.doc().cursor;
        all.iter()
            .copied()
            .find(|&(p, _)| p >= from)
            .or(all.first().copied())
    } else {
        let before = editor.doc().anchor.min(editor.doc().cursor);
        all.iter()
            .rev()
            .copied()
            .find(|&(p, _)| p < before)
            .or(all.last().copied())
    };
    match hit {
        Some((p, e)) => {
            let doc = editor.doc_mut();
            doc.anchor = p;
            doc.cursor = e;
            doc.goal_col = None;
        }
        None => {
            let q = editor.last_search.clone();
            editor.set_status(format!("no match: {q}"));
        }
    }
}

/// Add captured text to the register.
///
/// The first capture of a dispatch replaces the register; captures from the
/// other cursors of the same keypress accumulate, newline-separated, so a
/// multi-cursor delete can be pasted back as lines.
fn push_register(editor: &mut Editor, s: &str) {
    let fresh = editor.register_fresh;
    editor.register_fresh = false;
    let buf = match editor.active_register {
        Some(r) => editor.registers.entry(r).or_default(),
        None => &mut editor.register,
    };
    if fresh {
        buf.clear();
    }
    if !buf.is_empty() && !buf.ends_with('\n') {
        buf.push('\n');
    }
    buf.push_str(s);

    // Named registers stay inside crow; the unnamed one is what `c`, `x` and
    // `d` fill, so that is the one worth putting where other apps can see it.
    if editor.active_register.is_none() {
        let text = editor.register.clone();
        to_system_clipboard(&text);
    }
}

/// Mirror the register onto the system clipboard.
///
/// Best effort by design: a machine with none of these tools still has a
/// working editor, and a failed copy is not worth interrupting an edit over.
///
/// ponytail: the platform tool only. Over ssh the clipboard lives on the far
/// side of the terminal and this reaches the wrong machine — add an OSC 52
/// fallback (needs base64) if crow starts getting used that way.
fn to_system_clipboard(text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    const TOOLS: &[(&str, &[&str])] = &[
        ("pbcopy", &[]),
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];

    for (program, args) in TOOLS {
        let Ok(mut child) = Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue; // not installed; try the next one
        };
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        // The pipe is closed by the drop above, so this returns immediately.
        let _ = child.wait();
        return;
    }
}

/// The char range the next action applies to: the selection, or the character
/// under the cursor when nothing is selected.
fn selection_range(doc: &Document) -> (usize, usize) {
    let len = doc.text.len_chars();
    let a = doc.anchor.min(len);
    let c = doc.cursor.min(len);
    if a == c {
        (c, (c + 1).min(len))
    } else {
        (a.min(c), a.max(c))
    }
}

fn delete_selection(editor: &mut Editor) {
    let count = editor.take_count();
    // A bare `d`/`x` with a single cursor never gets here: `handle_key` arms
    // the doubled line op first. With extra cursors it deletes a char each.
    let (from, to) = {
        let doc = editor.doc();
        if doc.anchor == doc.cursor {
            // Nothing selected: delete `count` graphemes, staying on this line.
            let line = doc.cursor_line();
            let mut to = doc.cursor;
            for _ in 0..count {
                to = position::next_grapheme_boundary(doc.text.slice(..), to);
            }
            (doc.cursor, to.min(doc.line_end(line)))
        } else {
            selection_range(doc)
        }
    };
    if from >= to {
        return;
    }
    let text = editor.doc().text.slice(from..to).to_string();
    push_register(editor, &text);
    editor.extend = false;
    let doc = editor.doc_mut();
    doc.delete_range(from, to);
    doc.clamp_cursor(false);
    doc.goal_col = None;
}

fn change_selection(editor: &mut Editor) {
    editor.extend = false;
    let (from, to) = selection_range(editor.doc());
    if from < to {
        let text = editor.doc().text.slice(from..to).to_string();
        push_register(editor, &text);
        editor.doc_mut().delete_range(from, to);
    }
    editor.set_mode(Mode::Insert);
}

fn copy(editor: &mut Editor) {
    editor.extend = false;
    let (from, to) = selection_range(editor.doc());
    if from >= to {
        return;
    }
    let text = editor.doc().text.slice(from..to).to_string();
    push_register(editor, &text);
    // Cursor to the selection start, so `Vcp` duplicates the current line.
    let doc = editor.doc_mut();
    doc.cursor = from;
    doc.goal_col = None;
    editor.set_status(format!("copied {} chars", to - from));
}

/// `cc` — the whole line into the register, cursor left where it was.
fn copy_line(editor: &mut Editor) {
    let at = editor.doc().cursor;
    select_line(editor);
    copy(editor);
    let doc = editor.doc_mut();
    doc.cursor = at.min(doc.text.len_chars());
    doc.anchor = doc.cursor;
}

// ponytail: one unnamed register; named registers when someone misses them.
// A register ending in a newline came from a line selection and pastes linewise.
fn paste_after(editor: &mut Editor) {
    paste(editor, true);
}

fn paste_before(editor: &mut Editor) {
    paste(editor, false);
}

fn paste(editor: &mut Editor, after: bool) {
    let reg = match editor.active_register {
        Some(r) => editor.registers.get(&r).cloned().unwrap_or_default(),
        None => editor.register.clone(),
    };
    if reg.is_empty() {
        return;
    }
    let linewise = reg.ends_with('\n');
    let doc = editor.doc_mut();
    let line = doc.cursor_line();

    let (at, text, cursor_to) = if linewise {
        if after {
            let line_chars = doc.line(line).len_chars(); // includes the newline, if present
            let at = doc.line_start(line) + line_chars;
            if line_chars > doc.line_len(line) {
                (at, reg, at)
            } else {
                // Last line without a trailing newline: the break goes in
                // front of the pasted text instead of behind it.
                let text = format!("\n{}", reg.strip_suffix('\n').unwrap());
                (at, text, at + 1)
            }
        } else {
            let at = doc.line_start(line);
            (at, reg, at)
        }
    } else {
        let at = if after {
            (doc.cursor + 1).min(doc.line_end(line))
        } else {
            doc.cursor
        };
        let end = at + reg.chars().count();
        (at, reg, end.saturating_sub(1))
    };

    let tx = Transaction::insert(&doc.text, at, text);
    doc.apply(tx, cursor_to);
    doc.clamp_cursor(false);
    doc.goal_col = None;
}

fn next_word_start(doc: &Document, mut pos: usize) -> usize {
    let len = doc.text.len_chars();
    if pos >= len {
        return len;
    }
    let class = position::classify(doc.text.char(pos));
    if class != CharClass::Whitespace {
        while pos < len && position::classify(doc.text.char(pos)) == class {
            pos += 1;
        }
    }
    while pos < len && position::classify(doc.text.char(pos)) == CharClass::Whitespace {
        pos += 1;
    }
    pos
}

fn prev_word_start(doc: &Document, mut pos: usize) -> usize {
    if pos == 0 {
        return 0;
    }
    pos -= 1;
    while pos > 0 && position::classify(doc.text.char(pos)) == CharClass::Whitespace {
        pos -= 1;
    }
    let class = position::classify(doc.text.char(pos));
    if class == CharClass::Whitespace {
        return pos;
    }
    while pos > 0 && position::classify(doc.text.char(pos - 1)) == class {
        pos -= 1;
    }
    pos
}

fn word_end(doc: &Document, mut pos: usize) -> usize {
    let len = doc.text.len_chars();
    if len == 0 {
        return 0;
    }
    if pos + 1 >= len {
        return len - 1;
    }
    pos += 1;
    while pos < len && position::classify(doc.text.char(pos)) == CharClass::Whitespace {
        pos += 1;
    }
    if pos >= len {
        return len - 1;
    }
    let class = position::classify(doc.text.char(pos));
    while pos + 1 < len && position::classify(doc.text.char(pos + 1)) == class {
        pos += 1;
    }
    pos
}

fn goto_file_start(editor: &mut Editor) {
    goto_line(editor, 0);
}

fn goto_file_end(editor: &mut Editor) {
    let last = editor.doc().line_count().saturating_sub(1);
    goto_line(editor, last);
}

/// `gg`/`G` go to their end of the file — unless a count names a line, so
/// `42gg` and `42G` both jump to line 42.
fn goto_line(editor: &mut Editor, default: usize) {
    let line = editor
        .count
        .take()
        .map(|n| n.saturating_sub(1))
        .unwrap_or(default);
    let doc = editor.doc_mut();
    doc.cursor = doc.line_start(line.min(doc.line_count().saturating_sub(1)));
    doc.goal_col = None;
}

fn scroll_by(editor: &mut Editor, lines: isize) {
    let past_end = editor.mode == Mode::Insert;
    move_vertical(editor.doc_mut(), lines, past_end);
}

fn half_page_down(editor: &mut Editor) {
    let n = (editor.text_height() / 2).max(1) as isize;
    scroll_by(editor, n);
}

fn half_page_up(editor: &mut Editor) {
    let n = (editor.text_height() / 2).max(1) as isize;
    scroll_by(editor, -n);
}

fn page_down(editor: &mut Editor) {
    let n = editor.text_height().max(1) as isize;
    scroll_by(editor, n);
}

fn page_up(editor: &mut Editor) {
    let n = editor.text_height().max(1) as isize;
    scroll_by(editor, -n);
}

// ---- mode changes ----------------------------------------------------------

fn insert_mode(editor: &mut Editor) {
    editor.set_mode(Mode::Insert);
}

fn insert_at_line_start(editor: &mut Editor) {
    move_line_first_nonblank(editor);
    editor.set_mode(Mode::Insert);
}

fn append(editor: &mut Editor) {
    editor.set_mode(Mode::Insert);
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    if doc.line_len(line) > 0 {
        doc.cursor += 1;
    }
    doc.clamp_cursor(true);
}

fn append_at_line_end(editor: &mut Editor) {
    editor.set_mode(Mode::Insert);
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    doc.cursor = doc.line_end(line);
}

/// Leading whitespace of a line, copied so new lines keep the indent.
fn indent_of(doc: &Document, line: usize) -> String {
    let slice = doc.line(line);
    let len = doc.line_len(line);
    let mut out = String::new();
    for i in 0..len {
        let c = slice.char(i);
        if c == ' ' || c == '\t' {
            out.push(c);
        } else {
            break;
        }
    }
    out
}

fn open_below(editor: &mut Editor) {
    editor.set_mode(Mode::Insert);
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    let indent = indent_of(doc, line);
    let at = doc.line_end(line);
    let text = format!("\n{indent}");
    let tx = Transaction::insert(&doc.text, at, text.clone());
    doc.apply(tx, at + text.chars().count());
}

fn open_above(editor: &mut Editor) {
    editor.set_mode(Mode::Insert);
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    let indent = indent_of(doc, line);
    let at = doc.line_start(line);
    let text = format!("{indent}\n");
    let tx = Transaction::insert(&doc.text, at, text);
    doc.apply(tx, at + indent.chars().count());
}

fn normal_mode(editor: &mut Editor) {
    // Esc while already in normal mode dismisses the extra cursors; Esc out of
    // insert keeps them, so the result of a multi-cursor edit stays visible.
    if editor.mode == Mode::Normal {
        editor.doc_mut().extra.clear();
    }
    editor.extend = false;
    editor.set_mode(Mode::Normal);
}

fn command_mode(editor: &mut Editor) {
    editor.command_line.clear();
    editor.set_mode(Mode::Command);
}

// ---- edits -----------------------------------------------------------------

fn delete_char(editor: &mut Editor) {
    let count = editor.take_count();
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    let end = doc.line_end(line);
    let to = (doc.cursor + count).min(end);
    doc.delete_range(doc.cursor, to);
    doc.clamp_cursor(false);
}

fn delete_line(editor: &mut Editor) {
    let count = editor.take_count();
    let (from, to) = {
        let doc = editor.doc();
        let line = doc.cursor_line();
        let last = doc.line_count().saturating_sub(1);
        let end_line = (line + count - 1).min(last);

        let from = doc.line_start(line);
        let to = if end_line < last {
            doc.line_start(end_line + 1)
        } else {
            // Last line: take the preceding newline instead so no blank line
            // is left.
            doc.text.len_chars()
        };
        let from = if end_line == last && line > 0 {
            doc.line_end(line - 1)
        } else {
            from
        };
        (from, to)
    };
    if from >= to {
        return;
    }
    // Into the register, so `dd p` moves a line like vim.
    let text = editor.doc().text.slice(from..to).to_string();
    push_register(editor, &text);
    let doc = editor.doc_mut();
    doc.delete_range(from, to);
    doc.clamp_cursor(false);
    doc.goal_col = None;
}

fn delete_to_line_end(editor: &mut Editor) {
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    let end = doc.line_end(line);
    doc.delete_range(doc.cursor, end);
    doc.clamp_cursor(false);
}

fn join_lines(editor: &mut Editor) {
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    if line + 1 >= doc.line_count() {
        return;
    }

    let end = doc.line_end(line);
    let next_start = doc.line_start(line + 1);
    let next_len = doc.line_len(line + 1);
    let next_slice = doc.line(line + 1);

    let mut skip = 0;
    while skip < next_len && next_slice.char(skip).is_whitespace() {
        skip += 1;
    }

    let separator = if next_len == skip { "" } else { " " };
    let tx = Transaction::change(
        &doc.text,
        [(end, next_start + skip, Some(separator.to_string()))],
    );
    doc.apply(tx, end);
    doc.clamp_cursor(false);
}

/// Pipe the buffer through the extension's formatter (config `[fmt]` or the
/// built-in table) and replace it with the output. `None` means no file or no
/// formatter for it; `Some(Err)` carries the failure, buffer untouched.
///
/// ponytail: blocks the editor for the formatter's runtime; they're fast.
fn run_formatter(editor: &mut Editor) -> Option<Result<&'static str, String>> {
    use std::io::Write as _;
    use std::process::{Command as Proc, Stdio};

    let path = editor.doc().path.clone()?;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let command = crate::config::formatter(ext)?;
    // A `{file}` command was handed the path, so it may rewrite the file itself
    // (`rustfmt {file}`) instead of filtering stdin. That is still our write —
    // re-stamp below so the save guard does not read it as an external edit.
    let in_place = command.contains("{file}");
    // `{tmp}`: a formatter that rewrites the file it is given rather than
    // filtering stdin (`oxigen fmt`) gets a copy of the *buffer* — on :w the
    // real file is still the unformatted version, and may not exist yet.
    let tmp = command
        .contains("{tmp}")
        .then(|| std::env::temp_dir().join(format!("crow-fmt-{}.{ext}", std::process::id())));
    let src = editor.doc().text.to_string();
    if let Some(tmp) = &tmp {
        if let Err(e) = std::fs::write(tmp, &src) {
            return Some(Err(format!("{}: {e}", tmp.display())));
        }
    }
    let command = command
        .replace("{file}", &path.to_string_lossy())
        .replace("{tmp}", &tmp.as_deref().unwrap_or(&path).to_string_lossy());
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or("");

    let spawned = Proc::new(program)
        .args(words)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => {
            if let Some(tmp) = &tmp {
                let _ = std::fs::remove_file(tmp);
            }
            if e.kind() == std::io::ErrorKind::NotFound {
                if editor.offer_install(program) {
                    return None; // the status line is now the install prompt
                }
                return Some(Err(format!("{program}: not installed")));
            }
            return Some(Err(format!("{program}: {e}")));
        }
    };
    let _ = child.stdin.take().unwrap().write_all(src.as_bytes());
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            if let Some(tmp) = &tmp {
                let _ = std::fs::remove_file(tmp);
            }
            return Some(Err(format!("{program}: {e}")));
        }
    };
    // Read the copy back before the status check: a formatter that exits
    // non-zero still leaves its temp file behind.
    let rewritten = tmp.map(|tmp| {
        let text = std::fs::read_to_string(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);
        text
    });
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("failed");
        return Some(Err(format!("{program}: {first}")));
    }

    if in_place {
        editor.doc_mut().restamp();
    }
    let new = rewritten.unwrap_or_else(|| String::from_utf8_lossy(&out.stdout).into_owned());
    if new.is_empty() || new == src {
        return Some(Ok("already formatted"));
    }
    let new_len = new.chars().count();
    let doc = editor.doc_mut();
    let cursor = doc.cursor;
    let tx = Transaction::change(&doc.text, [(0, doc.text.len_chars(), Some(new))]);
    doc.apply(tx, cursor.min(new_len));
    doc.anchor = doc.cursor;
    doc.extra.clear();
    doc.clamp_cursor(false);
    Some(Ok("formatted"))
}

fn format_buffer(editor: &mut Editor) {
    match run_formatter(editor) {
        Some(Ok(what)) => editor.set_status(what),
        Some(Err(e)) => editor.set_status(e),
        // An armed install offer already owns the status line.
        None if editor.pending_install.is_some() => {}
        None => editor.set_status("no formatter for this buffer (see [fmt] in crow.toml)"),
    }
}

/// Rewrite the version on the cursor line of a package manifest to the
/// latest its registry reported (the ↑ badge).
fn dep_upgrade(editor: &mut Editor) {
    let kind = editor
        .doc()
        .path
        .as_ref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .and_then(crate::deps::manifest_kind);
    let Some(kind) = kind else {
        editor.set_status("not a package manifest");
        return;
    };
    let line_idx = editor.doc().cursor_line();
    let line: String = editor.doc().line(line_idx).chars().collect();
    let Some(name) = crate::deps::line_dep(kind, &line) else {
        editor.set_status("no dependency on this line");
        return;
    };
    let latest = editor
        .dep_info
        .get(&(kind, name.clone()))
        .and_then(|(_, latest)| latest.clone());
    let Some(latest) = latest else {
        editor.set_status(format!("no version info for {name} (still fetching?)"));
        return;
    };
    let Some((from, to)) = crate::deps::version_span(kind, &line) else {
        editor.set_status("no version number on this line to rewrite");
        return;
    };
    let old: String = line.chars().skip(from).take(to - from).collect();
    if old == latest {
        editor.set_status(format!("{name} is already {latest}"));
        return;
    }
    let doc = editor.doc_mut();
    let start = doc.line_start(line_idx);
    let tx = Transaction::change(
        &doc.text,
        [(start + from, start + to, Some(latest.clone()))],
    );
    let cursor = doc.cursor;
    doc.apply(tx, cursor);
    doc.clamp_cursor(false);
    editor.set_status(format!("{name}: {old} → {latest} (reinstall to apply)"));
}

fn undo(editor: &mut Editor) {
    if !editor.doc_mut().undo() {
        editor.set_status("Already at oldest change");
    }
    editor.doc_mut().clamp_cursor(false);
}

fn redo(editor: &mut Editor) {
    if !editor.doc_mut().redo() {
        editor.set_status("Already at newest change");
    }
    editor.doc_mut().clamp_cursor(false);
}

// ---- insert mode -----------------------------------------------------------

/// One level of indentation, matching the style already on the line:
/// tabs if the line's indent has tabs, else `tab_width` spaces.
pub fn indent_unit(indent: &str) -> String {
    if indent.contains('\t') {
        "\t".to_string()
    } else {
        " ".repeat(tab_width())
    }
}

fn insert_newline(editor: &mut Editor) {
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    let indent = indent_of(doc, line);
    // Don't carry indentation past the cursor if the cursor is inside it.
    let (_, col) = doc.cursor_line_col();
    let indent: String = indent.chars().take(col).collect();

    let prev = (doc.cursor > 0).then(|| doc.text.char(doc.cursor - 1));
    let next = (doc.cursor < doc.text.len_chars()).then(|| doc.text.char(doc.cursor));
    let opener = matches!(prev, Some('{') | Some('(') | Some('['));
    let closes = matches!(
        (prev, next),
        (Some('{'), Some('}')) | (Some('('), Some(')')) | (Some('['), Some(']'))
    );

    // ponytail: brace-aware indent only for the primary cursor; with extra
    // cursors every cursor gets the plain newline+indent.
    if opener && doc.extra.is_empty() {
        let unit = indent_unit(&indent);
        if closes {
            // `{|}` + Enter -> the closer moves to its own line and the
            // cursor lands on the indented one between.
            let at = doc.cursor;
            let text = format!("\n{indent}{unit}\n{indent}");
            let tx = Transaction::insert(&doc.text, at, text);
            doc.apply(tx, at + 1 + indent.chars().count() + unit.chars().count());
        } else {
            doc.insert_at_cursor(&format!("\n{indent}{unit}"));
        }
    } else {
        doc.insert_at_cursor(&format!("\n{indent}"));
    }
}

fn insert_tab(editor: &mut Editor) {
    let doc = editor.doc_mut();
    let indent = indent_of(doc, doc.cursor_line());
    let unit = indent_unit(&indent);
    doc.insert_at_cursor(&unit);
}

fn delete_backward(editor: &mut Editor) {
    let doc = editor.doc_mut();
    if doc.cursor == 0 {
        return;
    }
    let from = position::prev_grapheme_boundary(doc.text.slice(..), doc.cursor);
    // Backspacing an opener eats its auto-closed partner too.
    let prev = doc.text.char(doc.cursor - 1);
    let next = (doc.cursor < doc.text.len_chars()).then(|| doc.text.char(doc.cursor));
    let empty_pair = matches!(
        (prev, next),
        ('(', Some(')'))
            | ('[', Some(']'))
            | ('{', Some('}'))
            | ('"', Some('"'))
            | ('\'', Some('\''))
    );
    let to = if empty_pair && crate::config::autoclose() {
        doc.cursor + 1
    } else {
        doc.cursor
    };
    doc.delete_range(from, to);
}

fn delete_forward(editor: &mut Editor) {
    let doc = editor.doc_mut();
    if doc.cursor >= doc.text.len_chars() {
        return;
    }
    let to = position::next_grapheme_boundary(doc.text.slice(..), doc.cursor);
    doc.delete_range(doc.cursor, to);
}

// ---- buffers and lifecycle -------------------------------------------------

fn next_buffer(editor: &mut Editor) {
    editor.current = (editor.current + 1) % editor.documents.len();
}

fn prev_buffer(editor: &mut Editor) {
    editor.current = (editor.current + editor.documents.len() - 1) % editor.documents.len();
}

fn split_vertical(editor: &mut Editor) {
    editor.split_window(true);
}

fn split_horizontal(editor: &mut Editor) {
    editor.split_window(false);
}

fn next_window(editor: &mut Editor) {
    editor.focus_next_window();
}

fn save(editor: &mut Editor) {
    save_with(editor, false);
}

/// `force` waives the external-modification guard — that is `:w!`, which cannot
/// be a registry entry because command names have to be Rust idents.
pub fn save_with(editor: &mut Editor, force: bool) {
    // Format first; a broken formatter never blocks the write.
    let fmt_err = match crate::config::format_on_save().then(|| run_formatter(editor)) {
        Some(Some(Err(e))) => Some(e),
        // A missing formatter armed the install offer: carry its prompt into
        // the "written" message so the (y/N) stays visible.
        Some(None) if editor.pending_install.is_some() => Some(editor.status.clone()),
        _ => None,
    };
    match editor.doc_mut().save(force) {
        Ok(()) => {
            let name = editor.doc().name();
            let lines = editor.doc().line_count();
            match fmt_err {
                Some(e) => editor.set_status(format!("\"{name}\" {lines}L written ({e})")),
                None => editor.set_status(format!("\"{name}\" {lines}L written")),
            }
        }
        Err(e) => editor.set_status(format!("Error: {e}")),
    }
}

fn quit(editor: &mut Editor) {
    // Closing a window never loses data — the buffer stays open — so only the
    // last window checks for unsaved changes.
    if editor.window_count() > 1 {
        editor.close_focused_window();
        return;
    }
    if editor.doc().modified {
        editor.set_status("Unsaved changes — use :q! to discard, or :w to write");
        return;
    }
    editor.should_quit = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::tests::{editor_with, press};

    #[test]
    fn vertical_motion_follows_the_display_column_not_the_char_index() {
        // A tab is one character but four columns wide: `j` off a tab-indented
        // line has to land under where the cursor looked, not under char 2.
        let mut editor = editor_with("\tabc\nabcdefgh");
        editor.doc_mut().cursor = 2; // 'b', display column 5
        press(&mut editor, "j");
        assert_eq!(editor.doc().cursor_line_col(), (1, 5));
        press(&mut editor, "k");
        assert_eq!(editor.doc().cursor, 2);
    }

    #[test]
    fn the_goal_column_survives_a_short_line_and_a_count() {
        // Crossing a short line clamps the cursor but must not forget where the
        // column was, or coming back lands in the wrong place.
        let mut editor = editor_with("abcdefgh\nxy\nabcdefgh\nq");
        press(&mut editor, "$ 3j");
        assert_eq!(editor.doc().cursor_line_col(), (3, 0));
        press(&mut editor, "3k");
        assert_eq!(editor.doc().cursor_line_col(), (0, 7));
    }

    #[test]
    fn copy_gathers_every_cursor_and_rewinds_to_the_selection_start() {
        // The extra cursors run before the primary, and the register gets no
        // blank line between two line selections — that is what makes a
        // multi-cursor copy paste back as clean lines.
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "C V c");
        assert_eq!(editor.register, "two\none\n");
        assert_eq!(editor.doc().cursor, 0); // rewound, so `Vcp` duplicates
        assert!(!editor.extend);
    }

    #[test]
    fn a_second_copy_replaces_the_register() {
        // cc takes the line with its newline; on the last line there is none.
        let mut editor = editor_with("abc\ndef");
        press(&mut editor, "cc");
        assert_eq!(editor.register, "abc\n");
        assert_eq!(editor.doc().cursor, 0); // cc leaves the cursor alone
        press(&mut editor, "j cc");
        assert_eq!(editor.register, "def");
        assert_eq!(editor.status, "copied 3 chars");
        // And a selection copy replaces it in turn.
        press(&mut editor, "vlc");
        assert_eq!(editor.register, "d");
    }

    #[test]
    fn dd_on_the_last_line_takes_the_newline_in_front_of_it() {
        // Every other line takes its trailing newline; the last one has none,
        // so it takes the preceding one instead or leaves a blank line behind.
        let mut editor = editor_with("one\ntwo");
        press(&mut editor, "j dd");
        assert_eq!(editor.doc().text.to_string(), "one");
        assert_eq!(editor.register, "\ntwo");
        press(&mut editor, "p");
        assert_eq!(editor.doc().text.to_string(), "one\ntwo"); // and it round-trips
                                                               // A count past the end of the buffer clamps rather than panicking.
        let mut editor = editor_with("a\nb\nc\n");
        press(&mut editor, "j 5dd");
        assert_eq!(editor.doc().text.to_string(), "a");
    }

    #[test]
    fn help_shows_the_doubled_line_keys() {
        // They are armed by character, not bound in the trie, so the keys
        // column has to come from LINE_OPS or it renders blank.
        let editor = editor_with("");
        let lines = help_lines(&editor.keymaps.normal);
        let keys = |want: &str| {
            lines
                .iter()
                .find_map(|l| match l {
                    HelpLine::Entry { keys, name, .. } if name == want => Some(keys.clone()),
                    _ => None,
                })
                .unwrap()
        };
        assert_eq!(keys("delete_line"), "dd  xx");
        assert_eq!(keys("copy_line"), "cc");
        assert_eq!(keys("select_line"), "V");
        assert_eq!(keys("copy"), "c");
    }

    /// Ignored by default: it writes the real clipboard, and clobbering what
    /// the user had copied to run the test suite is rude. `cargo test --
    /// --ignored system_clipboard` when touching `to_system_clipboard`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn system_clipboard_gets_what_was_copied() {
        let mut editor = editor_with("hello\nworld");
        press(&mut editor, "cc");
        let out = std::process::Command::new("pbpaste").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
        // A named register stays inside crow.
        press(&mut editor, "j \"ax");
        let out = std::process::Command::new("pbpaste").output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "hello\n");
    }

    #[test]
    fn xx_cuts_a_line_and_x_cuts_the_selection() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "xx");
        assert_eq!(editor.doc().text.to_string(), "two\nthree");
        assert_eq!(editor.register, "one\n");
        // A pending x that is not doubled cancels; the next key runs normally.
        press(&mut editor, "xj");
        assert_eq!(editor.doc().text.to_string(), "two\nthree");
        assert_eq!(editor.doc().cursor_line(), 1);
        // With a selection, a single x cuts it.
        press(&mut editor, "Vx");
        assert_eq!(editor.doc().text.to_string(), "two\n");
        assert_eq!(editor.register, "three");
    }

    #[test]
    fn paste_puts_the_text_where_the_register_says() {
        // Linewise on the last line: the break goes in front of the pasted
        // text, because there is no trailing newline to paste behind.
        let mut editor = editor_with("ab");
        editor.register = "X\n".into();
        press(&mut editor, "p");
        assert_eq!(editor.doc().text.to_string(), "ab\nX");
        assert_eq!(editor.doc().cursor, 3);
        // Charwise after the last character of a line lands before the newline.
        let mut editor = editor_with("ab\ncd");
        editor.register = "XY".into();
        press(&mut editor, "$ p");
        assert_eq!(editor.doc().text.to_string(), "abXY\ncd");
        assert_eq!(editor.doc().cursor, 3); // on the last pasted char
    }

    #[test]
    fn join_lines_eats_the_indent_and_stops_at_the_last_line() {
        let mut editor = editor_with("foo\n    bar\nbaz");
        press(&mut editor, "J");
        assert_eq!(editor.doc().text.to_string(), "foo bar\nbaz");
        assert_eq!(editor.doc().cursor, 3); // on the joining space
                                            // A blank line joins with nothing between, not with a stray space.
        let mut editor = editor_with("foo\n\nbar");
        press(&mut editor, "J");
        assert_eq!(editor.doc().text.to_string(), "foo\nbar");
        let mut editor = editor_with("only");
        press(&mut editor, "J");
        assert_eq!(editor.doc().text.to_string(), "only");
    }

    #[test]
    fn dep_upgrade_rewrites_the_version_and_nothing_else() {
        // Each ecosystem writes its version differently, and only the digits
        // are ours to touch: go.mod keeps its `v`, npm keeps its caret, and a
        // Cargo inline table keeps everything but the number.
        let mut editor = editor_with("module x\n\nrequire golang.org/x/tools v0.29.0\n");
        editor.doc_mut().path = Some("go.mod".into());
        editor.dep_info.insert(
            (crate::deps::Kind::Go, "golang.org/x/tools".into()),
            (Some("0.29.0".into()), Some("0.30.0".into())),
        );
        press(&mut editor, "jj");
        (find("dep_upgrade").unwrap().func)(&mut editor);
        assert_eq!(
            editor.doc().text.to_string(),
            "module x\n\nrequire golang.org/x/tools v0.30.0\n"
        );

        let mut editor =
            editor_with("{\n  \"dependencies\": {\n    \"react\": \"^18.2.0\"\n  }\n}\n");
        editor.doc_mut().path = Some("package.json".into());
        editor.dep_info.insert(
            (crate::deps::Kind::Npm, "react".into()),
            (Some("18.2.0".into()), Some("19.0.0".into())),
        );
        press(&mut editor, "jj");
        (find("dep_upgrade").unwrap().func)(&mut editor);
        assert_eq!(
            editor.doc().text.to_string(),
            "{\n  \"dependencies\": {\n    \"react\": \"^19.0.0\"\n  }\n}\n"
        );

        let mut editor = editor_with(
            "[dependencies]\ncrossterm = { version = \"0.27.0\", features = [\"event-stream\"] }\n",
        );
        editor.doc_mut().path = Some("Cargo.toml".into());
        editor.dep_info.insert(
            (crate::deps::Kind::Cargo, "crossterm".into()),
            (Some("0.27.0".into()), Some("0.29.0".into())),
        );
        press(&mut editor, "j");
        (find("dep_upgrade").unwrap().func)(&mut editor);
        assert_eq!(
            editor.doc().text.to_string(),
            "[dependencies]\ncrossterm = { version = \"0.29.0\", features = [\"event-stream\"] }\n"
        );
    }

    #[test]
    fn dep_upgrade_refuses_without_touching_the_buffer() {
        // Every refusal is silent apart from the status line, so a wrong guard
        // would show up as a corrupted manifest rather than an error.
        let before = "[dependencies]\nropey = \"1.6.0\"\n";
        let mut editor = editor_with(before);
        (find("dep_upgrade").unwrap().func)(&mut editor);
        assert_eq!(editor.status, "not a package manifest");

        editor.doc_mut().path = Some("Cargo.toml".into());
        (find("dep_upgrade").unwrap().func)(&mut editor);
        assert_eq!(editor.status, "no dependency on this line"); // the [dependencies] header

        press(&mut editor, "j");
        (find("dep_upgrade").unwrap().func)(&mut editor);
        assert_eq!(editor.status, "no version info for ropey (still fetching?)");

        editor.dep_info.insert(
            (crate::deps::Kind::Cargo, "ropey".into()),
            (Some("1.6.0".into()), Some("1.6.0".into())),
        );
        (find("dep_upgrade").unwrap().func)(&mut editor);
        assert_eq!(editor.status, "ropey is already 1.6.0");
        assert_eq!(editor.doc().text.to_string(), before);
    }

    #[test]
    fn format_buffer_says_so_when_there_is_no_formatter() {
        // The two `None` paths: no file at all, and a file whose extension no
        // formatter claims. Neither is an error and neither touches the buffer.
        let mut editor = editor_with("x");
        (find("format_buffer").unwrap().func)(&mut editor);
        assert_eq!(
            editor.status,
            "no formatter for this buffer (see [fmt] in crow.toml)"
        );
        editor.doc_mut().path = Some("notes.zzz".into());
        editor.set_status("");
        (find("format_buffer").unwrap().func)(&mut editor);
        assert_eq!(
            editor.status,
            "no formatter for this buffer (see [fmt] in crow.toml)"
        );
        assert_eq!(editor.doc().text.to_string(), "x");
    }

    /// A `{tmp}` formatter (`oxigen fmt`) rewrites the file it is handed and
    /// prints something else on stdout. The new buffer has to be read back
    /// from that file — taking stdout would replace the code with a message.
    #[cfg(unix)]
    #[test]
    fn a_tmp_formatter_is_read_back_from_its_file() {
        use std::os::unix::fs::PermissionsExt;

        // The `[fmt]` overrides are global; this is the lock every test that
        // swaps them shares.
        let _guard = crate::theme::TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir();
        let script = dir.join("crow-tmp-fmt-test.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\ntr a-z A-Z < \"$1\" > \"$1.up\" && mv \"$1.up\" \"$1\"\necho 'Formatted 1 file'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::config::apply(&crate::config::Config {
            fmt: vec![("crowtest".into(), format!("{} {{tmp}}", script.display()))],
            ..crate::config::Config::default()
        });

        let mut editor = editor_with("hello");
        editor.doc_mut().path = Some(dir.join("never-written.crowtest"));
        (find("format_buffer").unwrap().func)(&mut editor);
        assert_eq!(editor.status, "formatted");
        assert_eq!(editor.doc().text.to_string(), "HELLO");
        let copy = dir.join(format!("crow-fmt-{}.crowtest", std::process::id()));
        assert!(!copy.exists(), "the temp copy outlived the format");
        let _ = std::fs::remove_file(&script);
    }

    #[test]
    fn bare_d_with_extra_cursors_deletes_a_char_each_instead_of_arming_dd() {
        let mut editor = editor_with("abc\ndef");
        press(&mut editor, "C d");
        assert_eq!(editor.doc().text.to_string(), "bc\nef");
        assert_eq!(editor.register, "d\na");
        assert!(editor.pending_line_op.is_none());
        // The count stops at the end of each cursor's own line.
        let mut editor = editor_with("ab\ncd");
        press(&mut editor, "C 5d");
        assert_eq!(editor.doc().text.to_string(), "\n");
    }

    #[test]
    fn word_motions_stop_at_punctuation_and_the_buffer_edges() {
        let mut doc = Document::empty();
        doc.text = ropey::Rope::from_str("foo.bar baz");
        assert_eq!(
            (
                next_word_start(&doc, 0),
                next_word_start(&doc, 3),
                next_word_start(&doc, 4)
            ),
            (3, 4, 8)
        );
        assert_eq!(
            (
                prev_word_start(&doc, 8),
                prev_word_start(&doc, 4),
                prev_word_start(&doc, 0)
            ),
            (4, 3, 0)
        );
        assert_eq!(
            (word_end(&doc, 0), word_end(&doc, 2), word_end(&doc, 10)),
            (2, 3, 10)
        );
        // `e` on the last word of the buffer still takes its final character.
        let mut editor = editor_with("foo");
        press(&mut editor, "ed");
        assert_eq!(editor.doc().text.to_string(), "");
    }
}
