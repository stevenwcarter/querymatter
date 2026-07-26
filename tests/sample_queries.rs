//! Pins docs/sample-queries.sql against a generated 1k tree.
//!
//! This is the load-bearing test for the committed sample queries: it fails
//! if the generator's data drifts, if the DSL's behavior drifts, or if the
//! .sql file stops parsing — the queries can never silently rot.

use std::path::Path;

use assert_cmd::Command;

/// Points HOME and XDG_CONFIG_HOME at `home` and strips
/// `QUERYMATTER_TABLE_STYLE`, so the pinned snapshot reflects `querymatter`'s
/// documented defaults (ascii table borders) rather than whatever
/// `~/.config/querymatter/config.toml` or shell environment the machine
/// running the test happens to have — see the same isolation in
/// `tests/cli.rs`'s `with_config_home`.
fn isolated<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
    cmd.env("HOME", home)
        .env("XDG_CONFIG_HOME", home)
        .env_remove("QUERYMATTER_TABLE_STYLE")
}

#[test]
fn sample_queries_run_clean_and_match_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    Command::cargo_bin("querymatter-samples")
        .unwrap()
        .args(["--scale", "1k"])
        .arg(dir.path())
        .assert()
        .success();

    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/sample-queries.sql"
    ))
    .unwrap();

    let home = tempfile::tempdir().unwrap();
    let assert = isolated(
        Command::cargo_bin("querymatter").unwrap().arg(dir.path()),
        home.path(),
    )
    .write_stdin(sql)
    .assert()
    .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    insta::assert_snapshot!("sample_queries_output", stdout);
}
