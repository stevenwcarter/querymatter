use assert_cmd::Command;
use predicates::prelude::*;
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
fn init_in_git_repo_non_tty_succeeds_and_summarizes() {
    // The git-ignore offer runs after a successful cache build; in a non-TTY
    // run it prints a hint rather than prompting. Even so, that step must never
    // fail the command: init succeeds (exit 0), writes the manifest, and still
    // prints the "cached N file(s)" summary to stderr (FIX 3 — the offer is
    // best-effort, downgraded to a warning rather than propagated).
    let td = tree();
    fs::create_dir_all(td.path().join(".git")).unwrap();

    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        // A piped (non-terminal) stdin forces the non-TTY hint branch
        // deterministically, so the test never blocks on an interactive prompt.
        .write_stdin("")
        .assert()
        .success()
        .stderr(predicates::str::contains("cached"));
    assert!(td.path().join(".querymatter/manifest.bin").is_file());
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
fn positional_dir_restricts_vault_query_to_that_subtree() {
    // Spec §5: with a vault in use, a positional [DIRS] restricts the query to
    // records under that subtree. The fixture has plans/a.md {status: draft}
    // and product/b.md {status: synced}; querying with positional `plans` from
    // inside the vault must return only the plans record, not product's.
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("plans/a.md", "---\nstatus: draft\n---\n"),
        ("product/b.md", "---\nstatus: synced\n---\n"),
    ] {
        let f = td.path().join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, s).unwrap();
    }

    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args(["-e", "SELECT status", "--format", "csv", "plans"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    let statuses: Vec<&str> = s.lines().skip(1).map(str::trim).collect();
    assert_eq!(
        statuses,
        vec!["draft"],
        "positional `plans` must restrict the vault query to plans/, excluding product/; got: {s:?}"
    );
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
fn refresh_relative_path_reparses_and_defeats_stale_cache() {
    use std::fs::File;
    let td = TempDir::new().unwrap();
    let plans = td.path().join("plans");
    fs::create_dir_all(&plans).unwrap();
    let a = plans.join("a.md");
    fs::write(&a, "---\nstatus: draft\n---\n").unwrap();
    let original_mtime = fs::metadata(&a).unwrap().modified().unwrap();

    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // Edit the content but keep the byte length equal ("draft" -> "fresh") and
    // restore the original mtime, so the default per-file freshness check
    // (mtime + size) would REUSE the stale cached value. Only a real
    // `--refresh` (a forced re-parse) can surface the edit — so a `--refresh
    // plans` that silently no-ops on a relative path leaves the output stale
    // at `draft`. Asserting `fresh` proves the relative path actually fired.
    fs::write(&a, "---\nstatus: fresh\n---\n").unwrap();
    File::open(&a)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();

    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT status",
            "--format",
            "csv",
            "--refresh",
            "plans",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s.lines().last().unwrap().trim(),
        "fresh",
        "--refresh with a relative path must force a re-parse; got: {s:?}"
    );
}

#[test]
fn refresh_all_forces_full_rescan_despite_unchanged_mtime_and_size() {
    // FIX 1: `--refresh-all` must force a full re-scan of the whole vault,
    // ignoring every per-file freshness shortcut (README: "force a full
    // re-scan, ignoring every freshness shortcut"). Editing content to an
    // equal-byte-length value ("draft" -> "ready") and restoring the original
    // mtime makes (mtime, size) indistinguishable from the cached entry, so
    // the default incremental refresh would reuse the stale "draft"; only a
    // forced re-scan surfaces "ready".
    use std::fs::File;
    let td = TempDir::new().unwrap();
    let a = td.path().join("a.md");
    fs::write(&a, "---\nstatus: draft\n---\n").unwrap();
    let original_mtime = fs::metadata(&a).unwrap().modified().unwrap();

    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    fs::write(&a, "---\nstatus: ready\n---\n").unwrap();
    File::open(&a)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();

    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args(["-e", "SELECT status", "--format", "csv", "--refresh-all"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s.lines().last().unwrap().trim(),
        "ready",
        "--refresh-all must force a full re-scan even when (mtime, size) are unchanged; got: {s:?}"
    );
}

#[test]
fn refresh_nonexistent_path_exits_nonzero() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT count(*) AS n",
            "--refresh",
            "definitely-not-here",
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("definitely-not-here"));
}

#[test]
fn refresh_path_outside_vault_exits_nonzero() {
    let vault = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(vault.path())
        .assert()
        .success();

    // A real, existing directory that is NOT under the vault.
    let outside = TempDir::new().unwrap();
    Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(vault.path())
        .args(["-e", "SELECT count(*) AS n", "--refresh"])
        .arg(outside.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("outside the vault"));
}

#[test]
fn cache_query_matches_live_scan_byte_for_byte() {
    // Spec §10 "cache-equals-live" (the load-bearing invariant): a query
    // answered from a `.querymatter` cache must produce byte-identical stdout
    // to the same query answered by a live (`--no-cache`) scan of the same
    // tree.
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    const QUERY: &str = "SELECT status, count(*) AS n GROUP BY status ORDER BY n";

    let cached = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args(["-e", QUERY, "--format", "csv"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let live = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args(["-e", QUERY, "--format", "csv", "--no-cache", "."])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(
        cached,
        live,
        "a vault-backed query must return byte-identical stdout to a --no-cache live scan; \
         cached={:?} live={:?}",
        String::from_utf8_lossy(&cached),
        String::from_utf8_lossy(&live),
    );
}

#[test]
fn force_cache_returns_stale_value_after_on_disk_edit() {
    // Spec §4: `--force-cache` does zero filesystem access, so it must keep
    // returning the value cached at `init` time even after the file changes
    // on disk (unlike default per-file freshness, tested below).
    let td = TempDir::new().unwrap();
    let a = td.path().join("a.md");
    fs::write(&a, "---\nstatus: draft\n---\n").unwrap();

    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    fs::write(&a, "---\nstatus: done\n---\n").unwrap();

    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args(["-e", "SELECT status", "--format", "csv", "--force-cache"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s.lines().last().unwrap().trim(),
        "draft",
        "--force-cache must return the stale cached value, never checking the filesystem; got: {s:?}"
    );
}

#[test]
fn default_freshness_reflects_on_disk_edit() {
    // Spec §4 default mode: accurate per-file (mtime+size) freshness
    // re-parses a changed file with no explicit `--refresh` needed. This
    // complements `refresh_relative_path_reparses_and_defeats_stale_cache`,
    // which pins the forced-refresh path instead.
    let td = TempDir::new().unwrap();
    let a = td.path().join("a.md");
    fs::write(&a, "---\nstatus: draft\n---\n").unwrap();

    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    fs::write(&a, "---\nstatus: done\n---\n").unwrap();

    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .current_dir(td.path())
        .args(["-e", "SELECT status", "--format", "csv"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s.lines().last().unwrap().trim(),
        "done",
        "default freshness must re-parse a changed file with no --refresh flag; got: {s:?}"
    );
}

#[test]
fn init_non_tty_does_not_modify_gitignore() {
    // Spec §7: a non-interactive (piped-stdin) `init` inside a git repo must
    // print a stderr hint but never create or modify `.gitignore`.
    let td = tree();
    fs::create_dir_all(td.path().join(".git")).unwrap();

    Command::cargo_bin("querymatter")
        .unwrap()
        .arg("init")
        .arg(td.path())
        // A piped (non-terminal) stdin forces the non-TTY hint branch
        // deterministically, so the test never blocks on an interactive prompt.
        .write_stdin("")
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "hint: add .querymatter/ to .gitignore",
        ));

    assert!(
        !td.path().join(".gitignore").exists(),
        ".gitignore must not be created by a non-TTY init"
    );
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

/// `--table-style` is opt-in: with nothing set, output stays ASCII. The
/// env-var removal matters — a developer with QUERYMATTER_TABLE_STYLE
/// exported would otherwise see this pass or fail by accident.
#[test]
fn table_style_defaults_to_ascii() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .env_remove("QUERYMATTER_TABLE_STYLE")
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("+--"))
        .stdout(predicates::str::contains("╭").not());
}

#[test]
fn table_style_flag_draws_unicode_borders() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .env_remove("QUERYMATTER_TABLE_STYLE")
        .args([
            "-e",
            "SELECT status WHERE prd = '010'",
            "--table-style",
            "unicode",
        ])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

#[test]
fn table_style_env_var_is_honored() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .env("QUERYMATTER_TABLE_STYLE", "unicode")
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

#[test]
fn table_style_flag_overrides_env_var() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .env("QUERYMATTER_TABLE_STYLE", "unicode")
        .args([
            "-e",
            "SELECT status WHERE prd = '010'",
            "--table-style",
            "ascii",
        ])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("+--"))
        .stdout(predicates::str::contains("╭").not());
}

/// A typo'd style must fail loudly from either source, never degrade to the
/// default.
#[test]
fn bad_table_style_flag_exits_non_zero() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .env_remove("QUERYMATTER_TABLE_STYLE")
        .args(["-e", "SELECT status", "--table-style", "fancy"])
        .arg(td.path())
        .assert()
        .failure();
}

#[test]
fn bad_table_style_env_var_exits_non_zero() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .env("QUERYMATTER_TABLE_STYLE", "fancy")
        .args(["-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .failure();
}

#[test]
fn oneshot_vertical_g_prints_row_blocks() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        // \G forces Output::Vertical, which render() dispatches independent
        // of TableStyle — so this assertion can't actually flip on an
        // ambient QUERYMATTER_TABLE_STYLE today, but removing it keeps the
        // test from silently growing a dependency on that if vertical
        // rendering ever starts consulting the style.
        .env_remove("QUERYMATTER_TABLE_STYLE")
        .args(["-e", "SELECT status, prd WHERE prd = '011'\\G"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("1. row"))
        .stdout(predicates::str::contains("status: synced"))
        .stdout(predicates::str::contains("+--").not());
}

/// One piped script, two terminators: each statement renders its own way.
/// The env-var removal matters here exactly as in `table_style_defaults_to_ascii`:
/// without it, a developer with QUERYMATTER_TABLE_STYLE=unicode exported sees
/// the `;` statement grow unicode borders and the `+--` assertion fails.
#[test]
fn batch_mode_mixes_terminators() {
    let td = tree();
    let out = Command::cargo_bin("querymatter")
        .unwrap()
        .env_remove("QUERYMATTER_TABLE_STYLE")
        .arg(td.path())
        .write_stdin("SELECT count(*) AS n;\nSELECT status WHERE prd = '011'\\G\n")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains("+--"),
        "the `;` statement stays a table:\n{text}"
    );
    assert!(
        text.contains("1. row"),
        "the `\\G` statement goes vertical:\n{text}"
    );
}

/// `\G` means "record-wise" whatever the standing format is.
#[test]
fn vertical_g_overrides_the_session_format() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .args([
            "-e",
            "SELECT status WHERE prd = '011'\\G",
            "--format",
            "json",
        ])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("1. row"))
        .stdout(predicates::str::contains("[").not());
}

#[test]
fn lowercase_g_terminates_like_a_semicolon() {
    let td = tree();
    Command::cargo_bin("querymatter")
        .unwrap()
        .env_remove("QUERYMATTER_TABLE_STYLE")
        .args(["-e", "SELECT status WHERE prd = '011'\\g"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("+--"))
        .stdout(predicates::str::contains("1. row").not());
}
