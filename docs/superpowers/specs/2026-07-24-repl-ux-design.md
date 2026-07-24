# REPL UX improvements — design

Date: 2026-07-24
Status: approved
Batch: WHATS-NEXT sub-project 2 of 4 (REPL UX)

## 1. Problem

Five interactive-experience gaps in the REPL, all cheap and compounding for
anyone who uses querymatter's shell day to day:

- **W4** no startup banner — a first-time user sees only `querymatter> ` with
  no record count and no hint that `.help`/`.schema` exist.
- **W5** a multi-line statement is stored as one history entry *per raw line*,
  so Up-arrow only recalls its last line.
- **W10** no row-count feedback — a zero-row result (header only) is easy to
  misread as "nothing happened".
- **W13** no tab-completion inside the REPL (the outer shell has completions,
  the REPL has none).
- **W18** `.schema` shows only field names — no per-field type or coverage.

All changes live in `src/repl.rs`, with small additions to `src/session.rs`
(and a read-only walk of the store's records for `.describe`). Nothing changes
the one-shot/`-e`/batch surface or default rendered output.

## 2. Startup banner (W4)

`repl::run` prints a one-line banner before entering the read loop, to
**stdout** once at startup (it is not query output; the batch/`-e` paths never
call `run`, so stdout-cleanliness for piping is unaffected):

```
querymatter — 42 records. Type .help for commands, .schema for fields.
```

The record count reuses the existing `record_count(session)` helper (a
`SELECT count(*)`), and the wording names `.help` and `.schema`. When the store
has zero records the count still prints (`0 records`). No banner is printed in
one-shot/batch mode.

## 3. Multi-line statement history (W5)

Today `editor.add_history_entry(line)` runs on **every raw input line** (before
`LineBuffer` knows whether the statement is complete), so a three-line
`SELECT … \n FROM … \n WHERE …;` becomes three fragments in history.

New behavior: do **not** add per-raw-line. Instead add exactly one history
entry when a line resolves:

- `Line::Statement(stmt)` → add the full assembled statement text (the joined
  multi-line SQL, with its terminator — `LineBuffer` already produces the
  joined `Statement.sql`; add that, optionally re-appending the terminator so
  re-running it works).
- `Line::Dot(_)` → add the dot-command line verbatim (single line).
- `Line::Blank` / `Line::More` → add nothing.

So Up-arrow recalls a whole statement or a whole dot-command, never a fragment.
History persistence (the `history_path()` file) is unchanged in mechanism.

## 4. Row-count line (W10)

After a REPL statement runs, print a summary line to **stderr** (never stdout,
so a `.output` capture or a piped REPL is unaffected) in the form:

```
-- 3 rows
```

Singular `-- 1 row` for exactly one; `-- 0 rows` for an empty result. Printed
only in the REPL (`repl::run`), not in one-shot/batch mode. A `\G`
(vertical) statement's count is the same record count.

**Implementation:** the REPL needs the row count alongside the rendered string.
`Session::render_statement` today returns only the `String`. Add
`Session::render_statement_counted(&Statement) -> anyhow::Result<(String, usize)>`
that runs the query once, renders it, and returns `(rendered, table.rows.len())`;
`render_statement` becomes a thin wrapper returning just the string (so
`main.rs`'s batch/`-e` callers are unchanged). `repl::run` calls the counted
variant, prints the rendered result to stdout and the count line to stderr.

## 5. `.describe [field]` (W18)

A new REPL dot-command giving per-field detail `.schema` does not:

```
querymatter> .describe status
status   Str    47/50 non-null (94%)
         values: draft(31), synced(14), archived(2)

querymatter> .describe
file.name   (file.*)
status      Str    47/50 non-null (94%)  draft(31) synced(14) …
prd         Str    50/50 (100%)          010(20) 011(30)
tags        List   38/50 (76%)
```

- `.describe <field>` — detail for one frontmatter field: the `Value` variant(s)
  seen across records (e.g. `Str`, or `Str/Int` when mixed), the non-null count
  and coverage percentage, and — for a low-cardinality field (distinct count ≤
  a small cap, e.g. 12) — the distinct values with per-value counts, most
  frequent first. A high-cardinality field omits the value list (shows the
  distinct count instead, e.g. `47 distinct values`).
- `.describe` (no arg) — a one-line-per-field summary for every field, plus the
  `file.*` pseudo-columns noted as such.
- `.describe <unknown>` — a clean stderr error naming the field and, when
  close, a did-you-mean drawn from the schema (reuse the same nearest-name
  idea W12 introduced, or a simple version — do not over-engineer).

`DotCommand` gains `Describe(Option<String>)` and `BadDescribeField(String)`
(or the unknown-field case is handled at dispatch against the live schema —
implementer's choice, but the error must name the field).

**Data source:** a new `Session::describe(field: Option<&str>) -> DescribeReport`
(or a pair of methods) that walks `self.store.records()` once, tallying per
field: the set of `Value` variant names, non-null count, total count, and a
value→count map (capped). `Value`'s variant name and its `display()` string are
the existing conversions to reuse. This is read-only; it does not run a query.
The formatting (alignment, the coverage percentage, the capped value list)
lives in `repl.rs`.

## 6. Tab-completion (W13)

The REPL currently uses `rustyline::DefaultEditor` (no completer). Replace it
with an `Editor<H, FileHistory>` where `H` is a small custom helper
implementing `rustyline::completion::Completer` (and the other `Helper`
sub-traits with default/no-op impls). The completer offers, based on the word
being typed and its position:

- **Dot-commands** — when the line starts with `.`, complete the command name
  (`.help`, `.schema`, `.describe`, `.format`, `.style`, `.set`, `.unset`,
  `.settings`, `.reload`, `.refresh`, `.refresh-all`, `.quit`, `.exit`, …). The
  set is derived from the same names `parse_dot` recognizes (keep them in one
  place so the completer and parser can't drift).
- **Config keys** — after `.set `/`.unset `, complete `ConfigKey::ALL` names.
- **Column names** — otherwise (in SQL position), complete frontmatter field
  names (`session.schema()`) and the `file.*` pseudo-columns (`FILE_COLUMNS`).

Not completed: SQL keywords (too noisy) and, in this sub-project, saved-query
names — saved queries (W15) do not exist yet; when they land in sub-project 3,
that task extends the completer. The helper holds a snapshot of the schema at
REPL start; a `.reload`/`.refresh` that changes the schema need not refresh the
completer mid-session in v1 (note this limitation).

**Scope guard:** the completer is best-effort — completing column names in
"SQL position" can be approximate (e.g. complete any bare word against the
schema). It must never block input, panic, or interfere with typing a value
that isn't a completion candidate.

## 7. Invariants this batch depends on

- **`record_count`, `print_schema`, `print_help` already exist** and compute
  exactly what the banner reuses.
- **`LineBuffer` assembles the full joined statement** at completion — the
  single point W5 hooks for one-entry-per-statement history.
- **stdout carries query results only in the REPL**; the banner is startup
  chrome (acceptable on stdout, matching sqlite3/mysql), the row-count and
  `.describe`-error lines go to stderr, so a piped REPL or `.output` capture is
  never corrupted.
- **`Value::display()` and the `Value` variant names are the canonical
  conversions** `.describe` reuses.

## 8. Testing

- **W4:** an integration test (piped stdin to the REPL, or an interactive
  path) asserting the banner line with the record count appears on stdout at
  startup; batch/`-e` mode emits NO banner.
- **W5:** unit-test the history hook — feeding a multi-line statement through
  the run-loop logic records ONE entry equal to the joined statement, and a
  dot-command records one entry; a blank/continuation records none. (Extract
  the "what to add to history" decision into a testable function if `run`'s IO
  loop is otherwise hard to test.)
- **W10:** `render_statement_counted` returns the correct `(String, usize)`
  for a 0-row, 1-row, and N-row result (unit); an integration test asserts the
  `-- N rows` line appears on **stderr** (not stdout) in REPL mode and the
  singular/plural form is right; batch mode emits no count line.
- **W18:** unit-test `Session::describe`: variant tally (incl. a mixed-type
  field), non-null coverage, distinct-value counts with the cap, and the
  high-cardinality omission; `parse_dot(".describe status")` /`.describe`
  parsing; an unknown field errors naming it.
- **W13:** unit-test the completer's candidate generation directly (given a
  line + cursor, it returns the expected dot-command / config-key / column
  candidates), so it's testable without a TTY. Assert dot-command completion
  after `.`, config-key completion after `.set `, column completion for a bare
  word, and no SQL-keyword noise.
- **Regression guard:** the committed render snapshots
  (`table_snapshot.snap`, `md_snapshot.snap`) stay byte-identical; the
  one-shot/`-e`/batch tests in `tests/cli.rs` are unchanged (this batch does
  not touch that surface).

## 9. Files touched

| file | change |
|---|---|
| `src/repl.rs` | banner, history hook, row-count line, `.describe` dispatch+format, the completer helper + `Editor<H,_>` wiring, `DotCommand::Describe` |
| `src/session.rs` | `render_statement_counted`, `describe(...)` (+ a `DescribeReport` type), any accessor the completer/describe needs |
| `README.md` | document the banner, `.describe`, the row-count line, REPL tab-completion, and the one-entry-per-statement history |
