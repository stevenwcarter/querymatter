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
  a frontmatter field name, `file.*` pseudo-column (below), `*` (every
  frontmatter key seen, in sorted order), a scalar expression (below), or an
  aggregate: `count(*)`, `count(col)`, `count(distinct col)`, `min`, `max`,
  `sum`, `avg`, `group_concat`. `SELECT DISTINCT` drops duplicate output rows
  (after projection, before `ORDER BY`); it cannot be combined with `GROUP
  BY` (a grouped query already yields one row per distinct key).
- **Scalar expressions** — usable in `SELECT` and on either side of a `WHERE`
  comparison: arithmetic `+ - * / %`, string concat `||`, and the functions
  `lower(s)`, `upper(s)` (both Unicode-aware, not ASCII-only), `length(s)`,
  `trim(s)`, `ltrim(s)`, `rtrim(s)`, `substr(s, start[, len])` (1-based,
  clamped), `replace(s, from, to)`. A `Null` or non-numeric operand to
  arithmetic, and divide/mod by zero, all yield `Null` rather than an error.
  Arithmetic is computed in `f64`, so an integer field beyond `f64`'s 53-bit
  exact range loses precision. An expression *containing* an aggregate (e.g.
  `count(*) + 1`) is not supported — mix them via `HAVING` instead.
- **FROM** — optional; when present its value is a path glob applied within
  the scanned directories (e.g. `FROM 'plans/**'`). Omit it and every
  discovered record is in scope.
- **WHERE** — a comparison (`= != <> < <= > >=`) between two scalar
  expressions (so `WHERE start < end` or `WHERE upper(status) = 'DRAFT'`
  work), plus `LIKE`/`NOT LIKE` (`%`/`_` wildcards), `IN (...)`/`NOT IN
  (...)`, `IS NULL`/`IS NOT NULL`, and `[NOT] '<value>' MEMBER OF(<col>)` for
  a list-valued field (e.g. `WHERE 'mobile' MEMBER OF(tags)`) — combined with
  `AND`, `OR`, `NOT`, and parentheses. A quoted string literal forces string
  comparison; a bare numeric literal compares numerically.
- **GROUP BY** — one or more grouping keys, each a column or a `SELECT AS`
  alias that resolves to one (`GROUP BY <alias>`); every non-aggregate
  `SELECT` item must be composed entirely of grouping-key columns.
- **HAVING** — filters *groups* (evaluated after aggregation, before `ORDER
  BY`/`LIMIT`): a comparison between a grouping-key column or an aggregate
  and a literal (e.g. `HAVING count(*) > 1`, `HAVING status = 'draft'`),
  combined with `AND`/`OR`/`NOT`. The aggregate need not appear in `SELECT` —
  it's computed on demand from each group's rows. Requires `GROUP BY`.
- **ORDER BY** — a column, a `SELECT AS` alias, `file.*`, or a bare aggregate
  call needing no alias (`ORDER BY count(*) DESC`, valid only alongside
  `GROUP BY`) — each with optional `ASC`/`DESC`. NULLs sort last regardless
  of direction.
- **LIMIT n [OFFSET m]**.

### Boundaries worth knowing

A few spots where this subset stops short of full SQL:

- **`ORDER BY`** accepts a column, a `SELECT` alias, or a bare aggregate call
  — not an arbitrary scalar expression (`ORDER BY upper(status)` is
  rejected; add `SELECT upper(status) AS s ... ORDER BY s` instead).
- **`HAVING`** only compares a leaf (grouping-key column or aggregate)
  against a literal — never aggregate-vs-aggregate (`HAVING count(*) <
  sum(n)`) and never a scalar function.
- **`GROUP BY`** keys must be plain columns (directly, or via a `SELECT AS`
  alias on a plain column) — an alias on a computed expression or an
  aggregate is not a valid grouping key.
- **`DISTINCT` + `GROUP BY`** together are rejected; a grouped query already
  produces one row per distinct key.

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

### `file.*` pseudo-columns

Resolved from the file path itself, independent of frontmatter, and always
available alongside frontmatter fields:

| Column | Meaning |
| --- | --- |
| `file.name` | file name with extension, e.g. `DCP-459.md` |
| `file.path` | path as discovered, relative to the scan root it was found under |
| `file.folder` | the parent-directory portion of `file.path` |
| `file.ext` | extension without the dot, e.g. `md` |

### Unknown-column validation

**Behavior change:** a typo'd column name (`SELECT staus` for `status`) is
now a **hard error by default**, naming the offending column and, when one is
close enough, suggesting the nearest real one:

```
$ querymatter -e "SELECT staus" notes/
Error: failed to execute query: SELECT staus

Caused by:
    unknown column `staus`, did you mean 'status'?
```

This checks every column position — `SELECT` (including inside a scalar
function or aggregate argument), `WHERE`, `GROUP BY`, `ORDER BY`, `HAVING`,
and `MEMBER OF`'s column — against the schema (the union of frontmatter field
names across the scanned records). An empty schema (a fresh or empty vault,
or one whose only records have an explicit-but-empty frontmatter mapping)
skips the check entirely, so it can't fail every query on that account alone.

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
| `--lenient` | Disable unknown-column validation — an unknown column reads as `NULL` instead of failing the query. Off by default — see [Unknown-column validation](#unknown-column-validation). |
| `--no-lenient` | Force strict unknown-column validation, overriding a config `lenient = true`. |
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
subcommand — `init`, `config …`, `query save`/`list`/`get`/`delete`,
`explain`, `completions` — keeps today's plain "error exits 1, success exits
0" behavior regardless of `--exit-code`; those aren't query-result concepts
the 0/1/2 mapping has an analog for. `--exit-code` has no effect on the
interactive REPL itself, which has no single "total rows for the whole
session" to report.

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
| `.output` / `.output stdout` | Reset: later results print to stdout again. |

```
querymatter> .output results.txt
querymatter: writing results to results.txt
querymatter> SELECT status;
querymatter> .output stdout
querymatter: results on stdout
```

A `.output <path>` that can't be opened for writing reports the error on
stderr and leaves the session writing wherever it already was.

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
like typing them one after another) and `.query list` lists every saved name
and its SQL — see the dot-commands table below. `query save`/`get`/`delete`
are CLI-only. Tab-completion offers saved-query names right after
`.query run `.

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

### Limitation

Positional `[DIRS]` restrict a vault query at **directory granularity** — a
directory that isn't already part of the cached vault matches nothing; it is
not live-scanned as a fallback. Point `init` at the tree you want covered, or
pass `--no-cache` for an ad-hoc scan outside it.

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
```

Every key is optional; an absent key falls through to the next layer. Values
resolve per key, independently:

```
flag  >  environment  >  config file  >  built-in default
```

So a configured `hidden = true` still scans hidden files when you pass no flag,
and `--no-hidden` turns it back off for one run. Likewise a configured
`lenient = true` still tolerates an unknown column when you pass no flag, and
`--no-lenient` turns it back to strict for one run. `--table-style`
additionally reads `QUERYMATTER_TABLE_STYLE`, which outranks the file but
loses to the flag.

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
```

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
relying on it surviving the next `config set`/`config unset`.

## Shell completions

`querymatter completions <SHELL>` prints a completion script to stdout for
`bash`, `zsh`, `fish`, `elvish`, or `powershell`. It completes subcommands,
flags, directories, and the allowed values of `--format`, `--table-style`, and
the `config` keys.

```sh
# bash
querymatter completions bash > ~/.local/share/bash-completion/completions/querymatter

# zsh — anywhere on your $fpath (must be writable without sudo; not every
# distro's default ${fpath[1]} is, so check first or point at a dir of
# your own that's on $fpath)
querymatter completions zsh > "${fpath[1]}/_querymatter"

# fish
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
| `.output [path\|stdout]` | Redirect subsequent results to `path` (truncating it first), or back to stdout with `.output`/`.output stdout`. See [Redirecting output](#redirecting-output---output). |
| `.settings` | List every setting, its resolved value, and which layer supplied it. |
| `.set <key> <value>` | Save a setting to the config file. Rendering settings (`format`, `table_style`) also apply immediately; scan settings take effect on the next run. |
| `.unset <key>` | Remove a setting from the config file. |
| `.reload` | Re-scan every tracked directory (in-memory only; never touches a `.querymatter` cache). |
| `.refresh [path]` | Force a re-scan of `path` (or the whole vault); updates the `.querymatter` cache when one is loaded, otherwise behaves like `.reload`. |
| `.refresh-all` | Force a re-scan of the whole vault; alias for `.refresh` with no path. |
| `.query run <name>` | Run a saved query in-session, honoring the current `.format`/`.style`/`.output`. See [Saved queries](#saved-queries-querymatter-query). |
| `.query list` | List every saved query's name and SQL. |
| `.quit` / `.exit` | Leave the REPL (Ctrl-D also exits; Ctrl-C cancels the current line). |

SQL statements may span multiple lines; a trailing `;` ends the statement and
runs it. Ending with `\G` instead runs it and prints each row as a block of
right-aligned `name: value` lines — the readable way to inspect a wide record,
as in `SELECT * LIMIT 1\G`. `\g` is accepted as a synonym for `;`. `\G`
overrides whatever `.format` is set to, and works in `-e` and piped batch mode
as well as the REPL.

`.format` and `.style` change the current session only; `.set format` and
`.set table_style` persist to the config file — so you can try a setting, then
keep it.

After each REPL statement's result, a `-- N rows` line (singular for exactly
one row) is printed to stderr — a quick sanity check that distinguishes a
genuinely empty result from a typo'd `WHERE`. It's REPL-only, printed to
stderr rather than stdout, and never appears in `-e` or piped batch mode, so
it never corrupts piped output.

Tab-completion is available for: frontmatter column names and the `file.*`
pseudo-columns, in SQL position; dot-command names, right after a leading
`.`; config keys, right after `.set`/`.unset`; and saved-query names, right
after `.query run`. It does not complete SQL keywords (`SELECT`, `WHERE`, and
so on) — only schema-derived and dot-command/config-key/saved-query names.
The column and saved-query-name lists are both one-time snapshots taken when
the REPL starts, so a field discovered by a later `.reload`/`.refresh`, or a
query saved from another shell (`querymatter query save`) mid-session, won't
tab-complete until you restart the REPL.

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
  piping).
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
Further planned work is tracked in [`TODO.md`](TODO.md).
