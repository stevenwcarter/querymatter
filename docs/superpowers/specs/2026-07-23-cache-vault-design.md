# `.querymatter` cache/vault — design spec

**Date:** 2026-07-23
**Status:** Approved (brainstorming complete)
**Builds on:** `docs/superpowers/specs/2026-07-22-querymatter-design.md` (§9 TTL-cache seams),
`docs/superpowers/specs/2026-07-23-querymatterignore-design.md` (`Cli::ignore_files` seam)

## 1. Summary

Add a persistent **flat-file cache** ("the database") stored in a `.querymatter/`
directory, to speed up startup on large vaults by avoiding a full re-read +
YAML-parse of every file. The cache is created explicitly with a new
`querymatter init [DIR]` subcommand; a normal query run searches **upward** from
the cwd for a `.querymatter/`, and if found, loads it and runs a freshness check
before querying (falling back to today's live in-memory scan when no cache is
found). Freshness is an **accurate per-file `(mtime, size)`** check by default,
with a faster dir-mtime + TTL hybrid behind `--fast`, a zero-FS `--force-cache`,
and explicit `--refresh <path>` / `--refresh-all`.

The cache is a **serialized record store, not a query engine**: the existing
in-memory query engine (`query::*`) and `render` are untouched — the cache only
replaces the "walk + read + parse" front of the pipeline (`store`/`discover`/
`frontmatter`) with "load blobs + re-parse only what changed."

### Goals
- `querymatter init [DIR]` builds a `.querymatter/` cache over everything under
  DIR (or cwd), honoring `--respect-gitignore`, `.querymatterignore`/`--ignore-file`,
  `--exclude`, `--hidden`, `--ext`.
- Normal runs auto-discover an ancestor `.querymatter/` and use it; no cache → live scan (unchanged).
- Correct-by-default freshness (per-file mtime+size); `--fast` (dir-mtime+TTL),
  `--force-cache` (zero FS), `--refresh <path>`/`--refresh-all` (forced re-scan).
- REPL `.refresh [path]` / `.refresh-all` update the DB and return to the prompt refreshed.
- `--no-cache` bypasses vault discovery entirely.
- Safe format evolution (version header → rebuild on mismatch) and crash-safe writes (atomic).
- On `init` inside a git repo, offer to add `.querymatter/` to `.gitignore` (interactive only).

### Non-goals (this change)
- No SQL/embedded DB (Turso/libsql/rusqlite) and no `rkyv` — see §12 rationale.
- No query push-down into storage (engine still loads all records into memory).
- No background/daemon refresh, no file-watching (refresh is explicit or per-run freshness).
- No network/sync (Turso's sync features are irrelevant here).

## 2. On-disk layout (`.querymatter/`)

```
.querymatter/
  manifest.bin      # header + directory index (read first, written last)
  <blobname>.bin    # one blob per cached filesystem directory
```

- **`manifest.bin`**: a header `{ magic: b"QMDB", schema_version: u32, crate_version: String }`
  followed by `Vec<ManifestEntry { dir: PathBuf (absolute), scanned_at: SystemTime,
  dir_mtime: SystemTime, blob: String }>`. On load, read and validate the header
  **first**: on `magic` mismatch or `schema_version` mismatch, treat the entire
  cache as absent (query → live scan; `init` → rebuild). `crate_version` is
  informational (logged), not a hard gate.
- **`<blobname>.bin`**: one `CachedDir` per filesystem directory that holds ≥1
  matched file. `blobname` is a collision-free per-directory name — a hex of a
  `std::hash` of the directory's absolute path, or a manifest-assigned index
  (`0.bin`, `1.bin`, …); **no new hashing dependency is required**. The real
  `dir` path is stored inside the blob and in the manifest entry.
- **Atomic writes:** every file is written to a temp name in `.querymatter/` and
  `rename()`d into place (atomic on the same filesystem). Per-dir blobs are
  written first; **`manifest.bin` is written last**, so a crash mid-write leaves
  a missing/stale manifest (→ safe re-scan), never a manifest pointing at a
  half-written blob. A `.querymatter/.gitignore` containing `*` is NOT written
  (the whole dir is what the user is asked to gitignore; §7).

## 3. Data model & serialization

- `#[derive(Serialize, Deserialize)]` is added to `model::Value` (recursive
  `List(Vec<Value>)` needs no annotations; `Bool/Int/Float/Str/Null` are trivial).
- New `cache` module types (all `Serialize + Deserialize`):
  - `CachedFile { rel_path: String, mtime: SystemTime, size: u64, fields: IndexMap<String, Value> }`
  - `CachedDir { dir: PathBuf, scanned_at: SystemTime, dir_mtime: SystemTime, files: Vec<CachedFile> }`
  - `Manifest { magic: [u8;4], schema_version: u32, crate_version: String, ttl_secs: u64, dirs: Vec<ManifestEntry> }`
    (`ttl_secs` is a per-DB setting, default 300, set at `init` via `--ttl <secs>`; consulted only by `--fast`.)
  - `ManifestEntry { dir: PathBuf, scanned_at: SystemTime, dir_mtime: SystemTime, blob: String }`
- Serialization crate: **`bincode` 2.0** via its serde bridge. (`indexmap` gains
  its `serde` feature; `PathBuf`/`SystemTime` are serde-native.)
- On load, a `CachedFile` reconstructs a `Record` via `Record::new(dir, dir.join(rel_path), fields)`.
  `Record` itself needs no serde derive (records are rebuilt from `CachedFile`),
  keeping the query model decoupled from the on-disk format.
- **`SCHEMA_VERSION: u32`** constant lives in `cache`; bump it whenever any cached
  struct's shape changes. The magic + version is the only safe "format changed →
  discard" mechanism (bincode has no built-in schema versioning).

## 4. Freshness (per directory, on a normal run)

The store is populated from the cache, then made fresh per the selected mode:

- **Default — accurate per-file:** re-walk the vault subtree structure (`discover`,
  which is a cheap `readdir`+filter, gives the current file set), and for each
  current file compare `(mtime, size)` from a `symlink_metadata`/`metadata` stat
  against the `CachedFile`. **Reuse** the cached `fields` (no read/parse) when
  unchanged; **read + parse** new or changed files; **drop** cached files no
  longer present. Compare **size alongside mtime** (git's racy-stat fix: a file
  written in the same clock tick as the last cache write can have an equal mtime).
- **`--fast` — dir-mtime + TTL hybrid:** for a directory whose `dir_mtime` is
  unchanged AND `now - scanned_at <= TTL`, skip statting that directory's files
  and reuse its cached records wholesale; otherwise fall back to the per-file
  check for that directory. Faster on huge/slow vaults; misses in-place content
  edits within the TTL window (documented tradeoff).
- **`--force-cache` — zero FS:** load `manifest.bin` + blobs and build records
  with **no** file/dir stats and no walking. Errors if no cache is found.
- **`--refresh <PATH>` (repeatable) / `--refresh-all`:** before querying, force a
  full re-scan (read+parse) of PATH's subtree / the whole vault, ignoring all
  freshness shortcuts, and rewrite the affected blobs + manifest.
- After any mode that changed the cache, rewrite the changed blob(s) and the
  manifest (atomic). `--force-cache` never writes. TTL default (5 minutes) is
  stored in the manifest/config and only consulted by `--fast`.

**Invariant (load-bearing, §10):** for an unchanged file, the record built from
the cache must be **identical** to the record a live scan produces. A stale cache
must never change query results silently — this is why the accurate per-file
check is the default and `--refresh` is cheap.

## 5. CLI surface

The CLI grows an **optional subcommand** (`clap` `#[command(subcommand)] command:
Option<Command>`); with no subcommand, the existing query args drive query mode.
Walk-related flags (`--respect-gitignore`, `--ignore-file`, `--no-ignore-file`,
`--exclude`, `--hidden`, `--ext`) are shared between query mode and `init` via a
`#[command(flatten)]` struct so they are not duplicated.

### `querymatter init [DIR]`
Creates `.querymatter/` in DIR (default cwd) and builds the cache over every
matched file under DIR, honoring the shared walk flags and `.querymatterignore`
(cwd + `--ignore-file`). `--ttl <secs>` sets the DB's TTL (default 300, stored in
the manifest, used only by `--fast`). Re-runnable (full rebuild). Prints a summary
(directories/files cached) to stderr. Fires the git prompt (§7).

### Query run — new flags (query mode only)
| Flag | Meaning |
| --- | --- |
| `--no-cache` | Ignore any `.querymatter/`; always live-scan (today's behavior). |
| `--force-cache` | Trust the cache; **no** FS access. Error if no cache found. |
| `--fast` | Use the dir-mtime + TTL hybrid freshness instead of per-file. |
| `--refresh <PATH>` | Force re-scan of PATH's subtree before querying; repeatable. |
| `--refresh-all` | Force re-scan of the whole vault before querying. |

Discovery: unless `--no-cache`, search **upward from cwd** for a directory
containing `.querymatter/`. If found, that directory is the **vault base**; load
its cache, apply the freshness mode, then query. If not found (or `--no-cache`),
live-scan the positional `[DIRS]` (or cwd) exactly as today. When a vault is used,
positional `[DIRS]`, if given, restrict the query to records under those subtrees
(a filter over the loaded vault); with no `[DIRS]`, the whole vault is queried.

### REPL
- `.refresh [path]` — force re-scan of `path` (or the whole vault if omitted),
  update the DB, and return to the prompt on the refreshed view.
- `.refresh-all` — force re-scan of the whole vault + update the DB.
- The existing `.reload` remains for the in-memory (no-DB) session. When a DB is
  loaded, `.refresh`/`.refresh-all` are the DB-aware path (they persist);
  `.reload` still works (in-memory only, no persist). `.help`/`.schema` updated.

## 6. Vault discovery

`fn find_vault(start: &Path) -> Option<PathBuf>`: walk from `start` (cwd) upward to
the filesystem root, returning the first ancestor that contains a `.querymatter/`
directory with a readable `manifest.bin`. This is a new resolution seam alongside
`Cli::resolved_roots` / `Cli::ignore_files`. `--no-cache` short-circuits it to `None`.

## 7. Git-ignore prompt (only on `init`)

During `init`, if the vault base is inside a git working tree (walk up for a
`.git` directory) **and** `.querymatter/` is not already ignored **and** stdin is
a TTY:
- Prompt on **stderr**: `Add .querymatter/ to .gitignore? [y/N] ` and read one
  line from stdin. On an affirmative (`y`/`yes`, case-insensitive), append a
  `.querymatter/` line to the repo's top-level `.gitignore` (creating it if
  absent). On anything else, do nothing.
- Non-interactive (piped stdin / no TTY): do **not** modify `.gitignore`; print a
  one-line stderr hint (`hint: add .querymatter/ to .gitignore`).

"Already ignored" is detected without shelling out to git when practical (read
the repo's `.gitignore` and check for a `.querymatter` entry); a git repo is
detected by finding a `.git` directory in an ancestor. Modifying `.gitignore` is
a persistent-config change and happens **only** on the explicit interactive yes.

## 8. Integration & module shape

- New **`cache`** module (`src/cache.rs`): `Manifest`/`CachedDir`/`CachedFile`,
  the `SCHEMA_VERSION`, (de)serialization with the version header, atomic writes,
  `find_vault`, and the load/freshness/refresh operations. It reuses
  `discover::discover` for the walk and `frontmatter::extract` for parsing (via a
  small shared "scan one file → CachedFile" helper factored from `store::scan_root`).
- **`store`**: gains a cache-backed construction path (e.g.
  `InMemoryStore::from_cache(vault, opts, mode) -> (Self, LoadReport)`) that
  populates slices from the cache + freshness pass, and a `refresh_dir/refresh_all`
  that re-scans and rewrites blobs. `reload_dir` (in-memory) stays. The
  `RecordStore` trait is unchanged; `records()`/`schema()` are identical.
- **`cli`**: the subcommand enum + shared flatten struct + the new query flags +
  a `find_vault`-based resolution. `main`: dispatch `init` vs query; in query mode,
  decide cache-vs-live and construct the store accordingly. `session`/`repl`: the
  `.refresh` commands call into the store's refresh + persist.
- **stdout discipline preserved:** all cache diagnostics, the git prompt, and
  refresh summaries go to **stderr**; only query results reach stdout.

## 9. Edge cases & decisions

- **No cache found + `--force-cache`:** hard error (naming that no `.querymatter/`
  was found), non-zero exit.
- **Incompatible cache (version/magic mismatch):** query → live scan (warn to
  stderr); `init` → rebuild. Never deserialize a mismatched blob.
- **Corrupt/partial blob** (deserialize error): treat that directory as absent →
  re-scan it; warn to stderr. A missing manifest → whole cache absent.
- **Symlinks / non-UTF-8 paths:** `rel_path` stored as `String` via lossy where
  needed; document that non-UTF-8 filenames are unsupported in the cache (rare).
- **Vault found but positional DIRS point outside it:** the DIRS filter yields no
  vault records; fall back to live-scanning those DIRS (they're outside the vault).
- **`.querymatter/` itself** is never treated as a scannable directory (excluded
  from the walk, like a hidden dir).
- **Concurrent runs:** last writer wins per blob (atomic rename); no locking in v1
  (documented). A `--refresh` racing a query may reload mid-flight — acceptable.

## 10. Invariants this feature depends on
- **Cache-equals-live:** a record reconstructed from an unchanged `CachedFile`
  equals the record a live scan of that file produces (same fields, same `file.*`).
  Pinned by a round-trip test AND a "cache vs `--no-cache` produce identical query
  output" integration test.
- **Freshness correctness:** an in-place edited file (mtime or size changed) is
  re-parsed under the default mode; a new file is added; a deleted file is dropped.
  One test each.
- **Version safety:** a manifest with a wrong `schema_version`/`magic` is treated
  as absent (never deserialized as the current shape). Test with a bumped/garbage header.
- **stdout cleanliness:** cache warnings, the git prompt, and refresh summaries go
  to stderr; a `--format json` run over a vault still yields pure JSON on stdout.

## 11. Testing (TDD)
- **Unit — `cache` serialization/versioning:** round-trip a `CachedDir` through
  bincode; a wrong-version/garbage header → `None`/rebuild; atomic write leaves no
  partial file on simulated failure (write-temp-then-rename covered by a rename test).
- **Unit — freshness:** given a cache + a temp tree, unchanged file reuses cached
  fields (assert the file is NOT re-read — e.g. mutate the on-disk file's *content*
  but keep mtime/size and confirm the cached value is used; then bump mtime and
  confirm re-parse); new file added; deleted file dropped; size-change detected.
  `--force-cache` returns cached data even when the file changed on disk.
- **Unit — `find_vault`:** finds an ancestor `.querymatter/`; returns None at root;
  `--no-cache` path bypasses.
- **Unit — git prompt logic:** `.querymatter` already-ignored detection; git-repo
  detection; the TTY gate (test the pure decision function, not real stdin).
- **Integration — `tests/cli.rs`:** `querymatter init` builds a cache (assert
  `.querymatter/manifest.bin` exists); a subsequent query over the vault returns
  the same rows as `--no-cache`; editing a file then querying reflects the change
  (default freshness); `--force-cache` after an edit returns the OLD value;
  `--refresh` after an edit returns the NEW value; `init` in a non-TTY does not
  touch `.gitignore`.

## 12. Crate decisions (rationale)
- **Flat-file, not SQL.** The engine loads all records into memory with no
  push-down, so an embedded DB is only a heavier record store. Turso's pure-Rust
  rewrite is beta/not-GA (2026); `libsql` is async-heavy and ~200× slower than
  `rusqlite` in local mode. Declined.
- **`bincode` 2.0 + serde derive, not `rkyv`.** The bottleneck removed is the FS
  walk + YAML parse, not blob deserialization (single/low-tens of ms even at 50k
  records) — so the ergonomic, low-risk format wins. `rkyv`'s zero-copy is negated
  because the engine needs owned `Record`s, and it fights the recursive `Value`
  enum + `SystemTime`/`PathBuf` (plus a live recursive-enum compiler-ICE risk).
- New deps: **`serde`** (`derive`), **`bincode`** 2.0, `indexmap` `serde` feature.
  Pure-Rust, OpenSSL-free. `directories` (already present) is available if a config
  path is needed; the cache lives in the vault's `.querymatter/`, not XDG.

## 13. Phasing (informs the plan's task breakdown)
1. `Value` serde derive + cache data model + bincode round-trip + version header (pure, no FS).
2. On-disk read/write: per-dir blobs + manifest, atomic temp-then-rename, corrupt/missing handling.
3. Freshness engine: per-file default + `--fast` hybrid + `--refresh` (pure-ish over a temp tree).
4. `find_vault` + `store::from_cache`/refresh wiring.
5. CLI: subcommand restructure (`init`) + shared flatten flags + `--no-cache/--force-cache/--fast/--refresh(-all)` + `main` dispatch.
6. `init` command body + git-ignore prompt.
7. REPL `.refresh`/`.refresh-all`.
8. Integration tests + README + docs.
