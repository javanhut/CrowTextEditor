//! The file tree sidebar: a persistent, expandable directory view.
//!
//! State is just the set of expanded directories; the visible rows are
//! re-flattened from the filesystem whenever that set changes, so the tree
//! is never stale after an expand/collapse or refresh.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub struct Row {
    pub path: PathBuf,
    pub name: String,
    pub depth: usize,
    pub is_dir: bool,
    pub expanded: bool,
}

pub struct FileTree {
    pub root: PathBuf,
    expanded: HashSet<PathBuf>,
    pub rows: Vec<Row>,
    pub selected: usize,
}

impl FileTree {
    pub fn new(root: PathBuf) -> FileTree {
        let mut tree = FileTree {
            root,
            expanded: HashSet::new(),
            rows: Vec::new(),
            selected: 0,
        };
        tree.rebuild();
        tree
    }

    /// Re-flatten the visible rows from the filesystem. Row 0 is always the
    /// root itself — the header, and the way "add/paste at the root" gets a
    /// selectable target.
    pub fn rebuild(&mut self) {
        let root = self.root.clone();
        self.rows.clear();
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.to_string_lossy().into_owned());
        self.rows.push(Row {
            path: root.clone(),
            name,
            depth: 0,
            is_dir: true,
            expanded: true,
        });
        self.walk(&root, 1);
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
    }

    fn walk(&mut self, dir: &Path, depth: usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut dirs = Vec::new();
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !crate::config::show_hidden()
                && (name.starts_with('.') || name == "target" || name == "node_modules")
            {
                continue;
            }
            if entry.path().is_dir() {
                dirs.push((name, entry.path()));
            } else {
                files.push((name, entry.path()));
            }
        }
        dirs.sort();
        files.sort();
        for (name, path) in dirs {
            let expanded = self.expanded.contains(&path);
            self.rows.push(Row {
                path: path.clone(),
                name,
                depth,
                is_dir: true,
                expanded,
            });
            if expanded {
                self.walk(&path, depth + 1);
            }
        }
        for (name, path) in files {
            self.rows.push(Row {
                path,
                name,
                depth,
                is_dir: false,
                expanded: false,
            });
        }
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.rows.get(self.selected)
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len() as isize;
        self.selected = (self.selected as isize + delta).clamp(0, n - 1) as usize;
    }

    /// Expand or collapse the selected directory. The root row refreshes
    /// instead — collapsing it would hide the whole tree.
    pub fn toggle_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if !row.is_dir {
            return;
        }
        if row.path == self.root {
            self.rebuild();
            return;
        }
        let path = row.path.clone();
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        self.rebuild();
    }

    /// Expand every ancestor of `path` and put the selection on its row —
    /// how a freshly created file becomes visible.
    pub fn reveal(&mut self, path: &Path) {
        let mut dir = path.parent();
        while let Some(d) = dir {
            if d == self.root || !d.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(d.to_path_buf());
            dir = d.parent();
        }
        self.rebuild();
        if let Some(i) = self.rows.iter().position(|r| r.path == path) {
            self.selected = i;
        }
    }

    /// Collapse the selected directory, or jump to the parent row.
    pub fn collapse_or_parent(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if row.is_dir && row.expanded {
            self.toggle_selected();
            return;
        }
        let depth = row.depth;
        if depth == 0 {
            return;
        }
        let mut i = self.selected;
        while i > 0 {
            i -= 1;
            if self.rows[i].depth < depth {
                self.selected = i;
                return;
            }
        }
    }
}

/// Copy a file, or a directory and everything under it.
pub fn copy_recursively(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)?.flatten() {
            copy_recursively(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(src, dst).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_tree() -> (PathBuf, FileTree) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("crow-tree-test-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        std::fs::write(dir.join("sub/inner.txt"), "").unwrap();
        let tree = FileTree::new(dir.clone());
        (dir, tree)
    }

    #[test]
    fn root_row_first_then_dirs_then_files_sorted() {
        let (_dir, tree) = scratch_tree();
        let names: Vec<&str> = tree.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names[1..], ["sub", "a.txt", "b.txt"]);
        assert_eq!(tree.rows[0].path, tree.root);
        assert!(tree.rows[0].is_dir);
    }

    #[test]
    fn expanding_inlines_children_and_collapsing_removes_them() {
        let (_dir, mut tree) = scratch_tree();
        tree.selected = 1; // "sub"
        tree.toggle_selected();
        let names: Vec<&str> = tree.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names[1..], ["sub", "inner.txt", "a.txt", "b.txt"]);
        assert_eq!(tree.rows[2].depth, 2);
        tree.toggle_selected();
        assert_eq!(tree.rows.len(), 4);
    }

    #[test]
    fn toggling_the_root_row_never_collapses_the_tree() {
        let (_dir, mut tree) = scratch_tree();
        tree.selected = 0;
        tree.toggle_selected();
        assert_eq!(tree.rows.len(), 4); // still all visible
    }
}
