//! `.editorconfig`: a project's own word on indentation and whitespace,
//! which wins over crow.toml for the files it covers.
//!
//! Files are read upward from the edited file's directory until one says
//! `root = true`; nearer files override farther ones, and within a file a
//! later section overrides an earlier one, as the spec has it.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// `indent_style`: tabs (true) or spaces (false).
    pub indent_tabs: Option<bool>,
    pub indent_size: Option<usize>,
    pub tab_width: Option<usize>,
    pub trim_trailing_whitespace: Option<bool>,
    pub insert_final_newline: Option<bool>,
    /// `end_of_line`: crlf (true) or lf (false).
    pub crlf: Option<bool>,
}

impl Settings {
    /// How wide a tab is drawn, and how deep one indent level goes: crow
    /// treats the two as one, so `tab_width` and `indent_size` stand in for
    /// each other.
    pub fn width(&self) -> Option<usize> {
        self.tab_width.or(self.indent_size).filter(|&w| w > 0)
    }
}

/// The settings that apply to `path`, which need not exist yet.
pub fn for_file(path: &Path) -> Settings {
    let Some(file) = absolute(path) else {
        return Settings::default();
    };
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    for dir in file.ancestors().skip(1) {
        if let Ok(text) = std::fs::read_to_string(dir.join(".editorconfig")) {
            let root = is_root(&text);
            found.push((dir.to_path_buf(), text));
            if root {
                break;
            }
        }
    }
    let mut settings = Settings::default();
    for (dir, text) in found.iter().rev() {
        apply(&mut settings, dir, text, &file);
    }
    settings
}

/// `path` made absolute without requiring it to exist: its directory is
/// canonicalized and its name put back.
fn absolute(path: &Path) -> Option<PathBuf> {
    if let Ok(p) = path.canonicalize() {
        return Some(p);
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.canonicalize().ok()?,
        _ => std::env::current_dir().ok()?,
    };
    Some(parent.join(path.file_name()?))
}

/// `root = true` before the first section.
fn is_root(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .take_while(|l| !l.starts_with('['))
        .filter_map(|l| l.split_once('='))
        .any(|(k, v)| {
            k.trim().eq_ignore_ascii_case("root") && v.trim().eq_ignore_ascii_case("true")
        })
}

/// Fold one `.editorconfig` (living in `dir`) into `settings` for `file`.
fn apply(settings: &mut Settings, dir: &Path, text: &str, file: &Path) {
    let Ok(rel) = file.strip_prefix(dir) else {
        return;
    };
    let rel = rel.to_string_lossy().replace('\\', "/");
    let mut active = false;
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(glob) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            active = glob_matches(glob, &rel);
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !active {
            continue;
        }
        let value = value.trim().to_ascii_lowercase();
        let flag = || match value.as_str() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None, // `unset`, or nonsense: back to crow's own default
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "indent_style" => {
                settings.indent_tabs = match value.as_str() {
                    "tab" => Some(true),
                    "space" => Some(false),
                    _ => None,
                }
            }
            "indent_size" => settings.indent_size = value.parse().ok(),
            "tab_width" => settings.tab_width = value.parse().ok(),
            "trim_trailing_whitespace" => settings.trim_trailing_whitespace = flag(),
            "insert_final_newline" => settings.insert_final_newline = flag(),
            "end_of_line" => {
                settings.crlf = match value.as_str() {
                    "crlf" => Some(true),
                    "lf" => Some(false),
                    _ => None,
                }
            }
            _ => {}
        }
    }
}

/// Whether an editorconfig section glob matches `rel`, the file's path from
/// the `.editorconfig`'s directory. A glob without a `/` matches the file
/// name in any directory; one with a `/` is anchored at that directory.
fn glob_matches(glob: &str, rel: &str) -> bool {
    let anywhere = !glob.contains('/');
    let glob = glob.strip_prefix('/').unwrap_or(glob);
    let mut re = String::from(if anywhere { "^(?:.*/)?" } else { "^" });
    let chars: Vec<char> = glob.chars().collect();
    let mut braces = 0;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' if chars.get(i + 1) == Some(&'*') => {
                re.push_str(".*");
                i += 1;
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            '[' => match chars[i..].iter().position(|&c| c == ']') {
                Some(len) => {
                    let class: String = chars[i + 1..i + len].iter().collect();
                    let class = class
                        .strip_prefix('!')
                        .map_or(class.clone(), |r| format!("^{r}"));
                    re.push_str(&format!("[{}]", class.replace('/', "")));
                    i += len;
                }
                None => re.push_str("\\["),
            },
            // `{1..3}`: a numeric range. ponytail: any number matches; the
            // bounds go unchecked.
            '{' if chars[i..]
                .iter()
                .position(|&c| c == '}')
                .is_some_and(|end| {
                    let inner: String = chars[i + 1..i + end].iter().collect();
                    inner.contains("..") && !inner.contains(',')
                }) =>
            {
                let end = chars[i..].iter().position(|&c| c == '}').unwrap();
                re.push_str("[+-]?\\d+");
                i += end;
            }
            '{' => {
                braces += 1;
                re.push_str("(?:");
            }
            '}' if braces > 0 => {
                braces -= 1;
                re.push(')');
            }
            ',' if braces > 0 => re.push('|'),
            '\\' if i + 1 < chars.len() => {
                i += 1;
                re.push_str(&regex::escape(&chars[i].to_string()));
            }
            _ => re.push_str(&regex::escape(&c.to_string())),
        }
        i += 1;
    }
    re.push('$');
    regex::Regex::new(&re).is_ok_and(|re| re.is_match(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_follow_the_editorconfig_rules() {
        assert!(glob_matches("*", "src/main.rs"));
        assert!(
            glob_matches("*.rs", "src/deep/main.rs"),
            "no slash: any directory"
        );
        assert!(!glob_matches("*.rs", "main.rsx"));
        assert!(glob_matches("*.{js,py}", "a/b.py"));
        assert!(glob_matches("{package.json,.travis.yml}", ".travis.yml"));
        assert!(glob_matches("lib/**.js", "lib/a/b.js"));
        assert!(
            !glob_matches("lib/*.js", "lib/a/b.js"),
            "* stops at a slash"
        );
        assert!(
            !glob_matches("lib/*.js", "x/lib/a.js"),
            "a slash anchors it"
        );
        assert!(glob_matches("Makefile", "sub/Makefile"));
        assert!(glob_matches("file[0-9].txt", "file7.txt"));
        assert!(glob_matches("v{1..3}.txt", "v2.txt"));
    }

    #[test]
    fn nearer_files_and_later_sections_win_up_to_the_root() {
        let top = std::env::temp_dir().join(format!("crow-ec-{}", std::process::id()));
        let sub = top.join("proj/sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(top.join(".editorconfig"), "[*]\nindent_style = tab\n").unwrap();
        std::fs::write(
            top.join("proj/.editorconfig"),
            "root = true\n[*]\nindent_style = space\nindent_size = 2\n\
             end_of_line = crlf\n[*.md]\ntrim_trailing_whitespace = false\n",
        )
        .unwrap();
        std::fs::write(sub.join(".editorconfig"), "[*.rs]\nindent_size = 4\n").unwrap();

        let rs = for_file(&sub.join("main.rs"));
        assert_eq!(
            rs.indent_tabs,
            Some(false),
            "root stops the walk before the tab rule"
        );
        assert_eq!(rs.width(), Some(4), "the nearer file wins");
        assert_eq!(rs.crlf, Some(true));
        let md = for_file(&sub.join("notes.md"));
        assert_eq!(md.width(), Some(2));
        assert_eq!(md.trim_trailing_whitespace, Some(false));
        let _ = std::fs::remove_dir_all(&top);
    }
}
