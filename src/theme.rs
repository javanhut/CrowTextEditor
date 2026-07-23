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
    /// Selection background.
    pub selection: Color,
    /// Line numbers.
    pub gutter: Color,
    /// The cursor line's number.
    pub gutter_cursor: Color,
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
        selection: Color::DarkGrey,
        gutter: Color::DarkGrey,
        gutter_cursor: Color::Yellow,
    },
    Theme {
        name: "gruvbox",
        syntax: [
            None,
            Some(Color::Rgb { r: 0x92, g: 0x83, b: 0x74 }), // comment
            Some(Color::Rgb { r: 0xb8, g: 0xbb, b: 0x26 }), // string
            Some(Color::Rgb { r: 0xfb, g: 0x49, b: 0x34 }), // keyword
            Some(Color::Rgb { r: 0x83, g: 0xa5, b: 0x98 }), // function
            Some(Color::Rgb { r: 0xfa, g: 0xbd, b: 0x2f }), // type
            Some(Color::Rgb { r: 0xd3, g: 0x86, b: 0x9b }), // constant
            Some(Color::Rgb { r: 0x8e, g: 0xc0, b: 0x7c }), // macro
        ],
        selection: Color::Rgb { r: 0x50, g: 0x49, b: 0x45 },
        gutter: Color::Rgb { r: 0x7c, g: 0x6f, b: 0x64 },
        gutter_cursor: Color::Rgb { r: 0xfa, g: 0xbd, b: 0x2f },
    },
    Theme {
        name: "mono",
        syntax: [None; 8],
        selection: Color::DarkGrey,
        gutter: Color::DarkGrey,
        gutter_cursor: Color::White,
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
    THEMES
        .iter()
        .map(|t| t.name)
        .collect::<Vec<_>>()
        .join(", ")
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
