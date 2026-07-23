//! The `.querymatter/` on-disk cache: its data model, bincode
//! (de)serialization, atomic read/write, vault discovery, and freshness.
//!
//! A `CachedDir` blob holds every matched file directly under one scanned
//! filesystem directory, plus a versioned `manifest.bin` header that lets a
//! future format change be detected and safely discarded rather than
//! mis-decoded. [`scan_file`] is the single "file → record" definition
//! shared with [`crate::store::scan_root`] (a live query), so a cached
//! record and a freshly scanned one never disagree.

use indexmap::IndexMap;
use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use std::{fs, io};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::discover::{self, WalkOpts};
use crate::frontmatter::{self, Extract};
use crate::model::{Record, Value};
use crate::store::LoadReport;

/// The on-disk directory (relative to a vault root) holding `manifest.bin`
/// and one blob file per cached directory.
const CACHE_DIR_NAME: &str = ".querymatter";

/// Filename of the manifest within [`CACHE_DIR_NAME`].
const MANIFEST_FILE_NAME: &str = "manifest.bin";

/// Magic bytes identifying a `manifest.bin` file, checked before any bincode
/// decode is attempted.
pub const MAGIC: [u8; 4] = *b"QMDB";

/// Bumped whenever any cached struct's shape changes. Together with `MAGIC`
/// this is the only safe "format changed → discard the cache" mechanism,
/// since bincode itself has no schema versioning.
pub const SCHEMA_VERSION: u32 = 1;

/// One cached Markdown file: its frontmatter fields plus enough metadata
/// (`mtime`, `size`) to detect that it has changed on disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedFile {
    pub rel_path: String,
    pub mtime: SystemTime,
    pub size: u64,
    pub fields: IndexMap<String, Value>,
}

/// One cached filesystem directory: every matched file directly under it,
/// plus the directory's own `mtime` (used by the `--fast` freshness hybrid)
/// and when it was scanned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedDir {
    pub dir: PathBuf,
    pub scanned_at: SystemTime,
    pub dir_mtime: SystemTime,
    pub files: Vec<CachedFile>,
}

/// A `manifest.bin` entry pointing at one `CachedDir` blob.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub dir: PathBuf,
    pub scanned_at: SystemTime,
    pub dir_mtime: SystemTime,
    pub blob: String,
}

/// The bincode-encoded body of `manifest.bin`, i.e. everything after the
/// `MAGIC ++ SCHEMA_VERSION` header. Kept separate from the header fields so
/// the header can be validated from raw bytes before any decode is attempted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestBody {
    pub crate_version: String,
    pub ttl_secs: u64,
    pub dirs: Vec<ManifestEntry>,
}

/// Encodes `value` with bincode's serde bridge (`bincode::config::standard()`).
///
/// # Panics
/// If `value` contains a non-UTF-8 `PathBuf`/`OsString` (serde's `Path` impl
/// errors in that case) — a documented, accepted limitation of the cache
/// (design spec §9). **Also** if `value` contains a [`SystemTime`] earlier
/// than [`std::time::UNIX_EPOCH`] (serde's `SystemTime` impl errors before
/// encoding a negative duration). Real filesystem mtimes can be pre-epoch
/// (clock skew, `touch -d`, archive extraction), so callers that encode
/// values built from live filesystem metadata — e.g. `save_cache` — must use
/// [`try_encode`] instead of calling this directly.
pub fn encode<T: Serialize>(value: &T) -> Vec<u8> {
    try_encode(value).expect("value not encodable (non-UTF-8 path or pre-epoch SystemTime)")
}

/// Fallible sibling of [`encode`]: `Err` instead of a panic on any bincode
/// encode error (see [`encode`]'s panic note). Used wherever the encoded
/// value may hold data pulled from the live filesystem rather than
/// constructed in-process, so one anomalous value can't crash a whole batch.
fn try_encode<T: Serialize>(value: &T) -> Result<Vec<u8>, bincode::error::EncodeError> {
    bincode::serde::encode_to_vec(value, bincode::config::standard())
}

/// Decodes a `T` previously produced by [`encode`], or `None` on any error
/// (truncated bytes, corrupt data, shape mismatch).
pub fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Option<T> {
    bincode::serde::decode_from_slice(bytes, bincode::config::standard())
        .map(|(value, _)| value)
        .ok()
}

/// Writes a `manifest.bin` payload: `MAGIC ++ SCHEMA_VERSION (LE u32) ++
/// bincode(body)`.
pub fn write_manifest_bytes(body: &ManifestBody) -> Vec<u8> {
    try_write_manifest_bytes(body)
        .expect("value not encodable (non-UTF-8 path or pre-epoch SystemTime)")
}

/// Fallible sibling of [`write_manifest_bytes`]; see [`try_encode`].
fn try_write_manifest_bytes(body: &ManifestBody) -> Result<Vec<u8>, bincode::error::EncodeError> {
    let mut bytes = Vec::with_capacity(8);
    bytes.extend_from_slice(&MAGIC);
    bytes.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
    bytes.extend_from_slice(&try_encode(body)?);
    Ok(bytes)
}

/// Validates the `manifest.bin` header (`MAGIC` then `SCHEMA_VERSION`)
/// *before* attempting to decode the body, so a future struct-shape change
/// (signaled by a version bump) can never be mis-decoded as the current
/// shape. Returns `None` on any mismatch, truncation, or decode failure.
pub fn read_manifest_bytes(bytes: &[u8]) -> Option<ManifestBody> {
    if bytes.len() < 8 || bytes[0..4] != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if version != SCHEMA_VERSION {
        return None;
    }
    decode(&bytes[8..])
}

/// A deterministic, filesystem-safe hash of `dir`'s absolute path, used to
/// derive its blob filename. Stable only within one build (`DefaultHasher`
/// makes no cross-version guarantee), which is fine: any format change that
/// would matter also bumps [`SCHEMA_VERSION`], discarding the whole cache.
fn stable_hash(dir: &Path) -> u64 {
    let mut hasher = DefaultHasher::new();
    dir.hash(&mut hasher);
    hasher.finish()
}

/// The blob filename for `dir`, e.g. `"3f2a9c1d8b0e4f5a.bin"`.
fn blob_file_name(dir: &Path) -> String {
    format!("{:016x}.bin", stable_hash(dir))
}

/// Writes `bytes` to `path` atomically: encode to a temp file in the same
/// directory, then `rename` over `path`. On a POSIX filesystem `rename` is
/// atomic, so a reader never observes a partially written `path` and a crash
/// mid-write leaves only the untouched previous file (or nothing) at `path`.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write_atomic: path has no file name",
        )
    })?;
    // `process::id()` keeps concurrent runs of the binary from colliding on
    // the same temp name; a single run never writes the same target path
    // twice concurrently, so no further uniquing is needed.
    let tmp_path = dir.join(format!(
        "{}.tmp-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    fs::write(&tmp_path, bytes)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

/// Persists `dirs` to `<vault_dir>/.querymatter/`: one blob file per
/// [`CachedDir`], then `manifest.bin` last, all via [`write_atomic`] so a
/// reader never sees a manifest that outraces its blobs.
///
/// A `CachedDir` that fails to encode (see [`encode`]'s panic note — in
/// practice a pre-1970 mtime from clock skew or archive extraction) is
/// skipped with a warning on stderr and omitted from the manifest, so the
/// caller simply re-scans that one directory next time rather than losing
/// the whole cache save.
pub fn save_cache(vault_dir: &Path, dirs: &[CachedDir], ttl_secs: u64) -> anyhow::Result<()> {
    let cache_dir = vault_dir.join(CACHE_DIR_NAME);
    fs::create_dir_all(&cache_dir).with_context(|| format!("creating {}", cache_dir.display()))?;

    let mut entries = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let bytes = match try_encode(dir) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!(
                    "querymatter: cache: skipping {} (won't encode: {err})",
                    dir.dir.display()
                );
                continue;
            }
        };
        let blob = blob_file_name(&dir.dir);
        let blob_path = cache_dir.join(&blob);
        write_atomic(&blob_path, &bytes)
            .with_context(|| format!("writing {}", blob_path.display()))?;
        entries.push(ManifestEntry {
            dir: dir.dir.clone(),
            scanned_at: dir.scanned_at,
            dir_mtime: dir.dir_mtime,
            blob,
        });
    }

    let body = ManifestBody {
        crate_version: env!("CARGO_PKG_VERSION").to_string(),
        ttl_secs,
        dirs: entries,
    };
    let manifest_bytes = try_write_manifest_bytes(&body).context("encoding manifest.bin")?;
    write_atomic(&cache_dir.join(MANIFEST_FILE_NAME), &manifest_bytes)
        .context("writing manifest.bin")?;
    Ok(())
}

/// Loads the cache saved by [`save_cache`], or `None` if `vault_dir` has no
/// usable one: no `manifest.bin`, or one whose header doesn't match
/// [`MAGIC`]/[`SCHEMA_VERSION`] (see [`read_manifest_bytes`]).
///
/// Each manifest entry's blob is loaded independently; a blob that's
/// missing or fails to decode is skipped — its directory is simply absent
/// from the returned `Vec`, and the caller re-scans it — rather than
/// discarding the whole cache over one corrupt file.
pub fn load_cache(vault_dir: &Path) -> Option<(ManifestBody, Vec<CachedDir>)> {
    let cache_dir = vault_dir.join(CACHE_DIR_NAME);
    let manifest_bytes = fs::read(cache_dir.join(MANIFEST_FILE_NAME)).ok()?;
    let body = read_manifest_bytes(&manifest_bytes)?;

    let loaded = body
        .dirs
        .iter()
        .filter_map(|entry| {
            let bytes = fs::read(cache_dir.join(&entry.blob)).ok()?;
            decode::<CachedDir>(&bytes)
        })
        .collect();
    Some((body, loaded))
}

/// Walks `start` upward through its ancestors, returning the canonicalized
/// path of the first one containing a `.querymatter/manifest.bin` — i.e. the
/// vault root a query rooted at `start` should read/write its cache from.
pub fn find_vault(start: &Path) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        if dir.join(CACHE_DIR_NAME).join(MANIFEST_FILE_NAME).is_file() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Resolves a `--refresh`/`.refresh <PATH>` argument to an absolute path
/// under `vault`.
///
/// The user-typed path may be relative — it is canonicalized against the cwd,
/// mirroring [`crate::cli::Cli::resolved_roots`], and must exist. This is
/// load-bearing: [`refresh_subtree`] filters the vault's *absolute* discovery
/// results with `starts_with(subtree)`, so a raw relative path (`plans`,
/// `./plans`) would prefix-match nothing and silently refresh zero files —
/// running the query against the stale cache (design spec §10). A target that
/// resolves outside the vault is rejected: nothing under the loaded cache
/// could be refreshed by it.
///
/// Shared by the CLI's `--refresh` path ([`crate::main`]) and the REPL's
/// `.refresh` ([`crate::session::Session::refresh`]) so the two behave
/// identically — a relative REPL path is no more a silent no-op than a
/// relative CLI one.
pub(crate) fn resolve_refresh_target(path: &Path, vault: &Path) -> anyhow::Result<PathBuf> {
    let canonical = fs::canonicalize(path)
        .with_context(|| format!("cannot access refresh path {}", path.display()))?;
    anyhow::ensure!(
        canonical.starts_with(vault),
        "refresh path {} is outside the vault {}",
        canonical.display(),
        vault.display()
    );
    Ok(canonical)
}

/// Selects how [`refresh_against_cache`] decides whether a cached file is
/// still trustworthy against the live filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// Stat every current file's `(mtime, size)` against its cached entry;
    /// reuse the cached fields on a match, re-parse on any mismatch. The
    /// accurate default (design spec §4).
    PerFile,
    /// Dir-mtime + TTL hybrid: for a directory whose on-disk `dir_mtime`
    /// still matches the cached value AND whose cache is still within TTL,
    /// reuse the whole cached directory verbatim without stat-ing any of its
    /// files; otherwise fall back to the [`Freshness::PerFile`] check for
    /// that directory only. A content-only edit that leaves the directory's
    /// mtime unmoved is intentionally NOT picked up within the TTL window
    /// (design spec §4) — a documented tradeoff for speed on large vaults.
    Fast,
    /// Trust the cache verbatim; no filesystem access at all. Erroring when
    /// no cache exists is the caller's responsibility.
    ForceCache,
}

/// The outcome of scanning one file for its frontmatter: mirrors
/// [`Extract`], but the frontmatter-found case also carries the on-disk stat
/// needed to build a [`CachedFile`].
#[derive(Debug, Clone, PartialEq)]
pub enum ScanResult {
    /// Frontmatter fields found — ready to cache and turn into a `Record`.
    Cached(CachedFile),
    /// No frontmatter fence: silently skipped, the same treatment a live
    /// scan gives an ordinary Markdown file with no fence.
    NoFrontmatter,
    /// Unreadable file or invalid frontmatter: skipped, with a
    /// human-readable reason meant for a [`LoadReport`].
    Warning(String),
}

/// Stats `path`'s `(mtime, size)` in one call.
fn stat_file(path: &Path) -> io::Result<(SystemTime, u64)> {
    let metadata = fs::metadata(path)?;
    Ok((metadata.modified()?, metadata.len()))
}

/// Scans one file for its frontmatter and on-disk stat — the single
/// "file → record" definition shared by [`crate::store::scan_root`] (a live
/// query, which only keeps `fields`) and [`refresh_against_cache`] (which
/// also needs the stat, to detect changes on a later run).
///
/// `dir` is the directory a [`CachedFile::rel_path`] is resolved relative
/// to; a caller that doesn't need `rel_path` (`store::scan_root`) may pass
/// any ancestor of `path`.
pub fn scan_file(dir: &Path, path: &Path) -> ScanResult {
    let (mtime, size) = match stat_file(path) {
        Ok(stat) => stat,
        Err(err) => return ScanResult::Warning(format!("{}: {err}", path.display())),
    };
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(err) => return ScanResult::Warning(format!("{}: {err}", path.display())),
    };
    match frontmatter::extract(&content) {
        Extract::Fields(fields) => ScanResult::Cached(CachedFile {
            rel_path: path
                .strip_prefix(dir)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned(),
            mtime,
            size,
            fields,
        }),
        Extract::None => ScanResult::NoFrontmatter,
        Extract::Invalid(msg) => ScanResult::Warning(format!("{}: {msg}", path.display())),
    }
}

/// Refreshes `cached` against the live filesystem under `vault` per `mode`,
/// returning the refreshed directories, a [`LoadReport`] of what was
/// (re)loaded/skipped, and whether the result differs from `cached` (so the
/// caller knows whether it's worth persisting via [`save_cache`]).
///
/// `ttl_secs` is only consulted by [`Freshness::Fast`] (it's the manifest's
/// per-DB TTL setting — design spec §3); `PerFile`/`ForceCache` ignore it.
///
/// Timestamp bookkeeping alone (each `CachedDir`'s `scanned_at`, which
/// always advances on a `PerFile`/`Fast` refresh) doesn't count toward
/// "changed" — only actual directory/file membership, stats, or fields do.
pub fn refresh_against_cache(
    vault: &Path,
    cached: &[CachedDir],
    opts: &WalkOpts,
    mode: Freshness,
    ttl_secs: u64,
) -> (Vec<CachedDir>, LoadReport, bool) {
    match mode {
        Freshness::ForceCache => (cached.to_vec(), LoadReport::default(), false),
        Freshness::PerFile => refresh_per_file(vault, cached, opts),
        Freshness::Fast => refresh_fast(vault, cached, opts, ttl_secs),
    }
}

/// The accurate per-file freshness check (see [`Freshness::PerFile`]).
fn refresh_per_file(
    vault: &Path,
    cached: &[CachedDir],
    opts: &WalkOpts,
) -> (Vec<CachedDir>, LoadReport, bool) {
    let cached_by_path: BTreeMap<PathBuf, &CachedFile> = cached
        .iter()
        .flat_map(|cached_dir| {
            cached_dir
                .files
                .iter()
                .map(move |file| (cached_dir.dir.join(&file.rel_path), file))
        })
        .collect();

    let mut report = LoadReport::default();
    let mut by_dir: BTreeMap<PathBuf, Vec<CachedFile>> = BTreeMap::new();
    for path in discover::discover(vault, opts) {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| vault.to_path_buf());
        let previous = cached_by_path.get(&path).copied();
        if let Some(file) = refresh_one_file(&dir, &path, previous, &mut report) {
            by_dir.entry(dir).or_default().push(file);
        }
    }

    let dirs: Vec<CachedDir> = by_dir
        .into_iter()
        .map(|(dir, files)| {
            let dir_mtime = stat_dir_mtime(&dir, &mut report);
            CachedDir {
                dir,
                scanned_at: SystemTime::now(),
                dir_mtime,
                files,
            }
        })
        .collect();

    let changed = !content_equal(&dirs, cached);
    (dirs, report, changed)
}

/// Refreshes one current file against its previous cached entry (if any):
/// reuses the cached fields when `(mtime, size)` are unchanged, otherwise
/// re-scans. Returns `None` when the file has no frontmatter or couldn't be
/// read/parsed — already folded into `report` in that case.
fn refresh_one_file(
    dir: &Path,
    path: &Path,
    previous: Option<&CachedFile>,
    report: &mut LoadReport,
) -> Option<CachedFile> {
    if let Some(previous) = previous
        && let Ok((mtime, size)) = stat_file(path)
        && mtime == previous.mtime
        && size == previous.size
    {
        report.loaded += 1;
        return Some(previous.clone());
    }

    match scan_file(dir, path) {
        ScanResult::Cached(file) => {
            report.loaded += 1;
            Some(file)
        }
        ScanResult::NoFrontmatter => None,
        ScanResult::Warning(msg) => {
            report.skipped += 1;
            report.warnings.push(msg);
            None
        }
    }
}

/// Stats `dir`'s own mtime, used by the `--fast` freshness hybrid (Task 4).
/// A directory that vanishes mid-scan (a rare TOCTOU race, since `discover`
/// just listed a file under it) falls back to `SystemTime::UNIX_EPOCH` with
/// a warning rather than aborting the whole refresh.
fn stat_dir_mtime(dir: &Path, report: &mut LoadReport) -> SystemTime {
    fs::metadata(dir)
        .and_then(|m| m.modified())
        .unwrap_or_else(|err| {
            report.warnings.push(format!("{}: {err}", dir.display()));
            SystemTime::UNIX_EPOCH
        })
}

/// The `--fast` freshness hybrid (see [`Freshness::Fast`]): reuses a cached
/// directory wholesale — no per-file stats at all — when its on-disk
/// `dir_mtime` still matches the cached value and it's still within
/// `ttl_secs` of its last scan; otherwise falls back to [`refresh_one_file`]
/// (the same per-file check [`refresh_per_file`] uses) for that directory's
/// files only.
fn refresh_fast(
    vault: &Path,
    cached: &[CachedDir],
    opts: &WalkOpts,
    ttl_secs: u64,
) -> (Vec<CachedDir>, LoadReport, bool) {
    let cached_by_dir: BTreeMap<PathBuf, &CachedDir> =
        cached.iter().map(|dir| (dir.dir.clone(), dir)).collect();
    let cached_by_path: BTreeMap<PathBuf, &CachedFile> = cached
        .iter()
        .flat_map(|cached_dir| {
            cached_dir
                .files
                .iter()
                .map(move |file| (cached_dir.dir.join(&file.rel_path), file))
        })
        .collect();

    let mut paths_by_dir: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for path in discover::discover(vault, opts) {
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| vault.to_path_buf());
        paths_by_dir.entry(dir).or_default().push(path);
    }

    let now = SystemTime::now();
    let ttl = Duration::from_secs(ttl_secs);
    let mut report = LoadReport::default();
    let dirs: Vec<CachedDir> = paths_by_dir
        .into_iter()
        .map(|(dir, paths)| {
            let current_mtime = stat_dir_mtime(&dir, &mut report);
            if let Some(previous) = cached_by_dir.get(&dir)
                && current_mtime == previous.dir_mtime
                && now
                    .duration_since(previous.scanned_at)
                    .is_ok_and(|age| age <= ttl)
            {
                report.loaded += previous.files.len();
                return (*previous).clone();
            }

            let files: Vec<CachedFile> = paths
                .iter()
                .filter_map(|path| {
                    let previous = cached_by_path.get(path).copied();
                    refresh_one_file(&dir, path, previous, &mut report)
                })
                .collect();
            CachedDir {
                dir,
                scanned_at: now,
                dir_mtime: current_mtime,
                files,
            }
        })
        .collect();

    let changed = !content_equal(&dirs, cached);
    (dirs, report, changed)
}

/// True when `a` and `b` contain the same directories with the same
/// `dir_mtime`/`files`, ignoring each `CachedDir`'s `scanned_at` (which
/// always advances on a `PerFile` refresh and carries no information about
/// whether anything actually changed).
fn content_equal(a: &[CachedDir], b: &[CachedDir]) -> bool {
    fn normalize(dirs: &[CachedDir]) -> Vec<CachedDir> {
        let mut normalized: Vec<CachedDir> = dirs
            .iter()
            .cloned()
            .map(|dir| CachedDir {
                scanned_at: SystemTime::UNIX_EPOCH,
                ..dir
            })
            .collect();
        normalized.sort_by(|x, y| x.dir.cmp(&y.dir));
        normalized
    }
    normalize(a) == normalize(b)
}

/// Reconstructs [`Record`]s from cached directories for querying, grouped
/// by directory.
///
/// `root` is the overall scan root — passing it (rather than each
/// [`CachedDir::dir`]) is what keeps the cache-equals-live invariant intact,
/// since a live scan ([`crate::store::scan_root`]) resolves every record's
/// `file.*` attributes relative to that same root, not to a file's
/// immediate containing directory.
pub fn records_from(root: &Path, dirs: &[CachedDir]) -> Vec<(PathBuf, Vec<Record>)> {
    dirs.iter()
        .map(|cached_dir| {
            let records = cached_dir
                .files
                .iter()
                .map(|file| {
                    let path = cached_dir.dir.join(&file.rel_path);
                    Record::new(root, &path, file.fields.clone())
                })
                .collect();
            (cached_dir.dir.clone(), records)
        })
        .collect()
}

/// The `querymatter init` core: a full scan of `base` (there is no previous
/// cache to reuse against, so every matched file is read and parsed),
/// grouped into [`CachedDir`]s and persisted via [`save_cache`] with the
/// given `ttl_secs`. Returns a [`LoadReport`] summarizing what was
/// loaded/skipped, so the caller can print an `init` summary.
pub fn build_vault(base: &Path, opts: &WalkOpts, ttl_secs: u64) -> anyhow::Result<LoadReport> {
    let (dirs, report, _changed) =
        refresh_against_cache(base, &[], opts, Freshness::PerFile, ttl_secs);
    save_cache(base, &dirs, ttl_secs)?;
    Ok(report)
}

/// Forces a full re-scan (read + parse every matched file, ignoring every
/// freshness shortcut) of the directories at or under `subtree`, replacing
/// those entries of `cached` in place: a directory whose files have all
/// disappeared is dropped, and a directory newly found under `subtree` is
/// appended. Directories outside `subtree` are left untouched. The caller
/// persists the result via [`save_cache`].
pub fn refresh_subtree(
    vault: &Path,
    cached: &mut Vec<CachedDir>,
    subtree: &Path,
    opts: &WalkOpts,
) -> LoadReport {
    let mut report = LoadReport::default();
    let now = SystemTime::now();

    let mut by_dir: BTreeMap<PathBuf, Vec<CachedFile>> = BTreeMap::new();
    for path in discover::discover(vault, opts) {
        if !path.starts_with(subtree) {
            continue;
        }
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| vault.to_path_buf());
        // `previous: None` forces `refresh_one_file` straight to `scan_file`
        // for every file, ignoring any cached (mtime, size) shortcut.
        if let Some(file) = refresh_one_file(&dir, &path, None, &mut report) {
            by_dir.entry(dir).or_default().push(file);
        }
    }

    let refreshed: BTreeMap<PathBuf, CachedDir> = by_dir
        .into_iter()
        .map(|(dir, files)| {
            let dir_mtime = stat_dir_mtime(&dir, &mut report);
            let cached_dir = CachedDir {
                dir: dir.clone(),
                scanned_at: now,
                dir_mtime,
                files,
            };
            (dir, cached_dir)
        })
        .collect();

    cached.retain(|dir| !dir.dir.starts_with(subtree) || refreshed.contains_key(&dir.dir));
    for dir in cached.iter_mut() {
        if let Some(fresh) = refreshed.get(&dir.dir) {
            *dir = fresh.clone();
        }
    }
    for (path, fresh) in refreshed {
        if !cached.iter().any(|dir| dir.dir == path) {
            cached.push(fresh);
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use crate::store::{InMemoryStore, RecordStore};
    use indexmap::IndexMap;
    use std::fs::File;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};
    use tempfile::TempDir;

    fn sample_dir() -> CachedDir {
        let mut f = IndexMap::new();
        f.insert("status".to_string(), Value::Str("draft".into()));
        f.insert(
            "tags".to_string(),
            Value::List(vec![Value::Str("a".into())]),
        );
        CachedDir {
            dir: PathBuf::from("/v/plans"),
            scanned_at: UNIX_EPOCH + Duration::from_secs(1000),
            dir_mtime: UNIX_EPOCH + Duration::from_secs(900),
            files: vec![CachedFile {
                rel_path: "a.md".into(),
                mtime: UNIX_EPOCH + Duration::from_secs(800),
                size: 42,
                fields: f,
            }],
        }
    }

    /// [`sample_dir`] relocated to `dir`, for tests that need real,
    /// distinct filesystem paths (blob names are derived from the path).
    fn sample_dir_at(dir: PathBuf) -> CachedDir {
        CachedDir {
            dir,
            ..sample_dir()
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
        let body = ManifestBody {
            crate_version: "0.1.0".into(),
            ttl_secs: 300,
            dirs: vec![],
        };
        let bytes = write_manifest_bytes(&body);
        assert_eq!(&bytes[0..4], &MAGIC);
        assert_eq!(read_manifest_bytes(&bytes), Some(body));
    }
    #[test]
    fn wrong_magic_rejected() {
        let mut bytes = write_manifest_bytes(&ManifestBody {
            crate_version: "x".into(),
            ttl_secs: 1,
            dirs: vec![],
        });
        bytes[0] = b'Z';
        assert_eq!(read_manifest_bytes(&bytes), None);
    }
    #[test]
    fn wrong_version_rejected() {
        let body = ManifestBody {
            crate_version: "x".into(),
            ttl_secs: 1,
            dirs: vec![],
        };
        let mut bytes = write_manifest_bytes(&body);
        bytes[4..8].copy_from_slice(&(SCHEMA_VERSION + 1).to_le_bytes());
        assert_eq!(read_manifest_bytes(&bytes), None);
    }
    #[test]
    fn garbage_rejected() {
        assert_eq!(read_manifest_bytes(b"xx"), None);
        assert_eq!(read_manifest_bytes(&[]), None);
    }

    #[test]
    fn write_atomic_writes_bytes_and_leaves_no_temp_file() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("out.bin");
        write_atomic(&path, b"hello").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"hello");
        let stray_files: Vec<_> = fs::read_dir(td.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name() != path.file_name().unwrap())
            .collect();
        assert!(
            stray_files.is_empty(),
            "leftover temp file: {stray_files:?}"
        );
    }

    #[test]
    fn save_then_load_roundtrips() {
        let td = TempDir::new().unwrap();
        let dirs = vec![sample_dir_at(td.path().join("plans"))];
        save_cache(td.path(), &dirs, 300).unwrap();
        assert!(td.path().join(".querymatter/manifest.bin").is_file());
        let (body, loaded) = load_cache(td.path()).unwrap();
        assert_eq!(body.ttl_secs, 300);
        assert_eq!(loaded, dirs);
    }

    #[test]
    fn missing_manifest_is_none() {
        let td = TempDir::new().unwrap();
        assert!(load_cache(td.path()).is_none());
    }

    #[test]
    fn incompatible_manifest_is_none() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join(".querymatter")).unwrap();
        fs::write(
            td.path().join(".querymatter/manifest.bin"),
            b"NOPEnotaversion",
        )
        .unwrap();
        assert!(load_cache(td.path()).is_none());
    }

    #[test]
    fn corrupt_blob_skips_only_that_dir() {
        // Save two dirs, then clobber one blob with garbage; load returns
        // only the other, not an error.
        let td = TempDir::new().unwrap();
        let good = sample_dir_at(td.path().join("plans"));
        let bad = sample_dir_at(td.path().join("notes"));
        save_cache(td.path(), &[good.clone(), bad.clone()], 300).unwrap();

        let (body, _) = load_cache(td.path()).unwrap();
        let bad_entry = body.dirs.iter().find(|e| e.dir == bad.dir).unwrap();
        fs::write(
            td.path().join(".querymatter").join(&bad_entry.blob),
            b"not a valid bincode blob",
        )
        .unwrap();

        let (_, loaded) = load_cache(td.path()).unwrap();
        assert_eq!(loaded, vec![good]);
    }

    #[test]
    fn pre_epoch_mtime_is_skipped_not_panicked() {
        // A `CachedDir` whose file mtime predates the Unix epoch (clock
        // skew, `touch -d`, archive extraction) must be skipped, not panic
        // the whole save (carry-forward fix from Task 1's review).
        let td = TempDir::new().unwrap();
        let good = sample_dir_at(td.path().join("plans"));
        let mut ancient = sample_dir_at(td.path().join("ancient"));
        ancient.files[0].mtime = UNIX_EPOCH.checked_sub(Duration::from_secs(1)).unwrap();

        save_cache(td.path(), &[good.clone(), ancient], 300).unwrap();

        let (_, loaded) = load_cache(td.path()).unwrap();
        assert_eq!(loaded, vec![good]);
    }

    #[test]
    fn find_vault_finds_ancestor() {
        let td = TempDir::new().unwrap();
        save_cache(td.path(), &[], 300).unwrap();
        let deep = td.path().join("a/b/c");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            find_vault(&deep),
            Some(fs::canonicalize(td.path()).unwrap())
        );

        let other = TempDir::new().unwrap();
        assert_eq!(find_vault(other.path()), None);
    }

    fn write_file(dir: &Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, body).unwrap();
    }

    fn set_mtime(path: &Path, time: SystemTime) {
        File::open(path).unwrap().set_modified(time).unwrap();
    }

    /// Builds an initial cache for `vault` by refreshing an empty cache
    /// against it — i.e. a first-ever scan, expressed via the same
    /// [`refresh_against_cache`] under test rather than a separate helper.
    fn build_initial_cache(vault: &Path) -> Vec<CachedDir> {
        let (dirs, _report, _changed) =
            refresh_against_cache(vault, &[], &WalkOpts::default(), Freshness::PerFile, 300);
        dirs
    }

    /// The cached `status` field for the file named `file_name`, searched
    /// across every directory.
    fn cached_status(dirs: &[CachedDir], file_name: &str) -> Value {
        dirs.iter()
            .flat_map(|d| &d.files)
            .find(|f| f.rel_path == file_name)
            .and_then(|f| f.fields.get("status").cloned())
            .expect("file not found in cache")
    }

    /// Every cached file's `rel_path`, across every directory, sorted.
    fn all_rel_paths(dirs: &[CachedDir]) -> Vec<String> {
        let mut names: Vec<String> = dirs
            .iter()
            .flat_map(|d| d.files.iter().map(|f| f.rel_path.clone()))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn unchanged_file_reuses_cached_fields_without_reparsing() {
        let td = TempDir::new().unwrap();
        let a_path = td.path().join("a.md");
        let content_a = "---\nstatus: draft\n---\n";
        write_file(td.path(), "a.md", content_a);
        let original_mtime = fs::metadata(&a_path).unwrap().modified().unwrap();

        let cached = build_initial_cache(td.path());
        assert_eq!(cached_status(&cached, "a.md"), Value::Str("draft".into()));

        // Same byte length as "draft" (5 chars), so overwriting keeps size
        // equal; restoring the original mtime then makes (mtime, size)
        // indistinguishable from the cached entry.
        let content_b = "---\nstatus: final\n---\n";
        assert_eq!(
            content_a.len(),
            content_b.len(),
            "fixture must keep byte length equal to isolate mtime as the only signal"
        );
        write_file(td.path(), "a.md", content_b);
        set_mtime(&a_path, original_mtime);

        let (refreshed, report, _changed) = refresh_against_cache(
            td.path(),
            &cached,
            &WalkOpts::default(),
            Freshness::PerFile,
            300,
        );
        assert_eq!(
            cached_status(&refreshed, "a.md"),
            Value::Str("draft".into()),
            "must reuse the cached value, not re-parse the changed content"
        );
        assert_eq!(report.loaded, 1);
        assert_eq!(report.skipped, 0);
    }

    #[test]
    fn changed_mtime_triggers_reparse() {
        let td = TempDir::new().unwrap();
        let a_path = td.path().join("a.md");
        let content_a = "---\nstatus: draft\n---\n";
        write_file(td.path(), "a.md", content_a);
        let original_mtime = fs::metadata(&a_path).unwrap().modified().unwrap();

        let cached = build_initial_cache(td.path());

        let content_b = "---\nstatus: final\n---\n";
        assert_eq!(content_a.len(), content_b.len());
        write_file(td.path(), "a.md", content_b);
        set_mtime(&a_path, original_mtime + Duration::from_secs(120));

        let (refreshed, report, _changed) = refresh_against_cache(
            td.path(),
            &cached,
            &WalkOpts::default(),
            Freshness::PerFile,
            300,
        );
        assert_eq!(
            cached_status(&refreshed, "a.md"),
            Value::Str("final".into())
        );
        assert_eq!(report.loaded, 1);
    }

    #[test]
    fn new_file_added_and_deleted_file_dropped() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.md", "---\nstatus: draft\n---\n");
        let cached = build_initial_cache(td.path());
        assert_eq!(all_rel_paths(&cached), vec!["a.md".to_string()]);

        fs::remove_file(td.path().join("a.md")).unwrap();
        write_file(td.path(), "b.md", "---\nstatus: draft\n---\n");

        let (refreshed, _report, changed) = refresh_against_cache(
            td.path(),
            &cached,
            &WalkOpts::default(),
            Freshness::PerFile,
            300,
        );
        assert_eq!(all_rel_paths(&refreshed), vec!["b.md".to_string()]);
        assert!(changed, "adding/removing a file must count as changed");
    }

    #[test]
    fn force_cache_returns_cached_even_when_file_changed() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.md", "---\nstatus: draft\n---\n");
        let cached = build_initial_cache(td.path());

        write_file(td.path(), "a.md", "---\nstatus: final\n---\n");

        let (refreshed, report, changed) = refresh_against_cache(
            td.path(),
            &cached,
            &WalkOpts::default(),
            Freshness::ForceCache,
            300,
        );
        assert_eq!(
            cached_status(&refreshed, "a.md"),
            Value::Str("draft".into()),
            "--force-cache must never re-read the changed file"
        );
        assert!(!changed);
        assert_eq!(report.loaded, 0);
        assert_eq!(report.skipped, 0);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn fast_skips_dir_with_unchanged_mtime_within_ttl() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.md", "---\nstatus: draft\n---\n");

        let mut cached = build_initial_cache(td.path());
        assert_eq!(cached_status(&cached, "a.md"), Value::Str("draft".into()));

        // Edit the file's CONTENT only (write in place; nothing is added or
        // removed). A content-only edit shouldn't move the *directory's*
        // mtime, but rather than depend on that holding across every
        // platform/filesystem, explicitly pin the cached `dir_mtime` to
        // whatever the directory's real on-disk mtime is right now — that
        // makes the "unchanged" branch deterministic regardless of whether
        // this edit happened to bump it.
        write_file(td.path(), "a.md", "---\nstatus: final\n---\n");
        let dir_mtime_now = fs::metadata(td.path()).unwrap().modified().unwrap();
        for dir in &mut cached {
            dir.dir_mtime = dir_mtime_now;
        }

        let (refreshed, report, changed) = refresh_against_cache(
            td.path(),
            &cached,
            &WalkOpts::default(),
            Freshness::Fast,
            300,
        );

        assert_eq!(
            cached_status(&refreshed, "a.md"),
            Value::Str("draft".into()),
            "Fast must reuse the stale cached value within TTL, proving it skipped stat-ing files"
        );
        assert_eq!(report.loaded, 1);
        assert_eq!(report.skipped, 0);
        assert!(
            !changed,
            "an unchanged dir_mtime within TTL must reuse the CachedDir verbatim"
        );
    }

    #[test]
    fn fast_rescans_dir_when_mtime_moved() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "a.md", "---\nstatus: draft\n---\n");
        let cached = build_initial_cache(td.path());
        assert_eq!(all_rel_paths(&cached), vec!["a.md".to_string()]);

        // Adding a file reliably bumps the directory's mtime, so Fast must
        // fall back to a per-file re-scan for this directory even though
        // it's well within TTL.
        write_file(td.path(), "b.md", "---\nstatus: draft\n---\n");

        let (refreshed, report, changed) = refresh_against_cache(
            td.path(),
            &cached,
            &WalkOpts::default(),
            Freshness::Fast,
            300,
        );

        assert_eq!(
            all_rel_paths(&refreshed),
            vec!["a.md".to_string(), "b.md".to_string()],
            "Fast must pick up the new file once dir_mtime has moved"
        );
        assert_eq!(report.loaded, 2);
        assert_eq!(report.skipped, 0);
        assert!(changed, "a newly added file must count as changed");
    }

    #[test]
    fn build_vault_writes_a_loadable_cache() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "plans/a.md", "---\nstatus: draft\n---\n");
        write_file(td.path(), "product/b.md", "---\nstatus: shipped\n---\n");

        let report = build_vault(td.path(), &WalkOpts::default(), 300).unwrap();
        assert_eq!(report.loaded, 2);
        assert_eq!(report.skipped, 0);

        let (body, loaded) = load_cache(td.path()).unwrap();
        assert_eq!(body.ttl_secs, 300);
        assert_eq!(
            all_rel_paths(&loaded),
            vec!["a.md".to_string(), "b.md".to_string()]
        );
        assert_eq!(cached_status(&loaded, "a.md"), Value::Str("draft".into()));
        assert_eq!(cached_status(&loaded, "b.md"), Value::Str("shipped".into()));
    }

    #[test]
    fn refresh_subtree_reparses_only_that_subtree() {
        let td = TempDir::new().unwrap();
        write_file(td.path(), "plans/a.md", "---\nstatus: draft\n---\n");
        write_file(td.path(), "product/b.md", "---\nstatus: draft\n---\n");

        let mut cached = build_initial_cache(td.path());
        assert_eq!(cached_status(&cached, "a.md"), Value::Str("draft".into()));
        assert_eq!(cached_status(&cached, "b.md"), Value::Str("draft".into()));

        write_file(td.path(), "plans/a.md", "---\nstatus: final\n---\n");
        write_file(td.path(), "product/b.md", "---\nstatus: final\n---\n");

        let report = refresh_subtree(
            td.path(),
            &mut cached,
            &td.path().join("plans"),
            &WalkOpts::default(),
        );

        assert_eq!(
            cached_status(&cached, "a.md"),
            Value::Str("final".into()),
            "plans/ was refreshed"
        );
        assert_eq!(
            cached_status(&cached, "b.md"),
            Value::Str("draft".into()),
            "product/ must be untouched by a refresh scoped to plans/"
        );
        assert_eq!(report.loaded, 1);
        assert_eq!(report.skipped, 0);
    }

    #[test]
    fn cached_record_matches_live_scan_record() {
        // Cache-equals-live invariant (design spec §4, and this task's own
        // "Global constraints"): a record rebuilt from an unchanged
        // CachedFile must be identical — same fields, same file.* — to a
        // live scan's record for the same file. Explicitly pinned here
        // rather than assumed, per the "no test needed because of invariant
        // X" red flag: this invariant is exactly what `records_from`'s
        // `root` parameter exists to preserve.
        let td = TempDir::new().unwrap();
        write_file(td.path(), "plans/a.md", "---\nstatus: draft\n---\n");

        let cached = build_initial_cache(td.path());
        let cached_records: Vec<Record> = records_from(td.path(), &cached)
            .into_iter()
            .flat_map(|(_, records)| records)
            .collect();

        let (live_store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        let live_records: Vec<&Record> = live_store.records().collect();

        assert_eq!(cached_records.len(), 1);
        assert_eq!(live_records.len(), 1);
        assert_eq!(&cached_records[0], live_records[0]);
    }
}
