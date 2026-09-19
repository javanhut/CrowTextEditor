//! Text objects: the ranges `mi` and `ma` select.
//!
//! Every finder takes the current selection as a half-open char range and
//! returns the object's range, or `None` when there isn't one here. Pressing
//! the same object again grows to the next enclosing one, so a selection that
//! already *is* the object is treated as a request for the one around it.

use ropey::{Rope, RopeSlice};

use crate::position::{self, CharClass};

/// How far out `bracket_pair` looks for a pair containing the selection
/// before giving up.
const MAX_NESTING: usize = 64;

/// The delimiter pair a character names: brackets by either half (plus `b`
/// for parentheses and `B` for braces, as in vim), quotes by themselves.
pub fn pair_for(c: char) -> Option<(char, char)> {
    Some(match c {
        '(' | ')' | 'b' => ('(', ')'),
        '[' | ']' => ('[', ']'),
        '{' | '}' | 'B' => ('{', '}'),
        '<' | '>' => ('<', '>'),
        '"' | '\'' | '`' => (c, c),
        _ => return None,
    })
}

/// The object named by `c` around `sel`: `inside` excludes its delimiters
/// (or trailing whitespace), otherwise they are included.
pub fn find(
    text: &Rope,
    tree: Option<&tree_sitter::Tree>,
    sel: (usize, usize),
    c: char,
    inside: bool,
) -> Option<(usize, usize)> {
    let slice = text.slice(..);
    if let Some((open, close)) = pair_for(c) {
        let (o, cl) = if open == close {
            quote_pair(slice, sel, open)?
        } else {
            let mut found = bracket_pair(slice, sel.0, sel.1, open, close)?;
            // Already exactly this object: step out to the next one.
            if span(found, inside) == sel && found.0 > 0 {
                found = bracket_pair(slice, found.0 - 1, found.1 + 1, open, close)?;
            }
            found
        };
        return Some(span((o, cl), inside));
    }
    match c {
        'w' => word(slice, sel.0, inside, false),
        'W' => word(slice, sel.0, inside, true),
        'p' => paragraph(text, sel.0, inside),
        'f' | 't' | 'a' | 'c' => syntax_object(text, tree?, sel, c, inside),
        _ => None,
    }
}

/// Delimiter positions (open, close) as the selected range.
fn span((o, c): (usize, usize), inside: bool) -> (usize, usize) {
    if inside {
        (o + 1, c)
    } else {
        (o, c + 1)
    }
}

/// The positions of the innermost `open`/`close` pair containing
/// `from..to`. A cursor on either delimiter belongs to that pair.
pub fn bracket_pair(
    text: RopeSlice,
    from: usize,
    to: usize,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    let len = text.len_chars();
    if len == 0 {
        return None;
    }
    let mut start = from.min(len - 1);
    // Each trip outwards rescans for the closer, so deeply nested text (a
    // generated data literal) would otherwise cost depth times length.
    for _ in 0..MAX_NESTING {
        // Walk back to an unmatched opener.
        let mut depth = 0usize;
        let mut o = None;
        let mut p = start as isize;
        while p >= 0 {
            let ch = text.char(p as usize);
            if ch == close && (p as usize) < start {
                depth += 1;
            } else if ch == open {
                if depth == 0 {
                    o = Some(p as usize);
                    break;
                }
                depth -= 1;
            }
            p -= 1;
        }
        let o = o?;
        let c = position::matching_bracket(text, o)?;
        // It has to reach past the selection's end too.
        if c + 1 >= to {
            return Some((o, c));
        }
        if o == 0 {
            return None;
        }
        start = o - 1;
    }
    None
}

/// The quote pair containing the selection, if it is inside one. Quotes
/// pair up left to right along the line, skipping backslash-escaped ones.
pub fn quote_pair_enclosing(
    text: RopeSlice,
    sel: (usize, usize),
    q: char,
) -> Option<(usize, usize)> {
    quote_pairs(text, sel.0, q)
        .into_iter()
        .find(|&(o, c)| o <= sel.0 && c + 1 >= sel.1)
}

/// What `mi"` / `ma"` select: the pair around the selection, the next one
/// out when the selection already is that pair, and — with the cursor
/// between pairs — the next one along the line, which is what was meant
/// nine times in ten. `md`/`mr` take the strict version above instead:
/// deleting the quotes around something else is never what was asked for.
fn quote_pair(text: RopeSlice, sel: (usize, usize), q: char) -> Option<(usize, usize)> {
    let pairs = quote_pairs(text, sel.0, q);
    pairs
        .iter()
        .copied()
        .find(|&(o, c)| o <= sel.0 && c + 1 >= sel.1 && (o, c + 1) != sel && (o + 1, c) != sel)
        .or_else(|| pairs.iter().copied().find(|&(o, _)| o > sel.0))
}

/// The quote pairs on the line holding `pos`, left to right.
fn quote_pairs(text: RopeSlice, pos: usize, q: char) -> Vec<(usize, usize)> {
    let pos = pos.min(text.len_chars().saturating_sub(1));
    let line = text.char_to_line(pos);
    let start = text.line_to_char(line);
    let mut quotes = Vec::new();
    let mut escaped = false;
    for (i, ch) in text.line(line).chars().enumerate() {
        if ch == '\\' && !escaped {
            escaped = true;
            continue;
        }
        if ch == q && !escaped {
            quotes.push(start + i);
        }
        escaped = false;
    }
    quotes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&[o, c]| (o, c))
        .collect()
}

/// The word (or WORD, whitespace-delimited) under `pos`. Around takes the
/// whitespace after it, or before it when it ends the line.
fn word(text: RopeSlice, pos: usize, inside: bool, big: bool) -> Option<(usize, usize)> {
    let len = text.len_chars();
    if pos >= len {
        return None;
    }
    let class = |ch: char| {
        let c = position::classify(ch);
        if big && c != CharClass::Whitespace {
            CharClass::Word
        } else {
            c
        }
    };
    let at = class(text.char(pos));
    let same = |i: usize| class(text.char(i)) == at && text.char(i) != '\n';
    let mut s = pos;
    while s > 0 && same(s - 1) {
        s -= 1;
    }
    let mut e = pos;
    while e < len && same(e) {
        e += 1;
    }
    if s == e {
        return None; // on a line ending: there is no word here
    }
    if inside || at == CharClass::Whitespace {
        return Some((s, e));
    }
    let blank = |i: usize| matches!(text.char(i), ' ' | '\t');
    let mut after = e;
    while after < len && blank(after) {
        after += 1;
    }
    if after > e {
        return Some((s, after));
    }
    let mut before = s;
    while before > 0 && blank(before - 1) {
        before -= 1;
    }
    Some((before, e))
}

/// The run of non-blank lines around `pos` (or of blank lines, on one).
/// Around adds the blank lines that follow.
fn paragraph(text: &Rope, pos: usize, inside: bool) -> Option<(usize, usize)> {
    let lines = text.len_lines();
    let blank = |l: usize| text.line(l).chars().all(char::is_whitespace);
    let line = text.char_to_line(pos.min(text.len_chars()));
    let kind = blank(line);
    let mut first = line;
    while first > 0 && blank(first - 1) == kind {
        first -= 1;
    }
    let mut last = line;
    while last + 1 < lines && blank(last + 1) == kind {
        last += 1;
    }
    if !inside && !kind {
        while last + 1 < lines && blank(last + 1) {
            last += 1;
        }
    }
    let end = if last + 1 < lines {
        text.line_to_char(last + 1)
    } else {
        text.len_chars()
    };
    Some((text.line_to_char(first), end))
}

/// Function (`f`), type (`t`), argument (`a`) or comment (`c`), from the
/// syntax tree: the smallest such node around the selection, or the next one
/// out when the selection already is that node.
fn syntax_object(
    text: &Rope,
    tree: &tree_sitter::Tree,
    sel: (usize, usize),
    c: char,
    inside: bool,
) -> Option<(usize, usize)> {
    let b0 = text.char_to_byte(sel.0);
    let b1 = text.char_to_byte(sel.1.max(sel.0));
    let mut node = tree.root_node().descendant_for_byte_range(b0, b1)?;
    loop {
        if matches_object(node, c) {
            if let Some(range) = object_range(text, node, c, inside) {
                if range != sel {
                    return Some(range);
                }
            }
        }
        node = node.parent()?;
    }
}

fn matches_object(node: tree_sitter::Node, c: char) -> bool {
    let kind = node.kind();
    match c {
        'f' => {
            (kind.contains("function")
                || kind.contains("method")
                || kind.contains("lambda")
                || kind.contains("closure"))
                && !kind.contains("call")
                && !kind.contains("type")
                && !kind.contains("signature")
                && !kind.contains("parameter")
                && node.is_named()
        }
        't' => {
            [
                "class",
                "struct",
                "enum",
                "interface",
                "trait",
                "impl",
                "union",
                "type",
            ]
            .iter()
            .any(|w| kind.contains(w))
                && (kind == "class"
                    || kind.ends_with("_item")
                    || kind.ends_with("_declaration")
                    || kind.ends_with("_definition")
                    || kind.ends_with("_specifier"))
        }
        'a' => {
            node.is_named()
                && node.parent().is_some_and(|p| {
                    let k = p.kind();
                    k.contains("argument")
                        || k.contains("parameters")
                        || k.ends_with("parameter_list")
                })
        }
        'c' => kind.contains("comment"),
        _ => false,
    }
}

fn object_range(
    text: &Rope,
    node: tree_sitter::Node,
    c: char,
    inside: bool,
) -> Option<(usize, usize)> {
    let chars = |n: tree_sitter::Node| {
        (
            text.byte_to_char(n.start_byte()),
            text.byte_to_char(n.end_byte()),
        )
    };
    let (s, e) = chars(node);
    match c {
        'f' | 't' if inside => {
            let body = node.child_by_field_name("body").or_else(|| {
                let mut cursor = node.walk();
                let last = node.named_children(&mut cursor).last();
                last.filter(|n| n.kind().contains("block") || n.kind().contains("body"))
            })?;
            let (bs, be) = chars(body);
            let braced = be > bs + 1
                && matches!(text.char(bs), '{' | '(' | '[')
                && matches!(text.char(be - 1), '}' | ')' | ']');
            Some(if braced { (bs + 1, be - 1) } else { (bs, be) })
        }
        // Around an argument takes its separator: the comma and space after
        // it, or the ones before it when it is the last.
        'a' if !inside => {
            let len = text.len_chars();
            let mut after = e;
            while after < len && text.char(after) == ' ' {
                after += 1;
            }
            if after < len && text.char(after) == ',' {
                let mut end = after + 1;
                while end < len && text.char(end) == ' ' {
                    end += 1;
                }
                return Some((s, end));
            }
            let mut before = s;
            while before > 0 && text.char(before - 1) == ' ' {
                before -= 1;
            }
            if before > 0 && text.char(before - 1) == ',' {
                return Some((before - 1, e));
            }
            Some((s, e))
        }
        _ => Some((s, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str, needle: &str) -> usize {
        text[..text.find(needle).unwrap()].chars().count()
    }

    #[test]
    fn inside_and_around_brackets() {
        let src = "call(a, (b + c), d)";
        let t = Rope::from_str(src);
        let pos = at(src, "b");
        let inner = find(&t, None, (pos, pos), '(', true).unwrap();
        assert_eq!(t.slice(inner.0..inner.1).to_string(), "b + c");
        let around = find(&t, None, (pos, pos), ')', false).unwrap();
        assert_eq!(t.slice(around.0..around.1).to_string(), "(b + c)");
        // Again on what is already selected: the pair around it.
        let outer = find(&t, None, inner, '(', true).unwrap();
        assert_eq!(t.slice(outer.0..outer.1).to_string(), "a, (b + c), d");
    }

    #[test]
    fn a_cursor_on_a_bracket_belongs_to_its_pair() {
        let src = "f(x) { y }";
        let t = Rope::from_str(src);
        let brace = at(src, "{");
        let r = find(&t, None, (brace, brace), 'B', true).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), " y ");
        let close = at(src, ")");
        let r = find(&t, None, (close, close), 'b', true).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), "x");
    }

    #[test]
    fn quotes_pair_left_to_right_and_skip_escapes() {
        let src = r#"a "one \" two" b "three""#;
        let t = Rope::from_str(src);
        let pos = at(src, "two");
        let r = find(&t, None, (pos, pos), '"', true).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), r#"one \" two"#);
        // Between pairs: the next one on the line.
        let pos = at(src, " b ") + 1;
        let r = find(&t, None, (pos, pos), '"', false).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), r#""three""#);
    }

    #[test]
    fn words_and_paragraphs() {
        let src = "alpha beta  gamma\n\nnext para\nstill\n";
        let t = Rope::from_str(src);
        let pos = at(src, "beta") + 1;
        let r = find(&t, None, (pos, pos), 'w', true).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), "beta");
        let r = find(&t, None, (pos, pos), 'w', false).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), "beta  ");
        let pos = at(src, "still");
        let r = find(&t, None, (pos, pos), 'p', true).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), "next para\nstill\n");
        let r = find(&t, None, (0, 0), 'p', false).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), "alpha beta  gamma\n\n");
    }

    #[test]
    fn functions_and_arguments_from_the_syntax_tree() {
        let src = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let t = Rope::from_str(src);
        let config = crate::syntax::config_for(Some(std::path::Path::new("x.rs"))).unwrap();
        let tree = crate::syntax::parse_tree(config, &t, None).unwrap();
        let pos = at(src, "a + b");
        let r = find(&t, Some(&tree), (pos, pos), 'f', true).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), "\n    a + b\n");
        let r = find(&t, Some(&tree), (pos, pos), 'f', false).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), src.trim_end());
        let pos = at(src, "a: i32");
        let r = find(&t, Some(&tree), (pos, pos), 'a', true).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), "a: i32");
        let r = find(&t, Some(&tree), (pos, pos), 'a', false).unwrap();
        assert_eq!(t.slice(r.0..r.1).to_string(), "a: i32, ");
    }
}
