# `.querymatter` Cache/Vault Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax. Implementers are `rust-developer` agents: clippy-clean (`cargo clippy --all-targets --all-features -- -D warnings`) and rustfmt-clean.

**Goal:** A persistent flat-file cache in `.querymatter/` that speeds startup on large vaults; created by `querymatter init`, auto-discovered on normal runs, freshness-checked (accurate per-file default; `--fast`/`--force-cache`/`--refresh`).

**Architecture:** New `cache` module (serde+bincode blobs per directory + a versioned `manifest.bin`) sits in front of the existing pipeline. It reuses `discover` + `frontmatter`, produces `Record`s that populate the unchanged `RecordStore`, so `query`/`render` are untouched. `main` gains a subcommand (`init`) and a cache-vs-live decision.

**Tech Stack:** Rust edition 2024; new deps `serde` (derive), `bincode` 2.0, `indexmap` `serde` feature. Existing: `ignore`, `clap`, `anyhow`. No SQL, no rkyv (see spec §12).

**Spec:** `docs/superpowers/specs/2026-07-23-cache-vault-design.md`

## Global Constraints
- Edition 2024; crate/binary `querymatter`; clippy-clean (`-D warnings`) and rustfmt-clean each commit. **Run `cargo fmt --all` yourself** (no pre-commit hook).
- Commit `Cargo.lock` with any dep addition. New deps must stay OpenSSL/native-tls-free (bincode/serde are pure-Rust; verify `cargo tree -i native-tls`/`-i openssl` empty).
- **stdout is results-only.** Cache warnings, the git prompt, refresh summaries → stderr.
- **Cache-equals-live invariant:** a record built from an unchanged cache entry must equal a live-scan record. This is load-bearing; every task preserves it.
- Bin-only crate: `cargo test <name>` (no `--lib`).
- Commit messages end with: `Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6`

## Shared types (defined in Task 1; referenced by all tasks — use these names/shapes verbatim)
```rust
// src/cache.rs
pub const MAGIC: [u8; 4] = *b"QMDB";
pub const SCHEMA_VERSION: u32 = 1;      // bump on ANY on-disk struct shape change

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct CachedFile { pub rel_path: String, pub mtime: SystemTime, pub size: u64, pub fields: IndexMap<String, Value> }
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct CachedDir  { pub dir: PathBuf, pub scanned_at: SystemTime, pub dir_mtime: SystemTime, pub files: Vec<CachedFile> }
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct ManifestEntry { pub dir: PathBuf, pub scanned_at: SystemTime, pub dir_mtime: SystemTime, pub blob: String }
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct ManifestBody { pub crate_version: String, pub ttl_secs: u64, pub dirs: Vec<ManifestEntry> }
// On-disk manifest = MAGIC(4) ++ SCHEMA_VERSION(4 LE) ++ bincode(ManifestBody). Blobs = bincode(CachedDir), no header (manifest version gates the epoch).

pub enum Freshness { PerFile, Fast, ForceCache }  // selected by flags
```

---

### Task 1: `Value` serde + cache data model + bincode round-trip + version header

**Files:** Cargo.toml (deps), `src/model.rs` (Value derive), `src/cache.rs` (new), `src/main.rs` (`pub mod cache;`). Test: inline in `src/cache.rs`.

**Interfaces produced:**
- The shared types above.
- `pub fn encode<T: serde::Serialize>(v: &T) -> Vec<u8>` (bincode 2.0 serde bridge, `bincode::config::standard()`).
- `pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T>` (returns None on error).
- `pub fn write_manifest_bytes(body: &ManifestBody) -> Vec<u8>` (MAGIC ++ version LE ++ bincode(body)).
- `pub fn read_manifest_bytes(bytes: &[u8]) -> Option<ManifestBody>` (check len≥8, magic, version == SCHEMA_VERSION; else None; then decode the rest).

- [ ] **Step 1: deps** — `cargo add serde --features derive`, `cargo add bincode@2`, and enable indexmap's serde feature (`cargo add indexmap --features serde`). Confirm TLS-free.
- [ ] **Step 2: derive serde on `Value`** — add `serde::Serialize, serde::Deserialize` to `model::Value`'s derives.
- [ ] **Step 3: failing tests** (`src/cache.rs`)
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use indexmap::IndexMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH, Duration};

    fn sample_dir() -> CachedDir {
        let mut f = IndexMap::new();
        f.insert("status".to_string(), Value::Str("draft".into()));
        f.insert("tags".to_string(), Value::List(vec![Value::Str("a".into())]));
        CachedDir {
            dir: PathBuf::from("/v/plans"),
            scanned_at: UNIX_EPOCH + Duration::from_secs(1000),
            dir_mtime: UNIX_EPOCH + Duration::from_secs(900),
            files: vec![CachedFile { rel_path: "a.md".into(), mtime: UNIX_EPOCH + Duration::from_secs(800), size: 42, fields: f }],
        }
    }
    #[test]
    fn cacheddir_roundtrips_through_bincode() {
        let d = sample_dir();
        let bytes = encode(&d);
        assert_eq!(decode::<CachedDir>(&bytes), Some(d));
    }
    #[test]
    fn manifest_header_roundtrips() {
        let body = ManifestBody { crate_version: "0.1.0".into(), ttl_secs: 300, dirs: vec![] };
        let bytes = write_manifest_bytes(&body);
        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(read_manifest_bytes(&bytes), Some(body));
    }
    #[test]
    fn wrong_magic_rejected() {
        let mut bytes = write_manifest_bytes(&ManifestBody { crate_version: "x".into(), ttl_secs: 1, dirs: vec![] });
        bytes[0] = b'Z';
        assert_eq!(read_manifest_bytes(&bytes), None);
    }
    #[test]
    fn wrong_version_rejected() {
        let body = ManifestBody { crate_version: "x".into(), ttl_secs: 1, dirs: vec![] };
        let mut bytes = write_manifest_bytes(&body);
        bytes[4..8].copy_from_slice(&(SCHEMA_VERSION + 1).to_le_bytes());
        assert_eq!(read_manifest_bytes(&bytes), None);
    }
    #[test]
    fn garbage_rejected() {
        assert_eq!(read_manifest_bytes(b"xx"), None);
        assert_eq!(read_manifest_bytes(&[]), None);
    }
}
```
- [ ] **Step 4: implement** the types + `encode`/`decode`/`write_manifest_bytes`/`read_manifest_bytes` + `pub mod cache;` in main.rs. Use `bincode::serde::encode_to_vec` / `decode_from_slice` with `bincode::config::standard()`.
- [ ] **Step 5:** `cargo test cache` + full `cargo test` (Value serde derive must not break existing tests) → PASS; fmt + clippy clean.
- [ ] **Step 6: commit** `feat(cache): serde/bincode data model + versioned manifest header`.

---

### Task 2: On-disk cache read/write (per-dir blobs + manifest, atomic) + `find_vault`

**Files:** `src/cache.rs`. Test: inline (tempfile).

**Interfaces produced:**
- `pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()>` — write to `path.with_extension("tmp-<n>")` (or a temp name in the same dir) then `fs::rename` into place.
- `pub fn save_cache(vault_dir: &Path, dirs: &[CachedDir], ttl_secs: u64) -> anyhow::Result<()>` — writes each `CachedDir` blob (blob name = hex of a `std::hash` of `dir` absolute path, or index), **then** `manifest.bin` last, all atomically, into `<vault_dir>/.querymatter/`.
- `pub fn load_cache(vault_dir: &Path) -> Option<(ManifestBody, Vec<CachedDir>)>` — read `<vault_dir>/.querymatter/manifest.bin`; `read_manifest_bytes` → None (missing/incompatible) means "no usable cache". For each manifest entry, load+decode its blob; a blob that fails to decode is **skipped with its dir omitted** (caller re-scans it) — do not abort the whole load.
- `pub fn find_vault(start: &Path) -> Option<PathBuf>` — walk `start` upward; return the first ancestor containing `.querymatter/manifest.bin`.

- [ ] **Step 1: failing tests** (representative)
```rust
    #[test]
    fn save_then_load_roundtrips() {
        let td = tempfile::TempDir::new().unwrap();
        let dirs = vec![sample_dir_at(td.path().join("plans"))]; // helper building a CachedDir with a real path
        save_cache(td.path(), &dirs, 300).unwrap();
        assert!(td.path().join(".querymatter/manifest.bin").is_file());
        let (body, loaded) = load_cache(td.path()).unwrap();
        assert_eq!(body.ttl_secs, 300);
        assert_eq!(loaded, dirs);
    }
    #[test]
    fn missing_manifest_is_none() {
        let td = tempfile::TempDir::new().unwrap();
        assert!(load_cache(td.path()).is_none());
    }
    #[test]
    fn incompatible_manifest_is_none() {
        let td = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(td.path().join(".querymatter")).unwrap();
        std::fs::write(td.path().join(".querymatter/manifest.bin"), b"NOPEnotaversion").unwrap();
        assert!(load_cache(td.path()).is_none());
    }
    #[test]
    fn corrupt_blob_skips_only_that_dir() {
        // save two dirs, then clobber one blob with garbage; load returns the other, not an error
        // (build via save_cache, read manifest to find a blob name, overwrite it, reload)
    }
    #[test]
    fn find_vault_finds_ancestor() {
        let td = tempfile::TempDir::new().unwrap();
        save_cache(td.path(), &[], 300).unwrap();
        let deep = td.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_vault(&deep), Some(std::fs::canonicalize(td.path()).unwrap()));
        // and None when no ancestor has one:
        let other = tempfile::TempDir::new().unwrap();
        assert_eq!(find_vault(other.path()), None);
    }
```
(Add a `sample_dir_at(path)` test helper; flesh out `corrupt_blob_skips_only_that_dir` per its comment.)
- [ ] **Step 2:** run → FAIL. **Step 3:** implement `write_atomic`, `save_cache`, `load_cache`, `find_vault`. Blob names: `format!("{:016x}.bin", stable_hash(dir))` using `std::hash::{Hash,Hasher}` with `std::collections::hash_map::DefaultHasher` (deterministic within a build; a format change bumps `SCHEMA_VERSION` anyway). Create `.querymatter/` as needed. **Step 4:** `cargo test cache` + full suite → PASS; fmt + clippy clean.
- [ ] **Step 5: commit** `feat(cache): atomic on-disk read/write + find_vault`.

---

### Task 3: scan-one-file helper + per-file freshness + `--force-cache`

**Files:** `src/cache.rs`; refactor a shared scan helper (may touch `src/store.rs` to share `scan_root`'s per-file logic — keep behavior identical). Test: inline (tempfile).

**Interfaces produced:**
- `pub fn scan_file(dir: &Path, path: &Path) -> Option<(CachedFile, Option<String>)>` — stat `(mtime, size)`, read+`frontmatter::extract`; returns the `CachedFile` when it has frontmatter (`Fields`), `None` for no-frontmatter, and threads a warning string for read/invalid errors. (Factor so `store::scan_root` and the cache share one definition of "file → record".)
- `pub fn refresh_against_cache(vault: &Path, cached: &[CachedDir], opts: &WalkOpts, mode: Freshness) -> (Vec<CachedDir>, LoadReport, bool /*changed*/)` — for `PerFile`: walk (`discover`) the current files; for each, reuse the matching `CachedFile` when `(mtime,size)` match, else re-scan; drop cached files no longer present; group into `CachedDir`s; `changed` = whether anything differed. For `ForceCache`: return `cached` unchanged, no FS access, `changed=false`. (`Fast` is Task 4.)
- `records_from(dirs: &[CachedDir]) -> Vec<(PathBuf /*dir*/, Vec<Record>)>` — reconstruct `Record`s (`Record::new(dir, dir.join(rel_path), fields)`), grouped by dir, for the store.

- [ ] **Step 1: failing tests**
```rust
    #[test]
    fn unchanged_file_reuses_cached_fields_without_reparsing() {
        // Build a cache for a temp dir with a.md {status: draft}. Then OVERWRITE a.md's
        // *content* to {status: DONE} but RESTORE its original mtime and keep size equal
        // (pad so size matches). PerFile refresh must still yield the CACHED value (draft),
        // proving it did not re-parse an (mtime,size)-unchanged file.
    }
    #[test]
    fn changed_mtime_triggers_reparse() {
        // Same, but bump mtime → refresh yields the NEW value (DONE).
    }
    #[test]
    fn new_file_added_and_deleted_file_dropped() {
        // add b.md → appears; remove a.md → gone.
    }
    #[test]
    fn force_cache_returns_cached_even_when_file_changed() {
        // change a.md on disk; ForceCache refresh yields the old cached value and touches no FS.
    }
```
(These need a helper to set a file's mtime — use `std::fs::File::set_modified` or the `filetime` crate ONLY if std is insufficient; prefer std `set_modified`. Keep size equal by padding content with trailing spaces inside the frontmatter value or a comment.)
- [ ] **Step 2-4:** FAIL → implement `scan_file`, `refresh_against_cache` (PerFile + ForceCache), `records_from`; factor the shared file-scan so `store::scan_root` uses it (existing store tests must still pass) → PASS; fmt + clippy clean.
- [ ] **Step 5: commit** `feat(cache): per-file freshness check and --force-cache path`.

---

### Task 4: `--fast` hybrid (dir-mtime + TTL) + forced refresh + `build_vault`

**Files:** `src/cache.rs`. Test: inline (tempfile).

**Interfaces produced:**
- Extend `refresh_against_cache` for `Freshness::Fast`: for each cached dir, if its on-disk `dir_mtime` is unchanged AND `now - scanned_at <= ttl` (ttl from the manifest), reuse the whole `CachedDir` without statting its files; else fall back to the PerFile path for that dir. (Pass `ttl_secs` in.)
- `pub fn build_vault(base: &Path, opts: &WalkOpts, ttl_secs: u64) -> anyhow::Result<LoadReport>` — the `init` core: full scan under `base` (read+parse ALL matched files), group into `CachedDir`s (with each file's mtime+size and each dir's dir_mtime), `save_cache(base, dirs, ttl_secs)`. Returns a `LoadReport`.
- `pub fn refresh_subtree(vault: &Path, cached: &mut Vec<CachedDir>, subtree: &Path, opts: &WalkOpts) -> LoadReport` — force re-scan (read+parse) of dirs at/under `subtree`, replacing those `CachedDir`s; caller persists via `save_cache`.

- [ ] **Step 1: failing tests**
```rust
    #[test]
    fn fast_skips_dir_with_unchanged_mtime_within_ttl() {
        // cache a dir; without changing its dir_mtime, edit a file's CONTENT (not add/remove);
        // Fast refresh within ttl reuses the cached (stale) value — proving it skipped stats.
    }
    #[test]
    fn fast_rescans_dir_when_mtime_moved() {
        // add a file (bumps dir_mtime) → Fast re-scans that dir and picks up the change.
    }
    #[test]
    fn build_vault_writes_a_loadable_cache() {
        // tree with plans/a.md, product/b.md → build_vault → load_cache returns both dirs.
    }
    #[test]
    fn refresh_subtree_reparses_only_that_subtree() {
        // edit a file under plans/ and one under product/; refresh_subtree(plans) updates plans only.
    }
```
- [ ] **Step 2-4:** FAIL → implement → PASS; fmt + clippy clean.
- [ ] **Step 5: commit** `feat(cache): --fast hybrid, forced refresh, build_vault`.

---

### Task 5: `store::from_cache` + refresh wiring

**Files:** `src/store.rs`. Test: inline (tempfile).

**Interfaces produced:**
- `impl InMemoryStore { pub fn from_cache(vault: &Path, opts: WalkOpts, mode: cache::Freshness) -> (Self, LoadReport); }` — `load_cache` (or empty if none), `cache::refresh_against_cache(mode)`, persist changed blobs (unless `ForceCache`), build slices via `records_from`, return the store. One `DirSlice` per cached directory.
- `impl InMemoryStore { pub fn refresh(&mut self, vault: &Path, subtree: Option<&Path>) -> LoadReport; }` — force re-scan of `subtree` (or all), update the in-memory slices, and persist. Used by the REPL `.refresh` and the `--refresh` flags.
- Keep `RecordStore` (records/schema/reload_dir/reload_all/roots) unchanged.

- [ ] **Step 1: failing tests**
```rust
    #[test]
    fn from_cache_matches_live_scan() {
        // init a cache (cache::build_vault) over a temp tree, then InMemoryStore::from_cache(PerFile)
        // must yield the SAME set of (path,status) records as InMemoryStore::load(live).
    }
    #[test]
    fn refresh_picks_up_edits_and_persists() {
        // from_cache; edit a file; store.refresh(vault, None); records reflect the edit AND
        // a fresh load_cache shows the new value (persisted).
    }
```
- [ ] **Step 2-4:** FAIL → implement → PASS; fmt + clippy clean.
- [ ] **Step 5: commit** `feat(store): cache-backed construction and refresh`.

---

### Task 6: CLI subcommand + shared flags + new query flags + `main` dispatch

**Files:** `src/cli.rs`, `src/main.rs`. Test: inline `cli.rs` + `tests/cli.rs`.

**Interfaces produced:**
- `cli.rs`: `#[derive(Subcommand)] enum Command { Init(InitArgs) }`; `Cli` gains `#[command(subcommand)] command: Option<Command>`. Shared walk flags move into a `#[derive(Args)] struct WalkFlags { respect_gitignore, hidden, exclude, ext, ignore_file, no_ignore_file }` `#[command(flatten)]`ed into BOTH `Cli` (query mode) and `InitArgs`. `Cli` gains query-only flags: `no_cache: bool`, `force_cache: bool`, `fast: bool`, `refresh: Vec<PathBuf>`, `refresh_all: bool`. `InitArgs { dir: Option<PathBuf>, ttl: u64 (default 300), #[flatten] walk: WalkFlags }`.
- Helper: `Cli::freshness() -> cache::Freshness` (ForceCache if `force_cache`, else Fast if `fast`, else PerFile) and validation (e.g. `--force-cache` + `--refresh` conflict → error; `--no-cache` + `--force-cache` conflict → error).
- `main`: if `Some(Command::Init(a))` → build `WalkOpts` from `a.walk`, resolve base dir, `cache::build_vault(base, opts, a.ttl)` (git prompt is Task 7 — leave a call site), print summary to stderr, exit. Else query mode: unless `--no-cache`, `cache::find_vault(cwd)`; if a vault → apply `--refresh(-all)` then `InMemoryStore::from_cache(vault, opts, freshness)`; else `InMemoryStore::load(roots, opts)` (today). Then the existing session dispatch.

- [ ] **Step 1: failing tests** — cli unit tests for flag parsing/validation (`freshness()` mapping; conflicting-flag errors) + `tests/cli.rs`: `querymatter init <dir>` creates `<dir>/.querymatter/manifest.bin`; a subsequent `querymatter -e "SELECT ..." ` run from inside the vault returns rows; `--no-cache` from inside a vault still works (live scan). (Run once, lock counts.)
- [ ] **Step 2-4:** FAIL → implement the clap restructure + `main` dispatch. Keep `walk_opts()`/`resolved_roots()`/`ignore_files()` working (now sourced from `WalkFlags`). → PASS; fmt + clippy clean.
- [ ] **Step 5: commit** `feat(cli): init subcommand, cache flags, and main dispatch`.

---

### Task 7: `init` git-ignore prompt

**Files:** `src/cli.rs` or a small `src/gitignore.rs` helper, `src/main.rs`. Test: inline (pure logic).

**Interfaces produced:**
- `pub fn git_root(start: &Path) -> Option<PathBuf>` — nearest ancestor containing `.git`.
- `pub fn querymatter_ignored(git_root: &Path) -> bool` — reads `<git_root>/.gitignore` and returns whether a `.querymatter` entry is present (simple line match: `.querymatter`, `.querymatter/`, `/.querymatter`).
- `pub fn append_gitignore(git_root: &Path) -> anyhow::Result<()>` — append `.querymatter/` to `<git_root>/.gitignore` (create if absent), idempotent.
- Wire into `main`'s init path: if `git_root(base).is_some()` && `!querymatter_ignored(root)` && `std::io::stdin().is_terminal()`, prompt on stderr `Add .querymatter/ to .gitignore? [y/N] `, read one stdin line; on `y`/`yes` (case-insensitive, trimmed) → `append_gitignore`. Non-TTY → print `hint: add .querymatter/ to .gitignore` to stderr, do nothing.

- [ ] **Step 1: failing tests** — `querymatter_ignored` true/false against a temp `.gitignore`; `git_root` finds ancestor `.git`; `append_gitignore` appends once and is idempotent (running twice doesn't duplicate). (The interactive prompt itself is covered by an integration test asserting a non-TTY `init` does NOT modify `.gitignore` — Task 9.)
- [ ] **Step 2-4:** FAIL → implement → PASS; fmt + clippy clean.
- [ ] **Step 5: commit** `feat(init): offer to add .querymatter to .gitignore (interactive)`.

---

### Task 8: REPL `.refresh` / `.refresh-all`

**Files:** `src/repl.rs`, `src/session.rs`. Test: inline (pure core).

**Interfaces produced:**
- `repl::DotCommand` gains `Refresh(Option<String>)` and `RefreshAll`; `parse_dot` maps `.refresh [path]` / `.refresh-all`.
- `Session` gains `refresh(&mut self, subtree: Option<&Path>) -> LoadReport` (delegates to the store's `refresh` when the session is vault-backed; falls back to `reload` when it isn't) — the session must know its vault path (thread it through `Session::new` or a setter). The REPL dispatches `.refresh`/`.refresh-all` to it, printing the report to stderr, then returns to the prompt.
- `.help`/`.schema` text updated to mention the new commands.

- [ ] **Step 1: failing tests** — `parse_dot(".refresh")` → `RefreshAll`? (decide: `.refresh` with no arg = whole vault = `RefreshAll` semantics, OR `Refresh(None)`; pick `Refresh(None)` = current folder-less = whole vault, and `.refresh-all` as an explicit alias). Assert `.refresh plans` → `Refresh(Some("plans"))`, `.refresh` → `Refresh(None)`, `.refresh-all` → `RefreshAll`. Keep the existing `LineBuffer`/dot tests green.
- [ ] **Step 2-4:** FAIL → implement the pure core + wire `run`'s dispatch + `Session::refresh` → PASS; fmt + clippy clean. Manual smoke: batch mode still works.
- [ ] **Step 5: commit** `feat(repl): .refresh and .refresh-all commands`.

---

### Task 9: Integration tests + README + docs

**Files:** `tests/cli.rs`, `README.md`, `Cargo.toml` (bump nothing; ensure metadata current). Test: `tests/cli.rs`.

- [ ] **Step 1: integration tests** (append to `tests/cli.rs`, `assert_cmd`):
  - `init` then query over the vault returns the **same rows** as the same query with `--no-cache` (cache-equals-live).
  - edit a file, then a default query reflects the change (per-file freshness); `--force-cache` returns the OLD value; `--refresh <dir>` (or `--refresh-all`) returns the NEW value.
  - `init` with **piped stdin** (non-TTY) does NOT create/modify a `.gitignore`.
  - `--force-cache` with no vault present exits non-zero with a clear stderr error.
  Run once, read outputs, lock assertions. No production change should be needed; if a test reveals a bug, STOP and report it.
- [ ] **Step 2: README** — a "Caching large vaults (`.querymatter`)" section: what `init` does, upward discovery, the freshness modes (`default per-file`, `--fast`, `--force-cache`, `--refresh`/`--refresh-all`), `--no-cache`, `--ttl`, the REPL `.refresh`/`.refresh-all`, and the `.gitignore` prompt. Add the new flags/subcommand to the flags list.
- [ ] **Step 3:** full `cargo test`, `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings` clean.
- [ ] **Step 4: commit** `test: end-to-end cache/vault integration; README`.

---

## Self-Review
**Spec coverage:** §2 layout → T1/T2. §3 data model/serde/version → T1. §4 freshness (per-file/fast/force-cache/refresh) → T3/T4. §5 CLI (init, flags, REPL) → T6/T8. §6 find_vault → T2. §7 git prompt → T7. §8 store integration → T5, main dispatch → T6. §9 edge cases (no-cache+force-cache error, incompatible/corrupt → rebuild/skip) → T2/T6/T9. §10 invariants (cache-equals-live, freshness, version, stdout) → T3/T5/T9 tests. §11 testing → per-task + T9. §12 deps → T1. §13 phasing → tasks 1-9. ✅
**Placeholder scan:** the "flesh out per its comment" tests (T2 corrupt-blob, T3/T4 mtime helpers) give the full scenario + assertion intent with concrete setup — standard characterization scaffolding, not vague TODOs. No bare TBD.
**Type consistency:** `CachedFile`/`CachedDir`/`ManifestBody`/`ManifestEntry`, `MAGIC`/`SCHEMA_VERSION`, `Freshness`, `encode/decode/read_manifest_bytes/write_manifest_bytes` (T1) → `save_cache/load_cache/find_vault/write_atomic` (T2) → `scan_file/refresh_against_cache/records_from` (T3) → `build_vault/refresh_subtree` (T4) → `from_cache/refresh` (T5) → CLI `Command/InitArgs/WalkFlags/freshness()` (T6) → git helpers (T7) → `DotCommand::Refresh/RefreshAll`, `Session::refresh` (T8). Names line up across tasks.
