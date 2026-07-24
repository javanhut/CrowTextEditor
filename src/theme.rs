//! Themes: named palettes for syntax groups and UI chrome.
//!
//! A theme is data — the config names one, `:theme <name>` switches live.
//! Adding a theme is adding one entry to `THEMES`.

use std::sync::atomic::{AtomicUsize, Ordering};

use crossterm::style::Color;

pub struct Theme {
    pub name: &'static str,
    /// Syntax colors indexed by highlight group id (see `syntax::group_of`);
    /// index 0 is unused (plain text).
    pub syntax: [Option<Color>; 8],
    /// Bitmasks over the same group ids: bit N set = group N renders bold /
    /// italic (on top of markdown's own strong/emphasis attributes).
    pub bold: u8,
    pub italic: u8,
    /// Editor background; `None` keeps the terminal's own.
    pub bg: Option<Color>,
    /// Default text color; `None` keeps the terminal's own.
    pub fg: Option<Color>,
    /// Selection background.
    pub selection: Color,
    /// Line numbers.
    pub gutter: Color,
    /// The cursor line's number.
    pub gutter_cursor: Color,
    /// Background of popups (pickers, completion menus, prompt bar).
    pub popup_bg: Color,
    /// Popup borders and accents.
    pub border: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb { r, g, b }
}

pub static THEMES: &[Theme] = &[
    Theme {
        name: "default",
        syntax: [
            None,
            Some(Color::DarkGrey),   // comment
            Some(Color::Green),      // string
            Some(Color::Magenta),    // keyword
            Some(Color::Cyan),       // function
            Some(Color::Yellow),     // type
            Some(Color::DarkYellow), // constant, number
            Some(Color::Blue),       // macro, attribute
        ],
        bold: 0,
        italic: 0,
        bg: None,
        fg: None,
        selection: Color::DarkGrey,
        gutter: Color::DarkGrey,
        gutter_cursor: Color::Yellow,
        popup_bg: Color::DarkGrey,
        border: Color::Yellow,
    },
    Theme {
        name: "tokyonight",
        syntax: [
            None,
            Some(rgb(0x56, 0x5f, 0x89)), // comment
            Some(rgb(0x9e, 0xce, 0x6a)), // string
            Some(rgb(0xbb, 0x9a, 0xf7)), // keyword
            Some(rgb(0x7a, 0xa2, 0xf7)), // function
            Some(rgb(0x2a, 0xc3, 0xde)), // type
            Some(rgb(0xff, 0x9e, 0x64)), // constant, number
            Some(rgb(0xe0, 0xaf, 0x68)), // macro, attribute
        ],
        bold: 0,
        italic: 0b0000_0010, // comments
        bg: Some(rgb(0x1a, 0x1b, 0x26)),
        fg: Some(rgb(0xc0, 0xca, 0xf5)),
        selection: rgb(0x28, 0x34, 0x57),
        gutter: rgb(0x3b, 0x42, 0x61),
        gutter_cursor: rgb(0xe0, 0xaf, 0x68),
        popup_bg: rgb(0x24, 0x28, 0x3b),
        border: rgb(0xff, 0x9e, 0x64),
    },
    Theme {
        name: "gruvbox",
        syntax: [
            None,
            Some(rgb(0x92, 0x83, 0x74)), // comment
            Some(rgb(0xb8, 0xbb, 0x26)), // string
            Some(rgb(0xfb, 0x49, 0x34)), // keyword
            Some(rgb(0x83, 0xa5, 0x98)), // function
            Some(rgb(0xfa, 0xbd, 0x2f)), // type
            Some(rgb(0xd3, 0x86, 0x9b)), // constant
            Some(rgb(0x8e, 0xc0, 0x7c)), // macro
        ],
        bold: 0,
        italic: 0b0000_0010, // comments
        bg: Some(rgb(0x28, 0x28, 0x28)),
        fg: Some(rgb(0xeb, 0xdb, 0xb2)),
        selection: rgb(0x50, 0x49, 0x45),
        gutter: rgb(0x7c, 0x6f, 0x64),
        gutter_cursor: rgb(0xfa, 0xbd, 0x2f),
        popup_bg: rgb(0x3c, 0x38, 0x36),
        border: rgb(0xfa, 0xbd, 0x2f),
    },
    Theme {
        name: "mono",
        syntax: [None; 8],
        bold: 0,
        italic: 0,
        bg: None,
        fg: None,
        selection: Color::DarkGrey,
        gutter: Color::DarkGrey,
        gutter_cursor: Color::White,
        popup_bg: Color::DarkGrey,
        border: Color::White,
    },
];

static CURRENT: AtomicUsize = AtomicUsize::new(0);

pub fn current() -> &'static Theme {
    &THEMES[CURRENT.load(Ordering::Relaxed).min(THEMES.len() - 1)]
}

/// Switch theme by name. Returns false (and keeps the old one) if unknown.
pub fn set(name: &str) -> bool {
    match THEMES.iter().position(|t| t.name == name) {
        Some(i) => {
            CURRENT.store(i, Ordering::Relaxed);
            true
        }
        None => false,
    }
}

pub fn names() -> String {
    THEMES.iter().map(|t| t.name).collect::<Vec<_>>().join(", ")
}

/// Tests that touch the global theme take this lock, so parallel test
/// threads don't race on `CURRENT`.
#[cfg(test)]
pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_switches_and_rejects_unknown() {
        let _guard = TEST_LOCK.lock().unwrap();
        assert!(set("gruvbox"));
        assert_eq!(current().name, "gruvbox");
        assert!(!set("nope"));
        assert_eq!(current().name, "gruvbox");
        assert!(set("default"));
    }
}
