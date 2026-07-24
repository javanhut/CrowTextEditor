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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

pub struct Config {
    pub theme: String,
    pub tab_width: usize,
    pub scrolloff: usize,
    pub autoclose: bool,
    pub icons: bool,
    pub format_on_save: bool,
    /// Extra bindings per mode: (key sequence, command name).
    pub keys_normal: Vec<(String, String)>,
    pub keys_insert: Vec<(String, String)>,
    /// Language servers: (file extension, server command line).
    pub lsp: Vec<(String, String)>,
    /// Formatters: (file extension, command reading stdin, writing stdout).
    /// Entries here override the built-in table.
    pub fmt: Vec<(String, String)>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            theme: "tokyonight".into(),
            tab_width: 4,
            scrolloff: 3,
            autoclose: true,
            icons: true,
            format_on_save: true,
            keys_normal: Vec::new(),
            keys_insert: Vec::new(),
            lsp: vec![("rs".into(), "rust-analyzer".into())],
            fmt: Vec::new(),
        }
    }
}

// Options read from hot paths live in statics, set once by `apply`.
static TAB_WIDTH: AtomicUsize = AtomicUsize::new(4);
static SCROLLOFF: AtomicUsize = AtomicUsize::new(3);
static AUTOCLOSE: AtomicBool = AtomicBool::new(true);
static ICONS: AtomicBool = AtomicBool::new(true);
static FORMAT_ON_SAVE: AtomicBool = AtomicBool::new(true);

pub fn tab_width() -> usize {
    TAB_WIDTH.load(Ordering::Relaxed)
}

pub fn scrolloff() -> usize {
    SCROLLOFF.load(Ordering::Relaxed)
}

pub fn autoclose() -> bool {
    AUTOCLOSE.load(Ordering::Relaxed)
}

pub fn icons() -> bool {
    ICONS.load(Ordering::Relaxed)
}

pub fn format_on_save() -> bool {
    FORMAT_ON_SAVE.load(Ordering::Relaxed)
}

/// Install the config's options and theme as the live values.
pub fn apply(config: &Config) {
    TAB_WIDTH.store(config.tab_width.clamp(1, 16), Ordering::Relaxed);
    SCROLLOFF.store(config.scrolloff.min(50), Ordering::Relaxed);
    AUTOCLOSE.store(config.autoclose, Ordering::Relaxed);
    ICONS.store(config.icons, Ordering::Relaxed);
    FORMAT_ON_SAVE.store(config.format_on_save, Ordering::Relaxed);
    crate::theme::set(&config.theme);
    let _ = FMT.set(config.fmt.clone());
}

static FMT: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();

/// Built-in formatters, all reading the buffer on stdin and writing the
/// result to stdout. `{file}` becomes the buffer's path (for tools that
/// pick style/parser from the filename).
const BUILTIN_FMT: &[(&str, &str)] = &[
    ("rs", "rustfmt --edition 2021"),
    ("go", "gofmt"),
    ("py", "black -q -"),
    ("sh", "shfmt"),
    ("bash", "shfmt"),
    ("zig", "zig fmt --stdin"),
    ("lua", "stylua -"),
    ("toml", "taplo fmt -"),
    ("odin", "odinfmt -stdin"),
    ("c", "clang-format --assume-filename={file}"),
    ("h", "clang-format --assume-filename={file}"),
    ("cpp", "clang-format --assume-filename={file}"),
    ("hpp", "clang-format --assume-filename={file}"),
    ("cc", "clang-format --assume-filename={file}"),
    ("cxx", "clang-format --assume-filename={file}"),
    ("hh", "clang-format --assume-filename={file}"),
    ("java", "clang-format --assume-filename={file}"),
    ("js", "prettier --stdin-filepath {file}"),
    ("jsx", "prettier --stdin-filepath {file}"),
    ("mjs", "prettier --stdin-filepath {file}"),
    ("cjs", "prettier --stdin-filepath {file}"),
    ("ts", "prettier --stdin-filepath {file}"),
    ("mts", "prettier --stdin-filepath {file}"),
    ("tsx", "prettier --stdin-filepath {file}"),
    ("json", "prettier --stdin-filepath {file}"),
    ("css", "prettier --stdin-filepath {file}"),
    ("html", "prettier --stdin-filepath {file}"),
    ("htm", "prettier --stdin-filepath {file}"),
    ("md", "prettier --stdin-filepath {file}"),
    ("markdown", "prettier --stdin-filepath {file}"),
    ("yml", "prettier --stdin-filepath {file}"),
    ("yaml", "prettier --stdin-filepath {file}"),
];

/// The formatter command line for a file extension: config entries first,
/// then the built-in table.
pub fn formatter(ext: &str) -> Option<String> {
    if let Some(user) = FMT.get() {
        if let Some((_, command)) = user.iter().find(|(e, _)| e == ext) {
            return Some(command.clone());
        }
    }
    BUILTIN_FMT
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, command)| command.to_string())
}

pub fn path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default()
        .join("crow/crow.toml")
}

/// The recent-files list, most recent first (XDG state, not config).
fn recent_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_default()
        .join("crow/recent")
}

/// Recently opened files that still exist, most recent first.
pub fn recent_files() -> Vec<PathBuf> {
    std::fs::read_to_string(recent_path())
        .unwrap_or_default()
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.is_file())
        .collect()
}

/// Move `path` to the front of the recent-files list.
pub fn record_recent(path: &Path) {
    let Ok(abs) = path.canonicalize() else {
        return;
    };
    let mut lines = vec![abs.to_string_lossy().into_owned()];
    lines.extend(
        recent_files()
            .into_iter()
            .filter(|p| *p != abs)
            .map(|p| p.to_string_lossy().into_owned()),
    );
    lines.truncate(50);
    let file = recent_path();
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, lines.join("\n"));
}

const TEMPLATE: &str = r#"# crow.toml — crow's config. Everything here is optional;
# delete a line and the default comes back.

theme = "tokyonight"     # tokyonight | gruvbox | mono | default (terminal colors)

[options]
tab_width = 4
scrolloff = 3
autoclose = true         # type ( [ { " ' and the closer appears
icons = true             # Nerd Font file icons in the tree (needs a Nerd Font)
format_on_save = true    # pipe the buffer through its [fmt] formatter on :w

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

# Formatters for :fmt — file extension = command reading stdin, writing
# stdout ({file} becomes the buffer's path). Common tools (rustfmt, gofmt,
# black, prettier, clang-format...) are built in; entries here override.
[fmt]
# py = "ruff format -"
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
                "autoclose" => config.autoclose = value.parse().unwrap_or(config.autoclose),
                "icons" => config.icons = value.parse().unwrap_or(config.icons),
                "format_on_save" => {
                    config.format_on_save = value.parse().unwrap_or(config.format_on_save)
                }
                _ => {}
            },
            "keys.normal" => config.keys_normal.push((key, value)),
            "keys.insert" => config.keys_insert.push((key, value)),
            "lsp" => config.lsp.push((key, value)),
            "fmt" => config.fmt.push((key, value)),
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
    fn formatter_lookup_falls_back_to_builtins() {
        assert_eq!(formatter("go").as_deref(), Some("gofmt"));
        assert!(formatter("xyz").is_none());
    }

    #[test]
    fn a_fmt_section_is_parsed() {
        let config = parse("[fmt]\npy = \"ruff format -\"\n");
        assert_eq!(config.fmt, vec![("py".to_string(), "ruff format -".to_string())]);
    }

    #[test]
    fn the_template_parses_to_defaults() {
        let config = parse(TEMPLATE);
        assert_eq!(config.theme, "tokyonight");
        assert_eq!(config.tab_width, 4);
        assert_eq!(config.scrolloff, 3);
        assert!(config.keys_normal.is_empty());
        assert_eq!(config.lsp, Config::default().lsp);
    }

    #[test]
    fn recent_files_dedupe_most_recent_first() {
        let state = std::env::temp_dir().join("crow-recent-test");
        std::fs::create_dir_all(&state).unwrap();
        std::env::set_var("XDG_STATE_HOME", &state);
        let _ = std::fs::remove_file(state.join("crow/recent"));
        let a = state.join("a.txt");
        let b = state.join("b.txt");
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        record_recent(&a);
        record_recent(&b);
        record_recent(&a); // back to the front, not duplicated
        let names: Vec<String> = recent_files()
            .iter()
            .filter_map(|p| Some(p.file_name()?.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(names, ["a.txt", "b.txt"]);
        std::env::remove_var("XDG_STATE_HOME");
    }
}
