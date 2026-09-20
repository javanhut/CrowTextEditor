# Crow - My Ideal Text Editor

A selection-first modal terminal text editor. One cursor is a crow; many are a
murder. It's a very opinionated text editor similar to a custom neovim config without
configuration.

```
./install.sh          # builds and installs to ~/.local/bin
crow src/main.rs      # or: cargo run --release -- src/main.rs
```

## What works

Selection-first modal editing — the Helix/Kakoune model, not the vim model.
Motions select the text they cross, `x` selects lines, and `d`/`c`/`y` act on
the selection, so what an edit will touch is on screen before you commit to it.
There is no operator-pending state.

Multiple cursors are native, not a plugin: `C-v` starts a block — `j`/`k`/`h`/`l`
and the other motions stretch a rectangle with one selection per line, and
whatever comes next (`i`/`a` at its left/right edge, `I`/`A` at each line's
start/end, `d`, `c`, `gc`) runs on all of them — `C` copies the cursor to the next
line, and then every motion, selection, edit, and keystroke of insert-mode
typing applies at every cursor. A multi-cursor edit is a single undo step, and
a multi-cursor delete or copy captures every selection into the register.

Repeat and macros. `.` runs the last change again where the cursor is now —
and because a selection made from the cursor (a word, a line, a text object)
is kept with the change that acted on it, `mi( d .` deletes the inside of the
parentheses you are in now, while `n .` walks the search matches deleting
each. `q` plus a register records a macro, `q` stops, `@a` plays it (`3@a`
three times, `@@` the last one). Both replay the keys through the same
dispatch that ran them, so anything you can type, you can repeat.

Text objects: `mi` and `ma` select inside or around `(` `[` `{` `<` `"` `'`
`` ` ``, `w` a word, `W` a WORD, `p` a paragraph — and, from the syntax tree,
`f` a function, `t` a type, `a` an argument, `c` a comment. Pressing the same
object again grows to the one enclosing it. `md(` deletes the surrounding
pair and `mr([` swaps it for another; both work at every cursor at once.

Search and multi-cursor are the same feature. `/` searches incrementally by
regex and the match _is_ a selection, so `d`, `c`, and `y` compose with it;
`n`/`N` walk the matches. `s` puts a selection on **every** match — inside the
current selection if there is one, else the whole buffer — so interactive
replace-all is just `s foo ⏎ c bar ⎋`, with every edit site visible before you
type, and `s \d+ ⏎` puts a cursor on every number. A pattern that doesn't
compile (yet) is searched literally, so the preview never breaks mid-keystroke.

Syntax is a selection too. Tree-sitter parses Rust, TOML, JSON, Python,
shell, and JavaScript (each further language is one grammar dependency and
one match arm), colors them, and
`A-o` grows the selection to the enclosing syntax node — token, expression,
statement, block, function — one keypress per level. Since it's just a
selection, `d`/`c`/`y`, multi-cursor, and `s` all compose with it.

Oxigen (`.oxi`) has no published grammar, so it is colored by a lexer built
into `syntax.rs` instead — keywords, `<type>` annotations, strings, comments,
`#[indent]` directives, calls. Same colors, minus `A-o`, which needs a tree.
`oxigen-lsp` and `oxigen fmt` are wired up by default like every other
language; `:install oxigen-lsp` builds them from the language's own repo.

Selections are a set you can work on as a set: `A-s` splits them into one per
line, `A-S` splits them on a regex, `A-k` keeps only the ones matching a
pattern and `A-K` drops those, `&` lines them up in a column, `_` trims their
whitespace, and `)` / `(` walk which one is primary.

Markdown renders, it doesn't just get highlighted. `:md` (or `space m`) opens
a preview beside the buffer that reads the way GitHub does: `**bold**` is
bold with no asterisks, headings get their rules, fences become tinted
syntax-colored blocks, lists get bullets and checkboxes, tables are drawn
with real columns and alignment, block quotes get a bar, and links show
their label instead of their URL. It re-renders as you type and scrolls in
step with the source, so the two panes always look at the same paragraph.
Focus never moves into it — you edit the markdown, you read the render.

Editing is incremental, and so is the parser. Tree-sitter is told what the
edit was and reparses only that, and the highlight query runs over the lines
on screen rather than the whole file — 194µs per keystroke on a 16,000-line
file, against 65ms for the full reparse it used to do. Input events are
drained in a burst before drawing, so holding a key or pasting costs one
frame instead of one per keystroke, and the frame leaves as a single
buffered write.

Long lines soft-wrap by default, at word boundaries, with the gutter left
blank on continuation rows — a paragraph-per-line markdown file reads as
prose instead of scrolling sideways. `:wrap` turns it off. Trailing
whitespace is tinted (except on the line you are typing) and cut on write,
never in markdown, where two trailing spaces are a line break.

Insert-mode niceties: brackets and quotes auto-close (type `(` and `)`
appears, retype the closer to step over it, backspace eats an empty pair;
quotes stay single after word characters so `don't` types naturally), and the
file tree shows Nerd Font icons per file type. Both are options in crow.toml
(`autoclose`, `icons`); the installer offers JetBrains Mono Nerd Font on
macOS.

Jumps are remembered. Anything that moves further than a motion — `gd`, a
picker, `gg`/`G`, a search, `:42`, a diagnostic or a change — leaves where it
came from in the jumplist, and `C-o` / `C-i` walk back and forward through it,
across files.

The mouse works, if you want it. Click to put the cursor there (and focus that
window), drag to select, wheel to scroll, click the file tree to open a file,
click the shell to focus it. `mouse = false` in crow.toml gives the terminal
its own selection back.

Change markers come from ivaldi, not git. `ivaldi whodidit` gives the file as
the last seal has it; crow diffs the buffer against that and marks the gutter —
green for added lines, blue for changed, red where sealed lines were removed —
with `]g` / `[g` jumping between the changes and the status line counting them
(`+12 ~3 -1`). The lookup runs in a background thread and the diff runs when
typing pauses, so neither is ever in the way of a keystroke.

Your work survives the machine. Unsaved buffers are written aside every couple
of seconds, and the next time you open that file crow says so: `:recover`
brings the text back as an undoable edit, `:recover!` throws the swap file
away. Undo history is saved on write and loaded when you reopen the file, so
`u` still reaches yesterday's edits — but only when the file on disk is still
the text the history was recorded against. Both are `crow.toml` options.

Files that change underneath you are noticed. An unmodified buffer whose file
changed on disk reloads itself as one undoable edit that leaves your cursor
where it was; a modified one says so once, and `:e!` takes their version while
`:w!` keeps yours.

LSP without an async runtime: the server runs as a child process, a thread
feeds its messages into a channel, and the main loop drains it between
keystrokes — the editor never blocks on the server. Diagnostics color the
gutter (red errors, yellow warnings) and the message for the cursor line shows
below the status bar; `gd` jumps to a definition (opening the file if needed);
`K` shows hover info. Completions come from the server as you type an
identifier, on the trigger characters it advertises (`.` for members, `<` for
Oxigen's type annotations), and on demand with `C-space`. rust-analyzer is
wired up by default; any server is one config line.

The rest of the protocol is wired up too: `gr` lists every reference in a
picker, `space a` offers the code actions and quick fixes for the selection
(applying whatever edits and commands they come back with), `space R` renames
the symbol across every file that uses it, `space s s` picks a symbol in this
buffer and `space S` searches them across the project as you type, `]d` / `[d`
walk the diagnostics and `space x` lists them all, a diagnostic shows its first
suggestion beside it and `gl` opens it in full (for Rust, the compiler's own
output with its `help:` rewrites), signature help pops up as
you type a call's arguments with the parameter you are on picked out, and
`:fmt` falls back to the server when crow knows no formatter for the file.
Checks a server only runs on save — rust-analyzer's `cargo check`, where the
borrow checker lives — need a `:w`; set `autosave = 1000` in crow.toml and crow
writes the buffer a second after you stop typing instead (as typed: no
formatting, no whitespace strip, and never over a file changed on disk).
Edits arrive incrementally: the transaction log becomes LSP change events, so
a keystroke in a 16,000-line file sends a few bytes rather than the buffer.

Configuration is a data file, not a program. `~/.config/crow/crow.toml` —
created with comments on first run, opened with `:config` — declares
everything, NvCrow-style: names in, wiring out.

```toml
theme = "gruvbox"            # default | gruvbox | mono; :theme switches live

[options]
tab_width = 4
scrolloff = 3
soft_wrap = true             # wrap long lines instead of scrolling sideways
strip_trailing_whitespace = true
shell = "zsh"                # what space t runs; default $SHELL

[lsp]                        # file extension = server command
rs = "rust-analyzer"
py = "pyright-langserver --stdio"

[keys.normal]                # any :command name is bindable
"C-p" = "search"
gq = "quit"
```

A shell lives in a split, like vim's `:terminal`. `space t` (or `:term`)
opens one below the buffer and puts you in it; the status bar says `TERM`
and every key goes to the shell. It is a real pseudo-terminal running your
`$SHELL`, with colors, the alternate screen, and scrollback, so `git diff`,
`cargo test`, `htop`, `less`, and a nested editor all behave. `C-\ C-n` (or
`C-w N`) drops to normal mode in the same window, where `j`/`k`/`C-u`/`C-d`
scroll the history and `i` goes back to the shell; `C-w` plus a window key
works straight from the shell (`C-w j` for the buffer above, `C-w :` for a
command, `C-w .` sends a literal `C-w`). `space t` from the terminal hides it
without killing the shell; the next `space t` brings it back, history and
running job intact. `exit` closes it. Output arrives as it happens: the
shell's reads share the wake channel keys come in on, so the main loop
sleeps on both and never polls.

Plus splits (`C-w v/s/w/q`) with independent cursors per window, counts (`3x`,
`10d`, `5C`), multi-key bindings (`gg`), transaction-based undo/redo with
sensible grouping, multiple buffers, ex commands, vertical and horizontal
scrolling, and grapheme-aware cursor movement — the cursor never lands inside
an emoji ZWJ sequence or a combining stack.

|                                    |                                                                                                                                                                                                                                             |
| ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `h` `j` `k` `l`                    | move, collapsing the selection (arrows work too); `h`/`l` wrap to the previous/next line                                                                                                                                                    |
| `%`                                | jump to the bracket matching the one under the cursor; the match also glows whenever the cursor sits on a bracket                                                                                                                           |
| `w` `b` `e`                        | select to next word / previous word / word end                                                                                                                                                                                              |
| `V`                                | select (highlight) the line; repeat to extend                                                                                                                                                                                               |
| `v`                                | select the character under the cursor, then grow or shrink the selection with motions (status shows `SELECT`)                                                                                                                               |
| `;`                                | collapse the selection to the cursor                                                                                                                                                                                                        |
| `mi` `ma` _(then an object)_       | select inside / around: `(` `[` `{` `<` `"` `'` `` ` ``, `w` word, `W` WORD, `p` paragraph, `f` function, `t` type, `a` argument, `c` comment; again grows outwards |
| `md` `mr` _(then a pair)_          | delete / replace the surrounding pair — `md(`, `mr"'` |
| `.`                                | repeat the last change where the cursor is now (`3.` three times) |
| `q{reg}` … `q`  `@{reg}`           | record a macro into a register, stop; play it (`@@` replays the last, `3@a` three times) |
| `A-s` `A-S`                        | split the selections into lines / on a regex |
| `A-k` `A-K`                        | keep / drop the selections matching a regex |
| `&` `_`  `(` `)`                   | align the selections, trim their whitespace, walk which is primary |
| `A-o`                              | expand the selection to the enclosing syntax node                                                                                                                                                                                           |
| `C-v`                              | block mode: motions stretch a rectangle, one selection per line; the next edit runs on every line (`C-v` again keeps the cursors, `Esc` drops them) |
| `C` `A-C`                          | add a cursor on the next / previous line                                                                                                                                                                                                    |
| `,`                                | drop the extra cursors (`Esc` in normal mode too)                                                                                                                                                                                           |
| `/` `n` `N`                        | incremental regex search; next / previous match                                                                                                                                                                                             |
| `s`                                | select every match within the selection (or buffer)                                                                                                                                                                                         |
| `"x`                               | use register `x` for the next cut/copy/paste                                                                                                                                                                                                |
| `d` `x` `c`                        | cut / cut / copy the selection; `S` changes it                                                                                                                                                                                              |
| `dd` `xx` `cc`                     | cut / cut / copy the whole line, no selection step (`3dd` for three)                                                                                                                                                                        |
| _(any cut or copy)_                | also goes to the system clipboard, so `Cmd-V` pastes it into other apps                                                                                                                                                                     |
| `p` `P`                            | paste after / before (linewise if the copy was)                                                                                                                                                                                             |
| `0` `^` `$`                        | line start / first non-blank / line end                                                                                                                                                                                                     |
| `gg` `G`                           | file start / end; `42gg` or `42G` jumps to line 42 (also `:42`)                                                                                                                                                                             |
| `C-d` `C-u` `C-f` `C-b`            | half page / full page                                                                                                                                                                                                                       |
| `C-w v` `C-w s` `C-w w` `C-w q`    | split side-by-side / stacked, cycle, close                                                                                                                                                                                                  |
| `i` `I` `a` `A` `o` `O`            | enter insert mode                                                                                                                                                                                                                           |
| `D` `J`                            | delete to line end, join                                                                                                                                                                                                                    |
| `u` `C-r`                          | undo / redo                                                                                                                                                                                                                                 |
| `gn` `gp`                          | next / previous buffer                                                                                                                                                                                                                      |
| `gd` `gr` `K`                      | goto definition / list references / hover (LSP) |
| `C-o` `C-i`                        | jump back / forward through the jumplist (Tab works as `C-i`) |
| `]d` `[d`  `]g` `[g`               | next / previous diagnostic, next / previous change since the last seal |
| `space a` `space R`                | code actions for the selection, rename the symbol everywhere (LSP) |
| `space s s` `space S` `space x`    | symbols in this buffer, symbols across the project, every diagnostic |
| _(the mouse)_                      | click to place the cursor, drag to select, wheel to scroll, click the tree or the shell to focus it |                                                                                                                                                                                                               |
| `gc`                               | comment or uncomment the selected lines                                                                                                                                                                                                     |
| `ms` _(then a character)_          | surround the selection with it — `ms(`, `ms"`, `ms*`                                                                                                                                                                                       |
| `space m` `:md`                    | live markdown preview beside the buffer                                                                                                                                                                                                     |
| `:wrap`                            | soft wrap on / off                                                                                                                                                                                                                          |
| _(typing)_                         | intellisense pops itself from buffer words, then from the language server when one is up — Tab accepts, Enter stays Enter                                                                                                                   |
| `C-space` (insert)                 | LSP completion menu — Tab/Enter accepts, type to narrow                                                                                                                                                                                     |
| `space e`                          | file tree sidebar — same key focuses and closes; `j`/`k` move, Enter/`l` expand or open, `h` collapse, `a` add (trailing `/` = dir), `r` rename, `d` delete (y/n), `x`/`c`/`p` cut/copy/paste, `R` refresh, `Esc` back to editor, `q` close |
| `space c`                          | command palette: fuzzy-run any command                                                                                                                                                                                                      |
| `space f`                          | fuzzy file finder                                                                                                                                                                                                                           |
| `space d`                          | directory browser picker (Enter descends, Backspace goes up)                                                                                                                                                                                |
| `space t` `:term`                  | shell in a split below; `C-\ C-n` or `C-w N` for normal mode there, `i` back in, `space t` again hides it                                                                                                                                  |
| `space T`                          | theme picker with live preview                                                                                                                                                                                                              |
| `:w` `:wa` `:q` `:q!` `:e f` `:42` | ex commands; also `:e!` (reload), `:rename`, `:recover`, `:md`, `:term`, `:theme`, `:help` for the full list                                                                                                                                                                                                                                 |
| `:%s/pat/repl/g`                   | substitute: `%` = whole buffer (omit for the cursor line), `g` = every match (omit for first per line), `i` = ignore case; pattern is a regex with `\1` groups                                                                              |

Any command in the registry is also callable by name, so `:join_lines` works.

## Architecture

```
transaction.rs   changesets: the edit and undo primitive
position.rs      char offsets <-> display columns, soft-wrap points
document.rs      rope buffer, cursor, undo history, swap and file I/O
keymap.rs        keys, and the trie mapping sequences to commands
commands.rs      every action, as a named static value
textobj.rs       the ranges mi/ma select
vcs.rs           ivaldi's sealed text, and the diff behind the gutter marks
editor.rs        state, key dispatch, ex commands, scrolling
editor/          one file per area of that state:
  repeat.rs        recording and replaying inputs (. and macros)
  objects.rs       what the character after mi/ma/md/mr/q/@ does
  selections.rs    operations on the whole set of selections
  jumps.rs         the jumplist, and finding the buffer for a file
  lsp_glue.rs      syncing buffers and draining server events
  lsp_features.rs  references, rename, code actions, symbols, formatting
  completion.rs    the completion menu
  watch.rs         files changed on disk, swap files, change markers
  mouse.rs         screen cells back to buffer positions
  tree.rs          the file tree sidebar
  picker_keys.rs   the popup picker's keys
  terminal_split.rs  the shell window
  tools.rs         background installs and dependency versions
markdown.rs      markdown -> styled rows, for the preview pane
vt.rs            terminal emulator: pty bytes -> a grid of styled cells
terminal.rs      the shell process on its pty, and the keys sent to it
ui.rs            rendering
```

Three decisions shape everything else.

**Edits are transactions, not mutations.** A `Transaction` describes the whole
document as a sequence of retain/delete/insert operations. It can be inverted
against the original text to produce an exact undo, and it can map a position
from before an edit to after it. Multiple cursors, macros, and collaborative
editing are all operations on changesets — none of them require touching the
buffer code. Mutating the rope directly and adding undo afterwards means
rewriting the core.

**Char offsets are canonical.** Byte offsets, char offsets, display columns, and
UTF-16 code units agree only for ASCII, and disagree the moment a tab, an emoji,
or a combining accent appears. Everything internal is a char offset into the
rope; `position.rs` owns every conversion and is the only place that knows what
a "column" means. Getting this wrong is the most common way a hobby editor ends
up with a cursor that drifts out of sync with what's on screen.

**Bindings are data.** Commands are `&'static` values with names; keymaps are
tries over key sequences. That is what makes `dd` and `gg` possible at all, and
it means user config becomes a matter of parsing strings into the existing
`bind_str` calls rather than restructuring dispatch.

Undo grouping uses a group id per history entry rather than composing
transactions: everything typed in one insert-mode session shares an id, and undo
pops the whole group. Cheaper than implementing transaction composition, and it
can be swapped out later without changing callers.

Multiple cursors cash in the transaction bet. Commands run once per cursor —
each extra selection is swapped into the primary slot in turn — and while one
cursor edits, every other cursor is remapped through the edit's changeset by
`map_pos`, so positions never go stale. Insert-mode typing is the other way
round: one transaction with an insert at every cursor. Either way the edits of
one keypress share an undo group, so a multi-cursor edit undoes as a unit. No
command needed rewriting to become multi-cursor aware.

## Not done yet

Roughly in the order worth doing them:

1. **Whole-project search and replace** — `space g` finds the matches; taking
   an edit across every file they are in needs the picker to hand its results
   to the multi-cursor machinery rather than to a jump.
2. **A diff view** — the gutter says a line changed; it can't yet show what it
   changed from. The sealed text is already in memory for the markers.
3. **Inlay hints and semantic tokens** — the two LSP features left. Both are
   virtual text, which the diagnostics already prove out.
4. **More grammars and themes** — a grammar is one dependency and one arm in
   `syntax::config_for`; a theme is one entry in `theme::THEMES`. The config
   parser is a deliberate TOML subset; swap in the `toml` crate if it ever
   needs arrays or nesting.

## Tests

```
cargo test
```

The tests cover the parts that are easy to get subtly wrong and hard to notice:
transaction inversion round-trips, undo grouping, tab and wide-character column
math (including that a ZWJ emoji is measured as the one glyph it is drawn as),
count parsing, the sticky goal column, soft-wrap break points and the row
arithmetic that scrolling depends on, that markdown's markup is consumed rather
than shown, what `.` decides to repeat, the text-object finders, the line diff
behind the gutter marks, applying a workspace edit to a file that isn't open,
and that a file changed on disk reloads without losing the cursor.

```
cargo test --release -- --ignored    # the benchmarks
```

`document::bench::editing_beats_reparsing_the_file` is the guard on the thing
this editor is supposed to be: it asserts that an edit plus colouring one
screenful costs less than a tenth of re-parsing the file.

## Note

Change markers assume ivaldi, not git: the base comes from `ivaldi whodidit`,
and a file outside an ivaldi repository simply has no markers. `vcs_gutter =
false` turns the whole thing off.
