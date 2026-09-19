//! The language-server features past completion and hover: references,
//! rename, code actions, signature help, symbols, formatting, diagnostics
//! navigation — and applying the workspace edits several of them answer with.

use super::*;
use crate::picker::{Item, Picker};
use crate::transaction::Transaction;
use serde_json::{json, Value};

impl Editor {
    /// The primary cursor as an LSP `TextDocumentPositionParams`.
    pub(crate) fn position_params(&self) -> Option<Value> {
        let path = self.doc().path.clone()?;
        let (line, col) = self.doc().cursor_line_col();
        let col = crate::position::char_to_utf16(self.doc().line(line), col);
        Some(json!({
            "textDocument": {"uri": lsp::uri_from_path(&path)},
            "position": {"line": line, "character": col}
        }))
    }

    /// Send a request to the current buffer's server, after making sure it has
    /// seen the buffer as it is now. False — with the reason on the status
    /// line — when there is no server to ask.
    pub(crate) fn lsp_request(&mut self, method: &str, params: Value, tag: &'static str) -> bool {
        self.lsp_sync();
        match self.current_client() {
            Some(lsp) => {
                lsp.request(method, params, tag);
                true
            }
            None => {
                self.set_status("language server not running");
                false
            }
        }
    }

    /// Tell the server the current buffer was written.
    pub fn lsp_did_save(&mut self) {
        let Some(path) = self.doc().path.clone() else {
            return;
        };
        // Only a server that is already up: a save is no reason to start one.
        if self.current_client().is_none() {
            return;
        }
        self.lsp_sync();
        if let Some(lsp) = self.current_client() {
            lsp.did_save(&path);
        }
    }

    // ---- references and symbols ---------------------------------------------

    pub fn find_references(&mut self) {
        let Some(mut params) = self.position_params() else {
            self.set_status("buffer has no file");
            return;
        };
        params["context"] = json!({"includeDeclaration": true});
        if self.lsp_request("textDocument/references", params, "references") {
            self.set_status("finding references…");
        }
    }

    pub(crate) fn show_references(&mut self, locs: Vec<lsp::Location>) {
        match locs.len() {
            0 => self.set_status("no references found"),
            1 => {
                let l = &locs[0];
                self.jump_to(l.path.clone(), l.line, l.col);
            }
            n => {
                let items = self.location_items(&locs);
                self.open_picker(Picker::new(
                    format!("references ({n})"),
                    crate::picker::Kind::Locations { locs },
                    items,
                ));
            }
        }
    }

    /// Picker rows for a list of locations: `path:line`, with the line's text.
    fn location_items(&self, locs: &[lsp::Location]) -> Vec<Item> {
        let root = std::env::current_dir().unwrap_or_default();
        let mut files: HashMap<PathBuf, Vec<String>> = HashMap::new();
        locs.iter()
            .map(|l| {
                let lines = files.entry(l.path.clone()).or_insert_with(|| {
                    match self.open_buffer(&l.path) {
                        Some(i) => self.documents[i]
                            .text
                            .lines()
                            .map(|s| s.to_string())
                            .collect(),
                        None => std::fs::read_to_string(&l.path)
                            .unwrap_or_default()
                            .lines()
                            .map(str::to_string)
                            .collect(),
                    }
                });
                let rel = l.path.strip_prefix(&root).unwrap_or(&l.path);
                Item {
                    label: format!("{}:{}", rel.display(), l.line + 1),
                    detail: lines
                        .get(l.line)
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    pub fn document_symbols(&mut self) {
        let Some(path) = self.doc().path.clone() else {
            self.set_status("buffer has no file");
            return;
        };
        let params = json!({"textDocument": {"uri": lsp::uri_from_path(&path)}});
        self.symbols_path = Some(path);
        self.lsp_request("textDocument/documentSymbol", params, "symbols");
    }

    /// `space S`: a picker whose list the server fills as you type.
    pub fn workspace_symbols(&mut self) {
        if self.current_client().is_none() {
            self.set_status("language server not running");
            return;
        }
        self.open_picker(Picker::new(
            "workspace symbols",
            crate::picker::Kind::WorkspaceSymbols { locs: Vec::new() },
            Vec::new(),
        ));
        self.lsp_request(
            "workspace/symbol",
            json!({"query": ""}),
            "workspace_symbols",
        );
    }

    pub(crate) fn show_symbols(&mut self, mut syms: Vec<lsp::Symbol>, workspace: bool) {
        if !workspace {
            // Document symbols come without a path: they are in the file asked about.
            let Some(path) = self.symbols_path.take() else {
                return;
            };
            for s in &mut syms {
                if s.location.path.as_os_str().is_empty() {
                    s.location.path = path.clone();
                }
            }
        }
        let items: Vec<Item> = syms
            .iter()
            .map(|s| Item {
                label: format!("{}{}", "  ".repeat(s.depth), s.name),
                detail: if s.container.is_empty() || !workspace {
                    format!("{}  :{}", s.kind, s.location.line + 1)
                } else {
                    format!("{} in {}", s.kind, s.container)
                },
            })
            .collect();
        let locs: Vec<lsp::Location> = syms.into_iter().map(|s| s.location).collect();
        if workspace {
            // Only land in a picker still waiting for these.
            if let Some(picker) = self.picker.as_mut() {
                if let crate::picker::Kind::WorkspaceSymbols { locs: slot } = &mut picker.kind {
                    *slot = locs;
                    picker.items = items;
                    picker.refilter();
                }
            }
            return;
        }
        if items.is_empty() {
            self.set_status("no symbols in this buffer");
            return;
        }
        self.open_picker(Picker::new(
            "symbols",
            crate::picker::Kind::Locations { locs },
            items,
        ));
    }

    // ---- rename -------------------------------------------------------------

    /// `space R`: the `:rename` prompt, prefilled with the name under the cursor.
    pub fn prompt_rename(&mut self) {
        let word = self.word_at_cursor();
        self.set_mode(Mode::Command);
        self.command_line = format!("rename {word}");
    }

    /// The identifier the cursor is on (or just after), empty if none.
    pub(crate) fn word_at_cursor(&self) -> String {
        let doc = self.doc();
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        let len = doc.text.len_chars();
        let mut start = doc.cursor.min(len);
        if start < len && !is_word(doc.text.char(start)) && start > 0 {
            start -= 1;
        }
        if start >= len || !is_word(doc.text.char(start)) {
            return String::new();
        }
        let mut end = start;
        while start > 0 && is_word(doc.text.char(start - 1)) {
            start -= 1;
        }
        while end < len && is_word(doc.text.char(end)) {
            end += 1;
        }
        doc.text.slice(start..end).to_string()
    }

    pub(crate) fn rename_symbol(&mut self, new_name: &str) {
        let Some(mut params) = self.position_params() else {
            self.set_status("buffer has no file");
            return;
        };
        params["newName"] = json!(new_name);
        if self.lsp_request("textDocument/rename", params, "rename") {
            self.set_status(format!("renaming to {new_name}…"));
        }
    }

    // ---- code actions -------------------------------------------------------

    /// `space a`: what the server offers for the selection (or cursor line),
    /// told about the diagnostics there so quick fixes show up.
    pub fn code_actions(&mut self) {
        let Some(path) = self.doc().path.clone() else {
            self.set_status("buffer has no file");
            return;
        };
        let doc = self.doc();
        let (from, to) = (doc.anchor.min(doc.cursor), doc.anchor.max(doc.cursor));
        let at = |pos: usize| {
            let line = doc.text.char_to_line(pos.min(doc.text.len_chars()));
            let col = crate::position::char_to_utf16(doc.line(line), pos - doc.line_start(line));
            (line, col)
        };
        let (start, end) = (at(from), at(to));
        let lines = start.0..=end.0;
        let diags: Vec<Value> = path
            .canonicalize()
            .ok()
            .and_then(|p| self.diagnostics.get(&p))
            .map(|ds| {
                ds.iter()
                    .filter(|d| lines.contains(&d.line))
                    .map(|d| d.raw.clone())
                    .collect()
            })
            .unwrap_or_default();
        let params = json!({
            "textDocument": {"uri": lsp::uri_from_path(&path)},
            "range": {
                "start": {"line": start.0, "character": start.1},
                "end": {"line": end.0, "character": end.1}
            },
            "context": {"diagnostics": diags}
        });
        self.lsp_request("textDocument/codeAction", params, "code_action");
    }

    pub(crate) fn show_code_actions(&mut self, actions: Vec<Value>) {
        if actions.is_empty() {
            self.set_status("no code actions here");
            return;
        }
        let items = actions
            .iter()
            .map(|a| Item {
                label: a["title"].as_str().unwrap_or("(untitled)").to_string(),
                detail: a["kind"].as_str().unwrap_or("").to_string(),
            })
            .collect();
        self.open_picker(Picker::new(
            "code actions",
            crate::picker::Kind::CodeActions { actions },
            items,
        ));
    }

    /// Run a picked code action: apply its edit and run its command, or ask
    /// the server to fill those in first when it left them out.
    pub(crate) fn run_code_action(&mut self, action: Value) {
        // A bare Command rather than a CodeAction.
        if action["command"].is_string() {
            self.execute_lsp_command(&action);
            return;
        }
        let has_edit = !action["edit"].is_null();
        let has_command = action["command"].is_object();
        if !has_edit && !has_command {
            self.lsp_request("codeAction/resolve", action, "code_action_resolve");
            return;
        }
        self.finish_code_action(action);
    }

    /// Apply a code action that is complete — never resolves again, so a
    /// server that resolves to nothing can't loop us.
    pub(crate) fn finish_code_action(&mut self, action: Value) {
        let mut did = false;
        if !action["edit"].is_null() {
            let n = self.apply_workspace_edit(&action["edit"]);
            self.set_status(format!(
                "{} — {n} file{} changed",
                title_of(&action),
                plural(n)
            ));
            did = true;
        }
        if action["command"].is_object() {
            self.execute_lsp_command(&action["command"]);
            did = true;
        }
        if !did {
            self.set_status(format!("{}: nothing to apply", title_of(&action)));
        }
    }

    fn execute_lsp_command(&mut self, cmd: &Value) {
        let params = json!({
            "command": cmd["command"],
            "arguments": cmd.get("arguments").cloned().unwrap_or(json!([]))
        });
        self.lsp_request("workspace/executeCommand", params, "execute");
    }

    // ---- workspace edits ----------------------------------------------------

    /// Apply a WorkspaceEdit across files — opening the ones not open yet, as
    /// modified buffers for you to review and `:wa` — and return how many
    /// files it touched. Each file's edit is one undo step.
    pub fn apply_workspace_edit(&mut self, edit: &Value) -> usize {
        let mut per_file: Vec<(PathBuf, Vec<Value>)> = Vec::new();
        if let Some(changes) = edit["changes"].as_object() {
            for (uri, edits) in changes {
                if let Some(path) = lsp::path_from_uri(uri) {
                    per_file.push((path, edits.as_array().cloned().unwrap_or_default()));
                }
            }
        }
        let path_of = |v: &Value| v.as_str().and_then(lsp::path_from_uri);
        let mut touched = 0;
        for change in edit["documentChanges"].as_array().into_iter().flatten() {
            match change["kind"].as_str() {
                Some("create") => {
                    if let Some(path) = path_of(&change["uri"]) {
                        if let Some(dir) = path.parent() {
                            let _ = std::fs::create_dir_all(dir);
                        }
                        let _ = std::fs::OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&path);
                        touched += 1;
                    }
                }
                Some("rename") => {
                    if let (Some(old), Some(new)) =
                        (path_of(&change["oldUri"]), path_of(&change["newUri"]))
                    {
                        if std::fs::rename(&old, &new).is_ok() {
                            self.retarget_buffers(&old, &new);
                            touched += 1;
                        }
                    }
                }
                Some("delete") => {
                    if let Some(path) = path_of(&change["uri"]) {
                        let _ = if path.is_dir() {
                            std::fs::remove_dir_all(&path)
                        } else {
                            std::fs::remove_file(&path)
                        };
                        touched += 1;
                    }
                }
                _ => {
                    if let Some(path) = path_of(&change["textDocument"]["uri"]) {
                        per_file.push((
                            path,
                            change["edits"].as_array().cloned().unwrap_or_default(),
                        ));
                    }
                }
            }
        }
        // Two entries for one file would have the second stated against the
        // text the first already changed; merged, they apply as one set
        // against the version the server was talking about.
        let mut merged: Vec<(PathBuf, Vec<Value>)> = Vec::new();
        for (path, edits) in per_file {
            match merged.iter_mut().find(|(p, _)| *p == path) {
                Some((_, existing)) => existing.extend(edits),
                None => merged.push((path, edits)),
            }
        }
        for (path, edits) in merged {
            match self.buffer_for(&path) {
                Some(idx) => {
                    if self.apply_text_edits(idx, &edits) {
                        touched += 1;
                    }
                }
                None => self.set_status(format!("can't open {}", path.display())),
            }
        }
        touched
    }

    /// Apply LSP TextEdits to buffer `idx` as one undo step. Overlapping
    /// edits — which the protocol forbids — are dropped rather than trusted.
    pub(crate) fn apply_text_edits(&mut self, idx: usize, edits: &[Value]) -> bool {
        let doc = &mut self.documents[idx];
        let mut changes: Vec<(usize, usize, String)> = edits
            .iter()
            .filter_map(|e| {
                let from = lsp_offset(doc, &e["range"]["start"])?;
                let to = lsp_offset(doc, &e["range"]["end"])?;
                Some((
                    from.min(to),
                    from.max(to),
                    e["newText"].as_str()?.to_string(),
                ))
            })
            .collect();
        // Stable: two inserts at one spot keep the order the server gave.
        changes.sort_by_key(|c| (c.0, c.1));
        let mut last = 0;
        changes.retain(|c| {
            let fits = c.0 >= last;
            if fits {
                last = c.1;
            }
            fits
        });
        if changes.is_empty() {
            return false;
        }
        let tx = Transaction::change(
            &doc.text,
            changes.into_iter().map(|(f, t, s)| (f, t, Some(s))),
        );
        let (cursor, anchor) = (tx.map_pos(doc.cursor, false), tx.map_pos(doc.anchor, false));
        doc.commit_undo_group();
        doc.apply(tx, cursor);
        doc.anchor = anchor.min(doc.text.len_chars());
        doc.clamp_cursor(false);
        doc.commit_undo_group();
        true
    }

    // ---- formatting ---------------------------------------------------------

    /// Ask the server to format the buffer — the fallback when crow knows no
    /// formatter for it. False when the server can't.
    pub(crate) fn lsp_format(&mut self) -> bool {
        if !self.current_client().is_some_and(|c| c.can_format()) {
            return false;
        }
        let Some(path) = self.doc().path.clone() else {
            return false;
        };
        let tabs = self
            .doc()
            .text
            .lines()
            .any(|l| l.chars().next() == Some('\t'));
        let params = json!({
            "textDocument": {"uri": lsp::uri_from_path(&path)},
            "options": {"tabSize": crate::config::tab_width(), "insertSpaces": !tabs}
        });
        let owed = (self.current, self.doc().revision);
        // Only once it is really on its way: a stale slot would swallow the
        // answer to the next request.
        if self.lsp_request("textDocument/formatting", params, "formatting") {
            self.pending_format = Some(owed);
            return true;
        }
        false
    }

    pub(crate) fn finish_lsp_format(&mut self, edits: Vec<Value>) {
        let Some((idx, revision)) = self.pending_format.take() else {
            return;
        };
        // An answer about text that has since moved would land in the wrong place.
        if self.documents.get(idx).map(|d| d.revision) != Some(revision) {
            self.set_status("buffer changed while formatting — :fmt again");
            return;
        }
        if self.apply_text_edits(idx, &edits) {
            self.set_status("formatted (language server)");
        } else {
            self.set_status("already formatted");
        }
    }

    // ---- signature help -----------------------------------------------------

    pub(crate) fn request_signature(&mut self) {
        if self.mode != Mode::Insert {
            return;
        }
        let Some(params) = self.position_params() else {
            return;
        };
        if let Some(lsp) = self.current_client() {
            lsp.request("textDocument/signatureHelp", params, "signature");
        }
    }

    // ---- diagnostics --------------------------------------------------------

    /// The current buffer's diagnostics as (line, char offset in line, message).
    fn buffer_diagnostics(&self) -> Vec<(usize, usize, String)> {
        let doc = self.doc();
        let mut out: Vec<(usize, usize, String)> = doc
            .path
            .as_ref()
            .and_then(|p| p.canonicalize().ok())
            .and_then(|p| self.diagnostics.get(&p))
            .map(|ds| {
                ds.iter()
                    .filter(|d| d.line < doc.line_count())
                    .map(|d| {
                        let col = crate::position::utf16_to_char(doc.line(d.line), d.col);
                        (d.line, col, d.message.clone())
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by_key(|d| (d.0, d.1));
        out
    }

    /// `]d` / `[d`: the next (or previous) diagnostic, wrapping around.
    pub fn goto_diagnostic(&mut self, forward: bool) {
        let diags = self.buffer_diagnostics();
        if diags.is_empty() {
            self.set_status("no diagnostics in this buffer");
            return;
        }
        let (line, col) = self.doc().cursor_line_col();
        let here = (line, col);
        let target = if forward {
            diags.iter().find(|d| (d.0, d.1) > here).or(diags.first())
        } else {
            diags
                .iter()
                .rev()
                .find(|d| (d.0, d.1) < here)
                .or(diags.last())
        };
        let Some((line, col, message)) = target.cloned() else {
            return;
        };
        self.push_jump();
        let doc = self.doc_mut();
        doc.cursor = doc.line_start(line) + col;
        doc.anchor = doc.cursor;
        doc.clamp_cursor(false);
        doc.goal_col = None;
        self.set_status(format!("● {message}"));
    }

    /// `space x`: every diagnostic in every file, this buffer's first.
    pub fn diagnostics_picker(&mut self) {
        let here = self.doc().path.as_ref().and_then(|p| p.canonicalize().ok());
        let here = here.as_ref();
        let mut all: Vec<(bool, &PathBuf, &lsp::Diagnostic)> = self
            .diagnostics
            .iter()
            .flat_map(|(p, ds)| ds.iter().map(move |d| (Some(p) != here, p, d)))
            .collect();
        all.sort_by(|a, b| {
            (a.0, a.1, a.2.severity, a.2.line).cmp(&(b.0, b.1, b.2.severity, b.2.line))
        });
        if all.is_empty() {
            self.set_status("no diagnostics");
            return;
        }
        let root = std::env::current_dir().unwrap_or_default();
        let items: Vec<Item> = all
            .iter()
            .map(|(_, p, d)| Item {
                label: format!(
                    "{} {}:{}",
                    if d.severity == 1 { "●" } else { "▲" },
                    p.strip_prefix(&root).unwrap_or(p).display(),
                    d.line + 1
                ),
                detail: d.message.clone(),
            })
            .collect();
        let locs = all
            .iter()
            .map(|(_, p, d)| lsp::Location {
                path: (*p).clone(),
                line: d.line,
                col: d.col,
            })
            .collect();
        self.open_picker(Picker::new(
            "diagnostics",
            crate::picker::Kind::Locations { locs },
            items,
        ));
    }
}

/// An LSP `Position` as a char offset into `doc`, clamped into the text.
fn lsp_offset(doc: &crate::document::Document, pos: &Value) -> Option<usize> {
    let line = pos["line"].as_u64()? as usize;
    let col = pos["character"].as_u64()? as usize;
    if line >= doc.text.len_lines() {
        return Some(doc.text.len_chars());
    }
    Some(doc.line_start(line) + crate::position::utf16_to_char(doc.line(line), col))
}

fn title_of(action: &Value) -> String {
    action["title"]
        .as_str()
        .unwrap_or("code action")
        .to_string()
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[cfg(test)]
mod tests {
    use crate::editor::tests::editor_with;
    use serde_json::json;

    #[test]
    fn text_edits_apply_as_one_undo_step_and_keep_the_cursor() {
        let mut editor = editor_with("let foo = 1;\nfoo + foo\n");
        editor.doc_mut().cursor = 17; // the `+` on the second line
        let edits = vec![
            json!({"range": {"start": {"line": 1, "character": 6}, "end": {"line": 1, "character": 9}}, "newText": "bar"}),
            json!({"range": {"start": {"line": 0, "character": 4}, "end": {"line": 0, "character": 7}}, "newText": "bar"}),
            json!({"range": {"start": {"line": 1, "character": 0}, "end": {"line": 1, "character": 3}}, "newText": "bar"}),
        ];
        assert!(editor.apply_text_edits(0, &edits));
        assert_eq!(editor.doc().text.to_string(), "let bar = 1;\nbar + bar\n");
        assert_eq!(editor.doc().cursor, 17);
        editor.doc_mut().undo();
        assert_eq!(editor.doc().text.to_string(), "let foo = 1;\nfoo + foo\n");
    }

    #[test]
    fn overlapping_edits_are_dropped_not_applied() {
        let mut editor = editor_with("abcdef");
        let edits = vec![
            json!({"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}}, "newText": "X"}),
            json!({"range": {"start": {"line": 0, "character": 2}, "end": {"line": 0, "character": 5}}, "newText": "Y"}),
        ];
        assert!(editor.apply_text_edits(0, &edits));
        assert_eq!(editor.doc().text.to_string(), "Xef");
    }

    #[test]
    fn a_workspace_edit_reaches_a_file_that_is_not_open() {
        let dir = std::env::temp_dir().join(format!("crow-wsedit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("other.rs");
        std::fs::write(&file, "fn old() {}\n").unwrap();
        let mut editor = editor_with("");
        let uri = crate::lsp::uri_from_path(&file);
        let edit = json!({"documentChanges": [{"textDocument": {"uri": uri, "version": 1},
            "edits": [{"range": {"start": {"line": 0, "character": 3}, "end": {"line": 0, "character": 6}}, "newText": "new"}]}]});
        assert_eq!(editor.apply_workspace_edit(&edit), 1);
        let idx = editor.open_buffer(&file).expect("opened as a buffer");
        assert_eq!(editor.documents[idx].text.to_string(), "fn new() {}\n");
        assert!(
            editor.documents[idx].modified,
            "left for the user to review and write"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn word_at_cursor_finds_the_identifier() {
        let mut editor = editor_with("let some_name = 1;");
        editor.doc_mut().cursor = 7;
        assert_eq!(editor.word_at_cursor(), "some_name");
        editor.doc_mut().cursor = 3; // the space after `let`
        assert_eq!(editor.word_at_cursor(), "let");
    }
}
