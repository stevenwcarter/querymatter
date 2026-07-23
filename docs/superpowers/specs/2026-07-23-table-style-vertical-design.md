# Table styles and `\G` vertical output — design

Date: 2026-07-23
Status: approved

## 1. Problem

`querymatter`'s `table` format renders through `comfy-table`'s default ASCII
preset, which is safe on every terminal but plain. Users on a UTF-8 terminal
want box-drawing borders. Users piping output, or on a terminal that mangles
non-ASCII, must keep today's output byte-for-byte.

Separately, a wide `select *` is unreadable as a table: the columns wrap or run
off screen precisely when you most want to inspect one record's fields — the
`select * from files limit 1` case. MySQL solves this with the `\G` statement
terminator, which renders each row as a vertical block of `name: value` lines.

## 2. Goals

1. An **opt-in** table style, with today's ASCII output as the untouched
   default.
2. Style selectable per-invocation (flag), per-shell (env var), and live in the
   REPL (dot-command).
3. A `\G` statement terminator that renders results vertically, one record per
   block, working identically in the REPL, `-e`, and piped batch mode.

## 3. Non-goals

Explicitly out of scope, and no code should anticipate them:

- Terminal-width-aware wrapping (`ContentArrangement::Dynamic`, `set_width`).
- Color or bold styling of headers/cells.
- Auto-detection of terminal Unicode support. The feature is opt-in by
  request: the default is ASCII regardless of `TERM`, locale, or TTY-ness.
- A `vertical` value on `--format` / `.format`. `\G` is the only way to get
  vertical output.

## 4. Table styles

### 4.1 The type

`src/render.rs` gains a style enum, orthogonal to `Format`:

```rust
pub enum TableStyle { Ascii, Unicode, Compact, Plain }
```

It implements `FromStr` (case-insensitive) with a `TableStyleParseError`
naming the offending input, mirroring `FormatParseError`. Accepted names:
`ascii`, `unicode`, `compact`, `plain`. No aliases.

### 4.2 Preset mapping

| style | comfy-table configuration |
|---|---|
| `ascii` | **no preset loaded** — `Table::new()`'s default, so output is byte-identical to today |
| `unicode` | `presets::UTF8_FULL` + `modifiers::UTF8_SOLID_INNER_BORDERS` + `modifiers::UTF8_ROUND_CORNERS` |
| `compact` | `presets::UTF8_HORIZONTAL_ONLY` |
| `plain` | `presets::NOTHING` |

`ascii` deliberately loads nothing rather than loading `ASCII_FULL`, so the
default path is provably unchanged rather than merely believed-equal.

### 4.3 Scope of the style

`TableStyle` affects `Format::Table` **only**.

- `Format::Md` keeps `ASCII_MARKDOWN` unconditionally — it is a fixed Markdown
  dialect, not decoration.
- `Format::Json` / `Csv` / `Tsv` are data interchange and never consult the
  style.

This is pinned by tests (§8), not left to reviewer vigilance.

## 5. Selecting a style

### 5.1 CLI + environment

`Cli` gains:

```rust
/// Border style for `--format table`.
#[arg(long, env = "QUERYMATTER_TABLE_STYLE", default_value = "ascii")]
pub table_style: TableStyle,
```

This requires adding the `env` feature to the `clap` dependency in
`Cargo.toml` (`features = ["derive", "env"]`).

Precedence follows from clap and is not hand-rolled:

1. `--table-style <name>` (explicit flag)
2. `QUERYMATTER_TABLE_STYLE=<name>` (environment)
3. `ascii` (default)

An unparseable value from **either** source is a hard clap error naming the
bad value — a typo'd env var must not silently degrade to the default.

`--table-style` is accepted in query mode only. `querymatter init` produces no
stdout and renders no tables, so it does not gain the flag.

### 5.2 REPL

`.style [name]` mirrors `.format` exactly:

- `.style` with no argument prints `style: <name>` to **stdout** (inspection
  output, matching `.format`'s report).
- `.style <name>` sets the style for subsequent queries.
- `.style <bogus>` prints `querymatter: unknown style 'bogus' (try: ascii,
  unicode, compact, plain)` to **stderr**.

`DotCommand` gains `Style(Option<TableStyle>)` and `BadStyle(String)`, so a bad
style name is reported as an unknown *style*, never an unknown *command* —
the same distinction `BadFormat` already draws.

`.help` gains a `.style` line and its trailing prose gains the `\G` sentence
(§6.5).

## 6. `\G` vertical output

### 6.1 Terminators

Statement splitting stops assuming `;`. `src/session.rs` gains:

```rust
/// How a statement was terminated, which selects how its result renders.
pub enum Terminator { Semicolon, VerticalG }

/// One statement plus the terminator that ended it.
pub struct Statement { pub sql: String, pub terminator: Terminator }
```

Recognized terminators, all outside quoted literals:

| input | terminator | effect |
|---|---|---|
| `;` | `Semicolon` | render in the session's format |
| `\g` | `Semicolon` | identical to `;` (MySQL parity) |
| `\G` | `VerticalG` | render vertically |

`\g` / `\G` are **case-sensitive**: lowercase `\g` is an ordinary terminator,
uppercase `\G` is the vertical one, exactly as MySQL behaves. No other
backslash command is recognized.

### 6.2 Where terminators are parsed

Both existing splitting seams learn the new terminators, so REPL and batch
agree:

- **`session::split_statements`** (used by `-e` and piped stdin) is
  quote-aware today: a `;` inside `'…'`/`"…"` does not split. `\g`/`\G` join
  the same top-level-only rule and return `Vec<Statement>` instead of
  `Vec<String>`.
- **`repl::LineBuffer::push`** checks for a terminator *suffix* after
  `trim_end()` — the same shape as its current `strip_suffix(';')`, and
  likewise not quote-aware. `Line::Statement` carries a `Statement`.

The asymmetry (batch is quote-aware, the REPL buffer is not) is pre-existing
and preserved deliberately; this change does not attempt to unify them.

### 6.3 Rendering selection

`Format`'s `FromStr` must stay a closed, round-trippable set, so `Vertical` is
not a `Format` variant. `render.rs` gains:

```rust
/// What a single statement's result set renders as: the session's configured
/// format, or the per-statement `\G` vertical override.
pub enum Output { Format(Format), Vertical }
```

`render(table: &ResultTable, output: Output, style: TableStyle) -> String`.

`Session` maps `Terminator::Semicolon -> Output::Format(self.format)` and
`Terminator::VerticalG -> Output::Vertical`.

**`\G` wins over the session format.** With `.format json` active, a `\G`
statement renders vertically. `\G` means "show me this record-wise" regardless
of the standing format.

### 6.4 The vertical layout

MySQL's layout, fixed and unaffected by `--table-style`:

```
*************************** 1. row ***************************
   file.name: 2026-07-23-note.md
   file.path: /vault/notes/2026-07-23-note.md
      status: draft
        tags: rust, cli
*************************** 2. row ***************************
   file.name: other.md
      status: synced
```

- Banner: 27 asterisks, `` N. row `` (space-delimited, 1-based), 27 asterisks.
- Column names right-aligned to the widest header in the result, then `: `,
  then the cell's `Value::display()` — the same conversion every other format
  uses, so lists render `a, b` and `Null` renders empty.
- Header width is measured in `chars().count()`. Frontmatter keys are
  overwhelmingly ASCII, and taking a `unicode-width` dependency for this is
  not warranted.
- **Zero rows renders the empty string.** There are no headers to show without
  a row, and an empty result must stay distinguishable when piped.

### 6.5 Documentation surface

`.help`'s closing line becomes: statements end with `;` (or `\G` to render the
result one record per block) and may span multiple lines.

## 7. Invariants this feature depends on

- **`render()` returns output with no trailing newline.** Both printers
  (`repl::run` and `main::run_statements`) add exactly one via `println!`. The
  vertical renderer must uphold this, so it gets its own assertion rather than
  inheriting confidence from the other formats' tests.
- **`ResultTable` guarantees one cell per header** (see `query::ResultTable`).
  The vertical renderer zips headers with cells and relies on this; it must
  not panic if the invariant is ever weakened, so it zips (truncating) rather
  than indexing.
- **`Value::display()` is the single cell-to-text conversion.** Vertical
  output uses it rather than re-deriving formatting, so a future change to
  list or float rendering reaches every format at once.

## 8. Testing

Unit tests in `src/render.rs`:

- insta snapshots of `Format::Table` under all four styles.
- insta snapshots of vertical: multi-row, and a single row with a wide
  column-name spread (to pin right-alignment).
- Vertical with zero rows returns `""`.
- Vertical output does not end in `\n` (the §7 invariant).
- Vertical renders `Null`/`Bool`/`Float`/`List` via `Value::display()`.
- `TableStyle::from_str` accepts the four names case-insensitively and rejects
  an unknown one.
- **Style isolation:** `json`, `csv`, `tsv`, and `md` produce identical output
  under every `TableStyle`.
- **Default preservation:** `Format::Table` + `TableStyle::Ascii` matches the
  existing committed snapshot.

Unit tests in `src/session.rs`:

- `split_statements` on `;`, `\g`, `\G`, and mixtures, returning the right
  `Terminator` per statement.
- `\G` inside a quoted literal does not split.
- `\G` wins over a non-table session format.

Unit tests in `src/repl.rs`:

- `LineBuffer` terminates on `\G` (single-line and multi-line) with
  `Terminator::VerticalG`, on `\g` and `;` with `Semicolon`.
- `parse_dot(".style unicode")`, `.style` bare, and `.style bogus` →
  `BadStyle`.

Integration tests in `tests/cli.rs`:

- `--table-style unicode` on a real vault emits box-drawing characters.
- `QUERYMATTER_TABLE_STYLE=unicode` with no flag does the same.
- The flag overrides a conflicting env var.
- An invalid value in the flag, and separately in the env var, exits non-zero.
- `-e '… \G'` emits a `1. row` banner.
- Piped stdin mixing `;` and `\G` statements renders each accordingly.

## 9. Files touched

| file | change |
|---|---|
| `Cargo.toml` | clap `env` feature |
| `src/render.rs` | `TableStyle`, `Output`, `render_vertical`, `render()` signature |
| `src/cli.rs` | `--table-style` arg |
| `src/session.rs` | `Terminator`, `Statement`, `style` field, `set_style`, splitter + render signatures |
| `src/repl.rs` | `\G`/`\g` in `LineBuffer`, `.style` dot-command, help text |
| `src/main.rs` | pass style into `Session`, iterate `Statement`s |
| `README.md` | `--table-style`, `QUERYMATTER_TABLE_STYLE`, `.style`, `\G` |
