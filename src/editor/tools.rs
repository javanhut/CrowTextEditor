//! Background work the editor kicks off: tool installs and dependency-version fetches.

use super::*;

impl Editor {
    // ---- tool installs -----------------------------------------------------

    /// `:install x` — x is a tool name, or a file extension whose formatter
    /// (or, with `lsp_only`, language server) gets resolved and installed.
    pub(crate) fn install_named(&mut self, name: &str, lsp_only: bool) {
        if self.install_running() {
            self.set_status("an install is already running");
            return;
        }
        let lsp_program = |table: &[(String, String)]| {
            table
                .iter()
                .find(|(e, _)| e == name)
                .map(|(_, c)| c.as_str())
                .or_else(|| crate::config::builtin_lsp(name))
                .and_then(|c| c.split_whitespace().next().map(str::to_string))
        };
        let program = if crate::config::installer(name).is_some() {
            Some(name.to_string())
        } else if lsp_only {
            lsp_program(&self.lsp_table)
        } else {
            crate::config::formatter(name)
                .and_then(|c| c.split_whitespace().next().map(str::to_string))
                .or_else(|| lsp_program(&self.lsp_table))
        };
        match program {
            // Nothing to do — and a system package would only have asked for
            // a password it can't get.
            Some(p) if crate::config::on_path(&p) => {
                self.set_status(format!("{p} is already installed"));
            }
            Some(p) => match crate::config::installer(&p) {
                Some(cmd) => self.start_install(&p, &cmd),
                None => self.set_status(format!("don't know how to install {p}")),
            },
            None => self.set_status(format!(
                "nothing known for {name:?} — use a tool name or a file extension"
            )),
        }
    }

    /// Offer to install a missing `program` if we know how: arms the (y/N)
    /// prompt and puts it in the status line. False when we can't help.
    pub fn offer_install(&mut self, program: &str) -> bool {
        let Some(cmd) = crate::config::installer(program) else {
            return false;
        };
        if self.install_running() {
            return false;
        }
        self.set_status(format!("{program} not installed — run `{cmd}`? (y/N)"));
        self.pending_install = Some((program.to_string(), cmd.to_string()));
        true
    }

    /// Is an install underway that another would collide with?
    pub(crate) fn install_running(&self) -> bool {
        self.install.is_some() || matches!(self.terminal_install, Some((_, true)))
    }

    /// Run `cmd`: in the terminal split when it needs root, so sudo has a
    /// tty to ask for the password on; else in a background thread that
    /// `install_tick` picks the result up from.
    pub(crate) fn start_install(&mut self, program: &str, cmd: &str) {
        if cmd.starts_with("sudo ") {
            self.start_install_in_terminal(program, cmd);
            return;
        }
        let shell_cmd = cmd.to_string();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let result = match std::process::Command::new("sh")
                .args(["-c", &shell_cmd])
                .output()
            {
                Ok(o) if o.status.success() => Ok(()),
                Ok(o) => {
                    let err = String::from_utf8_lossy(&o.stderr);
                    Err(err
                        .lines()
                        .rev()
                        .find(|l| !l.trim().is_empty())
                        .unwrap_or("failed")
                        .to_string())
                }
                Err(e) => Err(e.to_string()),
            };
            let _ = tx.send(result);
        });
        self.set_status(format!("installing {program}… (`{cmd}`)"));
        self.install = Some((program.to_string(), rx));
    }

    /// A system package manager needs root, and a headless `sudo` has
    /// nowhere to ask for the password: it would hang for the rest of the
    /// session. So the command runs in the terminal split, in front of the
    /// user. With no shell open, one is started just for this and closes
    /// itself when done; an open shell is handed the command instead, and
    /// the program is watched for on PATH.
    pub(crate) fn start_install_in_terminal(&mut self, program: &str, cmd: &str) {
        if self.terminal.is_some() {
            if !self.terminal_focused() {
                self.toggle_terminal();
            }
            if let Some(t) = self.terminal.as_mut() {
                t.write(format!("{cmd}\n").as_bytes());
            }
            self.terminal_install = Some((program.to_string(), false));
            self.set_status(format!("installing {program} in the terminal…"));
            return;
        }
        // A failed install would otherwise vanish with its output; hold the
        // window so the reason can be read.
        let script = format!(
            "{cmd} || {{ printf '\\n{program}: install failed — press Enter to close\\n'; read _; exit 1; }}"
        );
        self.show_terminal_running("sh", &["-c".to_string(), script]);
        if self.terminal.is_none() {
            return; // the spawn failure is already on the status line
        }
        self.terminal_install = Some((program.to_string(), true));
        self.set_status(format!("installing {program}… (`{cmd}`)"));
    }

    /// `program` is now installed: retry what needed it.
    pub(crate) fn install_done(&mut self, program: &str) {
        // A server that failed to spawn earlier can be retried now.
        self.lsp_failed.clear();
        self.set_status(format!("{program} installed"));
        // If it formats the current buffer, finish what :fmt started.
        let formats_this = self
            .doc()
            .path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|e| e.to_str())
            .and_then(crate::config::formatter)
            .is_some_and(|c| c.split_whitespace().next() == Some(program));
        if formats_this {
            (commands::find("format_buffer").unwrap().func)(self);
        }
    }

    /// Poll the installs from the main loop. True when the status changed
    /// and a redraw is due.
    pub fn install_tick(&mut self) -> bool {
        // An install typed into the user's own shell reports nothing back;
        // the program turning up on PATH is the signal.
        if let Some((program, false)) = &self.terminal_install {
            if crate::config::on_path(program) {
                let (program, _) = self.terminal_install.take().unwrap();
                self.install_done(&program);
                return true;
            }
        }
        let Some((_, rx)) = self.install.as_ref() else {
            return false;
        };
        let result = match rx.try_recv() {
            Err(std::sync::mpsc::TryRecvError::Empty) => return false,
            Err(_) => Err("install process vanished".to_string()),
            Ok(r) => r,
        };
        let (program, _) = self.install.take().unwrap();
        match result {
            Ok(()) => self.install_done(&program),
            Err(e) => self.set_status(format!("{program}: install failed — {e}")),
        }
        true
    }

    // ---- dependency versions -------------------------------------------------

    /// Poll the registry fetches from the main loop, starting one for any
    /// open package manifest not yet fetched. True on new badges.
    pub fn deps_tick(&mut self) -> bool {
        let pending: Vec<(crate::deps::Kind, PathBuf, String)> = self
            .documents
            .iter()
            .filter_map(|d| {
                let path = d.path.as_ref()?;
                let kind = crate::deps::manifest_kind(path.file_name()?.to_str()?)?;
                (!self.deps_fetched.contains(path))
                    .then(|| (kind, path.clone(), d.text.to_string()))
            })
            .collect();
        for (kind, path, text) in pending {
            self.deps_fetched.insert(path.clone());
            let tx = match &self.deps_tx {
                Some(tx) => tx.clone(),
                None => {
                    let (tx, rx) = std::sync::mpsc::channel();
                    self.deps_rx = Some(rx);
                    self.deps_tx = Some(tx.clone());
                    tx
                }
            };
            crate::deps::fetch(kind, path, text, tx);
        }
        let Some(rx) = self.deps_rx.as_ref() else {
            return false;
        };
        let mut changed = false;
        while let Ok((kind, name, current, latest)) = rx.try_recv() {
            self.dep_info.insert((kind, name), (current, latest));
            changed = true;
        }
        changed
    }
}
