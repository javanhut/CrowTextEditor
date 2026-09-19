//! The shell split: showing, hiding and feeding keys to the terminal window.

use super::*;

impl Editor {
    // ---- terminal ----------------------------------------------------------

    /// The window the terminal is shown in, if it is open and not hidden.
    pub fn terminal_win(&self) -> Option<usize> {
        self.terminal.as_ref().and_then(|t| t.win)
    }

    pub fn terminal_focused(&self) -> bool {
        self.terminal_win() == Some(self.focused)
    }

    /// `space t` / `:term`: open a shell below the window, jump to it if it
    /// is open elsewhere, or hide it if it has focus. Hiding keeps the shell
    /// running; the next `space t` brings it back with its history.
    pub fn toggle_terminal(&mut self) {
        match self.terminal_win() {
            Some(win) if win == self.focused => self.hide_terminal(),
            Some(win) => {
                self.save_focus_state();
                self.focused = win;
                self.restore_focus_state();
                self.sync_focus_mode();
            }
            None => self.show_terminal(),
        }
    }

    pub(crate) fn show_terminal(&mut self) {
        let (program, args) = crate::config::shell();
        self.show_terminal_running(&program, &args);
        self.set_status("terminal — C-\\ C-n for normal mode, space t hides it, exit closes it");
    }

    /// Open the terminal split: bring back the hidden shell if there is one,
    /// else start `program` in a fresh one. Focus lands in it either way.
    pub(crate) fn show_terminal_running(&mut self, program: &str, args: &[String]) {
        // Split first, so the shell is born at the size it will be drawn at
        // and its first prompt lays out correctly.
        self.split_window(false);
        let win = self.focused;
        let (_, _, w, h) = self.focused_rect();
        match self.terminal.as_mut() {
            Some(t) => {
                t.win = Some(win);
                t.resize(w, h);
            }
            None => {
                self.terminal_generation += 1;
                let generation = self.terminal_generation;
                match Terminal::spawn(program, args, w, h, generation, self.wake_tx.clone()) {
                    Ok(mut t) => {
                        t.win = Some(win);
                        self.terminal = Some(t);
                    }
                    Err(e) => {
                        self.detach_window(win);
                        self.set_status(format!("Error: can't start {program}: {e}"));
                        return;
                    }
                }
            }
        }
        self.sync_focus_mode();
    }

    /// Take the terminal's window down without touching the shell.
    pub(crate) fn hide_terminal(&mut self) {
        let Some(win) = self.terminal_win() else {
            return;
        };
        if self.window_count() <= 1 {
            self.set_status("the terminal is the last window — :q! quits, or exit the shell");
            return;
        }
        self.detach_window(win);
        if let Some(t) = self.terminal.as_mut() {
            t.win = None;
        }
        // Back from the shell, where files may have been changed or sealed.
        self.external_change_hint();
    }

    /// Remove a window from the layout, moving focus off it first: to the
    /// window above (the terminal lives below the text it was opened from),
    /// else the next one along.
    pub(crate) fn detach_window(&mut self, win: usize) {
        if self.focused == win && !self.focus_window_dir(0, -1) {
            self.focus_next_window();
        }
        self.layout.close(win);
        if self.layout.find(self.focused).is_none() {
            let mut ids = Vec::new();
            self.layout.leaf_ids(&mut ids);
            self.focused = ids.first().copied().unwrap_or(0);
            self.restore_focus_state();
        }
        self.sync_focus_mode();
    }

    /// Hang up on the shell and drop its window.
    pub fn close_terminal(&mut self) {
        if let Some(win) = self.terminal_win() {
            if self.window_count() > 1 {
                self.detach_window(win);
            }
        }
        self.terminal = None;
        if self.mode == Mode::Terminal {
            self.mode = Mode::Normal;
        }
    }

    /// Bytes from the pty, via the wake channel. An empty chunk is the
    /// hangup at the end.
    pub fn terminal_output(&mut self, generation: u64, bytes: &[u8]) {
        let Some(t) = self.terminal.as_mut() else {
            return;
        };
        if t.generation != generation {
            return;
        }
        if bytes.is_empty() {
            self.terminal_tick();
        } else {
            t.feed(bytes);
        }
    }

    /// Notice a shell that has exited. True when something changed.
    pub fn terminal_tick(&mut self) -> bool {
        let Some(code) = self.terminal.as_mut().and_then(Terminal::poll_exit) else {
            return false;
        };
        self.close_terminal();
        match self.terminal_install.take() {
            // The terminal was ours: its exit status is the install's.
            Some((program, true)) => {
                if code != 0 {
                    self.set_status(format!(
                        "{program}: install failed — :install {program} to retry"
                    ));
                } else if crate::config::on_path(&program) {
                    self.install_done(&program);
                } else {
                    self.set_status(format!("{program}: installed, but not on PATH"));
                }
                return true;
            }
            // The user's shell went away with the install still unseen: they
            // know, and the status line has nothing to add.
            Some((_, false)) => return true,
            None => {}
        }
        self.set_status(match code {
            0 => "shell exited".to_string(),
            n => format!("shell exited with status {n}"),
        });
        true
    }

    /// Keep the shell's screen the size of its window. Once per frame; a
    /// frame where nothing moved costs one compare.
    pub fn refresh_terminal(&mut self) {
        let Some(win) = self.terminal_win() else {
            return;
        };
        let Some((_, (.., w, h))) = self.window_rects().0.into_iter().find(|&(id, _)| id == win)
        else {
            return;
        };
        if let Some(t) = self.terminal.as_mut() {
            t.resize(w, h);
        }
    }

    /// Entering the terminal window puts you in the shell, the way vim does;
    /// leaving it puts you back in normal mode.
    pub(crate) fn sync_focus_mode(&mut self) {
        self.term_pending = None;
        if self.terminal_focused() {
            if matches!(self.mode, Mode::Normal | Mode::Insert) {
                self.set_mode(Mode::Terminal);
                self.extend = false;
            }
        } else if self.mode == Mode::Terminal {
            self.mode = Mode::Normal;
            self.pending.clear();
        }
    }

    /// Opening a file while the shell has focus: put it in a text window.
    pub(crate) fn leave_terminal_for_edit(&mut self) {
        if !self.terminal_focused() {
            return;
        }
        if self.window_count() == 1 || !self.focus_window_dir(0, -1) {
            let mut ids = Vec::new();
            self.layout.leaf_ids(&mut ids);
            let text = ids
                .into_iter()
                .find(|&id| id != self.focused && Some(id) != self.preview_win());
            match text {
                Some(id) => {
                    self.save_focus_state();
                    self.focused = id;
                    self.restore_focus_state();
                    self.sync_focus_mode();
                }
                None => self.split_window(false),
            }
        }
    }

    /// A key while the shell has focus. Everything goes to the shell except
    /// two prefixes borrowed from vim: `C-\ C-n` drops to normal mode, and
    /// `C-w` plus a key runs that window command (`C-w N` is normal mode,
    /// `C-w .` sends a literal `C-w`).
    pub(crate) fn handle_terminal_key(&mut self, key: Key) {
        if self.terminal.is_none() {
            self.mode = Mode::Normal;
            return;
        }
        let ctrl = |c: char| Key {
            code: KeyCode::Char(c),
            ctrl: true,
            alt: false,
        };
        // Without the kitty keyboard protocol, `C-\` reaches us as the raw
        // byte 0x1c, which crossterm reports as Ctrl-4.
        let is_backslash = |k: Key| k == ctrl('\\') || k == ctrl('4');
        if let Some(prefix) = self.term_pending.take() {
            if is_backslash(prefix) {
                if key == ctrl('n') {
                    self.mode = Mode::Normal;
                    self.set_status("normal mode — i returns to the shell, space t hides it");
                } else if let Some(t) = self.terminal.as_mut() {
                    t.send_key(prefix);
                    t.send_key(key);
                }
                return;
            }
            match key.code {
                KeyCode::Char('N') if !key.ctrl && !key.alt => {
                    self.mode = Mode::Normal;
                    self.set_status("normal mode — i returns to the shell, space t hides it");
                }
                KeyCode::Char('.') if !key.ctrl && !key.alt => {
                    if let Some(t) = self.terminal.as_mut() {
                        t.send_key(prefix);
                    }
                }
                KeyCode::Char(':') if !key.ctrl && !key.alt => {
                    (commands::find("command_mode").unwrap().func)(self);
                }
                KeyCode::Char('w') if key.ctrl => self.focus_next_window(),
                _ => {
                    if let KeymapResult::Matched(command) =
                        self.keymaps.normal.lookup(&[prefix, key])
                    {
                        self.run_terminal_command(command);
                    }
                }
            }
            return;
        }
        if is_backslash(key) || key == ctrl('w') {
            self.term_pending = Some(key);
            return;
        }
        if let Some(t) = self.terminal.as_mut() {
            t.send_key(key);
        }
    }

    /// Normal mode inside the terminal window: the usual keymap, with the
    /// motions scrolling the shell's history and the inserts returning to it.
    pub(crate) fn handle_terminal_normal_key(&mut self, key: Key) {
        if self.pending.is_empty() && !key.ctrl && !key.alt {
            if let KeyCode::Char(c) = key.code {
                if c.is_ascii_digit() && !(c == '0' && self.count.is_none()) {
                    let digit = c.to_digit(10).unwrap() as usize;
                    self.count = Some(self.count.unwrap_or(0).saturating_mul(10) + digit);
                    return;
                }
            }
        }
        self.pending.push(key);
        match self.keymaps.normal.lookup(&self.pending) {
            KeymapResult::Pending => {}
            KeymapResult::Matched(command) => {
                self.pending.clear();
                self.run_terminal_command(command);
                self.count = None;
            }
            KeymapResult::NotFound => {
                self.pending.clear();
                self.count = None;
            }
        }
    }

    /// What a normal-mode command means with the shell in front of you.
    /// Editing commands have nothing to act on and do nothing.
    pub(crate) fn run_terminal_command(&mut self, command: &'static commands::Command) {
        let count = self.take_count() as isize;
        let rows = self
            .terminal
            .as_ref()
            .map(|t| t.screen.size().1 as isize)
            .unwrap_or(1);
        let scroll = |editor: &mut Editor, delta: isize| {
            if let Some(t) = editor.terminal.as_mut() {
                t.scroll_by(delta);
            }
        };
        match command.name {
            "move_up" => scroll(self, count),
            "move_down" => scroll(self, -count),
            "half_page_up" => scroll(self, rows / 2 * count),
            "half_page_down" => scroll(self, -(rows / 2) * count),
            "page_up" => scroll(self, rows * count),
            "page_down" => scroll(self, -rows * count),
            "goto_file_start" => scroll(self, isize::MAX / 2),
            "goto_file_end" => scroll(self, isize::MIN / 2),
            "insert_mode"
            | "append"
            | "insert_at_line_start"
            | "append_at_line_end"
            | "open_below"
            | "open_above" => {
                scroll(self, isize::MIN / 2);
                self.mode = Mode::Terminal;
            }
            "quit" | "terminal" | "normal_mode" | "focus_left" | "focus_right" | "focus_up"
            | "focus_down" | "next_window" | "split_vertical" | "split_horizontal"
            | "command_mode" | "command_palette" | "find_files" | "grep_text" | "recent_files"
            | "file_explorer" | "tree_toggle" | "toggle_hidden" | "theme_picker" => {
                (command.func)(self)
            }
            _ => {}
        }
    }
}
