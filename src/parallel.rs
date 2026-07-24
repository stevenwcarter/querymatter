//! A small parallel-map primitive for scanning many files' contents
//! independently, used by [`crate::store::scan_root`] and the freshness
//! refresh in [`crate::cache`] to spread the disk-bound read+parse step of a
//! scan across all cores.
//!
//! Deliberately minimal — no thread pool crate, no shared mutable state
//! beyond the immutable `paths` each worker reads from. Every unit of work is
//! an independent `path -> T`, so the only coordination needed is splitting
//! `paths` into chunks, running each chunk on its own thread, and joining the
//! results back together **sorted by path** — the same order a serial
//! `paths.iter().map(f)` would have produced, regardless of which worker
//! thread happens to finish first.

use std::path::{Path, PathBuf};
use std::thread;

/// The number of worker threads [`map_paths`] spreads work across: the
/// machine's available parallelism, or `1` when it can't be determined (some
/// platforms don't expose thread-affinity info) — a scan must never fail just
/// because the worker count couldn't be queried.
fn worker_count() -> usize {
    thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

/// Runs `f` over every path in `paths`, spreading the work across
/// [`worker_count`] threads, and returns the `(path, result)` pairs **sorted
/// by path** — the deterministic order a serial `paths.iter().map(f)` would
/// produce, independent of which worker happens to finish first.
///
/// Falls back to running serially, with no threads spawned at all, whenever
/// there's nothing to gain from parallelizing: zero or one path, or a single
/// available worker. This is what makes a one- or two-file vault behave
/// identically to a large one rather than needing a minimum path count.
///
/// `f` must be safe to call concurrently from multiple threads (it takes
/// `&Path`, not `&mut`, and is required to be `Sync`): every caller here
/// passes something that only reads from disk, like
/// [`crate::cache::scan_file`].
pub fn map_paths<T, F>(paths: Vec<PathBuf>, f: F) -> Vec<(PathBuf, T)>
where
    T: Send,
    F: Fn(&Path) -> T + Sync,
{
    let workers = worker_count();
    if paths.len() <= 1 || workers <= 1 {
        return paths
            .into_iter()
            .map(|path| {
                let result = f(&path);
                (path, result)
            })
            .collect();
    }

    // `workers >= 2` and `paths.len() >= 2` here, so this is always >= 1 —
    // `chunks` below never sees a zero chunk size.
    let chunk_size = paths.len().div_ceil(workers);
    let mut results: Vec<(PathBuf, T)> = thread::scope(|scope| {
        let handles: Vec<_> = paths
            .chunks(chunk_size)
            .map(|chunk| {
                scope.spawn(|| {
                    chunk
                        .iter()
                        .map(|path| (path.clone(), f(path)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| match handle.join() {
                Ok(chunk_results) => chunk_results,
                // A worker only ever calls `f`, which reads from disk and
                // reports failure through its return value rather than
                // panicking — propagate rather than silently dropping a
                // chunk's results on the (unexpected) panic.
                Err(payload) => std::panic::resume_unwind(payload),
            })
            .collect()
    });

    results.sort_by(|(a, _), (b, _)| a.cmp(b));
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input_returns_empty() {
        assert!(map_paths(Vec::<PathBuf>::new(), |_| 0).is_empty());
    }

    #[test]
    fn single_path_takes_the_serial_fallback() {
        let results = map_paths(vec![PathBuf::from("only")], |_| 42);
        assert_eq!(results, vec![(PathBuf::from("only"), 42)]);
    }

    #[test]
    fn results_are_sorted_by_path_regardless_of_input_order() {
        let paths: Vec<PathBuf> = ["z", "a", "m", "b", "y"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let results = map_paths(paths, |path| path.to_string_lossy().into_owned());
        assert_eq!(
            results,
            vec![
                (PathBuf::from("a"), "a".to_string()),
                (PathBuf::from("b"), "b".to_string()),
                (PathBuf::from("m"), "m".to_string()),
                (PathBuf::from("y"), "y".to_string()),
                (PathBuf::from("z"), "z".to_string()),
            ]
        );
    }

    #[test]
    fn every_path_is_mapped_exactly_once_across_many_chunks() {
        let paths: Vec<PathBuf> = (0..200).map(|i| PathBuf::from(format!("{i:04}"))).collect();
        let results = map_paths(paths.clone(), |_| 1_u32);
        let mut got: Vec<PathBuf> = results.into_iter().map(|(path, _)| path).collect();
        let mut want = paths;
        want.sort();
        assert_eq!(got.len(), want.len());
        got.sort();
        assert_eq!(got, want);
    }
}
