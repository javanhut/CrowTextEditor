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

/// Display width of the chars `from..to` of a line, measured as if the row
/// started at column 0 — which is what a soft-wrapped row does.
pub fn display_col_between(line: RopeSlice, from: usize, to: usize, tab_width: usize) -> usize {
    let mut col = 0usize;
    for c in line.chars_at(from).take(to.saturating_sub(from)) {
        col += char_width(c, col, tab_width);
    }
    col
}

/// Char offsets within `line` where each soft-wrapped visual row begins.
///
/// Always starts with `0`, so a line that fits returns `[0]` and the count is
/// the number of screen rows the line takes. Breaks after the last space that
/// fits, and mid-word only when one word is wider than the window.
pub fn wrap_offsets(line: RopeSlice, width: usize, tab_width: usize) -> Vec<usize> {
    let mut offsets = vec![0usize];
    if width == 0 {
        return offsets;
    }
    let chars: Vec<char> = line.chars().take(line_len_without_newline(line)).collect();
    let mut col = 0usize;
    let mut row_start = 0usize;
    // Offset just past the last space seen on this row — where a break would
    // land without splitting a word.
    let mut last_space: Option<usize> = None;

    for (i, &c) in chars.iter().enumerate() {
        let w = char_width(c, col, tab_width);
        if col + w > width && i > row_start {
            let brk = last_space.filter(|&b| b > row_start && b <= i).unwrap_or(i);
            offsets.push(brk);
            row_start = brk;
            last_space = None;
            // Re-lay what moved down onto the new row, tabs included.
            col = chars[brk..i]
                .iter()
                .fold(0, |acc, &c| acc + char_width(c, acc, tab_width));
            col += char_width(c, col, tab_width);
        } else {
            col += w;
        }
        if c == ' ' || c == '\t' {
            last_space = Some(i + 1);
        }
    }
    offsets
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

/// Char offset within a line -> UTF-16 code units, the metric LSP speaks.
pub fn char_to_utf16(line: RopeSlice, char_offset: usize) -> usize {
    line.chars().take(char_offset).map(|c| c.len_utf16()).sum()
}

/// UTF-16 code units -> char offset within a line.
pub fn utf16_to_char(line: RopeSlice, utf16_offset: usize) -> usize {
    let mut units = 0;
    for (i, c) in line.chars().enumerate() {
        if units >= utf16_offset {
            return i;
        }
        units += c.len_utf16();
    }
    line_len_without_newline(line)
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
        assert_eq!(char_to_display_col(line, 3, 4), 6);
        assert_eq!(char_to_display_col(line, 2, 4), 4);
    }

    #[test]
    fn combining_marks_are_zero_width() {
        // "e" followed by U+0301 COMBINING ACUTE ACCENT renders as one column.
        let rope = Rope::from_str("e\u{0301}x");
        let line = rope.line(0);
        assert_eq!(char_to_display_col(line, 3, 4), 2);
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
    fn utf16_conversion_roundtrips_past_astral_chars() {
        // 𝕏 is one char but two UTF-16 units.
        let rope = Rope::from_str("a𝕏b");
        let line = rope.line(0);
        assert_eq!(char_to_utf16(line, 2), 3);
        assert_eq!(utf16_to_char(line, 3), 2);
        assert_eq!(utf16_to_char(line, 1), 1);
    }

    #[test]
    fn soft_wrap_breaks_at_spaces_and_splits_only_long_words() {
        let rope = Rope::from_str("the quick brown fox\n");
        // Rows: "the quick " / "brown fox"
        assert_eq!(wrap_offsets(rope.line(0), 10, 4), vec![0, 10]);
        // A word longer than the window has to be cut.
        let rope = Rope::from_str("abcdefghijkl\n");
        assert_eq!(wrap_offsets(rope.line(0), 5, 4), vec![0, 5, 10]);
        // A line that fits is one row.
        let rope = Rope::from_str("short\n");
        assert_eq!(wrap_offsets(rope.line(0), 40, 4), vec![0]);
        // Wide characters count double, so five of them fill six columns.
        let rope = Rope::from_str("日本語です\n");
        assert_eq!(wrap_offsets(rope.line(0), 6, 4), vec![0, 3]);
    }

    #[test]
    fn wrapped_rows_measure_their_columns_from_the_row_start() {
        let rope = Rope::from_str("the quick brown fox\n");
        let line = rope.line(0);
        // "brown fox" starts at char 10; "fox" is 6 columns into its own row.
        assert_eq!(display_col_between(line, 10, 16, 4), 6);
    }

    #[test]
    fn newline_is_not_counted_in_line_length() {
        let rope = Rope::from_str("abc\ndef");
        assert_eq!(line_len_without_newline(rope.line(0)), 3);
        assert_eq!(line_len_without_newline(rope.line(1)), 3);
    }
}
