//! Keys and keymaps.
//!
//! Two things matter here. First, `Key` is the editor's own type, not
//! crossterm's — the keymap should not change shape if the terminal backend
//! does, and having our own type lets bindings be parsed from strings for
//! config later.
//!
//! Second, bindings live in a trie mapping key *sequences* to named commands,
//! not in a `match` statement. That is what makes `dd`, `gg`, counts like `3w`,
//! and eventual user rebinding possible. Hardcoded dispatch works until it
//! very abruptly doesn't.

use std::collections::HashMap;

use crossterm::event::{KeyCode as CtKeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::commands::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Tab,
    BackTab,
    Delete,
    Insert,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
}

impl Key {
    pub fn new(code: KeyCode) -> Self {
        Key {
            code,
            ctrl: false,
            alt: false,
        }
    }

    pub fn char(c: char) -> Self {
        Key::new(KeyCode::Char(c))
    }

    /// Translate a crossterm event, or `None` for events we ignore.
    ///
    /// Filtering on `KeyEventKind::Press` matters: on Windows, and on terminals
    /// with the kitty keyboard protocol enabled, release events also arrive and
    /// every keystroke would otherwise register twice.
    pub fn from_crossterm(ev: KeyEvent) -> Option<Self> {
        if ev.kind != KeyEventKind::Press {
            return None;
        }

        let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
        let alt = ev.modifiers.contains(KeyModifiers::ALT);

        let code = match ev.code {
            CtKeyCode::Char(c) => KeyCode::Char(c),
            CtKeyCode::Enter => KeyCode::Enter,
            CtKeyCode::Esc => KeyCode::Esc,
            CtKeyCode::Backspace => KeyCode::Backspace,
            CtKeyCode::Tab => KeyCode::Tab,
            CtKeyCode::BackTab => KeyCode::BackTab,
            CtKeyCode::Delete => KeyCode::Delete,
            CtKeyCode::Insert => KeyCode::Insert,
            CtKeyCode::Left => KeyCode::Left,
            CtKeyCode::Right => KeyCode::Right,
            CtKeyCode::Up => KeyCode::Up,
            CtKeyCode::Down => KeyCode::Down,
            CtKeyCode::Home => KeyCode::Home,
            CtKeyCode::End => KeyCode::End,
            CtKeyCode::PageUp => KeyCode::PageUp,
            CtKeyCode::PageDown => KeyCode::PageDown,
            CtKeyCode::F(n) => KeyCode::F(n),
            _ => return None,
        };

        Some(Key { code, ctrl, alt })
    }

    /// Parse a binding written the way a config file would write it:
    /// `j`, `C-d`, `A-x`, `<esc>`, `<enter>`, `C-<pageup>`.
    pub fn parse(s: &str) -> Option<Self> {
        let mut rest = s;
        let mut ctrl = false;
        let mut alt = false;

        loop {
            if let Some(tail) = rest.strip_prefix("Ctrl-").or_else(|| rest.strip_prefix("C-")) {
                ctrl = true;
                rest = tail;
            } else if let Some(tail) = rest.strip_prefix("Alt-").or_else(|| rest.strip_prefix("A-"))
            {
                alt = true;
                rest = tail;
            } else {
                break;
            }
        }

        let code = if rest.chars().count() == 1 {
            KeyCode::Char(rest.chars().next().unwrap())
        } else {
            let name = rest.trim_start_matches('<').trim_end_matches('>');
            match name.to_ascii_lowercase().as_str() {
                "esc" => KeyCode::Esc,
                "enter" | "cr" | "return" => KeyCode::Enter,
                "bs" | "backspace" => KeyCode::Backspace,
                "tab" => KeyCode::Tab,
                "del" | "delete" => KeyCode::Delete,
                "left" => KeyCode::Left,
                "right" => KeyCode::Right,
                "up" => KeyCode::Up,
                "down" => KeyCode::Down,
                "home" => KeyCode::Home,
                "end" => KeyCode::End,
                "pageup" => KeyCode::PageUp,
                "pagedown" => KeyCode::PageDown,
                "space" => KeyCode::Char(' '),
                other => {
                    let n: u8 = other.strip_prefix('f')?.parse().ok()?;
                    KeyCode::F(n)
                }
            }
        };

        Some(Key { code, ctrl, alt })
    }

    /// How this key should appear in the pending-keys indicator.
    pub fn display(&self) -> String {
        let mut s = String::new();
        if self.ctrl {
            s.push_str("Ctrl-");
        }
        if self.alt {
            s.push_str("Alt-");
        }
        match self.code {
            KeyCode::Char(' ') => s.push_str("<space>"),
            KeyCode::Char(c) => s.push(c),
            KeyCode::Esc => s.push_str("<esc>"),
            KeyCode::Enter => s.push_str("<enter>"),
            other => s.push_str(&format!("{:?}", other).to_lowercase()),
        }
        s
    }
}

pub enum KeyTrie {
    Leaf(&'static Command),
    Node(HashMap<Key, KeyTrie>),
}

/// Result of feeding a key sequence to a keymap.
///
/// Note this borrows nothing from the editor: commands are `'static`, so the
/// caller is free to mutate the editor while holding the result.
pub enum KeymapResult {
    /// A prefix of one or more bindings. Wait for more keys.
    Pending,
    Matched(&'static Command),
    NotFound,
}

impl KeyTrie {
    pub fn new() -> Self {
        KeyTrie::Node(HashMap::new())
    }

    /// Bind a key sequence to a command. Later bindings replace earlier ones.
    pub fn bind(&mut self, keys: &[Key], command: &'static Command) {
        if keys.is_empty() {
            *self = KeyTrie::Leaf(command);
            return;
        }

        // Binding through an existing leaf (say `d` then `dd`) turns the leaf
        // into a node; the shorter binding is dropped.
        if matches!(self, KeyTrie::Leaf(_)) {
            *self = KeyTrie::new();
        }

        if let KeyTrie::Node(map) = self {
            map.entry(keys[0])
                .or_insert_with(KeyTrie::new)
                .bind(&keys[1..], command);
        }
    }

    /// Bind a sequence written as a string, e.g. `"dd"` or `"C-w v"`.
    ///
    /// Space-separated tokens are parsed individually; a single token with no
    /// modifiers is treated as a sequence of character keys, so `"dd"` means
    /// `d` then `d`.
    pub fn bind_str(&mut self, sequence: &str, command_name: &str) {
        let command = match crate::commands::find(command_name) {
            Some(c) => c,
            None => {
                debug_assert!(false, "unknown command: {command_name}");
                return;
            }
        };

        let mut keys = Vec::new();
        for token in sequence.split(' ').filter(|t| !t.is_empty()) {
            if token.len() > 1 && !token.contains('-') && !token.starts_with('<') {
                keys.extend(token.chars().map(Key::char));
            } else if let Some(key) = Key::parse(token) {
                keys.push(key);
            } else {
                debug_assert!(false, "unparseable key: {token}");
                return;
            }
        }

        self.bind(&keys, command);
    }

    /// The keys available after `prefix`: (key, command name), with `…` for
    /// deeper groups. Sorted by key for a stable display.
    pub fn continuations(&self, prefix: &[Key]) -> Vec<(String, String)> {
        let mut node = self;
        for key in prefix {
            match node {
                KeyTrie::Node(map) => match map.get(key) {
                    Some(next) => node = next,
                    None => return Vec::new(),
                },
                KeyTrie::Leaf(_) => return Vec::new(),
            }
        }
        let KeyTrie::Node(map) = node else {
            return Vec::new();
        };
        let mut out: Vec<(String, String)> = map
            .iter()
            .map(|(key, trie)| {
                let target = match trie {
                    KeyTrie::Leaf(command) => command.name.to_string(),
                    KeyTrie::Node(_) => "…".to_string(),
                };
                (key.display(), target)
            })
            .collect();
        out.sort();
        out
    }

    /// The shortest key sequence bound to `name`, e.g. `"<space> f"`.
    pub fn binding_of(&self, name: &str) -> Option<String> {
        fn walk(trie: &KeyTrie, name: &str, path: &mut Vec<String>, found: &mut Vec<String>) {
            match trie {
                KeyTrie::Leaf(command) => {
                    if command.name == name {
                        found.push(path.join(" "));
                    }
                }
                KeyTrie::Node(map) => {
                    for (key, next) in map {
                        path.push(key.display());
                        walk(next, name, path, found);
                        path.pop();
                    }
                }
            }
        }
        let mut found = Vec::new();
        walk(self, name, &mut Vec::new(), &mut found);
        found.into_iter().min_by_key(|s| (s.len(), s.clone()))
    }

    pub fn lookup(&self, keys: &[Key]) -> KeymapResult {
        let mut node = self;
        for key in keys {
            match node {
                KeyTrie::Leaf(_) => return KeymapResult::NotFound,
                KeyTrie::Node(map) => match map.get(key) {
                    Some(next) => node = next,
                    None => return KeymapResult::NotFound,
                },
            }
        }
        match node {
            KeyTrie::Leaf(command) => KeymapResult::Matched(command),
            KeyTrie::Node(map) if map.is_empty() => KeymapResult::NotFound,
            KeyTrie::Node(_) => KeymapResult::Pending,
        }
    }
}

impl Default for KeyTrie {
    fn default() -> Self {
        KeyTrie::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_modified_keys() {
        assert_eq!(Key::parse("j"), Some(Key::char('j')));
        assert_eq!(
            Key::parse("C-d"),
            Some(Key {
                code: KeyCode::Char('d'),
                ctrl: true,
                alt: false
            })
        );
        assert_eq!(Key::parse("Ctrl-d"), Key::parse("C-d"));
        assert_eq!(Key::parse("Alt-x"), Key::parse("A-x"));
        assert_eq!(Key::parse("<esc>"), Some(Key::new(KeyCode::Esc)));
        assert_eq!(
            Key::parse("A-x"),
            Some(Key {
                code: KeyCode::Char('x'),
                ctrl: false,
                alt: true
            })
        );
    }

    #[test]
    fn multi_key_sequence_reports_pending_then_matches() {
        let mut map = KeyTrie::new();
        map.bind_str("dd", "delete_line");

        assert!(matches!(
            map.lookup(&[Key::char('d')]),
            KeymapResult::Pending
        ));
        assert!(matches!(
            map.lookup(&[Key::char('d'), Key::char('d')]),
            KeymapResult::Matched(_)
        ));
        assert!(matches!(
            map.lookup(&[Key::char('d'), Key::char('x')]),
            KeymapResult::NotFound
        ));
    }

    #[test]
    fn unbound_key_is_not_found() {
        let map = KeyTrie::new();
        assert!(matches!(
            map.lookup(&[Key::char('q')]),
            KeymapResult::NotFound
        ));
    }
}
