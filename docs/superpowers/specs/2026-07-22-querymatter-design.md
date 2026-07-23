# querymatter — design spec

**Date:** 2026-07-22
**Status:** Approved (brainstorming complete)
**Repo dir:** `hub-reader/` · **Crate/binary name:** `querymatter`

## 1. Summary

`querymatter` is a single static Rust binary that reads YAML frontmatter from
Markdown files under one or more directories and runs a **SQL-subset query**
against the resulting record set, printing results as a formatted table (or
JSON / CSV / TSV / Markdown). It is REPL-first — like the `sqlite3` or `mysql`
shell — with a one-shot `-e/--query` mode for scripting and piping.

It exists to fill a concrete gap: querying hundreds of AI-generated
work-tracking Markdown files by their frontmatter, e.g. counting how many files
sit in each `status`. The closest existing tool, `vaultdb` (Rust, actively
maintained), covers select/where/sort/limit over frontmatter but **has no
`GROUP BY` / aggregation** — which is exactly the headline use case here. Other
aggregating options are disqualified: Obsidian Dataview needs Obsidian,
`mdquery` is Python + a SQLite index, DuckDB is a verbose multi-piece install.

### Goals
- REPL and one-shot query over Markdown frontmatter, no Obsidian, no index/daemon.
- SQL-subset DSL: `SELECT … [AS alias] [FROM …] WHERE … GROUP BY … ORDER BY … LIMIT …`.
- `GROUP BY` + aggregates (`COUNT`, `MIN`, `MAX`, `SUM`, `AVG`, `GROUP_CONCAT`) as a first-class feature.
- Renameable column headers via SQL `AS`.
- `file.*` pseudo-columns (name/path/folder/ext) queryable alongside frontmatter fields.
- Output: pretty table (default), JSON, CSV, TSV, Markdown table.
- Architected so a future TTL-based directory cache (see §9) drops in without
  touching the query or render layers.

### Non-goals (v1)
- No list/array or nested-map query operators (flat scalars only; see §5). A
  stray list value renders comma-joined but has no membership operators yet.
- No JOINs, subqueries, window functions, or full ANSI SQL.
- No file watching / live reload (data is scanned once at startup; `.reload` re-scans on demand).
- No `.querymatter` vault marker, config file, or TTL cache yet (design for it — §9 — but do not build it).
- No writing/mutating frontmatter. Read-only.

## 2. CLI interface

```
querymatter [OPTIONS] [DIRS]...
```

| Arg / option | Meaning |
| --- | --- |
| `[DIRS]...` | Directories to scan recursively. Positional. Default: current working directory. **The query is never a positional.** |
| `-e, --query <QUERY>` | One-shot mode: run `QUERY`, print output, exit. A quoted SQL string. `--query -` reads the query text from **stdin**. May contain several `;`-separated statements. |
| `--format <FMT>` | Output format: `table` (default), `json`, `csv`, `tsv`, `md`. In the REPL this is the *initial* format; `.format` changes it live. |
| `--ext <LIST>` | Comma-separated file extensions to include. Default: `md,markdown`. |
| `--respect-gitignore` | Honor `.gitignore`/`.ignore` while walking. **Off by default** (see §8.1). |
| `--hidden` | Descend into hidden directories/files (e.g. `.git`, `.obsidian`). Off by default. |
| `--exclude <GLOB>` | Path glob to skip. Repeatable. E.g. `--exclude '**/templates/**'`. |
| `-h, --help` / `-V, --version` | Standard clap output; version from `Cargo.toml`. |

### Mode selection (truth table)

| Invocation | Behavior |
| --- | --- |
| `querymatter [dirs]`, stdin is a TTY | Interactive REPL |
| `querymatter -e "SELECT …" [dirs]` | Run once, print, exit |
| `querymatter --query - [dirs]` | Read the query from stdin, run once, exit |
| `… \| querymatter [dirs]` (stdin piped, no `-e`) | Batch mode: read `;`-separated statements from stdin, execute each, no prompt |

TTY detection uses `std::io::IsTerminal` on stdin so the REPL never blocks
waiting on a pipe. Precedence: if `--query` is given it always wins; otherwise
a non-TTY stdin means batch mode; otherwise interactive REPL.

## 3. Query DSL (v1 surface)

The DSL is a **subset of SQL** parsed by the `sqlparser` crate and executed by a
small hand-written interpreter. Supported clauses:

- **SELECT** — a list of items, each optionally `AS <alias>`:
  - a frontmatter field name (`status`, `jira`, `prd`, …)
  - a `file.*` pseudo-column: `file.name`, `file.path`, `file.folder`, `file.ext` (see §4)
  - `*` — expands to the union of all frontmatter keys seen across the loaded
    records, in sorted (alphabetical) order (does **not** include `file.*`
    pseudo-columns)
  - an aggregate: `COUNT(*)`, `COUNT(<col>)`, `COUNT(DISTINCT <col>)`,
    `MIN(<col>)`, `MAX(<col>)`, `SUM(<col>)`, `AVG(<col>)`, `GROUP_CONCAT(<col>)`
  - `AS <alias>` sets the column header used in every output format.
- **FROM** — optional. When present, its value is treated as a path glob filter
  applied within the scanned directories (Dataview-style `FROM 'plans/**'`).
  A single quoted string or bare identifier is accepted. When omitted, all
  discovered records are in scope and directories come from the CLI.
- **WHERE** — boolean expression over fields / `file.*` / literals:
  - comparisons: `= != <> < <= > >=`
  - `LIKE` / `NOT LIKE` (SQL `%`/`_` wildcards, case-sensitive)
  - `IN (<list>)` / `NOT IN (<list>)`
  - `IS NULL` / `IS NOT NULL`
  - combined with `AND`, `OR`, `NOT`, and parentheses.
- **GROUP BY** — one or more grouping keys (fields or `file.*`). When present,
  every non-aggregate SELECT item must be a grouping key (standard SQL rule;
  violations are a clear query error, not a silent "first value").
- **ORDER BY** — `<col|alias> [ASC|DESC]`, multiple keys. Aliases defined in
  SELECT are resolvable here. NULLs sort last regardless of direction.
- **LIMIT `<n>` [OFFSET `<m>`]**.

### FROM-less parsing note (implementation)
With the `GenericDialect`, `sqlparser` 0.62 accepts a `SELECT` with no `FROM`
(including one that carries `WHERE`/`GROUP BY`) natively, and parses a quoted
`FROM '<glob>'` as a quoted-identifier table — so **no** synthetic-`FROM`
injection and **no** raw-SQL regex are needed. `parse()` hands the SQL straight
to `sqlparser`; the optional FROM target (quoted glob or bare identifier) is
read from the parsed AST during lowering. Covered by parser tests for: no FROM,
`FROM '<glob>'`, `FROM ident`. (An earlier design assumed a synthetic-FROM /
regex-strip approach; the native behavior made both unnecessary.)

### Example queries
```sql
-- headline: count files per status, renamed header, within two dirs
SELECT status, count(*) AS Count WHERE prd = '010' GROUP BY status ORDER BY Count DESC

-- which files are still draft, showing the path
SELECT file.name, jira, status WHERE status = 'draft' ORDER BY file.name

-- group by folder
SELECT file.folder, count(*) AS n GROUP BY file.folder

-- list the jira keys in each epic
SELECT epic, group_concat(jira) AS keys GROUP BY epic
```

## 4. Data model & value semantics

### Record
Each Markdown file **that has a frontmatter block** becomes one `Record`:
- a map `field name → Value` from the YAML frontmatter (per-record field order
  follows `gray_matter`'s unordered map, so `SELECT *` and `.schema` sort field
  names alphabetically for determinism), and
- file metadata backing the `file.*` pseudo-columns.

A file with **no frontmatter block is skipped** — it is not emitted as an
all-NULL row. A file whose frontmatter block exists but fails to parse as YAML
is skipped with a warning to stderr (suppressible only by fixing the file; v1
has no `--quiet`, and warnings go to stderr so they never pollute piped stdout).

### `file.*` pseudo-columns
Resolved from the file path, independent of frontmatter:
- `file.name` — file name with extension (e.g. `DCP-459-some-work.md`)
- `file.path` — path as discovered (relative to the scan root it was found under)
- `file.folder` — parent directory portion of `file.path`
- `file.ext` — extension without the dot (e.g. `md`)

`file.*` names never collide with frontmatter fields because the `file.`
qualifier is reserved; a frontmatter key literally named `file` is accessed
unqualified and does not shadow the pseudo-namespace.

### Value type
An internal enum (not `serde_json::Value`, so comparison/coercion/rendering
semantics are ours):
```
Value = Null | Bool(bool) | Int(i64) | Float(f64) | Str(String) | List(Vec<Value>)
```
- Frontmatter scalars map to `Bool/Int/Float/Str`; YAML nulls and missing fields → `Null`.
- YAML sequences map to `List` (rendered comma-joined; no query operators in v1).
- YAML mappings (nested) map to `Str` of their compact serialization in v1
  (rendered but not addressable by dotted path — that is a §5 non-goal).

### Comparison & coercion (WHERE / ORDER BY / aggregates)
- **Missing field → `Null`.**
- Comparison coerces by the **literal's type**: a numeric literal compares
  numerically (both sides coerced to number when possible); a **quoted string
  literal forces string comparison** (the field value is stringified).
- Any comparison where a side is `Null` yields "not true" (SQL-like); such rows
  are excluded by a `WHERE`. Use `IS NULL` / `IS NOT NULL` to test for absence.
- Aggregates ignore `Null` inputs **except `COUNT(*)`**, which counts rows.
  `SUM`/`AVG` operate on numeric-coercible values and skip non-numeric ones;
  `MIN`/`MAX` use the same ordering as `ORDER BY`; `GROUP_CONCAT` joins the
  stringified non-null values with `, `.

## 5. Explicitly out of scope for v1 (with rationale)
- **List/array query operators** (`CONTAINS`, `IN <field>`): current sample data
  is flat scalars; YAGNI until real array frontmatter appears. Lists still render.
- **Nested-map dotted access** (`estimate.low`): no nested structure in the data yet.
- These are called out so a later change that adds them knows the seam (§4 Value
  already models `List`; the executor's field-resolution is the single place to extend).

## 6. Architecture

Pipeline (also the module layout of the `querymatter` binary crate):

```
discover ──▶ frontmatter ──▶ model::Record          (load phase, per directory)
                                   │
                                   ▼
                             store::RecordStore       (holds dir-keyed slices)
                                   │  records() flattened view
                                   ▼
   query::parse (sqlparser→AST) ──▶ query::exec ──▶ render (table/json/csv/tsv/md)
```

### Modules
- **`cli`** — clap `Parser` struct; flags/args from §2; resolves the mode.
- **`discover`** — `ignore::WalkBuilder` over the scan roots. gitignore and
  hidden filtering **off by default** (opt in via flags); extension filter and
  `--exclude` globs applied. Yields `(root, path)` pairs grouped by scan root.
- **`frontmatter`** — extract the `---` fence with `gray_matter`, deserialize the
  YAML block into the record map (`gray_matter`'s dynamic value → our `Value`).
  Returns `Option<Record>` (None for no-frontmatter / parse-error).
- **`model`** — `Record`, `Value`, and value coercion/compare/display helpers.
- **`store`** — `RecordStore` trait and the v1 eager in-memory implementation.
  Holds records **grouped by source directory** (a slice per directory) plus a
  per-slice `scanned_at` timestamp. Exposes:
  - `records() -> impl Iterator<&Record>` — flattened view the executor sees
  - `reload_dir(dir)` — re-scan one directory and **overwrite** its slice
  - `reload_all()` — `reload_dir` across all roots
  - `schema()` — the union of frontmatter field names (for `.schema` and `*`)
- **`query::ast`** — the small internal query AST (Select items, predicates,
  group/order/limit) independent of `sqlparser` types.
- **`query::parse`** — `sqlparser` → internal AST; FROM-less normalization (§3);
  rejects unsupported clauses with a clear `thiserror` error.
- **`query::exec`** — executes the AST over `store.records()`: filter → group →
  aggregate → project (aliases) → order → limit. Produces a `ResultTable`
  (header list + rows of `Value`).
- **`render`** — `ResultTable` → chosen format. `comfy-table` for `table` and
  `md`; `serde_json` for `json`; `csv` crate for `csv`/`tsv`. Aliases become
  headers; `Null` renders as an empty cell (empty in CSV/TSV, `null` in JSON).
- **`session`** — ties a `RecordStore` + current output format together; exposes
  `run(sql) -> Result<ResultTable>` and `set_format` / `reload`. Both REPL and
  one-shot drive the same `session`.
- **`repl`** — `rustyline` loop: prompt `querymatter> `, continuation `   ...> `
  until a `;` terminates a statement; multi-line buffering; dot-command
  dispatch; history persisted via `directories` to `$XDG_STATE_HOME/querymatter/history`.
  Query errors print and return to the prompt (never exit).
- **`main`** — wires `cli` → build `session` (initial scan) → dispatch on the
  mode truth table (§2).

### REPL dot-commands (single line, no `;`)
- `.help` — list commands.
- `.schema` — list discovered frontmatter fields, the `file.*` pseudo-columns, and record count.
- `.format <fmt>` — switch output format for subsequent queries.
- `.reload` — `reload_all()` (re-scan every directory).
- `.quit` / `.exit` — leave (Ctrl-D also exits; Ctrl-C cancels the current line).

## 7. Error handling
- **Binary boundary** (`main`): `anyhow` — a fatal startup error (unreadable dir,
  bad `--format` value) prints to stderr and exits non-zero.
- **Query layer** (`query::parse` / `query::exec`): `thiserror` typed errors with
  actionable messages (unsupported clause, unknown aggregate, non-grouped column
  in a `GROUP BY` query, unparseable SQL). In the REPL these print and return to
  the prompt; in one-shot/batch mode a query error exits non-zero (batch mode
  reports the failing statement and stops).
- **Per-file problems** (missing/invalid frontmatter): warn to **stderr**, skip
  the file, continue. Never abort the whole scan for one bad file. stdout stays
  clean for piping.

## 8. Edge cases & decisions

### 8.1 `.gitignore` is NOT honored by default
The sample data here is gitignored, and real work-tracking docs may live under
gitignored paths. Respecting ignores by default would silently hide the user's
data and make the tool look broken. Default: walk everything (extensions +
`--exclude` still apply); opt in to ignore semantics with `--respect-gitignore`.
Hidden directories (`.git`, `.obsidian`, dot-dirs) are skipped unless `--hidden`.

### 8.2 Template files
Template files (e.g. `samples/templates/*.md`) carry placeholder frontmatter
like `status: <draft | generated | synced>`. YAML parses that as the literal
string `<draft | generated | synced>`, so templates appear as ordinary rows with
placeholder values. Exclude them by scoping directories, `--exclude
'**/templates/**'`, or a `WHERE`. The tool does not special-case templates.

### 8.3 Leading-zero / type coercion (`prd: 010`)
YAML may load `prd: 010` as integer `10` (losing the zero) or as string `"010"`
depending on the parser. **A characterization test pins the actual behavior of
our `gray_matter` version and the coercion path**, and the docs note that
quoting in YAML (`prd: "010"`) forces a string. This is a shared-funnel
invariant (§10) that gets a test rather than a prose assurance.

### 8.4 Files with no frontmatter
Skipped entirely (not emitted as all-NULL rows). Only files with a frontmatter
block are records.

## 9. Design-for-extension: TTL cache (future, not built in v1)

A near-future feature (see `TODO.md`) will add a `.querymatter` vault marker
found by walking upward from the cwd, a user config file with a configurable
**TTL**, and a persisted cache so a directory is only re-scanned when its slice
is stale. v1 must not build this, but must leave these seams so it drops in
without touching the query or render layers:

1. **Directory-keyed record slices.** `store` holds records grouped by source
   directory, never one flat undifferentiated pile. "Re-scan directory X,
   overwrite its records" is a single slice replacement — the exact cache primitive.
2. **`RecordStore` trait.** The executor and render layers depend only on
   `records()` / `schema()`. v1's eager in-memory store implements the trait; a
   future `CachedStore` implements the *same* trait with TTL + persistence, and
   nothing downstream changes.
3. **`.reload` is built on `reload_dir`.** v1 already exercises and tests the
   "re-scan one directory and overwrite its slice" path the cache will reuse — it
   is real, tested code, not a stub.
4. **Per-slice `scanned_at` timestamp**, populated in v1 even though nothing
   consumes it yet, so `now − scanned_at > ttl` staleness is computable later
   with no data-model change.
5. **Single root-resolution seam.** One function resolves the scan roots (v1: CLI
   dirs or cwd). The future `.querymatter` upward search and TTL-config read live
   there and only there.

## 10. Invariants this feature depends on
Per repo discipline, changes touching these funnels must re-verify the listed
producers with a test:
- **Frontmatter scalar coercion** (§4, §8.3): the mapping from YAML scalars to
  `Value` and the string/number comparison-coercion rule. Producers that must
  keep working if this changes: `WHERE status = 'draft'` (string eq), `WHERE prd
  = '010'` (leading-zero string), a numeric compare (`WHERE x > 2`). One test each.
- **`file.*` resolution** (§4): `file.folder`/`file.path` are relative to the
  scan root a file was found under. A change to path handling must keep
  `GROUP BY file.folder` and `SELECT file.name` correct — covered by an
  integration test over the sample tree.
- **stdout cleanliness**: warnings and prompts go to stderr; only query results
  go to stdout, so piping (`-e … | jq`) stays valid. A change that logs to stdout
  breaks piping — pinned by a one-shot JSON pipe test asserting stdout parses.

## 11. Testing strategy (TDD)
- **Unit — `query::parse`**: SQL string → AST for each clause, aliases, FROM-less
  and `FROM '<glob>'` forms, and rejection of unsupported clauses.
- **Unit — `query::exec`**: filter/group/aggregate/project/order/limit over
  hand-built record vectors; `COUNT(*)` vs `COUNT(col)` vs `COUNT(DISTINCT)`;
  NULL handling; non-grouped-column error.
- **Unit — `model`**: value coercion & comparison, including the §8.3
  characterization test and NULL-sorts-last.
- **Unit — `frontmatter`**: scalar record, no-frontmatter → None, invalid YAML →
  None + warning, list value → comma-joined render.
- **Unit — `render`**: snapshot (`insta`) of each format for a fixed `ResultTable`.
- **Unit — `store`**: `reload_dir` overwrites exactly one directory's slice and
  leaves others intact; `schema()` union order.
- **Integration** (`assert_cmd` + `predicates` + `insta`): fixtures copied into
  `tests/fixtures/` (from `samples/`). Cover: the headline `GROUP BY status`
  count with renamed header; `file.*` query; one-shot `-e` JSON output parses as
  JSON (stdout-clean invariant); batch mode via piped stdin; gitignored file
  still found by default; `--respect-gitignore` hides it.

## 12. Crate list
Edition 2024, clippy/rustfmt-clean, `Cargo.lock` committed.
- **`clap`** (derive) — CLI.
- **`ignore`** — directory walk (gitignore/hidden toggleable; off by default).
- **`gray_matter`** — frontmatter fence split + YAML block → dynamic value.
- **`sqlparser`** — parse the SQL string to an AST.
- **`comfy-table`** — table + Markdown-table rendering.
- **`serde` / `serde_json`** — JSON output; internal value interop.
- **`csv`** — CSV/TSV output.
- **`rustyline`** — REPL line editing + history.
- **`directories`** — XDG path for the history file.
- **`anyhow`** (binary boundary) + **`thiserror`** (query error types).
- **`globset`** (or `ignore`'s `overrides`) — `--exclude` / FROM glob matching.
- Dev: **`assert_cmd`**, **`predicates`**, **`insta`**, **`tempfile`**.

## 13. Future work (post-v1)
- `.querymatter` vault marker + upward search + user config file (`TODO.md`).
- TTL-based directory cache using the §9 seams; per-database TTL override.
- List/array membership operators and nested dotted-path access (§5).
- Possibly file-watching for live reload.
