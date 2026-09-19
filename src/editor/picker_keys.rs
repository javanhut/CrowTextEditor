//! Keys and accept actions for the popup picker.

use super::*;

impl Editor {
    // ---- picker ------------------------------------------------------------

    pub fn open_picker(&mut self, picker: crate::picker::Picker) {
        self.picker = Some(picker);
        self.set_mode(Mode::Picker);
    }

    pub(crate) fn close_picker(&mut self) {
        self.picker = None;
        self.set_mode(Mode::Normal);
    }

    pub(crate) fn handle_picker_key(&mut self, key: Key) {
        use crate::picker::Kind;
        let Some(picker) = self.picker.as_mut() else {
            self.mode = Mode::Normal;
            return;
        };
        match key.code {
            KeyCode::Esc => self.picker_cancel(),
            // The focus keys work from here too: cancel, then move.
            KeyCode::Char('h') | KeyCode::Left if key.ctrl => {
                self.picker_cancel();
                (crate::commands::find("focus_left").unwrap().func)(self);
            }
            KeyCode::Char('l') | KeyCode::Right if key.ctrl => {
                self.picker_cancel();
                (crate::commands::find("focus_right").unwrap().func)(self);
            }
            KeyCode::Enter => self.picker_accept(),
            KeyCode::Down => self.picker_move(1),
            KeyCode::Up => self.picker_move(-1),
            KeyCode::Char('n') if key.ctrl => self.picker_move(1),
            KeyCode::Char('p') if key.ctrl => self.picker_move(-1),
            KeyCode::Backspace => {
                if picker.query.pop().is_some() {
                    picker.requery();
                    self.picker_query_changed();
                } else if let Kind::Explorer { dir } = &picker.kind {
                    // Empty query: backspace climbs to the parent directory.
                    let parent = dir.parent().map(Path::to_path_buf);
                    if let Some(parent) = parent {
                        *picker = crate::picker::Picker::explorer(parent);
                    }
                }
            }
            KeyCode::Char(c) if !key.ctrl && !key.alt => {
                picker.query.push(c);
                picker.requery();
                self.picker_query_changed();
            }
            _ => {}
        }
    }

    /// After the query changed: preview a theme, or ask the language server
    /// for the workspace symbols matching it.
    fn picker_query_changed(&mut self) {
        self.picker_preview();
        let query = match &self.picker {
            Some(p) if matches!(p.kind, crate::picker::Kind::WorkspaceSymbols { .. }) => {
                p.query.clone()
            }
            _ => return,
        };
        if let Some(lsp) = self.current_client() {
            lsp.request(
                "workspace/symbol",
                serde_json::json!({ "query": query }),
                "workspace_symbols",
            );
        }
    }

    /// Take in results a background search has streamed in. True when the
    /// list changed.
    pub fn picker_tick(&mut self) -> bool {
        self.picker.as_mut().is_some_and(|p| p.poll())
    }

    /// Close the picker without accepting, undoing any live theme preview.
    pub(crate) fn picker_cancel(&mut self) {
        if let Some(picker) = &self.picker {
            if let crate::picker::Kind::Theme { original } = &picker.kind {
                crate::theme::set(original);
            }
        }
        self.close_picker();
    }

    pub(crate) fn picker_move(&mut self, delta: isize) {
        if let Some(picker) = self.picker.as_mut() {
            picker.move_selection(delta);
        }
        self.picker_preview();
    }

    /// Theme picking previews live: the highlighted theme is applied at once.
    pub(crate) fn picker_preview(&mut self) {
        let Some(picker) = &self.picker else {
            return;
        };
        if matches!(picker.kind, crate::picker::Kind::Theme { .. }) {
            if let Some(item) = picker.selected_item() {
                let name = item.label.clone();
                crate::theme::set(&name);
            }
        }
    }

    pub(crate) fn picker_accept(&mut self) {
        use crate::picker::Kind;
        let Some(picker) = self.picker.take() else {
            return;
        };
        let Some(item) = picker.selected_item() else {
            self.close_picker();
            return;
        };
        let label = item.label.clone();
        let index = picker.selected_index().unwrap_or(0);
        self.close_picker();
        match picker.kind {
            Kind::Locations { locs } | Kind::WorkspaceSymbols { locs } => {
                if let Some(l) = locs.get(index) {
                    self.jump_to(l.path.clone(), l.line, l.col);
                }
            }
            Kind::CodeActions { mut actions } => {
                if index < actions.len() {
                    let action = actions.swap_remove(index);
                    self.run_code_action(action);
                }
            }
            Kind::Command => {
                if let Some(command) = commands::find(&label) {
                    self.register_fresh = true;
                    (command.func)(self);
                }
            }
            Kind::Theme { .. } => {
                crate::theme::set(&label);
                self.set_status(format!("theme: {label}"));
            }
            Kind::Files { root } => self.jump_to(root.join(label), 0, 0),
            Kind::Grep { root, .. } => {
                if let Some((path, line)) = label.rsplit_once(':') {
                    let line = line.parse::<usize>().unwrap_or(1).saturating_sub(1);
                    self.jump_to(root.join(path), line, 0);
                }
            }
            Kind::Recent => {
                let path = match label.strip_prefix("~/") {
                    Some(rest) => {
                        PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest)
                    }
                    None => PathBuf::from(label),
                };
                self.jump_to(path, 0, 0);
            }
            Kind::Explorer { dir } => {
                if label == "../" {
                    if let Some(parent) = dir.parent() {
                        self.open_picker(crate::picker::Picker::explorer(parent.to_path_buf()));
                    }
                } else if let Some(subdir) = label.strip_suffix('/') {
                    self.open_picker(crate::picker::Picker::explorer(dir.join(subdir)));
                } else {
                    self.jump_to(dir.join(label), 0, 0);
                }
            }
        }
    }

    /// The floating `:`/`/` prompt: (x, y, width), centered near the top.
    pub fn prompt_rect(&self) -> (u16, u16, u16) {
        let w = ((self.size.0 as usize) * 3 / 5).clamp(20, 70) as u16;
        let x = (self.size.0.saturating_sub(w)) / 2;
        (x, 1, w)
    }

    /// Overlay rectangle for the picker, centered near the top.
    pub fn picker_rect(&self) -> Rect {
        let w = ((self.size.0 as usize) * 3 / 4).clamp(20, 80) as u16;
        let h = 12.min(self.size.1.saturating_sub(4)).max(2);
        let x = (self.size.0.saturating_sub(w)) / 2;
        (x, 1, w, h)
    }
}
