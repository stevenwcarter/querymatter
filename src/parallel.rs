//! A small parallel-map primitive for scanning many files' contents
//! independently, used by [`crate::store::scan_root`] and the freshness
//! refresh in [`crate::cache`] to spread the disk-bound read+parse step of a
//! scan across all cores.
//!
//! Deliberately minimal — no thread pool crate. Every unit of work is an
//! independent `path -> T`, and the only shared state is a single
//! [`AtomicUsize`] cursor: each worker `fetch_add`s the next index and
//! processes that path (work-stealing), so a slow path never strands the
//! other cores idle. Workers write only into their own local buffers; the
//! results are joined back together **sorted by path** — the same order a
//! serial `paths.iter().map(f)` would have produced, regardless of which
//! worker thread happens to finish first.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
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
        let mut results: Vec<(PathBuf, T)> = paths
            .into_iter()
            .map(|path| {
                let result = f(&path);
                (path, result)
            })
            .collect();
        // Unconditional even here: a no-op for 0/1 elements, but keeps the
        // "sorted by path" guarantee holding regardless of worker count —
        // callers on a single-core host must see the same order as everyone
        // else, not merely input order.
        results.sort_by(|(a, _), (b, _)| a.cmp(b));
        return results;
    }

    // Shared work-stealing cursor: every worker pulls the next unclaimed
    // index rather than owning a fixed contiguous slice, so a run of
    // expensive paths doesn't strand one worker with a heavy chunk while
    // others sit idle. `Relaxed` suffices — each index is claimed by exactly
    // one thread (the `fetch_add` itself is the only synchronization
    // needed), and `paths`/the results vectors are combined only after every
    // worker thread has been joined.
    let cursor = AtomicUsize::new(0);
    let mut results: Vec<(PathBuf, T)> = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut mapped = Vec::new();
                    loop {
                        let i = cursor.fetch_add(1, Ordering::Relaxed);
                        let Some(path) = paths.get(i) else {
                            break;
                        };
                        mapped.push((path.clone(), f(path)));
                    }
                    mapped
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|handle| match handle.join() {
                Ok(worker_results) => worker_results,
                // A worker only ever calls `f`, which reads from disk and
                // reports failure through its return value rather than
                // panicking — propagate rather than silently dropping a
                // worker's results on the (unexpected) panic.
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

    /// Characterization test: a size-skewed workload (`f`'s cost varies wildly
    /// by path, simulated with a busy-spin proportional to a value baked into
    /// the path name) must still come back sorted by path and byte-identical
    /// to what a plain serial `paths.iter().map(f)` would produce — regardless
    /// of how work happens to be split or interleaved across worker threads.
    /// This must hold both today (static contiguous chunks) and after the
    /// planned refactor to a shared work-stealing cursor, so a few of the
    /// "expensive" paths are deliberately clustered in one contiguous region
    /// of the input to make a naive static split imbalanced.
    #[test]
    fn size_skewed_workload_matches_serial_map_sorted_by_path() {
        // Work units, deliberately skewed: a run of expensive paths up front
        // (would land in one worker's chunk under static contiguous
        // splitting) followed by many cheap ones.
        let work_units: Vec<u64> = std::iter::repeat_n(5_000_u64, 6)
            .chain(std::iter::repeat_n(50_u64, 150))
            .collect();
        let paths: Vec<PathBuf> = (0..work_units.len())
            .map(|i| PathBuf::from(format!("{i:04}")))
            .collect();

        // Simulates variable-cost work with a busy-spin rather than a sleep,
        // so the test runs fast while still making some units far more
        // expensive than others.
        let cost_by_path = |path: &Path| -> u64 {
            let idx: usize = path.to_string_lossy().parse().expect("numeric test path");
            let units = work_units[idx];
            let mut acc = 0_u64;
            for i in 0..units {
                acc = acc.wrapping_add(i);
            }
            acc
        };

        let serial: Vec<(PathBuf, u64)> = paths
            .iter()
            .map(|path| (path.clone(), cost_by_path(path)))
            .collect();

        let parallel = map_paths(paths, cost_by_path);

        assert_eq!(parallel, serial);
        assert!(parallel.windows(2).all(|w| w[0].0 <= w[1].0));
    }
}
