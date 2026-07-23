use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;

fn tree() -> TempDir {
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("plans/a.md", "---\nstatus: draft\nprd: '010'\n---\n"),
        ("plans/b.md", "---\nstatus: synced\nprd: '010'\n---\n"),
        ("product/c.md", "---\nstatus: synced\nprd: '011'\n---\n"),
    ] {
        let f = td.path().join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, s).unwrap();
    }
    td
}

#[test]
fn oneshot_group_count_table() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("-e")
        .arg("SELECT status, count(*) AS Count GROUP BY status ORDER BY Count DESC")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("Count"))
        .stdout(predicates::str::contains("synced"));
}

#[test]
fn oneshot_json_is_clean_stdout() {
    let td = tree();
    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .args(["-e", "SELECT status WHERE prd = '010'", "--format", "json"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap(); // stdout must be pure JSON
    assert_eq!(v.as_array().unwrap().len(), 2);
}

#[test]
fn batch_mode_from_stdin() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg(td.path())
        .write_stdin("SELECT count(*) AS n;\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("n"));
}

#[test]
fn query_error_exits_nonzero() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .args(["-e", "SELCT bad"])
        .arg(td.path())
        .assert()
        .failure();
}

/// The committed sample tree under `tests/fixtures/` (mirrors `samples/`,
/// which is gitignored) so these tests don't depend on gitignored data.
const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

#[test]
fn headline_status_counts() {
    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .args([
            "-e",
            "SELECT status, count(*) AS Count WHERE prd = '010' GROUP BY status ORDER BY Count DESC",
            "--format",
            "csv",
        ])
        .arg(format!("{FIX}/plans"))
        .arg(format!("{FIX}/product"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    let mut lines = s.lines();
    assert_eq!(lines.next().unwrap(), "status,Count");
    // prd '010': plans/DCP-459 draft x1, plans/DCP-461 synced x1,
    // product/stories/DCP-459 synced x1 -> synced=2, draft=1, DESC by count.
    assert_eq!(lines.next().unwrap(), "synced,2");
    assert_eq!(lines.next().unwrap(), "draft,1");
    assert!(lines.next().is_none());
}

#[test]
fn group_by_file_folder() {
    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .args([
            "-e",
            "SELECT file.folder, count(*) AS n GROUP BY file.folder",
            "--format",
            "json",
        ])
        .arg(FIX)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    // plans, product/stories, templates
    assert!(v.as_array().unwrap().len() >= 2);
}

#[test]
fn exclude_templates() {
    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .args([
            "-e",
            "SELECT count(*) AS n",
            "--exclude",
            "**/templates/**",
            "--format",
            "csv",
        ])
        .arg(FIX)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    // 3 real docs (2 plans + 1 story); the template is excluded.
    assert_eq!(s.trim(), "n\n3");
}

#[test]
fn missing_directory_exits_nonzero_and_names_path() {
    let bad = "/no/such/directory/definitely-not-real-qm";
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg(bad)
        .assert()
        .failure()
        .stderr(predicates::str::contains(bad));
}

#[test]
fn invalid_exclude_glob_exits_nonzero() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .args(["--exclude", "["])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("exclude"));
}

#[test]
fn malformed_frontmatter_warning_stays_off_stdout() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("good.md"), "---\nstatus: draft\n---\n").unwrap();
    fs::write(td.path().join("bad.md"), "---\n: : broken\n  bad\n---\n").unwrap();

    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .args(["-e", "SELECT status", "--format", "json"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    // stdout must be pure, parseable JSON even though one file warned on stderr.
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 1);
}

#[test]
fn batch_good_then_bad_exits_nonzero() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg(td.path())
        .write_stdin("SELECT count(*) AS n;\nSELCT bad;\n")
        .assert()
        .failure();
}

#[test]
fn querymatterignore_in_cwd_excludes_matches() {
    let td = TempDir::new().unwrap();
    let w = |rel: &str, body: &str| {
        let p = td.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    };
    w("plans/a.md", "---\nstatus: draft\n---\n");
    w("templates/t.md", "---\nstatus: draft\n---\n");
    w(".querymatterignore", "templates/\n");

    // Run with cwd = td so the cwd .querymatterignore is auto-discovered; scan ".".
    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args(["-e", "SELECT count(*) AS n", "--format", "csv", "."])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    // Only plans/a.md counts; templates/t.md is ignored.
    assert_eq!(s.lines().last().unwrap().trim(), "1", "got: {s:?}");
}

#[test]
fn init_creates_manifest() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();
    assert!(
        td.path().join(".querymatter/manifest.bin").is_file(),
        "init must create <dir>/.querymatter/manifest.bin"
    );
}

#[test]
fn query_from_inside_vault_returns_rows() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // No positional dirs: the run auto-discovers the ancestor vault (cwd is
    // the vault) and queries the whole cache.
    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args(["-e", "SELECT count(*) AS n", "--format", "csv"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(s.lines().last().unwrap().trim(), "3", "got: {s:?}");
}

#[test]
fn no_cache_live_scans_even_inside_a_vault() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // `--no-cache` bypasses vault discovery: it live-scans the positional dir.
    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT count(*) AS n",
            "--format",
            "csv",
            "--no-cache",
            ".",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(s.lines().last().unwrap().trim(), "3", "got: {s:?}");
}

#[test]
fn force_cache_without_a_vault_exits_nonzero() {
    let td = tree();
    // No `init`, so no vault exists anywhere above `td`.
    Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args(["-e", "SELECT count(*) AS n", "--force-cache"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("force-cache"));
}

#[test]
fn missing_ignore_file_flag_exits_nonzero() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("a.md"), "---\nstatus: draft\n---\n").unwrap();
    Command::cargo_bin("querymatter")
        .unwrap()
        .args([
            "--ignore-file",
            "definitely-nonexistent.ignore",
            "-e",
            "SELECT count(*) AS n",
        ])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("definitely-nonexistent.ignore"));
}
