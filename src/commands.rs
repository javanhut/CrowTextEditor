//! Every editor action is a named value, not an arm of a `match`.
//!
//! Commands are `&'static` so a keymap can hold references to them without
//! borrowing the editor, and so bindings can be looked up by name — which is
//! what a config file will need.

use crate::document::Document;
use crate::editor::{Editor, Mode};
use crate::config::tab_width;
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
    hover => "show type and docs for the symbol under the cursor (LSP)",
    complete => "open the completion menu (LSP)",
    command_palette => "fuzzy-pick any command by name",
    find_files => "fuzzy-find a file under the current directory",
    file_explorer => "browse the current directory in a picker",
    tree_toggle => "toggle the file tree sidebar",
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
    yank => "copy the selection to the register",
    paste_after => "paste the register after the cursor",
    paste_before => "paste the register before the cursor",
    delete_char => "delete the character under the cursor",
    delete_line => "delete the current line",
    delete_to_line_end => "delete to the end of the line",
    join_lines => "join this line with the next",
    undo => "undo the last change",
    redo => "redo the last undone change",

    insert_newline => "insert a line break",
    insert_tab => "insert a tab",
    delete_backward => "delete the character before the cursor",
    delete_forward => "delete the character under the cursor",

    format_buffer => "run the file's formatter over the buffer (:fmt)",
    next_buffer => "switch to the next buffer",
    prev_buffer => "switch to the previous buffer",
    split_vertical => "split the window side by side",
    split_horizontal => "split the window stacked",
    next_window => "focus the next window",
    save => "write the buffer to disk",
    quit => "close the window, or the editor with the last one",
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
    "yank",
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
        doc.syntax.as_ref().map(|syn| {
            let b0 = doc.text.char_to_byte(from);
            let b1 = doc.text.char_to_byte(to);
            let mut node = syn.tree.root_node().descendant_for_byte_range(b0, b1)?;
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
            doc.line_start(target) + position::display_col_to_char(doc.line(target), col, tab_width())
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
    editor.open_picker(crate::picker::Picker::commands());
}

fn theme_picker(editor: &mut Editor) {
    editor.open_picker(crate::picker::Picker::themes());
}

fn find_files(editor: &mut Editor) {
    let root = std::env::current_dir().unwrap_or_default();
    editor.open_picker(crate::picker::Picker::files(&root));
}

fn file_explorer(editor: &mut Editor) {
    let dir = std::env::current_dir().unwrap_or_default();
    editor.open_picker(crate::picker::Picker::explorer(dir));
}

fn tree_toggle(editor: &mut Editor) {
    editor.tree_toggle();
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
    match editor.lsp.as_mut() {
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

fn yank(editor: &mut Editor) {
    editor.extend = false;
    let (from, to) = selection_range(editor.doc());
    if from >= to {
        return;
    }
    let text = editor.doc().text.slice(from..to).to_string();
    push_register(editor, &text);
    // Cursor to the selection start, so `xyp` duplicates the current line.
    let doc = editor.doc_mut();
    doc.cursor = from;
    doc.goal_col = None;
    editor.set_status(format!("yanked {} chars", to - from));
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
    let line = editor.count.take().map(|n| n.saturating_sub(1)).unwrap_or(default);
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
    let doc = editor.doc_mut();
    let line = doc.cursor_line();
    let last = doc.line_count().saturating_sub(1);
    let end_line = (line + count - 1).min(last);

    let from = doc.line_start(line);
    let to = if end_line < last {
        doc.line_start(end_line + 1)
    } else {
        // Last line: take the preceding newline instead so no blank line is left.
        doc.text.len_chars()
    };
    let from = if end_line == last && line > 0 {
        doc.line_end(line - 1)
    } else {
        from
    };

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
    let command = command.replace("{file}", &path.to_string_lossy());
    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or("");

    let src = editor.doc().text.to_string();
    let spawned = Proc::new(program)
        .args(words)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return Some(Err(format!("{program}: {e}"))),
    };
    let _ = child.stdin.take().unwrap().write_all(src.as_bytes());
    let out = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return Some(Err(format!("{program}: {e}"))),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("failed");
        return Some(Err(format!("{program}: {first}")));
    }

    let new = String::from_utf8_lossy(&out.stdout).into_owned();
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
        None => editor.set_status("no formatter for this buffer (see [fmt] in crow.toml)"),
    }
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
        ('(', Some(')')) | ('[', Some(']')) | ('{', Some('}')) | ('"', Some('"')) | ('\'', Some('\''))
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
    // Format first; a broken formatter never blocks the write.
    let fmt_err = match crate::config::format_on_save().then(|| run_formatter(editor)) {
        Some(Some(Err(e))) => Some(e),
        _ => None,
    };
    match editor.doc_mut().save() {
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
