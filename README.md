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
| `… \| querymatter [dirs]` (stdin piped, no `-e`) | Batch mode: run each `;`-separated statement from stdin in turn, no prompt |

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
| `-e, --query <QUERY>` | One-shot mode; `-` reads the query text from stdin. May contain several `;`-separated statements. |
| `--format <FMT>` | `table` (default), `json`, `csv`, `tsv`, or `md`. In the REPL this is just the *initial* format — `.format` changes it live. |
| `--ext <LIST>` | Comma-separated extensions to include. Default `md,markdown`. |
| `--respect-gitignore` | Honor `.gitignore`/`.ignore` while walking. **Off by default** — see below. |
| `--hidden` | Descend into hidden files/directories (e.g. `.git`, `.obsidian`). Off by default. |
| `--exclude <GLOB>` | Path glob to skip. Repeatable, e.g. `--exclude '**/templates/**'`. |

## REPL dot-commands

Inside the REPL, a line starting with `.` (no trailing `;`) is a command
rather than SQL:

| Command | Meaning |
| --- | --- |
| `.help` | List the dot-commands. |
| `.schema` | List discovered frontmatter fields, the `file.*` columns, and the record count. |
| `.format [fmt]` | Show, or set, the output format for subsequent queries. |
| `.reload` | Re-scan every tracked directory. |
| `.quit` / `.exit` | Leave the REPL (Ctrl-D also exits; Ctrl-C cancels the current line). |

SQL statements may span multiple lines; a trailing `;` ends the statement and
runs it.

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
Planned future work — notably a `.querymatter` vault marker with a
TTL-based directory cache — is tracked in [`TODO.md`](TODO.md).
