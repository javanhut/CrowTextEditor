//! Tree-sitter parsing: syntax highlighting, and syntax nodes as selections.
//!
//! Rust only for now — each additional language is one grammar dependency and
//! one arm in `config_for`. Highlight groups are deliberately coarse: a
//! handful of colors reads better in a terminal than a full theme.

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
}

pub struct Syntax {
    pub config: &'static Config,
    pub tree: Tree,
    /// Highlight spans as (start, end, group) char ranges, sorted by start.
    pub spans: Vec<(usize, usize, u8)>,
}

/// Terminal color for a highlight group, from the active theme.
pub fn color(group: u8) -> Option<Color> {
    crate::theme::current().syntax[(group as usize).min(7)]
}

fn group_of(capture_name: &str) -> u8 {
    match capture_name.split('.').next().unwrap_or("") {
        "comment" => 1,
        "string" | "character" => 2,
        "keyword" => 3,
        "function" | "constructor" => 4,
        "type" => 5,
        "constant" | "number" | "escape" | "boolean" => 6,
        "macro" | "attribute" => 7,
        _ => 0,
    }
}

/// One lazily built grammar config; `None` if its bundled query fails to
/// compile (a broken grammar should degrade to plain text, not a panic).
macro_rules! lang {
    ($fn_name:ident, $language:expr, $query:expr) => {
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
                })
            })
            .as_ref()
        }
    };
}

lang!(rust, tree_sitter_rust::LANGUAGE, tree_sitter_rust::HIGHLIGHTS_QUERY);
lang!(toml, tree_sitter_toml_ng::LANGUAGE, tree_sitter_toml_ng::HIGHLIGHTS_QUERY);
lang!(json, tree_sitter_json::LANGUAGE, tree_sitter_json::HIGHLIGHTS_QUERY);
lang!(python, tree_sitter_python::LANGUAGE, tree_sitter_python::HIGHLIGHTS_QUERY);
lang!(bash, tree_sitter_bash::LANGUAGE, tree_sitter_bash::HIGHLIGHT_QUERY);
lang!(javascript, tree_sitter_javascript::LANGUAGE, tree_sitter_javascript::HIGHLIGHT_QUERY);

pub fn config_for(path: Option<&Path>) -> Option<&'static Config> {
    match path?.extension()?.to_str()? {
        "rs" => rust(),
        "toml" => toml(),
        "json" => json(),
        "py" => python(),
        "sh" | "bash" | "zsh" => bash(),
        "js" | "jsx" | "mjs" => javascript(),
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
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&config.query, tree.root_node(), src.as_bytes());
    while let Some(m) = matches.next() {
        for cap in m.captures {
            let group = config.groups[cap.index as usize];
            if group == 0 {
                continue;
            }
            let r = cap.node.byte_range();
            if r.start < r.end {
                spans.push((text.byte_to_char(r.start), text.byte_to_char(r.end), group));
            }
        }
    }
    spans.sort_unstable();
    Some(Syntax {
        config,
        tree,
        spans,
    })
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
    fn group_at_prefers_the_innermost_span() {
        let spans = vec![(0, 10, 2), (3, 5, 6)];
        assert_eq!(group_at(&spans, 1), 2);
        assert_eq!(group_at(&spans, 4), 6);
        assert_eq!(group_at(&spans, 9), 2);
        assert_eq!(group_at(&spans, 10), 0);
    }
}
