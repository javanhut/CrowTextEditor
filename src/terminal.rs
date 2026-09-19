//! A shell in a split: the pty, the process on the far end of it, and the
//! screen it draws on.
//!
//! The process is a child on a pseudo-terminal, the same way vim's
//! `:terminal` does it. A thread blocks reading the pty master and hands
//! chunks to the main loop over the editor's wake channel, so output shows
//! up the moment it arrives and the editor never blocks on the shell. Keys
//! are encoded the way xterm would send them and written straight through.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use crate::keymap::{Key, KeyCode};
use crate::vt::Screen;

/// What wakes the main loop: a key or resize from the terminal, or output
/// from a pty. The generation tags pty output so a shell that was closed
/// can't paint into the one that replaced it; an empty chunk is end of file.
pub enum Wake {
    Input(crossterm::event::Event),
    Pty(u64, Vec<u8>),
    /// Background work finished something worth drawing (a grep batch).
    Refresh,
}

pub struct Terminal {
    /// The window showing it; `None` while hidden (the shell keeps running).
    pub win: Option<usize>,
    pub screen: Screen,
    /// Lines scrolled back into history; 0 is live.
    pub scroll: usize,
    pub generation: u64,
    /// The shell's name, for the status line.
    pub name: String,
    master: File,
    child: Child,
    stop: Arc<AtomicBool>,
}

impl Terminal {
    /// Start `program` on a fresh pty of the given size. Output arrives on
    /// `wake` tagged with `generation`.
    pub fn spawn(
        program: &str,
        args: &[String],
        cols: u16,
        rows: u16,
        generation: u64,
        wake: Sender<Wake>,
    ) -> io::Result<Terminal> {
        let (master, slave) = openpty(cols, rows)?;
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave))
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env_remove("COLUMNS")
            .env_remove("LINES");
        // Runs in the child after its stdio is the pty: make it the session
        // leader and give it the pty as its controlling terminal, so job
        // control and ^C work the way they do in a real terminal.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;

        let stop = Arc::new(AtomicBool::new(false));
        let reader = master.try_clone()?;
        let flag = stop.clone();
        std::thread::spawn(move || pump(reader, generation, wake, flag));

        let name = std::path::Path::new(program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(program)
            .to_string();
        Ok(Terminal {
            win: None,
            screen: Screen::new(cols as usize, rows as usize),
            scroll: 0,
            generation,
            name,
            master,
            child,
            stop,
        })
    }

    /// The status-line label: the window title the shell set (most prompts
    /// put the directory there), else just what it is.
    pub fn label(&self) -> &str {
        if self.screen.title.is_empty() {
            "terminal"
        } else {
            &self.screen.title
        }
    }

    pub fn write(&mut self, bytes: &[u8]) {
        // A dead shell gives EIO; the exit is noticed on the next tick.
        let _ = self.master.write_all(bytes);
    }

    /// Interpret a chunk of output, answering any queries in it.
    pub fn feed(&mut self, bytes: &[u8]) {
        let mut reply = Vec::new();
        self.screen.feed(bytes, &mut reply);
        if !reply.is_empty() {
            self.write(&reply);
        }
        self.scroll = self.scroll.min(self.screen.scrollback_len());
    }

    /// Fit the screen to its window and tell the process (SIGWINCH).
    pub fn resize(&mut self, cols: u16, rows: u16) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if self.screen.size() == (cols as usize, rows as usize) {
            return;
        }
        self.screen.resize(cols as usize, rows as usize);
        let ws = winsize(cols, rows);
        unsafe {
            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &ws);
        }
    }

    /// The exit status, once the process is gone.
    pub fn poll_exit(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            Ok(None) => None,
            Err(_) => Some(-1),
        }
    }

    pub fn send_key(&mut self, key: Key) {
        let bytes = encode_key(key, self.screen.app_cursor);
        if !bytes.is_empty() {
            self.scroll = 0;
            self.write(&bytes);
        }
    }

    pub fn paste(&mut self, text: &str) {
        self.scroll = 0;
        if self.screen.bracketed_paste {
            self.write(b"\x1b[200~");
            self.write(text.as_bytes());
            self.write(b"\x1b[201~");
        } else {
            self.write(text.as_bytes());
        }
    }

    /// Move the view `delta` lines into history (positive) or back toward
    /// live (negative).
    pub fn scroll_by(&mut self, delta: isize) {
        let max = self.screen.scrollback_len() as isize;
        self.scroll = (self.scroll as isize + delta).clamp(0, max) as usize;
    }
}

impl Drop for Terminal {
    /// Hang up on the shell the way closing a terminal window would, and
    /// reap it. Killing outright is the fallback for a shell that traps
    /// SIGHUP and stays.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let pid = self.child.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGHUP);
        }
        for _ in 0..20 {
            if matches!(self.child.try_wait(), Ok(Some(_)) | Err(_)) {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The reader thread: wait for output, hand it over, repeat until the pty
/// hangs up or the terminal is dropped. Polling with a timeout, rather than
/// a bare blocking read, is what lets the thread notice it should stop and
/// close its copy of the master — which is the hangup a background job left
/// behind by the shell is waiting for.
fn pump(mut reader: File, generation: u64, wake: Sender<Wake>, stop: Arc<AtomicBool>) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut pfd = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready == 0 {
            continue;
        }
        if ready < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if wake.send(Wake::Pty(generation, buf[..n].to_vec())).is_err() {
                    return;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break, // EIO: the slave side closed
        }
    }
    let _ = wake.send(Wake::Pty(generation, Vec::new()));
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// A master/slave pty pair. The master is close-on-exec so the shell
/// doesn't inherit a handle to its own terminal.
fn openpty(cols: u16, rows: u16) -> io::Result<(File, File)> {
    let mut master = 0;
    let mut slave = 0;
    let mut ws = winsize(cols, rows);
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws as *mut libc::winsize as _,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        libc::fcntl(master, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(slave, libc::F_SETFD, libc::FD_CLOEXEC);
        Ok((File::from_raw_fd(master), File::from_raw_fd(slave)))
    }
}

/// What xterm sends for a key. `app_cursor` is DECCKM, set by full-screen
/// programs that want `ESC O A` for the arrows.
pub fn encode_key(key: Key, app_cursor: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if key.alt {
        out.push(0x1b);
    }
    // `ESC [ 1 ; m X` modifier parameter for the cursor and function keys.
    let modifier = match (key.ctrl, key.alt) {
        (false, false) => None,
        (false, true) => Some(3),
        (true, false) => Some(5),
        (true, true) => Some(7),
    };
    let cursor = |out: &mut Vec<u8>, letter: u8| match modifier {
        Some(m) => out.extend_from_slice(format!("\x1b[1;{m}{}", letter as char).as_bytes()),
        None if app_cursor => out.extend_from_slice(&[0x1b, b'O', letter]),
        None => out.extend_from_slice(&[0x1b, b'[', letter]),
    };
    let tilde = |out: &mut Vec<u8>, n: u8| match modifier {
        Some(m) => out.extend_from_slice(format!("\x1b[{n};{m}~").as_bytes()),
        None => out.extend_from_slice(format!("\x1b[{n}~").as_bytes()),
    };
    match key.code {
        KeyCode::Char(c) if key.ctrl => {
            let byte = match c {
                'a'..='z' => Some(c as u8 - b'a' + 1),
                'A'..='Z' => Some(c as u8 - b'A' + 1),
                ' ' | '@' | '2' => Some(0),
                '[' | '3' => Some(0x1b),
                '\\' | '4' => Some(0x1c),
                ']' | '5' => Some(0x1d),
                '^' | '6' => Some(0x1e),
                '_' | '7' | '-' => Some(0x1f),
                '?' | '8' => Some(0x7f),
                _ => None,
            };
            match byte {
                Some(b) => out.push(b),
                None => {
                    if key.alt {
                        out.pop();
                    }
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                }
            }
        }
        KeyCode::Char(c) => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        KeyCode::Enter => out.push(b'\r'),
        KeyCode::Esc => out.push(0x1b),
        KeyCode::Backspace => out.push(if key.ctrl { 0x08 } else { 0x7f }),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Delete => tilde(&mut out, 3),
        KeyCode::Insert => tilde(&mut out, 2),
        KeyCode::Up => cursor(&mut out, b'A'),
        KeyCode::Down => cursor(&mut out, b'B'),
        KeyCode::Right => cursor(&mut out, b'C'),
        KeyCode::Left => cursor(&mut out, b'D'),
        KeyCode::Home => cursor(&mut out, b'H'),
        KeyCode::End => cursor(&mut out, b'F'),
        KeyCode::PageUp => tilde(&mut out, 5),
        KeyCode::PageDown => tilde(&mut out, 6),
        KeyCode::F(n @ 1..=4) => match modifier {
            Some(m) => {
                out.extend_from_slice(format!("\x1b[1;{m}{}", (b'O' + n) as char).as_bytes())
            }
            None => out.extend_from_slice(&[0x1b, b'O', b'O' + n]),
        },
        KeyCode::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return Vec::new(),
            };
            tilde(&mut out, code);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Instant;

    #[test]
    fn keys_encode_the_way_xterm_sends_them() {
        let k = |s: &str| Key::parse(s).unwrap();
        assert_eq!(encode_key(k("a"), false), b"a");
        assert_eq!(encode_key(k("C-c"), false), b"\x03");
        assert_eq!(encode_key(k("C-\\"), false), b"\x1c");
        assert_eq!(encode_key(k("A-x"), false), b"\x1bx");
        assert_eq!(encode_key(k("<enter>"), false), b"\r");
        assert_eq!(encode_key(k("<bs>"), false), b"\x7f");
        assert_eq!(encode_key(k("<up>"), false), b"\x1b[A");
        assert_eq!(encode_key(k("<up>"), true), b"\x1bOA");
        assert_eq!(encode_key(k("C-<right>"), false), b"\x1b[1;5C");
        assert_eq!(encode_key(k("<del>"), false), b"\x1b[3~");
        assert_eq!(encode_key(k("<f1>"), false), b"\x1bOP");
        assert_eq!(encode_key(k("<f5>"), false), b"\x1b[15~");
        assert_eq!(encode_key(k("<s-tab>"), false), b"\x1b[Z");
        assert_eq!(encode_key(k("é"), false), "é".as_bytes());
    }

    /// The whole pipe end to end: a real pty, a real process, its output
    /// arriving on the wake channel, its exit status reported.
    #[test]
    fn a_process_on_the_pty_draws_on_the_screen_and_exits() {
        let (tx, rx) = mpsc::channel();
        let args = vec!["-c".to_string(), "printf 'hello\\n'; exit 3".to_string()];
        let mut term = Terminal::spawn("sh", &args, 40, 5, 7, tx).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut eof = false;
        while !eof && Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(Wake::Pty(7, bytes)) if bytes.is_empty() => eof = true,
                Ok(Wake::Pty(7, bytes)) => term.feed(&bytes),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(eof, "the pty should hang up when the process exits");
        assert_eq!(term.screen.text_rows()[0], "hello");
        let mut status = None;
        while status.is_none() && Instant::now() < deadline {
            status = term.poll_exit();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(status, Some(3));
    }

    #[test]
    fn dropping_a_terminal_hangs_up_on_the_shell() {
        let (tx, _rx) = mpsc::channel();
        let term = Terminal::spawn("sh", &[], 20, 4, 1, tx).unwrap();
        let pid = term.child.id() as libc::pid_t;
        let started = Instant::now();
        drop(term);
        assert!(started.elapsed() < Duration::from_secs(2));
        // The process is reaped: kill(0) can no longer find it.
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    }
}
