//! The completion menu: navigation, accepting, and resolving docs.

use super::*;

impl Editor {
    // ---- completion --------------------------------------------------------

    /// Handle a key while the completion menu is open. Returns true if the
    /// key was consumed.
    pub(crate) fn handle_completion_key(&mut self, key: Key) -> bool {
        let consumed = self.completion_key_inner(key);
        if consumed {
            self.maybe_resolve_completion();
            // A key that typed a character replays as itself. One that moved
            // the highlight or accepted means nothing on replay without the
            // same menu: it comes out of the recording, and an accept is
            // recorded as the text it put in (see `completion_accept`).
            let typed = matches!(key.code, KeyCode::Char(c) if !key.ctrl && !key.alt
                && (c.is_alphanumeric() || c == '_' || c == '/'));
            if !typed {
                self.unrecord_last();
                if let Some(effect) = self.completion_effect.take() {
                    self.record_effect(effect);
                }
            }
        }
        consumed
    }

    pub(crate) fn completion_key_inner(&mut self, key: Key) -> bool {
        let Some(completion) = self.completion.as_mut() else {
            return false;
        };
        let next = |c: &mut Completion| {
            c.selected = (c.selected + 1) % c.items.len();
        };
        let prev = |c: &mut Completion| {
            c.selected = (c.selected + c.items.len() - 1) % c.items.len();
        };
        match key.code {
            KeyCode::Enter if !completion.navigated => {
                self.completion = None;
                false // the newline happens normally
            }
            KeyCode::Enter => {
                self.completion_accept();
                true
            }
            KeyCode::Esc => {
                self.completion = None;
                // Not consumed: one Esc both closes the menu and leaves
                // insert mode via the keymap, not two.
                false
            }
            // Tab steps into the list (first press selects the top item),
            // then Tab/S-Tab cycle; Enter accepts.
            KeyCode::Tab => {
                if completion.navigated {
                    next(completion);
                } else {
                    completion.navigated = true;
                }
                true
            }
            KeyCode::BackTab => {
                if completion.navigated {
                    prev(completion);
                } else {
                    completion.navigated = true;
                }
                true
            }
            KeyCode::Down => {
                completion.navigated = true;
                next(completion);
                true
            }
            KeyCode::Up => {
                completion.navigated = true;
                prev(completion);
                true
            }
            KeyCode::Char('n') if key.ctrl => {
                completion.navigated = true;
                next(completion);
                true
            }
            KeyCode::Char('p') if key.ctrl => {
                completion.navigated = true;
                prev(completion);
                true
            }
            KeyCode::Char('.' | ':') if !key.ctrl && !key.alt => {
                // Member access ends this menu; `insert_typed` sees the key
                // and asks the language server for the members.
                self.completion = None;
                false
            }
            KeyCode::Char('/') if !key.ctrl && !key.alt => {
                // A slash ends the current word; for paths it descends, so
                // start over and list the next directory.
                self.completion = None;
                self.doc_mut().insert_at_cursor("/");
                self.maybe_autocomplete();
                true
            }
            // Only identifier characters type through the menu. Anything
            // else — brackets, quotes, operators, space — ends the word, so
            // it falls to the catch-all below, which closes the menu and
            // lets `insert_typed` handle the key with autoclose intact.
            KeyCode::Char(c) if !key.ctrl && !key.alt && (c.is_alphanumeric() || c == '_') => {
                // Type through the menu: insert the char and narrow the list.
                completion.prefix.push(c);
                let prefix = completion.prefix.to_lowercase();
                completion
                    .items
                    .retain(|(label, _)| label.to_lowercase().starts_with(&prefix));
                completion.selected = 0;
                let empty = completion.items.is_empty();
                self.doc_mut().insert_at_cursor(&c.to_string());
                if empty {
                    self.completion = None;
                }
                true
            }
            _ => {
                // Anything else (backspace, arrows, escape sequences…) closes
                // the menu and is handled normally.
                self.completion = None;
                false
            }
        }
    }

    /// The highlighted completion has no docs yet: ask the server to
    /// resolve them, once. rust-analyzer and friends defer documentation to
    /// `completionItem/resolve` so the initial list stays fast.
    pub(crate) fn maybe_resolve_completion(&mut self) {
        let Some(c) = self.completion.as_ref() else {
            return;
        };
        if !c.navigated {
            return;
        }
        let Some((label, _)) = c.items.get(c.selected) else {
            return;
        };
        if c.docs.contains_key(label) {
            return;
        }
        let label = label.clone();
        let Some(lsp) = self.current_client() else {
            return;
        };
        lsp.resolve_completion(&label);
        // A placeholder, so cycling back over the item doesn't re-request;
        // the resolve response overwrites it.
        if let Some(c) = self.completion.as_mut() {
            c.docs.insert(label, String::new());
        }
    }

    pub(crate) fn completion_accept(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some((_, text)) = completion.items.get(completion.selected) else {
            return;
        };
        let prefix_chars = completion.prefix.chars().count();
        let entered_dir = text.ends_with('/');
        if text
            .to_lowercase()
            .starts_with(&completion.prefix.to_lowercase())
        {
            // The typed prefix stands; append the rest at every cursor.
            let suffix: String = text.chars().skip(prefix_chars).collect();
            self.doc_mut().insert_at_cursor(&suffix);
            self.completion_effect = Some(Input::Complete(0, suffix));
        } else {
            // ponytail: replacement completions rewrite the primary cursor
            // only; per-cursor replacement when multi-cursor completion itches.
            let doc = self.doc_mut();
            let from = doc.cursor.saturating_sub(prefix_chars);
            doc.delete_range(from, doc.cursor);
            let text = text.clone();
            self.doc_mut().insert_at_cursor(&text);
            self.completion_effect = Some(Input::Complete(prefix_chars, text));
        }
        // Accepting a directory rolls straight into listing its contents.
        if entered_dir {
            self.maybe_autocomplete();
        }
    }

    /// The identifier fragment just before the cursor.
    pub(crate) fn word_prefix(&self) -> String {
        let doc = self.doc();
        let (line, col) = doc.cursor_line_col();
        let slice = doc.line(line);
        let mut start = col;
        while start > 0 {
            let c = slice.char(start - 1);
            if c.is_alphanumeric() || c == '_' {
                start -= 1;
            } else {
                break;
            }
        }
        (start..col).map(|i| slice.char(i)).collect()
    }
}
