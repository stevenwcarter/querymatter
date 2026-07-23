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
