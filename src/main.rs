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

use std::io::{stdout, BufWriter, Write};
use std::panic;
use std::path::PathBuf;
use std::time::Duration;

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

    // Give the terminal back first: killing and reaping the language servers
    // is the last thing `:q` does, and nobody should watch it happen.
    restore_terminal()?;
    editor.shutdown_lsps();
    result
}

/// How many queued events one iteration will absorb before it has to draw.
/// Key repeat can outrun the renderer indefinitely; without a ceiling the
/// screen would never update while a key is held.
const MAX_BURST: usize = 512;

/// How long typing has to pause before the buffer is recolored. Short enough
/// that a keyword lights up as you finish the word, long enough that a full
/// reparse never lands between two keystrokes.
const REPARSE_GAP: Duration = Duration::from_millis(20);

/// How long an idle loop blocks waiting for input. The ceiling on how late a
/// language-server message can show up.
const IDLE_POLL: Duration = Duration::from_millis(100);

fn run(editor: &mut Editor) -> std::io::Result<()> {
    // A frame is tens of kilobytes of escape sequences. Bare `stdout()` is
    // line-buffered, so it turns that into a syscall every kilobyte or so;
    // one big buffer makes it a single write.
    let mut out = BufWriter::with_capacity(1 << 20, stdout());
    // Repaint only when state actually changed; redrawing the whole screen
    // on every idle poll tick is what made the cursor and text flicker.
    let mut dirty = true;

    loop {
        // Drain everything the terminal already has before drawing anything.
        // Key repeat and fast typing arrive as a burst, and rendering (plus
        // reparsing, plus syncing the language server) once per key in that
        // burst is what makes the editor lag a whole word behind the keyboard.
        let mut burst = 0;
        while burst < MAX_BURST && event::poll(Duration::ZERO)? {
            match event::read()? {
                Event::Key(ev) => {
                    if let Some(key) = Key::from_crossterm(ev) {
                        editor.handle_key(key);
                        dirty = true;
                        burst += 1;
                    }
                }
                Event::Paste(text) => {
                    editor.handle_paste(&text);
                    dirty = true;
                    burst += 1;
                }
                Event::Resize(cols, rows) => {
                    editor.size = (cols, rows);
                    dirty = true;
                    burst += 1;
                }
                _ => {}
            }
            if editor.should_quit {
                break;
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

        // Recolor in the gaps between keystrokes, never in the middle of one:
        // a reparse is a whole-file tree-sitter pass, and edits carry the old
        // spans along so the frames before it are drawn correctly anyway.
        let reparse_due = editor.needs_reparse();
        if reparse_due && editor.idle_for(REPARSE_GAP) {
            editor.settle();
            dirty = true;
        }

        if dirty {
            editor.ensure_cursor_visible();
            ui::render(editor, &mut out)?;
            dirty = false;
        }

        // Up to date and nothing queued: wait for input instead of spinning,
        // but wake up regularly so language-server messages arriving while
        // idle still get drained and drawn — and sooner when a recolor is owed.
        let wait = if reparse_due { REPARSE_GAP } else { IDLE_POLL };
        event::poll(wait)?;
    }

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
