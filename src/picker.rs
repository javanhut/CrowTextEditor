//! The popup picker: one overlay widget, many sources.
//!
//! Commands, themes, files, and directories all go through the same fuzzy
//! list — type to filter, arrows or C-n/C-p to move, Enter to accept, Esc to
//! cancel. Adding a source is a constructor and an arm in the editor's
//! accept handler.

use std::path::{Path, PathBuf};

pub struct Item {
    pub label: String,
    pub detail: String,
}

pub enum Kind {
    /// Run the selected command by name.
    Command,
    /// Live-previews while browsing; Esc restores `original`.
    Theme { original: &'static str },
    /// Open the selected file (labels are paths relative to the root).
    Files { root: PathBuf },
    /// Browse a directory: Enter descends into `dir/label` or opens a file.
    Explorer { dir: PathBuf },
    /// Live content search: the query greps files, labels are `path:line`.
    Grep { root: PathBuf },
    /// Recently opened files; labels are absolute paths (`~`-shortened).
    Recent,
}

pub struct Picker {
    pub title: String,
    pub kind: Kind,
    pub items: Vec<Item>,
    pub query: String,
    /// Indices into `items`, best match first.
    pub filtered: Vec<usize>,
    /// Index into `filtered`.
    pub selected: usize,
}

impl Picker {
    fn new(title: impl Into<String>, kind: Kind, items: Vec<Item>) -> Picker {
        let mut picker = Picker {
            title: title.into(),
            kind,
            items,
            query: String::new(),
            filtered: Vec::new(),
            selected: 0,
        };
        picker.refilter();
        picker
    }

    pub fn commands(keymap: &crate::keymap::KeyTrie) -> Picker {
        let items = crate::commands::COMMANDS
            .iter()
            .map(|c| Item {
                label: c.name.to_string(),
                detail: match keymap.binding_of(c.name) {
                    Some(keys) => format!("{keys}  ·  {}", c.doc),
                    None => c.doc.to_string(),
                },
            })
            .collect();
        Picker::new("command", Kind::Command, items)
    }

    pub fn themes() -> Picker {
        let original = crate::theme::current().name;
        let items = crate::theme::THEMES
            .iter()
            .map(|t| Item {
                label: t.name.to_string(),
                detail: String::new(),
            })
            .collect();
        Picker::new("theme", Kind::Theme { original }, items)
    }

    pub fn files(root: &Path) -> Picker {
        let items = list_files(root)
            .into_iter()
            .map(|p| Item {
                label: p,
                detail: String::new(),
            })
            .collect();
        Picker::new(
            "file",
            Kind::Files {
                root: root.to_path_buf(),
            },
            items,
        )
    }

    pub fn explorer(dir: PathBuf) -> Picker {
        let items = list_dir(&dir);
        let title = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| dir.to_string_lossy().into_owned());
        Picker::new(title, Kind::Explorer { dir }, items)
    }

    pub fn recent() -> Picker {
        let home = std::env::var("HOME").unwrap_or_default();
        let items = crate::config::recent_files()
            .into_iter()
            .map(|p| {
                let s = p.to_string_lossy().into_owned();
                let label = match s.strip_prefix(&home) {
                    Some(rest) if !home.is_empty() => format!("~{rest}"),
                    _ => s,
                };
                Item {
                    label,
                    detail: String::new(),
                }
            })
            .collect();
        Picker::new("recent", Kind::Recent, items)
    }

    pub fn grep(root: &Path) -> Picker {
        Picker::new(
            "grep",
            Kind::Grep {
                root: root.to_path_buf(),
            },
            Vec::new(),
        )
    }

    /// React to a query change: Grep pickers re-search file contents, every
    /// other kind fuzzy-refilters its fixed item list.
    pub fn requery(&mut self) {
        if let Kind::Grep { root } = &self.kind {
            self.items = grep_files(root, &self.query);
            self.filtered = (0..self.items.len()).collect();
            self.selected = 0;
        } else {
            self.refilter();
        }
    }

    pub fn refilter(&mut self) {
        let mut scored: Vec<(i64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| fuzzy_score(&self.query, &item.label).map(|s| (s, i)))
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.filtered = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.filtered.is_empty() {
            return;
        }
        let n = self.filtered.len() as isize;
        self.selected = ((self.selected as isize + delta).rem_euclid(n)) as usize;
    }

    pub fn selected_item(&self) -> Option<&Item> {
        self.filtered.get(self.selected).map(|&i| &self.items[i])
    }
}

/// Subsequence fuzzy match: every query char must appear in order.
/// Consecutive hits and word starts score higher; shorter targets win ties.
pub fn fuzzy_score(query: &str, target: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = target.to_lowercase().chars().collect();
    let mut score = 0i64;
    let mut pos = 0usize;
    let mut last_hit: Option<usize> = None;

    for qc in query.to_lowercase().chars() {
        let mut hit = None;
        while pos < chars.len() {
            if chars[pos] == qc {
                hit = Some(pos);
                break;
            }
            pos += 1;
        }
        let i = hit?;
        score += 1;
        if last_hit == Some(i.wrapping_sub(1)) {
            score += 3; // consecutive
        }
        if i == 0 || !chars[i - 1].is_alphanumeric() {
            score += 2; // word start
        }
        last_hit = Some(i);
        pos = i + 1;
    }
    Some(score - (target.chars().count() as i64) / 8)
}

/// Every file under `root`, relative paths, skipping hidden entries and
/// build/vendor directories. ponytail: capped and synchronous — a background
/// walker with .gitignore support when big repos itch.
fn list_files(root: &Path) -> Vec<String> {
    const SKIP: &[&str] = &["target", "node_modules", "dist", "build"];
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= 5000 {
            break;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    out.sort();
    out
}

/// Case-insensitive substring search over every project file.
/// ponytail: a synchronous rescan per keystroke, capped at 100 hits; a
/// background ripgrep-style walker when big repos itch.
fn grep_files(root: &Path, query: &str) -> Vec<Item> {
    if query.chars().count() < 2 {
        return Vec::new(); // one char would light up the whole repo
    }
    let query = query.to_lowercase();
    let mut out = Vec::new();
    for rel in list_files(root) {
        let Ok(text) = std::fs::read_to_string(root.join(&rel)) else {
            continue; // binary or unreadable
        };
        for (i, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(&query) {
                out.push(Item {
                    label: format!("{rel}:{}", i + 1),
                    detail: line.trim().to_string(),
                });
                if out.len() >= 100 {
                    return out;
                }
            }
        }
    }
    out
}

/// One directory level: `../`, then subdirectories (marked with `/`), then
/// files, each group sorted.
fn list_dir(dir: &Path) -> Vec<Item> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if entry.path().is_dir() {
                dirs.push(name + "/");
            } else {
                files.push(name);
            }
        }
    }
    dirs.sort();
    files.sort();
    let mut items = vec![Item {
        label: "../".to_string(),
        detail: String::new(),
    }];
    items.extend(dirs.into_iter().chain(files).map(|label| Item {
        label,
        detail: String::new(),
    }));
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_requires_subsequence() {
        assert!(fuzzy_score("gd", "goto_definition").is_some());
        assert!(fuzzy_score("xyz", "goto_definition").is_none());
        assert!(fuzzy_score("", "anything").is_some());
    }

    #[test]
    fn fuzzy_prefers_word_starts_and_runs() {
        let tight = fuzzy_score("word", "word_start").unwrap();
        let scattered = fuzzy_score("word", "w_o_r_d_x").unwrap();
        assert!(tight > scattered);
    }

    #[test]
    fn grep_finds_matching_lines_by_path_and_number() {
        let dir = std::env::temp_dir().join("crow-grep-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\nthe needle is here\n").unwrap();
        let mut picker = Picker::grep(&dir);
        picker.query = "NEEDLE".into(); // case-insensitive
        picker.requery();
        let item = picker.selected_item().expect("one hit");
        assert_eq!(item.label, "a.txt:2");
        assert_eq!(item.detail, "the needle is here");
        picker.query = "n".into(); // too short: no full-repo scan
        picker.requery();
        assert!(picker.selected_item().is_none());
    }

    #[test]
    fn filtering_ranks_and_narrows() {
        let mut picker = Picker::commands(&crate::keymap::KeyTrie::new());
        let all = picker.filtered.len();
        picker.query = "quit".into();
        picker.refilter();
        assert!(!picker.filtered.is_empty());
        assert!(picker.filtered.len() < all);
        assert_eq!(picker.selected_item().unwrap().label, "quit");
    }
}
