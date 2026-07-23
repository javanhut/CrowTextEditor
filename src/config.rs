//! crow.toml — the only file you edit.
//!
//! The NvCrow idea, native: a declarative spec — theme, options, keys,
//! language servers — with no programming language in the config. Names in,
//! wiring out. Everything is optional; a missing file means defaults, and the
//! first run writes a commented template to grow from.
//!
//! Parsed with a tiny TOML subset — `[sections]`, `key = value` with quoted
//! strings and integers, `#` comments. ponytail: swap in the `toml` crate if
//! the config ever needs arrays or nesting.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Config {
    pub theme: String,
    pub tab_width: usize,
    pub scrolloff: usize,
    /// Extra bindings per mode: (key sequence, command name).
    pub keys_normal: Vec<(String, String)>,
    pub keys_insert: Vec<(String, String)>,
    /// Language servers: (file extension, server command line).
    pub lsp: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "default".into(),
            tab_width: 4,
            scrolloff: 3,
            keys_normal: Vec::new(),
            keys_insert: Vec::new(),
            lsp: vec![("rs".into(), "rust-analyzer".into())],
        }
    }
}

// Options read from hot paths live in statics, set once by `apply`.
static TAB_WIDTH: AtomicUsize = AtomicUsize::new(4);
static SCROLLOFF: AtomicUsize = AtomicUsize::new(3);

pub fn tab_width() -> usize {
    TAB_WIDTH.load(Ordering::Relaxed)
}

pub fn scrolloff() -> usize {
    SCROLLOFF.load(Ordering::Relaxed)
}

/// Install the config's options and theme as the live values.
pub fn apply(config: &Config) {
    TAB_WIDTH.store(config.tab_width.clamp(1, 16), Ordering::Relaxed);
    SCROLLOFF.store(config.scrolloff.min(50), Ordering::Relaxed);
    crate::theme::set(&config.theme);
}

pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default()
        .join("crow/crow.toml")
}

const TEMPLATE: &str = r#"# crow.toml — crow's config. Everything here is optional;
# delete a line and the default comes back.

theme = "default"        # default | gruvbox | mono

[options]
tab_width = 4
scrolloff = 3

# Language servers: file extension = server command. crow starts the first
# server whose extension matches an open file.
[lsp]
rs = "rust-analyzer"
# py = "pyright-langserver --stdio"
# go = "gopls"

# Extra keybindings: "sequence" = "command". Any command callable with
# `:name` can be bound. Later bindings win over defaults.
[keys.normal]
# "gq" = "quit"
# "C-p" = "search"

[keys.insert]
"#;

/// Read the config, creating a commented template on first run.
pub fn load() -> Config {
    let path = path();
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text),
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, TEMPLATE);
            Config::default()
        }
    }
}

fn parse(text: &str) -> Config {
    let mut config = Config {
        lsp: Vec::new(), // replaced wholesale if the file has an [lsp] section
        ..Config::default()
    };
    let mut has_lsp_section = false;
    let mut section = String::new();

    for raw in text.lines() {
        let line = strip_comment(raw).trim().to_string();
        if line.is_empty() {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = name.trim().to_string();
            if section == "lsp" {
                has_lsp_section = true;
            }
            continue;
        }
        let Some((key, value)) = split_kv(&line) else {
            continue;
        };
        match section.as_str() {
            "" => {
                if key == "theme" {
                    config.theme = value;
                }
            }
            "options" => match key.as_str() {
                "tab_width" => config.tab_width = value.parse().unwrap_or(config.tab_width),
                "scrolloff" => config.scrolloff = value.parse().unwrap_or(config.scrolloff),
                _ => {}
            },
            "keys.normal" => config.keys_normal.push((key, value)),
            "keys.insert" => config.keys_insert.push((key, value)),
            "lsp" => config.lsp.push((key, value)),
            _ => {}
        }
    }

    if !has_lsp_section {
        config.lsp = Config::default().lsp;
    }
    config
}

/// Drop a `#` comment, ignoring `#` inside quoted strings.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..i],
            _ => {}
        }
    }
    line
}

/// `key = value` with optionally quoted key and value.
fn split_kv(line: &str) -> Option<(String, String)> {
    let eq = find_unquoted_eq(line)?;
    let key = unquote(line[..eq].trim());
    let value = unquote(line[eq + 1..].trim());
    if key.is_empty() {
        None
    } else {
        Some((key, value))
    }
}

fn find_unquoted_eq(line: &str) -> Option<usize> {
    let mut in_string = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_string = !in_string,
            '=' if !in_string => return Some(i),
            _ => {}
        }
    }
    None
}

fn unquote(s: &str) -> String {
    s.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(s)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_full_config() {
        let config = parse(
            r##"
theme = "gruvbox"  # comment after value

[options]
tab_width = 8
scrolloff = 5

[keys.normal]
"C-p" = "search"
gq = "quit"

[keys.insert]
"C-s" = "save"

[lsp]
py = "pyright-langserver --stdio"
"##,
        );
        assert_eq!(config.theme, "gruvbox");
        assert_eq!(config.tab_width, 8);
        assert_eq!(config.scrolloff, 5);
        assert_eq!(config.keys_normal, vec![
            ("C-p".to_string(), "search".to_string()),
            ("gq".to_string(), "quit".to_string()),
        ]);
        assert_eq!(config.keys_insert, vec![("C-s".to_string(), "save".to_string())]);
        assert_eq!(config.lsp, vec![("py".to_string(), "pyright-langserver --stdio".to_string())]);
    }

    #[test]
    fn missing_sections_mean_defaults() {
        let config = parse("theme = \"mono\"\n");
        assert_eq!(config.theme, "mono");
        assert_eq!(config.tab_width, 4);
        // No [lsp] section: the built-in rust-analyzer entry stays.
        assert_eq!(config.lsp, Config::default().lsp);
    }

    #[test]
    fn an_lsp_section_replaces_the_default_table() {
        let config = parse("[lsp]\ngo = \"gopls\"\n");
        assert_eq!(config.lsp, vec![("go".to_string(), "gopls".to_string())]);
    }

    #[test]
    fn comments_and_junk_are_ignored() {
        let config = parse("# hello\ntheme = \"x # not a comment\"\nnoise without equals\n");
        assert_eq!(config.theme, "x # not a comment");
    }

    #[test]
    fn the_template_parses_to_defaults() {
        let config = parse(TEMPLATE);
        assert_eq!(config.theme, "default");
        assert_eq!(config.tab_width, 4);
        assert_eq!(config.scrolloff, 3);
        assert!(config.keys_normal.is_empty());
        assert_eq!(config.lsp, Config::default().lsp);
    }
}
