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

    /// Re-flatten the visible rows from the filesystem.
    pub fn rebuild(&mut self) {
        let root = self.root.clone();
        self.rows.clear();
        self.walk(&root, 0);
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
            if name.starts_with('.') || name == "target" || name == "node_modules" {
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

    /// Expand or collapse the selected directory.
    pub fn toggle_selected(&mut self) {
        let Some(row) = self.selected_row() else {
            return;
        };
        if !row.is_dir {
            return;
        }
        let path = row.path.clone();
        if !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        self.rebuild();
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

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_tree() -> (PathBuf, FileTree) {
        let dir = std::env::temp_dir().join(format!("crow-tree-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("b.txt"), "").unwrap();
        std::fs::write(dir.join("a.txt"), "").unwrap();
        std::fs::write(dir.join("sub/inner.txt"), "").unwrap();
        let tree = FileTree::new(dir.clone());
        (dir, tree)
    }

    #[test]
    fn dirs_first_then_files_sorted() {
        let (_dir, tree) = scratch_tree();
        let names: Vec<&str> = tree.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "a.txt", "b.txt"]);
    }

    #[test]
    fn expanding_inlines_children_and_collapsing_removes_them() {
        let (_dir, mut tree) = scratch_tree();
        tree.selected = 0; // "sub"
        tree.toggle_selected();
        let names: Vec<&str> = tree.rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["sub", "inner.txt", "a.txt", "b.txt"]);
        assert_eq!(tree.rows[1].depth, 1);
        tree.toggle_selected();
        assert_eq!(tree.rows.len(), 3);
    }
}
