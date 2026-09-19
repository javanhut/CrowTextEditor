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
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete, UnicodeSegmentation};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

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

/// Display width of one grapheme cluster starting at display column `col`.
///
/// This, not a sum over the cluster's chars, is what the renderer draws: a
/// ZWJ emoji sequence is one glyph, not three. Measuring the cursor the same
/// way is what keeps it on the character it is drawn over.
pub fn grapheme_width(g: &str, col: usize, tab_width: usize) -> usize {
    match g {
        "\t" => char_width('\t', col, tab_width),
        _ if g.starts_with(['\n', '\r']) => 0,
        _ => UnicodeWidthStr::width(g),
    }
}

/// The line's grapheme clusters as (char offset, cluster), newline excluded.
fn graphemes(line: RopeSlice) -> Vec<(usize, String)> {
    let text: String = line.chars().take(line_len_without_newline(line)).collect();
    let mut at = 0;
    text.graphemes(true)
        .map(|g| {
            let start = at;
            at += g.chars().count();
            (start, g.to_string())
        })
        .collect()
}

/// Convert a char offset within a line to the display column it renders at.
pub fn char_to_display_col(line: RopeSlice, char_offset: usize, tab_width: usize) -> usize {
    let mut col = 0usize;
    for (at, g) in graphemes(line) {
        if at >= char_offset {
            break;
        }
        col += grapheme_width(&g, col, tab_width);
    }
    col
}

/// Convert a display column to the nearest char offset within a line.
///
/// If the column falls inside a wide character, a tab, or a cluster, this
/// returns the offset where that starts rather than splitting it.
pub fn display_col_to_char(line: RopeSlice, target_col: usize, tab_width: usize) -> usize {
    let mut col = 0usize;
    for (at, g) in graphemes(line) {
        let w = grapheme_width(&g, col, tab_width);
        if col + w > target_col {
            return at;
        }
        col += w;
    }
    line_len_without_newline(line)
}

/// Display width of the chars `from..to` of a line, measured as if the row
/// started at column 0 — which is what a soft-wrapped row does.
pub fn display_col_between(line: RopeSlice, from: usize, to: usize, tab_width: usize) -> usize {
    let text: String = line.chars_at(from).take(to.saturating_sub(from)).collect();
    let mut col = 0usize;
    for g in text.graphemes(true) {
        col += grapheme_width(g, col, tab_width);
    }
    col
}

/// The char offset in `from..to` of a line drawn at display column `target`,
/// columns counted from `from` — a click on a soft-wrapped row. Past the end
/// it is `to`.
pub fn display_col_to_char_between(
    line: RopeSlice,
    from: usize,
    to: usize,
    target: usize,
    tab_width: usize,
) -> usize {
    let text: String = line.chars_at(from).take(to.saturating_sub(from)).collect();
    let mut col = 0usize;
    let mut at = from;
    for g in text.graphemes(true) {
        let w = grapheme_width(g, col, tab_width);
        if col + w > target {
            return at;
        }
        col += w;
        at += g.chars().count();
    }
    to
}

/// Char offsets within `line` where each soft-wrapped visual row begins.
///
/// Always starts with `0`, so a line that fits returns `[0]` and the count is
/// the number of screen rows the line takes. Breaks after the last space that
/// fits, and mid-word only when one word is wider than the window — and even
/// then between clusters, never inside one.
pub fn wrap_offsets(line: RopeSlice, width: usize, tab_width: usize) -> Vec<usize> {
    let mut offsets = vec![0usize];
    if width == 0 {
        return offsets;
    }
    let clusters = graphemes(line);
    let mut col = 0usize;
    let mut row_start = 0usize; // index into `clusters`
                                // Cluster index just past the last space seen on this row — where a
                                // break would land without splitting a word.
    let mut last_space: Option<usize> = None;

    for (i, (_, g)) in clusters.iter().enumerate() {
        let w = grapheme_width(g, col, tab_width);
        if col + w > width && i > row_start {
            let brk = last_space.filter(|&b| b > row_start && b <= i).unwrap_or(i);
            offsets.push(clusters[brk].0);
            row_start = brk;
            last_space = None;
            // Re-lay what moved down onto the new row, tabs included.
            col = clusters[brk..i]
                .iter()
                .fold(0, |acc, (_, g)| acc + grapheme_width(g, acc, tab_width));
            col += grapheme_width(g, col, tab_width);
        } else {
            col += w;
        }
        if g == " " || g == "\t" {
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

/// The grapheme boundary at or before `char_idx`.
pub fn grapheme_floor(slice: RopeSlice, char_idx: usize) -> usize {
    let at = char_idx.min(slice.len_chars());
    if at == 0 {
        return 0;
    }
    let prev = prev_grapheme_boundary(slice, at);
    if next_grapheme_boundary(slice, prev) == at {
        at
    } else {
        prev
    }
}

/// The grapheme boundary at or after `char_idx`.
pub fn grapheme_ceil(slice: RopeSlice, char_idx: usize) -> usize {
    let at = char_idx.min(slice.len_chars());
    let floor = grapheme_floor(slice, at);
    if floor == at {
        at
    } else {
        next_grapheme_boundary(slice, floor)
    }
}

/// The bracket matching the one at `pos`, by depth counting.
///
/// `()`, `[]` and `{}` pair with their own kind only, so a `)` inside a
/// parenthesised expression doesn't disturb a `{` scan. No string/comment
/// awareness: a bracket in a string literal counts like any other.
pub fn matching_bracket(text: RopeSlice, pos: usize) -> Option<usize> {
    if pos >= text.len_chars() {
        return None;
    }
    let (open, close, forward) = match text.char(pos) {
        '(' => ('(', ')', true),
        '[' => ('[', ']', true),
        '{' => ('{', '}', true),
        ')' => ('(', ')', false),
        ']' => ('[', ']', false),
        '}' => ('{', '}', false),
        _ => return None,
    };
    let mut depth = 0usize;
    if forward {
        for (i, c) in text.chars_at(pos).enumerate() {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(pos + i);
                }
            }
        }
    } else {
        for i in (0..=pos).rev() {
            let c = text.char(i);
            if c == close {
                depth += 1;
            } else if c == open {
                if depth == 1 {
                    return Some(i);
                }
                depth -= 1;
            }
        }
    }
    None
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
    fn grapheme_floor_and_ceil_expand_an_interior_offset() {
        let rope = Rope::from_str("ae\u{301}b");
        let s = rope.slice(..);
        assert_eq!(grapheme_floor(s, 2), 1);
        assert_eq!(grapheme_ceil(s, 2), 3);
        assert_eq!(grapheme_floor(s, 3), 3);
        assert_eq!(grapheme_ceil(s, 3), 3);
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
    fn a_cluster_is_measured_the_way_it_is_drawn() {
        // A ZWJ sequence: its width is the cluster's, not the sum of its chars.
        let family = "👨\u{200d}👩\u{200d}👧";
        let rope = Rope::from_str(&format!("a{family}b"));
        let line = rope.line(0);
        let drawn = UnicodeWidthStr::width(family);
        assert_eq!(char_to_display_col(line, 6, 4), 1 + drawn);
        // A column inside the cluster lands on its start, never mid-cluster.
        assert_eq!(display_col_to_char(line, 2, 4), 1);
        assert_eq!(display_col_to_char(line, 1 + drawn, 4), 6);
        assert_eq!(display_col_between(line, 1, 7, 4), drawn + 1);
        // Wrapping never splits it either.
        for off in wrap_offsets(line, 2, 4) {
            assert!(
                [0, 1, 6].contains(&off),
                "split inside the cluster at {off}"
            );
        }
    }

    #[test]
    fn clicks_on_a_wrapped_row_find_their_char() {
        let rope = Rope::from_str("the quick brown fox\n");
        let line = rope.line(0);
        assert_eq!(display_col_to_char_between(line, 10, 19, 6, 4), 16);
        assert_eq!(display_col_to_char_between(line, 10, 19, 50, 4), 19);
    }

    #[test]
    fn newline_is_not_counted_in_line_length() {
        let rope = Rope::from_str("abc\ndef");
        assert_eq!(line_len_without_newline(rope.line(0)), 3);
        assert_eq!(line_len_without_newline(rope.line(1)), 3);
    }

    #[test]
    fn matching_bracket_pairs_both_directions() {
        //      012345678901234
        let rope = Rope::from_str("fn main() { x }");
        let s = rope.slice(..);
        assert_eq!(matching_bracket(s, 7), Some(8)); // ( -> )
        assert_eq!(matching_bracket(s, 8), Some(7)); // ) -> (
        assert_eq!(matching_bracket(s, 10), Some(14)); // { -> }
        assert_eq!(matching_bracket(s, 14), Some(10)); // } -> {
        assert_eq!(matching_bracket(s, 0), None); // not a bracket
    }

    #[test]
    fn matching_bracket_skips_nesting_and_matches_its_own_kind() {
        let rope = Rope::from_str("{ a { b ( c ) } d }");
        let s = rope.slice(..);
        assert_eq!(matching_bracket(s, 0), Some(18));
        assert_eq!(matching_bracket(s, 18), Some(0));
        assert_eq!(matching_bracket(s, 4), Some(14));
        // The closing paren does not end the brace scan.
        assert_eq!(matching_bracket(s, 12), Some(8));
    }

    #[test]
    fn matching_bracket_returns_none_when_unbalanced() {
        let rope = Rope::from_str("(a");
        let s = rope.slice(..);
        assert_eq!(matching_bracket(s, 0), None);
        let rope = Rope::from_str("a)");
        assert_eq!(matching_bracket(rope.slice(..), 1), None);
    }
}
