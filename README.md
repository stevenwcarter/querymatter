# querymatter

`querymatter` is a single static Rust binary that reads YAML frontmatter from
Markdown files under one or more directories and runs a **SQL-subset query**
against the resulting record set, printing results as a formatted table (or
JSON / CSV / TSV / Markdown). It exists to answer questions like "how many of
my work-tracking docs are still `draft`?" without standing up Obsidian, a
SQLite index, or DuckDB — it's REPL-first, like the `sqlite3` or `mysql`
shell, with a one-shot `-e` mode for scripting and piping.

## Install

```sh
cargo install --path .
```

This builds and installs the `querymatter` binary from this repo.

## Modes

```
querymatter [OPTIONS] [DIRS]...
```

`[DIRS]...` are the directories to scan recursively (default: the current
directory). The query is never a positional argument — it's always `-e` or
piped stdin.

| Invocation | Behavior |
| --- | --- |
| `querymatter [dirs]` (stdin is a TTY) | Interactive REPL |
| `querymatter -e "SELECT …" [dirs]` | Run one query, print, exit |
| `querymatter --query - [dirs]` | Read the query text from stdin, run once, exit |
| `… \| querymatter [dirs]` (stdin piped, no `-e`) | Batch mode: run each statement from stdin in turn (ended by `;` or `\G`), no prompt |
| `querymatter init [dir]` | Build a `.querymatter/` cache over `dir` (default cwd) — see [Caching large vaults](#caching-large-vaults-querymatter) |

`-e`/`--query` always wins if given; otherwise piped (non-TTY) stdin means
batch mode; otherwise you get the REPL.

Batch mode prints results back to back with nothing identifying which
statement produced which — pass `--echo` to have each statement (comments
included) printed as a headline before its result, sqlite3 `.echo on`-style;
see [Flags](#flags) and `docs/sample-queries.md`.

**stdout carries data** — query results, `config list`/`get`/`path` output,
and completion scripts. Every diagnostic, warning, confirmation, and prompt
goes to stderr, so `querymatter -e '…' --format json | jq` always sees pure
JSON.

## The query DSL

A subset of SQL:

```
SELECT [DISTINCT] cols [AS alias] [FROM 'glob'] [WHERE ...] [GROUP BY ...] [HAVING ...] [ORDER BY ... [ASC|DESC]] [LIMIT n [OFFSET m]]
```

- **SELECT** — a comma-separated list of items, each optionally `AS <alias>`:
  a frontmatter field name (a dotted path like `estimate.low` reads into a
  nested mapping — see [Nested (dotted-path)
  columns](#nested-dotted-path-columns) below), `file.*` pseudo-column
  (below), `*` (every frontmatter key seen, in sorted order), a scalar
  expression (below), or an aggregate: `count(*)`, `count(col)`, `count(distinct
  col)`, `min`, `max`, `sum`, `avg`, `group_concat`. `SELECT DISTINCT` drops
  duplicate output rows (after projection, before `ORDER BY`); it cannot be
  combined with `GROUP BY` or with an aggregate (either way the query already
  yields one row per group).
- **Scalar expressions** — usable in `SELECT` and on either side of a `WHERE`
  comparison: arithmetic `+ - * / %`, string concat `||`, and the functions
  `lower(s)`, `upper(s)` (both Unicode-aware, not ASCII-only), `length(s)`,
  `trim(s)`, `ltrim(s)`, `rtrim(s)`, `substr(s, start[, len])` (1-based,
  clamped), `replace(s, from, to)`, and `DATE(s)`/`DATE(s, fmt)` — casts `s`
  to a date/datetime, `NULL` on failure (see [Dates](#dates) below).
  `COALESCE(a, b, ...)` is also supported — variadic, returning the first
  non-null argument (columns, literals, and nested expressions can all be
  arguments), e.g. `SELECT jira, COALESCE(epic, 'none') AS epic`. A `Null`
  or non-numeric operand to arithmetic, and divide/mod by zero, all yield
  `Null` rather than an error. Arithmetic is computed in `f64`, so an
  integer field beyond `f64`'s 53-bit exact range loses precision. An
  expression *containing* an aggregate (e.g. `count(*) + 1`) is not
  supported — mix them via `HAVING` instead.
- **FROM** — optional; when present its value is a path glob applied within
  the scanned directories (e.g. `FROM 'plans/**'`). Omit it and every
  discovered record is in scope.
- **WHERE** — a comparison (`= != <> < <= > >=`) between two scalar
  expressions (so `WHERE start < end` or `WHERE upper(status) = 'DRAFT'`
  work), plus `LIKE`/`NOT LIKE` (`%`/`_` wildcards), `[NOT] REGEXP
  '<pattern>'` (`RLIKE` is a synonym — sqlparser maps both keywords to the
  same node) for a case-sensitive regex match, backed by the `regex` crate,
  against **any scalar expression**, not just a bare column (e.g.
  `lower(status) REGEXP 'draft'`); an uncompilable pattern is rejected at
  parse time rather than on every row. Also `IN (...)`/`NOT IN (...)`, `IS
  NULL`/`IS NOT NULL`, and `[NOT] <expr> MEMBER OF(<col>)` for a
  list-valued field. **The tested side of `LIKE`, `IN`, `IS NULL`, and
  `MEMBER OF` is likewise a full scalar expression**, not just a bare
  column, so `WHERE lower(status) LIKE '%draft%'`, `WHERE trim(x) IS
  NULL`, and `WHERE lead MEMBER OF(tags)` (a column on the left —
  previously it had to be a literal) all work, e.g. `WHERE 'mobile' MEMBER
  OF(tags)` still does too — combined with `AND`, `OR`, `NOT`, and
  parentheses. A quoted string literal forces string comparison — unless
  it matches the [relative-date grammar](#relative-date-literals)
  (`'-7d'`, `'today'`, `'now'`, …), in which case it's resolved to a
  concrete date/timestamp before comparing; a bare numeric literal
  compares numerically.
- **GROUP BY** — one or more grouping keys, each a column or a `SELECT AS`
  alias that resolves to one (`GROUP BY <alias>`); every non-aggregate
  `SELECT` item must be composed entirely of grouping-key columns.
- **HAVING** — filters *groups* (evaluated after aggregation, before `ORDER
  BY`/`LIMIT`): a comparison between a grouping-key column or an aggregate
  and a literal (e.g. `HAVING count(*) > 1`, `HAVING status = 'draft'`),
  combined with `AND`/`OR`/`NOT`. The leaf may also be a `SELECT AS` alias
  that resolves to an aggregate or a grouping key (`count(*) AS n … HAVING n
  > 1`), mirroring `GROUP BY`/`ORDER BY` alias handling. The aggregate need
  not appear in `SELECT` — it's computed on demand from each group's rows.
  Requires `GROUP BY`.
- **ORDER BY** — a column, a `SELECT AS` alias, `file.*`, a bare aggregate
  call needing no alias (`ORDER BY count(*) DESC`, valid only alongside
  `GROUP BY`), or any other computed expression — arithmetic, `CASE`,
  `COALESCE`, a scalar function call (see the boundary noted below) — each
  with optional `ASC`/`DESC`. NULLs sort last regardless of direction.
- **`CASE`** — `CASE WHEN cond THEN val [WHEN cond THEN val ...] [ELSE val]
  END` (the *searched* form; each `WHEN` is a full `WHERE`-style condition —
  comparisons, `LIKE`, `REGEXP`, `IN`, `MEMBER OF`, `AND`/`OR`/`NOT`) or `CASE
  expr WHEN val THEN val2 ... [ELSE val] END` (the *simple* form; each
  `WHEN` value is compared against `expr` for equality). The first matching
  arm wins; a missing `ELSE` yields `Null` when none match. Usable in
  `SELECT`, `WHERE`, and `ORDER BY` — e.g. `SELECT CASE WHEN status =
  'draft' THEN 'D' ELSE 'S' END AS c`.
- **LIMIT n [OFFSET m]**.

### Boundaries worth knowing

A few spots where this subset stops short of full SQL:

- **`ORDER BY`** accepts arbitrary computed expressions (`ORDER BY CASE WHEN
  status = 'draft' THEN 0 ELSE 1 END`, `ORDER BY length(status) + 1`) in
  addition to a column/alias/bare-aggregate — but a **bare, top-level scalar
  function call** is still rejected: `ORDER BY <fn>(...)` is tried as an
  aggregate first, so `ORDER BY upper(status)` errors naming the function
  `upper` rather than falling through. Wrap it in parentheses (`ORDER BY
  (upper(status))`) or alias it in `SELECT` (`SELECT upper(status) AS s ...
  ORDER BY s`) instead.
- **`HAVING`** only compares a leaf (grouping-key column or aggregate)
  against a literal — never aggregate-vs-aggregate (`HAVING count(*) <
  sum(n)`) and never a scalar function.
- **`GROUP BY`** keys must be plain columns (directly, or via a `SELECT AS`
  alias on a plain column) — an alias on a computed expression or an
  aggregate is not a valid grouping key.
- **`DISTINCT` + grouping** is rejected — both an explicit `GROUP BY` and an
  aggregate `SELECT` item (`SELECT DISTINCT count(*)`, which groups every row
  into one implicit group). A grouped query already produces one row per
  group.

### Headline example

```sql
SELECT status, count(*) AS Count WHERE prd = '010' GROUP BY status ORDER BY Count DESC
```

```
| status | Count |
|--------|-------|
| synced | 2     |
| draft  | 1     |
```

### Nested (dotted-path) columns

A dotted identifier walks into a nested YAML mapping, one segment per level:

```sql
SELECT estimate.low WHERE estimate.high > 10
```

It works anywhere a column is valid: `SELECT`, `WHERE`, `GROUP BY`, `ORDER
BY`, `HAVING`. A missing key, or a non-map value partway down the path,
resolves to `NULL` rather than erroring. Nested maps render as a compact
`{high: 10, low: 5}` string under `table`/`csv`/`tsv`/`md` output, and as a
real nested JSON object under `--format json`.

[Unknown-column validation](#unknown-column-validation) (below) checks only
the **top-level** segment (`estimate`) — sub-keys vary file to file and
aren't validated.

### `file.*` pseudo-columns

Resolved from the file path itself, independent of frontmatter, and always
available alongside frontmatter fields:

| Column | Meaning |
| --- | --- |
| `file.name` | file name with extension, e.g. `DCP-459.md` |
| `file.path` | path as discovered, relative to the scan root it was found under |
| `file.folder` | the parent-directory portion of `file.path` |
| `file.ext` | extension without the dot, e.g. `md` |
| `file.mtime` | modification time as an ISO-8601 UTC string, e.g. `2026-07-20T10:30:00Z` (sorts and compares lexicographically) |
| `file.size` | file size in bytes, as an integer |
| `file.word_count` | word count of the Markdown body after the frontmatter fence |
| `file.body` | the Markdown body after the frontmatter fence, as text |

`file.mtime`, `file.size`, and `file.word_count` all come from the same stat
and parse querymatter already does for every record — reading them costs no
extra I/O, cache or no cache.

`file.body` is different: it's read from disk **lazily, at query time**,
only for a query that actually references it (in `SELECT`, `WHERE`, or
anywhere else), rather than being materialized for every record up front.
That makes it the one `file.*` column with a real caveat — **under
`--force-cache`** (which promises zero filesystem access for the whole run),
or whenever the underlying file has moved, been deleted, or changed since it
was cached, `file.body` can't be read and resolves to `NULL` rather than a
wrong or stale answer. (Strict mode fails the whole query up front instead,
naming `--force-cache`, so a `NULL` you didn't expect is never silently
mistaken for "the file has an empty body"; pass `--lenient` to get the
per-row `NULL` instead.) A file larger than `max_file_bytes` (see
[Configuration](#configuration)) resolves to `NULL` the same way, rather than
being read into memory.

```sql
SELECT file.name, file.word_count WHERE file.body REGEXP 'TODO|FIXME'
```

### Dates

A frontmatter scalar that's a **strict ISO date or datetime** — `YYYY-MM-DD`
(non-zero-padded forms like `2026-7-4` count too, since they're unambiguous)
or a full RFC3339 timestamp — is auto-detected at ingest and compared
**chronologically**, not lexicographically: `WHERE created < updated` sorts
and filters by calendar order, not string order. A partial form (`2026`,
`2026-07`) or anything else that isn't a clean ISO date stays a plain
string, no change in behavior. A field that's a date in some files and an
ordinary string in others (a stray `'TBD'`, say) still sorts sensibly — the
comparison falls back to the values' ISO text, which orders identically to a
chronological compare for well-formed dates.

`DATE(x)` / `DATE(x, '<chrono-format>')` casts `x` to a date, for a
non-ISO-formatted field: with no format argument it tries strict
`%Y-%m-%d` then RFC3339 (the same detection ingest applies); with one, `x`
is parsed against that [chrono strftime
pattern](https://docs.rs/chrono/latest/chrono/format/strftime/index.html)
instead. An already-`Date`/`DateTime` value passes through unchanged;
anything `DATE()` can't parse yields `NULL` rather than an error.

```sql
SELECT jira, DATE(reported, '%m/%d/%Y') AS reported_on ORDER BY reported_on
```

### Relative-date literals

A quoted string literal in a comparison may be a **relative date**, resolved
against the current date/time when the query runs, instead of an ordinary
string:

- `'today'` — today's date, `YYYY-MM-DD`.
- `'now'` — the current instant, full `YYYY-MM-DDTHH:MM:SSZ` timestamp.
- `'[+-]<int>(d|w|mo|y)'` — an offset from today: `'-7d'` (7 days ago),
  `'+3w'` (3 weeks from now), `'-2mo'` (2 calendar months ago — calendar
  arithmetic, not a fixed 30-day span), `'-1y'`. The sign is required.

```sql
WHERE created >= '-7d'
WHERE updated < 'today'
```

A relative-date literal always resolves to an ISO-8601 string, so it
compares correctly whichever way the other side happens to be represented:
chronologically against an auto-detected [`Date`/`DateTime`
field](#dates), and lexicographically against a plain ISO string like
`file.mtime` — both orderings agree for well-formed ISO text. A quoted
string that doesn't match this grammar (`'draft'`) stays an ordinary string
literal — no change in behavior.

### Unknown-column validation

**Behavior change:** a typo'd column name (`SELECT staus` for `status`) is
now a **hard error by default**, naming the offending column and, when one is
close enough, suggesting the nearest real one:

```console
$ querymatter -e "SELECT staus" notes/
querymatter: failed to execute query: SELECT staus: unknown column `staus`, did you mean 'status'?
```

This checks every column position — `SELECT` (including inside a scalar
function or aggregate argument), `WHERE`, `GROUP BY`, `ORDER BY`, `HAVING`,
and `MEMBER OF`'s column — against the schema (the union of frontmatter field
names across the scanned records). An empty schema (a fresh or empty vault,
or one whose only records have an explicit-but-empty frontmatter mapping)
skips the check entirely, so it can't fail every query on that account alone.

**Subtree-scoped queries validate against the subtree's schema, not the
vault's.** When a `.querymatter`-backed query names a subtree — a positional
`[DIRS]` argument (not the SQL `FROM 'glob'` clause, which filters after
loading) — in one-shot (`-e`), piped-batch, or `query run` mode, querymatter
loads only that subtree from the cache, so query cost tracks the subtree's
size rather than the whole vault's. The tradeoff: the schema used for
validation (and the did-you-mean suggestion) is built from that subtree
alone, so `SELECT foo` against `plans/` errors as unknown if `foo` exists
only under `product/`, even though the vault as a whole has it. This is
intentional — the query only reads that subtree. `--lenient` still bypasses
validation entirely if you'd rather not deal with it. The REPL is
unaffected: it keeps a whole-vault store across queries, so it always
validates against the whole vault regardless of subtree.

Pass `--lenient` (or set the `lenient` config key) to restore the old
behavior — an unknown column silently reads as `NULL` throughout. `--no-lenient`
forces strict validation for one invocation, overriding a configured `lenient
= true`.

## Flags

| Flag | Meaning |
| --- | --- |
| `-e, --query <QUERY>` | One-shot mode; `-` reads the query text from stdin. May contain several statements, each ended by `;` (or `\G`, which prints every row as a block of `name: value` lines instead of a table). |
| `--format <FMT>` | `table` (default), `json`, `csv`, `tsv`, or `md`. In the REPL this is just the *initial* format — `.format` changes it live. |
| `--table-style <STYLE>` | Border style for `--format table`: `ascii` (default), `unicode`, `compact`, or `plain`. Also settable per-shell with `QUERYMATTER_TABLE_STYLE`; the flag wins. Ignored by `json`/`csv`/`tsv`/`md`. In the REPL this is just the *initial* style — `.style` changes it live. |
| `--output <PATH>` | Write query results to `PATH` instead of stdout (one-shot/batch mode only). See [Redirecting output](#redirecting-output---output). |
| `--echo` | Echo each statement — including its leading `--` comments — before its result, with a blank line after (one-shot/batch/`query run`). In the REPL this is just the *initial* echo state — `.echo` changes it live. |
| `--lenient` | Disable unknown-column validation — an unknown column reads as `NULL` instead of failing the query. Off by default — see [Unknown-column validation](#unknown-column-validation). |
| `--no-lenient` | Force strict unknown-column validation, overriding a config `lenient = true`. |
| `--header` | Force the header row on in table/csv/tsv/md output, overriding a config `header = false`. On by default. |
| `--no-header` | Suppress the header row in table/csv/tsv/md output. |
| `-q, --quiet` | Suppress the non-error stderr chatter a query run emits (skipped/unparsable-file warnings); errors are always shown. |
| `--no-quiet` | Force chatter on, overriding a config `quiet = true`. |
| `--ext <LIST>` | Comma-separated extensions to include. Default `md,markdown`. |
| `--respect-gitignore` | Honor `.gitignore`/`.ignore` while walking. **Off by default** — see below. |
| `--no-respect-gitignore` | Ignore `.gitignore`/`.ignore` rules, overriding a config `respect_gitignore = true`. |
| `--hidden` | Descend into hidden files/directories (e.g. `.git`, `.obsidian`). Off by default. |
| `--no-hidden` | Do not descend into hidden files/directories, overriding a config `hidden = true`. |
| `--exclude <GLOB>` | Path glob to skip. Repeatable, e.g. `--exclude '**/templates/**'`. |
| `--ignore-file <PATH>` | Apply a gitignore-style ignore file. Repeatable; applied in order after the auto-discovered cwd `.querymatterignore`. |
| `--no-ignore-file` | Skip auto-discovering `.querymatterignore` in the current directory. Explicit `--ignore-file`s still apply. |
| `--no-cache` | Ignore any ancestor `.querymatter/` cache; always live-scan (today's default when no cache exists). |
| `--force-cache` | Trust the `.querymatter/` cache verbatim, with **no** filesystem access. Errors if no cache is found. |
| `--fast` | Use the dir-mtime + TTL hybrid freshness check instead of the accurate per-file default. |
| `--refresh <PATH>` | Force a re-scan of `PATH`'s subtree before querying, updating the cache. Repeatable. |
| `--refresh-all` | Force a re-scan of the whole vault before querying, updating the cache. |
| `--exit-code` | Grep-style 0/1/2 exit status instead of the default 0-unless-erroring behavior. Query mode and `query run` only. See [Exit codes for scripting](#exit-codes-for-scripting---exit-code). |

See [Caching large vaults](#caching-large-vaults-querymatter) below for what
these mean and for `querymatter init`'s own flags (`--ttl`, plus the walk
flags above, which `init` shares).

## Exit codes for scripting (`--exit-code`)

By default a clean run — even one that matched zero rows — exits `0`; only a
genuine error (bad SQL, an IO problem, a missing directory) exits `1`. Pass
`--exit-code` for grep-style semantics instead:

| Exit code | Meaning |
| --- | --- |
| `0` | The query matched at least one row. |
| `1` | The query ran cleanly but matched no rows. |
| `2` | A parse/exec/IO error. |

For `-e`/piped batch input containing several `;`-separated statements, row
counts are summed across every statement in the run — exit `1` only when
**every** statement matched zero rows; if any one statement matched at least
one row, the whole run exits `0`.

`--exit-code` only applies to **query mode** (no subcommand, `-e`/piped
stdin/the REPL's non-interactive paths) and **`query run`** (which resolves a
saved name to SQL and runs it exactly like `-e` would). Every other
subcommand — `init`, `config …`, `cache status`, `query
save`/`list`/`get`/`delete`, `explain`, `completions` — keeps today's plain
"error exits 1, success exits 0" behavior regardless of `--exit-code`; those
aren't query-result concepts the 0/1/2 mapping has an analog for.
`--exit-code` has no effect on the interactive REPL itself, which has no
single "total rows for the whole session" to report.

```console
$ querymatter -e "SELECT status WHERE status = 'archived'" --exit-code; echo $?
1
$ querymatter -e "SELECT status WHERE status = 'draft'" --exit-code; echo $?
0
$ querymatter -e "SELECT (" --exit-code; echo $?
2
```

## Redirecting output (`--output`)

`--output <PATH>` writes query results to a file instead of stdout, for
one-shot (`-e`) and batch (piped-stdin) mode:

```sh
querymatter -e "SELECT status" --output results.txt
```

`PATH` is **truncated up front** — like a shell `>` redirect — before the
first statement runs, and every statement in the run appends to that same
handle; stdout stays completely empty. Because the file is cleared before any
query work happens, a run that fails partway through (a later statement in a
multi-statement `-e`, say) can leave `PATH` empty or holding only the
statements that ran before the failure — check the process exit code, not
just the file's existence, before trusting its contents.

**`--output` applies to one-shot/batch mode only; it is ignored in the
interactive REPL.** Use `.output` inside the REPL instead:

| Command | Effect |
| --- | --- |
| `.output <path>` | Truncate/open `<path>`; every later statement's result is appended there instead of printed. |
| `.output \|<cmd>` | Pipe every later statement's result through `<cmd>` via the shell (the `sqlite3` convention), e.g. `.output \|less` or `.output \|column -t`. |
| `.output` / `.output stdout` | Reset: later results print to stdout again. |

```
querymatter> .output results.txt
querymatter: writing results to results.txt
querymatter> SELECT status;
querymatter> .output stdout
querymatter: results on stdout
```

A `.output <path>` that can't be opened for writing, or a `.output |<cmd>`
that can't be spawned, reports the error on stderr and leaves the session
writing wherever it already was — a live piped command keeps running and
accepting writes exactly as before the failed switch. A `.output |<cmd>`
spawns `<cmd>` once and keeps its stdin open across every later statement
until the next `.output` switch (or REPL exit) closes and reaps it:

```
querymatter> .output |less
querymatter: piping results through less
querymatter> SELECT status;
querymatter> .output stdout
querymatter: results on stdout
```

## Saved queries (`querymatter query`)

Save SQL under a short name once, then re-run it by name instead of retyping
it — handy for a query you run often (e.g. "everything still `draft`").

| Command | Meaning |
| --- | --- |
| `query save <NAME> <SQL>` | Save `SQL` under `NAME`, overwriting any query already saved under that name. Rejected up front, naming the parse error, if `SQL` fails to parse — a saved query can never be one that only breaks later at `query run`. |
| `query list` | List every saved query's name and SQL, one `name<TAB>sql` line each. |
| `query get <NAME>` | Print one saved query's SQL. |
| `query run <NAME> [DIR]` | Resolve `NAME` to its saved SQL and run it — see below. |
| `query delete <NAME>` | Remove a saved query. Deleting a name that was never saved is reported on stderr, not an error. |

`NAME` may contain letters, digits, `_`, and `-` only.

Saved queries live in their own file, **`queries.toml`**, in the same config
directory as `config.toml` (`querymatter config path`'s directory) but never
merged into it — a malformed `queries.toml` only blocks `query` actions, never
an ordinary query, and vice versa. `query save`/`list`/`get`/`delete` read and
write only `queries.toml`, so they keep working even when `config.toml` is
broken — mirroring `config path`'s and `completions`' own resilience (see
[Shell completions](#shell-completions)) — while `query run` builds a session
like a normal query, so it does need a valid `config.toml`.

`query run <NAME> [DIR]` resolves `NAME`'s saved SQL and executes it through
the exact same machinery as `-e` (fed the resolved SQL as the query text), so
it honors `--format`/`--table-style`/`--output`/`--exit-code` and every walk
flag exactly like a one-shot query would. The optional `[DIR]` positional
overrides the scan root, exactly like a single positional `[DIRS]` entry in
query mode; omit it to keep the usual cwd/vault behavior.

```console
$ querymatter query save drafts "SELECT status WHERE status = 'draft'"
querymatter: saved query 'drafts' in ~/.config/querymatter/queries.toml

$ querymatter query run drafts --format csv
status
draft
```

Inside the REPL, `.query run <name>` runs a saved query in-session (splitting
a multi-statement saved query and running each statement in turn, exactly
like typing them one after another), `.query list` lists every saved name and
its SQL, and `.query save <name> [sql]` saves `sql` — or, with `sql` omitted,
the last successfully-run statement of this session — under `name`, using the
exact same validation as `querymatter query save` (a bad name or unparseable
SQL is rejected the same way; nothing is written). `.query save` with
neither an inline `sql` nor a prior statement this session is a clean error;
nothing is saved. See the dot-commands table below. `query get`/`delete`
remain CLI-only. Tab-completion offers saved-query names right after
`.query run ` (not yet after `.query save`).

## Diagnosing exclusions (`querymatter explain <path>`)

`querymatter explain <path>` reports whether `path` would be discovered by a
real query — and when it wouldn't, **which filter layer is responsible**:

```console
$ querymatter explain notes/draft.md
included

$ querymatter explain notes/archive.txt
excluded: extension 'txt' not in --ext (md, markdown)

$ querymatter explain .obsidian/workspace.md
excluded: hidden directory '.obsidian' (pass --hidden to include)
```

The verdict is ground truth: it's exactly whether a live scan under the same
`--ext`/`--hidden`/`--exclude`/etc. flags would find `path`, so it can never
disagree with a real query run. `explain` roots its scan at the ancestor
`.querymatter` vault when one exists — the same vault a real query from this
directory would use — or the current directory when there is none; `path`
must resolve under that root, or `explain` reports a clean error rather than a
silently wrong verdict. Like a real query, `explain` always does a **live
scan** — it never consults a `.querymatter` cache's contents, only its
location (to find the vault root).

## Ignoring files (`.querymatterignore`)

`querymatter` will skip files matched by a `.querymatterignore` in the
current directory, if one exists. It uses **gitignore syntax** — one pattern
per line, `#` comments, and `!` to re-include a path a broader pattern
excluded:

```gitignore
templates/*
!templates/keep-this.md   # re-include one file (works because templates/ itself isn't excluded)
*.draft.md
```

Note: a `!` negation can't re-include a file whose parent directory is
excluded — exclude the directory's contents (`dir/*`) rather than the
directory (`dir/`) when you want to un-ignore something inside it.

A few things worth knowing:

- **Always honored.** Unlike `.gitignore`, which `querymatter` only reads
  when you pass `--respect-gitignore`, a `.querymatterignore` applies
  unconditionally whenever it's found.
- **Auto-discovered** as `.querymatterignore` in the current directory. A
  `.querymatter/` vault's cache was itself built honoring whichever
  `.querymatterignore`/`--ignore-file`s were in effect at `init` time — see
  [Caching large vaults](#caching-large-vaults-querymatter) below.
- `--ignore-file <PATH>` applies additional ignore files, in the order
  given, after the cwd file. Repeat it to layer several.
- `--no-ignore-file` disables the cwd auto-discovery; any explicit
  `--ignore-file`s you pass still apply.
- It composes with `--exclude <GLOB>` (ad-hoc globs) and
  `--respect-gitignore` (`.gitignore`/`.ignore` rules) — all three filters
  apply together.
- **Anchoring.** The cwd `.querymatterignore` governs content under the
  current directory; a scan root *outside* cwd is not governed by it (its
  non-anchored patterns still match by name, but cwd-anchored `/…` patterns
  won't apply elsewhere).

## Sample data & sample queries

The repo ships a deterministic sample-vault generator as a second binary:

```sh
cargo run --bin querymatter-samples -- --scale 1k samples
```

This writes exactly 1000 Markdown files (`--scale 10k` / `--scale 100k` for
10,000 / 100,000) into `samples/` (gitignored): a fixed `starwars/` folder —
the classic GraphQL star-wars cast, identical at every scale — plus three
scaled themes (`work/`, `recipes/`, `reading/`). Generation is fully
deterministic: wiping the directory and regenerating from the same build
produces byte-identical files, mtimes included. A non-empty target directory
is refused unless you pass `--force` (which deletes and regenerates it).

[`docs/sample-queries.md`](docs/sample-queries.md) walks through queries
exercising most of the DSL against this tree, with expected output;
[`docs/sample-queries.sql`](docs/sample-queries.sql) is the runnable version
(`querymatter --echo samples < docs/sample-queries.sql`), pinned by an integration
test so the examples can never silently rot. The 100k scale plus
`querymatter init samples` is a quick way to feel the cache speedup on a
large vault. Passing `--echo` causes the comments and queries to be printed
from the batch file, making it easier to determine what each query is doing.

## Caching large vaults (`.querymatter`)

Re-walking and re-parsing every file on every run gets slow once a vault has
thousands of files. `querymatter init [DIR]` builds a persistent, on-disk
cache — a `.querymatter/` directory — over everything under `DIR` (default:
the current directory), honoring the same walk flags as a normal query
(`--ext`, `--respect-gitignore`, `--hidden`, `--exclude`, `--ignore-file`,
`--no-ignore-file`). It's re-runnable: running `init` again does a full
rebuild.

```sh
querymatter init ~/notes
querymatter init ~/notes --ttl 60   # shorten the TTL --fast consults (default 300s)
```

A cache written by an older, schema-incompatible `querymatter` is treated
exactly like a missing one for a normal (non-`--force-cache`) query: the
next run does one full live rebuild and re-persists it in the current
format, printing a one-line stderr warning explaining why that run was
slower — no manual `querymatter init` needed. (`querymatter cache status`,
which only ever reads the cache rather than rebuilding it, still hard-errors
naming `init` for the same incompatible cache — see [Inspecting the
cache](#inspecting-the-cache-querymatter-cache-status) below.)

### Automatic discovery

A normal query run (no subcommand) searches **upward** from the current
directory for an ancestor containing `.querymatter/`. If found, that directory
becomes the query's **vault base** and the query runs against the cache
instead of a live scan; if not found, behavior is unchanged — a live scan of
the positional `[DIRS]` (or cwd). Pass `--no-cache` to skip vault discovery
entirely and always live-scan.

When a vault is in use, a positional `[DIRS]` argument restricts the query to
records under those subtrees instead of triggering a live scan of them — see
the limitation below.

### Freshness modes

A cached run still needs to notice files that changed since `init` or the
last refresh. Four modes trade accuracy for speed:

| Mode | Behavior |
| --- | --- |
| *(default)* | Accurate per-file check: stat every cached file's `(mtime, size)` and re-parse only what changed; new files are picked up, deleted files are dropped. |
| `--fast` | Dir-mtime + TTL hybrid: skip per-file stats for a directory whose mtime is unchanged and was scanned within the cache's TTL. Faster on huge vaults; can miss an in-place edit that lands within the TTL window. |
| `--force-cache` | Trust the cache verbatim — **no filesystem access at all**. Fails with a clear error if no `.querymatter/` is found. |
| `--refresh <PATH>` / `--refresh-all` | Force a full re-scan (ignoring every freshness shortcut) of `PATH`'s subtree, or the whole vault, before querying, and persist the update. `--refresh` is repeatable. |

`--no-cache` bypasses caching altogether (today's live-scan behavior); it
can't be combined with `--force-cache`/`--refresh`/`--refresh-all`, since none
of those mean anything without a cache.

### REPL

Inside a vault-backed REPL session, `.refresh [path]` and `.refresh-all` do
the same forced re-scan as their CLI counterparts, persisting the update to
`.querymatter/` before returning to the prompt refreshed. `.reload` remains as
the in-memory-only rescan (it never touches the cache) — see the dot-commands
table below.

### The `.gitignore` prompt

Right after a successful `init` inside a git working tree, if `.querymatter`
isn't already ignored: an interactive (TTY) session is asked
`Add .querymatter/ to .gitignore? [y/N]` on stderr, and an affirmative answer
appends a `.querymatter/` line. A non-interactive run (piped stdin — e.g. in a
script or CI) never touches `.gitignore`; it prints a one-line stderr hint
instead.

### Inspecting the cache (`querymatter cache status`)

`querymatter cache status [DIR]` reports an existing `.querymatter` cache's
health: its root and on-disk location, how many directories/files it covers,
its size on disk, its TTL, the crate version it was built with, and each
cached directory's last-scanned time — printed to stdout, like `config list`.
`DIR` defaults to the current directory; the vault is found the same way a
real query finds one (searching upward for an ancestor `.querymatter/`).

```console
$ querymatter cache status ~/notes
root:        /home/steve/notes
cache dir:   /home/steve/notes/.querymatter
directories: 3
files:       128
size:        42.1 KiB
ttl:         300s
built with:  querymatter 0.1.0

directories scanned:
  /home/steve/notes/plans            2026-07-20 10:30
  /home/steve/notes/product/stories  2026-07-20 10:30
```

It errors — naming `querymatter init` as the fix — both when no
`.querymatter` cache is found at or above `DIR`, and when one is found but
unreadable (corrupt, or written by an incompatible crate version).

### Limitation

Positional `[DIRS]` restrict a vault query at **directory granularity** — a
directory that isn't already part of the cached vault matches nothing; it is
not live-scanned as a fallback. Point `init` at the tree you want covered, or
pass `--no-cache` for an ad-hoc scan outside it.

### Performance

Two optimizations are transparent — they change speed, never results. A
large-vault scan (`init`, a live scan, or a cache refresh) spreads each file's
read+parse across all available cores instead of one, in deterministic
path-sorted order regardless of thread timing. And a narrow one-shot/batch/
`query run` query (`-e`, piped stdin, or `query run`) — e.g. `SELECT
count(*)` or `SELECT status` — materializes only the frontmatter fields it
actually references, skipping the rest; the full schema (every field name, for
column validation and `SELECT *`) is still tracked regardless.

A third optimization, subtree-scoped cache loading (a positional `[DIRS]`
argument in one-shot/batch/`query run` mode), is deliberately **not** listed
as transparent above — it changes what unknown-column validation sees. See
[Unknown-column validation](#unknown-column-validation) for that tradeoff.

## Configuration

Persistent settings live in a single user-global TOML file. `querymatter config
path` prints its location — on Linux that is
`~/.config/querymatter/config.toml`.

```toml
format            = "table"     # table, json, csv, tsv, md
table_style       = "unicode"   # ascii, unicode, compact, plain
ext               = ["md", "markdown"]
respect_gitignore = true
hidden            = false
exclude           = ["**/templates/**"]
lenient           = false
timer             = false
header            = true
quiet             = false
max_file_bytes    = 8388608     # 8 MiB; caps file.body and per-file scanning
```

Every key is optional; an absent key falls through to the next layer. Values
resolve per key, independently:

```
flag  >  environment  >  vault config  >  config file  >  built-in default
```

`vault config` is a `.querymatter.toml` a team can commit at the vault
root, spliced in between the environment and this per-user file — see
[Vault-level config](#vault-level-config-querymattertoml) below.

So a configured `hidden = true` still scans hidden files when you pass no flag,
and `--no-hidden` turns it back off for one run. Likewise a configured
`lenient = true` still tolerates an unknown column when you pass no flag, and
`--no-lenient` turns it back to strict for one run. `--table-style`
additionally reads `QUERYMATTER_TABLE_STYLE`, which outranks the file but
loses to the flag.

`header` and `quiet` follow the same rule: a configured `header = false` still
suppresses the header row when you pass no flag, and `--header` turns it back
on for one run (`--no-header`/`--no-quiet` work the same way in reverse).
`timer` has no CLI flag at all — `config set timer true` is the only durable
way to turn it on. Either way, a REPL session's own `.header [on|off]` /
`.timer [on|off]` (see [REPL dot-commands](#repl-dot-commands)) toggle just
that session, on top of whatever the flag/config layers already resolved.

`max_file_bytes` also has no CLI flag — `config set max_file_bytes <N>` is the
only durable way to change it. It caps the largest file (in bytes) querymatter
will read into memory, both while scanning/`init`-ing (a larger file is
skipped with a warning, exactly like invalid frontmatter) and while resolving
`file.body` (a larger file resolves to `NULL`, exactly like an unreadable
file) — a defense against a single multi-gigabyte or padded file in a scanned
vault exhausting memory. The built-in default is 8 MiB, generous enough that
an ordinary Markdown note is never affected.

| Command | Meaning |
| --- | --- |
| `config list` | Every setting, its resolved value, and which layer supplied it. |
| `config get <KEY>` | One setting's value, then the values it accepts. |
| `config set <KEY> <VALUE>` | Write the setting to the config file. `ext` and `exclude` take a comma-separated list. |
| `config unset <KEY>` | Remove the setting, returning it to the next layer. |
| `config path` | Print the config file's path, whether or not it exists. |

```console
$ querymatter config set table_style unicode
querymatter: set table_style = unicode in ~/.config/querymatter/config.toml

$ querymatter config list
format             table        (default)
table_style        unicode      (config)
ext                md,markdown  (default)
respect_gitignore  false        (default)
hidden             false        (default)
exclude            (none)       (default)
lenient            false        (default)
timer              false        (default)
header             true         (default)
quiet              false        (default)
max_file_bytes     8388608      (default)
```

A key supplied by a `.querymatter.toml` above the current directory shows
its layer as `vault` in this same listing.

A malformed config file, an unknown key, or an invalid value is a hard error
naming the file — a typo must not silently do nothing. The file is read once
per session, at resolution time: a `config set` run in another shell cannot
change an already-running session's resolved settings. (Inside a single
session, the REPL's `.set`/`.unset` re-read the file on every call rather than
a cached snapshot, so a prior `.set` to a sibling key earlier in that same
session survives a later one — but that governs what gets written to the
file, not what a different, already-running session resolved.)

`config set`/`config unset` rewrite the whole file from the parsed settings,
so any comments, blank lines, or key order you added by hand are **not**
preserved. If you hand-edit `config.toml` (e.g. to add the comments in the
example above), keep documentation like that in a separate note rather than
relying on it surviving the next `config set`/`config unset`. (These
commands only ever touch the per-user file — never the vault-level one,
which is meant to be edited by hand and committed like any other shared
config.)

### Vault-level config (`.querymatter.toml`)

A team can commit shared defaults at the vault root as `.querymatter.toml`,
using the **same schema** as the per-user `config.toml` above (the same
keys, the same strict "unknown key is an error" rule). It's discovered by
walking **upward** from the current directory — the same direction (and
starting point) `.querymatter/` cache discovery walks, but independently: a
`.querymatter.toml` doesn't need a `.querymatter/` cache directory alongside
it, so a team can commit shared defaults before anyone has ever run `init`,
and an existing cache implies nothing about whether a `.querymatter.toml`
exists above it.

A key it sets outranks the per-user config file but still loses to a flag
or environment variable — see the precedence chain above. A missing
`.querymatter.toml` is not an error (resolution just falls through to the
per-user config/default layers); one that's found but fails to parse is a
hard error naming its path, exactly like a malformed per-user
`config.toml`. There's no `config`-style subcommand for it — it's meant to
be edited by hand and committed to version control, not written by
`config set`.

```toml
# .querymatter.toml, committed at the vault root
respect_gitignore = true
exclude           = ["**/templates/**"]
```

## Shell completions

`querymatter completions <SHELL>` prints a completion script to stdout for
`bash`, `zsh`, `fish`, `elvish`, or `powershell`. It completes subcommands,
flags, directories, and the allowed values of `--format`, `--table-style`, and
the `config` keys.

`querymatter completions --install [SHELL]` writes that same script directly
into the shell's user completion directory instead of printing it — a
one-step setup for `bash`/`zsh`/`fish`:

| Shell | Installed to |
| --- | --- |
| `bash` | `~/.local/share/bash-completion/completions/querymatter` |
| `zsh` | `~/.zsh/completions/_querymatter` — a directory of our own, since not every distro's default `${fpath[1]}` is user-writable; add `fpath=(~/.zsh/completions $fpath)` before `compinit` if it isn't already on your `$fpath` |
| `fish` | `~/.config/fish/completions/querymatter.fish` |

`SHELL` is optional with `--install` — it's auto-detected from `$SHELL`
(recognizing `bash`/`zsh`/`fish`; anything else needs it named explicitly).
Without `--install`, `SHELL` stays required, exactly as before.

`--install` is pure convenience, so nothing about it is a hard error:
whenever it can't determine your home directory, doesn't know a completion
directory for the shell (`elvish`/`powershell` have no such convention to
target), or hits a write failure (e.g. permissions), it prints a clear
reason to stderr and **falls back to printing the script to stdout** —
today's manual-redirect behavior, so you're never left empty-handed.

```sh
querymatter completions --install          # auto-detects $SHELL
querymatter completions --install zsh      # explicit shell

# the manual form still works for any of the five shells, e.g. to choose
# your own path or target elvish/powershell:
querymatter completions bash > ~/.local/share/bash-completion/completions/querymatter
querymatter completions fish > ~/.config/fish/completions/querymatter.fish
```

Completions — and `config path` itself — work even when the config file is
malformed, so you can always tab-complete your way to `querymatter config
path` and find the file worth fixing.

## REPL dot-commands

On entering the REPL, a startup banner reports the record count and points
you at `.help`/`.schema`:

```
$ querymatter
querymatter — 128 records. Type .help for commands, .schema for fields.
querymatter>
```

The banner is REPL-only — never printed by `-e` or piped batch mode, so
piping a query's output never picks up stray banner text.

Inside the REPL, a line starting with `.` (no trailing `;`) is a command
rather than SQL:

| Command | Meaning |
| --- | --- |
| `.help` | List the dot-commands. |
| `.schema` | List discovered frontmatter fields, the `file.*` columns, and the record count. |
| `.describe [field]` | With no argument, a one-line-per-field summary of every field's type and coverage. With `<field>`, that field's `Value` type(s), non-null coverage, and its most-frequent-first value list (or a bare distinct count, when there are too many distinct values to list). |
| `.format [fmt]` | Show, or set, the output format for subsequent queries. |
| `.style [style]` | Show, or set, the table border style (`ascii`, `unicode`, `compact`, `plain`) for subsequent queries. |
| `.header [on\|off]` | Show, or toggle, whether results include a header row (this session only). |
| `.timer [on\|off]` | Show, or toggle, whether the `-- N rows` line also reports elapsed query time (this session only). |
| `.echo [on\|off]` | Show, or toggle, whether each statement (comments included) is echoed before its result (this session only). |
| `.output [path\|stdout]` | Redirect subsequent results to `path` (truncating it first), pipe them through a shell command with `.output \|cmd`, or back to stdout with `.output`/`.output stdout`. See [Redirecting output](#redirecting-output---output). |
| `.settings` | List every setting, its resolved value, and which layer supplied it. |
| `.set <key> <value>` | Save a setting to the config file. Rendering settings (`format`, `table_style`) also apply immediately; scan settings take effect on the next run. |
| `.unset <key>` | Remove a setting from the config file. |
| `.reload` | Re-scan every tracked directory (in-memory only; never touches a `.querymatter` cache). |
| `.refresh [path]` | Force a re-scan of `path` (or the whole vault); updates the `.querymatter` cache when one is loaded, otherwise behaves like `.reload`. |
| `.refresh-all` | Force a re-scan of the whole vault; alias for `.refresh` with no path. |
| `.query run <name>` | Run a saved query in-session, honoring the current `.format`/`.style`/`.output`. See [Saved queries](#saved-queries-querymatter-query). |
| `.query list` | List every saved query's name and SQL. |
| `.query save <name> [sql]` | Save `sql` under `name`, or, when `sql` is omitted, the last successfully-run statement of this session. Same validation as `querymatter query save`. |
| `.quit` / `.exit` | Leave the REPL (Ctrl-D also exits; Ctrl-C cancels the current line). |

SQL statements may span multiple lines; a trailing `;` ends the statement and
runs it. Ending with `\G` instead runs it and prints each row as a block of
right-aligned `name: value` lines — the readable way to inspect a wide record,
as in `SELECT * LIMIT 1\G`. `\g` is accepted as a synonym for `;`. `\G`
overrides whatever `.format` is set to, and works in `-e` and piped batch mode
as well as the REPL.

Multiline values (a multiline frontmatter string, or `file.body`) render
their line breaks for real: as extra lines inside the cell in `--format
table`, raw MySQL-style continuation lines under `\G`, and `<br>` in
`--format md` (whose rows must each stay one physical line). Other control
characters in frontmatter (ESC, lone `\r`, …) are still neutralized to
`U+FFFD` in table/`\G` output so a hostile file can't forge terminal escape
sequences; json/csv/tsv always carry the raw value with their own escaping.

On a real terminal, `--format table` (and `.format table`) also fits the
table to the terminal's width automatically instead of overflowing off the
screen; piped or redirected output (a script, `--output`, `.output <path>`)
is unaffected and stays exactly as wide as its content, since an interchange
format has to be terminal-independent. For a single very wide record, `\G`
(above) is still the more readable option; piping through a pager works too,
e.g. `.output |less -S` inside the REPL.

`.format` and `.style` change the current session only; `.set format` and
`.set table_style` persist to the config file *and* apply immediately — so you
can try a setting, then keep it. `.header`/`.timer` are session-only the same
way, but their `.set`/`config set` counterparts are **deferred**, like any
scan setting: `.set header false`/`.set timer true`/`.set quiet true` persist
the default for future runs without changing what's already resolved for
*this* session — use `.header`/`.timer` directly to change this session's
behavior right now. `.echo` is session-only too, seeded from `--echo`'s
initial value, but has no `.set echo`/`config set echo` config-file
counterpart at all — turn it on for the run/session you want it in, via
`--echo` or `.echo on`.

After each REPL statement's result, a `-- N rows` line (singular for exactly
one row) is printed to stderr — a quick sanity check that distinguishes a
genuinely empty result from a typo'd `WHERE`. It's REPL-only, printed to
stderr rather than stdout, and never appears in `-e` or piped batch mode, so
it never corrupts piped output. With `.timer on` (or a configured `timer =
true`), the same line also reports the statement's wall-clock time in seconds
to three decimal places, e.g. `-- 3 rows (0.004s)`; `.timer off` (the
default) reproduces the untimed line exactly.

Tab-completion is available for: frontmatter column names and the `file.*`
pseudo-columns, in SQL position; dot-command names, right after a leading
`.`; config keys, right after `.set`/`.unset`; and saved-query names, right
after `.query run`. It does not complete SQL keywords (`SELECT`, `WHERE`, and
so on) — only schema-derived and dot-command/config-key/saved-query names.
The column and saved-query-name lists are snapshots, but `.reload`,
`.refresh`/`.refresh-all`, and `.query save` all refresh both of them live
right afterward — a field just discovered by that rescan, a query you just
saved (in this session or, since the refresh re-reads `queries.toml`, from
another shell too) tab-completes immediately, no REPL restart needed.

History records one entry per statement or dot-command, not per line: typing
a SQL statement across several lines (ended by `;` or `\G`) or a dot-command
still leaves exactly one entry, so pressing Up-arrow recalls the whole thing
rather than a fragment of it.

## Accuracy notes / gotchas

- **`.gitignore` is *not* honored by default.** Work-tracking docs often live
  under gitignored paths (this repo's own `samples/` does), and hiding them
  silently would make the tool look broken. Pass `--respect-gitignore` to opt
  in to ignore semantics.
- **Files with no frontmatter block are skipped entirely** — they never show
  up as an all-`NULL` row. A file whose frontmatter exists but fails to parse
  as YAML is also skipped, with a warning on stderr (stdout stays clean for
  piping) — silenced by `--quiet`/`-q` (see [Flags](#flags)). A file larger
  than `max_file_bytes` (default 8 MiB — see
  [Configuration](#configuration)) is skipped the same way, before it's ever
  read into memory.
- **Unquoted leading-zero YAML values parse as integers.** `prd: 010` loads
  as the integer `10`, not the string `"010"` — quote it (`prd: '010'`) if
  you need it to stay a string.
- **Overlapping scan roots.** Exact-duplicate directory arguments are
  de-duplicated after canonicalization, so `querymatter . .` scans once. A
  root that *contains* another (e.g. `querymatter . ./plans`) is not yet
  detected, so files under the nested root are scanned — and counted — twice;
  pass non-overlapping roots to avoid it.

## Design & roadmap

The full design spec is at
[`docs/superpowers/specs/2026-07-22-querymatter-design.md`](docs/superpowers/specs/2026-07-22-querymatter-design.md).
The `.querymatter/` cache/vault feature described above has its own design
doc at
[`docs/superpowers/specs/2026-07-23-cache-vault-design.md`](docs/superpowers/specs/2026-07-23-cache-vault-design.md).
`REGEXP`, dates, `file.body`/`file.word_count`, `completions --install`, and
the vault-level config layer (above) are specified in
[`docs/superpowers/specs/2026-07-25-query-power-bundle-design.md`](docs/superpowers/specs/2026-07-25-query-power-bundle-design.md).
Further planned work is tracked in [`TODO.md`](TODO.md).
