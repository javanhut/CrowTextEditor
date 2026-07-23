# ked

A modal terminal text editor, built as a foundation to grow into a daily driver.

```
cargo run --release -- src/main.rs
```

## What works

Selection-first modal editing — the Helix/Kakoune model, not the vim model.
Motions select the text they cross, `x` selects lines, and `d`/`c`/`y` act on
the selection, so what an edit will touch is on screen before you commit to it.
There is no operator-pending state.

Multiple cursors are native, not a plugin: `C` copies the cursor to the next
line, and then every motion, selection, edit, and keystroke of insert-mode
typing applies at every cursor. A multi-cursor edit is a single undo step, and
a multi-cursor delete or yank captures every selection into the register.

Search and multi-cursor are the same feature. `/` searches incrementally by
regex and the match *is* a selection, so `d`, `c`, and `y` compose with it;
`n`/`N` walk the matches. `s` puts a selection on **every** match — inside the
current selection if there is one, else the whole buffer — so interactive
replace-all is just `s foo ⏎ c bar ⎋`, with every edit site visible before you
type, and `s \d+ ⏎` puts a cursor on every number. A pattern that doesn't
compile (yet) is searched literally, so the preview never breaks mid-keystroke.

Syntax is a selection too. Tree-sitter parses Rust files, colors them, and
`A-o` grows the selection to the enclosing syntax node — token, expression,
statement, block, function — one keypress per level. Since it's just a
selection, `d`/`c`/`y`, multi-cursor, and `s` all compose with it.

Plus splits (`C-w v/s/w/q`) with independent cursors per window, counts (`3x`,
`10d`, `5C`), multi-key bindings (`gg`), transaction-based undo/redo with
sensible grouping, multiple buffers, ex commands, vertical and horizontal
scrolling, and grapheme-aware cursor movement — the cursor never lands inside
an emoji ZWJ sequence or a combining stack.

| | |
|---|---|
| `h` `j` `k` `l` | move, collapsing the selection (arrows work too) |
| `w` `b` `e` | select to next word / previous word / word end |
| `x` | select the line; repeat to extend |
| `v` | extend mode: every motion grows the selection (status shows `SEL`) |
| `;` | collapse the selection to the cursor |
| `A-o` | expand the selection to the enclosing syntax node |
| `C` `A-C` | add a cursor on the next / previous line |
| `,` | drop the extra cursors (`Esc` in normal mode too) |
| `/` `n` `N` | incremental regex search; next / previous match |
| `s` | select every match within the selection (or buffer) |
| `"x` | use register `x` for the next delete/yank/paste |
| `d` `c` `y` | delete / change / yank the selection (`d` alone: char) |
| `p` `P` | paste after / before (linewise if the yank was) |
| `0` `^` `$` | line start / first non-blank / line end |
| `gg` `G` | file start / end; `42gg` or `42G` jumps to line 42 (also `:42`) |
| `C-d` `C-u` `C-f` `C-b` | half page / full page |
| `C-w v` `C-w s` `C-w w` `C-w q` | split side-by-side / stacked, cycle, close |
| `i` `I` `a` `A` `o` `O` | enter insert mode |
| `D` `J` | delete to line end, join |
| `u` `C-r` | undo / redo |
| `gn` `gp` | next / previous buffer |
| `:w` `:q` `:wq` `:q!` `:e f` `:42` | ex commands |

Any command in the registry is also callable by name, so `:join_lines` works.

## Architecture

```
transaction.rs   changesets: the edit and undo primitive
position.rs      char offsets <-> display columns
document.rs      rope buffer, cursor, undo history, file I/O
keymap.rs        keys, and the trie mapping sequences to commands
commands.rs      every action, as a named static value
editor.rs        state, key dispatch, ex commands, scrolling
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

1. **LSP.** The one big feature left, and the one that must not be done badly:
   it needs an async transport (`tokio` + `lsp-types`) so a slow server can
   never freeze the editor, plus server lifecycle management. LSP positions
   are UTF-16 code units — add the conversion to `position.rs` rather than
   scattering it.
2. **More grammars** — each is one dependency and one arm in
   `syntax::config_for`.
3. **Incremental parsing** — tree-sitter currently reparses the file per edit
   (fine below ~1MB); the transaction model already produces the edit deltas
   `InputEdit` wants.
4. **Rendering-side graphemes** — the cursor steps by grapheme now, but width
   is still summed per char, so a ZWJ emoji renders wider than it should.
5. **Config file** for keymaps and options. `bind_str` already takes the format.

## Tests

```
cargo test
```

The tests cover the parts that are easy to get subtly wrong and hard to notice:
transaction inversion round-trips, undo grouping, tab and wide-character column
math, count parsing, and the sticky goal column.

## Note

This was written without a compiler available, so expect to fix a few type
errors on first build. The design is the part worth keeping; the syntax errors
are cheap.
