//! Ties directory discovery and frontmatter extraction into a queryable,
//! directory-keyed record store.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::cache::{self, CachedDir, CachedFile, Freshness, ScanResult};
use crate::discover::{self, WalkOpts};
use crate::model::{FileAttr, Record};

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
/// [`InMemoryStore::refresh`] finds no on-disk `.querymatter` manifest to
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
    pub fn load(roots: Vec<PathBuf>, opts: WalkOpts) -> (Self, LoadReport) {
        let mut store = InMemoryStore {
            slices: Vec::new(),
            opts,
        };
        let mut report = LoadReport::default();
        for root in roots {
            let (records, slice_report) = scan_root(&root, &store.opts);
            report.merge(slice_report);
            store.slices.push(DirSlice {
                root,
                records,
                scanned_at: SystemTime::now(),
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
    pub fn from_cache(vault: &Path, opts: WalkOpts, mode: Freshness) -> (Self, LoadReport) {
        let (cached, ttl_secs) = match cache::load_cache(vault) {
            Some((body, dirs)) => (dirs, body.ttl_secs),
            None => (Vec::new(), DEFAULT_TTL_SECS),
        };

        let (fresh, mut report, changed) =
            cache::refresh_against_cache(vault, &cached, &opts, mode, ttl_secs);

        if changed
            && mode != Freshness::ForceCache
            && let Err(err) = cache::save_cache(vault, &fresh, ttl_secs)
        {
            report.warnings.push(format!("saving cache: {err}"));
        }

        let slices = slices_from_cached(vault, &fresh);
        (InMemoryStore { slices, opts }, report)
    }

    /// Forces a fresh read of `subtree` (or the whole vault, when `None`)
    /// against the live filesystem, updates the in-memory slices to match,
    /// and persists the result — used by the REPL `.refresh` and the
    /// `--refresh` flags.
    ///
    /// The starting point is the on-disk cache ([`cache::load_cache`]) when
    /// one exists; otherwise it's rebuilt from the store's own current
    /// slices ([`Self::cached_dirs_from_slices`]) so that directories outside
    /// `subtree` aren't silently dropped. A `subtree` forces a full re-parse
    /// of just that directory tree ([`cache::refresh_subtree`], ignoring any
    /// cached shortcut); `None` instead does a whole-vault incremental
    /// refresh ([`Freshness::PerFile`]), reusing cached fields for files
    /// whose `(mtime, size)` haven't changed.
    pub fn refresh(&mut self, vault: &Path, subtree: Option<&Path>) -> LoadReport {
        let (mut cached, ttl_secs) = match cache::load_cache(vault) {
            Some((body, dirs)) => (dirs, body.ttl_secs),
            None => (self.cached_dirs_from_slices(), DEFAULT_TTL_SECS),
        };

        let mut report = match subtree {
            Some(subtree) => cache::refresh_subtree(vault, &mut cached, subtree, &self.opts),
            None => {
                let (fresh, report, _changed) = cache::refresh_against_cache(
                    vault,
                    &cached,
                    &self.opts,
                    Freshness::PerFile,
                    ttl_secs,
                );
                cached = fresh;
                report
            }
        };

        if let Err(err) = cache::save_cache(vault, &cached, ttl_secs) {
            report.warnings.push(format!("saving cache: {err}"));
        }

        self.slices = slices_from_cached(vault, &cached);
        report
    }

    /// Reconstructs a fine-grained `Vec<CachedDir>` — one entry per
    /// immediate parent directory, the same granularity
    /// [`cache::refresh_subtree`] computes internally — from this store's
    /// current in-memory slices. Used by [`Self::refresh`] as a fallback
    /// when no on-disk cache exists yet, so a directory outside the
    /// requested subtree is carried forward untouched rather than dropped.
    ///
    /// The `mtime`/`size`/`scanned_at`/`dir_mtime` fields are placeholders
    /// (`UNIX_EPOCH`/`0`): an entry built here is only ever consulted by
    /// [`cache::refresh_subtree`] for a directory *outside* the subtree being
    /// refreshed, where it's carried through unread rather than compared
    /// against — so its stat accuracy doesn't matter, only that it
    /// round-trips through [`cache::save_cache`] cleanly.
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
                    .map(|name| (name.to_string(), record.field(name)))
                    .collect();
                by_dir.entry(dir).or_default().push(CachedFile {
                    rel_path,
                    mtime: SystemTime::UNIX_EPOCH,
                    size: 0,
                    fields,
                });
            }
        }

        by_dir
            .into_iter()
            .map(|(dir, files)| CachedDir {
                dir,
                scanned_at: SystemTime::UNIX_EPOCH,
                dir_mtime: SystemTime::UNIX_EPOCH,
                files,
            })
            .collect()
    }
}

/// Builds one [`DirSlice`] per cached directory in `dirs` via
/// [`cache::records_from`], stamping each with the current time. Shared by
/// [`InMemoryStore::from_cache`] and [`InMemoryStore::refresh`], both of
/// which rebuild the store's slices from a freshly refreshed `Vec<CachedDir>`.
fn slices_from_cached(vault: &Path, dirs: &[CachedDir]) -> Vec<DirSlice> {
    let now = SystemTime::now();
    cache::records_from(vault, dirs)
        .into_iter()
        .map(|(root, records)| DirSlice {
            root,
            records,
            scanned_at: now,
        })
        .collect()
}

impl RecordStore for InMemoryStore {
    fn records(&self) -> Box<dyn Iterator<Item = &Record> + '_> {
        Box::new(self.slices.iter().flat_map(|slice| slice.records.iter()))
    }

    fn schema(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        for record in self.records() {
            names.extend(record.field_names().map(String::from));
        }
        names.into_iter().collect()
    }

    fn reload_dir(&mut self, root: &Path) -> LoadReport {
        let (records, report) = scan_root(root, &self.opts);
        if let Some(slice) = self.slices.iter_mut().find(|slice| slice.root == root) {
            slice.records = records;
            slice.scanned_at = SystemTime::now();
        } else {
            self.slices.push(DirSlice {
                root: root.to_path_buf(),
                records,
                scanned_at: SystemTime::now(),
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
}

/// Scans `root` for frontmatter records using `opts`, returning the loaded
/// records alongside a report of how many files were skipped and why.
///
/// Delegates the per-file work to [`cache::scan_file`] — the single
/// "file → record" definition shared with the cache's freshness checks —
/// discarding the on-disk stat it carries, since a live scan doesn't need
/// it. A file that fails to read from disk is treated like invalid
/// frontmatter: it's counted as skipped and warned about, not a hard error.
fn scan_root(root: &Path, opts: &WalkOpts) -> (Vec<Record>, LoadReport) {
    let mut records = Vec::new();
    let mut report = LoadReport::default();

    for path in discover::discover(root, opts) {
        match cache::scan_file(root, &path) {
            ScanResult::Cached(file) => {
                records.push(Record::new(root, &path, file.fields));
                report.loaded += 1;
            }
            ScanResult::NoFrontmatter => {}
            ScanResult::Warning(msg) => {
                report.skipped += 1;
                report.warnings.push(msg);
            }
        }
    }

    (records, report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &std::path::Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn from_cache_matches_live_scan() {
        let td = TempDir::new().unwrap();
        write(td.path(), "plans/a.md", "---\nstatus: draft\n---\n");
        write(td.path(), "product/b.md", "---\nstatus: shipped\n---\n");
        cache::build_vault(td.path(), &WalkOpts::default(), 300).unwrap();

        let (cached_store, report) =
            InMemoryStore::from_cache(td.path(), WalkOpts::default(), Freshness::PerFile);
        assert_eq!(report.skipped, 0);

        let (live_store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());

        let mut cached: Vec<&Record> = cached_store.records().collect();
        let mut live: Vec<&Record> = live_store.records().collect();
        cached.sort_by_key(|r| r.file_attr(FileAttr::Path).display());
        live.sort_by_key(|r| r.file_attr(FileAttr::Path).display());

        assert_eq!(
            cached, live,
            "from_cache(PerFile) must yield the same records as a live scan"
        );
    }

    #[test]
    fn refresh_picks_up_edits_and_persists() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");
        cache::build_vault(td.path(), &WalkOpts::default(), 300).unwrap();

        let (mut store, _report) =
            InMemoryStore::from_cache(td.path(), WalkOpts::default(), Freshness::PerFile);
        assert_eq!(
            store.records().next().unwrap().field("status"),
            Value::Str("draft".into())
        );

        // Different byte length than "draft" (not just different content),
        // so the refresh can't be satisfied by a size-unchanged coincidence
        // if mtime resolution doesn't tick between the two writes.
        write(td.path(), "a.md", "---\nstatus: in-progress\n---\n");

        let report = store.refresh(td.path(), None);
        assert_eq!(report.skipped, 0);

        assert_eq!(
            store.records().next().unwrap().field("status"),
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

    #[test]
    fn force_cache_mode_skips_persist_and_uses_stale_value() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");
        cache::build_vault(td.path(), &WalkOpts::default(), 300).unwrap();

        write(td.path(), "a.md", "---\nstatus: final\n---\n");

        let (store, report) =
            InMemoryStore::from_cache(td.path(), WalkOpts::default(), Freshness::ForceCache);
        assert_eq!(report.skipped, 0);
        assert_eq!(
            store.records().next().unwrap().field("status"),
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

        let (cached_store, report) =
            InMemoryStore::from_cache(td.path(), WalkOpts::default(), Freshness::PerFile);
        assert_eq!(report.skipped, 0);

        let (live_store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());

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
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        assert!(cache::load_cache(td.path()).is_none());

        write(td.path(), "a.md", "---\nstatus: in-progress\n---\n");

        let report = store.refresh(td.path(), None);
        assert_eq!(report.skipped, 0);

        assert_eq!(
            store.records().next().unwrap().field("status"),
            Value::Str("in-progress".into()),
            "refresh must pick up the edit even when cached_dirs_from_slices \
             (not an on-disk cache) supplies the starting point"
        );
    }

    #[test]
    fn loads_records_and_skips_no_frontmatter() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");
        write(td.path(), "b.md", "no frontmatter here\n");
        let (store, report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        assert_eq!(report.loaded, 1);
        assert_eq!(store.records().count(), 1);
    }
    #[test]
    fn schema_is_sorted_union() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\njira: X\n---\n");
        write(td.path(), "b.md", "---\nepic: E\nstatus: synced\n---\n");
        let (store, _) = InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        assert_eq!(store.schema(), vec!["epic", "jira", "status"]);
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
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        assert_eq!(report.skipped, 1);
        assert_eq!(report.warnings.len(), 1);
    }
}
