mod commands;
mod config;
mod deps;
mod document;
mod editor;
mod filetree;
mod keymap;
mod lsp;
mod picker;
mod position;
mod search;
mod syntax;
mod theme;
mod transaction;
mod ui;

use std::io::{stdout, Write};
use std::panic;
use std::path::PathBuf;

use crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};

use editor::Editor;
use keymap::Key;

const HELP: &str = "\
crow — a selection-first modal terminal text editor

USAGE:
    crow [FILE]...

KEYS (normal mode):
    h j k l      move            i a I A o O   insert
    w b e        select word     d c y p P     delete/change/yank/paste
    dd           delete line (into the register, so p pastes it)
    x v ;        select line, extend mode,     collapse selection
    C A-C  ,     add cursor below/above, drop  extra cursors
    A-o          expand selection to syntax node
    / n N        regex search    s  \"x        select all matches, register
    C-w v/s/w/q  split side-by-side/stacked,   cycle, close window
    0 ^ $        line ends       u  C-r        undo, redo
    gg G  42gg   file ends, jump to line       :w :q :wq  write, quit
    C-d C-u      half page       gn gp         next/prev buffer
    gd K         goto definition, hover        C-space  LSP complete (insert)
    space e      file tree sidebar             (typing pops word completion)
    space c/f/d/t  command palette, find file, browse dir, themes

    Motions select the text they cross; d/c/y act on the selection.
    Every motion, edit, and inserted keystroke applies at every cursor.
    A count may prefix most commands: 3x, 10d, 5C.

CONFIG:
    ~/.config/crow/crow.toml — theme, options, keybindings, language
    servers. Created with comments on first run; :config opens it and
    :config! reloads it without a restart (an edited [lsp] section
    restarts your language servers). :theme <name> switches themes live.
";

fn main() -> std::io::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP}");
        return Ok(());
    }

    let paths: Vec<PathBuf> = args.into_iter().map(PathBuf::from).collect();
    let cfg = config::load();
    config::apply(&cfg);
    let size = crossterm::terminal::size()?;
    let mut editor = Editor::new(paths, size, &cfg)?;

    install_panic_hook();
    setup_terminal()?;

    let result = run(&mut editor);

    restore_terminal()?;
    result
}

fn run(editor: &mut Editor) -> std::io::Result<()> {
    let mut out = stdout();
    // Repaint only when state actually changed; redrawing the whole screen
    // on every idle poll tick is what made the cursor and text flicker.
    let mut dirty = true;

    loop {
        if dirty {
            editor.ensure_cursor_visible();
            ui::render(editor, &mut out)?;
            dirty = false;
        }

        // Poll instead of block, so language-server messages arriving while
        // idle still get drained and drawn.
        if event::poll(std::time::Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(ev) => {
                    if let Some(key) = Key::from_crossterm(ev) {
                        editor.handle_key(key);
                        dirty = true;
                    }
                }
                Event::Paste(text) => {
                    editor.handle_paste(&text);
                    dirty = true;
                }
                Event::Resize(cols, rows) => {
                    editor.size = (cols, rows);
                    dirty = true;
                }
                _ => {}
            }
        }
        if editor.lsp_tick() {
            dirty = true;
        }
        if editor.install_tick() {
            dirty = true;
        }
        if editor.deps_tick() {
            dirty = true;
        }

        if editor.should_quit {
            break;
        }
    }

    editor.shutdown_lsps();
    Ok(())
}

fn setup_terminal() -> std::io::Result<()> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        cursor::Hide
    )
}

fn restore_terminal() -> std::io::Result<()> {
    let mut out = stdout();
    let _ = execute!(
        out,
        cursor::SetCursorStyle::DefaultUserShape,
        cursor::Show,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = out.flush();
    disable_raw_mode()
}

/// A panic in raw mode leaves the terminal unusable — no echo, no line
/// discipline, and the panic message smeared across the alternate screen. Put
/// the terminal back before the default hook prints anything.
fn install_panic_hook() {
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        default_hook(info);
    }));
}
