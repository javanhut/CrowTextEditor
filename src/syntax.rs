//! Tree-sitter parsing: syntax highlighting, and syntax nodes as selections.
//!
//! Each additional language is one grammar dependency, one `lang!` line, and
//! one arm in `config_for`. Highlight groups are deliberately coarse: a
//! handful of colors reads better in a terminal than a full theme.
//!
//! A language with no published grammar can still be colored: `highlight`
//! falls back to a hand-rolled lexer that emits the same spans without a
//! tree. Oxigen (`.oxi`) is the one that takes that path.

use std::path::Path;
use std::sync::OnceLock;

use crossterm::style::Color;
use ropey::Rope;
use streaming_iterator::StreamingIterator;
use tree_sitter::{Language, Parser, Query, QueryCursor, Tree};

pub struct Config {
    language: Language,
    query: Query,
    /// Capture index -> highlight group, precomputed from capture names.
    groups: Vec<u8>,
    /// Second grammar run over the block tree's `inline` nodes (markdown).
    inline: Option<&'static Config>,
}

pub struct Syntax {
    /// The grammar that produced this, kept so a reparse reuses it. `None`
    /// when the fallback lexer did the coloring.
    pub config: Option<&'static Config>,
    /// `None` from the fallback lexer: it produces spans, not a tree, so
    /// `A-o` (expand to the enclosing node) has nothing to walk.
    pub tree: Option<Tree>,
    /// Highlight spans as (start, end, group) char ranges, sorted by start.
    pub spans: Vec<(usize, usize, u8)>,
}

/// A span's group byte is a color group id (low bits) plus attribute flags,
/// so markdown emphasis can be bold/italic without eating a color slot.
pub const BOLD: u8 = 0x40;
pub const ITALIC: u8 = 0x80;
const GROUP_MASK: u8 = 0x3f;

/// Terminal color for a highlight group, from the active theme.
pub fn color(group: u8) -> Option<Color> {
    crate::theme::current().syntax[((group & GROUP_MASK) as usize).min(7)]
}

/// BOLD/ITALIC flags for a group byte: the span's own flags plus the active
/// theme's per-group attribute bitmasks.
pub fn attrs(group: u8) -> u8 {
    let theme = crate::theme::current();
    let bit = 1u8 << ((group & GROUP_MASK).min(7));
    let mut a = group & (BOLD | ITALIC);
    if theme.bold & bit != 0 {
        a |= BOLD;
    }
    if theme.italic & bit != 0 {
        a |= ITALIC;
    }
    a
}

fn group_of(capture_name: &str) -> u8 {
    // Markdown's block query names its captures @text.*; special-case the
    // full names so headings/code/links don't all fall in one bucket.
    match capture_name {
        "text.title" => 3 | BOLD,
        "text.strong" => BOLD,
        "text.emphasis" => ITALIC,
        "text.literal" => 2,
        "text.uri" | "text.reference" => 6,
        _ => match capture_name.split('.').next().unwrap_or("") {
            "comment" => 1,
            "string" | "character" => 2,
            "keyword" => 3,
            "function" | "constructor" => 4,
            "type" | "tag" | "property" => 5,
            "constant" | "number" | "escape" | "boolean" => 6,
            "macro" | "attribute" => 7,
            _ => 0,
        },
    }
}

/// One lazily built grammar config; `None` if its bundled query fails to
/// compile (a broken grammar should degrade to plain text, not a panic).
macro_rules! lang {
    ($fn_name:ident, $language:expr, $query:expr) => {
        lang!($fn_name, $language, $query, None);
    };
    ($fn_name:ident, $language:expr, $query:expr, $inline:expr) => {
        fn $fn_name() -> Option<&'static Config> {
            static CELL: OnceLock<Option<Config>> = OnceLock::new();
            CELL.get_or_init(|| {
                let language: Language = $language.into();
                let query = Query::new(&language, $query).ok()?;
                let groups = query.capture_names().iter().map(|n| group_of(n)).collect();
                Some(Config {
                    language,
                    query,
                    groups,
                    inline: $inline,
                })
            })
            .as_ref()
        }
    };
}

lang!(
    rust,
    tree_sitter_rust::LANGUAGE,
    tree_sitter_rust::HIGHLIGHTS_QUERY
);
lang!(
    toml,
    tree_sitter_toml_ng::LANGUAGE,
    tree_sitter_toml_ng::HIGHLIGHTS_QUERY
);
lang!(
    json,
    tree_sitter_json::LANGUAGE,
    tree_sitter_json::HIGHLIGHTS_QUERY
);
lang!(
    python,
    tree_sitter_python::LANGUAGE,
    tree_sitter_python::HIGHLIGHTS_QUERY
);
lang!(
    bash,
    tree_sitter_bash::LANGUAGE,
    tree_sitter_bash::HIGHLIGHT_QUERY
);
lang!(
    javascript,
    tree_sitter_javascript::LANGUAGE,
    tree_sitter_javascript::HIGHLIGHT_QUERY
);
// The typescript crate's query is only the TS additions; prepend JS's.
lang!(
    typescript,
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT,
    &[
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_typescript::HIGHLIGHTS_QUERY
    ]
    .concat()
);
lang!(
    tsx,
    tree_sitter_typescript::LANGUAGE_TSX,
    &[
        tree_sitter_javascript::HIGHLIGHT_QUERY,
        tree_sitter_typescript::HIGHLIGHTS_QUERY
    ]
    .concat()
);
lang!(
    html,
    tree_sitter_html::LANGUAGE,
    tree_sitter_html::HIGHLIGHTS_QUERY
);
lang!(
    css,
    tree_sitter_css::LANGUAGE,
    tree_sitter_css::HIGHLIGHTS_QUERY
);
lang!(c, tree_sitter_c::LANGUAGE, tree_sitter_c::HIGHLIGHT_QUERY);
// Likewise cpp's query is only the additions on top of C's.
lang!(
    cpp,
    tree_sitter_cpp::LANGUAGE,
    &[
        tree_sitter_c::HIGHLIGHT_QUERY,
        tree_sitter_cpp::HIGHLIGHT_QUERY
    ]
    .concat()
);
lang!(
    go,
    tree_sitter_go::LANGUAGE,
    tree_sitter_go::HIGHLIGHTS_QUERY
);
lang!(
    java,
    tree_sitter_java::LANGUAGE,
    tree_sitter_java::HIGHLIGHTS_QUERY
);
lang!(
    ruby,
    tree_sitter_ruby::LANGUAGE,
    tree_sitter_ruby::HIGHLIGHTS_QUERY
);
lang!(
    php,
    tree_sitter_php::LANGUAGE_PHP,
    tree_sitter_php::HIGHLIGHTS_QUERY
);
lang!(
    csharp,
    tree_sitter_c_sharp::LANGUAGE,
    tree_sitter_c_sharp::HIGHLIGHTS_QUERY
);
lang!(
    yaml,
    tree_sitter_yaml::LANGUAGE,
    tree_sitter_yaml::HIGHLIGHTS_QUERY
);
lang!(
    markdown_inline,
    tree_sitter_md::INLINE_LANGUAGE,
    tree_sitter_md::HIGHLIGHT_QUERY_INLINE
);
lang!(
    markdown,
    tree_sitter_md::LANGUAGE,
    tree_sitter_md::HIGHLIGHT_QUERY_BLOCK,
    markdown_inline()
);
lang!(
    odin,
    tree_sitter_odin::LANGUAGE,
    tree_sitter_odin::HIGHLIGHTS_QUERY
);
lang!(
    zig,
    tree_sitter_zig::LANGUAGE,
    tree_sitter_zig::HIGHLIGHTS_QUERY
);
lang!(
    lua,
    tree_sitter_lua::LANGUAGE,
    tree_sitter_lua::HIGHLIGHTS_QUERY
);

pub fn config_for(path: Option<&Path>) -> Option<&'static Config> {
    match path?.extension()?.to_str()? {
        "rs" => rust(),
        "toml" => toml(),
        "json" => json(),
        "py" => python(),
        "sh" | "bash" | "zsh" => bash(),
        "js" | "jsx" | "mjs" | "cjs" => javascript(),
        "ts" | "mts" => typescript(),
        "tsx" => tsx(),
        "html" | "htm" => html(),
        "css" => css(),
        "c" | "h" => c(),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => cpp(),
        "go" => go(),
        "java" => java(),
        "rb" => ruby(),
        "php" => php(),
        "cs" => csharp(),
        "yml" | "yaml" => yaml(),
        "md" | "markdown" => markdown(),
        "odin" => odin(),
        "zig" => zig(),
        "lua" => lua(),
        _ => None,
    }
}

/// Color a buffer: tree-sitter when a grammar covers the file, else the
/// fallback lexer. `cached` is the grammar the buffer already had, so an
/// unsaved or renamed buffer keeps the language it was opened with.
pub fn highlight(
    path: Option<&Path>,
    text: &Rope,
    cached: Option<&'static Config>,
) -> Option<Syntax> {
    if let Some(config) = cached.or_else(|| config_for(path)) {
        return parse(config, text);
    }
    match path?.extension()?.to_str()? {
        "oxi" => Some(Syntax {
            config: None,
            tree: None,
            spans: oxigen_spans(&text.chars().collect::<Vec<char>>()),
        }),
        _ => None,
    }
}

/// Parse the whole buffer and collect highlight spans.
///
/// ponytail: full reparse and a flattened copy per edit; tree-sitter's
/// incremental `InputEdit` path is the upgrade when large files itch.
pub fn parse(config: &'static Config, text: &Rope) -> Option<Syntax> {
    let src = text.to_string();
    let mut parser = Parser::new();
    parser.set_language(&config.language).ok()?;
    let tree = parser.parse(&src, None)?;

    let mut spans = Vec::new();
    collect_spans(config, tree.root_node(), &src, 0, text, &mut spans);

    // Markdown: the block grammar leaves `inline` nodes unparsed; run the
    // inline grammar over each for emphasis, code spans, and links.
    if let Some(inline) = config.inline {
        let mut ip = Parser::new();
        if ip.set_language(&inline.language).is_ok() {
            let mut stack = vec![tree.root_node()];
            while let Some(node) = stack.pop() {
                if node.kind() == "inline" {
                    let range = node.byte_range();
                    if let Some(itree) = ip.parse(&src[range.clone()], None) {
                        collect_spans(
                            inline,
                            itree.root_node(),
                            &src[range.clone()],
                            range.start,
                            text,
                            &mut spans,
                        );
                    }
                } else {
                    for i in 0..node.child_count() {
                        stack.push(node.child(i).unwrap());
                    }
                }
            }
        }
    }

    spans.sort_unstable();
    Some(Syntax {
        config: Some(config),
        tree: Some(tree),
        spans,
    })
}

/// Run `config`'s query over `root` (parsed from `src`, which starts at
/// `byte_off` in the buffer) and append the resulting spans.
fn collect_spans(
    config: &Config,
    root: tree_sitter::Node,
    src: &str,
    byte_off: usize,
    text: &Rope,
    spans: &mut Vec<(usize, usize, u8)>,
) {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&config.query, root, src.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let group = config.groups[cap.index as usize];
            if group == 0 {
                continue;
            }
            let r = cap.node.byte_range();
            if r.start < r.end {
                spans.push((
                    text.byte_to_char(byte_off + r.start),
                    text.byte_to_char(byte_off + r.end),
                    group,
                ));
            }
        }
    }
}

/// The innermost highlight group covering `pos`, or 0.
///
/// ponytail: spans nest only a few levels in practice, so a short backwards
/// scan from the binary-search point is enough.
pub fn group_at(spans: &[(usize, usize, u8)], pos: usize) -> u8 {
    let idx = spans.partition_point(|s| s.0 <= pos);
    spans[..idx]
        .iter()
        .rev()
        .take(24)
        .find(|s| s.1 > pos)
        .map(|s| s.2)
        .unwrap_or(0)
}

// ---- Oxigen ---------------------------------------------------------------
//
// No tree-sitter grammar exists for Oxigen, so it gets a lexer instead: one
// pass over the chars emitting the same (start, end, group) spans the query
// path emits. Everything the language's own Neovim syntax file colors, minus
// what needs a parser.

/// Reserved words (`token_map` in the Oxigen lexer) plus the parser's
/// contextual ones: `includes`, `main`, `hidden`.
const OXIGEN_KEYWORDS: &[&str] = &[
    "and",
    "as",
    "choose",
    "converge",
    "diverge",
    "each",
    "enum",
    "fail",
    "from",
    "fun",
    "give",
    "guard",
    "hidden",
    "hide",
    "in",
    "includes",
    "intro",
    "introduce",
    "main",
    "not",
    "option",
    "or",
    "pattern",
    "repeat",
    "self",
    "skip",
    "stop",
    "struct",
    "then",
    "unless",
    "when",
    "within",
];

/// Built-in functions, colored like the functions they are even where they
/// are passed around rather than called.
const OXIGEN_BUILTINS: &[&str] = &[
    "byte", "cancel", "chars", "chr", "error", "first", "float", "has", "insert", "int",
    "is_error", "is_value", "keys", "last", "len", "ord", "print", "println", "push", "range",
    "remove", "rest", "set", "str", "tuple", "type", "uint", "values",
];

const OXIGEN_CONSTANTS: &[&str] = &["True", "False", "None"];

/// Highlight spans for an Oxigen buffer, in char offsets.
fn oxigen_spans(c: &[char]) -> Vec<(usize, usize, u8)> {
    let mut spans = Vec::new();
    let mut prev_word = String::new();
    let mut i = 0;
    while i < c.len() {
        let start = i;
        match c[i] {
            '/' if c.get(i + 1) == Some(&'/') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                spans.push((start, i, 1));
            }
            '/' if c.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < c.len() && !(c[i] == '*' && c.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i = (i + 2).min(c.len());
                spans.push((start, i, 1));
            }
            '"' | '\'' => {
                i = oxigen_string_end(c, i);
                spans.push((start, i, 2));
            }
            // `#[indent]` and friends: a directive, not a comment.
            '#' if c.get(i + 1) == Some(&'[') => {
                while i < c.len() && c[i] != ']' && c[i] != '\n' {
                    i += 1;
                }
                i = (i + 1).min(c.len());
                spans.push((start, i, 7));
            }
            '<' => match oxigen_type_end(c, i) {
                Some(end) => {
                    spans.push((start, end, 5));
                    i = end;
                }
                None => i += 1,
            },
            ch if ch.is_ascii_digit() => {
                while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '.' || c[i] == '_') {
                    i += 1;
                }
                spans.push((start, i, 6));
            }
            ch if ch.is_alphabetic() || ch == '_' => {
                while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                    i += 1;
                }
                let word: String = c[start..i].iter().collect();
                let group = if OXIGEN_KEYWORDS.contains(&word.as_str()) {
                    3
                } else if OXIGEN_CONSTANTS.contains(&word.as_str()) {
                    6
                } else if OXIGEN_BUILTINS.contains(&word.as_str()) || c.get(i) == Some(&'(') {
                    4
                } else if prev_word == "struct"
                    || prev_word == "enum"
                    || word.starts_with(char::is_uppercase)
                {
                    5
                } else {
                    0
                };
                if group != 0 {
                    spans.push((start, i, group));
                }
                prev_word = word;
            }
            _ => i += 1,
        }
    }
    spans
}

/// Past the end of the string literal starting at `i`. An unterminated one
/// ends at the newline, or at the end of the file if it is triple-quoted.
fn oxigen_string_end(c: &[char], i: usize) -> usize {
    let quote = c[i];
    let triple = c.get(i + 1) == Some(&quote) && c.get(i + 2) == Some(&quote);
    let fence = if triple { 3 } else { 1 };
    let mut j = i + fence;
    while j < c.len() {
        if c[j] == '\\' {
            j += 2;
            continue;
        }
        if !triple && c[j] == '\n' {
            return j;
        }
        if c[j] == quote
            && (!triple || (c.get(j + 1) == Some(&quote) && c.get(j + 2) == Some(&quote)))
        {
            return (j + fence).min(c.len());
        }
        j += 1;
    }
    c.len()
}

/// The end of the type annotation opening at `i` — `<int>`, `<Array>`,
/// `<Error<div_by_zero>>`, `<type<Error || Value>>`, `<test>` — or `None`
/// when this `<` is a comparison.
///
/// ponytail: no parser, so `a<b || c>d` colors as a type; the space-free
/// form nobody writes is the price of not building one.
fn oxigen_type_end(c: &[char], i: usize) -> Option<usize> {
    if !c
        .get(i + 1)
        .is_some_and(|ch| ch.is_alphabetic() || *ch == '_')
    {
        return None;
    }
    let mut depth = 0usize;
    for (j, ch) in c.iter().enumerate().take(c.len().min(i + 64)).skip(i) {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.checked_sub(1)?; // never negative from a `<` start
                if depth == 0 {
                    return Some(j + 1);
                }
            }
            ch if ch.is_alphanumeric() || " _|,".contains(*ch) => {}
            _ => return None,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_source_gets_highlight_spans() {
        let text = Rope::from_str("fn main() { let x = \"hi\"; }\n");
        let syntax = parse(rust().unwrap(), &text).unwrap();
        assert!(!syntax.spans.is_empty());
        // "fn" at chars 0..2 is a keyword.
        assert_eq!(group_at(&syntax.spans, 0), 3);
        // The string literal is a string.
        let quote = text.to_string().find('"').unwrap();
        assert_eq!(group_at(&syntax.spans, quote + 1), 2);
    }

    #[test]
    fn every_bundled_grammar_loads_and_highlights() {
        for (file, source, probe) in [
            ("x.toml", "# note\nkey = \"v\"\n", 0usize),
            ("x.json", "{\"k\": \"v\"}\n", 1),
            ("x.py", "# note\ndef f():\n    pass\n", 0),
            ("x.sh", "# note\necho hi\n", 0),
            ("x.js", "// note\nlet x = 1;\n", 0),
            ("x.ts", "// note\nlet x: number = 1;\n", 0),
            ("x.tsx", "// note\nlet x = <a/>;\n", 0),
            ("x.html", "<!-- note --><p>hi</p>\n", 0),
            ("x.css", "/* note */ a { color: red; }\n", 0),
            ("x.c", "// note\nint main() {}\n", 0),
            ("x.cpp", "// note\nint main() {}\n", 0),
            ("x.go", "// note\npackage main\n", 0),
            ("x.java", "// note\nclass A {}\n", 0),
            ("x.rb", "# note\ndef f; end\n", 0),
            ("x.php", "<?php // note\n", 6),
            ("x.cs", "// note\nclass A {}\n", 0),
            ("x.yml", "# note\nkey: v\n", 0),
            ("x.md", "# Title\n", 2),
            ("x.odin", "// note\nmain :: proc() {}\n", 0),
            ("x.zig", "// note\nconst x = 1;\n", 0),
            ("x.lua", "-- note\nlocal x = 1\n", 0),
        ] {
            let config = config_for(Some(Path::new(file))).unwrap_or_else(|| {
                panic!("grammar for {file} failed to load");
            });
            let syntax = parse(config, &Rope::from_str(source)).unwrap();
            assert!(!syntax.spans.is_empty(), "{file}: no spans");
            assert_ne!(group_at(&syntax.spans, probe), 0, "{file}: probe unstyled");
        }
    }

    #[test]
    fn markdown_inline_emphasis_is_highlighted() {
        let text = Rope::from_str("plain *em* and **strong** and `code`\n");
        let config = config_for(Some(Path::new("x.md"))).unwrap();
        let syntax = parse(config, &text).unwrap();
        let s = text.to_string();
        assert_eq!(group_at(&syntax.spans, s.find("em").unwrap()), ITALIC);
        assert_eq!(group_at(&syntax.spans, s.find("strong").unwrap()), BOLD);
        assert_eq!(group_at(&syntax.spans, s.find("code").unwrap()), 2);
        assert_eq!(group_at(&syntax.spans, s.find("plain").unwrap()), 0);
        // Emphasis carries no color of its own, only the attribute.
        assert_eq!(color(BOLD), None);
        assert_eq!(attrs(BOLD) & BOLD, BOLD);
    }

    /// Oxigen has no grammar, so `highlight` must reach the fallback lexer
    /// and color the same shapes tree-sitter would.
    #[test]
    fn oxigen_is_highlighted_without_a_grammar() {
        let src = "// note\n#[indent]\nstruct Point {\n    x <int>\n}\n\nfun main() {\n    p <Point>\n    println(\"hi\", True, 42)\n}\n";
        let text = Rope::from_str(src);
        let syntax = highlight(Some(Path::new("x.oxi")), &text, None).unwrap();
        assert!(syntax.tree.is_none(), "the fallback produces no tree");
        let at = |needle: &str| group_at(&syntax.spans, src.find(needle).unwrap());
        assert_eq!(at("// note"), 1);
        assert_eq!(at("#[indent]"), 7);
        assert_eq!(at("struct"), 3);
        assert_eq!(at("Point"), 5);
        assert_eq!(at("<int>"), 5);
        assert_eq!(at("fun"), 3);
        assert_eq!(at("main"), 3);
        assert_eq!(at("println"), 4);
        assert_eq!(at("\"hi\""), 2);
        assert_eq!(at("True"), 6);
        assert_eq!(at("42"), 6);
        assert_eq!(at("    p <Point>"), 0); // a plain binding stays uncolored
    }

    /// The `<` heuristic must not turn every comparison into a type.
    #[test]
    fn oxigen_comparisons_are_not_types() {
        let src = "repeat when n < limit {\n    d <= n\n    a<b\n}\n";
        let text = Rope::from_str(src);
        let syntax = highlight(Some(Path::new("x.oxi")), &text, None).unwrap();
        for (i, _) in src.match_indices('<') {
            assert_eq!(
                group_at(&syntax.spans, i),
                0,
                "`<` at {i} colored as a type"
            );
        }
    }

    #[test]
    fn oxigen_strings_swallow_their_contents() {
        let src = "s <string> := \"fun struct // 42\"\nt := \"\"\"line\nfun\"\"\"\n";
        let text = Rope::from_str(src);
        let syntax = highlight(Some(Path::new("x.oxi")), &text, None).unwrap();
        assert_eq!(group_at(&syntax.spans, src.find("fun struct").unwrap()), 2);
        assert_eq!(group_at(&syntax.spans, src.rfind("fun").unwrap()), 2);
    }

    #[test]
    fn group_at_prefers_the_innermost_span() {
        let spans = vec![(0, 10, 2), (3, 5, 6)];
        assert_eq!(group_at(&spans, 1), 2);
        assert_eq!(group_at(&spans, 4), 6);
        assert_eq!(group_at(&spans, 9), 2);
        assert_eq!(group_at(&spans, 10), 0);
    }
}
