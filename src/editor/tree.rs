//! The file tree sidebar: its keys and the file operations it runs.

use super::*;

impl Editor {
    // ---- file tree ---------------------------------------------------------

    /// `space e` from anywhere: hidden -> shown+focused, unfocused -> focused,
    /// focused -> closed. Every state reaches every other with the same key.
    pub fn tree_toggle(&mut self) {
        match (&self.tree, self.tree_focused) {
            (None, _) => {
                let root = std::env::current_dir().unwrap_or_default();
                self.tree = Some(crate::filetree::FileTree::new(root));
                self.tree_focused = true;
            }
            (Some(_), false) => self.tree_focused = true,
            (Some(_), true) => {
                self.tree = None;
                self.tree_focused = false;
            }
        }
    }

    pub(crate) fn handle_tree_key(&mut self, key: Key) {
        if self.tree_input.is_some() {
            self.handle_tree_input_key(key);
            return;
        }
        // The tree owns the keyboard while focused, so it must recognize the
        // `space e` toggle itself — otherwise the sidebar could never close.
        if self.tree_leader {
            self.tree_leader = false;
            if key.code == KeyCode::Char('e') && !key.ctrl && !key.alt {
                self.tree_toggle();
            }
            return;
        }
        if key.code == KeyCode::Char(' ') && !key.ctrl && !key.alt {
            self.tree_leader = true;
            return;
        }
        let Some(tree) = self.tree.as_mut() else {
            self.tree_focused = false;
            return;
        };
        match key.code {
            KeyCode::Char('l') | KeyCode::Right if key.ctrl => self.tree_focused = false,
            KeyCode::Char('t') if key.ctrl => self.tree_toggle(),
            KeyCode::Esc => self.tree_focused = false,
            KeyCode::Char('q') => {
                self.tree = None;
                self.tree_focused = false;
            }
            KeyCode::Up | KeyCode::Char('k') => tree.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => tree.move_selection(1),
            KeyCode::Char('h') | KeyCode::Left => tree.collapse_or_parent(),
            KeyCode::Char('R') => tree.rebuild(),
            KeyCode::Char('.') => {
                (crate::commands::find("toggle_hidden").unwrap().func)(self);
            }
            KeyCode::Char('r') => {
                if let Some(row) = tree.selected_row() {
                    if row.path == tree.root {
                        self.set_status("the root can't be renamed from here");
                    } else {
                        self.tree_input = Some(TreeInput::Rename {
                            path: row.path.clone(),
                            name: row.name.clone(),
                        });
                    }
                }
            }
            KeyCode::Char('a') => {
                // New entries go into the selected directory, or beside the
                // selected file.
                let dir = match tree.selected_row() {
                    Some(row) if row.is_dir => row.path.clone(),
                    Some(row) => row.path.parent().unwrap_or(&tree.root).to_path_buf(),
                    None => tree.root.clone(),
                };
                self.tree_input = Some(TreeInput::Create {
                    dir,
                    name: String::new(),
                });
            }
            KeyCode::Char('d') => {
                if let Some(row) = tree.selected_row() {
                    if row.path == tree.root {
                        self.set_status("not deleting the project root");
                    } else {
                        self.tree_input = Some(TreeInput::Delete {
                            path: row.path.clone(),
                        });
                    }
                }
            }
            KeyCode::Char('x') | KeyCode::Char('c') => {
                if let Some(row) = tree.selected_row() {
                    if row.path == tree.root {
                        self.set_status("the root can't be cut or copied");
                        return;
                    }
                    let cut = key.code == KeyCode::Char('x');
                    let name = row.name.clone();
                    self.tree_clipboard = Some((row.path.clone(), cut));
                    self.set_status(format!(
                        "{} {name} — p pastes",
                        if cut { "cut" } else { "copied" }
                    ));
                }
            }
            KeyCode::Char('p') => self.tree_paste(),
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                let Some(row) = tree.selected_row() else {
                    return;
                };
                if row.is_dir {
                    tree.toggle_selected();
                } else {
                    let path = row.path.clone();
                    self.tree_focused = false;
                    self.jump_to(path, 0, 0);
                }
            }
            _ => {}
        }
    }

    /// Keys while a tree create/delete prompt is open. `take` + re-store
    /// keeps the borrow checker out of the way.
    pub(crate) fn handle_tree_input_key(&mut self, key: Key) {
        let Some(input) = self.tree_input.take() else {
            return;
        };
        match input {
            TreeInput::Create { dir, mut name } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.tree_create(&dir, name.trim()),
                KeyCode::Backspace => {
                    name.pop();
                    self.tree_input = Some(TreeInput::Create { dir, name });
                }
                KeyCode::Char(c) if !key.ctrl && !key.alt => {
                    name.push(c);
                    self.tree_input = Some(TreeInput::Create { dir, name });
                }
                _ => self.tree_input = Some(TreeInput::Create { dir, name }),
            },
            TreeInput::Delete { path } => {
                if key.code == KeyCode::Char('y') {
                    self.tree_delete(&path);
                }
            }
            TreeInput::Rename { path, mut name } => match key.code {
                KeyCode::Esc => {}
                KeyCode::Enter => self.tree_rename(&path, name.trim()),
                KeyCode::Backspace => {
                    name.pop();
                    self.tree_input = Some(TreeInput::Rename { path, name });
                }
                KeyCode::Char(c) if !key.ctrl && !key.alt => {
                    name.push(c);
                    self.tree_input = Some(TreeInput::Rename { path, name });
                }
                _ => self.tree_input = Some(TreeInput::Rename { path, name }),
            },
        }
    }

    pub(crate) fn tree_create(&mut self, dir: &Path, name: &str) {
        if name.is_empty() {
            return;
        }
        let target = dir.join(name);
        let result = if name.ends_with('/') {
            std::fs::create_dir_all(&target)
        } else {
            // `a src/deep/new.rs` works: intermediate directories appear too.
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // create_new: never truncate something that already exists.
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)
                .map(|_| ())
        };
        match result {
            Ok(()) => {
                if let Some(tree) = self.tree.as_mut() {
                    tree.reveal(&target);
                }
                self.set_status(format!("created {name}"));
            }
            Err(e) => self.set_status(format!("Error: {e}")),
        }
    }

    pub(crate) fn tree_rename(&mut self, path: &Path, name: &str) {
        if name.is_empty() {
            return;
        }
        let target = path.parent().unwrap_or(Path::new("")).join(name);
        if target == path {
            return;
        }
        if target.exists() {
            self.set_status(format!("Error: {name:?} already exists"));
            return;
        }
        // A name with slashes is a move; the intermediate directories appear.
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(path, &target) {
            Ok(()) => {
                self.retarget_buffers(path, &target);
                if let Some(tree) = self.tree.as_mut() {
                    tree.reveal(&target);
                }
                self.set_status(format!("renamed to {name}"));
            }
            Err(e) => self.set_status(format!("Error: {e}")),
        }
    }

    pub(crate) fn tree_paste(&mut self) {
        let Some((src, cut)) = self.tree_clipboard.clone() else {
            self.set_status("nothing cut or copied");
            return;
        };
        let Some(tree) = self.tree.as_ref() else {
            return;
        };
        // Same landing rule as `a`: the selected directory, or beside the
        // selected file.
        let dir = match tree.selected_row() {
            Some(row) if row.is_dir => row.path.clone(),
            Some(row) => row.path.parent().unwrap_or(&tree.root).to_path_buf(),
            None => tree.root.clone(),
        };
        let Some(name) = src.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            return;
        };
        let target = dir.join(&name);
        if target == src {
            self.set_status("already there");
            return;
        }
        if target.exists() {
            self.set_status(format!("Error: {name:?} already exists here"));
            return;
        }
        if src.is_dir() && dir.starts_with(&src) {
            self.set_status("Error: can't paste a directory into itself");
            return;
        }

        let result = if cut {
            std::fs::rename(&src, &target)
        } else {
            crate::filetree::copy_recursively(&src, &target)
        };
        match result {
            Ok(()) => {
                if cut {
                    // The move is done: open buffers follow, and a second
                    // paste would be meaningless.
                    self.retarget_buffers(&src, &target);
                    self.tree_clipboard = None;
                }
                if let Some(tree) = self.tree.as_mut() {
                    tree.reveal(&target);
                }
                self.set_status(format!("{} {name}", if cut { "moved" } else { "copied" }));
            }
            Err(e) => self.set_status(format!("Error: {e}")),
        }
    }

    /// Point any open buffer at a file's new location after a move.
    pub(crate) fn retarget_buffers(&mut self, old: &Path, new: &Path) {
        for doc in &mut self.documents {
            if doc.path.as_deref() == Some(old) {
                doc.path = Some(new.to_path_buf());
                doc.refresh_syntax();
            }
        }
    }

    pub(crate) fn tree_delete(&mut self, path: &Path) {
        // ponytail: a real delete, not a trash can — the y/n prompt names
        // exactly what goes.
        let result = if path.is_dir() {
            std::fs::remove_dir_all(path)
        } else {
            std::fs::remove_file(path)
        };
        match result {
            Ok(()) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if let Some(tree) = self.tree.as_mut() {
                    tree.rebuild();
                }
                self.set_status(format!("deleted {name}"));
            }
            Err(e) => self.set_status(format!("Error: {e}")),
        }
    }
}
