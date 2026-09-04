//! A terminal emulator: bytes from a pty in, a grid of styled cells out.
//!
//! This speaks the xterm dialect of VT100 well enough for the things you
//! run in an editor's terminal split — shells, `ls --color`, `git log`,
//! `cargo`, `less`, `htop`, even a nested editor. Cursor motion, erasing,
//! insert/delete, scroll regions, the alternate screen, 16/256/truecolor SGR,
//! DEC line drawing, bracketed paste, a scrollback buffer. Mouse reporting
//! and the odder DEC modes are parsed and ignored rather than misparsed.
//!
//! `Screen` knows nothing about processes or the editor. `feed` takes bytes
//! and returns any bytes the application is owed in reply (cursor position
//! reports and the like); `row` hands back cells for drawing.

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;

/// A cell color. `Default` is whatever the editor theme's background or
/// foreground is; indexed colors are the terminal's 16 named ones and then
/// the 256-color cube.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Default,
    Idx(u8),
    Rgb(u8, u8, u8),
}

pub const BOLD: u8 = 1;
pub const DIM: u8 = 2;
pub const ITALIC: u8 = 4;
pub const UNDERLINE: u8 = 8;
pub const REVERSE: u8 = 16;
pub const STRIKE: u8 = 32;

/// The second column of a double-width character.
pub const WIDE_TAIL: char = '\0';

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Col,
    pub bg: Col,
    pub attrs: u8,
}

impl Cell {
    pub const BLANK: Cell = Cell {
        ch: ' ',
        fg: Col::Default,
        bg: Col::Default,
        attrs: 0,
    };

    pub fn is_wide_tail(&self) -> bool {
        self.ch == WIDE_TAIL
    }
}

/// How many lines of history to keep once they scroll off the top.
const MAX_SCROLLBACK: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    Esc,
    Csi,
    Osc,
    OscEsc,
    /// DCS / APC / PM / SOS: a string we skip until ST.
    Str,
    StrEsc,
    /// `ESC (`, `ESC )`: the next byte names a character set for G0/G1.
    Charset(usize),
    /// `ESC #`: the next byte is a DEC screen alignment request.
    Hash,
}

/// A saved cursor (DECSC): where it was and what it was drawing with.
#[derive(Clone, Copy)]
struct SavedCursor {
    row: usize,
    col: usize,
    pen: Cell,
    origin: bool,
    charsets: [bool; 2],
    shift: usize,
}

/// The main screen, stashed while the alternate screen is in use.
struct MainScreen {
    lines: Vec<Vec<Cell>>,
    row: usize,
    col: usize,
    pending_wrap: bool,
}

pub struct Screen {
    cols: usize,
    rows: usize,
    lines: Vec<Vec<Cell>>,
    scrollback: VecDeque<Vec<Cell>>,
    row: usize,
    col: usize,
    /// The cursor sits past the last column; the next character wraps.
    pending_wrap: bool,
    pen: Cell,
    /// Scroll region, inclusive.
    top: usize,
    bottom: usize,
    saved: Option<SavedCursor>,
    alt: Option<MainScreen>,
    /// DECCKM: arrow keys send `ESC O A` instead of `ESC [ A`.
    pub app_cursor: bool,
    /// Mode 2004: wrap pastes in `ESC [ 200 ~` … `ESC [ 201 ~`.
    pub bracketed_paste: bool,
    pub cursor_visible: bool,
    autowrap: bool,
    origin: bool,
    insert: bool,
    /// Which of G0/G1 is the DEC line-drawing set, and which is shifted in.
    charsets: [bool; 2],
    shift: usize,
    tabs: Vec<bool>,
    /// Set by OSC 0/2 — shells put the cwd or the running command here.
    pub title: String,
    last_char: Option<char>,

    state: State,
    params: Vec<u16>,
    param: Option<u16>,
    private: Option<u8>,
    intermediates: Vec<u8>,
    osc: Vec<u8>,
    utf8: Vec<u8>,
    utf8_need: usize,
}

impl Screen {
    pub fn new(cols: usize, rows: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Screen {
            cols,
            rows,
            lines: vec![vec![Cell::BLANK; cols]; rows],
            scrollback: VecDeque::new(),
            row: 0,
            col: 0,
            pending_wrap: false,
            pen: Cell::BLANK,
            top: 0,
            bottom: rows - 1,
            saved: None,
            alt: None,
            app_cursor: false,
            bracketed_paste: false,
            cursor_visible: true,
            autowrap: true,
            origin: false,
            insert: false,
            charsets: [false; 2],
            shift: 0,
            tabs: default_tabs(cols),
            title: String::new(),
            last_char: None,
            state: State::Ground,
            params: Vec::new(),
            param: None,
            private: None,
            intermediates: Vec::new(),
            osc: Vec::new(),
            utf8: Vec::new(),
            utf8_need: 0,
        }
    }

    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// (row, col) of the cursor.
    pub fn cursor(&self) -> (usize, usize) {
        (self.row, self.col)
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Screen row `r` as seen with the view scrolled `scroll` lines back
    /// into history (0 = live).
    pub fn row(&self, scroll: usize, r: usize) -> &[Cell] {
        let back = scroll.min(self.scrollback.len());
        if r < back {
            &self.scrollback[self.scrollback.len() - back + r]
        } else {
            &self.lines[(r - back).min(self.rows - 1)]
        }
    }

    /// The live rows as plain strings, trailing blanks trimmed. For tests.
    #[cfg(test)]
    pub fn text_rows(&self) -> Vec<String> {
        self.lines
            .iter()
            .map(|l| {
                l.iter()
                    .filter(|c| !c.is_wide_tail())
                    .map(|c| c.ch)
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    // ---- geometry ----------------------------------------------------------

    /// Fit the grid to a new size. Shrinking pushes lines off the top into
    /// scrollback rather than losing them; growing pulls them back.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        for line in self.lines.iter_mut().chain(self.scrollback.iter_mut()) {
            line.resize(cols, Cell::BLANK);
            // A wide char cut in half at the new edge is dropped whole.
            if line[cols - 1].ch.width() == Some(2) {
                line[cols - 1] = Cell::BLANK;
            }
        }
        if let Some(main) = self.alt.as_mut() {
            for line in main.lines.iter_mut() {
                line.resize(cols, Cell::BLANK);
            }
            main.col = main.col.min(cols - 1);
        }
        self.cols = cols;
        let mut tabs = default_tabs(cols);
        for (i, t) in self.tabs.iter().enumerate().take(cols) {
            tabs[i] = *t;
        }
        self.tabs = tabs;

        let in_alt = self.alt.is_some();
        while self.lines.len() > rows {
            // Drop blank rows below the cursor first; only then scroll.
            let last_is_blank = self.lines.last().is_some_and(|l| is_blank(l));
            if last_is_blank && self.row + 1 < self.lines.len() {
                self.lines.pop();
            } else {
                let line = self.lines.remove(0);
                if !in_alt {
                    self.push_scrollback(line);
                }
                self.row = self.row.saturating_sub(1);
            }
        }
        while self.lines.len() < rows {
            let restored = if in_alt {
                None
            } else {
                self.scrollback.pop_back()
            };
            match restored {
                Some(line) => {
                    self.lines.insert(0, line);
                    self.row += 1;
                }
                None => self.lines.push(vec![Cell::BLANK; cols]),
            }
        }
        if let Some(main) = self.alt.as_mut() {
            main.lines.resize(rows, vec![Cell::BLANK; cols]);
            main.row = main.row.min(rows - 1);
        }
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
        self.pending_wrap = false;
    }

    fn blank(&self) -> Cell {
        Cell {
            ch: ' ',
            fg: Col::Default,
            bg: self.pen.bg,
            attrs: 0,
        }
    }

    fn blank_line(&self) -> Vec<Cell> {
        vec![self.blank(); self.cols]
    }

    fn push_scrollback(&mut self, line: Vec<Cell>) {
        if self.scrollback.len() >= MAX_SCROLLBACK {
            self.scrollback.pop_front();
        }
        self.scrollback.push_back(line);
    }

    // ---- input -------------------------------------------------------------

    /// Interpret a chunk of output. Anything the application asked to be
    /// told (a cursor position report, device attributes) is appended to
    /// `reply` for the caller to write back to it.
    pub fn feed(&mut self, bytes: &[u8], reply: &mut Vec<u8>) {
        for &b in bytes {
            self.byte(b, reply);
        }
    }

    fn byte(&mut self, b: u8, reply: &mut Vec<u8>) {
        if !self.utf8.is_empty() {
            if b & 0xC0 == 0x80 {
                self.utf8.push(b);
                if self.utf8.len() == self.utf8_need {
                    let ch = std::str::from_utf8(&self.utf8)
                        .ok()
                        .and_then(|s| s.chars().next())
                        .unwrap_or('\u{FFFD}');
                    self.utf8.clear();
                    self.print(ch);
                }
                return;
            }
            // A sequence cut short: show that something was there, then
            // handle this byte on its own.
            self.utf8.clear();
            self.print('\u{FFFD}');
        }

        match self.state {
            State::Ground => match b {
                0x00..=0x1f | 0x7f => self.control(b),
                0x20..=0x7e => {
                    let ch = self.map_charset(b as char);
                    self.print(ch);
                }
                0x80..=0xff => {
                    let need = match b {
                        0xC2..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        0xF0..=0xF4 => 4,
                        _ => {
                            self.print('\u{FFFD}');
                            return;
                        }
                    };
                    self.utf8.push(b);
                    self.utf8_need = need;
                }
            },
            State::Esc => {
                self.state = State::Ground;
                match b {
                    b'[' => {
                        self.state = State::Csi;
                        self.params.clear();
                        self.param = None;
                        self.private = None;
                        self.intermediates.clear();
                    }
                    b']' => {
                        self.state = State::Osc;
                        self.osc.clear();
                    }
                    b'P' | b'X' | b'^' | b'_' => self.state = State::Str,
                    b'(' => self.state = State::Charset(0),
                    b')' => self.state = State::Charset(1),
                    b'*' | b'+' => self.state = State::Charset(2),
                    b'#' => self.state = State::Hash,
                    b'7' => self.save_cursor(),
                    b'8' => self.restore_cursor(),
                    b'D' => self.linefeed(),
                    b'E' => {
                        self.col = 0;
                        self.linefeed();
                    }
                    b'M' => self.reverse_index(),
                    b'H' => self.tabs[self.col] = true,
                    b'c' => self.reset(),
                    0x00..=0x1f => {
                        self.control(b);
                        self.state = State::Esc;
                    }
                    _ => {} // `=`, `>`, `\`, and friends: nothing to do
                }
            }
            State::Csi => match b {
                b'0'..=b'9' => {
                    let d = (b - b'0') as u16;
                    self.param = Some(self.param.unwrap_or(0).saturating_mul(10).saturating_add(d));
                }
                b';' | b':' => {
                    self.params.push(self.param.take().unwrap_or(0));
                }
                b'?' | b'>' | b'<' | b'=' if self.params.is_empty() && self.param.is_none() => {
                    self.private = Some(b);
                }
                b' '..=b'/' => self.intermediates.push(b),
                b'@'..=b'~' => {
                    if self.param.is_some() || !self.params.is_empty() {
                        self.params.push(self.param.take().unwrap_or(0));
                    }
                    self.state = State::Ground;
                    self.csi(b, reply);
                }
                0x1b => self.state = State::Esc,
                0x18 | 0x1a => self.state = State::Ground,
                0x00..=0x1f => self.control(b),
                _ => self.state = State::Ground,
            },
            State::Osc => match b {
                0x07 => {
                    self.osc_done();
                    self.state = State::Ground;
                }
                0x1b => self.state = State::OscEsc,
                _ => self.osc.push(b),
            },
            State::OscEsc => {
                self.osc_done();
                self.state = State::Ground;
                if b != b'\\' {
                    self.state = State::Esc;
                    self.byte(b, reply);
                }
            }
            State::Str => match b {
                0x1b => self.state = State::StrEsc,
                0x07 => self.state = State::Ground,
                _ => {}
            },
            State::StrEsc => {
                self.state = State::Ground;
                if b != b'\\' {
                    self.state = State::Esc;
                    self.byte(b, reply);
                }
            }
            State::Charset(g) => {
                if g < 2 {
                    self.charsets[g] = b == b'0';
                }
                self.state = State::Ground;
            }
            State::Hash => {
                if b == b'8' {
                    let e = Cell {
                        ch: 'E',
                        ..self.blank()
                    };
                    for line in self.lines.iter_mut() {
                        line.fill(e);
                    }
                }
                self.state = State::Ground;
            }
        }
    }

    fn control(&mut self, b: u8) {
        match b {
            0x08 => {
                self.col = self.col.saturating_sub(1);
                self.pending_wrap = false;
            }
            0x09 => self.tab_forward(1),
            0x0a..=0x0c => self.linefeed(),
            0x0d => {
                self.col = 0;
                self.pending_wrap = false;
            }
            0x0e => self.shift = 1,
            0x0f => self.shift = 0,
            0x1b => self.state = State::Esc,
            0x18 | 0x1a => self.state = State::Ground,
            _ => {} // BEL, NUL, DEL, …
        }
    }

    fn map_charset(&self, c: char) -> char {
        if !self.charsets[self.shift] {
            return c;
        }
        match c {
            'j' => '┘',
            'k' => '┐',
            'l' => '┌',
            'm' => '└',
            'n' => '┼',
            'q' => '─',
            't' => '├',
            'u' => '┤',
            'v' => '┴',
            'w' => '┬',
            'x' => '│',
            'a' => '▒',
            '`' => '◆',
            'f' => '°',
            'g' => '±',
            'o' => '⎺',
            'p' => '⎻',
            'r' => '⎼',
            's' => '⎽',
            'y' => '≤',
            'z' => '≥',
            '{' => 'π',
            '|' => '≠',
            '}' => '£',
            '~' => '·',
            '_' => ' ',
            other => other,
        }
    }

    fn print(&mut self, ch: char) {
        let width = ch.width().unwrap_or(0);
        if width == 0 {
            return; // combining marks and zero-width joiners: not cells
        }
        if self.pending_wrap {
            self.pending_wrap = false;
            self.col = 0;
            self.linefeed();
        }
        if width == 2 && self.col + 1 >= self.cols {
            // No room for both halves: blank the stub and wrap early.
            let blank = self.blank();
            self.put(self.row, self.col, blank);
            if self.autowrap {
                self.col = 0;
                self.linefeed();
            } else {
                self.col = self.cols.saturating_sub(2);
            }
        }
        if self.insert {
            let blank = self.blank();
            let row = &mut self.lines[self.row];
            for _ in 0..width {
                row.pop();
                row.insert(self.col, blank);
            }
        }
        let cell = Cell { ch, ..self.pen };
        self.put(self.row, self.col, cell);
        if width == 2 {
            let tail = Cell {
                ch: WIDE_TAIL,
                ..self.pen
            };
            self.lines[self.row][self.col + 1] = tail;
        }
        self.col += width;
        if self.col >= self.cols {
            self.col = self.cols - 1;
            if self.autowrap {
                self.pending_wrap = true;
            }
        }
        self.last_char = Some(ch);
    }

    /// Write one cell, keeping wide characters whole: overwriting either
    /// half of one blanks the other half.
    fn put(&mut self, row: usize, col: usize, cell: Cell) {
        let line = &mut self.lines[row];
        if line[col].is_wide_tail() && col > 0 {
            line[col - 1] = Cell::BLANK;
        }
        if col + 1 < line.len() && line[col + 1].is_wide_tail() {
            line[col + 1] = Cell::BLANK;
        }
        line[col] = cell;
    }

    fn linefeed(&mut self) {
        if self.row == self.bottom {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
        self.pending_wrap = false;
    }

    fn reverse_index(&mut self) {
        if self.row == self.top {
            self.scroll_down(1);
        } else {
            self.row = self.row.saturating_sub(1);
        }
        self.pending_wrap = false;
    }

    fn scroll_up(&mut self, n: usize) {
        let blank = self.blank_line();
        for _ in 0..n.min(self.bottom - self.top + 1) {
            let line = self.lines.remove(self.top);
            if self.top == 0 && self.alt.is_none() {
                self.push_scrollback(line);
            }
            self.lines.insert(self.bottom, blank.clone());
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let blank = self.blank_line();
        for _ in 0..n.min(self.bottom - self.top + 1) {
            self.lines.remove(self.bottom);
            self.lines.insert(self.top, blank.clone());
        }
    }

    fn tab_forward(&mut self, n: usize) {
        for _ in 0..n {
            let next = (self.col + 1..self.cols).find(|&c| self.tabs[c]);
            self.col = next.unwrap_or(self.cols - 1);
        }
        self.pending_wrap = false;
    }

    fn tab_backward(&mut self, n: usize) {
        for _ in 0..n {
            let prev = (0..self.col).rev().find(|&c| self.tabs[c]);
            self.col = prev.unwrap_or(0);
        }
        self.pending_wrap = false;
    }

    /// Blank the cells `from..to` on `row`, never leaving half a wide char.
    fn clear_cells(&mut self, row: usize, from: usize, to: usize) {
        let blank = self.blank();
        let to = to.min(self.cols);
        if from >= to {
            return;
        }
        let line = &mut self.lines[row];
        if line[from].is_wide_tail() && from > 0 {
            line[from - 1] = blank;
        }
        if to < self.cols && line[to].is_wide_tail() {
            line[to] = blank;
        }
        line[from..to].fill(blank);
    }

    fn save_cursor(&mut self) {
        self.saved = Some(SavedCursor {
            row: self.row,
            col: self.col,
            pen: self.pen,
            origin: self.origin,
            charsets: self.charsets,
            shift: self.shift,
        });
    }

    fn restore_cursor(&mut self) {
        if let Some(s) = self.saved {
            self.row = s.row.min(self.rows - 1);
            self.col = s.col.min(self.cols - 1);
            self.pen = s.pen;
            self.origin = s.origin;
            self.charsets = s.charsets;
            self.shift = s.shift;
        } else {
            self.row = 0;
            self.col = 0;
            self.pen = Cell::BLANK;
        }
        self.pending_wrap = false;
    }

    fn leave_alt(&mut self) {
        if let Some(main) = self.alt.take() {
            self.lines = main.lines;
            self.row = main.row.min(self.rows - 1);
            self.col = main.col.min(self.cols - 1);
            self.pending_wrap = main.pending_wrap;
        }
    }

    fn reset(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        let scrollback = std::mem::take(&mut self.scrollback);
        *self = Screen::new(cols, rows);
        self.scrollback = scrollback;
    }

    fn osc_done(&mut self) {
        let text = String::from_utf8_lossy(&self.osc).into_owned();
        let (num, rest) = text.split_once(';').unwrap_or((text.as_str(), ""));
        if matches!(num, "0" | "2") {
            self.title = rest.to_string();
        }
    }

    /// Parameter `i`, with `0` and absence both meaning `default`.
    fn p(&self, i: usize, default: usize) -> usize {
        match self.params.get(i) {
            Some(&v) if v != 0 => v as usize,
            _ => default,
        }
    }

    fn csi(&mut self, action: u8, reply: &mut Vec<u8>) {
        if !self.intermediates.is_empty() {
            return; // DECSCUSR, soft resets, and other things we don't draw
        }
        let last_row = self.rows - 1;
        let last_col = self.cols - 1;
        match (self.private, action) {
            (None, b'@') => {
                let n = self.p(0, 1).min(self.cols - self.col);
                let blank = self.blank();
                let line = &mut self.lines[self.row];
                for _ in 0..n {
                    line.pop();
                    line.insert(self.col, blank);
                }
            }
            (None, b'A') => {
                let floor = if self.row >= self.top { self.top } else { 0 };
                self.row = self.row.saturating_sub(self.p(0, 1)).max(floor);
                self.pending_wrap = false;
            }
            (None, b'B') | (None, b'e') => {
                let ceil = if self.row <= self.bottom {
                    self.bottom
                } else {
                    last_row
                };
                self.row = (self.row + self.p(0, 1)).min(ceil);
                self.pending_wrap = false;
            }
            (None, b'C') | (None, b'a') => {
                self.col = (self.col + self.p(0, 1)).min(last_col);
                self.pending_wrap = false;
            }
            (None, b'D') => {
                self.col = self.col.saturating_sub(self.p(0, 1));
                self.pending_wrap = false;
            }
            (None, b'E') => {
                self.row = (self.row + self.p(0, 1)).min(last_row);
                self.col = 0;
                self.pending_wrap = false;
            }
            (None, b'F') => {
                self.row = self.row.saturating_sub(self.p(0, 1));
                self.col = 0;
                self.pending_wrap = false;
            }
            (None, b'G') | (None, b'`') => {
                self.col = (self.p(0, 1) - 1).min(last_col);
                self.pending_wrap = false;
            }
            (None, b'H') | (None, b'f') => {
                let mut row = self.p(0, 1) - 1;
                if self.origin {
                    row = (row + self.top).min(self.bottom);
                }
                self.row = row.min(last_row);
                self.col = (self.p(1, 1) - 1).min(last_col);
                self.pending_wrap = false;
            }
            (None, b'I') => self.tab_forward(self.p(0, 1)),
            (None, b'J') => match self.params.first().copied().unwrap_or(0) {
                0 => {
                    self.clear_cells(self.row, self.col, self.cols);
                    for r in self.row + 1..self.rows {
                        self.clear_cells(r, 0, self.cols);
                    }
                }
                1 => {
                    for r in 0..self.row {
                        self.clear_cells(r, 0, self.cols);
                    }
                    self.clear_cells(self.row, 0, self.col + 1);
                }
                2 => {
                    for r in 0..self.rows {
                        self.clear_cells(r, 0, self.cols);
                    }
                }
                3 => self.scrollback.clear(),
                _ => {}
            },
            (None, b'K') => match self.params.first().copied().unwrap_or(0) {
                0 => self.clear_cells(self.row, self.col, self.cols),
                1 => self.clear_cells(self.row, 0, self.col + 1),
                2 => self.clear_cells(self.row, 0, self.cols),
                _ => {}
            },
            (None, b'L') => {
                if self.row >= self.top && self.row <= self.bottom {
                    let n = self.p(0, 1).min(self.bottom - self.row + 1);
                    let blank = self.blank_line();
                    for _ in 0..n {
                        self.lines.remove(self.bottom);
                        self.lines.insert(self.row, blank.clone());
                    }
                }
                self.pending_wrap = false;
            }
            (None, b'M') => {
                if self.row >= self.top && self.row <= self.bottom {
                    let n = self.p(0, 1).min(self.bottom - self.row + 1);
                    let blank = self.blank_line();
                    for _ in 0..n {
                        self.lines.remove(self.row);
                        self.lines.insert(self.bottom, blank.clone());
                    }
                }
                self.pending_wrap = false;
            }
            (None, b'P') => {
                let n = self.p(0, 1).min(self.cols - self.col);
                let blank = self.blank();
                self.clear_cells(self.row, self.col, self.col + n);
                let line = &mut self.lines[self.row];
                for _ in 0..n {
                    line.remove(self.col);
                    line.push(blank);
                }
            }
            (None, b'S') => self.scroll_up(self.p(0, 1)),
            (None, b'T') => self.scroll_down(self.p(0, 1)),
            (None, b'X') => {
                let n = self.p(0, 1);
                self.clear_cells(self.row, self.col, self.col + n);
            }
            (None, b'Z') => self.tab_backward(self.p(0, 1)),
            (None, b'b') => {
                if let Some(ch) = self.last_char {
                    for _ in 0..self.p(0, 1) {
                        self.print(ch);
                    }
                }
            }
            (None, b'd') => {
                self.row = (self.p(0, 1) - 1).min(last_row);
                self.pending_wrap = false;
            }
            (None, b'g') => match self.params.first().copied().unwrap_or(0) {
                0 => self.tabs[self.col] = false,
                3 => self.tabs.fill(false),
                _ => {}
            },
            (None, b'h') | (None, b'l') => {
                if self.params.contains(&4) {
                    self.insert = action == b'h';
                }
            }
            (Some(b'?'), b'h') | (Some(b'?'), b'l') => {
                let on = action == b'h';
                for i in 0..self.params.len() {
                    match self.params[i] {
                        1 => self.app_cursor = on,
                        6 => {
                            self.origin = on;
                            self.row = if on { self.top } else { 0 };
                            self.col = 0;
                        }
                        7 => self.autowrap = on,
                        25 => self.cursor_visible = on,
                        47 | 1047 => {
                            if on {
                                self.enter_alt_screen();
                            } else {
                                self.leave_alt();
                            }
                        }
                        1048 => {
                            if on {
                                self.save_cursor();
                            } else {
                                self.restore_cursor();
                            }
                        }
                        1049 => {
                            if on {
                                self.save_cursor();
                                self.enter_alt_screen();
                            } else {
                                self.leave_alt();
                                self.restore_cursor();
                            }
                        }
                        2004 => self.bracketed_paste = on,
                        _ => {} // mouse, blink, focus events: not reported
                    }
                }
            }
            (None, b'm') => self.sgr(),
            (None, b'n') => match self.params.first().copied().unwrap_or(0) {
                5 => reply.extend_from_slice(b"\x1b[0n"),
                6 => {
                    let row = if self.origin {
                        self.row - self.top
                    } else {
                        self.row
                    };
                    reply.extend_from_slice(
                        format!("\x1b[{};{}R", row + 1, self.col + 1).as_bytes(),
                    );
                }
                _ => {}
            },
            (None, b'c') => reply.extend_from_slice(b"\x1b[?6c"),
            (Some(b'>'), b'c') => reply.extend_from_slice(b"\x1b[>0;0;0c"),
            (None, b'r') => {
                let top = self.p(0, 1) - 1;
                let bottom = self.p(1, self.rows) - 1;
                if top < bottom && bottom <= last_row {
                    self.top = top;
                    self.bottom = bottom;
                    self.row = if self.origin { top } else { 0 };
                    self.col = 0;
                    self.pending_wrap = false;
                }
            }
            (None, b's') => self.save_cursor(),
            (None, b'u') => self.restore_cursor(),
            _ => {} // window ops, cursor styles, DEC private queries
        }
    }

    fn enter_alt_screen(&mut self) {
        if self.alt.is_some() {
            return;
        }
        let blank = vec![vec![Cell::BLANK; self.cols]; self.rows];
        let lines = std::mem::replace(&mut self.lines, blank);
        self.alt = Some(MainScreen {
            lines,
            row: self.row,
            col: self.col,
            pending_wrap: self.pending_wrap,
        });
        self.pending_wrap = false;
    }

    fn sgr(&mut self) {
        if self.params.is_empty() {
            self.pen = Cell::BLANK;
            return;
        }
        let params = std::mem::take(&mut self.params);
        let mut i = 0;
        while i < params.len() {
            let p = params[i];
            match p {
                0 => self.pen = Cell::BLANK,
                1 => self.pen.attrs |= BOLD,
                2 => self.pen.attrs |= DIM,
                3 => self.pen.attrs |= ITALIC,
                4 => self.pen.attrs |= UNDERLINE,
                7 => self.pen.attrs |= REVERSE,
                9 => self.pen.attrs |= STRIKE,
                22 => self.pen.attrs &= !(BOLD | DIM),
                23 => self.pen.attrs &= !ITALIC,
                24 => self.pen.attrs &= !UNDERLINE,
                27 => self.pen.attrs &= !REVERSE,
                29 => self.pen.attrs &= !STRIKE,
                30..=37 => self.pen.fg = Col::Idx((p - 30) as u8),
                39 => self.pen.fg = Col::Default,
                40..=47 => self.pen.bg = Col::Idx((p - 40) as u8),
                49 => self.pen.bg = Col::Default,
                90..=97 => self.pen.fg = Col::Idx((p - 90 + 8) as u8),
                100..=107 => self.pen.bg = Col::Idx((p - 100 + 8) as u8),
                38 | 48 => {
                    let (color, used) = extended_color(&params[i + 1..]);
                    if let Some(color) = color {
                        if p == 38 {
                            self.pen.fg = color;
                        } else {
                            self.pen.bg = color;
                        }
                    }
                    i += used;
                }
                _ => {} // blink, hidden, fonts, underline colors
            }
            i += 1;
        }
        self.params = params;
    }
}

/// `38;5;n` / `38;2;r;g;b` after the 38 or 48: the color and how many
/// parameters it consumed.
fn extended_color(rest: &[u16]) -> (Option<Col>, usize) {
    match rest.first() {
        Some(5) => match rest.get(1) {
            Some(&n) => (Some(Col::Idx(n.min(255) as u8)), 2),
            None => (None, 1),
        },
        Some(2) => match (rest.get(1), rest.get(2), rest.get(3)) {
            (Some(&r), Some(&g), Some(&b)) => (
                Some(Col::Rgb(
                    r.min(255) as u8,
                    g.min(255) as u8,
                    b.min(255) as u8,
                )),
                4,
            ),
            _ => (None, rest.len()),
        },
        _ => (None, 0),
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c % 8 == 0).collect()
}

fn is_blank(line: &[Cell]) -> bool {
    line.iter().all(|c| c.ch == ' ' && c.bg == Col::Default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(cols: usize, rows: usize, input: &str) -> Screen {
        let mut s = Screen::new(cols, rows);
        let mut reply = Vec::new();
        s.feed(input.as_bytes(), &mut reply);
        s
    }

    #[test]
    fn text_lands_where_the_cursor_is_and_wraps_at_the_edge() {
        let s = screen(5, 3, "hello world");
        assert_eq!(s.text_rows(), ["hello", " worl", "d"]);
        assert_eq!(s.cursor(), (2, 1));
    }

    #[test]
    fn cr_lf_and_backspace_move_the_cursor() {
        let s = screen(10, 3, "ab\r\ncd\x08X");
        assert_eq!(s.text_rows(), ["ab", "cX", ""]);
    }

    #[test]
    fn cursor_motion_and_erase() {
        let s = screen(10, 3, "aaaaa\r\nbbbbb\r\nccccc\x1b[2;3H\x1b[K");
        assert_eq!(s.text_rows(), ["aaaaa", "bb", "ccccc"]);
        let s = screen(10, 3, "aaaaa\r\nbbbbb\r\nccccc\x1b[2;3H\x1b[J");
        assert_eq!(s.text_rows(), ["aaaaa", "bb", ""]);
        let s = screen(10, 3, "aaaaa\r\nbbbbb\x1b[2J\x1b[H!");
        assert_eq!(s.text_rows(), ["!", "", ""]);
        let s = screen(10, 3, "abcdef\x1b[3G\x1b[2P");
        assert_eq!(s.text_rows(), ["abef", "", ""]);
        let s = screen(10, 3, "abcdef\x1b[3G\x1b[2@");
        assert_eq!(s.text_rows(), ["ab  cdef", "", ""]);
        let s = screen(10, 3, "abcdef\x1b[3G\x1b[2X");
        assert_eq!(s.text_rows(), ["ab  ef", "", ""]);
    }

    #[test]
    fn scrolling_off_the_top_goes_into_history() {
        let s = screen(4, 2, "1\r\n2\r\n3\r\n4");
        assert_eq!(s.text_rows(), ["3", "4"]);
        assert_eq!(s.scrollback_len(), 2);
        let line: String = s.row(2, 0).iter().map(|c| c.ch).collect();
        assert_eq!(line.trim_end(), "1");
        let line: String = s.row(1, 1).iter().map(|c| c.ch).collect();
        assert_eq!(line.trim_end(), "3");
    }

    #[test]
    fn scroll_regions_keep_the_rest_of_the_screen_still() {
        // Rows 2-3 scroll; row 1 and row 4 stay put.
        let s = screen(4, 4, "a\r\nb\r\nc\r\nd\x1b[2;3r\x1b[3;1H\n\nX");
        assert_eq!(s.text_rows(), ["a", "", "X", "d"]);
        assert_eq!(s.scrollback_len(), 0);
    }

    #[test]
    fn insert_and_delete_lines_work_inside_the_region() {
        let s = screen(4, 4, "a\r\nb\r\nc\r\nd\x1b[2;1H\x1b[L");
        assert_eq!(s.text_rows(), ["a", "", "b", "c"]);
        let s = screen(4, 4, "a\r\nb\r\nc\r\nd\x1b[2;1H\x1b[M");
        assert_eq!(s.text_rows(), ["a", "c", "d", ""]);
    }

    #[test]
    fn sgr_colors_and_attributes_stick_to_cells() {
        let s = screen(
            10,
            1,
            "\x1b[1;31mr\x1b[0m\x1b[38;5;200mx\x1b[48;2;1;2;3my\x1b[mz",
        );
        let row = s.row(0, 0);
        assert_eq!(row[0].fg, Col::Idx(1));
        assert_eq!(row[0].attrs, BOLD);
        assert_eq!(row[1].fg, Col::Idx(200));
        assert_eq!(row[1].attrs, 0);
        assert_eq!(row[2].bg, Col::Rgb(1, 2, 3));
        assert_eq!(
            row[3],
            Cell {
                ch: 'z',
                ..Cell::BLANK
            }
        );
        let s = screen(10, 1, "\x1b[97;104mA");
        assert_eq!(
            (s.row(0, 0)[0].fg, s.row(0, 0)[0].bg),
            (Col::Idx(15), Col::Idx(12))
        );
    }

    #[test]
    fn utf8_split_across_chunks_and_wide_chars() {
        let mut s = Screen::new(6, 1);
        let mut reply = Vec::new();
        let bytes = "日本".as_bytes();
        s.feed(&bytes[..2], &mut reply);
        s.feed(&bytes[2..], &mut reply);
        let row = s.row(0, 0);
        assert_eq!(row[0].ch, '日');
        assert!(row[1].is_wide_tail());
        assert_eq!(row[2].ch, '本');
        assert_eq!(s.cursor(), (0, 4));
        // Overwriting half of a wide char blanks the other half.
        s.feed(b"\x1b[2Gx", &mut reply);
        assert_eq!(s.text_rows(), [" x本"]);
    }

    #[test]
    fn a_wide_char_that_does_not_fit_wraps_whole() {
        let s = screen(3, 2, "ab日");
        assert_eq!(s.text_rows(), ["ab", "日"]);
    }

    #[test]
    fn alternate_screen_comes_and_goes_without_the_main_one_noticing() {
        let mut s = screen(8, 2, "main");
        let mut reply = Vec::new();
        s.feed(b"\x1b[?1049h\x1b[Halt", &mut reply);
        assert_eq!(s.text_rows(), ["alt", ""]);
        s.feed(b"\x1b[?1049l", &mut reply);
        assert_eq!(s.text_rows(), ["main", ""]);
        assert_eq!(s.cursor(), (0, 4));
        assert_eq!(s.scrollback_len(), 0);
    }

    #[test]
    fn dec_line_drawing_maps_to_box_characters() {
        let s = screen(8, 1, "\x1b(0lqk\x1b(Bx");
        assert_eq!(s.text_rows(), ["┌─┐x"]);
    }

    #[test]
    fn queries_are_answered() {
        let mut s = Screen::new(10, 3);
        let mut reply = Vec::new();
        s.feed(b"\x1b[2;4H\x1b[6n", &mut reply);
        assert_eq!(reply, b"\x1b[2;4R");
        reply.clear();
        s.feed(b"\x1b[c", &mut reply);
        assert_eq!(reply, b"\x1b[?6c");
    }

    #[test]
    fn modes_are_tracked_and_osc_sets_the_title() {
        let s = screen(10, 1, "\x1b[?1h\x1b[?2004h\x1b[?25l\x1b]0;my title\x07");
        assert!(s.app_cursor);
        assert!(s.bracketed_paste);
        assert!(!s.cursor_visible);
        assert_eq!(s.title, "my title");
        // An OSC ended by ST rather than BEL.
        let s = screen(10, 1, "\x1b]2;other\x1b\\ok");
        assert_eq!(s.title, "other");
        assert_eq!(s.text_rows(), ["ok"]);
    }

    #[test]
    fn tabs_and_saved_cursors() {
        let s = screen(20, 1, "a\tb\x1b7\x1b[5Gz\x1b8c");
        assert_eq!(s.text_rows(), ["a   z   bc"]);
    }

    #[test]
    fn resizing_keeps_the_text_and_moves_lines_through_history() {
        let mut s = screen(10, 4, "1\r\n2\r\n3\r\n4");
        s.resize(10, 2);
        assert_eq!(s.text_rows(), ["3", "4"]);
        assert_eq!(s.scrollback_len(), 2);
        assert_eq!(s.cursor(), (1, 1));
        s.resize(10, 4);
        assert_eq!(s.text_rows(), ["1", "2", "3", "4"]);
        assert_eq!(s.cursor(), (3, 1));
        s.resize(3, 4);
        assert_eq!(s.size(), (3, 4));
        assert_eq!(s.text_rows(), ["1", "2", "3", "4"]);
    }

    #[test]
    fn unknown_sequences_are_swallowed_not_printed() {
        let s = screen(20, 1, "a\x1b[?1000h\x1b[>4;2m\x1b[22t\x1bP+q\x1b\\\x1b[ qb");
        assert_eq!(s.text_rows(), ["ab"]);
    }

    #[test]
    fn reverse_index_and_scroll_down() {
        let s = screen(4, 3, "a\r\nb\r\nc\x1b[H\x1bMx");
        assert_eq!(s.text_rows(), ["x", "a", "b"]);
    }
}
