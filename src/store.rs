//! Ties directory discovery and frontmatter extraction into a queryable,
//! directory-keyed record store.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::cache::{self, ScanResult};
use crate::discover::{self, WalkOpts};
use crate::model::Record;

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
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &std::path::Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
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
