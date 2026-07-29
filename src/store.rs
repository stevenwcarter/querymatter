//! Ties directory discovery and frontmatter extraction into a queryable,
//! directory-keyed record store.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::cache::{self, CachedDir, CachedFile, Freshness, ScanResult};
use crate::discover::{self, WalkOpts};
use crate::model::{FileAttr, Record};
use crate::parallel;
use crate::paths::{DirPath, FilePath, VaultRoot};

/// Summary of a load/reload operation: how many files became records, how
/// many were skipped (no valid frontmatter, or unreadable), and a
/// human-readable warning for each skip.
#[derive(Debug, Default)]
pub struct LoadReport {
    pub loaded: usize,
    pub skipped: usize,
    pub warnings: Vec<String>,
}

impl LoadReport {
    /// Folds `other`'s counts and warnings into this report, used when
    /// combining several roots' reports into one.
    fn merge(&mut self, other: LoadReport) {
        self.loaded += other.loaded;
        self.skipped += other.skipped;
        self.warnings.extend(other.warnings);
    }
}

/// TTL (seconds) assumed when [`InMemoryStore::from_cache`] or
/// [`RecordStore::refresh`] finds no on-disk `.querymatter` manifest to
/// read a real `ttl_secs` from. Matches the default used elsewhere for a
/// fresh vault (design spec §3); only ever consulted by [`Freshness::Fast`].
const DEFAULT_TTL_SECS: u64 = 300;

/// The records discovered under one scan root, plus when that scan ran.
///
/// Slices are strictly per-root: [`InMemoryStore::reload_dir`] rebuilds
/// exactly one slice and leaves the rest untouched. This is the seam a
/// future TTL-based cache reuses (spec §9) — don't collapse slices into one
/// flat `Vec<Record>`.
#[derive(Debug)]
pub struct DirSlice {
    pub root: PathBuf,
    pub records: Vec<Record>,
    pub scanned_at: SystemTime,
    /// The full frontmatter field-NAME union for this slice, independent of
    /// which field VALUES `records` actually carry.
    ///
    /// Populated during scan ([`scan_root`]) or cache materialization
    /// ([`cache::records_from`]) from every discovered file's real fields,
    /// before projection push-down (design W17) narrows `records`' own value
    /// maps to only the fields a known-in-advance query references. Keeping
    /// this separate from `records` is what lets [`RecordStore::schema`]
    /// report the FULL schema even when most values were pruned away —
    /// load-bearing for W12's unknown-column validation/did-you-mean, which
    /// checks a query's referenced columns against `schema()`.
    field_names: BTreeSet<String>,
}

/// A queryable collection of [`Record`]s, grouped into directory-keyed
/// slices that can be reloaded independently.
pub trait RecordStore {
    /// All records across every slice, in slice order.
    fn records(&self) -> Box<dyn Iterator<Item = &Record> + '_>;
    /// The sorted union of frontmatter field names across all records.
    ///
    /// Sorted rather than first-seen: gray_matter's YAML engine hands back
    /// fields in `HashMap` order, so sorting is the only way this (and
    /// later `SELECT *`) is deterministic.
    fn schema(&self) -> Vec<String>;
    /// Rebuilds the slice for `root` and refreshes its `scanned_at`,
    /// leaving every other slice untouched. Appends a new slice if `root`
    /// isn't already tracked.
    fn reload_dir(&mut self, root: &Path) -> LoadReport;
    /// Reloads every currently tracked root, one slice at a time.
    fn reload_all(&mut self) -> LoadReport;
    /// The roots currently tracked, in slice order.
    fn roots(&self) -> Vec<PathBuf>;
    /// Forces a fresh read of `subtree` (or the whole vault, when `None`)
    /// against live storage, updating in place and persisting when the
    /// implementation is backed by an on-disk cache. Callable through
    /// `Box<dyn RecordStore>`, this is what the REPL's `.refresh`/
    /// `.refresh-all` and the CLI's `--refresh`/`--refresh-all` dispatch to.
    fn refresh(&mut self, vault: &VaultRoot, subtree: Option<&Path>) -> LoadReport;
}

/// A [`RecordStore`] that keeps every loaded record in memory, partitioned
/// into one [`DirSlice`] per scan root.
pub struct InMemoryStore {
    slices: Vec<DirSlice>,
    opts: WalkOpts,
}

impl InMemoryStore {
    /// Loads every root in `roots` with `opts`, returning the populated
    /// store and a [`LoadReport`] combined across all roots.
    ///
    /// `wanted` implements projection push-down (design W17): `None` keeps
    /// every field's value on every [`Record`] (today's behavior, and always
    /// what the interactive REPL passes — its store outlives any one query);
    /// `Some(set)` keeps only the values whose key is in `set`, for a
    /// one-shot/batch/`query run` invocation whose statement(s) are known
    /// before the store is built. [`RecordStore::schema`] stays the FULL
    /// field-name union regardless — see [`DirSlice::field_names`].
    pub fn load(
        roots: Vec<PathBuf>,
        opts: WalkOpts,
        wanted: Option<&BTreeSet<String>>,
    ) -> (Self, LoadReport) {
        let mut store = InMemoryStore {
            slices: Vec::new(),
            opts,
        };
        let mut report = LoadReport::default();
        for root in roots {
            let vault_root = VaultRoot::new(root.clone());
            let (records, field_names, slice_report) = scan_root(&vault_root, &store.opts, wanted);
            report.merge(slice_report);
            store.slices.push(DirSlice {
                root,
                records,
                scanned_at: SystemTime::now(),
                field_names,
            });
        }
        (store, report)
    }

    /// Builds a store from the `.querymatter` cache under `vault`, refreshing
    /// it against the live filesystem per `mode` and returning the populated
    /// store alongside a [`LoadReport`] of what was (re)loaded/skipped.
    ///
    /// A missing or incompatible on-disk cache ([`cache::load_cache`]
    /// returning `None`) is treated as an empty one with [`DEFAULT_TTL_SECS`],
    /// so the freshness pass below rebuilds it by scanning every file. When
    /// that pass reports the result changed, it's persisted back via
    /// [`cache::save_cache`] — unless `mode` is [`Freshness::ForceCache`],
    /// which never touches the filesystem beyond the initial cache read and
    /// so never has anything new to persist. A save failure is folded into
    /// the report's warnings rather than panicking.
    ///
    /// The two `None` causes are distinguished (design spec §9): when a
    /// `manifest.bin` is present on disk but [`cache::load_cache`] rejected it
    /// (incompatible schema or corruption), a warning is prepended to the
    /// report — `main` prints report warnings to stderr — so the user sees
    /// *why* every run is doing a slow full rebuild. With no manifest at all
    /// (a fresh vault, or the unit-test path) the rebuild stays silent.
    ///
    /// `wanted` is the same projection push-down parameter [`InMemoryStore::load`]
    /// takes — see its doc comment. It only affects which field VALUES end up
    /// on the returned store's [`Record`]s; the on-disk cache written by
    /// [`cache::save_cache`] above is built from `fresh` (every field,
    /// straight from [`cache::refresh_against_cache`]) and is never pruned.
    ///
    /// `scope` is subtree scoping (design W26 / spec §7): `None` loads the
    /// whole vault (today's behavior, byte-identical). `Some(dirs)` loads
    /// ONLY the cache directories at/under one of `dirs` — the blob decode is
    /// scoped ([`cache::load_cache_under`]) *and* the freshness re-walk is
    /// scoped ([`cache::refresh_against_cache_scoped`]), so the query pays
    /// O(subtree), not O(vault). The store's `schema()` then derives from the
    /// (scoped) loaded records, becoming the subtree's schema — an accepted,
    /// documented narrowing of the W12 validation surface (spec §7.3).
    ///
    /// A scoped load is **not** persisted: [`cache::save_cache`] rewrites the
    /// manifest wholesale from what it is handed, and a scoped `fresh` holds
    /// only the subtree, so saving it would drop every out-of-subtree
    /// directory from the manifest. A scoped query is a read — it refreshes in
    /// memory for a correct result and leaves cache maintenance to the
    /// whole-vault path. Hence the save below stays gated on `scope.is_none()`.
    pub fn from_cache(
        vault: &VaultRoot,
        opts: WalkOpts,
        mode: Freshness,
        wanted: Option<&BTreeSet<String>>,
        scope: Option<&[PathBuf]>,
    ) -> (Self, LoadReport) {
        let (cached, ttl_secs, incompatible) = match cache::load_cache_under(vault, scope) {
            Some((body, dirs)) => (dirs, body.ttl_secs, false),
            // A `None` with a manifest present means it's unreadable; without
            // one it's simply a fresh/absent cache (stay silent).
            None => (Vec::new(), DEFAULT_TTL_SECS, cache::manifest_exists(vault)),
        };

        let (fresh, mut report, changed) =
            cache::refresh_against_cache_scoped(vault, &cached, &opts, mode, ttl_secs, scope);

        if incompatible {
            report.warnings.insert(
                0,
                format!(
                    "incompatible or unreadable cache at {} — rebuilding",
                    cache::cache_dir(vault).display()
                ),
            );
        }

        if scope.is_none()
            && changed
            && mode != Freshness::ForceCache
            && let Err(err) = cache::save_cache(vault, &fresh, ttl_secs)
        {
            report.warnings.push(format!("saving cache: {err}"));
        }

        let (slices, slices_report) = slices_from_cached(vault, &fresh, wanted);
        report.merge(slices_report);
        (InMemoryStore { slices, opts }, report)
    }

    /// Restricts the store to the slices whose `root` lies at or under at
    /// least one path in `dirs`, dropping the rest. Honors positional
    /// `[DIRS]` on a vault-backed query (design spec §5): the vault is loaded
    /// whole, then narrowed to the named subtrees at slice (directory)
    /// granularity.
    ///
    /// `dirs` must be absolute/canonical (the caller canonicalizes them). An
    /// empty `dirs` is a no-op — the whole vault is kept. A `dirs` entry that
    /// lies entirely outside the vault matches no slice, so its records are
    /// simply absent: v1 does not live-scan outside-vault directories (a
    /// known limitation — such a dir contributes nothing rather than being
    /// scanned fresh).
    pub fn retain_under(&mut self, dirs: &[PathBuf]) {
        if dirs.is_empty() {
            return;
        }
        self.slices
            .retain(|slice| dirs.iter().any(|dir| slice.root.starts_with(dir)));
    }

    /// Reconstructs a fine-grained `Vec<CachedDir>` — one entry per
    /// immediate parent directory, the same granularity
    /// [`cache::refresh_subtree`] computes internally — from this store's
    /// current in-memory slices. Used by [`RecordStore::refresh`] as a
    /// fallback when no on-disk cache exists yet, so a directory outside the
    /// requested subtree is carried forward untouched rather than dropped.
    ///
    /// The `mtime`/`size`/`scanned_at`/`dir_mtime` fields are placeholders
    /// (`UNIX_EPOCH`/`0`): an entry built here is only ever consulted by
    /// [`cache::refresh_subtree`] for a directory *outside* the subtree being
    /// refreshed, where it's carried through unread rather than compared
    /// against — so its stat accuracy doesn't matter, only that it
    /// round-trips through [`cache::save_cache`] cleanly. Unlike those stats,
    /// `fields` and `word_count` are read straight off the existing `Record`
    /// (no extra I/O), so a subsequent read of the persisted cache still
    /// answers `file.word_count` correctly rather than a stale `0`.
    ///
    /// Invariant this depends on: `self.slices`' records must carry every
    /// field, not a projection-push-down-pruned subset (design W17) — a
    /// pruned record's missing fields would round-trip through
    /// [`cache::save_cache`] as genuinely lost, not merely re-derivable,
    /// corrupting the on-disk cache for directories outside the refreshed
    /// subtree. Holds today because every caller that can reach this
    /// fallback (this crate's `RecordStore::refresh` callers) only does so
    /// for a store built with `wanted = None`: push-down is confined to
    /// [`crate::main`]'s one-shot/batch/`query run` construction, and those
    /// never chain into this no-on-disk-cache fallback within the same run
    /// (`InMemoryStore::from_cache` always repairs/persists a valid manifest
    /// before any subsequent [`RecordStore::refresh`] call can need it).
    fn cached_dirs_from_slices(&self) -> Vec<CachedDir> {
        let mut by_dir: BTreeMap<PathBuf, Vec<CachedFile>> = BTreeMap::new();
        for slice in &self.slices {
            for record in &slice.records {
                let rel = record.file_attr(FileAttr::Path).display();
                let abs = slice.root.join(&rel);
                let dir = abs
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| slice.root.clone());
                let rel_path = abs
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or(rel);
                let fields = record
                    .field_names()
                    .map(|name| (name.to_string(), record.field(&[name.into()])))
                    .collect();
                by_dir.entry(dir).or_default().push(CachedFile {
                    rel_path,
                    mtime: SystemTime::UNIX_EPOCH,
                    size: 0,
                    fields,
                    word_count: record.word_count(),
                });
            }
        }

        by_dir
            .into_iter()
            .map(|(dir, files)| CachedDir {
                dir: DirPath::new(dir),
                scanned_at: SystemTime::UNIX_EPOCH,
                dir_mtime: SystemTime::UNIX_EPOCH,
                files,
            })
            .collect()
    }
}

/// Builds one [`DirSlice`] per cached directory in `dirs` via
/// [`cache::records_from`], stamping each with the current time, alongside
/// the [`LoadReport`] [`cache::records_from`] returns (e.g. any `CachedFile`
/// rejected as an unsafe cached `rel_path` — B6). Shared by
/// [`InMemoryStore::from_cache`] and [`RecordStore::refresh`], both of
/// which rebuild the store's slices from a freshly refreshed `Vec<CachedDir>`.
///
/// `wanted` is forwarded straight to [`cache::records_from`] — see its doc
/// comment for what it prunes and why `schema()` stays complete regardless.
fn slices_from_cached(
    vault: &VaultRoot,
    dirs: &[CachedDir],
    wanted: Option<&BTreeSet<String>>,
) -> (Vec<DirSlice>, LoadReport) {
    let now = SystemTime::now();
    let (entries, report) = cache::records_from(vault, dirs, wanted);
    let slices = entries
        .into_iter()
        .map(|(root, records, field_names)| DirSlice {
            root,
            records,
            scanned_at: now,
            field_names,
        })
        .collect();
    (slices, report)
}

impl RecordStore for InMemoryStore {
    fn records(&self) -> Box<dyn Iterator<Item = &Record> + '_> {
        Box::new(self.slices.iter().flat_map(|slice| slice.records.iter()))
    }

    /// The full field-name union across every slice's [`DirSlice::field_names`]
    /// — tracked independently of `records`' own (possibly push-down-pruned)
    /// field values, so this stays complete even when most VALUES were
    /// pruned away for a narrow one-shot query.
    fn schema(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        for slice in &self.slices {
            names.extend(slice.field_names.iter().cloned());
        }
        names.into_iter().collect()
    }

    /// REPL-only in practice ([`Session::reload`](crate::session::Session::reload));
    /// always passes `wanted = None` to [`scan_root`] — push-down never
    /// applies to a store that outlives a single query.
    fn reload_dir(&mut self, root: &Path) -> LoadReport {
        let vault_root = VaultRoot::new(root.to_path_buf());
        let (records, field_names, report) = scan_root(&vault_root, &self.opts, None);
        if let Some(slice) = self.slices.iter_mut().find(|slice| slice.root == root) {
            slice.records = records;
            slice.field_names = field_names;
            slice.scanned_at = SystemTime::now();
        } else {
            self.slices.push(DirSlice {
                root: root.to_path_buf(),
                records,
                scanned_at: SystemTime::now(),
                field_names,
            });
        }
        report
    }

    fn reload_all(&mut self) -> LoadReport {
        let mut report = LoadReport::default();
        for root in self.roots() {
            report.merge(self.reload_dir(&root));
        }
        report
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.slices.iter().map(|slice| slice.root.clone()).collect()
    }

    /// The starting point is the on-disk cache ([`cache::load_cache`]) when
    /// one exists; otherwise it's rebuilt from the store's own current
    /// slices ([`InMemoryStore::cached_dirs_from_slices`]) so that
    /// directories outside `subtree` aren't silently dropped.
    ///
    /// Both arms force a full re-parse via [`cache::refresh_subtree`],
    /// ignoring every cached `(mtime, size)` shortcut: a `Some(subtree)`
    /// re-scans just that directory tree, and `None` re-scans the whole vault
    /// (by passing `vault` itself as the subtree). This is the forced
    /// re-scan `--refresh <path>` and `--refresh-all` both promise (spec §4);
    /// the incremental freshness shortcuts live on the read path
    /// ([`InMemoryStore::from_cache`]), not here.
    ///
    /// Always rematerializes with `wanted = None` (every field): projection
    /// push-down only ever prunes a store's INITIAL construction
    /// ([`InMemoryStore::load`]/[`from_cache`](InMemoryStore::from_cache)),
    /// not a later refresh — see
    /// [`cached_dirs_from_slices`](InMemoryStore::cached_dirs_from_slices)'s
    /// doc comment for why that invariant matters here.
    fn refresh(&mut self, vault: &VaultRoot, subtree: Option<&Path>) -> LoadReport {
        let (mut cached, ttl_secs) = match cache::load_cache(vault) {
            Some((body, dirs)) => (dirs, body.ttl_secs),
            None => (self.cached_dirs_from_slices(), DEFAULT_TTL_SECS),
        };

        // `None` (whole-vault) re-scans every directory under `vault` by using
        // `vault` itself as the subtree, matching the forced re-parse a
        // `Some(subtree)` already performs.
        let subtree = subtree.unwrap_or(vault);
        let mut report = cache::refresh_subtree(vault, &mut cached, subtree, &self.opts);

        if let Err(err) = cache::save_cache(vault, &cached, ttl_secs) {
            report.warnings.push(format!("saving cache: {err}"));
        }

        let (slices, slices_report) = slices_from_cached(vault, &cached, None);
        self.slices = slices;
        report.merge(slices_report);
        report
    }
}

/// Scans `root` for frontmatter records using `opts`, returning the loaded
/// records alongside a report of how many files were skipped and why.
///
/// Delegates the per-file work to [`cache::scan_file`] — the single
/// "file → record" definition shared with the cache's freshness checks —
/// reusing the on-disk stat it already carries as each [`Record`]'s
/// `file.mtime`/`file.size` rather than stat-ing again. A file that fails to
/// read from disk is treated like invalid frontmatter: it's counted as
/// skipped and warned about, not a hard error.
///
/// The read+parse itself runs across [`parallel::map_paths`]'s worker
/// threads, but the results are folded into `records`/`report` in
/// `discover`'s path-sorted order (the order `map_paths` returns them in),
/// so the outcome is byte-for-byte identical to the old serial loop — just
/// not bottlenecked on one core.
///
/// `wanted` implements projection push-down (design W17): `None` keeps every
/// field's value on each [`Record`] (today's behavior); `Some(set)` keeps
/// only the values whose key is in `set`. Either way, the returned
/// `BTreeSet<String>` is the FULL field-name union this scan discovered —
/// every key seen in a parsed file's frontmatter, before pruning — so a
/// caller can retain it as the store's true schema regardless of `wanted`.
fn scan_root(
    root: &VaultRoot,
    opts: &WalkOpts,
    wanted: Option<&BTreeSet<String>>,
) -> (Vec<Record>, BTreeSet<String>, LoadReport) {
    let paths = discover::discover(root, opts);
    // `store::scan_root` legitimately scans every file directly against the
    // vault root rather than each file's immediate parent — see
    // `DirPath::from_root`'s doc comment.
    let dir = DirPath::from_root(root);
    let scanned = parallel::map_paths(paths, |path| {
        cache::scan_file(
            &dir,
            &FilePath::new(path.to_path_buf()),
            opts.max_file_bytes,
        )
    });

    let mut records = Vec::new();
    let mut field_names = BTreeSet::new();
    let mut report = LoadReport::default();
    for (path, result) in scanned {
        match result {
            ScanResult::Cached(file) => {
                field_names.extend(file.fields.keys().cloned());
                let mtime = file.mtime;
                let size = file.size;
                let word_count = file.word_count;
                let fields = match wanted {
                    None => file.fields,
                    Some(set) => file
                        .fields
                        .into_iter()
                        .filter(|(name, _)| set.contains(name))
                        .collect(),
                };
                let file_path = FilePath::new(path);
                records.push(Record::new(
                    root, &file_path, fields, mtime, size, word_count,
                ));
                report.loaded += 1;
            }
            ScanResult::NoFrontmatter => {}
            ScanResult::Warning(msg) => {
                report.skipped += 1;
                report.warnings.push(msg);
            }
        }
    }

    (records, field_names, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use std::fs;
    use std::fs::File;
    use tempfile::TempDir;

    fn write(dir: &std::path::Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    /// Runs `SELECT file.size, file.mtime` over `store`'s records and checks
    /// the single row's shape: `file.size` a positive `Int` (the real
    /// on-disk byte count, never a placeholder `0`), `file.mtime` a `Str`
    /// starting with a 4-digit year and `-` (an RFC3339 timestamp). Shared
    /// by the live-scan and cache-path producer-parity tests below (Task 4,
    /// spec §9): both producers must expose these columns identically.
    fn assert_file_size_and_mtime_row(store: &InMemoryStore) {
        let parsed = crate::query::parse("SELECT file.size, file.mtime").unwrap();
        let result = crate::query::execute(&parsed, store.records(), false).unwrap();
        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];

        let Value::Int(size) = row[0] else {
            panic!("file.size must be Value::Int, got {:?}", row[0]);
        };
        assert!(size > 0, "file.size must be the real on-disk byte count");

        let Value::Str(mtime) = &row[1] else {
            panic!("file.mtime must be Value::Str, got {:?}", row[1]);
        };
        assert!(
            mtime.len() >= 5
                && mtime.as_bytes()[4] == b'-'
                && mtime.as_bytes()[..4].iter().all(u8::is_ascii_digit),
            "file.mtime must be an RFC3339 string starting with a 4-digit year, got {mtime:?}"
        );
    }

    /// Producer-parity guard, LIVE half (Task 4, spec §9): `scan_root` must
    /// surface the file's real on-disk `(mtime, size)` stat through
    /// `file.size`/`file.mtime`, not a placeholder — this is the stat
    /// `cache::scan_file` already read, threaded through at zero extra I/O.
    #[test]
    fn scan_root_exposes_file_mtime_and_size() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");

        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);

        assert_file_size_and_mtime_row(&store);
    }

    /// Producer-parity guard, CACHE half (Task 4, spec §9): `records_from`
    /// (behind `InMemoryStore::from_cache`) must expose `file.size`/
    /// `file.mtime` identically to the live scan, threading through the
    /// `CachedFile`'s stored `(mtime, size)` rather than a placeholder.
    #[test]
    fn from_cache_exposes_file_mtime_and_size() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");
        let vault = VaultRoot::new(td.path().to_path_buf());
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let (store, _report) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile, None, None);

        assert_file_size_and_mtime_row(&store);
    }

    /// A known fixture body: five whitespace-separated words after the
    /// frontmatter fence. Shared by the live-scan and cache-path
    /// `file.word_count` producer-parity tests below (Task 6, W56).
    const WORD_COUNT_FIXTURE_BODY: &str = "---\nstatus: draft\n---\none two three four five\n";
    const WORD_COUNT_FIXTURE_COUNT: i64 = 5;

    /// Runs `SELECT file.word_count` over `store`'s records and checks the
    /// single row matches [`WORD_COUNT_FIXTURE_COUNT`] — pinning that both
    /// producers (live scan and on-disk cache) expose the real body word
    /// count, not a placeholder `0`.
    fn assert_file_word_count_row(store: &InMemoryStore) {
        let parsed = crate::query::parse("SELECT file.word_count").unwrap();
        let result = crate::query::execute(&parsed, store.records(), false).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int(WORD_COUNT_FIXTURE_COUNT));
    }

    /// Producer-parity guard, LIVE half (Task 6, W56): `scan_root` must
    /// surface the body's real word count through `file.word_count`, not a
    /// placeholder `0` — the count `cache::scan_file` already computed via
    /// `frontmatter::extract`, threaded through at zero extra work.
    #[test]
    fn scan_root_exposes_file_word_count() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", WORD_COUNT_FIXTURE_BODY);

        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);

        assert_file_word_count_row(&store);
    }

    /// Producer-parity guard, CACHE half (Task 6, W56): `records_from`
    /// (behind `InMemoryStore::from_cache`) must expose `file.word_count`
    /// identically to the live scan, threading through the persisted
    /// `CachedFile::word_count` rather than a placeholder.
    #[test]
    fn from_cache_exposes_file_word_count() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", WORD_COUNT_FIXTURE_BODY);
        let vault = VaultRoot::new(td.path().to_path_buf());
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let (store, _report) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile, None, None);

        assert_file_word_count_row(&store);
    }

    #[test]
    fn from_cache_matches_live_scan() {
        let td = TempDir::new().unwrap();
        write(td.path(), "plans/a.md", "---\nstatus: draft\n---\n");
        write(td.path(), "product/b.md", "---\nstatus: shipped\n---\n");
        let vault = VaultRoot::new(td.path().to_path_buf());
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let (cached_store, report) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile, None, None);
        assert_eq!(report.skipped, 0);

        let (live_store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);

        let mut cached: Vec<&Record> = cached_store.records().collect();
        let mut live: Vec<&Record> = live_store.records().collect();
        cached.sort_by_key(|r| r.file_attr(FileAttr::Path).display());
        live.sort_by_key(|r| r.file_attr(FileAttr::Path).display());

        assert_eq!(
            cached, live,
            "from_cache(PerFile) must yield the same records as a live scan"
        );
    }

    /// W26 load-bearing equality (spec §9): for an in-subtree query, building
    /// the store with `scope = Some([plans])` must yield the SAME records as
    /// the old path — load the whole vault, then `retain_under([plans])`. The
    /// scoped path replaces the second for scoped queries, so their results
    /// must be identical.
    #[test]
    fn scoped_load_matches_whole_vault_then_retain() {
        let td = TempDir::new().unwrap();
        let vault = VaultRoot::new(fs::canonicalize(td.path()).unwrap());
        write(&vault, "plans/a.md", "---\nstatus: draft\n---\n");
        write(&vault, "plans/nested/b.md", "---\nstatus: synced\n---\n");
        write(&vault, "product/c.md", "---\nstatus: shipped\n---\n");
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let scope = [vault.join("plans")];

        let (scoped_store, _r) = InMemoryStore::from_cache(
            &vault,
            WalkOpts::default(),
            Freshness::PerFile,
            None,
            Some(&scope),
        );
        let (mut whole_store, _r) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile, None, None);
        whole_store.retain_under(&scope);

        let mut scoped: Vec<&Record> = scoped_store.records().collect();
        let mut whole: Vec<&Record> = whole_store.records().collect();
        scoped.sort_by_key(|r| r.file_attr(FileAttr::Path).display());
        whole.sort_by_key(|r| r.file_attr(FileAttr::Path).display());

        assert_eq!(
            scoped, whole,
            "a scoped in-subtree load must match whole-vault-then-retain_under"
        );
        // And it really is scoped: only the two plans/ records, never product/.
        assert_eq!(scoped.len(), 2);
        assert!(
            scoped.iter().all(|r| !matches!(
                r.file_attr(FileAttr::Path),
                Value::Str(ref p) if p.contains("product")
            )),
            "a plans-scoped load must never surface a product/ record"
        );
    }

    /// W26 accepted behavior change (spec §7.3): because the scoped load never
    /// decodes out-of-subtree files, `schema()` becomes the SUBTREE's schema.
    /// A column present only under `product/` is absent from a `plans`-scoped
    /// `schema()`, so a default-mode query for it errors as an unknown column,
    /// while `--lenient` still bypasses validation.
    #[test]
    fn scoped_schema_is_subtree_only() {
        let td = TempDir::new().unwrap();
        let vault = VaultRoot::new(fs::canonicalize(td.path()).unwrap());
        write(&vault, "plans/a.md", "---\nstatus: draft\n---\n");
        // `roadmap` exists ONLY under product/.
        write(&vault, "product/b.md", "---\nroadmap: q3\n---\n");
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let scope = [vault.join("plans")];
        let (store, _r) = InMemoryStore::from_cache(
            &vault,
            WalkOpts::default(),
            Freshness::PerFile,
            None,
            Some(&scope),
        );

        let schema = store.schema();
        assert_eq!(
            schema,
            vec!["status".to_string()],
            "a plans-scoped schema must not include the product-only `roadmap`"
        );

        let parsed = crate::query::parse("SELECT roadmap").unwrap();
        assert!(
            crate::query::execute_with_schema(
                &parsed,
                store.records(),
                &schema,
                false,
                true,
                u64::MAX,
            )
            .is_err(),
            "a product-only column must be unknown under a plans-scoped default-mode query"
        );
        assert!(
            crate::query::execute_with_schema(
                &parsed,
                store.records(),
                &schema,
                true,
                true,
                u64::MAX,
            )
            .is_ok(),
            "--lenient must bypass the subtree-scoped validation surface"
        );
    }

    /// The whole-vault (REPL / no-`[DIRS]`) path is untouched by W26: with
    /// `scope = None` the entire vault loads, so `schema()` is the FULL field
    /// union — the exact contrast to `scoped_schema_is_subtree_only`.
    #[test]
    fn unscoped_load_keeps_full_vault_schema() {
        let td = TempDir::new().unwrap();
        let vault = VaultRoot::new(fs::canonicalize(td.path()).unwrap());
        write(&vault, "plans/a.md", "---\nstatus: draft\n---\n");
        write(&vault, "product/b.md", "---\nroadmap: q3\n---\n");
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let (store, _r) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile, None, None);
        assert_eq!(
            store.schema(),
            vec!["roadmap".to_string(), "status".to_string()],
            "an unscoped load must keep the FULL vault schema"
        );
        assert_eq!(store.records().count(), 2);
    }

    #[test]
    fn refresh_picks_up_edits_and_persists() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");
        let vault = VaultRoot::new(td.path().to_path_buf());
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let (mut store, _report) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile, None, None);
        assert_eq!(
            store.records().next().unwrap().field(&["status".into()]),
            Value::Str("draft".into())
        );

        // Different byte length than "draft" (not just different content),
        // so the refresh can't be satisfied by a size-unchanged coincidence
        // if mtime resolution doesn't tick between the two writes.
        write(td.path(), "a.md", "---\nstatus: in-progress\n---\n");

        let report = store.refresh(&vault, None);
        assert_eq!(report.skipped, 0);

        assert_eq!(
            store.records().next().unwrap().field(&["status".into()]),
            Value::Str("in-progress".into()),
            "in-memory records must reflect the edit"
        );

        let (_body, loaded) = cache::load_cache(td.path()).unwrap();
        let persisted_status = loaded
            .iter()
            .flat_map(|dir| &dir.files)
            .find(|file| file.rel_path == "a.md")
            .and_then(|file| file.fields.get("status").cloned())
            .expect("a.md not found in persisted cache");
        assert_eq!(
            persisted_status,
            Value::Str("in-progress".into()),
            "refresh must persist the edit to disk"
        );
    }

    /// FIX 1 (load-bearing): a whole-vault `refresh(vault, None)` must FORCE a
    /// full re-parse, ignoring the per-file `(mtime, size)` freshness
    /// shortcut — matching what `refresh(vault, Some(subtree))` already does
    /// and what spec §4 / the README ("force a full re-scan, ignoring every
    /// freshness shortcut") promise for `--refresh-all`.
    ///
    /// The fixture edits `a.md`'s content to a status of the SAME byte length
    /// (`draft` -> `ready`, both 5 chars) and restores its original mtime, so
    /// `(mtime, size)` are indistinguishable from the cached entry: the
    /// default incremental refresh would REUSE the stale `draft`. Asserting
    /// `ready` proves the whole-vault refresh forced a re-parse regardless.
    #[test]
    fn whole_vault_refresh_forces_reparse_despite_unchanged_mtime_and_size() {
        let td = TempDir::new().unwrap();
        let a_path = td.path().join("a.md");
        let content_a = "---\nstatus: draft\n---\n";
        write(td.path(), "a.md", content_a);
        let original_mtime = fs::metadata(&a_path).unwrap().modified().unwrap();
        let vault = VaultRoot::new(td.path().to_path_buf());
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let (mut store, _report) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile, None, None);
        assert_eq!(
            store.records().next().unwrap().field(&["status".into()]),
            Value::Str("draft".into())
        );

        // Equal byte length keeps `size` unchanged; restoring the original
        // mtime keeps `(mtime, size)` indistinguishable from the cached entry.
        let content_b = "---\nstatus: ready\n---\n";
        assert_eq!(
            content_a.len(),
            content_b.len(),
            "fixture must keep byte length equal to isolate the forced re-parse"
        );
        write(td.path(), "a.md", content_b);
        File::open(&a_path)
            .unwrap()
            .set_modified(original_mtime)
            .unwrap();

        let report = store.refresh(&vault, None);
        assert_eq!(report.skipped, 0);
        assert_eq!(
            store.records().next().unwrap().field(&["status".into()]),
            Value::Str("ready".into()),
            "whole-vault refresh must FORCE a re-parse, not reuse the stale cached value"
        );
    }

    #[test]
    fn force_cache_mode_skips_persist_and_uses_stale_value() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");
        let vault = VaultRoot::new(td.path().to_path_buf());
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        write(td.path(), "a.md", "---\nstatus: final\n---\n");

        let (store, report) = InMemoryStore::from_cache(
            &vault,
            WalkOpts::default(),
            Freshness::ForceCache,
            None,
            None,
        );
        assert_eq!(report.skipped, 0);
        assert_eq!(
            store.records().next().unwrap().field(&["status".into()]),
            Value::Str("draft".into()),
            "ForceCache must never re-read the changed file"
        );

        let (_body, loaded) = cache::load_cache(td.path()).unwrap();
        let persisted_status = loaded
            .iter()
            .flat_map(|dir| &dir.files)
            .find(|file| file.rel_path == "a.md")
            .and_then(|file| file.fields.get("status").cloned())
            .expect("a.md not found in persisted cache");
        assert_eq!(
            persisted_status,
            Value::Str("draft".into()),
            "ForceCache must never persist — the on-disk cache stays at the stale value"
        );
    }

    #[test]
    fn from_cache_with_no_existing_cache_builds_and_persists() {
        let td = TempDir::new().unwrap();
        write(td.path(), "plans/a.md", "---\nstatus: draft\n---\n");
        write(td.path(), "product/b.md", "---\nstatus: shipped\n---\n");
        // No cache::build_vault call: no .querymatter/manifest.bin exists yet.
        assert!(cache::load_cache(td.path()).is_none());

        let vault = VaultRoot::new(td.path().to_path_buf());
        let (cached_store, report) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile, None, None);
        assert_eq!(report.skipped, 0);

        let (live_store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);

        let mut cached: Vec<&Record> = cached_store.records().collect();
        let mut live: Vec<&Record> = live_store.records().collect();
        cached.sort_by_key(|r| r.file_attr(FileAttr::Path).display());
        live.sort_by_key(|r| r.file_attr(FileAttr::Path).display());
        assert_eq!(
            cached, live,
            "from_cache with no prior cache must build correct records via a live scan"
        );

        let (_body, loaded) = cache::load_cache(td.path())
            .expect("from_cache must persist a new cache when none existed");
        assert_eq!(loaded.len(), 2, "one CachedDir per matched directory");
    }

    #[test]
    fn refresh_reconstructs_cached_dirs_when_no_cache_exists() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");

        // Built via InMemoryStore::load (a live scan), never InMemoryStore::from_cache,
        // so no .querymatter/manifest.bin exists on disk. refresh's load_cache
        // lookup below therefore falls back to cached_dirs_from_slices,
        // reconstructing a starting point from this store's own in-memory
        // slices rather than the (nonexistent) on-disk cache.
        let (mut store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);
        assert!(cache::load_cache(td.path()).is_none());

        write(td.path(), "a.md", "---\nstatus: in-progress\n---\n");

        let report = store.refresh(&VaultRoot::new(td.path().to_path_buf()), None);
        assert_eq!(report.skipped, 0);

        assert_eq!(
            store.records().next().unwrap().field(&["status".into()]),
            Value::Str("in-progress".into()),
            "refresh must pick up the edit even when cached_dirs_from_slices \
             (not an on-disk cache) supplies the starting point"
        );
    }

    /// MUST-FIX #2 regression guard: `RecordStore::refresh`'s on-disk-cache-
    /// unavailable fallback (`cached_dirs_from_slices`) reconstructs its
    /// starting point from THIS store's own in-memory slices, then persists
    /// it — so a directory `refresh` never touches keeps its fields only if
    /// those slices were unpruned to begin with. `build_session` (main.rs)
    /// guarantees that by forcing `wanted = None` whenever a refresh flag is
    /// present, regardless of how narrow the triggering query was; this test
    /// pins the invariant directly at the seam the finding identified,
    /// reusing `refresh_reconstructs_cached_dirs_when_no_cache_exists`'s
    /// technique (`InMemoryStore::load`, so no on-disk cache exists and
    /// `refresh` is forced through the fallback) to trigger it
    /// deterministically.
    #[test]
    fn refresh_fallback_preserves_out_of_subtree_fields_when_store_is_unpruned() {
        let td = TempDir::new().unwrap();
        write(td.path(), "plans/a.md", "---\nstatus: draft\n---\n");
        write(td.path(), "product/c.md", "---\nprd: '011'\n---\n");

        // wanted = None: what build_session now always passes to the store
        // build whenever a refresh flag is set.
        let (mut store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);
        assert!(cache::load_cache(td.path()).is_none());

        // Refresh only "plans" — "product" is never rescanned, so its record
        // can only come through intact if the fallback's starting point
        // (this store's own slices) still carried every field.
        let plans = td.path().join("plans");
        store.refresh(&VaultRoot::new(td.path().to_path_buf()), Some(&plans));

        let prd = store
            .records()
            .find(|r| r.field_names().any(|name| name == "prd"))
            .map(|r| r.field(&["prd".into()]));
        assert_eq!(
            prd,
            Some(Value::Str("011".into())),
            "an out-of-subtree field must survive refresh's on-disk-cache- \
             unavailable fallback when the store it falls back to is unpruned"
        );
    }

    /// Companion to the guard above: proves the invariant it pins is real,
    /// not vacuous. Feeding the store a narrow `wanted` — the pre-fix
    /// `build_session` behavior whenever `--refresh` accompanied a narrow
    /// query — DOES lose an out-of-subtree field once `refresh` falls back
    /// to `cached_dirs_from_slices`, which is exactly what `build_session`
    /// forcing `wanted = None` on a refresh now prevents.
    #[test]
    fn refresh_fallback_loses_pruned_out_of_subtree_fields_when_store_is_pruned() {
        let td = TempDir::new().unwrap();
        write(td.path(), "plans/a.md", "---\nstatus: draft\n---\n");
        write(td.path(), "product/c.md", "---\nprd: '011'\n---\n");

        let wanted: BTreeSet<String> = ["status".to_string()].into_iter().collect();
        let (mut store, _report) = InMemoryStore::load(
            vec![td.path().to_path_buf()],
            WalkOpts::default(),
            Some(&wanted),
        );

        let plans = td.path().join("plans");
        store.refresh(&VaultRoot::new(td.path().to_path_buf()), Some(&plans));

        let has_prd = store
            .records()
            .any(|r| r.field_names().any(|name| name == "prd"));
        assert!(
            !has_prd,
            "narrowing wanted to a query's own fields without also forcing \
             wanted = None on --refresh silently drops an out-of-subtree \
             field the refresh never touched — the exact corruption \
             build_session's guard exists to prevent"
        );
    }

    /// FIX 2 (spec §9): a `manifest.bin` present on disk but rejected by
    /// `load_cache` (incompatible/corrupt) must warn to the report — which
    /// `main` prints to stderr — before rebuilding via a full live scan, so a
    /// schema-bumped vault doesn't silently slow-scan every run. A vault with
    /// NO manifest at all rebuilds silently (no such warning).
    #[test]
    fn from_cache_warns_on_incompatible_manifest_but_not_on_missing_one() {
        // Incompatible: a manifest.bin whose bytes fail the MAGIC/version check.
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");
        fs::create_dir_all(td.path().join(".querymatter")).unwrap();
        fs::write(
            td.path().join(".querymatter/manifest.bin"),
            b"NOPEnotaversion",
        )
        .unwrap();
        assert!(cache::load_cache(td.path()).is_none());

        let (store, report) = InMemoryStore::from_cache(
            &VaultRoot::new(td.path().to_path_buf()),
            WalkOpts::default(),
            Freshness::PerFile,
            None,
            None,
        );
        assert!(
            report.warnings.iter().any(|w| w.contains("incompatible")),
            "an incompatible on-disk manifest must warn, got: {:?}",
            report.warnings
        );
        // The rebuild still produces correct records via the live scan.
        assert_eq!(
            store.records().next().unwrap().field(&["status".into()]),
            Value::Str("draft".into())
        );

        // Missing: no .querymatter at all -> silent rebuild (no such warning).
        let td2 = TempDir::new().unwrap();
        write(td2.path(), "a.md", "---\nstatus: draft\n---\n");
        assert!(cache::load_cache(td2.path()).is_none());

        let (_store, report2) = InMemoryStore::from_cache(
            &VaultRoot::new(td2.path().to_path_buf()),
            WalkOpts::default(),
            Freshness::PerFile,
            None,
            None,
        );
        assert!(
            !report2.warnings.iter().any(|w| w.contains("incompatible")),
            "a missing cache must rebuild silently, got: {:?}",
            report2.warnings
        );
    }

    #[test]
    fn loads_records_and_skips_no_frontmatter() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");
        write(td.path(), "b.md", "no frontmatter here\n");
        let (store, report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);
        assert_eq!(report.loaded, 1);
        assert_eq!(store.records().count(), 1);
    }
    #[test]
    fn schema_is_sorted_union() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\njira: X\n---\n");
        write(td.path(), "b.md", "---\nepic: E\nstatus: synced\n---\n");
        let (store, _) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);
        assert_eq!(store.schema(), vec!["epic", "jira", "status"]);
    }

    /// Load-bearing (Task 3, projection push-down): materializing with
    /// `wanted = Some({"status"})` must keep ONLY `status`'s value on each
    /// `Record` — `prd`/`tags` absent from `field_names()` — while
    /// `store.schema()` still reports the FULL field-name union (`prd`,
    /// `status`, `tags`), proving the schema is tracked independently of
    /// which values got pruned, not derived from the pruned records
    /// themselves. This is what lets W12's unknown-column/did-you-mean
    /// validation keep working under push-down (see
    /// `tests/cli.rs::typo_under_pushdown_still_errors_with_didyoumean`).
    #[test]
    fn pruning_keeps_only_wanted_field_values_but_full_schema() {
        let td = TempDir::new().unwrap();
        write(
            td.path(),
            "a.md",
            "---\nstatus: draft\nprd: '010'\ntags: [a, b]\n---\n",
        );
        write(
            td.path(),
            "b.md",
            "---\nstatus: synced\nprd: '011'\ntags: [c]\n---\n",
        );

        let wanted: BTreeSet<String> = ["status".to_string()].into_iter().collect();
        let (store, _report) = InMemoryStore::load(
            vec![td.path().to_path_buf()],
            WalkOpts::default(),
            Some(&wanted),
        );

        for record in store.records() {
            assert_eq!(
                record.field_names().collect::<Vec<_>>(),
                vec!["status"],
                "a pruned record must carry only the wanted field"
            );
        }
        assert_eq!(
            store.schema(),
            vec!["prd", "status", "tags"],
            "schema() must stay the FULL field-name union regardless of pruning"
        );
    }

    #[test]
    fn reload_dir_overwrites_only_that_slice() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        write(a.path(), "x.md", "---\nstatus: draft\n---\n");
        write(b.path(), "y.md", "---\nstatus: synced\n---\n");
        let (mut store, _) = InMemoryStore::load(
            vec![a.path().to_path_buf(), b.path().to_path_buf()],
            WalkOpts::default(),
            None,
        );
        assert_eq!(store.records().count(), 2);
        // add a file to A, reload only A
        write(a.path(), "z.md", "---\nstatus: draft\n---\n");
        let report = store.reload_dir(a.path());
        assert_eq!(report.loaded, 2); // A now has 2
        assert_eq!(store.records().count(), 3); // A(2) + B(1) unchanged
    }
    #[test]
    fn invalid_yaml_is_skipped_with_warning() {
        let td = TempDir::new().unwrap();
        write(td.path(), "bad.md", "---\n: : broken\n  x\n---\n");
        let (_store, report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.warnings.len(), 1);
    }

    /// Guard for Task 2 (parallel scan): the read+parse of each file happens
    /// across worker threads, but `InMemoryStore::load`'s records and the
    /// load report's warnings must come out in exactly the order a serial
    /// scan would produce — `discover`'s path-sorted order — regardless of
    /// which worker finishes first.
    ///
    /// Note: this test passes even with `map_paths`'s final sort removed,
    /// since `discover()` already hands back a path-sorted list and this
    /// fixture's worker split happens to preserve that order. The real guard
    /// for the sort itself is
    /// `parallel::tests::results_are_sorted_by_path_regardless_of_input_order`.
    #[test]
    fn parallel_scan_matches_serial_records_and_order() {
        let td = TempDir::new().unwrap();
        for i in 0..20 {
            write(
                td.path(),
                &format!("dir{}/file{i:02}.md", i % 4),
                &format!("---\nstatus: s{i:02}\n---\n"),
            );
        }
        write(td.path(), "no_fm_a.md", "no frontmatter here\n");
        write(td.path(), "no_fm_b.md", "also no frontmatter\n");
        write(td.path(), "bad.md", "---\n: : broken\n  x\n---\n");

        // Independently derive the expected order from `discover`'s (already
        // path-sorted) output and the pure `cache::scan_file`, rather than
        // reusing `scan_root` itself, so this doesn't just check the
        // parallel code against a copy of itself.
        let mut expected_paths = Vec::new();
        let mut expected_warnings = Vec::new();
        let dir = DirPath::new(td.path().to_path_buf());
        for path in discover::discover(td.path(), &WalkOpts::default()) {
            let file_path = FilePath::new(path.clone());
            match cache::scan_file(&dir, &file_path, WalkOpts::default().max_file_bytes) {
                ScanResult::Cached(_) => expected_paths.push(path),
                ScanResult::NoFrontmatter => {}
                ScanResult::Warning(msg) => expected_warnings.push(msg),
            }
        }

        let (store, report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);

        let got_paths: Vec<PathBuf> = store
            .records()
            .map(|r| match r.file_attr(FileAttr::Path) {
                Value::Str(rel) => td.path().join(rel),
                other => panic!("file.path must be a string, got {other:?}"),
            })
            .collect();
        assert_eq!(
            got_paths, expected_paths,
            "records must come out in path-sorted (serial) order"
        );
        assert_eq!(
            report.warnings, expected_warnings,
            "warnings must come out in path-sorted (serial) order"
        );
        assert_eq!(report.loaded, 20);
        assert_eq!(report.skipped, 1);
    }

    /// Guard for Task 2: loading the same tree twice must yield identical row
    /// order for a query with no `ORDER BY` — the parallel scan must not
    /// introduce run-to-run nondeterminism.
    #[test]
    fn scan_is_deterministic_across_runs() {
        let td = TempDir::new().unwrap();
        for i in 0..20 {
            write(
                td.path(),
                &format!("dir{}/file{i:02}.md", i % 4),
                &format!("---\nstatus: s{i:02}\n---\n"),
            );
        }

        let parsed = crate::query::parse("SELECT file.path").unwrap();
        let mut runs = Vec::new();
        for _ in 0..5 {
            let (store, _report) =
                InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);
            let result = crate::query::execute(&parsed, store.records(), false).unwrap();
            runs.push(result.rows);
        }

        for run in &runs[1..] {
            assert_eq!(
                run, &runs[0],
                "row order with no ORDER BY must be identical run-to-run"
            );
        }
    }
}
