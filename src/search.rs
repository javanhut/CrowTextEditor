//! Regex search over the rope.
//!
//! A pattern that fails to compile — which happens constantly mid-keystroke,
//! `[` half-typed — falls back to a literal char-window scan, so incremental
//! preview never breaks while a pattern is being typed.
//!
//! Offsets in and out are char offsets, the editor's canonical metric.

use std::collections::VecDeque;

use regex::Regex;
use ropey::Rope;

/// (start, end) char ranges of every non-overlapping match, in order.
pub fn matches(text: &Rope, pat: &str) -> Vec<(usize, usize)> {
    if pat.is_empty() {
        return Vec::new();
    }
    match Regex::new(pat) {
        Ok(re) => regex_matches(text, &re),
        Err(_) => literal_matches(text, &pat.chars().collect::<Vec<_>>()),
    }
}

fn regex_matches(text: &Rope, re: &Regex) -> Vec<(usize, usize)> {
    // ponytail: flattens the rope per search; the regex-cursor crate searches
    // rope chunks directly when big files make this itch.
    let s = text.to_string();
    let mut out = Vec::new();
    let mut byte = 0usize;
    let mut ch = 0usize;
    for m in re.find_iter(&s) {
        if m.start() == m.end() {
            continue; // a zero-width match selects nothing
        }
        ch += s[byte..m.start()].chars().count();
        let start = ch;
        ch += s[m.start()..m.end()].chars().count();
        byte = m.end();
        out.push((start, ch));
    }
    out
}

fn literal_matches(text: &Rope, pat: &[char]) -> Vec<(usize, usize)> {
    let m = pat.len();
    let mut out = Vec::new();
    let mut window: VecDeque<char> = VecDeque::with_capacity(m + 1);
    for (i, c) in text.chars().enumerate() {
        window.push_back(c);
        if window.len() > m {
            window.pop_front();
        }
        if window.len() == m && window.iter().eq(pat.iter()) {
            out.push((i + 1 - m, i + 1));
            window.clear(); // non-overlapping
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find(text: &str, pat: &str) -> Vec<(usize, usize)> {
        matches(&Rope::from_str(text), pat)
    }

    #[test]
    fn finds_all_matches() {
        assert_eq!(
            find("foo x foo y foo", "foo"),
            vec![(0, 3), (6, 9), (12, 15)]
        );
    }

    #[test]
    fn regex_patterns_match_variable_lengths() {
        assert_eq!(find("a1 b22 c", r"\d+"), vec![(1, 2), (4, 6)]);
    }

    #[test]
    fn invalid_regex_falls_back_to_literal() {
        assert_eq!(find("a[b c", "a["), vec![(0, 2)]);
    }

    #[test]
    fn matches_do_not_overlap() {
        assert_eq!(find("aaaa", "aa"), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn offsets_are_chars_not_bytes() {
        assert_eq!(find("日本語x日本", "日本"), vec![(0, 2), (4, 6)]);
    }

    #[test]
    fn empty_pattern_matches_nothing() {
        assert_eq!(find("abc", ""), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn zero_width_matches_are_skipped() {
        assert_eq!(find("ab", "x*"), Vec::<(usize, usize)>::new());
    }
}
