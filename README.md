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

**stdout carries query results only.** Warnings (e.g. a file with malformed
frontmatter), reload reports, and prompts all go to stderr, so
`querymatter -e '…' --format json | jq` always sees pure JSON.

## The query DSL

A subset of SQL:

```
SELECT cols [AS alias] [FROM 'glob'] [WHERE ...] [GROUP BY ...] [ORDER BY ... [ASC|DESC]] [LIMIT n [OFFSET m]]
```

- **SELECT** — frontmatter field names, `file.*` pseudo-columns (below), `*`
  (every frontmatter key seen, in sorted (alphabetical) order), or an aggregate:
  `count(*)`, `count(col)`, `count(distinct col)`, `min`, `max`, `sum`, `avg`,
  `group_concat`. Any item may take `AS <alias>` to rename its output header.
- **FROM** — optional; when present its value is a path glob applied within
  the scanned directories (e.g. `FROM 'plans/**'`). Omit it and every
  discovered record is in scope.
- **WHERE** — `= != <> < <= > >=`, `LIKE`/`NOT LIKE` (`%`/`_` wildcards),
  `IN (...)`/`NOT IN (...)`, `IS NULL`/`IS NOT NULL`, combined with `AND`,
  `OR`, `NOT`, and parentheses. A quoted string literal forces string
  comparison; a bare numeric literal compares numerically.
- **GROUP BY** — one or more grouping keys; every non-aggregate `SELECT` item
  must be one of them.
- **ORDER BY** — column, alias, or `file.*`, each with optional `ASC`/`DESC`.
  NULLs sort last regardless of direction.
- **LIMIT n [OFFSET m]**.

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

## Flags

| Flag | Meaning |
| --- | --- |
| `-e, --query <QUERY>` | One-shot mode; `-` reads the query text from stdin. May contain several statements, each ended by `;` (or `\G`, which prints every row as a block of `name: value` lines instead of a table). |
| `--format <FMT>` | `table` (default), `json`, `csv`, `tsv`, or `md`. In the REPL this is just the *initial* format — `.format` changes it live. |
| `--table-style <STYLE>` | Border style for `--format table`: `ascii` (default), `unicode`, `compact`, or `plain`. Also settable per-shell with `QUERYMATTER_TABLE_STYLE`; the flag wins. Ignored by `json`/`csv`/`tsv`/`md`. In the REPL this is just the *initial* style — `.style` changes it live. |
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

See [Caching large vaults](#caching-large-vaults-querymatter) below for what
these mean and for `querymatter init`'s own flags (`--ttl`, plus the walk
flags above, which `init` shares).

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
```

Every key is optional; an absent key falls through to the next layer. Values
resolve per key, independently:

```
flag  >  environment  >  config file  >  built-in default
```

So a configured `hidden = true` still scans hidden files when you pass no flag,
and `--no-hidden` turns it back off for one run. `--table-style` additionally
reads `QUERYMATTER_TABLE_STYLE`, which outranks the file but loses to the flag.

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
```

A malformed config file, an unknown key, or an invalid value is a hard error
naming the file — a typo must not silently do nothing. The file is read once,
at startup.

## REPL dot-commands

Inside the REPL, a line starting with `.` (no trailing `;`) is a command
rather than SQL:

| Command | Meaning |
| --- | --- |
| `.help` | List the dot-commands. |
| `.schema` | List discovered frontmatter fields, the `file.*` columns, and the record count. |
| `.format [fmt]` | Show, or set, the output format for subsequent queries. |
| `.style [style]` | Show, or set, the table border style (`ascii`, `unicode`, `compact`, `plain`) for subsequent queries. |
| `.settings` | List every setting, its resolved value, and which layer supplied it. |
| `.set <key> <value>` | Save a setting to the config file. Rendering settings (`format`, `table_style`) also apply immediately; scan settings take effect on the next run. |
| `.unset <key>` | Remove a setting from the config file. |
| `.reload` | Re-scan every tracked directory (in-memory only; never touches a `.querymatter` cache). |
| `.refresh [path]` | Force a re-scan of `path` (or the whole vault); updates the `.querymatter` cache when one is loaded, otherwise behaves like `.reload`. |
| `.refresh-all` | Force a re-scan of the whole vault; alias for `.refresh` with no path. |
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
