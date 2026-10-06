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
/// With `smartcase` on, a pattern without a capital ignores case.
pub fn matches(text: &Rope, pat: &str) -> Vec<(usize, usize)> {
    if pat.is_empty() {
        return Vec::new();
    }
    let fold = crate::config::smartcase() && !has_capital(pat);
    let re = if fold {
        format!("(?i){pat}")
    } else {
        pat.to_string()
    };
    match Regex::new(&re) {
        Ok(re) => regex_matches(text, &re),
        Err(_) => literal(text, pat, fold),
    }
}

/// `literal_matches`, ignoring case when `fold`.
fn literal(text: &Rope, pat: &str, fold: bool) -> Vec<(usize, usize)> {
    if !fold {
        return literal_matches(text, &pat.chars().collect::<Vec<_>>());
    }
    // Lowercasing can change a char count (`İ`), which would throw every
    // offset after it; those chars keep their case instead.
    let lower = |c: char| {
        let mut l = c.to_lowercase();
        match (l.next(), l.next()) {
            (Some(one), None) => one,
            _ => c,
        }
    };
    let pat: Vec<char> = pat.chars().map(lower).collect();
    let folded: Rope = text.chars().map(lower).collect::<String>().as_str().into();
    literal_matches(&folded, &pat)
}

/// A capital typed in the pattern — not one in an escape like `\S` or `\W`,
/// which name a character class rather than asking for case.
pub fn has_capital(pat: &str) -> bool {
    let mut chars = pat.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next();
        } else if c.is_uppercase() {
            return true;
        }
    }
    false
}

/// The replacement for `matched` when it was found ignoring case: a plain
/// lowercase replacement takes the match's shape — `FOO` → `BAR`, `Foo` →
/// `Bar`. Anything else, or a replacement with capitals of its own, is left
/// as typed.
fn preserve_case(matched: &str, with: String) -> String {
    if has_capital(&with) || !matched.chars().any(char::is_alphabetic) {
        return with;
    }
    let letters = || matched.chars().filter(|c| c.is_alphabetic());
    if letters().count() > 1 && letters().all(char::is_uppercase) {
        return with.to_uppercase();
    }
    if letters().next().is_some_and(char::is_uppercase) {
        let mut chars = with.chars();
        return match chars.next() {
            Some(first) => first.to_uppercase().chain(chars).collect(),
            None => with,
        };
    }
    with
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

/// The edits vim's `:s` would make: `(from, to, replacement)` char ranges
/// over the whole rope, one per match of `pat` inside `scope`.
///
/// Without `global` only the first match on each line is replaced — vim's
/// rule. With `insensitive` the match ignores case. A pattern the regex
/// engine rejects is matched literally, like `/` search, and then the
/// replacement is literal too; with a real regex, vim-style `\1`..`\9` in
/// the replacement expand capture groups (a literal `$` stays literal).
pub fn substitutions(
    text: &Rope,
    scope: std::ops::Range<usize>,
    pat: &str,
    repl: &str,
    global: bool,
    insensitive: bool,
) -> Vec<(usize, usize, String)> {
    if pat.is_empty() || scope.start >= scope.end.min(text.len_chars()) {
        return Vec::new();
    }
    let scoped: String = text.slice(scope.clone()).chars().collect();
    // An explicit `i`, or smartcase on a pattern without capitals; either
    // way a replacement follows the case of what it replaces.
    let insensitive = insensitive || (crate::config::smartcase() && !has_capital(pat));
    let re = if insensitive {
        format!("(?i){pat}")
    } else {
        pat.to_string()
    };
    // Match ranges as (start, end, expanded replacement) in scope-relative
    // char offsets, in order and non-overlapping.
    let found: Vec<(usize, usize, String)> = match Regex::new(&re) {
        Ok(re) => {
            let expanded = vim_replacement(repl);
            re.captures_iter(&scoped)
                .filter_map(|caps| {
                    let m = caps.get(0)?;
                    if m.start() == m.end() {
                        return None; // a zero-width match replaces nothing
                    }
                    let mut with = String::new();
                    caps.expand(&expanded, &mut with);
                    if insensitive {
                        with = preserve_case(m.as_str(), with);
                    }
                    Some((
                        scoped[..m.start()].chars().count(),
                        scoped[..m.end()].chars().count(),
                        with,
                    ))
                })
                .collect()
        }
        Err(_) => {
            let scope_rope: Rope = scoped.as_str().into();
            literal(&scope_rope, pat, insensitive)
                .into_iter()
                .map(|(f, t)| {
                    let matched = scope_rope.slice(f..t).to_string();
                    let with = repl.to_string();
                    let with = if insensitive {
                        preserve_case(&matched, with)
                    } else {
                        with
                    };
                    (f, t, with)
                })
                .collect()
        }
    };

    let mut out = Vec::new();
    let mut done_line: Option<usize> = None;
    for (from, to, with) in found {
        let line = text.char_to_line(scope.start + from);
        if !global && done_line == Some(line) {
            continue;
        }
        done_line = Some(line);
        out.push((scope.start + from, scope.start + to, with));
    }
    out
}

/// Translate a vim replacement string for the regex engine: `\1`..`\9` name
/// capture groups, and a `$` the user typed stays a literal dollar sign.
fn vim_replacement(repl: &str) -> String {
    let mut out = String::with_capacity(repl.len());
    let mut chars = repl.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(d @ '1'..='9') => {
                    out.push('$');
                    out.push(d);
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '$' => out.push_str("$$"),
            _ => out.push(c),
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

    fn subs(text: &str, pat: &str, repl: &str, global: bool) -> Vec<(usize, usize, String)> {
        let rope = Rope::from_str(text);
        let end = rope.len_chars();
        substitutions(&rope, 0..end, pat, repl, global, false)
    }

    #[test]
    fn smartcase_ignores_case_until_a_capital_is_typed() {
        assert_eq!(find("Foo foo", "foo"), vec![(0, 3), (4, 7)]);
        assert_eq!(find("Foo foo", "Foo"), vec![(0, 3)]);
        assert_eq!(
            find("A b", r"\S"),
            vec![(0, 1), (2, 3)],
            "an escape is not a capital"
        );
        assert_eq!(
            find("X[ x[", "x["),
            vec![(0, 2), (3, 5)],
            "the literal fallback folds too"
        );
    }

    #[test]
    fn replacing_ignoring_case_keeps_the_case_of_each_match() {
        assert_eq!(
            subs("foo Foo FOO", "foo", "bar", true),
            vec![
                (0, 3, "bar".to_string()),
                (4, 7, "Bar".to_string()),
                (8, 11, "BAR".to_string()),
            ]
        );
        // A replacement with capitals of its own is taken as typed.
        assert_eq!(
            subs("FOO", "foo", "bAr", true),
            vec![(0, 3, "bAr".to_string())]
        );
    }

    #[test]
    fn substitution_replaces_first_match_per_line_without_g() {
        assert_eq!(
            subs("foo foo\nfoo foo", "foo", "bar", false),
            vec![(0, 3, "bar".to_string()), (8, 11, "bar".to_string())]
        );
    }

    #[test]
    fn substitution_with_g_replaces_every_match() {
        assert_eq!(
            subs("foo foo", "foo", "bar", true),
            vec![(0, 3, "bar".to_string()), (4, 7, "bar".to_string())]
        );
    }

    #[test]
    fn substitution_expands_vim_capture_groups() {
        assert_eq!(
            subs("hello world", r"(\w+) (\w+)", r"\2 \1", false),
            vec![(0, 11, "world hello".to_string())]
        );
        // A literal dollar sign in the replacement stays literal.
        assert_eq!(
            subs("price", "price", "$5", false),
            vec![(0, 5, "$5".to_string())]
        );
    }

    #[test]
    fn substitution_falls_back_to_literal_and_respects_scope() {
        assert_eq!(
            subs("a[b a[b", "a[", "x", true),
            vec![(0, 2, "x".to_string()), (4, 6, "x".to_string())]
        );
        let rope = Rope::from_str("foo foo");
        assert_eq!(
            substitutions(&rope, 4..7, "foo", "bar", true, false).len(),
            1
        );
        assert!(substitutions(&rope, 4..7, "nope", "bar", true, false).is_empty());
    }

    #[test]
    fn substitution_case_insensitive_flag() {
        let rope = Rope::from_str("Foo FOO");
        assert_eq!(substitutions(&rope, 0..7, "foo", "x", true, true).len(), 2);
    }
}
