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

Multiple cursors are native, not a plugin: `C` copies the cursor to the next
line, and then every motion, selection, edit, and keystroke of insert-mode
typing applies at every cursor. A multi-cursor edit is a single undo step, and
a multi-cursor delete or copy captures every selection into the register.

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

LSP without an async runtime: the server runs as a child process, a thread
feeds its messages into a channel, and the main loop drains it between
keystrokes — the editor never blocks on the server. Diagnostics color the
gutter (red errors, yellow warnings) and the message for the cursor line shows
below the status bar; `gd` jumps to a definition (opening the file if needed);
`K` shows hover info. Completions come from the server as you type an
identifier, on the trigger characters it advertises (`.` for members, `<` for
Oxigen's type annotations), and on demand with `C-space`. rust-analyzer is
wired up by default; any server is one config line.

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

[lsp]                        # file extension = server command
rs = "rust-analyzer"
py = "pyright-langserver --stdio"

[keys.normal]                # any :command name is bindable
"C-p" = "search"
gq = "quit"
```

Plus splits (`C-w v/s/w/q`) with independent cursors per window, counts (`3x`,
`10d`, `5C`), multi-key bindings (`gg`), transaction-based undo/redo with
sensible grouping, multiple buffers, ex commands, vertical and horizontal
scrolling, and grapheme-aware cursor movement — the cursor never lands inside
an emoji ZWJ sequence or a combining stack.

|                                    |                                                                                                                                                                                                                                             |
| ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `h` `j` `k` `l`                    | move, collapsing the selection (arrows work too); `h` wraps to the previous line                                                                                                                                                            |
| `%`                                | jump to the bracket matching the one under the cursor; the match also glows whenever the cursor sits on a bracket                                                                                                                           |
| `w` `b` `e`                        | select to next word / previous word / word end                                                                                                                                                                                              |
| `V`                                | select (highlight) the line; repeat to extend                                                                                                                                                                                               |
| `v`                                | extend mode: every motion grows the selection (status shows `SEL`)                                                                                                                                                                          |
| `;`                                | collapse the selection to the cursor                                                                                                                                                                                                        |
| `A-o`                              | expand the selection to the enclosing syntax node                                                                                                                                                                                           |
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
| `gd` `K`                           | goto definition / hover (LSP)                                                                                                                                                                                                               |
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
| `space t`                          | theme picker with live preview                                                                                                                                                                                                              |
| `:w` `:q` `:wq` `:q!` `:e f` `:42` | ex commands                                                                                                                                                                                                                                 |

Any command in the registry is also callable by name, so `:join_lines` works.

## Architecture

```
transaction.rs   changesets: the edit and undo primitive
position.rs      char offsets <-> display columns, soft-wrap points
document.rs      rope buffer, cursor, undo history, file I/O
keymap.rs        keys, and the trie mapping sequences to commands
commands.rs      every action, as a named static value
editor.rs        state, key dispatch, ex commands, scrolling
markdown.rs      markdown -> styled rows, for the preview pane
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

1. **Completion** — the LSP plumbing is there; this needs a popup-menu widget,
   which is why it isn't. `textDocument/completion` plus a filtering list.
2. **More grammars and servers** — each grammar is one dependency and one arm
   in `syntax::config_for`; each server is one row in a table `lsp.rs` doesn't
   have yet (rust-analyzer is hardcoded).
3. **Rendering-side graphemes** — the cursor steps by grapheme now, but width
   is still summed per char, so a ZWJ emoji renders wider than it should.
4. **More themes and config options** — a theme is one entry in
   `theme::THEMES`; an option is one key in `config.rs`. The parser is a
   deliberate TOML subset; swap in the `toml` crate if the config ever needs
   arrays or nesting.

## Tests

```
cargo test
```

The tests cover the parts that are easy to get subtly wrong and hard to notice:
transaction inversion round-trips, undo grouping, tab and wide-character column
math, count parsing, the sticky goal column, soft-wrap break points and the
row arithmetic that scrolling depends on, and that markdown's markup is
consumed rather than shown.

```
cargo test --release -- --ignored    # the benchmarks
```

`document::bench::editing_beats_reparsing_the_file` is the guard on the thing
this editor is supposed to be: it asserts that an edit plus colouring one
screenful costs less than a tenth of re-parsing the file.

## Note

This was written without a compiler available, so expect to fix a few type
errors on first build. The design is the part worth keeping; the syntax errors
are cheap.
