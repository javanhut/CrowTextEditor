//! Conversions between the several different things "column" can mean.
//!
//! There are at least four position metrics in play in any text editor:
//!
//!   - **byte offsets** — what files and `String` use
//!   - **char offsets** — what ropey indexes by; the canonical metric here
//!   - **display columns** — what the terminal draws, after tab expansion and
//!     accounting for wide (CJK, emoji) and zero-width (combining) characters
//!   - **UTF-16 code units** — what the Language Server Protocol speaks
//!
//! These agree only for pure ASCII. The rule in this codebase: **char offsets
//! are canonical**, conversions happen at the edges (rendering, LSP), and no
//! bare integer crosses a module boundary without its metric being obvious from
//! the name.

use ropey::RopeSlice;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};
use unicode_width::UnicodeWidthChar;

/// Columns a tab advances to the next stop.
pub const TAB_WIDTH: usize = 4;

/// Display width of a single character at a given display column.
///
/// Tabs depend on where they start, which is why this takes the current column.
pub fn char_width(c: char, at_col: usize, tab_width: usize) -> usize {
    match c {
        '\t' => tab_width - (at_col % tab_width),
        '\n' | '\r' => 0,
        _ => UnicodeWidthChar::width(c).unwrap_or(0),
    }
}

/// Convert a char offset within a line to the display column it renders at.
pub fn char_to_display_col(line: RopeSlice, char_offset: usize, tab_width: usize) -> usize {
    let mut col = 0usize;
    for (i, c) in line.chars().enumerate() {
        if i >= char_offset {
            break;
        }
        col += char_width(c, col, tab_width);
    }
    col
}

/// Convert a display column to the nearest char offset within a line.
///
/// If the column falls inside a wide character or a tab, this returns the
/// offset of that character rather than splitting it.
pub fn display_col_to_char(line: RopeSlice, target_col: usize, tab_width: usize) -> usize {
    let mut col = 0usize;
    for (i, c) in line.chars().enumerate() {
        if c == '\n' || c == '\r' {
            return i;
        }
        let w = char_width(c, col, tab_width);
        if col + w > target_col {
            return i;
        }
        col += w;
    }
    line_len_without_newline(line)
}

/// Total display width of a line, excluding its line ending.
pub fn line_display_width(line: RopeSlice, tab_width: usize) -> usize {
    let mut col = 0usize;
    for c in line.chars() {
        if c == '\n' || c == '\r' {
            break;
        }
        col += char_width(c, col, tab_width);
    }
    col
}

/// Length of a line in chars, not counting `\n` or `\r\n`.
pub fn line_len_without_newline(line: RopeSlice) -> usize {
    let mut len = line.len_chars();
    if len > 0 && line.char(len - 1) == '\n' {
        len -= 1;
        if len > 0 && line.char(len - 1) == '\r' {
            len -= 1;
        }
    }
    len
}

/// Next grapheme-cluster boundary after `char_idx`, so the cursor never lands
/// inside an emoji ZWJ sequence, a flag, or a combining stack.
///
/// The standard rope-chunk-feeding dance: `GraphemeCursor` works in bytes over
/// string chunks, so we translate at the edges.
pub fn next_grapheme_boundary(slice: RopeSlice, char_idx: usize) -> usize {
    let byte_idx = slice.char_to_byte(char_idx.min(slice.len_chars()));
    let mut gc = GraphemeCursor::new(byte_idx, slice.len_bytes(), true);
    let (mut chunk, mut chunk_start, _, _) = slice.chunk_at_byte(byte_idx);
    loop {
        match gc.next_boundary(chunk, chunk_start) {
            Ok(None) => return slice.len_chars(),
            Ok(Some(b)) => return slice.byte_to_char(b),
            Err(GraphemeIncomplete::NextChunk) => {
                chunk_start += chunk.len();
                chunk = slice.chunk_at_byte(chunk_start).0;
            }
            Err(GraphemeIncomplete::PreContext(b)) => {
                let (ctx, ctx_start, _, _) = slice.chunk_at_byte(b - 1);
                gc.provide_context(ctx, ctx_start);
            }
            _ => unreachable!(),
        }
    }
}

/// Previous grapheme-cluster boundary before `char_idx`.
pub fn prev_grapheme_boundary(slice: RopeSlice, char_idx: usize) -> usize {
    let byte_idx = slice.char_to_byte(char_idx.min(slice.len_chars()));
    let mut gc = GraphemeCursor::new(byte_idx, slice.len_bytes(), true);
    let (mut chunk, mut chunk_start, _, _) = slice.chunk_at_byte(byte_idx);
    loop {
        match gc.prev_boundary(chunk, chunk_start) {
            Ok(None) => return 0,
            Ok(Some(b)) => return slice.byte_to_char(b),
            Err(GraphemeIncomplete::PrevChunk) => {
                let (c, s, _, _) = slice.chunk_at_byte(chunk_start - 1);
                chunk = c;
                chunk_start = s;
            }
            Err(GraphemeIncomplete::PreContext(b)) => {
                let (ctx, ctx_start, _, _) = slice.chunk_at_byte(b - 1);
                gc.provide_context(ctx, ctx_start);
            }
            _ => unreachable!(),
        }
    }
}

/// Classification used by word motions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass {
    Whitespace,
    Word,
    Punctuation,
}

pub fn classify(c: char) -> CharClass {
    if c.is_whitespace() {
        CharClass::Whitespace
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punctuation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ropey::Rope;

    #[test]
    fn tabs_advance_to_the_next_stop() {
        let rope = Rope::from_str("\tx");
        let line = rope.line(0);
        assert_eq!(char_to_display_col(line, 1, 4), 4);
        assert_eq!(char_to_display_col(line, 2, 4), 5);
    }

    #[test]
    fn tab_width_depends_on_starting_column() {
        // "ab\t" — the tab starts at column 2 and advances only 2 columns.
        let rope = Rope::from_str("ab\tx");
        let line = rope.line(0);
        assert_eq!(char_to_display_col(line, 3, 4), 4);
    }

    #[test]
    fn wide_chars_take_two_columns() {
        let rope = Rope::from_str("日本語");
        let line = rope.line(0);
        assert_eq!(line_display_width(line, 4), 6);
        assert_eq!(char_to_display_col(line, 2, 4), 4);
    }

    #[test]
    fn combining_marks_are_zero_width() {
        // "e" followed by U+0301 COMBINING ACUTE ACCENT renders as one column.
        let rope = Rope::from_str("e\u{0301}x");
        let line = rope.line(0);
        assert_eq!(line_display_width(line, 4), 2);
    }

    #[test]
    fn display_col_roundtrips_through_char_offset() {
        let rope = Rope::from_str("日本語abc");
        let line = rope.line(0);
        for offset in 0..6 {
            let col = char_to_display_col(line, offset, 4);
            assert_eq!(display_col_to_char(line, col, 4), offset);
        }
    }

    #[test]
    fn grapheme_boundaries_keep_clusters_whole() {
        // A ZWJ family emoji: 👨 ZWJ 👩 ZWJ 👧 — seven chars, one grapheme.
        let rope = Rope::from_str("a👨\u{200d}👩\u{200d}👧b");
        let s = rope.slice(..);
        assert_eq!(next_grapheme_boundary(s, 0), 1);
        assert_eq!(next_grapheme_boundary(s, 1), 6); // skips the whole family
        assert_eq!(prev_grapheme_boundary(s, 6), 1);
        assert_eq!(prev_grapheme_boundary(s, 7), 6);
    }

    #[test]
    fn newline_is_not_counted_in_line_length() {
        let rope = Rope::from_str("abc\ndef");
        assert_eq!(line_len_without_newline(rope.line(0)), 3);
        assert_eq!(line_len_without_newline(rope.line(1)), 3);
    }
}
