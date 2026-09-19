//! Language-server glue: syncing buffers, draining events, and the insert-mode
//! typing path that feeds completion requests.

use super::*;

impl Editor {
    // ---- lsp ---------------------------------------------------------------

    /// The configured server command for the current buffer, if any.
    pub(crate) fn current_server_command(&self) -> Option<String> {
        let path = self.doc().path.as_deref()?;
        server_for(&self.lsp_table, path).map(str::to_string)
    }

    /// True when a language server is up for the current buffer (status line).
    pub fn lsp_active(&self) -> bool {
        self.current_server_command()
            .is_some_and(|cmd| self.lsps.iter().any(|c| c.command() == cmd))
    }

    /// The running client that serves the current buffer's language.
    pub fn current_client(&mut self) -> Option<&mut lsp::Client> {
        let command = self.current_server_command()?;
        self.lsps.iter_mut().find(|c| c.command() == command)
    }

    /// Shut every language server down (quit path).
    pub fn shutdown_lsps(&mut self) {
        for lsp in self.lsps.drain(..) {
            lsp.shutdown();
        }
    }

    /// Called from the main loop between keystrokes: keep the server in sync
    /// with edited buffers and apply anything it sent back.
    /// Drains language-server messages. Returns true when anything on screen
    /// may have changed, so the main loop knows a redraw is needed.
    pub fn lsp_tick(&mut self) -> bool {
        // Changes go over once typing pauses: most servers re-analyse on
        // every `didChange`, and one per keystroke only makes them busier.
        // A completion or signature request can't wait — the server has to
        // see the edit that prompted it first.
        if self.lsp_completion_pending.is_some()
            || self.lsp_signature_pending
            || self.last_key_at.elapsed() >= std::time::Duration::from_millis(120)
        {
            self.lsp_sync();
        }
        if std::mem::take(&mut self.lsp_signature_pending) {
            self.request_signature();
        }
        if let Some(tag) = self.lsp_completion_pending.take() {
            if self.mode == Mode::Insert {
                if let Some(path) = self.doc().path.clone() {
                    let (line, col) = self.doc().cursor_line_col();
                    let utf16_col = crate::position::char_to_utf16(self.doc().line(line), col);
                    if let Some(lsp) = self.current_client() {
                        lsp.request_position(
                            tag,
                            "textDocument/completion",
                            &path,
                            line,
                            utf16_col,
                        );
                    }
                }
            }
        }
        let mut events = Vec::new();
        let mut i = 0;
        while i < self.lsps.len() {
            events.extend(self.lsps[i].poll());
            if self.lsps[i].is_dead() {
                // The server exited (crashed, or was a broken shim): stop
                // syncing it and don't respawn every tick.
                let dead = self.lsps.remove(i);
                self.lsp_failed.insert(dead.command().to_string());
                // Dropping a Child neither kills nor reaps it; shutdown does.
                dead.shutdown();
            } else {
                i += 1;
            }
        }
        let changed = !events.is_empty();
        for event in events {
            match event {
                lsp::Event::Definition(path, line, col) => self.jump_to(path, line, col),
                lsp::Event::Hover(text) => self.open_hover(&text),
                lsp::Event::Status(text) => self.set_status(text),
                lsp::Event::Diagnostics(path, diags) => {
                    self.diagnostics.insert(path, diags);
                }
                lsp::Event::Completions(items, typed) => self.show_completions(items, typed),
                lsp::Event::CompletionResolved(label, info) => {
                    if let Some(c) = self.completion.as_mut() {
                        if !info.is_empty() {
                            c.docs.insert(label, info);
                        }
                    }
                }
                lsp::Event::References(locs) => self.show_references(locs),
                lsp::Event::ApplyEdit(edit) => {
                    let n = self.apply_workspace_edit(&edit);
                    self.set_status(format!(
                        "{n} file{} changed — review, then :wa writes them",
                        if n == 1 { "" } else { "s" }
                    ));
                }
                lsp::Event::CodeActions(actions) => self.show_code_actions(actions),
                lsp::Event::CodeActionResolved(action) => self.finish_code_action(action),
                lsp::Event::Signature(sig) => {
                    if self.mode == Mode::Insert {
                        self.signature = sig;
                    }
                }
                lsp::Event::Symbols(syms, workspace) => self.show_symbols(syms, workspace),
                lsp::Event::Formatting(edits) => self.finish_lsp_format(edits),
            }
        }
        changed
    }

    /// Open the hover docs popup — signature, description, examples — or
    /// fall back to the status line when the buffer isn't in normal mode.
    pub fn open_hover(&mut self, text: &str) {
        if self.mode != Mode::Normal {
            self.set_status(text.lines().next().unwrap_or("").to_string());
            return;
        }
        self.hover = Some((text.lines().map(str::to_string).collect(), 0));
    }

    /// `typed` marks the list crow asked for on its own while an identifier
    /// was being typed, rather than one the user asked for.
    pub(crate) fn show_completions(&mut self, items: Vec<(String, String, String)>, typed: bool) {
        if self.mode != Mode::Insert {
            return; // the answer arrived after insert mode ended
        }
        let prefix = self.word_prefix();
        let lower = prefix.to_lowercase();
        let mut docs = std::collections::HashMap::new();
        let mut items: Vec<(String, String)> = items
            .into_iter()
            .filter(|(label, _, _)| label.to_lowercase().starts_with(&lower))
            .map(|(label, text, info)| {
                if !info.is_empty() {
                    docs.insert(label.clone(), info);
                }
                (label, text)
            })
            .collect();
        items.truncate(50);
        if items.is_empty() {
            // An unasked-for list that no longer matches what has been typed
            // since must not close the popup that is up, nor say anything.
            if !typed {
                self.set_status("no completions");
                self.completion = None;
            }
            return;
        }
        self.completion = Some(Completion {
            items,
            selected: 0,
            prefix,
            // Asked for (C-space, or a `.`/`<`/`::` trigger): the list is the
            // point, so Enter accepts. Offered while typing: Enter stays
            // Enter and Tab accepts, like the buffer-word popup it replaced.
            navigated: !typed,
            docs,
        });
        // Docs for the item highlighted on open, if the server defers them.
        self.maybe_resolve_completion();
    }

    /// One typed character in insert mode: bracket/quote pairs close
    /// themselves, retyping a closer steps over it, and identifier chars
    /// feed the intellisense popup.
    pub(crate) fn insert_typed(&mut self, c: char) {
        // Signature help: `(` and `,` (or whatever the server says) ask for
        // it; the call's closing `)` puts it away. Decided up front, since
        // autoclose below returns early for exactly these characters. The
        // request itself waits for the tick that syncs this edit.
        if c == ')' {
            self.signature = None;
        } else if !self.replaying
            && self
                .current_client()
                .is_some_and(|lsp| lsp.triggers_signature(c))
        {
            self.lsp_signature_pending = true;
        }

        // A closer typed on a whitespace-only line dedents it one level first.
        if matches!(c, ')' | ']' | '}') && self.doc().extra.is_empty() {
            let doc = self.doc();
            let (line, col) = doc.cursor_line_col();
            let slice = doc.line(line);
            if col > 0 && (0..col).all(|i| matches!(slice.char(i), ' ' | '\t')) {
                let take = if slice.char(col - 1) == '\t' {
                    1
                } else {
                    crate::config::tab_width().min(col)
                };
                let to = doc.cursor;
                self.doc_mut().delete_range(to - take, to);
            }
        }

        if crate::config::autoclose() {
            let doc = self.doc();
            let next = (doc.cursor < doc.text.len_chars()).then(|| doc.text.char(doc.cursor));
            let prev = (doc.cursor > 0).then(|| doc.text.char(doc.cursor - 1));

            // Retyping the closer that's already there steps over it — but
            // only when it is there for every cursor, or the ones without it
            // would skip whatever they are sitting on instead.
            let all_on_closer = next == Some(c)
                && doc
                    .extra
                    .iter()
                    .all(|&(_, cur)| doc.text.get_char(cur) == Some(c));
            if matches!(c, ')' | ']' | '}' | '"' | '\'') && all_on_closer {
                let doc = self.doc_mut();
                let len = doc.text.len_chars();
                doc.cursor = (doc.cursor + 1).min(len);
                doc.anchor = doc.cursor;
                for (a, cur) in &mut doc.extra {
                    *cur = (*cur + 1).min(len);
                    *a = *cur;
                }
                return;
            }

            // Openers bring their closer. The apostrophe is the one that
            // has to read the room: right after a word it is a contraction
            // (don't, can't…), not an opener. A double quote after a word
            // char is not — `x="`, `f(a,"` — so it always pairs.
            let close = match c {
                '(' => Some(')'),
                '[' => Some(']'),
                '{' => Some('}'),
                '"' => Some('"'),
                '\'' if !prev.is_some_and(|p| p.is_alphanumeric() || p == '_') => Some(c),
                _ => None,
            };
            if let Some(close) = close {
                let pair: String = [c, close].iter().collect();
                self.doc_mut().insert_at_cursor(&pair);
                // Every cursor steps back between its pair.
                let doc = self.doc_mut();
                doc.cursor = doc.cursor.saturating_sub(1);
                doc.anchor = doc.cursor;
                for (a, cur) in &mut doc.extra {
                    *cur = cur.saturating_sub(1);
                    *a = *cur;
                }
                return;
            }
        }

        let prev = {
            let doc = self.doc();
            (doc.cursor > 0).then(|| doc.text.char(doc.cursor - 1))
        };
        self.doc_mut().insert_at_cursor(&c.to_string());
        if c.is_alphanumeric() || c == '_' || c == '/' {
            self.maybe_autocomplete();
            // With a server up, ask it as well: buffer words can only offer
            // what the file already says, so `println` is invisible until
            // something types it first. Its answer lands in the same popup a
            // tick later, filtered by whatever the prefix is by then.
            //
            // ponytail: one request per identifier keystroke, held to one in
            // flight by the single slot; a real debounce timer if a server
            // ever starts falling behind.
            if self.lsp_completion_pending.is_none()
                && !self.replaying
                && self.word_prefix().chars().count() >= 2
                && self.current_server_command().is_some()
            {
                self.lsp_completion_pending = Some("completion_typed");
            }
        }
        // Member access: `.` or a second `:` asks the server what's inside.
        // Deferred to `lsp_tick` so the request follows this edit's didChange.
        // A digit before the dot is a float literal, not member access.
        let member_dot = c == '.' && !prev.is_some_and(|p| p.is_ascii_digit());
        // Plus whatever else the server itself calls a trigger — `<` opens
        // Oxigen's type list, and nothing but the server knows that.
        let declared = c != '.'
            && self
                .current_client()
                .is_some_and(|lsp| lsp.triggers_completion(c));
        if (member_dot || declared || (c == ':' && prev == Some(':')))
            && self.current_server_command().is_some()
            && !self.replaying
        {
            self.completion = None;
            self.lsp_completion_pending = Some("completion");
        }
    }

    /// Intellisense while typing: once two identifier chars are down, offer
    /// matching words from every open buffer. Instant and offline; `C-space`
    /// still asks the language server for the smart list.
    pub(crate) fn maybe_autocomplete(&mut self) {
        // A replay types what was typed; menus would only get in its way.
        if self.completion.is_some() || self.replaying {
            return;
        }
        if let Some(completion) = self.path_completion() {
            self.completion = Some(completion);
            return;
        }
        let prefix = self.word_prefix();
        if prefix.chars().count() < 2 {
            return;
        }
        let items = self.buffer_words(&prefix);
        if !items.is_empty() {
            self.completion = Some(Completion {
                items,
                selected: 0,
                prefix,
                navigated: false,
                docs: std::collections::HashMap::new(),
            });
        }
    }

    /// Words from all open buffers matching `prefix`, excluding the prefix
    /// itself. ponytail: a full scan per popup; a word index maintained on
    /// edit when huge buffers itch.
    pub(crate) fn buffer_words(&self, prefix: &str) -> Vec<(String, String)> {
        let lower = prefix.to_lowercase();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for doc in &self.documents {
            let mut word = String::new();
            for c in doc.text.chars().chain(std::iter::once(' ')) {
                if c.is_alphanumeric() || c == '_' {
                    word.push(c);
                    continue;
                }
                if word.chars().count() >= 3
                    && word.to_lowercase().starts_with(&lower)
                    && word != prefix
                    && seen.insert(word.clone())
                {
                    out.push((word.clone(), word.clone()));
                }
                word.clear();
            }
            if out.len() >= 50 {
                break;
            }
        }
        out.sort();
        out.truncate(50);
        out
    }

    /// Filesystem completion for a `./`, `../` or absolute path being typed.
    /// Returns None when the text before the cursor doesn't look like one.
    pub(crate) fn path_completion(&self) -> Option<Completion> {
        let doc = self.doc();
        let (line, col) = doc.cursor_line_col();
        let slice = doc.line(line);
        let mut start = col;
        while start > 0 {
            let c = slice.char(start - 1);
            if c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~') {
                start -= 1;
            } else {
                break;
            }
        }
        let token: String = (start..col).map(|i| slice.char(i)).collect();
        // A bare "/" (division, say) doesn't count; "./", "../", "~/" and "/usr" do.
        let looks_like_path = token.starts_with("./")
            || token.starts_with("../")
            || token.starts_with("~/")
            || (token.starts_with('/') && token.len() > 1);
        if !looks_like_path {
            return None;
        }
        let (dir, prefix) = token.rsplit_once('/')?;
        let dir = match dir.strip_prefix('~') {
            Some(rest) => format!("{}{rest}", std::env::var("HOME").ok()?),
            None => dir.to_string(),
        };
        let lower = prefix.to_lowercase();
        let mut items: Vec<(String, String)> = std::fs::read_dir(format!("{dir}/"))
            .ok()?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let mut name = entry.file_name().into_string().ok()?;
                // Hidden entries only once the prefix opts in with a dot.
                if name.starts_with('.') && !prefix.starts_with('.') {
                    return None;
                }
                if !name.to_lowercase().starts_with(&lower) || name == prefix {
                    return None;
                }
                if entry.file_type().ok()?.is_dir() {
                    name.push('/');
                }
                Some((name.clone(), name))
            })
            .collect();
        items.sort();
        items.truncate(50);
        (!items.is_empty()).then(|| Completion {
            items,
            selected: 0,
            prefix: prefix.to_string(),
            navigated: false,
            docs: std::collections::HashMap::new(),
        })
    }

    pub(crate) fn lsp_sync(&mut self) {
        // One client per distinct server command among the open files.
        let needed: Vec<String> = self
            .documents
            .iter()
            .filter_map(|d| d.path.as_deref())
            .filter_map(|p| server_for(&self.lsp_table, p))
            .map(str::to_string)
            .collect();
        for command in needed {
            if self.lsps.iter().any(|c| c.command() == command)
                || self.lsp_failed.contains(&command)
            {
                continue;
            }
            let root = std::env::current_dir().unwrap_or_default();
            match lsp::Client::spawn(&root, &command) {
                Some(client) => self.lsps.push(client),
                None => {
                    let program = command.split_whitespace().next().unwrap_or("").to_string();
                    self.lsp_failed.insert(command.clone());
                    if !self.offer_install(&program) {
                        self.set_status(format!("could not start {command:?} — no LSP"));
                    }
                }
            }
        }
        // Sync every document to its own language's server — never another's
        // (taplo getting a .rs file marks it "excluded", and worse).
        for doc in &mut self.documents {
            let Some(path) = doc.path.clone() else {
                continue;
            };
            let Some(command) = server_for(&self.lsp_table, &path) else {
                continue;
            };
            let Some(lsp) = self.lsps.iter_mut().find(|c| c.command() == command) else {
                continue;
            };
            match lsp.synced.get(path.as_path()).copied() {
                None => {
                    lsp.did_open(&path, doc.text.to_string(), doc.revision);
                    doc.reset_lsp_log();
                }
                Some((_, revision)) if revision != doc.revision => {
                    // Just the edits when the server takes them and the log
                    // covers exactly what it hasn't seen; the whole text if not.
                    match doc.take_lsp_changes(revision).filter(|_| lsp.incremental()) {
                        Some(changes) => lsp.did_change_incremental(&path, changes, doc.revision),
                        None => lsp.did_change(&path, doc.text.to_string(), doc.revision),
                    }
                }
                Some(_) => {}
            }
        }
    }

    /// Jump to a position given as (path, line, UTF-16 column) — reusing an
    /// open buffer for the file when there is one.
    pub fn jump_to(&mut self, path: PathBuf, line: usize, utf16_col: usize) {
        let Some(idx) = self.buffer_for(&path) else {
            return;
        };
        let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
        self.push_jump();
        crate::config::record_recent(&canon);
        self.leave_terminal_for_edit();
        self.current = idx;
        let doc = self.doc_mut();
        let line = line.min(doc.line_count().saturating_sub(1));
        let col = crate::position::utf16_to_char(doc.line(line), utf16_col);
        doc.cursor = doc.line_start(line) + col;
        doc.anchor = doc.cursor;
        doc.clamp_cursor(false);
        doc.goal_col = None;
    }

    /// Whether any buffer is owed a reparse.
    pub fn needs_reparse(&self) -> bool {
        self.documents.iter().any(Document::syntax_stale)
    }

    /// Whether nothing has been typed for `gap`.
    pub fn idle_for(&self, gap: std::time::Duration) -> bool {
        self.last_key_at.elapsed() >= gap
    }

    /// Recolor the stale buffers. A reparse is a whole-file tree-sitter pass —
    /// tens of milliseconds on a big file — so the main loop holds this until
    /// typing pauses, and draws the edits with the old spans slid through them
    /// in the meantime.
    pub fn settle(&mut self) {
        for doc in &mut self.documents {
            doc.settle_syntax();
        }
    }
}
