# Scripting & output — design

Date: 2026-07-24
Status: approved
Batch: WHATS-NEXT sub-project 3 of 4 (scripting/output)

## 1. Problem

Four gaps that matter for scripting querymatter and for reusing queries:

- **W2** no way to branch on "did the query match anything" — a zero-row
  result exits 0 like any success.
- **W3** no way to write a query's result to a file except shell `>`
  (unavailable mid-REPL-session).
- **W15** no way to save and re-run a named query — recurring reports are
  retyped every time.
- **W21** no way to ask why a given file isn't showing up in results, given
  the four interacting discovery/ignore layers.

All changes are in `src/main.rs`/`src/cli.rs` plus a new `src/queries.rs`
(saved queries) and small `src/repl.rs`/`src/discover.rs` additions. The query
engine and default rendering are untouched.

## 2. `--exit-code` (W2)

`main` today returns `anyhow::Result<()>`, so any error exits 1 (anyhow's
`Termination`) and success always exits 0. To give a scriptable signal, `main`
returns `std::process::ExitCode`, and a `--exit-code` flag on `Cli` opts into
grep-style codes:

| condition (with `--exit-code`) | exit |
|---|---|
| the run produced **≥ 1** total result row | 0 |
| the run produced **0** total rows across all statements | 1 |
| a parse/execution/IO error | 2 |

- Without `--exit-code`: today's behavior is unchanged — 0 on success, and an
  error still exits non-zero (1) via the existing propagation.
- **Multi-statement:** the "row" tally is the sum across every statement in the
  run (`-e 'a; b'` or piped batch); exit 1 only when **all** statements
  produced zero rows.
- The error path: under `--exit-code`, `main` catches the error, prints it to
  stderr (the `querymatter: {err:#}` style already used elsewhere), and returns
  `ExitCode::from(2)` — so "no rows" (1) and "error" (2) are distinguishable.
- `run_statements` returns the total row count (it already renders each
  statement; add up `render_statement_counted`'s counts) so `run_query` can map
  it to the exit code. `--exit-code` only affects query mode; `init`/`config`/
  `completions`/`query`/`explain` are unaffected (they keep returning success/error).

`--exit-code` is a `Cli` flag (query mode). It does not change what is printed,
only the process exit status.

## 3. `--output <path>` and REPL `.output` (W3)

A single **output sink** abstraction: results go to stdout by default, or to a
file when redirected.

- **CLI `--output <PATH>`** (query mode): the rendered result(s) are written to
  `PATH` instead of stdout, **overwriting** it (truncate). A multi-statement
  run concatenates its statements' rendered output into the one file, each
  followed by a newline (mirroring how stdout prints them today). Diagnostics
  and the (optional) `--exit-code` logic are unaffected. `--output -` is not
  special-cased; use the absence of the flag for stdout.
- **REPL `.output [path]`**: `.output <path>` redirects subsequent statement
  results to `<path>` (overwriting on first open, then appending each result
  within the session so a sequence of queries accumulates — the REPL is
  interactive, so append-within-session is the useful semantics); `.output`
  with no argument, or `.output stdout`, resets to stdout. The REPL prints a
  one-line confirmation to stderr (`querymatter: writing results to <path>` /
  `querymatter: results back on stdout`). The row-count line and errors stay on
  stderr regardless of the sink.

Implementation: the choke points are `main::run_statements` (which `println!`s
each rendered result) and `repl::run`'s `Line::Statement` arm. Introduce a
tiny sink (an `enum OutputSink { Stdout, File(PathBuf) }` with a
`write_result(&self, rendered: &str)` that appends the rendered block + newline,
opening the file in truncate-on-first-write / append-after mode as the mode
requires) or, for the one-shot path, simply collect the rendered blocks and
write them once. Keep it small; do not build a general redirection framework.

## 4. Saved queries (W15)

A separate file `<config_dir>/querymatter/queries.toml` (never the settings
`config.toml`), mapping a name to SQL text:

```toml
stale = "SELECT file.name WHERE status = 'draft'"
by-epic = "SELECT epic, count(*) AS n GROUP BY epic"
```

A new `src/queries.rs` mirrors `config.rs`'s shape: `queries_path()` (via the
same `ProjectDirs`), `load()`/`load_from`, `save_to` (atomic, via the existing
`write_atomic`), and `set`/`remove`/`get` over a `BTreeMap<String, String>`
wrapper (or a `#[serde(flatten)]` map). A missing file is empty, not an error;
a malformed file is a hard error naming the path (same discipline as config).

**Name rule:** a saved-query name must match `[A-Za-z0-9_-]+` (a simple
identifier), validated on `save`, so it round-trips unambiguously in TOML and
is safe to tab-complete. An invalid name is a clean error.

### 4.1 CLI: `querymatter query <ACTION>`

| command | behavior |
|---|---|
| `query save <NAME> <SQL>` | validate the name and that `SQL` parses (reuse `query::parse` to reject a broken query up front), then write it; confirm on stderr. |
| `query list` | one saved query per line to stdout as `NAME<TAB>SQL` (greppable and machine-splittable); empty output when none are saved. |
| `query get <NAME>` | print the saved SQL to stdout (composes with a shell pipe). |
| `query run <NAME>` | resolve `NAME` → SQL and run it exactly as `-e <SQL>` would — build the store, run the statement(s), render. Honors `--format`/`--table-style`/`--output`/`--exit-code`/the walk flags like any query. An unknown name is a clean error naming it. |
| `query delete <NAME>` | remove it; a missing name is reported, not an error; confirm on stderr. |

`query` is a peer of `init`/`config`/`completions` in the `Command` enum.
`query run` shares `run_query`'s store-building + `run_statements` path (feed
the resolved SQL in place of `cli.query`), so saved queries get the full query
surface for free.

### 4.2 REPL: `.query`

- `.query list` — the saved-query names (and SQL) to stdout.
- `.query run <name>` — run the saved query in the current session (reuse
  `render_statement`/`render_statement_counted` on the resolved SQL, honoring
  the session's format/style/output sink).
- `.query save <name> <sql>` is **out of scope** for the REPL in v1 (the CLI
  `query save` covers authoring); `.query` in the REPL is run/list only.
  Unknown name → stderr error.

### 4.3 Tab-completion hook

The REPL completer (from sub-project 2) gains a case: after `.query run `,
complete saved-query names (from `queries::load()`'s keys). This realizes the
"saved-query names" part of the completion design that was deferred because
saved queries didn't exist yet.

## 5. `explain <path>` (W21)

`querymatter explain <PATH>` reports whether `PATH` would be discovered under
the current walk configuration and, if excluded, which layer excluded it:

```
$ querymatter explain notes/.draft/x.md
excluded: hidden directory '.draft' (pass --hidden to include)

$ querymatter explain notes/todo.txt
excluded: extension 'txt' not in --ext (md, markdown)

$ querymatter explain notes/keep.md
included
```

- The **verdict** (included/excluded) is ground-truthed against the real
  discovery: run `discover(root, opts)` for the enclosing root and check
  whether the canonicalized `PATH` is in the result. This guarantees the
  verdict matches what a query actually sees.
- The **reason** (when excluded) is attributed by checking the layers in the
  order discovery applies them, reporting the first that excludes `PATH`:
  extension not in `--ext`; a hidden path component (no `--hidden`); a matching
  `--exclude` glob; a match in an applied ignore file (`.querymatterignore` /,
  under `--respect-gitignore`, `.gitignore`). If the verdict says excluded but
  no single layer is identified, report a generic "excluded by the ignore
  rules" rather than guessing. Output goes to **stdout** (it is the command's
  data).
- A path that does not exist, or is outside every scan root, is a clean error.
- Optional REPL `.explain <path>` mirrors it; include it if cheap, else CLI-only
  (state which in the plan).

`explain` reuses `WalkFlags`/`WalkOpts` resolution (via `Settings`), so it
reflects the same `--ext`/`--hidden`/`--exclude`/ignore-file configuration a
query would use.

## 6. Invariants this batch depends on

- **`main`'s exit status is the only thing `--exit-code` changes** — printed
  output is byte-identical with and without the flag (the flag is read after
  rendering).
- **stdout carries data; stderr carries diagnostics** — `--output` redirects
  the *data* stream only; the row-count line, `.output`/`query`/`explain`
  confirmations, and errors stay on stderr (or, for `explain`/`query get`, the
  command's data is its stdout).
- **`write_atomic` + `ProjectDirs`** are the existing primitives `queries.rs`
  reuses; `queries.toml` is a *separate* file from `config.toml`.
- **`discover()` is the ground truth** for `explain`'s verdict; the reason is a
  best-effort attribution, never contradicting the verdict.

## 7. Testing

- **W2:** `--exit-code` exits 0 when a query matches rows, 1 when it matches
  none, 2 on a parse/exec error; without the flag the exit status is unchanged
  (0 on the zero-row case). Multi-statement: exit 1 only when every statement is
  empty; exit 0 if any statement has rows. (Integration tests via `assert_cmd`
  asserting `.code(...)`.)
- **W3:** `--output <file>` writes the rendered table to the file and stdout is
  empty; a multi-statement run concatenates; the file is overwritten not
  appended across separate invocations. Unit-test the sink's write/reset logic.
  (REPL `.output` sink behavior via a testable helper, not a PTY.)
- **W15:** `queries.rs` round-trip (save→load); name validation rejects a bad
  name; a malformed file errors naming the path; `query save` rejects a query
  that doesn't parse; `query run <name>` produces the same output as `-e <sql>`;
  `query run <unknown>` errors; `query delete` of an absent name is not an
  error; `query list`/`get` output. Integration tests isolate `queries.toml`
  via the temp-config-home helper. The completer offers saved names after
  `.query run `.
- **W21:** `explain` reports `included` for a matched file; `excluded:
  extension …` for a wrong-ext file; `excluded: hidden …` for a hidden-dir
  file; `excluded:` for a `--exclude`-matched and a `.querymatterignore`-matched
  file; the verdict always matches `discover()` membership (a property test over
  a small tree); a nonexistent path errors.
- **Regression guard:** the committed render snapshots stay byte-identical; the
  existing one-shot/`-e`/batch tests still pass (this batch adds flags/commands,
  doesn't change existing query behavior). Isolate `HOME`+`XDG_CONFIG_HOME`
  (+`XDG_STATE_HOME`/`XDG_DATA_HOME` if a REPL path is exercised) in every test.

## 8. Files touched

| file | change |
|---|---|
| `src/main.rs` | `ExitCode` return, `--exit-code` mapping, `--output` sink wiring, `query`/`explain` dispatch |
| `src/cli.rs` | `--exit-code`, `--output`, `Command::Query`/`QueryAction`, `Command::Explain`/`ExplainArgs` |
| `src/queries.rs` | **new** — saved-query file schema + IO + name validation |
| `src/discover.rs` | an `explain`-supporting attribution helper (or a small `pub` seam) |
| `src/repl.rs` | `.output`, `.query run/list`, the output sink in the REPL loop, completer hook for `.query run` |
| `src/session.rs` | if the REPL output sink needs a seam (else none) |
| `README.md` | `--exit-code`, `--output`/`.output`, `query` subcommand + `queries.toml` + `.query`, `explain` |
