//! Integration tests for the querymatter-samples generator binary.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::SystemTime;

use assert_cmd::Command;

/// Recursive map of rel-path -> (bytes, mtime).
fn tree(root: &Path) -> BTreeMap<String, (Vec<u8>, SystemTime)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let e = entry.unwrap();
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
                continue;
            }
            let rel = e.path().strip_prefix(root).unwrap().display().to_string();
            let mtime = e.metadata().unwrap().modified().unwrap();
            out.insert(rel, (std::fs::read(e.path()).unwrap(), mtime));
        }
    }
    out
}

fn generate(dir: &Path, extra_args: &[&str]) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("querymatter-samples").unwrap();
    cmd.args(extra_args).arg(dir);
    cmd.assert()
}

/// Spec §7 test 1 — the headline determinism guarantee: two generations are
/// identical in paths, bytes, AND mtimes.
#[test]
fn regeneration_is_byte_and_mtime_identical() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    generate(a.path(), &["--scale", "1k"]).success();
    generate(b.path(), &["--scale", "1k"]).success();
    let (ta, tb) = (tree(a.path()), tree(b.path()));
    assert_eq!(ta.len(), tb.len());
    assert_eq!(ta, tb);
}

/// Spec §7 test 2 — exact counts: 1000 total, 35 under starwars/.
#[test]
fn one_k_scale_holds_exactly_1000_files() {
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &["--scale", "1k"]).success();
    let t = tree(dir.path());
    assert_eq!(t.len(), 1000);
    assert_eq!(t.keys().filter(|k| k.starts_with("starwars/")).count(), 35);
    assert!(
        t.keys().all(|k| k.ends_with(".md")),
        "tree must be pure .md data"
    );
}

/// Default scale is 1k.
#[test]
fn default_scale_is_1k() {
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &[]).success();
    assert_eq!(tree(dir.path()).len(), 1000);
}

/// Spec §7 test 4 — --force semantics.
#[test]
fn nonempty_dir_refused_without_force() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("precious.txt"), "keep me").unwrap();
    generate(dir.path(), &[])
        .failure()
        .stderr(predicates::str::contains("--force"));
    // The refusal must leave the dir untouched.
    assert!(dir.path().join("precious.txt").exists());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn force_wipes_and_regenerates() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("stale.txt"), "old").unwrap();
    generate(dir.path(), &["--force"]).success();
    assert!(!dir.path().join("stale.txt").exists());
    assert_eq!(tree(dir.path()).len(), 1000);
}

/// A missing directory is created (with parents).
#[test]
fn missing_dir_is_created() {
    let parent = tempfile::tempdir().unwrap();
    let target = parent.path().join("deep/nested/samples");
    generate(&target, &[]).success();
    assert_eq!(tree(&target).len(), 1000);
}

/// stdout stays empty — the repo convention is stdout carries data only.
#[test]
fn stdout_is_empty_summary_on_stderr() {
    let dir = tempfile::tempdir().unwrap();
    generate(dir.path(), &[])
        .success()
        .stdout(predicates::str::is_empty())
        .stderr(predicates::str::contains("1000"));
}
