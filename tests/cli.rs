use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::path::Path;
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

/// Points HOME and XDG_CONFIG_HOME at `dir` so a command never reads or
/// writes the developer's real config. HOME covers macOS, where `directories`
/// uses ~/Library/Application Support and ignores XDG_CONFIG_HOME.
/// `QUERYMATTER_TABLE_STYLE` is also stripped from the inherited environment,
/// since an exported value would otherwise silently change the resolved
/// `table_style` out from under a test that never mentions it.
fn with_config_home<'a>(cmd: &'a mut Command, dir: &Path) -> &'a mut Command {
    cmd.env("HOME", dir)
        .env("XDG_CONFIG_HOME", dir)
        .env_remove("QUERYMATTER_TABLE_STYLE")
}

/// Returns an already-isolated `Command` for the `querymatter` binary: `HOME`
/// and `XDG_CONFIG_HOME` point at `home`, never the developer's real
/// `~/.config/querymatter/` (see this project's hard rule against writing
/// there), and `QUERYMATTER_TABLE_STYLE` is stripped from the inherited
/// environment.
///
/// Every `Command` in this file is built through this (or, when a test needs
/// to seed a specific config, `write_config` plus this same helper) — even
/// tests that never touch config content — so isolation is structural rather
/// than a per-test opt-in. IMPORTANT 2: before this helper existed, only the
/// newest ~14 tests called `with_config_home`; the other ~41 spawned the
/// binary against the developer's REAL environment, so a `~/.config/querymatter/
/// config.toml` with `table_style = "unicode"`/`hidden = true` (both settings
/// this README teaches users to set) or an exported `QUERYMATTER_TABLE_STYLE`
/// broke them.
fn qm(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, home);
    cmd
}

/// Writes a config file into the fake config home `dir`.
fn write_config(dir: &Path, body: &str) {
    let path = dir.join("querymatter");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("config.toml"), body).unwrap();
}

/// Writes a vault-level `.querymatter.toml` directly into `dir` (the vault
/// root) — distinct from [`write_config`]'s per-user
/// `<home>/querymatter/config.toml` (W54).
fn write_vault_config(dir: &Path, body: &str) {
    fs::write(dir.join(".querymatter.toml"), body).unwrap();
}

/// Finds the `config list`/`.settings` row for `key` (the line whose first
/// word is its name), so a test can assert a value and source are on the
/// SAME row rather than merely present somewhere in the output.
fn row_for<'a>(text: &'a str, key: &str) -> &'a str {
    text.lines()
        .find(|line| line.split_whitespace().next() == Some(key))
        .unwrap_or_else(|| panic!("no {key} row in:\n{text}"))
}

#[test]
fn oneshot_group_count_table() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
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
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
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

/// Regression: `Format` gaining `clap::ValueEnum` switched `--format`'s
/// inferred parser away from `FromStr` (which accepts `markdown` as an alias
/// for `md`) to the `ValueEnum` parser, which by default only knows
/// canonical spellings. This runs the real binary end to end, the layer the
/// regression actually broke.
#[test]
fn format_markdown_alias_is_accepted_by_the_real_binary() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT status", "--format", "markdown"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("| status |"));
}

#[test]
fn batch_mode_from_stdin() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg(td.path())
        .write_stdin("SELECT count(*) AS n;\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("n"));
}

/// Batch mode (piped stdin, no `-e`) is the same code path as `-e`/one-shot
/// and must never print the REPL-only `-- N rows` count line — regression
/// guard for the row-count feature, which is wired only into `repl::run`.
#[test]
fn batch_mode_stdin_emits_no_row_count_line() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg(td.path())
        .write_stdin("SELECT count(*) AS n;\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("n"))
        .stderr(predicates::str::contains("-- ").not());
}

/// The startup banner (record count + `.help`/`.schema` hint) is REPL-only:
/// batch mode (piped stdin, no `-e`) must never print it on stdout, since
/// that would corrupt piped output. The REPL-positive path needs a TTY and
/// is out of scope here; `repl`'s `banner_contains_record_count_and_hints`
/// unit test pins the banner's own content.
#[test]
fn batch_mode_stdin_emits_no_startup_banner() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg(td.path())
        .write_stdin("SELECT count(*) AS n;\n")
        .assert()
        .success()
        .stdout(predicates::str::contains("Type .help").not());
}

/// Same guard for `-e`/one-shot mode, which is an entirely separate code path
/// from batch mode's `run_statements`.
#[test]
fn oneshot_emits_no_startup_banner() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT count(*) AS n"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("Type .help").not());
}

#[test]
fn query_error_exits_nonzero() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
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
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
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
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
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
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg(bad)
        .assert()
        .failure()
        .stderr(predicates::str::contains(bad));
}

#[test]
fn invalid_exclude_glob_exits_nonzero() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
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

    let home = TempDir::new().unwrap();
    let out = qm(home.path())
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
fn quiet_suppresses_skipped_file_warnings_but_not_errors() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("good.md"), "---\nstatus: draft\n---\n").unwrap();
    fs::write(td.path().join("bad.md"), "---\n: : broken\n  bad\n---\n").unwrap();
    let home = TempDir::new().unwrap();

    // Without --quiet: the skipped-file warning appears on stderr.
    qm(home.path())
        .args(["-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("querymatter:"));

    // With --quiet: stderr carries no "querymatter: <warning>" chatter for the
    // successful query (the "-- N rows" line is REPL-only, absent in -e mode).
    qm(home.path())
        .args(["--quiet", "-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .success()
        .stderr(predicates::str::is_empty());

    // --quiet must never swallow a real query error.
    qm(home.path())
        .args(["--quiet", "-e", "SELECT FROM WHERE bogus((("])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::is_empty().not());
}

/// B8 (security, resource exhaustion): a file whose on-disk size exceeds a
/// configured `max_file_bytes` is skipped — never handed to
/// `fs::read_to_string` — with a warning naming it on stderr, exactly like an
/// unreadable file or invalid frontmatter; the small file alongside it still
/// surfaces normally. A small test cap (`1000`) via the config knob keeps
/// this test itself fast, rather than writing a real multi-megabyte fixture.
#[test]
fn oversized_file_is_skipped_with_warning() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("big.md"),
        format!("---\ntitle: big\n---\n{}", "x".repeat(20_000)),
    )
    .unwrap();
    fs::write(td.path().join("small.md"), "---\ntitle: small\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "max_file_bytes = 1000\n");
    qm(home.path())
        .arg("-e")
        .arg("SELECT title")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("small"))
        .stdout(predicates::str::contains("big").not())
        .stderr(predicates::str::contains("big.md"));
}

/// B8: `querymatter init` must apply the same `max_file_bytes` cap while
/// building the cache — an oversized file is skipped with a warning here too,
/// not just on a live (`--no-cache`) query, since `init`'s scan is the other
/// caller of `cache::scan_file`.
#[test]
fn init_skips_an_oversized_file_with_warning() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("big.md"),
        format!("---\ntitle: big\n---\n{}", "x".repeat(20_000)),
    )
    .unwrap();
    fs::write(td.path().join("small.md"), "---\ntitle: small\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "max_file_bytes = 1000\n");
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("big.md"));

    qm(home.path())
        .arg("-e")
        .arg("SELECT title")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("small"))
        .stdout(predicates::str::contains("big").not());
}

#[test]
fn batch_good_then_bad_exits_nonzero() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg(td.path())
        .write_stdin("SELECT count(*) AS n;\nSELCT bad;\n")
        .assert()
        .failure();
}

/// A batch/`-e` run of 3 statements whose 2nd fails must name its 1-based
/// position in the error, so a partial `--output` run tells the caller
/// exactly which statement to blame (design W36).
#[test]
fn batch_failure_names_the_statement_index() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT status; SELECT bogus((( ; SELECT status")
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("statement 2 of 3"));
}

/// A lone failing statement needs no "N of M" attribution — `1 of 1` would
/// just be noise.
#[test]
fn single_statement_failure_is_not_indexed() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT bogus(((")
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("statement 1 of 1").not());
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
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();
    assert!(
        td.path().join(".querymatter/manifest.bin").is_file(),
        "init must create <dir>/.querymatter/manifest.bin"
    );
}

/// `build_vault` already pushes a `path: reason` entry onto `LoadReport.
/// warnings` for every file it skips (unreadable content, invalid YAML
/// frontmatter, or — since B6 — a traversal-rejected cached path); before
/// this fix `run_init` printed only the bare `(N skipped)` count and
/// discarded `report.warnings`, so the user had no way to learn which file
/// was skipped or why (silent data loss: that file is then permanently
/// absent from the cache). `bad.md`'s content is genuinely invalid YAML
/// (mirrors `frontmatter::tests::invalid_yaml_is_invalid`), so it fails to
/// parse (`Extract::Invalid` → `ScanResult::Warning`) rather than merely
/// lacking a fence (`Extract::None`, silently and correctly skipped with no
/// warning at all).
#[test]
fn init_reports_which_files_were_skipped() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("good.md"), "---\ntitle: ok\n---\n").unwrap();
    fs::write(
        td.path().join("bad.md"),
        "---\nkey: : : broken\n  bad indent\n---\n",
    )
    .unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("bad.md"));
}

#[test]
fn cache_status_reports_root_and_file_count() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    let canonical = fs::canonicalize(td.path()).unwrap();
    qm(home.path())
        .args(["cache", "status"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains(canonical.display().to_string()))
        .stdout(predicates::str::contains("files:       3"));
}

#[test]
fn cache_status_without_a_vault_exits_nonzero_and_mentions_init() {
    let td = tree(); // no `init`, so no vault exists anywhere above `td`.
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["cache", "status"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("init"));
}

#[test]
fn cache_status_with_corrupt_manifest_exits_nonzero_and_mentions_init() {
    // A vault whose `manifest.bin` is present but unreadable (corrupt, or
    // left behind by an incompatible crate version after an upgrade) is a
    // different failure mode than "no vault at all" — `find_vault` still
    // resolves it, but `cache_summary`'s read of the header fails. That path
    // must still point the user at `querymatter init` (W33).
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    let manifest = td.path().join(".querymatter/manifest.bin");
    fs::write(&manifest, b"not a real manifest").unwrap();

    qm(home.path())
        .args(["cache", "status"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("init"));
}

/// `init`'s walk flags parse into the "init" subcommand's own nested
/// `ArgMatches`, not the top-level one `main` builds `Cli` from —
/// `Settings::resolve_walk` must be handed that nested `ArgMatches`, or every
/// init walk flag (`--hidden` included) would resolve as if it had never been
/// passed, since `WalkFlags` is flattened separately onto `Cli` and
/// `InitArgs` and each gets its own arg registration. Pins that
/// `querymatter init --hidden` actually descends into a dotdir, and that the
/// default (no `--hidden`) still skips it.
#[test]
fn init_hidden_flag_includes_dotdirs() {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join(".hidden")).unwrap();
    fs::write(td.path().join(".hidden/a.md"), "---\nstatus: draft\n---\n").unwrap();
    fs::write(td.path().join("visible.md"), "---\nstatus: draft\n---\n").unwrap();
    let home = TempDir::new().unwrap();

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();
    let out = qm(home.path())
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT count(*) AS n",
            "--format",
            "csv",
            "--force-cache",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(out).unwrap().trim(),
        "n\n1",
        "without --hidden, init must skip the dotdir"
    );

    fs::remove_dir_all(td.path().join(".querymatter")).unwrap();
    qm(home.path())
        .arg("init")
        .arg("--hidden")
        .arg(td.path())
        .assert()
        .success();
    let out = qm(home.path())
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT count(*) AS n",
            "--format",
            "csv",
            "--force-cache",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(out).unwrap().trim(),
        "n\n2",
        "querymatter init --hidden must descend into dotdirs, not silently drop the flag"
    );
}

/// A config-supplied `hidden = true` (no `--hidden` flag at all) must reach
/// `init`'s walk exactly like the flag does above — `Settings::resolve_walk`
/// resolves it, and `main::run_init` must actually use that resolution
/// (RECOMMENDED 5: "a config-supplied scan setting reaching `init`" was
/// previously pinned only for the flag).
#[test]
fn config_hidden_true_reaches_init_without_a_flag() {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join(".hidden")).unwrap();
    fs::write(td.path().join(".hidden/a.md"), "---\nstatus: draft\n---\n").unwrap();
    fs::write(td.path().join("visible.md"), "---\nstatus: draft\n---\n").unwrap();

    let home = TempDir::new().unwrap();
    write_config(home.path(), "hidden = true\n");

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    let out = qm(home.path())
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT count(*) AS n",
            "--format",
            "csv",
            "--force-cache",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(out).unwrap().trim(),
        "n\n2",
        "a configured hidden = true (no --hidden flag) must reach init's walk; got: {}",
        String::from_utf8_lossy(&fs::read(td.path().join("visible.md")).unwrap_or_default())
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
    let home = TempDir::new().unwrap();

    qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // No positional dirs: the run auto-discovers the ancestor vault (cwd is
    // the vault) and queries the whole cache.
    let out = qm(home.path())
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
    let home = TempDir::new().unwrap();

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    let out = qm(home.path())
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
fn subtree_scoped_load_narrows_column_validation_surface() {
    // Spec §7.3: a subtree-scoped one-shot query (positional `[DIRS]`) loads
    // ONLY that subtree from the cache, so a column present ONLY outside the
    // subtree is unknown under default mode — and `--lenient` still bypasses.
    // The fixture puts `status` under plans/ and a product-only `roadmap`
    // under product/; querying `roadmap` scoped to plans/ must fail strict but
    // succeed lenient.
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("plans/a.md", "---\nstatus: draft\n---\n"),
        ("product/b.md", "---\nroadmap: q3\n---\n"),
    ] {
        let f = td.path().join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, s).unwrap();
    }
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // Default mode: `roadmap` is out-of-subtree, so it is an unknown column.
    qm(home.path())
        .current_dir(td.path())
        .args(["-e", "SELECT roadmap", "plans"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("roadmap"));

    // --lenient bypasses validation: the query runs, treating the absent
    // column as NULL, and returns the single plans/ row.
    qm(home.path())
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT roadmap",
            "--lenient",
            "--format",
            "csv",
            "plans",
        ])
        .assert()
        .success();
}

#[test]
fn no_cache_live_scans_even_inside_a_vault() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // `--no-cache` bypasses vault discovery: it live-scans the positional dir.
    let out = qm(home.path())
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
    let home = TempDir::new().unwrap();
    // No `init`, so no vault exists anywhere above `td`.
    qm(home.path())
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
    let home = TempDir::new().unwrap();

    qm(home.path())
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

    let out = qm(home.path())
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
    let home = TempDir::new().unwrap();

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    fs::write(&a, "---\nstatus: ready\n---\n").unwrap();
    File::open(&a)
        .unwrap()
        .set_modified(original_mtime)
        .unwrap();

    let out = qm(home.path())
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

/// MUST-FIX #2 (design W17 + `--refresh` interaction): a narrow one-shot
/// query's projection push-down `wanted` set must not leak into a
/// `--refresh <subtree>` alongside it. `build_session` forces `wanted = None`
/// whenever a refresh flag is present specifically so an out-of-subtree
/// field a narrow query never asked for still survives in the cache a LATER,
/// unrelated query reads — this end-to-end run pins that observable
/// contract; `store::tests::refresh_fallback_preserves_out_of_subtree_fields_when_store_is_unpruned`
/// (and its loses-the-field companion) pin the exact fallback mechanism the
/// guard closes.
#[test]
fn narrow_query_with_refresh_does_not_lose_out_of_subtree_field_from_cache() {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join("plans")).unwrap();
    fs::create_dir_all(td.path().join("product")).unwrap();
    fs::write(td.path().join("plans/a.md"), "---\nstatus: draft\n---\n").unwrap();
    fs::write(td.path().join("product/c.md"), "---\nprd: '011'\n---\n").unwrap();
    let home = TempDir::new().unwrap();

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // A narrow query (`SELECT status` only references "status") combined
    // with `--refresh plans` — the exact shape whose `wanted` must not reach
    // `from_cache` un-widened.
    qm(home.path())
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
        .success();

    // A separate, later query for the DIFFERENT, out-of-subtree field must
    // still find it in the cache the run above left behind.
    let out = qm(home.path())
        .current_dir(td.path())
        .args(["-e", "SELECT prd", "--format", "csv"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s.lines().last().unwrap().trim(),
        "011",
        "an out-of-subtree field must not be lost from the cache after a \
         narrow query ran alongside --refresh on a different subtree; got: {s:?}"
    );
}

#[test]
fn refresh_nonexistent_path_exits_nonzero() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(vault.path())
        .assert()
        .success();

    // A real, existing directory that is NOT under the vault.
    let outside = TempDir::new().unwrap();
    qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    const QUERY: &str = "SELECT status, count(*) AS n GROUP BY status ORDER BY n";

    let cached = qm(home.path())
        .current_dir(td.path())
        .args(["-e", QUERY, "--format", "csv"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let live = qm(home.path())
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
    let home = TempDir::new().unwrap();

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    fs::write(&a, "---\nstatus: done\n---\n").unwrap();

    let out = qm(home.path())
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

/// Task 7 (W56 part 2), test 1 (brief): `WHERE file.body LIKE '%TODO%'`
/// matches a fixture whose body contains it, end to end through the real
/// CLI/cache path (not just the in-process executor unit tests).
#[test]
fn file_body_like_matches_a_fixture_containing_todo() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("has-todo.md"),
        "---\nstatus: draft\n---\nTODO fix this\n",
    )
    .unwrap();
    fs::write(
        td.path().join("no-todo.md"),
        "---\nstatus: draft\n---\nall done\n",
    )
    .unwrap();
    let home = TempDir::new().unwrap();

    let out = qm(home.path())
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT file.name WHERE file.body LIKE '%TODO%'",
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
    assert_eq!(s.lines().last().unwrap().trim(), "has-todo.md");
}

/// Task 10 (B10): a query referencing `file.body` from BOTH `WHERE` and
/// `SELECT` (indirectly, via projecting an unrelated column that's only kept
/// because the `WHERE` matched) must still return the same row/value as
/// before the per-record memo was added — the equivalence pin the brief
/// calls for. The in-process unit test alongside `read_body_cached` proves
/// the memoization itself (one disk read, not two); this end-to-end test
/// proves memoizing it didn't change what the query returns.
#[test]
fn file_body_referenced_twice_is_consistent() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("n.md"),
        "---\ntitle: t\n---\nhello TODO world\n",
    )
    .unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT title WHERE file.body LIKE '%TODO%'")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("t"));
}

/// B8: `file.body` always re-reads fresh from disk (design W56 — the cache
/// never stores body text), so it must respect `max_file_bytes` even for a
/// file whose cached (mtime, size) entry was written under a LARGER cap and
/// hasn't changed since — `refresh_one_file`'s per-file freshness shortcut
/// reuses that entry verbatim (a cheap, safe reuse: no raw content is read),
/// so tightening the cap alone does not retroactively drop the file from the
/// vault, but a later `SELECT file.body` against it must still resolve to
/// NULL rather than buffer the now-too-large body into memory.
#[test]
fn file_body_is_null_for_a_cached_file_over_a_later_lowered_max_file_bytes() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("big.md"),
        format!("---\nstatus: draft\n---\n{}", "x".repeat(20_000)),
    )
    .unwrap();
    let home = TempDir::new().unwrap();

    // Build the cache under the default (large) cap: the file scans fine.
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // Lower the cap after the fact. The file's mtime/size are unchanged, so
    // the default per-file freshness check reuses the cached record without
    // re-reading it — `status` still resolves normally.
    write_config(home.path(), "max_file_bytes = 1000\n");
    qm(home.path())
        .current_dir(td.path())
        .args(["-e", "SELECT status", "."])
        .assert()
        .success()
        .stdout(predicates::str::contains("draft"));

    // But `file.body` must now respect the lowered cap rather than reading
    // the 20 KB body fresh off disk.
    let out = qm(home.path())
        .current_dir(td.path())
        .args(["-e", "SELECT file.body", "--format", "csv", "."])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s.lines().last().unwrap().trim(),
        "\"\"",
        "file.body over a lowered max_file_bytes must be NULL, got: {s:?}"
    );
}

/// Task 7 (W56 part 2), test 2 (brief): under `--force-cache`, a `file.body`
/// query yields NULL under `--lenient`, and a clear diagnostic (nonzero exit,
/// stderr naming `--force-cache`) in strict (default) mode — never a wrong
/// answer.
#[test]
fn file_body_under_force_cache_is_null_lenient_and_diagnostic_strict() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("a.md"),
        "---\nstatus: draft\n---\nTODO fix this\n",
    )
    .unwrap();
    let home = TempDir::new().unwrap();

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // Strict (default): the whole query fails fast with a clear diagnostic.
    qm(home.path())
        .current_dir(td.path())
        .args(["-e", "SELECT file.body", "--force-cache"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("force-cache"));

    // --lenient: the query still runs, resolving file.body to NULL (empty
    // in CSV) rather than erroring or returning stale/wrong text.
    let out = qm(home.path())
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT file.body",
            "--force-cache",
            "--lenient",
            "--format",
            "csv",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s.lines().last().unwrap().trim(),
        "\"\"",
        "file.body under --force-cache --lenient must be NULL (empty in CSV), got: {s:?}"
    );
}

/// Task 7 (W56 part 2), test 3 (brief): a frontmatter-only query performs NO
/// body read — proven by deleting every source file after `init` builds the
/// cache, then running a normal-freshness (not `--force-cache`) query that
/// never references `file.body`. If evaluating an unrelated column ever
/// eagerly touched `file.body`'s disk read, `--force-cache` (which forbids
/// ANY filesystem access beyond the initial cache read) would surface it as
/// a hard failure — the source files are gone, so any such access errors.
/// Plain per-file freshness isn't a suitable vehicle for this check: it
/// would itself notice the deletion and drop the row (a correct, unrelated
/// behavior — see `new_file_added_and_deleted_file_dropped` in
/// `cache.rs`), giving a false signal either way.
#[test]
fn frontmatter_only_query_needs_no_body_read_after_files_deleted() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("a.md"),
        "---\nstatus: draft\n---\nTODO fix this\n",
    )
    .unwrap();
    let home = TempDir::new().unwrap();

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    fs::remove_file(td.path().join("a.md")).unwrap();

    let out = qm(home.path())
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
        "a frontmatter-only --force-cache query must succeed from the cache \
         alone, even though the source file is gone; got: {s:?}"
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
    let home = TempDir::new().unwrap();

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    fs::write(&a, "---\nstatus: done\n---\n").unwrap();

    let out = qm(home.path())
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
    let home = TempDir::new().unwrap();

    qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
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

/// `--table-style` is opt-in: with nothing set, output stays ASCII.
#[test]
fn table_style_defaults_to_ascii() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT status", "--table-style", "fancy"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("fancy"));
}

#[test]
fn bad_table_style_env_var_exits_non_zero() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .env("QUERYMATTER_TABLE_STYLE", "fancy")
        .args(["-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("fancy"));
}

#[test]
fn oneshot_vertical_g_prints_row_blocks() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        // \G forces Output::Vertical, which render() dispatches independent
        // of TableStyle — so this assertion can't actually flip on an
        // ambient QUERYMATTER_TABLE_STYLE today, but the isolation `qm`
        // provides keeps the test from silently growing a dependency on that
        // if vertical rendering ever starts consulting the style.
        .args(["-e", "SELECT status, prd WHERE prd = '011'\\G"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("1. row"))
        .stdout(predicates::str::contains("status: synced"))
        .stdout(predicates::str::contains("+--").not());
}

/// One piped script, two terminators: each statement renders its own way.
#[test]
fn batch_mode_mixes_terminators() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
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
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT status WHERE prd = '011'\\g"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("+--"))
        .stdout(predicates::str::contains("1. row").not());
}

#[test]
fn config_file_supplies_the_table_style() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    qm(home.path())
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

#[test]
fn flag_overrides_the_config_file() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    qm(home.path())
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

#[test]
fn env_overrides_the_config_file() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"ascii\"\n");
    qm(home.path())
        .env("QUERYMATTER_TABLE_STYLE", "unicode")
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

/// Pins `Source::Env` directly (nothing else in the suite asserts it): a
/// unit test can't exercise this without mutating the whole test process's
/// environment (unsafe under `cargo test`'s parallelism), since clap's `env`
/// resolution reads the real process environment during parsing. Spawning
/// the real binary as a child process — as `assert_cmd`'s `.env()` already
/// does for a single child — sidesteps that: `config list`'s `table_style`
/// row must show `(env)`, which only happens if `source_of` maps
/// `ValueSource::EnvVariable` to `Source::Env` correctly (a swapped match arm
/// would still pass `env_overrides_the_config_file` above, since that test
/// only checks the rendered value, not its reported source).
#[test]
fn config_list_reports_env_as_table_styles_source() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"ascii\"\n");
    let out = qm(home.path())
        .env("QUERYMATTER_TABLE_STYLE", "unicode")
        .args(["config", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let table_style_row = row_for(&text, "table_style");
    assert!(
        table_style_row.contains("unicode") && table_style_row.contains("(env)"),
        "table_style row must show the env value and (env) source, got: {table_style_row:?}"
    );
}

// -- Vault-level `.querymatter.toml` (W54): flag > env > vault > config > default

#[test]
fn vault_config_beats_user_config() {
    let td = tree();
    write_vault_config(td.path(), "table_style = \"unicode\"\n");
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"compact\"\n");
    qm(home.path())
        .current_dir(td.path())
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

#[test]
fn flag_beats_vault_config() {
    let td = tree();
    write_vault_config(td.path(), "table_style = \"unicode\"\n");
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT status WHERE prd = '010'",
            "--table-style",
            "ascii",
        ])
        .assert()
        .success()
        .stdout(predicates::str::contains("+--"))
        .stdout(predicates::str::contains("╭").not());
}

#[test]
fn env_beats_vault_config() {
    let td = tree();
    write_vault_config(td.path(), "table_style = \"ascii\"\n");
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .env("QUERYMATTER_TABLE_STYLE", "unicode")
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

#[test]
fn config_list_reports_vault_as_table_styles_source() {
    let td = tree();
    write_vault_config(td.path(), "table_style = \"unicode\"\n");
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"compact\"\n");
    let out = qm(home.path())
        .current_dir(td.path())
        .args(["config", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let table_style_row = row_for(&text, "table_style");
    assert!(
        table_style_row.contains("unicode") && table_style_row.contains("(vault)"),
        "table_style row must show the vault value and (vault) source, got: {table_style_row:?}"
    );
}

/// No `.querymatter.toml` anywhere in `td`'s ancestry: vault discovery must
/// be a pure no-op, falling straight through to the per-user config exactly
/// as it did before this feature existed.
#[test]
fn absent_vault_file_falls_through_to_user_config() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    qm(home.path())
        .current_dir(td.path())
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

/// A broken vault config blocks every command, just like a broken per-user
/// config does (`malformed_config_exits_non_zero_naming_the_path` above); its
/// message must name the vault file specifically, not the user's config.toml.
#[test]
fn malformed_vault_config_exits_nonzero_naming_the_path() {
    let td = tree();
    write_vault_config(td.path(), "table_style = = broken\n");
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["-e", "SELECT status"])
        .assert()
        .failure()
        .stderr(predicates::str::contains(".querymatter.toml"));
}

/// A vault-configured `hidden = true` (no `--hidden` flag, no per-user
/// config) must reach `init`'s walk exactly like a per-user configured one
/// does (`config_hidden_true_reaches_init_without_a_flag` above), proving
/// `run_init` discovers/loads it from the target directory (`args.dir`).
#[test]
fn vault_config_hidden_true_reaches_init_without_a_flag() {
    let td = TempDir::new().unwrap();
    fs::create_dir_all(td.path().join(".hidden")).unwrap();
    fs::write(td.path().join(".hidden/a.md"), "---\nstatus: draft\n---\n").unwrap();
    fs::write(td.path().join("visible.md"), "---\nstatus: draft\n---\n").unwrap();
    write_vault_config(td.path(), "hidden = true\n");

    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    let out = qm(home.path())
        .current_dir(td.path())
        .args([
            "-e",
            "SELECT count(*) AS n",
            "--format",
            "csv",
            "--force-cache",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(out).unwrap().trim(),
        "n\n2",
        "a vault-configured hidden = true must reach init's walk"
    );
}

#[test]
fn config_file_supplies_the_format() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "format = \"json\"\n");
    let out = qm(home.path())
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 2);
}

/// A broken config blocks every command, so its message must name the file.
#[test]
fn malformed_config_exits_non_zero_naming_the_path() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = = broken\n");
    qm(home.path())
        .args(["-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("config.toml"));
}

#[test]
fn unknown_config_key_exits_non_zero_naming_the_key() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "tabel_style = \"unicode\"\n");
    qm(home.path())
        .args(["-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("tabel_style"));
}

#[test]
fn config_set_then_query_honors_it() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["config", "set", "table_style", "unicode"])
        .assert()
        .success();
    qm(home.path())
        .args(["-e", "SELECT status WHERE prd = '010'"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("╭"));
}

#[test]
fn config_list_names_the_source_of_each_value() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    let out = qm(home.path())
        .args(["config", "list"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();

    // The configured key's own row must carry both its value and its
    // source — not just have both strings present anywhere in the output,
    // which wouldn't catch a source/key attribution swap between rows.
    let table_style_row = row_for(&text, "table_style");
    assert!(
        table_style_row.contains("unicode") && table_style_row.contains("(config)"),
        "table_style row must show its value and source together, got: {table_style_row:?}"
    );
    let format_row = row_for(&text, "format");
    assert!(
        format_row.contains("(default)"),
        "format row must show its (unconfigured) source, got: {format_row:?}"
    );
}

#[test]
fn config_get_prints_the_value_and_allowed_values() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    qm(home.path())
        .args(["config", "get", "table_style"])
        .assert()
        .success()
        .stdout(predicates::str::contains("unicode"))
        .stdout(predicates::str::contains("ascii"))
        .stdout(predicates::str::contains("compact"))
        .stdout(predicates::str::contains("plain"));
}

/// A rejected value must not touch the file.
#[test]
fn config_set_rejects_a_bad_value_and_leaves_the_file_alone() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    let before = fs::read_to_string(home.path().join("querymatter/config.toml")).unwrap();
    qm(home.path())
        .args(["config", "set", "table_style", "fancy"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("fancy"))
        .stderr(predicates::str::contains("unicode"));
    let after = fs::read_to_string(home.path().join("querymatter/config.toml")).unwrap();
    assert_eq!(before, after, "a rejected set must not rewrite the file");
}

/// IMPORTANT 1: an invalid `--exclude` glob is rejected by the CLI (see
/// `invalid_exclude_glob_exits_nonzero` above); `config set exclude` must
/// reject one too, naming the offending pattern, and must not touch the
/// file — otherwise a config-supplied `exclude` silently does nothing at
/// scan time instead of failing loudly (the exact reviewer repro this test
/// closes: `querymatter config set exclude '[plans/**'` used to exit 0).
#[test]
fn config_set_rejects_an_invalid_exclude_glob_and_leaves_the_file_alone() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    let before = fs::read_to_string(home.path().join("querymatter/config.toml")).unwrap();
    qm(home.path())
        .args(["config", "set", "exclude", "[plans/**"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("[plans/**"));
    let after = fs::read_to_string(home.path().join("querymatter/config.toml")).unwrap();
    assert_eq!(before, after, "a rejected set must not rewrite the file");
}

/// IMPORTANT 1, second half: `config set` validates the normal write path,
/// but a hand-edited `config.toml` bypasses `config set` entirely — the
/// runtime check must also fire, against the RESOLVED exclude list, so a
/// hand-written bad glob still fails loudly instead of silently matching
/// nothing (`discover::build_exclude_set` has no error channel and drops
/// what doesn't compile).
#[test]
fn hand_written_config_with_invalid_exclude_glob_fails_a_query_naming_the_pattern() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "exclude = [\"[plans/**\"]\n");
    qm(home.path())
        .args(["-e", "SELECT count(*) AS n"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("[plans/**"));
}

/// Same hand-written-config invariant as above, but for `init` rather than a
/// query — the fix touches both call sites (`main::run_query` AND
/// `main::run_init`), and a fix that only covered one would leave `init`
/// silently building a cache with a no-op exclude.
#[test]
fn init_with_hand_written_invalid_exclude_glob_exits_nonzero() {
    let td = tree();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "exclude = [\"[plans/**\"]\n");
    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("[plans/**"));
}

/// RECOMMENDED 5: the spec's first named invariant ("`.querymatterignore`
/// remains the project-local exclusion mechanism... both apply") was
/// untested — a config-supplied `exclude` and the cwd `.querymatterignore`
/// must both take effect on the same query, each excluding a different file.
#[test]
fn config_exclude_and_querymatterignore_both_apply() {
    let td = TempDir::new().unwrap();
    let w = |rel: &str, body: &str| {
        let p = td.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    };
    w("keep.md", "---\nstatus: draft\n---\n");
    w("templates/t.md", "---\nstatus: draft\n---\n");
    w("drafts/d.md", "---\nstatus: draft\n---\n");
    w(".querymatterignore", "drafts/\n");

    let home = TempDir::new().unwrap();
    write_config(home.path(), "exclude = [\"**/templates/**\"]\n");

    let out = qm(home.path())
        .current_dir(td.path())
        .args(["-e", "SELECT count(*) AS n", "--format", "csv", "."])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(
        s.lines().last().unwrap().trim(),
        "1",
        "config exclude AND the cwd .querymatterignore must both apply; got: {s:?}"
    );
}

#[test]
fn config_unset_removes_the_key() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = \"unicode\"\n");
    qm(home.path())
        .args(["config", "unset", "table_style"])
        .assert()
        .success();
    let text = fs::read_to_string(home.path().join("querymatter/config.toml")).unwrap();
    assert!(!text.contains("table_style"), "got:\n{text}");
}

/// `config unset` on a key that was never set, with no config file existing
/// at all, is semantically a no-op: it must not materialize the config file
/// or its parent directory, and it must report accurately rather than
/// claiming a removal that never happened.
#[test]
fn config_unset_on_absent_key_with_no_config_file_is_a_true_no_op() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["config", "unset", "table_style"])
        .assert()
        .success()
        .stderr(predicates::str::contains("was not set"));
    assert!(
        !home.path().join("querymatter").exists(),
        "no config directory must be created for a no-op unset"
    );
}

#[test]
fn config_path_prints_a_path_on_stdout() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicates::str::contains("config.toml"));
}

/// IMPORTANT 4: the README promises `config path` always works, "even when
/// the config file is malformed" — the whole payoff being that a user with a
/// broken config can tab-complete their way to the file worth fixing. Before
/// the fix, only `completions` was dispatched before `config::load()`, so
/// `config path` itself failed on a malformed config — the exact command the
/// README's recovery story depends on.
#[test]
fn config_path_survives_a_malformed_config_while_a_query_still_fails() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = = broken\n");

    qm(home.path())
        .args(["config", "path"])
        .assert()
        .success()
        .stdout(predicates::str::contains("config.toml"));

    let td = tree();
    qm(home.path())
        .args(["-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .failure();
}

#[test]
fn completions_emit_a_script_per_shell() {
    let home = TempDir::new().unwrap();
    for shell in ["bash", "zsh", "fish"] {
        let out = qm(home.path())
            .args(["completions", shell])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let script = String::from_utf8(out).unwrap();
        assert!(
            script.contains("querymatter"),
            "{shell} script must name the binary, got:\n{script}"
        );
        assert!(script.len() > 100, "{shell} script looks empty:\n{script}");
    }
}

/// The completion script must offer the enum values, which is the whole
/// reason Format/TableStyle/ConfigKey became ValueEnums.
#[test]
fn bash_completions_include_enum_values() {
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
        .args(["completions", "bash"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let script = String::from_utf8(out).unwrap();
    assert!(
        script.contains("unicode"),
        "table styles missing:\n{script}"
    );
    assert!(script.contains("tsv"), "formats missing:\n{script}");
    assert!(
        script.contains("table_style"),
        "config keys missing:\n{script}"
    );
}

/// RECOMMENDED 5: completions surviving a malformed config is the entire
/// justification for dispatching it before `config::load()` (spec, plan, and
/// README all name it) — but nothing wrote a broken config before invoking
/// `completions` until now.
#[test]
fn completions_survive_a_malformed_config() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = = broken\n");

    qm(home.path())
        .args(["completions", "bash"])
        .assert()
        .success()
        .stdout(predicates::str::contains("querymatter"));
}

/// `completions --install bash` writes the generated script straight into
/// bash's user completion directory (rather than requiring the caller to
/// redirect stdout there themselves) and confirms the path on stderr.
#[test]
fn completions_install_writes_the_script_into_place() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["completions", "--install", "bash"])
        .assert()
        .success()
        .stderr(predicates::str::contains("installed"));

    let path = home
        .path()
        .join(".local/share/bash-completion/completions/querymatter");
    let script = fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("expected a completion script at {path:?}: {err}"));
    assert!(script.contains("querymatter"), "script:\n{script}");
}

/// zsh installs to a dedicated fpath directory of our own (`~/.zsh/completions`,
/// not `${fpath[1]}` — see the README's note on why) with the `_`-prefixed
/// filename zsh's completion system expects.
#[test]
fn completions_install_writes_zsh_with_underscore_prefix() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["completions", "--install", "zsh"])
        .assert()
        .success();

    let path = home.path().join(".zsh/completions/_querymatter");
    assert!(path.exists(), "expected {path:?} to exist");
}

/// fish installs to `~/.config/fish/completions/<name>.fish`.
#[test]
fn completions_install_writes_fish_to_its_completions_dir() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["completions", "--install", "fish"])
        .assert()
        .success();

    let path = home
        .path()
        .join(".config/fish/completions/querymatter.fish");
    assert!(path.exists(), "expected {path:?} to exist");
}

/// `--install` with no shell named auto-detects one from `$SHELL`.
#[test]
fn completions_install_auto_detects_the_shell_from_env() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .env("SHELL", "/usr/bin/fish")
        .arg("completions")
        .arg("--install")
        .assert()
        .success()
        .stderr(predicates::str::contains("installed"));

    let path = home
        .path()
        .join(".config/fish/completions/querymatter.fish");
    assert!(path.exists(), "expected {path:?} to exist");
}

/// An unwritable/uncreatable target must not leave the user stuck: a clear
/// stderr error, PLUS the same script on stdout `--install` would otherwise
/// have replaced (today's plain behavior). `.local` is pre-created as a
/// plain file (not a directory) so `create_dir_all` deterministically fails —
/// unlike a chmod-based permission test, which a root-run CI would silently
/// ignore.
#[test]
fn completions_install_falls_back_to_stdout_when_the_target_is_unwritable() {
    let home = TempDir::new().unwrap();
    fs::write(home.path().join(".local"), "not a directory").unwrap();

    qm(home.path())
        .args(["completions", "--install", "bash"])
        .assert()
        .success()
        .stderr(predicates::str::contains("could not install"))
        .stdout(predicates::str::contains("querymatter"));
}

/// With no shell named and an unrecognized/absent `$SHELL`, there is no
/// script to produce at all — the one case `--install` treats as a hard
/// error rather than falling back.
#[test]
fn completions_install_errors_clearly_when_the_shell_cannot_be_detected() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .env("SHELL", "/bin/tcsh")
        .arg("completions")
        .arg("--install")
        .assert()
        .failure()
        .stderr(predicates::str::contains("SHELL"));
}

/// The plain (no `--install`) path must be completely unaffected by any of
/// the above: still a required positional, still stdout-only.
#[test]
fn completions_with_no_shell_and_no_install_errors_clearly() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("completions")
        .assert()
        .failure()
        .stderr(predicates::str::contains("required"));
}

/// A typo'd column (`staus` for `status`) must fail the query with a
/// non-zero exit and a message naming both the offending column and the
/// nearest real one, in every mode — here, one-shot `-e`.
#[test]
fn unknown_column_exits_nonzero_with_suggestion() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT staus"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("staus"))
        .stderr(predicates::str::contains("status"));
}

/// `--lenient` restores the pre-validation escape hatch: the same typo'd
/// query that fails above must succeed (as NULL) under the flag.
#[test]
fn lenient_flag_allows_unknown_column() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT staus", "--lenient"])
        .arg(td.path())
        .assert()
        .success();
}

/// The same escape hatch, persisted via `config set lenient true` rather than
/// passed as a flag each time — pins the config-layer precedence, not just
/// the flag.
#[test]
fn configured_lenient_allows_unknown_column_without_the_flag() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "lenient = true\n");
    let td = tree();

    qm(home.path())
        .args(["-e", "SELECT staus"])
        .arg(td.path())
        .assert()
        .success();
}

/// `--no-lenient` overrides a configured `lenient = true` back to strict for
/// one invocation — the same symmetry `--no-hidden` gives `hidden`.
#[test]
fn no_lenient_overrides_configured_lenient() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["config", "set", "lenient", "true"])
        .assert()
        .success();
    let td = tree();

    qm(home.path())
        .args(["-e", "SELECT staus", "--no-lenient"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("staus"));

    qm(home.path())
        .args(["-e", "SELECT staus"])
        .arg(td.path())
        .assert()
        .success();
}

#[test]
fn exit_code_zero_when_rows_match() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT status WHERE prd = '010'", "--exit-code"])
        .arg(td.path())
        .assert()
        .code(0);
}

#[test]
fn exit_code_one_when_no_rows() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT status WHERE prd = 'nope'", "--exit-code"])
        .arg(td.path())
        .assert()
        .code(1);
}

#[test]
fn exit_code_two_on_error() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT (", "--exit-code"])
        .arg(td.path())
        .assert()
        .code(2);
}

/// MUST-FIX: `--exit-code`'s grep-style 0/1/2 error mapping (`is_query_like`
/// in `main.rs`) applies only to query mode and `query run` — every other
/// subcommand error stays exit 1 even under `--exit-code`. `query get` is a
/// `query` subcommand ACTION but not a query RUN, so it must stay 1, same
/// for a rejected `query save`; `query run` on an unknown name IS
/// query-like (it resolves a name to SQL and runs it exactly like `-e`
/// would), so it's 2, for contrast.
#[test]
fn exit_code_only_maps_query_mode_errors_not_every_query_subcommand_error() {
    let home = TempDir::new().unwrap();

    qm(home.path())
        .args(["--exit-code", "query", "get", "nope"])
        .assert()
        .code(1);

    qm(home.path())
        .args(["--exit-code", "query", "save", "has space", "SELECT status"])
        .assert()
        .code(1);

    qm(home.path())
        .args(["--exit-code", "query", "run", "nope"])
        .assert()
        .code(2);
}

#[test]
fn without_exit_code_zero_rows_still_exits_zero() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT status WHERE prd = 'nope'"])
        .arg(td.path())
        .assert()
        .code(0);
}

#[test]
fn output_flag_writes_file_and_stdout_is_empty() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let out = td.path().join("res.txt");
    qm(home.path())
        .args([
            "-e",
            "SELECT status WHERE prd = '010'",
            "--format",
            "csv",
            "--output",
            out.to_str().unwrap(),
        ])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::is_empty());
    let body = fs::read_to_string(&out).unwrap();
    assert!(body.contains("status"));
}

/// Every statement in one `--output` run appends to the same file — the
/// second statement's result must not truncate the first's away.
#[test]
fn output_flag_appends_multiple_statements_in_one_run() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let out = td.path().join("res.txt");
    qm(home.path())
        .args([
            "-e",
            "SELECT status WHERE prd = '010'; SELECT status WHERE prd = '011';",
            "--format",
            "csv",
            "--output",
            out.to_str().unwrap(),
        ])
        .arg(td.path())
        .assert()
        .success();
    let body = fs::read_to_string(&out).unwrap();
    assert_eq!(
        body.matches("status").count(),
        2,
        "each statement's CSV header should appear once: {body:?}"
    );
}

/// A stale file already at the `--output` path is truncated at the start of
/// the run, not appended to.
#[test]
fn output_flag_truncates_a_preexisting_file() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let out = td.path().join("res.txt");
    fs::write(&out, "stale content that must not survive\n").unwrap();
    qm(home.path())
        .args([
            "-e",
            "SELECT status WHERE prd = '010'",
            "--output",
            out.to_str().unwrap(),
        ])
        .arg(td.path())
        .assert()
        .success();
    let body = fs::read_to_string(&out).unwrap();
    assert!(!body.contains("stale content"));
}

/// A `--output` path that can't be opened for writing is a clean, propagated
/// error, not a panic, and the process exits non-zero.
#[test]
fn output_flag_errors_cleanly_on_an_unwritable_path() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let bad = td.path().join("no-such-dir").join("res.txt");
    qm(home.path())
        .args(["-e", "SELECT status", "--output", bad.to_str().unwrap()])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("querymatter:"));
}

// --- `querymatter query` (saved named queries) ---

#[test]
fn query_save_then_get_and_list() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["query", "save", "stale", "SELECT status WHERE prd = '010'"])
        .assert()
        .success()
        .stderr(predicates::str::contains("stale"));

    qm(home.path())
        .args(["query", "get", "stale"])
        .assert()
        .success()
        .stdout("SELECT status WHERE prd = '010'\n");

    qm(home.path())
        .args(["query", "list"])
        .assert()
        .success()
        .stdout("stale\tSELECT status WHERE prd = '010'\n");
}

/// `query list` with nothing saved prints nothing on stdout — it must not
/// error just because `queries.toml` has never been written.
#[test]
fn query_list_is_empty_when_nothing_is_saved() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["query", "list"])
        .assert()
        .success()
        .stdout(predicates::str::is_empty());
}

/// A query that fails to parse is rejected up front, naming the parse
/// error — it must never be possible to save a query that only breaks later
/// at `query run`.
#[test]
fn query_save_rejects_a_query_that_fails_to_parse() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["query", "save", "broken", "SELECT ("])
        .assert()
        .failure();

    // A rejected save must not persist anything.
    qm(home.path())
        .args(["query", "list"])
        .assert()
        .success()
        .stdout(predicates::str::is_empty());
}

#[test]
fn query_save_rejects_a_bad_name() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["query", "save", "has space", "SELECT status"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("has space"));
}

/// A name collision on `save` overwrites — last-write-wins, like `config set`.
#[test]
fn query_save_overwrites_an_existing_name() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["query", "save", "stale", "SELECT status"])
        .assert()
        .success();
    qm(home.path())
        .args(["query", "save", "stale", "SELECT status WHERE prd = '010'"])
        .assert()
        .success();
    qm(home.path())
        .args(["query", "get", "stale"])
        .assert()
        .success()
        .stdout("SELECT status WHERE prd = '010'\n");
}

#[test]
fn query_get_unknown_name_errors() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["query", "get", "nope"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("nope"));
}

#[test]
fn query_delete_removes_a_saved_query() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["query", "save", "stale", "SELECT status"])
        .assert()
        .success();
    qm(home.path())
        .args(["query", "delete", "stale"])
        .assert()
        .success()
        .stderr(predicates::str::contains("stale"));
    qm(home.path())
        .args(["query", "get", "stale"])
        .assert()
        .failure();
}

/// Deleting a name that was never saved (with no `queries.toml` at all) is
/// reported on stderr, not an error — mirrors `config unset` on an absent key.
#[test]
fn query_delete_on_absent_name_is_not_an_error() {
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["query", "delete", "nope"])
        .assert()
        .success()
        .stderr(predicates::str::contains("no saved query named 'nope'"));
}

/// `query run <name>` must produce byte-identical output to `-e` given the
/// same resolved SQL — it is fed through the SAME store-building/rendering
/// path, not a parallel implementation.
#[test]
fn query_run_matches_dash_e_byte_for_byte() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let sql = "SELECT status, count(*) AS Count GROUP BY status ORDER BY Count DESC";

    qm(home.path())
        .args(["query", "save", "counts", sql])
        .assert()
        .success();

    let via_dash_e = qm(home.path())
        .current_dir(td.path())
        .args(["-e", sql])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let via_query_run = qm(home.path())
        .current_dir(td.path())
        .args(["query", "run", "counts"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(via_dash_e, via_query_run);
}

/// `query run` honors `--format`/`--output`, exactly like `-e` — proof it
/// shares `run_query`'s machinery rather than a parallel path that could
/// silently ignore them.
#[test]
fn query_run_honors_format_and_output_flags() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["query", "save", "drafts", "SELECT status WHERE prd = '010'"])
        .assert()
        .success();

    let out = td.path().join("res.csv");
    qm(home.path())
        .current_dir(td.path())
        .args([
            "--format",
            "csv",
            "--output",
            out.to_str().unwrap(),
            "query",
            "run",
            "drafts",
        ])
        .assert()
        .success()
        .stdout(predicates::str::is_empty());
    let body = fs::read_to_string(&out).unwrap();
    assert!(body.contains("status"), "got: {body:?}");
}

/// `query run` participates in `--exit-code`'s grep-style mapping exactly
/// like `-e` does: 0 when rows matched, 1 when none did.
#[test]
fn query_run_honors_exit_code_flag() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["query", "save", "drafts", "SELECT status WHERE prd = '010'"])
        .assert()
        .success();
    qm(home.path())
        .current_dir(td.path())
        .args(["--exit-code", "query", "run", "drafts"])
        .assert()
        .code(0);

    qm(home.path())
        .current_dir(td.path())
        .args(["query", "save", "none", "SELECT status WHERE prd = 'nope'"])
        .assert()
        .success();
    qm(home.path())
        .current_dir(td.path())
        .args(["--exit-code", "query", "run", "none"])
        .assert()
        .code(1);
}

/// FOLD-IN: `query run <name> [DIR]` scans `[DIR]` instead of cwd/vault when
/// given — exactly like a positional `[DIRS]` entry would in query mode
/// (the live-scan counterpart of `positional_dir_restricts_vault_query_to_that_subtree`,
/// which covers the vault-narrowing case for `-e`). Run from an unrelated
/// cwd containing BOTH subtrees, `query run names <dir>` must match `-e`
/// scoped to `<dir>` via `.current_dir`, byte for byte.
#[test]
fn query_run_dir_argument_scans_that_directory() {
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("a/one.md", "---\nstatus: draft\n---\n"),
        ("b/two.md", "---\nstatus: synced\n---\n"),
    ] {
        let f = td.path().join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, s).unwrap();
    }
    let home = TempDir::new().unwrap();
    let sql = "SELECT file.name";

    qm(home.path())
        .args(["query", "save", "names", sql])
        .assert()
        .success();

    let via_dash_e = qm(home.path())
        .current_dir(td.path().join("a"))
        .args(["-e", sql])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let via_query_run = qm(home.path())
        .current_dir(td.path())
        .args(["query", "run", "names"])
        .arg(td.path().join("a"))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    assert_eq!(via_dash_e, via_query_run);
    let text = String::from_utf8(via_query_run).unwrap();
    assert!(text.contains("one.md"), "got: {text:?}");
    assert!(!text.contains("two.md"), "got: {text:?}");
}

#[test]
fn query_run_unknown_name_errors() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["query", "run", "nope"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("nope"));
}

/// MUST-FIX: `query save`/`list`/`get`/`delete` operate only on
/// `queries.toml`, a file separate from `config.toml` — they must not be
/// blocked by a broken `config.toml`, mirroring `config path`'s and
/// `completions`' own pre-`config::load()` dispatch (see
/// `config_path_survives_a_malformed_config_while_a_query_still_fails`).
/// `query run` DOES build a session, so it still needs a valid config and is
/// expected to fail, just like an ordinary query.
#[test]
fn query_actions_survive_a_malformed_config_but_run_still_fails() {
    let home = TempDir::new().unwrap();
    write_config(home.path(), "table_style = = broken\n");

    qm(home.path())
        .args(["query", "save", "stale", "SELECT status"])
        .assert()
        .success()
        .stderr(predicates::str::contains("stale"));

    qm(home.path())
        .args(["query", "list"])
        .assert()
        .success()
        .stdout("stale\tSELECT status\n");

    qm(home.path())
        .args(["query", "get", "stale"])
        .assert()
        .success()
        .stdout("SELECT status\n");

    // `query run` DOES build a session, so — unlike its config-free siblings
    // above — it still needs a valid config, and fails here naming
    // config.toml just like an ordinary query would.
    qm(home.path())
        .args(["query", "run", "stale"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("config.toml"));

    qm(home.path())
        .args(["query", "delete", "stale"])
        .assert()
        .success()
        .stderr(predicates::str::contains("stale"));

    // An ordinary query still errors too, naming config.toml — the broken
    // file is real, just irrelevant to the four config-free actions above.
    let td = tree();
    qm(home.path())
        .args(["-e", "SELECT status"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("config.toml"));
}

/// A broken `queries.toml` blocks every `query` action, so its message must
/// name the file — mirrors `malformed_config_exits_non_zero_naming_the_path`.
#[test]
fn malformed_queries_file_exits_non_zero_naming_the_path() {
    let home = TempDir::new().unwrap();
    let dir = home.path().join("querymatter");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("queries.toml"), "= = broken\n").unwrap();

    qm(home.path())
        .args(["query", "list"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("queries.toml"));
}

// --- Task 3 (W17): projection push-down ---------------------------------
//
// A one-shot/`-e`/piped-batch/`query run` invocation knows its statement
// text before the record store is built, so it materializes only the field
// VALUES the query references instead of cloning every field of every file.
// The load-bearing correctness constraint: `store.schema()` (and therefore
// unknown-column validation + did-you-mean) must stay the FULL field-name
// union regardless of which values got pruned — see
// `typo_under_pushdown_still_errors_with_didyoumean` below.

/// THE load-bearing guard: a typo'd column, under push-down, over a vault
/// whose files have OTHER fields the query never references (`prd`), must
/// still error naming the typo and suggesting the real field — identical to
/// pre-push-down behavior. `SELECT staus` references only the literal
/// (misspelled) field `staus`, so push-down would prune every record down to
/// zero real values; if validation were derived from the pruned records'
/// own fields (rather than the store's independently tracked full schema),
/// the schema would look empty and the typo would silently stop erroring.
#[test]
fn typo_under_pushdown_still_errors_with_didyoumean() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args(["-e", "SELECT staus"])
        .arg(td.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown column `staus`"))
        .stderr(predicates::str::contains("did you mean 'status'"));
}

/// A narrow query's push-down-pruned output must match the SAME column
/// projected out of a `SELECT *` run, which projects
/// [`crate::query::ast::SelectExpr::Star`] and so disables push-down
/// (every field materialized). Same `WHERE`, same fixture -> same rows in
/// the same order, so this proves pruning `prd`'s value out of every
/// `Record` never changes what `status` reads as.
#[test]
fn pushdown_output_is_byte_identical() {
    let td = tree();
    let home = TempDir::new().unwrap();

    let narrow = qm(home.path())
        .args(["-e", "SELECT status WHERE prd = '010'", "--format", "json"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let full = qm(home.path())
        .args(["-e", "SELECT * WHERE prd = '010'", "--format", "json"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let narrow_rows: Vec<serde_json::Value> = serde_json::from_slice(&narrow).unwrap();
    let full_rows: Vec<serde_json::Value> = serde_json::from_slice(&full).unwrap();
    assert_eq!(narrow_rows.len(), full_rows.len());
    assert!(!narrow_rows.is_empty());

    let projected_from_full: Vec<serde_json::Value> = full_rows
        .iter()
        .map(|row| serde_json::json!({ "status": row["status"] }))
        .collect();
    assert_eq!(
        narrow_rows, projected_from_full,
        "a push-down-pruned narrow query must return the exact same values \
         a SELECT * run (which disables push-down) returns for that column"
    );
}

/// `SELECT *` must disable pruning entirely: every frontmatter field present
/// on the fixture files (`status`, `prd`) shows up on every row, not just
/// the columns some OTHER statement in the same run might have referenced.
#[test]
fn select_star_disables_pruning() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
        .args(["-e", "SELECT *", "--format", "json"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let rows: Vec<serde_json::Value> = serde_json::from_slice(&out).unwrap();
    assert!(!rows.is_empty());
    for row in &rows {
        let obj = row.as_object().expect("each row is a JSON object");
        assert!(obj.contains_key("status"), "row missing status: {row}");
        assert!(obj.contains_key("prd"), "row missing prd: {row}");
    }
}

/// `count(*)` references no field at all, so push-down prunes every value
/// out of every materialized `Record` — but the row count must still be
/// correct, since counting rows never needed the field values to begin with.
#[test]
fn count_star_pushdown_prunes_all_fields_but_counts_correctly() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
        .args(["-e", "SELECT count(*) AS n", "--format", "csv"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(String::from_utf8(out).unwrap().trim(), "n\n3");
}

/// Builds a tree with one file each layer of `explain` can attribute an
/// exclusion to: a plain included markdown file, a wrong-extension file, and
/// a file inside a hidden directory.
fn explain_tree() -> TempDir {
    let td = TempDir::new().unwrap();
    let w = |rel: &str, body: &str| {
        let p = td.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    };
    w("keep.md", "---\nstatus: x\n---\n");
    w("todo.txt", "x");
    w(".draft/h.md", "---\nstatus: y\n---\n");
    td
}

#[test]
fn explain_reports_included_for_a_discovered_file() {
    let td = explain_tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["explain", "keep.md"])
        .assert()
        .success()
        .stdout(predicates::str::contains("included"));
}

#[test]
fn explain_attributes_wrong_extension() {
    let td = explain_tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["explain", "todo.txt"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "excluded: extension 'txt' not in --ext (md, markdown)",
        ));
}

#[test]
fn explain_attributes_a_hidden_directory() {
    let td = explain_tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["explain", ".draft/h.md"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "excluded: hidden directory '.draft' (pass --hidden to include)",
        ));
}

/// The shared `WalkFlags` (here `--hidden`) must reach `explain` through its
/// `#[command(flatten)]`, exactly like `init`'s — a hidden file explain would
/// otherwise always report excluded regardless of the flag.
#[test]
fn explain_hidden_flag_flips_a_hidden_directory_file_to_included() {
    let td = explain_tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["explain", ".draft/h.md", "--hidden"])
        .assert()
        .success()
        .stdout(predicates::str::contains("included"));
}

#[test]
fn explain_attributes_an_exclude_glob_flag() {
    let td = explain_tree();
    fs::create_dir_all(td.path().join("templates")).unwrap();
    fs::write(td.path().join("templates/t.md"), "---\nstatus: x\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["explain", "templates/t.md", "--exclude", "**/templates/**"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "excluded: matches --exclude glob '**/templates/**'",
        ));
}

#[test]
fn explain_nonexistent_path_is_a_clean_error() {
    let td = explain_tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["explain", "no-such-file.md"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no-such-file.md"));
}

/// A path outside the implicit scan root (the current directory) is a clean
/// error rather than a silently wrong verdict.
#[test]
fn explain_path_outside_cwd_is_a_clean_error() {
    let td = explain_tree();
    let outside = TempDir::new().unwrap();
    fs::write(outside.path().join("elsewhere.md"), "---\nstatus: x\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .current_dir(td.path())
        .args(["explain"])
        .arg(outside.path().join("elsewhere.md"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("outside"));
}

/// MUST-FIX: `explain` must root at the ancestor `.querymatter` vault when
/// one exists, exactly like a real query does — not always at cwd. From a
/// vault subdirectory, `explain ../top.md` names a file that IS under the
/// vault root (and so IS what a real query from this subdirectory would
/// scan) even though it is outside cwd; before the fix this was rejected as
/// "outside the current directory".
#[test]
fn explain_roots_at_the_vault_from_a_subdirectory_matching_a_real_query() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("top.md"), "---\nstatus: x\n---\n").unwrap();
    let sub = td.path().join("sub");
    fs::create_dir_all(&sub).unwrap();
    let home = TempDir::new().unwrap();

    qm(home.path())
        .arg("init")
        .arg(td.path())
        .assert()
        .success();

    // A real query from `sub` discovers `top.md` via the vault.
    qm(home.path())
        .current_dir(&sub)
        .args(["-e", "SELECT file.name WHERE file.name = 'top.md'"])
        .assert()
        .success()
        .stdout(predicates::str::contains("top.md"));

    // `explain` from the same directory must agree, not reject the path as
    // outside cwd.
    qm(home.path())
        .current_dir(&sub)
        .args(["explain", "../top.md"])
        .assert()
        .success()
        .stdout(predicates::str::contains("included"));
}

/// Property: `explain`'s verdict must match `discover()`'s actual membership
/// for every file in a small tree exercising every filter layer at once.
#[test]
fn explain_verdict_matches_discover_membership_end_to_end() {
    let td = explain_tree();
    fs::create_dir_all(td.path().join("templates")).unwrap();
    fs::write(td.path().join("templates/t.md"), "---\nstatus: x\n---\n").unwrap();
    let home = TempDir::new().unwrap();

    let cases = [
        ("keep.md", true),
        ("todo.txt", false),
        (".draft/h.md", false),
        ("templates/t.md", false),
    ];
    for (rel, included) in cases {
        let assert = qm(home.path())
            .current_dir(td.path())
            .args(["explain", rel, "--exclude", "**/templates/**"])
            .assert()
            .success();
        let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
        if included {
            assert!(out.contains("included"), "{rel}: got {out:?}");
        } else {
            assert!(out.contains("excluded:"), "{rel}: got {out:?}");
        }
    }
}

/// I2 (W57 characterization): a strict-ISO `created` field auto-detects to
/// `Value::Date` at ingest rather than staying `Value::Str`, and the
/// relative-date literal it's compared against resolves to an ISO string —
/// pin that `WHERE created > '-7d'` still returns exactly the rows it would
/// have before dates existed, end to end through the real binary. Fixture
/// dates are computed from the real clock (this codebase has no `--now`
/// override for the CLI, unlike `execute_with_schema_at`'s test-only seam),
/// so the test stays correct regardless of when it runs.
#[test]
fn relative_date_filter_matches_after_auto_detected_date_type() {
    use chrono::{DateTime, Duration, Utc};

    // `chrono`'s "clock" feature is off (see Cargo.toml), so read the real
    // time via `std::time::SystemTime::now()` and convert, rather than
    // `Utc::now()` directly — the same pattern `model::system_time_to_iso`
    // and `query::exec::resolve_reldate` already use.
    let today = DateTime::<Utc>::from(std::time::SystemTime::now()).date_naive();
    let recent = (today - Duration::days(3)).format("%Y-%m-%d").to_string();
    let old = (today - Duration::days(30)).format("%Y-%m-%d").to_string();

    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("recent.md"),
        format!("---\ncreated: {recent}\n---\n"),
    )
    .unwrap();
    fs::write(
        td.path().join("old.md"),
        format!("---\ncreated: {old}\n---\n"),
    )
    .unwrap();

    let home = TempDir::new().unwrap();
    let out = qm(home.path())
        .args([
            "-e",
            "SELECT file.name WHERE created > '-7d'",
            "--format",
            "csv",
        ])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(out).unwrap().trim(),
        "file.name\nrecent.md"
    );
}

/// I1 (W57 characterization): a `Value::Date` field must render its ISO text
/// byte-identical to the source frontmatter string, across every output
/// format — a date must not render any differently from the `Value::Str` it
/// would have been before auto-detection.
#[test]
fn date_field_renders_verbatim_across_csv_json_table() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("a.md"), "---\ncreated: 2026-03-15\n---\n").unwrap();
    let home = TempDir::new().unwrap();

    let csv = qm(home.path())
        .args(["-e", "SELECT created", "--format", "csv"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        String::from_utf8(csv).unwrap().trim(),
        "created\n2026-03-15"
    );

    let json = qm(home.path())
        .args(["-e", "SELECT created", "--format", "json"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(
        v.as_array().unwrap()[0]["created"],
        serde_json::Value::String("2026-03-15".into())
    );

    qm(home.path())
        .args(["-e", "SELECT created", "--format", "table"])
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("2026-03-15"));
}

/// W43: a scalar function on the tested side of LIKE (matches `=`'s ability).
#[test]
fn where_scalar_expr_like() {
    let home = TempDir::new().unwrap();
    let td = tree(); // plans/a.md=draft, plans/b.md=synced, product/c.md=synced
    qm(home.path())
        .args([
            "-e",
            "SELECT status WHERE lower(status) LIKE '%draf%'",
            "--format",
            "csv",
            td.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout("status\ndraft\n");
}

/// W43: IN accepts a scalar expression on the tested side.
#[test]
fn where_scalar_expr_in() {
    let home = TempDir::new().unwrap();
    let td = tree();
    qm(home.path())
        .args([
            "-e",
            "SELECT status WHERE lower(status) IN ('draft','synced')",
            "--format",
            "csv",
            td.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout("status\ndraft\nsynced\nsynced\n");
}

/// W43: IS NULL / IS NOT NULL accept a scalar expression on the tested side.
/// `b.md` omits `status` entirely, so `lower(status)` evaluates to
/// `Value::Null` for it (a scalar function propagates a NULL argument) —
/// pinning that `lower(status) IS NULL` genuinely evaluates the expression
/// rather than falling back to some literal-only check.
#[test]
fn where_scalar_expr_isnull() {
    let home = TempDir::new().unwrap();
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("a.md", "---\nstatus: DRAFT\n---\n"),
        ("b.md", "---\ntitle: untagged\n---\n"),
    ] {
        let f = td.path().join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, s).unwrap();
    }

    qm(home.path())
        .args([
            "-e",
            "SELECT file.name WHERE lower(status) IS NULL",
            "--format",
            "csv",
            td.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout("file.name\nb.md\n");

    qm(home.path())
        .args([
            "-e",
            "SELECT file.name WHERE lower(status) IS NOT NULL",
            "--format",
            "csv",
            td.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout("file.name\na.md\n");
}

/// W43: MEMBER OF now accepts a bare column (and any expression) on the left —
/// impossible before (the left had to be a literal).
#[test]
fn where_column_member_of_list() {
    let home = TempDir::new().unwrap();
    let td = TempDir::new().unwrap();
    for (p, s) in [
        (
            "x.md",
            "---\nlead: mobile\ntags:\n  - mobile\n  - web\n---\n",
        ),
        ("y.md", "---\nlead: infra\ntags:\n  - web\n---\n"),
    ] {
        let f = td.path().join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, s).unwrap();
    }
    qm(home.path())
        .args([
            "-e",
            "SELECT lead WHERE lead MEMBER OF(tags)",
            "--format",
            "csv",
            td.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout("lead\nmobile\n"); // x.md: 'mobile' is in tags; y.md: 'infra' is not
}

/// W43 invariant I1: `file.body` referenced through the WIDENED predicate funnel
/// must still be detected, so `--force-cache` still fails fast (W56 guard).
#[test]
fn force_cache_file_body_like_still_errors_after_widening() {
    let home = TempDir::new().unwrap();
    let td = tree();
    qm(home.path())
        .args([
            "-e",
            "SELECT file.name WHERE file.body LIKE '%draft%'",
            "--force-cache",
            td.path().to_str().unwrap(),
        ])
        .assert()
        .failure()
        .stderr(predicates::str::contains("force-cache"));
}

/// W47 W3: piped csv/tsv/json output is byte-for-byte the expected bytes
/// (streaming must not perturb them). Rows come back sorted by path:
/// plans/a.md=draft, plans/b.md=synced, product/c.md=synced.
#[test]
fn streaming_formats_are_byte_identical() {
    let home = TempDir::new().unwrap();
    let td = tree();
    let path = td.path().to_str().unwrap();

    qm(home.path())
        .args(["-e", "SELECT status", "--format", "csv", path])
        .assert()
        .success()
        .stdout("status\ndraft\nsynced\nsynced\n");

    qm(home.path())
        .args(["-e", "SELECT status", "--format", "tsv", path])
        .assert()
        .success()
        .stdout("status\ndraft\nsynced\nsynced\n");

    qm(home.path())
        .args(["-e", "SELECT status", "--format", "json", path])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("[\n"))
        .stdout(predicate::str::contains("\"status\": \"draft\""))
        .stdout(predicate::str::ends_with("]\n"));
}

/// W47 W2: `--output <file>` writes identical bytes to the file (not stdout).
#[test]
fn streaming_output_flag_writes_file_bytes() {
    let home = TempDir::new().unwrap();
    let td = tree();
    let out = td.path().join("result.csv");
    qm(home.path())
        .args([
            "-e",
            "SELECT status",
            "--format",
            "csv",
            "--output",
            out.to_str().unwrap(),
            td.path().to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(""); // nothing on stdout when redirected
    assert_eq!(
        fs::read_to_string(&out).unwrap(),
        "status\ndraft\nsynced\nsynced\n"
    );
}

/// Spec W1: a zero-row result renders to the empty JSON array plus one
/// trailing newline. `streaming_formats_are_byte_identical` above never
/// exercises zero rows, so this was previously "byte-identical by
/// construction" rather than actually pinned — this asserts the exact bytes
/// the streaming JSON path emits, not merely that it produced valid JSON.
#[test]
fn empty_result_json_is_bracket_bracket_newline() {
    let td = tree();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .args([
            "-e",
            "SELECT status WHERE status = 'nonexistent'",
            "--format",
            "json",
        ])
        .arg(td.path())
        .assert()
        .success()
        .stdout("[]\n");
}

/// Task 1 (B1): pins `LIKE`'s match semantics across the hoisting of its
/// pattern-to-`Regex` compile from once-per-row to once-per-query — same
/// matches, same case-sensitivity, same anchoring as before the refactor.
#[test]
fn like_matches_are_stable_after_hoisting() {
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("a.md", "---\ntitle: alpha\n---\n"),
        ("b.md", "---\ntitle: beta\n---\n"),
        ("c.md", "---\ntitle: alphabet\n---\n"),
    ] {
        fs::write(td.path().join(p), s).unwrap();
    }
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT title WHERE title LIKE 'alpha%' ORDER BY title")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("alpha"))
        .stdout(predicate::str::contains("alphabet"))
        .stdout(predicate::str::contains("beta").not());
}

/// Task 2 (B2): pins `REGEXP`'s match semantics across the hoisting of its
/// pattern-to-`Regex` compile from once-per-row to once-per-query — same
/// matches as before the refactor.
#[test]
fn regexp_matches_are_stable_after_hoisting() {
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("a.md", "---\ncode: A123\n---\n"),
        ("b.md", "---\ncode: B999\n---\n"),
    ] {
        fs::write(td.path().join(p), s).unwrap();
    }
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT code WHERE code REGEXP '^A[0-9]+$'")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("A123"))
        .stdout(predicate::str::contains("B999").not());
}

/// Task 3 (B3) RED: frontmatter is fully attacker-controlled (any Markdown
/// file in a queried vault). A `title` carrying a raw ESC screen-clear
/// sequence must not reach the terminal unsanitized in the default table
/// render — today it does, since `Value::display()` is written verbatim.
#[test]
fn table_output_neutralizes_ansi_escapes_from_frontmatter() {
    let td = TempDir::new().unwrap();
    // title carries a raw ESC (0x1b) screen-clear sequence
    fs::write(
        td.path().join("evil.md"),
        "---\ntitle: \"\u{1b}[2J[H spoof\"\n---\n",
    )
    .unwrap();
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
        .arg("-e")
        .arg("SELECT title")
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    // The default (table) render must not emit the raw ESC byte.
    assert!(!out.contains(&0x1b), "raw ESC leaked into table output");
}

/// Task 3 (B3) INV-1 pin: the table/vertical sanitizer must not touch the
/// json interchange path. The same ESC-bearing value must still come through
/// as serde's own `` escape, byte-identical to before this fix.
#[test]
fn json_output_unchanged_by_terminal_sanitizer() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("evil.md"), "---\ntitle: \"\u{1b}x\"\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT title")
        .arg("--format")
        .arg("json")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("\\u001b"));
}

/// Task 3 (B3) review fix: the other sanitized path besides table — pins
/// that `render_vertical` (`\G`) also neutralizes the raw ESC byte, mirroring
/// `table_output_neutralizes_ansi_escapes_from_frontmatter` above.
#[test]
fn vertical_output_neutralizes_ansi_escapes_from_frontmatter() {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("evil.md"),
        "---\ntitle: \"\u{1b}[2J[H spoof\"\n---\n",
    )
    .unwrap();
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
        .args(["-e", "SELECT title\\G"])
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(!out.contains(&0x1b), "raw ESC leaked into vertical output");
}

/// Task 3 (B3) review fix, INV-1 pin: csv stays byte-untouched by the
/// terminal sanitizer — mirrors `json_output_unchanged_by_terminal_sanitizer`
/// above, but for `--format csv`, where the ESC byte survives raw (csv has no
/// escaping mechanism of its own for control bytes the way json's `\uXXXX`
/// does).
#[test]
fn csv_output_unchanged_by_terminal_sanitizer() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("evil.md"), "---\ntitle: \"\u{1b}x\"\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT title")
        .arg("--format")
        .arg("csv")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}"));
}

/// Task 3 (B3) review fix (FIX 1 scope pin): Markdown (`Format::Md`) is a
/// fixed interchange format, not a terminal display, so it must NOT be
/// sanitized — the raw ESC byte must survive `--format md` untouched. Pins
/// FIX 1's decision so a future "just pass sanitize through" refactor of
/// `new_table`'s shared plumbing can't silently start scrubbing Markdown.
#[test]
fn md_output_unchanged_by_terminal_sanitizer() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("evil.md"), "---\ntitle: \"\u{1b}x\"\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT title")
        .arg("--format")
        .arg("md")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("\u{1b}"));
}

/// Task 4 (B4) characterization: a downstream reader that closes its end of
/// the pipe early (`| head`) must NOT make querymatter panic (Rust's default
/// SIGPIPE=SIG_IGN turns the resulting write into an `EPIPE` `io::Error`,
/// which `println!`/`write!` on stdout convert into a panic) or print
/// `Broken pipe` noise to stderr from the streaming result path. Uses raw
/// `std::process::Command`, NOT `assert_cmd`: `assert_cmd::Command::assert`
/// captures the whole child's stdout before returning, so the pipe is never
/// closed while the child is still writing — this needs a real early close.
///
/// Each row carries a 300-byte `pad` field (selected alongside `idx`), not
/// just the bare index: a Linux pipe's kernel buffer is 64 KiB, and
/// `io::Stdout` line-buffers, flushing (i.e. issuing a real `write(2)`) on
/// every row. With `idx` alone, 500 rows total under 2 KiB of CSV — it all
/// fits in one flush that lands before the reader below has a chance to
/// close its end, so the bug never fires. Padded, 500 rows total well over
/// 64 KiB, so later rows' flushes land after the pipe closes and hit EPIPE —
/// reproducing the real `| head` scenario this pins.
#[test]
fn broken_pipe_exits_without_panic_or_error_noise() {
    use std::io::Read;
    use std::process::{Command as PCommand, Stdio};

    let td = TempDir::new().unwrap();
    let pad = "x".repeat(300);
    for i in 0..500 {
        fs::write(
            td.path().join(format!("n{i}.md")),
            format!("---\nidx: {i}\npad: \"{pad}\"\n---\n"),
        )
        .unwrap();
    }
    let home = TempDir::new().unwrap();
    let bin = assert_cmd::cargo::cargo_bin("querymatter");
    let mut child = PCommand::new(bin)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env_remove("QUERYMATTER_TABLE_STYLE")
        .arg("-e")
        .arg("SELECT idx, pad")
        .arg("--format")
        .arg("csv")
        .arg(td.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read a little, then drop the read end to close the pipe early.
    let mut buf = [0u8; 64];
    let _ = child.stdout.take().unwrap().read(&mut buf);
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let code = out.status.code();
    assert_ne!(code, Some(101), "panicked on broken pipe");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("Broken pipe"),
        "leaked Broken pipe error to stderr"
    );
}

/// Task 4 (B4) GREEN pin for the OTHER manifestation of the same bug: the
/// plain `println!` sinks (`query list`, `config list`/`get`, `cache
/// status`) don't go through [`OutputSink`](crate) at all, so a broken pipe
/// there doesn't surface as an `io::Error` to propagate — `println!` panics
/// outright ("failed printing to stdout"), aborting with exit code 101
/// before `main`'s error handler ever runs. `query list` is exercised here
/// (the others share the exact same `println!`-to-stdout shape): 2000 saved
/// queries, each padded past a couple hundred bytes, comfortably exceed a
/// pipe's 64 KiB kernel buffer, so later rows are still being printed after
/// the early close below — reproducing the same "flush lands after the pipe
/// is gone" race as the streaming-path test above, just through `println!`
/// instead of `OutputSink::write_result`.
///
/// Written directly as `queries.toml` (bypassing `query save`, which parses
/// each SQL string and would make 2000 saves too slow) — `query list` never
/// re-parses saved SQL, only prints it, so hand-written, deliberately-inert
/// SQL text is fine here.
#[test]
fn query_list_broken_pipe_exits_without_panic() {
    use std::io::Read;
    use std::process::{Command as PCommand, Stdio};

    let home = TempDir::new().unwrap();
    let config_dir = home.path().join("querymatter");
    fs::create_dir_all(&config_dir).unwrap();
    let pad = "x".repeat(200);
    let mut queries_toml = String::new();
    for i in 0..2000 {
        queries_toml.push_str(&format!("q{i} = \"SELECT 1 -- {pad}\"\n"));
    }
    fs::write(config_dir.join("queries.toml"), queries_toml).unwrap();

    let bin = assert_cmd::cargo::cargo_bin("querymatter");
    let mut child = PCommand::new(bin)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env_remove("QUERYMATTER_TABLE_STYLE")
        .arg("query")
        .arg("list")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut buf = [0u8; 64];
    let _ = child.stdout.take().unwrap().read(&mut buf);
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let code = out.status.code();
    assert_ne!(code, Some(101), "panicked on broken pipe");
    assert!(
        !String::from_utf8_lossy(&out.stderr).contains("Broken pipe"),
        "leaked Broken pipe error to stderr"
    );
}

/// Task 5 (B5): pins `ORDER BY`/`GROUP BY ... ORDER BY`'s row/group order
/// across the decorate-sort-undecorate refactor — each row's (or group's)
/// sort key is now computed once, up front, instead of on every pairwise
/// comparison, but the output order must stay byte-identical (INV-2).
/// `g = x` has two rows (`count(*) = 2`) and `g = y` has one (`count(*) =
/// 1`), so `ORDER BY c DESC` must place `x` before `y`.
#[test]
fn order_by_and_group_order_stable_after_decorate() {
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("a.md", "---\ng: x\nn: 3\n---\n"),
        ("b.md", "---\ng: x\nn: 1\n---\n"),
        ("c.md", "---\ng: y\nn: 2\n---\n"),
    ] {
        fs::write(td.path().join(p), s).unwrap();
    }
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT g, count(*) AS c GROUP BY g ORDER BY c DESC, g ASC")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicate::str::is_match("(?s)x.*y").unwrap()); // x (c=2) before y (c=1)
}

/// Task 9 (B9, correctness): a frontmatter value nested far beyond any
/// legitimate document must be skipped with a warning, not take the whole
/// process down.
///
/// Bracket nesting (`[[[…]]]`) is *not* the reproducer: yaml-rust2 0.10 caps
/// flow-collection (`[`/`{`) nesting itself (a `u8` counter, `flow_level` in
/// its scanner) at 255 and returns a clean parse error past that — already
/// exercised by `invalid_yaml_is_invalid`'s sibling paths, no crash to
/// characterize there. The reproducer empirically found instead is a
/// **compact block sequence** — `- - - - … v` — which isn't flow syntax at
/// all, so that counter never engages; nesting it this deeply overflows
/// yaml-rust2's own recursive-descent parser before `pod_to_value` (this
/// crate's code) ever runs. On this crate's file-scanning worker threads
/// (`parallel::map_paths`, default ~2 MiB stacks) that overflow reliably
/// aborts the whole process (`fatal runtime error: stack overflow,
/// aborting`) somewhere between depth 900 and 950; depth 2000 here gives
/// comfortable margin. `ok.md` alongside it must still load.
#[test]
fn deeply_nested_frontmatter_is_skipped_not_crashed() {
    let td = TempDir::new().unwrap();
    let depth = 2000;
    fs::write(
        td.path().join("deep.md"),
        format!("---\nx:\n  {}v\n---\n", "- ".repeat(depth)),
    )
    .unwrap();
    fs::write(td.path().join("ok.md"), "---\ntitle: ok\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e")
        .arg("SELECT title")
        .arg(td.path())
        .assert()
        .success() // process did not abort/overflow
        .stdout(predicates::str::contains("ok"))
        .stderr(predicates::str::contains("deep.md"));
}

/// B9 review fix (Critical false-positive): a file with LEGITIMATE, shallow
/// frontmatter but a body containing unrelated bracket-heavy or dash-heavy
/// text must load normally — the pre-parse depth cap that stops the crash in
/// `deeply_nested_frontmatter_is_skipped_not_crashed` above must only ever
/// fire on the fenced frontmatter YAML gray_matter actually parses, never on
/// Markdown body content. `intervals.md`'s body is interval notation (`[0,1)
/// [1,2) …`, 200 unclosed `[`); `outline.md`'s is a flattened pasted outline
/// (`- - - - … v`, 200 levels) — both comfortably past the 128 cap. Before
/// the fix that scopes `check_nesting_depth` to just the fenced block, BOTH
/// were wrongly rejected as `Extract::Invalid("frontmatter nesting exceeds
/// 128 levels — skipped")` even though their frontmatter (`status: draft`)
/// is trivially shallow.
#[test]
fn body_bracket_or_dash_heavy_text_does_not_reject_legitimate_frontmatter() {
    let td = TempDir::new().unwrap();
    let intervals: String = (0..200).map(|i| format!("[{i},{}) ", i + 1)).collect();
    fs::write(
        td.path().join("intervals.md"),
        format!("---\nstatus: draft\n---\n{intervals}\n"),
    )
    .unwrap();
    let outline = "- ".repeat(200) + "v";
    fs::write(
        td.path().join("outline.md"),
        format!("---\nstatus: draft\n---\n{outline}\n"),
    )
    .unwrap();
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
        .args(["-e", "SELECT status", "--format", "json"])
        .arg(td.path())
        .assert()
        .success()
        .stderr(predicates::str::contains("nesting").not())
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(
        v.as_array().unwrap().len(),
        2,
        "both files must load, neither skipped as over-nested: {v}"
    );
}
