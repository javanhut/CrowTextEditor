//! Markdown rendered the way a browser would render it, in a terminal.
//!
//! Not a highlighter: the source markup is *consumed*. `**bold**` comes out as
//! bold text with no asterisks, a fence becomes a tinted, syntax-colored block,
//! a table becomes a drawn table. That is the difference between reading
//! markdown and reading its punctuation.
//!
//! Output is rows of styled spans plus the source line each row came from, so
//! the preview window can scroll in step with the buffer being edited.
//!
//! ponytail: a line-oriented CommonMark subset — the constructs people actually
//! type — not a spec-complete parser. Nested block quotes inside lists and
//! reference-style links are the known gaps; a real parser is the upgrade.

use crossterm::style::Color;
use ropey::Rope;

use crate::theme::Theme;

pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;
pub const UNDERLINE: u8 = 4;
pub const STRIKE: u8 = 8;

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Style {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub attrs: u8,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Debug)]
pub struct Row {
    pub spans: Vec<Span>,
    /// Line in the source buffer this row came from — the scroll-sync key.
    pub src_line: usize,
    /// Fills the rest of the row's width (code blocks and tables).
    pub bg: Option<Color>,
}

/// Render a whole markdown buffer to `width` columns.
pub fn render(text: &Rope, width: usize) -> Vec<Row> {
    let width = width.max(8);
    let lines: Vec<String> = text
        .lines()
        .map(|l| {
            l.chars()
                .filter(|&c| c != '\n' && c != '\r')
                .collect::<String>()
        })
        .collect();
    let mut md = Md {
        width,
        theme: crate::theme::current(),
        out: Vec::new(),
    };
    md.blocks(&lines, 0);
    md.out
}

struct Md {
    width: usize,
    theme: &'static Theme,
    out: Vec<Row>,
}

impl Md {
    fn base(&self) -> Style {
        Style {
            fg: self.theme.fg,
            ..Style::default()
        }
    }

    fn dim(&self) -> Style {
        Style {
            fg: Some(self.theme.gutter),
            ..Style::default()
        }
    }

    fn blank(&mut self, src_line: usize) {
        self.out.push(Row {
            spans: Vec::new(),
            src_line,
            bg: None,
        });
    }

    /// Walk `lines` (already stripped of line endings), emitting rows.
    /// `offset` is where this slice starts in the buffer, so block quotes can
    /// recurse and still report real source lines.
    fn blocks(&mut self, lines: &[String], offset: usize) {
        let mut i = 0;
        while i < lines.len() {
            let line = &lines[i];
            let trimmed = line.trim_start();
            let src = offset + i;

            if trimmed.is_empty() {
                self.blank(src);
                i += 1;
            } else if let Some(fence) = fence_of(trimmed) {
                i = self.code_block(lines, i, offset, fence);
            } else if is_rule(trimmed) {
                self.rule(src);
                i += 1;
            } else if let Some((level, title)) = heading_of(trimmed) {
                self.heading(level, title, src);
                i += 1;
            } else if trimmed.starts_with('>') {
                i = self.quote(lines, i, offset);
            } else if list_marker(line).is_some() {
                i = self.list(lines, i, offset);
            } else if is_table(lines, i) {
                i = self.table(lines, i, offset);
            } else {
                i = self.paragraph(lines, i, offset);
            }
        }
    }

    // ---- blocks ------------------------------------------------------------

    fn heading(&mut self, level: usize, title: &str, src: usize) {
        let style = Style {
            fg: Some(self.theme.border),
            bg: None,
            attrs: BOLD,
        };
        // GitHub gives h1 and h2 a rule under them and nothing else does.
        let text = if level == 1 {
            title.to_uppercase()
        } else {
            title.to_string()
        };
        let body = inline(&text, style, self.theme);
        self.wrapped(&[], &[], &body, src);
        if level <= 2 {
            let rule = if level == 1 { '━' } else { '─' };
            self.out.push(Row {
                spans: vec![Span {
                    text: rule.to_string().repeat(self.width),
                    style: self.dim(),
                }],
                src_line: src,
                bg: None,
            });
        }
    }

    fn rule(&mut self, src: usize) {
        self.out.push(Row {
            spans: vec![Span {
                text: "─".repeat(self.width),
                style: self.dim(),
            }],
            src_line: src,
            bg: None,
        });
    }

    /// A fenced code block: the fence lines vanish, the body gets the popup
    /// background and — when the info string names a language crow has a
    /// grammar for — real syntax colors.
    fn code_block(
        &mut self,
        lines: &[String],
        start: usize,
        offset: usize,
        fence: (char, usize),
    ) -> usize {
        let info = lines[start].trim_start()[fence.1..].trim().to_string();
        let mut end = start + 1;
        while end < lines.len() && !closes_fence(lines[end].trim_start(), fence) {
            end += 1;
        }
        let body: Vec<&str> = lines[start + 1..end.min(lines.len())]
            .iter()
            .map(String::as_str)
            .collect();

        let bg = self.theme.popup_bg;
        let lang = info.split_whitespace().next().unwrap_or("");
        let spans = highlight_snippet(lang, &body);

        // A label row carrying the language, then the code, padded a column in
        // from each edge so the tint reads as a block rather than a stripe.
        if !lang.is_empty() {
            self.out.push(Row {
                spans: vec![Span {
                    text: format!(" {lang} "),
                    style: Style {
                        fg: Some(self.theme.gutter),
                        bg: Some(bg),
                        attrs: ITALIC,
                    },
                }],
                src_line: offset + start,
                bg: Some(bg),
            });
        }
        for n in 0..body.len() {
            let mut row = vec![Span {
                text: "  ".to_string(),
                style: Style {
                    fg: None,
                    bg: Some(bg),
                    attrs: 0,
                },
            }];
            for (text, group) in spans.get(n).into_iter().flatten() {
                row.push(Span {
                    text: expand_tabs(text),
                    style: Style {
                        fg: crate::syntax::color(*group).or(self.theme.fg),
                        bg: Some(bg),
                        attrs: 0,
                    },
                });
            }
            self.out.push(Row {
                spans: row,
                src_line: offset + start + 1 + n,
                bg: Some(bg),
            });
        }
        (end + 1).min(lines.len()).max(start + 1)
    }

    /// `> quoted` — a bar in the gutter color and the contents rendered as
    /// their own document, one level narrower.
    fn quote(&mut self, lines: &[String], start: usize, offset: usize) -> usize {
        let mut end = start;
        let mut inner = Vec::new();
        while end < lines.len() {
            let t = lines[end].trim_start();
            let Some(rest) = t.strip_prefix('>') else {
                break; // a lazy continuation line would go here; keep it simple
            };
            inner.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            end += 1;
        }

        let mut nested = Md {
            width: self.width.saturating_sub(2),
            theme: self.theme,
            out: Vec::new(),
        };
        nested.blocks(&inner, offset + start);
        for mut row in nested.out {
            let mut spans = vec![Span {
                text: "▌ ".to_string(),
                style: Style {
                    fg: Some(self.theme.border),
                    bg: None,
                    attrs: 0,
                },
            }];
            for span in &mut row.spans {
                span.style.attrs |= ITALIC;
                if span.style.fg == self.theme.fg {
                    span.style.fg = Some(self.theme.gutter);
                }
            }
            spans.append(&mut row.spans);
            self.out.push(Row {
                spans,
                src_line: row.src_line,
                bg: row.bg,
            });
        }
        end
    }

    /// A run of list items. Bullets are `•`/`◦`/`▪` by depth, ordered items
    /// keep their numbers, and `- [ ]`/`- [x]` become checkboxes.
    fn list(&mut self, lines: &[String], start: usize, offset: usize) -> usize {
        let mut i = start;
        while i < lines.len() {
            let Some((indent, marker, rest)) = list_marker(&lines[i]) else {
                if lines[i].trim().is_empty() {
                    // A blank line ends the list unless another item follows.
                    if lines.get(i + 1).is_some_and(|l| list_marker(l).is_some()) {
                        self.blank(offset + i);
                        i += 1;
                        continue;
                    }
                }
                break;
            };
            // Continuation lines: indented text belonging to this item.
            let mut text = rest.to_string();
            let mut j = i + 1;
            while j < lines.len()
                && list_marker(&lines[j]).is_none()
                && !lines[j].trim().is_empty()
                && lines[j].starts_with(' ')
            {
                text.push(' ');
                text.push_str(lines[j].trim());
                j += 1;
            }

            let depth = indent / 2;
            let (bullet, body) = match check_box(&text) {
                Some((done, rest)) => {
                    (if done { "☑ " } else { "☐ " }.to_string(), rest.to_string())
                }
                None => (
                    match marker {
                        Marker::Ordered(n) => format!("{n}. "),
                        Marker::Bullet => match depth % 3 {
                            0 => "• ".to_string(),
                            1 => "◦ ".to_string(),
                            _ => "▪ ".to_string(),
                        },
                    },
                    text.clone(),
                ),
            };
            let pad = "  ".repeat(depth + 1);
            let marker_style = Style {
                fg: Some(self.theme.border),
                bg: None,
                attrs: 0,
            };
            let lead: Vec<(char, Style)> = pad
                .chars()
                .map(|c| (c, self.base()))
                .chain(bullet.chars().map(|c| (c, marker_style)))
                .collect();
            let cont: Vec<(char, Style)> = std::iter::repeat_n(
                (' ', self.base()),
                pad.chars().count() + bullet.chars().count(),
            )
            .collect();
            let body = inline(&body, self.base(), self.theme);
            self.wrapped(&lead, &cont, &body, offset + i);
            i = j;
        }
        i.max(start + 1)
    }

    /// A pipe table, drawn with real box lines and per-column alignment.
    fn table(&mut self, lines: &[String], start: usize, offset: usize) -> usize {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut i = start;
        while i < lines.len() && lines[i].contains('|') && !lines[i].trim().is_empty() {
            rows.push(split_row(&lines[i]));
            i += 1;
        }
        if rows.len() < 2 {
            return self.paragraph(lines, start, offset);
        }
        let aligns = alignments(&rows.remove(1));
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);

        // Widen each column to its content, then shrink them evenly if the
        // table would overflow the window.
        let mut widths = vec![0usize; cols];
        for row in &rows {
            for (c, cell) in row.iter().enumerate() {
                widths[c] = widths[c].max(display_width(cell).min(40));
            }
        }
        let total: usize = widths.iter().sum::<usize>() + 3 * cols.saturating_sub(1) + 2;
        if total > self.width && cols > 0 {
            let room = self.width.saturating_sub(3 * cols.saturating_sub(1) + 2);
            let share = (room / cols).max(3);
            for w in &mut widths {
                *w = (*w).min(share);
            }
        }

        for (n, row) in rows.iter().enumerate() {
            let mut chars: Vec<(char, Style)> = vec![(' ', self.base())];
            for (c, &cell_w) in widths.iter().enumerate() {
                if c > 0 {
                    chars.extend(" │ ".chars().map(|ch| (ch, self.dim())));
                }
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                let mut style = self.base();
                if n == 0 {
                    style.attrs |= BOLD;
                    style.fg = Some(self.theme.border);
                }
                let mut body = inline(cell, style, self.theme);
                body.truncate(cell_w);
                let padding = cell_w.saturating_sub(body.len());
                let (before, after) = match aligns.get(c) {
                    Some(Align::Right) => (padding, 0),
                    Some(Align::Center) => (padding / 2, padding - padding / 2),
                    _ => (0, padding),
                };
                chars.extend(std::iter::repeat_n((' ', style), before));
                chars.extend(body);
                chars.extend(std::iter::repeat_n((' ', style), after));
            }
            self.out.push(Row {
                spans: coalesce(&chars),
                src_line: offset + start + if n == 0 { 0 } else { n + 1 },
                bg: None,
            });
            if n == 0 {
                let rule: String = widths
                    .iter()
                    .map(|w| "─".repeat(w + 2))
                    .collect::<Vec<_>>()
                    .join("┼");
                self.out.push(Row {
                    spans: vec![Span {
                        text: rule,
                        style: self.dim(),
                    }],
                    src_line: offset + start + 1,
                    bg: None,
                });
            }
        }
        i
    }

    /// A run of non-blank lines: one wrapped paragraph. A line ending in two
    /// spaces is a hard break, and a `===`/`---` underline makes the whole
    /// thing a setext heading instead.
    fn paragraph(&mut self, lines: &[String], start: usize, offset: usize) -> usize {
        let mut i = start;
        let mut chunk: Vec<(char, Style)> = Vec::new();
        while i < lines.len() {
            let line = &lines[i];
            let trimmed = line.trim_start();
            if trimmed.is_empty()
                || (i > start
                    && (fence_of(trimmed).is_some()
                        || heading_of(trimmed).is_some()
                        || trimmed.starts_with('>')
                        || list_marker(line).is_some()))
            {
                break;
            }
            if let Some(level) = setext_of(trimmed) {
                // The underline belongs to the line above, which we have not
                // emitted yet, so re-run it as a heading and stop.
                if i > start {
                    let title: String = lines[i - 1].trim().to_string();
                    chunk.clear();
                    for l in &lines[start..i - 1] {
                        chunk.extend(inline(l.trim(), self.base(), self.theme));
                        chunk.push((' ', self.base()));
                    }
                    if !chunk.is_empty() {
                        self.wrapped(&[], &[], &chunk, offset + start);
                    }
                    self.heading(level, &title, offset + i - 1);
                    return i + 1;
                }
                break;
            }
            if !chunk.is_empty() {
                chunk.push((' ', self.base()));
            }
            chunk.extend(inline(trimmed, self.base(), self.theme));
            if line.ends_with("  ") {
                self.wrapped(&[], &[], &chunk, offset + i);
                chunk.clear();
            }
            i += 1;
        }
        if !chunk.is_empty() {
            self.wrapped(&[], &[], &chunk, offset + start);
        }
        i.max(start + 1)
    }

    // ---- wrapping ----------------------------------------------------------

    /// Emit `body` wrapped to the window, with `lead` before the first row and
    /// `cont` before every row after it (the hanging indent of a list item).
    fn wrapped(
        &mut self,
        lead: &[(char, Style)],
        cont: &[(char, Style)],
        body: &[(char, Style)],
        src_line: usize,
    ) {
        let indent = lead.len().max(cont.len());
        let room = self.width.saturating_sub(indent).max(4);
        let mut first = true;
        for line in wrap(body, room) {
            let mut chars: Vec<(char, Style)> = if first { lead.to_vec() } else { cont.to_vec() };
            chars.extend(line);
            self.out.push(Row {
                spans: coalesce(&chars),
                src_line,
                bg: None,
            });
            first = false;
        }
        if first {
            // Empty body (a bare list bullet): still show the marker.
            self.out.push(Row {
                spans: coalesce(lead),
                src_line,
                bg: None,
            });
        }
    }
}

/// Break styled text into rows of at most `width` columns, at spaces where
/// there is one and mid-word only when a single word is longer than the row.
fn wrap(chars: &[(char, Style)], width: usize) -> Vec<Vec<(char, Style)>> {
    let mut rows = Vec::new();
    let mut row: Vec<(char, Style)> = Vec::new();
    let mut col = 0usize;
    let mut last_space: Option<usize> = None;

    for &(c, style) in chars {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if col + w > width && !row.is_empty() {
            match last_space {
                Some(at) if at > 0 => {
                    let rest: Vec<(char, Style)> = row.split_off(at + 1);
                    row.pop(); // the space itself
                    rows.push(std::mem::take(&mut row));
                    col = rest
                        .iter()
                        .map(|(c, _)| unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0))
                        .sum();
                    row = rest;
                }
                _ => {
                    rows.push(std::mem::take(&mut row));
                    col = 0;
                }
            }
            last_space = None;
        }
        if c == ' ' {
            last_space = Some(row.len());
        }
        row.push((c, style));
        col += w;
    }
    if !row.is_empty() {
        rows.push(row);
    }
    rows
}

/// Merge runs of equally styled chars into spans.
fn coalesce(chars: &[(char, Style)]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for &(c, style) in chars {
        match spans.last_mut() {
            Some(last) if last.style == style => last.text.push(c),
            _ => spans.push(Span {
                text: c.to_string(),
                style,
            }),
        }
    }
    spans
}

// ---- inline ----------------------------------------------------------------

/// Parse inline markup into styled characters, dropping the markup itself.
fn inline(src: &str, base: Style, theme: &Theme) -> Vec<(char, Style)> {
    let c: Vec<char> = src.chars().collect();
    let mut out: Vec<(char, Style)> = Vec::new();
    let mut style = base;
    let code_style = Style {
        fg: theme.syntax[2],
        bg: Some(theme.popup_bg),
        attrs: 0,
    };
    let link_style = Style {
        fg: theme.syntax[7].or(Some(Color::Blue)),
        bg: None,
        attrs: UNDERLINE,
    };
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' if c
                .get(i + 1)
                .is_some_and(|n| "\\`*_{}[]()#+-.!~<>|".contains(*n)) =>
            {
                out.push((c[i + 1], style));
                i += 2;
            }
            '`' => {
                let n = run_len(&c, i, '`');
                match find_run(&c, i + n, '`', n) {
                    Some(end) => {
                        out.push((' ', code_style));
                        out.extend(c[i + n..end].iter().map(|&ch| (ch, code_style)));
                        out.push((' ', code_style));
                        i = end + n;
                    }
                    None => {
                        out.push((c[i], style));
                        i += 1;
                    }
                }
            }
            '~' if run_len(&c, i, '~') >= 2 => match find_run(&c, i + 2, '~', 2) {
                Some(_) => {
                    style.attrs ^= STRIKE;
                    i += 2;
                }
                None if style.attrs & STRIKE != 0 => {
                    style.attrs ^= STRIKE;
                    i += 2;
                }
                None => {
                    out.push((c[i], style));
                    i += 1;
                }
            },
            '*' | '_' => {
                let delim = c[i];
                let n = run_len(&c, i, delim).min(2);
                let bit = if n == 2 { BOLD } else { ITALIC };
                // `_` inside a word is snake_case, not emphasis.
                let intraword = delim == '_'
                    && i > 0
                    && c[i - 1].is_alphanumeric()
                    && c.get(i + n).is_some_and(|n| n.is_alphanumeric());
                if !intraword && (style.attrs & bit != 0 || find_run(&c, i + n, delim, n).is_some())
                {
                    style.attrs ^= bit;
                    i += n;
                } else {
                    out.push((c[i], style));
                    i += 1;
                }
            }
            '!' if c.get(i + 1) == Some(&'[') => match link_at(&c, i + 1) {
                Some((label, end)) => {
                    out.push(('\u{f03e}', link_style)); //  image
                    out.push((' ', link_style));
                    out.extend(label.iter().map(|&ch| (ch, link_style)));
                    i = end;
                }
                None => {
                    out.push((c[i], style));
                    i += 1;
                }
            },
            '[' => match link_at(&c, i) {
                Some((label, end)) => {
                    let mut s = link_style;
                    s.attrs |= style.attrs;
                    out.extend(label.iter().map(|&ch| (ch, s)));
                    i = end;
                }
                None => {
                    out.push((c[i], style));
                    i += 1;
                }
            },
            // <https://…> autolinks; anything else angled is left alone.
            '<' => match c[i..].iter().position(|&ch| ch == '>') {
                Some(rel)
                    if c[i + 1..i + rel].starts_with(&['h', 't', 't', 'p'])
                        || c[i + 1..i + rel].starts_with(&['m', 'a', 'i', 'l']) =>
                {
                    out.extend(c[i + 1..i + rel].iter().map(|&ch| (ch, link_style)));
                    i += rel + 1;
                }
                _ => {
                    out.push((c[i], style));
                    i += 1;
                }
            },
            _ => {
                out.push((c[i], style));
                i += 1;
            }
        }
    }
    out
}

/// `[label](target)` starting at `i`: the label's chars and the index just
/// past the closing paren.
fn link_at(c: &[char], i: usize) -> Option<(&[char], usize)> {
    let close = c[i..].iter().position(|&ch| ch == ']')? + i;
    if c.get(close + 1) != Some(&'(') {
        return None;
    }
    let mut depth = 0usize;
    for (j, &ch) in c.iter().enumerate().skip(close + 1) {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&c[i + 1..close], j + 1));
                }
            }
            _ => {}
        }
    }
    None
}

fn run_len(c: &[char], i: usize, ch: char) -> usize {
    c[i..].iter().take_while(|&&x| x == ch).count()
}

/// The start of the next run of exactly `n` `ch`s at or after `from`.
fn find_run(c: &[char], from: usize, ch: char, n: usize) -> Option<usize> {
    let mut i = from;
    while i < c.len() {
        if c[i] == ch {
            let len = run_len(c, i, ch);
            if len >= n {
                return Some(i);
            }
            i += len;
        } else {
            i += 1;
        }
    }
    None
}

// ---- block recognition -----------------------------------------------------

enum Marker {
    Bullet,
    Ordered(usize),
}

enum Align {
    Left,
    Center,
    Right,
}

/// `#{1,6} Title` -> (level, title).
fn heading_of(trimmed: &str) -> Option<(usize, &str)> {
    let level = trimmed.chars().take_while(|&c| c == '#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = trimmed[level..].strip_prefix(' ')?;
    Some((level, rest.trim_end_matches(['#', ' '])))
}

/// A setext underline: `===` (h1) or `---` (h2).
fn setext_of(trimmed: &str) -> Option<usize> {
    let t = trimmed.trim_end();
    if t.len() >= 2 && t.chars().all(|c| c == '=') {
        Some(1)
    } else if t.len() >= 2 && t.chars().all(|c| c == '-') {
        Some(2)
    } else {
        None
    }
}

/// ``` or ~~~ opening a fence -> (char, fence length).
fn fence_of(trimmed: &str) -> Option<(char, usize)> {
    for ch in ['`', '~'] {
        let n = trimmed.chars().take_while(|&c| c == ch).count();
        if n >= 3 {
            return Some((ch, n));
        }
    }
    None
}

fn closes_fence(trimmed: &str, fence: (char, usize)) -> bool {
    let n = trimmed.chars().take_while(|&c| c == fence.0).count();
    n >= fence.1 && trimmed[n..].trim().is_empty()
}

/// `---`, `***`, `___` on their own — but not a setext underline, which the
/// paragraph handler claims first.
fn is_rule(trimmed: &str) -> bool {
    let t: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    t.len() >= 3 && ['-', '*', '_'].iter().any(|&c| t.chars().all(|x| x == c))
}

/// `- item`, `* item`, `1. item` -> (indent, marker, the rest of the line).
fn list_marker(line: &str) -> Option<(usize, Marker, &str)> {
    let indent = line.len() - line.trim_start().len();
    let t = line.trim_start();
    if let Some(rest) = t.strip_prefix("- ").or(t.strip_prefix("+ ")) {
        return Some((indent, Marker::Bullet, rest));
    }
    // `*` is a bullet only when it isn't a rule or the start of emphasis.
    if let Some(rest) = t.strip_prefix("* ") {
        if !is_rule(t) {
            return Some((indent, Marker::Bullet, rest));
        }
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        if let Some(rest) = t[digits..]
            .strip_prefix(". ")
            .or(t[digits..].strip_prefix(") "))
        {
            return Some((indent, Marker::Ordered(t[..digits].parse().ok()?), rest));
        }
    }
    None
}

/// `[ ] rest` / `[x] rest` -> (checked, rest).
fn check_box(text: &str) -> Option<(bool, &str)> {
    let rest = text.strip_prefix("[ ] ").map(|r| (false, r));
    rest.or_else(|| {
        text.strip_prefix("[x] ")
            .or(text.strip_prefix("[X] "))
            .map(|r| (true, r))
    })
}

/// A pipe table starts where a row of cells is followed by a `|---|---|`.
fn is_table(lines: &[String], i: usize) -> bool {
    if !lines[i].contains('|') {
        return false;
    }
    lines.get(i + 1).is_some_and(|next| {
        next.contains('-')
            && next.contains('|')
            && next
                .chars()
                .all(|c| matches!(c, '-' | '|' | ':' | ' ' | '\t'))
    })
}

fn split_row(line: &str) -> Vec<String> {
    let t = line.trim().trim_start_matches('|').trim_end_matches('|');
    t.split('|').map(|c| c.trim().to_string()).collect()
}

fn alignments(delims: &[String]) -> Vec<Align> {
    delims
        .iter()
        .map(|d| match (d.starts_with(':'), d.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        })
        .collect()
}

fn display_width(s: &str) -> usize {
    s.chars()
        .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
        .sum()
}

fn expand_tabs(s: &str) -> String {
    s.replace('\t', &" ".repeat(crate::config::tab_width()))
}

/// Syntax-color a code fence's body, as one `(text, group)` list per line.
/// An unknown or missing language leaves it plain.
fn highlight_snippet(lang: &str, body: &[&str]) -> Vec<Vec<(String, u8)>> {
    let plain = || {
        body.iter()
            .map(|l| vec![(l.to_string(), 0u8)])
            .collect::<Vec<_>>()
    };
    let Some(config) = crate::syntax::config_for_lang(lang) else {
        return plain();
    };
    let src = Rope::from_str(&(body.join("\n") + "\n"));
    let Some(syntax) = crate::syntax::parse(config, &src) else {
        return plain();
    };
    (0..body.len())
        .map(|n| {
            let start = src.line_to_char(n);
            let line: Vec<char> = body[n].chars().collect();
            let mut runs: Vec<(String, u8)> = Vec::new();
            for (i, ch) in line.iter().enumerate() {
                let group = crate::syntax::group_at(&syntax.spans, start + i);
                match runs.last_mut() {
                    Some((text, g)) if *g == group => text.push(*ch),
                    _ => runs.push((ch.to_string(), group)),
                }
            }
            runs
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| r.spans.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    fn render_str(src: &str, width: usize) -> Vec<Row> {
        render(&Rope::from_str(src), width)
    }

    #[test]
    fn markup_is_consumed_not_shown() {
        let rows = render_str("Some **bold** and *em* and `code` here.\n", 60);
        let line = &text_of(&rows)[0];
        assert!(!line.contains('*'), "asterisks survived: {line}");
        assert!(line.contains("bold") && line.contains("em"));
        // The code span keeps its text and gains a background.
        assert!(rows[0]
            .spans
            .iter()
            .any(|s| s.text.contains("code") && s.style.bg.is_some()));
        assert!(rows[0].spans.iter().any(|s| s.style.attrs & BOLD != 0));
        assert!(rows[0].spans.iter().any(|s| s.style.attrs & ITALIC != 0));
    }

    #[test]
    fn headings_lose_their_hashes_and_h1_h2_get_a_rule() {
        let rows = render_str("# Title\n\n## Sub\n\n### Small\n", 40);
        let lines = text_of(&rows);
        assert_eq!(lines[0], "TITLE");
        assert!(lines[1].starts_with('━'));
        assert_eq!(lines[3], "Sub");
        assert!(lines[4].starts_with('─'));
        assert_eq!(lines[6], "Small");
        assert!(
            lines.get(7).is_none_or(|l| l.is_empty()),
            "an h3 gets no rule under it"
        );
    }

    #[test]
    fn lists_get_bullets_and_checkboxes() {
        let rows = render_str("- one\n- two\n  - nested\n- [ ] todo\n- [x] done\n", 40);
        let lines = text_of(&rows);
        assert_eq!(lines[0], "  • one");
        assert_eq!(lines[2], "    ◦ nested");
        assert!(lines[3].contains("☐ todo"));
        assert!(lines[4].contains("☑ done"));
    }

    #[test]
    fn ordered_lists_keep_their_numbers() {
        let lines = text_of(&render_str("1. first\n2. second\n", 40));
        assert_eq!(lines[0], "  1. first");
        assert_eq!(lines[1], "  2. second");
    }

    #[test]
    fn a_fence_becomes_a_tinted_block_without_its_backticks() {
        let rows = render_str("```rust\nfn main() {}\n```\n", 40);
        let lines = text_of(&rows);
        assert!(!lines.iter().any(|l| l.contains("```")));
        assert!(lines.iter().any(|l| l.contains("fn main() {}")));
        assert!(rows
            .iter()
            .filter(|r| !r.spans.is_empty())
            .all(|r| r.bg.is_some()));
        // Known language: the keyword picked up a syntax color.
        assert!(rows
            .iter()
            .flat_map(|r| &r.spans)
            .any(|s| s.text.trim() == "fn" && s.style.fg.is_some()));
    }

    #[test]
    fn links_show_their_label_not_their_url() {
        let line = &text_of(&render_str("see [the docs](https://example.com) ok\n", 60))[0];
        assert_eq!(line, "see the docs ok");
    }

    #[test]
    fn quotes_get_a_bar() {
        let lines = text_of(&render_str("> quoted words\n", 40));
        assert!(lines[0].starts_with("▌ "));
        assert!(lines[0].contains("quoted words"));
    }

    #[test]
    fn tables_are_drawn_with_columns() {
        let lines = text_of(&render_str("| a | bbb |\n|---|-----|\n| 1 | 2 |\n", 40));
        assert!(lines[0].contains('│'), "header has a column rule");
        assert!(lines[1].contains('┼'), "separator row");
        assert!(lines[2].contains('1') && lines[2].contains('2'));
        assert!(!lines.iter().any(|l| l.contains('|')), "pipes consumed");
    }

    #[test]
    fn paragraphs_wrap_to_the_window() {
        let rows = render_str(&"word ".repeat(30), 20);
        assert!(rows.len() > 1);
        for row in &rows {
            let w: usize = row.spans.iter().map(|s| display_width(&s.text)).sum();
            assert!(w <= 20, "row overflowed: {w}");
        }
    }

    #[test]
    fn rows_carry_their_source_line_for_scroll_sync() {
        let rows = render_str("# One\n\npara\n\n## Two\n", 40);
        // The `## Two` heading reports line 4, not row 4.
        let two = rows
            .iter()
            .find(|r| r.spans.iter().any(|s| s.text.contains("Two")));
        assert_eq!(two.map(|r| r.src_line), Some(4));
    }

    #[test]
    fn snake_case_is_not_emphasis() {
        let line = &text_of(&render_str("call some_long_name here\n", 40))[0];
        assert_eq!(line, "call some_long_name here");
    }

    #[test]
    fn a_rule_is_a_rule_and_a_setext_underline_is_a_heading() {
        let lines = text_of(&render_str("---\n", 10));
        assert_eq!(lines[0], "──────────");
        let lines = text_of(&render_str("Title\n---\n", 10));
        assert_eq!(lines[0], "Title");
        assert!(lines[1].starts_with('─'));
    }

    /// An unterminated fence or a table with nothing after it must not spin.
    #[test]
    fn malformed_input_terminates() {
        render_str("```\nno close\n", 40);
        render_str("| a |\n", 40);
        render_str("> \n", 40);
        render_str("- \n", 40);
        render_str("", 40);
    }
}
