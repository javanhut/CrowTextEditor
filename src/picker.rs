//! The popup picker: one overlay widget, many sources.
//!
//! Commands, themes, files, and directories all go through the same fuzzy
//! list — type to filter, arrows or C-n/C-p to move, Enter to accept, Esc to
//! cancel. Adding a source is a constructor and an arm in the editor's
//! accept handler.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;

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
    /// The search runs in a background thread — ripgrep when it is
    /// installed, a walker of our own when not — and streams its hits in, so
    /// a big repository never freezes the editor. `wake` pokes the main loop
    /// when a batch lands.
    Grep {
        root: PathBuf,
        search: Option<GrepSearch>,
        wake: Option<Sender<crate::terminal::Wake>>,
    },
    /// Recently opened files; labels are absolute paths (`~`-shortened).
    Recent,
    /// Places to jump to — references, symbols, diagnostics — one per item.
    Locations { locs: Vec<crate::lsp::Location> },
    /// Code actions from the language server, one per item.
    CodeActions { actions: Vec<serde_json::Value> },
    /// Workspace symbols: the server refills the list as the query changes.
    WorkspaceSymbols { locs: Vec<crate::lsp::Location> },
}

/// A grep in flight: its results arrive in batches, and setting `cancel`
/// tells it to stop because the query moved on.
pub struct GrepSearch {
    rx: Receiver<Vec<Item>>,
    cancel: Arc<AtomicBool>,
}

impl Drop for GrepSearch {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Hits a grep stops at; a common word would otherwise list the whole repo.
const GREP_LIMIT: usize = 1000;

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
    pub fn new(title: impl Into<String>, kind: Kind, items: Vec<Item>) -> Picker {
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

    pub fn grep(root: &Path, wake: Option<Sender<crate::terminal::Wake>>) -> Picker {
        Picker::new(
            "grep",
            Kind::Grep {
                root: root.to_path_buf(),
                search: None,
                wake,
            },
            Vec::new(),
        )
    }

    /// React to a query change: Grep pickers start a new search, every
    /// other kind fuzzy-refilters its item list.
    pub fn requery(&mut self) {
        if let Kind::Grep { root, search, wake } = &mut self.kind {
            // Dropping the old search cancels it.
            *search = None;
            self.items.clear();
            self.filtered.clear();
            self.selected = 0;
            if self.query.chars().count() >= 2 {
                // One char would light up the whole repo.
                *search = Some(start_grep(root.clone(), self.query.clone(), wake.clone()));
            }
        } else {
            self.refilter();
        }
    }

    /// Take in whatever a running grep has found since last time. True when
    /// the list grew.
    pub fn poll(&mut self) -> bool {
        let Kind::Grep { search, .. } = &mut self.kind else {
            return false;
        };
        let Some(running) = search.as_ref() else {
            return false;
        };
        let mut grew = false;
        loop {
            match running.rx.try_recv() {
                Ok(batch) => {
                    let start = self.items.len();
                    self.items.extend(batch);
                    self.filtered.extend(start..self.items.len());
                    grew = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    *search = None; // finished
                    break;
                }
            }
        }
        grew
    }

    /// Whether a grep is still running (the title says so).
    pub fn searching(&self) -> bool {
        matches!(
            &self.kind,
            Kind::Grep {
                search: Some(_),
                ..
            }
        )
    }

    pub fn refilter(&mut self) {
        let query = self.query.to_lowercase();
        let mut scored: Vec<(i64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| score_lowered(&query, &item.label).map(|s| (s, i)))
            .collect();
        scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
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

    /// Index into `items` of the highlighted row.
    pub fn selected_index(&self) -> Option<usize> {
        self.filtered.get(self.selected).copied()
    }
}

/// Subsequence fuzzy match: every query char must appear in order.
/// Consecutive hits and word starts score higher; shorter targets win ties.
pub fn fuzzy_score(query: &str, target: &str) -> Option<i64> {
    score_lowered(&query.to_lowercase(), target)
}

/// `fuzzy_score` with the query already lowercased, so filtering a 5000-item
/// list does that once instead of once per item. Neither side is collected
/// into a buffer — this used to allocate two Strings and a `Vec<char>` per
/// item per keystroke, which is what made the file picker feel gluey.
fn score_lowered(query: &str, target: &str) -> Option<i64> {
    let mut wanted = query.chars();
    let Some(mut want) = wanted.next() else {
        return Some(0);
    };

    let mut score = 0i64;
    let mut len = 0usize;
    let mut prev: Option<char> = None;
    let mut last_hit: Option<usize> = None;
    let mut matched = false;

    for (i, c) in target.chars().enumerate() {
        len += 1;
        // `to_lowercase` yields a sequence for a few characters; the first one
        // is what the old `String`-building version compared against too.
        let c = c.to_lowercase().next().unwrap_or(c);
        if !matched && c == want {
            score += 1;
            if last_hit == Some(i.wrapping_sub(1)) {
                score += 3; // consecutive
            }
            if prev.is_none_or(|p| !p.is_alphanumeric()) {
                score += 2; // word start
            }
            last_hit = Some(i);
            match wanted.next() {
                Some(next) => want = next,
                None => matched = true,
            }
        }
        prev = Some(c);
    }

    matched.then(|| score - (len as i64) / 8)
}

/// Every file under `root`, relative paths, skipping hidden entries and
/// build/vendor directories. ponytail: capped and synchronous — a background
/// walker with .gitignore support when big repos itch.
/// Directories no search wants to see: build output and vendored code.
const SKIP: &[&str] = &["target", "node_modules", "dist", "build"];

fn list_files(root: &Path) -> Vec<String> {
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
            if !crate::config::show_hidden()
                && (name.starts_with('.') || SKIP.contains(&name.as_str()))
            {
                continue;
            }
            let path = entry.path();
            // `file_type()` comes off the directory entry on every platform we
            // run on; `path.is_dir()` would be another stat per file.
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
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

/// Start searching `root` for `query` (case-insensitive, literal) in a
/// background thread; hits come back over the returned search's channel in
/// batches, as `path:line` items with the line as detail.
fn start_grep(
    root: PathBuf,
    query: String,
    wake: Option<Sender<crate::terminal::Wake>>,
) -> GrepSearch {
    let (tx, rx) = std::sync::mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = Arc::clone(&cancel);
    std::thread::spawn(move || {
        let send = |batch: Vec<Item>| {
            let ok = tx.send(batch).is_ok();
            if let Some(w) = &wake {
                let _ = w.send(crate::terminal::Wake::Refresh);
            }
            ok
        };
        if crate::config::on_path("rg") {
            ripgrep(&root, &query, &stop, send);
        } else {
            walk_grep(&root, &query, &stop, send);
        }
    });
    GrepSearch { rx, cancel }
}

/// Batches of this many hits go over the channel at a time.
const GREP_BATCH: usize = 64;

/// The search, by ripgrep: fast, and it knows `.gitignore`. The same
/// directories the file finder skips are skipped here, `.ivaldiignore` counts
/// too, and dotfiles follow the hidden toggle.
fn ripgrep(root: &Path, query: &str, stop: &AtomicBool, mut send: impl FnMut(Vec<Item>) -> bool) {
    use std::io::BufRead;
    let mut cmd = std::process::Command::new("rg");
    cmd.args([
        "--line-number",
        "--no-heading",
        "--color=never",
        "--null",
        "--fixed-strings",
        "--ignore-case",
        "--max-columns=300",
    ]);
    for dir in SKIP {
        cmd.arg("--glob").arg(format!("!{dir}"));
    }
    if crate::config::show_hidden() {
        cmd.arg("--hidden");
    }
    if root.join(".ivaldiignore").is_file() {
        cmd.arg("--ignore-file").arg(root.join(".ivaldiignore"));
    }
    cmd.arg("--").arg(query).arg(".");
    let Ok(mut child) = cmd
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return;
    };
    let Some(out) = child.stdout.take() else {
        return;
    };
    let mut batch = Vec::new();
    let mut total = 0;
    for line in std::io::BufReader::new(out).lines() {
        if stop.load(Ordering::Relaxed) || total >= GREP_LIMIT {
            break;
        }
        let Ok(line) = line else {
            continue; // not UTF-8
        };
        // `path\0line:text`
        let Some((path, rest)) = line.split_once('\0') else {
            continue;
        };
        let Some((num, text)) = rest.split_once(':') else {
            continue;
        };
        batch.push(Item {
            label: format!("{}:{num}", path.strip_prefix("./").unwrap_or(path)),
            detail: text.trim().to_string(),
        });
        total += 1;
        if batch.len() >= GREP_BATCH && !send(std::mem::take(&mut batch)) {
            break;
        }
    }
    if !batch.is_empty() {
        send(batch);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The search without ripgrep: the file finder's walk, each file read and
/// matched in turn.
fn walk_grep(root: &Path, query: &str, stop: &AtomicBool, mut send: impl FnMut(Vec<Item>) -> bool) {
    let query = query.to_lowercase();
    let mut batch = Vec::new();
    let mut total = 0;
    for rel in list_files(root) {
        if stop.load(Ordering::Relaxed) || total >= GREP_LIMIT {
            break;
        }
        let Ok(text) = std::fs::read_to_string(root.join(&rel)) else {
            continue; // binary or unreadable
        };
        if !text.to_lowercase().contains(&query) {
            continue;
        }
        for (i, line) in text.lines().enumerate() {
            if total >= GREP_LIMIT {
                break; // one file of matches must not outrun the whole cap
            }
            if line.to_lowercase().contains(&query) {
                batch.push(Item {
                    label: format!("{rel}:{}", i + 1),
                    detail: line.trim().to_string(),
                });
                total += 1;
            }
        }
        if batch.len() >= GREP_BATCH && !send(std::mem::take(&mut batch)) {
            return;
        }
    }
    if !batch.is_empty() {
        send(batch);
    }
}

/// One directory level: `../`, then subdirectories (marked with `/`), then
/// files, each group sorted.
fn list_dir(dir: &Path) -> Vec<Item> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') && !crate::config::show_hidden() {
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
    fn hidden_files_follow_the_toggle() {
        let dir = std::env::temp_dir().join("crow-hidden-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1").unwrap();
        std::fs::write(dir.join("main.rs"), "").unwrap();
        assert!(!crate::config::show_hidden(), "hidden by default");
        assert!(!list_files(&dir).iter().any(|f| f == ".env"));
        crate::config::toggle_hidden();
        assert!(list_files(&dir).iter().any(|f| f == ".env"));
        crate::config::toggle_hidden(); // restore for other tests
    }

    #[test]
    fn grep_finds_matching_lines_by_path_and_number() {
        let dir = std::env::temp_dir().join("crow-grep-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\nthe needle is here\n").unwrap();
        let mut picker = Picker::grep(&dir, None);
        picker.query = "NEEDLE".into(); // case-insensitive
        picker.requery();
        // The search streams in from a thread; give it a moment.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while picker.searching() && std::time::Instant::now() < deadline {
            picker.poll();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let item = picker.selected_item().expect("one hit");
        assert_eq!(item.label, "a.txt:2");
        assert_eq!(item.detail, "the needle is here");
        picker.query = "n".into(); // too short: no full-repo scan
        picker.requery();
        assert!(picker.selected_item().is_none());
        assert!(!picker.searching());

        // Both engines agree: the walker finds the same line.
        let stop = AtomicBool::new(false);
        let mut found = Vec::new();
        walk_grep(&dir, "NEEDLE", &stop, |batch| {
            found.extend(batch);
            true
        });
        assert_eq!(found[0].label, "a.txt:2");
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
