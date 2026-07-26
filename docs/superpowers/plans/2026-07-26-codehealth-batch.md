# code-health batch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the 12 code-health findings the user selected (B1–B10, B14, B15) in `querymatter`, one commit per finding, each landing with a test.

**Architecture:** Surgical fixes to an existing Rust CLI/REPL. Hot-path fixes hoist per-row work in the query executor; safety fixes add containment/bounds at the cache and frontmatter boundaries; a presentation-boundary sanitizer neutralizes terminal escapes; process-level SIGPIPE handling fixes broken-pipe behavior.

**Tech Stack:** Rust edition 2024, binary crate `querymatter`. Deps in play: `regex`, `gray_matter`, `csv`, `comfy-table`, `ignore`, `bincode`, `directories`. Tests: `assert_cmd` + `predicates` + `tempfile` (integration, `tests/cli.rs`) and in-crate `#[cfg(test)] mod tests` (unit, access to private fns since there is no lib target).

## Global Constraints

- Edition 2024; keep `cargo clippy --all-targets -- -D warnings` clean and `cargo fmt` applied (no pre-commit hook — run `cargo fmt` yourself before every commit).
- Build: `cargo build` · Test: `cargo test` · Lint: `cargo clippy --all-targets -- -D warnings`. All three green before each commit.
- Binary crate: no `cargo test --lib`. Integration tests go in `tests/cli.rs` through the `qm(home)` helper (isolates HOME/XDG). Unit tests go in the target file's `#[cfg(test)] mod tests`.
- **Do not modify existing tests** (one-way rule) — add new ones. Existing suites (`like_and_in`, empty-JSON/piped byte-identity W1/W2, table/vertical snapshots) must stay green.
- **Interchange byte-identity (INV-1):** CSV/JSON/TSV output is a stable contract. The B3 terminal sanitizer must not touch those paths or piped (non-tty) output.
- **Deterministic ordering (INV-2):** results are sorted/`IndexMap`, never `HashMap` iteration. B5/B10 must preserve exact order + values.
- **Each commit:** `fix(<category>): <summary> [B<n>]` AND strip that finding's block from `bughunt.md` in the same commit. `risk: high` tasks first commit `test: characterize <unit> before fix [B<n>]` (RED).

---

### Task 1 (B1): Compile LIKE regex once per query, not per row

**Category:** caching · **Risk:** low

**Files:**
- Modify: `src/query/exec.rs` (`like_matches` ~1820; call sites `eval_predicate` ~1696, `filter_records` ~417, projection `Expr::Predicate` ~1480) and/or `src/query/parse.rs` / `src/query/ast.rs` (`Predicate` LIKE variant) to carry a precompiled matcher.
- Test: `tests/cli.rs` (behavior-equivalence) + optional `#[cfg(test)]` in `exec.rs`.

**Interfaces:**
- Produces: a LIKE predicate that holds its translated `Regex` (or an enum matcher) compiled once. Later tasks touching `Predicate` (B2) must keep this field.

- [ ] **Step 1: Write the failing/using test** — add to `tests/cli.rs`:

```rust
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
```

- [ ] **Step 2: Run it** — `cargo test --test cli like_matches_are_stable_after_hoisting` — expect PASS today (this pins semantics; it guards the refactor). Confirm existing `like_and_in` tests also pass.
- [ ] **Step 3: Implement** — hoist compilation: give the LIKE `Predicate` a compiled `Regex` produced once (at parse-time lowering in `parse.rs`, mirroring how REGEXP is validated, or by pre-walking the query before the row loop and building a `pattern→Regex` map borrowed by `eval_predicate`). Remove the per-call `regex::escape`+`String::replace`+`Regex::new` from `like_matches`. Keep the exact translation (`%`→`.*`, `_`→`.`, escape everything else, anchored `^…$`, same case-sensitivity).
- [ ] **Step 4: Verify** — `cargo test` (all), `cargo clippy --all-targets -- -D warnings`, `cargo fmt`. Green.
- [ ] **Step 5: Commit + strip** — remove the B1 block from `bughunt.md`; `git commit -m "fix(caching): compile LIKE regex once per query [B1]"` (add exec.rs/parse.rs/ast.rs, tests/cli.rs, bughunt.md).

---

### Task 2 (B2): Compile REGEXP regex once per query, not per row

**Category:** caching · **Risk:** low

**Files:**
- Modify: `src/query/exec.rs` (`regexp_matches` ~1834; sites `eval_predicate` ~1704) + `src/query/parse.rs` (`lower_regexp` already validates compilation) / `ast.rs` `Predicate` REGEXP variant.
- Test: `tests/cli.rs`.

**Interfaces:**
- Consumes: the B1 pattern of a `Predicate` carrying a precompiled `Regex`.

- [ ] **Step 1: Write the test** — add to `tests/cli.rs`:

```rust
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
```

- [ ] **Step 2: Run it** — `cargo test --test cli regexp_matches_are_stable_after_hoisting` — PASS today (pins semantics).
- [ ] **Step 3: Implement** — attach the compiled `Regex` to the REGEXP `Predicate` at parse time (`lower_regexp` already builds+validates it — keep it instead of discarding), or memoize before the filter loop. Remove `Regex::new` from the per-row `regexp_matches`.
- [ ] **Step 4: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 5: Commit + strip** — remove B2 block; `git commit -m "fix(caching): compile REGEXP regex once per query [B2]"`.

---

### Task 3 (B3): Neutralize terminal ANSI/control escapes from frontmatter values

**Category:** frontend · **Risk:** high → **characterization test FIRST**

**Files:**
- Modify: `src/render.rs` (`new_table` add_row/set_header ~322, `render_vertical` ~301). Add a private `sanitize_for_terminal(&str) -> Cow<str>` (or `String`).
- Test: `#[cfg(test)] mod tests` in `src/render.rs` (unit, for the sanitizer) + `tests/cli.rs` (integration).

- [ ] **Step 1: RED — characterization test** — add a unit test in `src/render.rs`'s test module asserting the sanitizer exists and neutralizes control bytes, AND an integration test in `tests/cli.rs`:

```rust
// tests/cli.rs
#[test]
fn table_output_neutralizes_ansi_escapes_from_frontmatter() {
    let td = TempDir::new().unwrap();
    // title carries a raw ESC (0x1b) screen-clear sequence
    fs::write(td.path().join("evil.md"), "---\ntitle: \"\u{1b}[2J[H spoof\"\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
        .arg("-e").arg("SELECT title")
        .arg(td.path())
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    // The default (table) render must not emit the raw ESC byte.
    assert!(!out.contains(&0x1b), "raw ESC leaked into table output");
}
```

- [ ] **Step 2: Run it** — `cargo test --test cli table_output_neutralizes_ansi_escapes_from_frontmatter` — expect **FAIL** (raw ESC leaks today). Commit RED: `git commit -m "test: characterize terminal escape leak in table render before fix [B3]"`.
- [ ] **Step 3: Implement** — add `sanitize_for_terminal` replacing C0/C1 control bytes (ESC `0x1b`, `\r`, all control chars except `\t`) with a visible marker or U+FFFD; apply to each cell + header string **only** in the table and vertical formats. Leave CSV/JSON/TSV untouched (INV-1). If the current table path is gated on `is_terminal` for other behavior, either sanitize unconditionally in the table/vertical builders (safest — the test pipes stdout, so it must pass) or mirror the finding's gating — but the test above runs with piped stdout and must pass, so **sanitize the table/vertical human formats unconditionally**. Update any table/vertical snapshot fixtures; do **not** change interchange snapshots.
- [ ] **Step 4: GREEN + INV-1 pin** — the RED test passes. Add an INV-1 test asserting `--format json` of the same file is unchanged/valid (control char stays serde-escaped, not altered):

```rust
#[test]
fn json_output_unchanged_by_terminal_sanitizer() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("evil.md"), "---\ntitle: \"\u{1b}x\"\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e").arg("SELECT title").arg("--format").arg("json")
        .arg(td.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("\\u001b"));
}
```

- [ ] **Step 5: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 6: Commit + strip** — remove B3 block; `git commit -m "fix(frontend): neutralize terminal control escapes in table/vertical render [B3]"`.

---

### Task 4 (B4): Handle broken pipe cleanly (SIGPIPE)

**Category:** api-surface · **Risk:** high → **characterization test FIRST**

**Files:**
- Modify: `src/main.rs` (process entry — reset SIGPIPE at the top of `main`). Add `libc` to `Cargo.toml` deps IF the crate-decisions menu permits (it is a minimal, standard dep); otherwise use the two-part fallback (BrokenPipe→exit 0 in the Err arm + a stdout-writer helper).
- Test: `tests/cli.rs` (raw `std::process` pipe-close test).

- [ ] **Step 1: RED — characterization test** — add to `tests/cli.rs` (uses raw `std::process`, since `assert_cmd` buffers stdout and never closes the pipe early):

```rust
#[test]
fn broken_pipe_exits_without_panic_or_error_noise() {
    use std::io::Read;
    use std::process::{Command as PCommand, Stdio};
    let td = TempDir::new().unwrap();
    for i in 0..500 {
        fs::write(td.path().join(format!("n{i}.md")), format!("---\nidx: {i}\n---\n")).unwrap();
    }
    let home = TempDir::new().unwrap();
    let bin = assert_cmd::cargo::cargo_bin("querymatter");
    let mut child = PCommand::new(bin)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path())
        .env_remove("QUERYMATTER_TABLE_STYLE")
        .arg("-e").arg("SELECT idx").arg("--format").arg("csv")
        .arg(td.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn().unwrap();
    // Read a little, then drop the read end to close the pipe early.
    let mut buf = [0u8; 64];
    let _ = child.stdout.take().unwrap().read(&mut buf);
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let code = out.status.code();
    assert_ne!(code, Some(101), "panicked on broken pipe");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("Broken pipe"),
            "leaked Broken pipe error to stderr");
}
```

- [ ] **Step 2: Run it** — `cargo test --test cli broken_pipe_exits_without_panic_or_error_noise` — expect **FAIL** (exit 101 panic or `Broken pipe` on stderr today). Commit RED: `git commit -m "test: characterize broken-pipe panic/error before fix [B4]"`.
- [ ] **Step 3: Implement** — at the very start of `main` (before any output), reset SIGPIPE to default: `unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL); }` (add `libc = "0.2"` to `Cargo.toml`). A closed reader now terminates the process via SIGPIPE with no stderr noise and no 101 panic. (Fallback if `libc` is disallowed: in `main`'s `Err` arm, walk `err.chain()` for `io::ErrorKind::BrokenPipe` → return `ExitCode::SUCCESS` silently, and route `println!` command sinks in main.rs through a helper on a locked stdout handle that treats `BrokenPipe` as clean.)
- [ ] **Step 4: GREEN** — the RED test passes.
- [ ] **Step 5: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 6: Commit + strip** — remove B4 block; `git commit -m "fix(api-surface): reset SIGPIPE so broken-pipe exits cleanly [B4]"`.

---

### Task 5 (B5): Decorate-sort-undecorate for ORDER BY

**Category:** caching · **Risk:** low

**Files:**
- Modify: `src/query/exec.rs` (comparator ~448-458; `order_key_value` ~1883; grouped `group_order_key_value`/`compute_aggregate` ~1305-1314).
- Test: `tests/cli.rs` (ordering-equivalence).

- [ ] **Step 1: Write the test** — add to `tests/cli.rs`:

```rust
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
        .arg("-e").arg("SELECT g, count(*) AS c GROUP BY g ORDER BY c DESC, g ASC")
        .arg(td.path())
        .assert().success()
        .stdout(predicate::str::is_match("(?s)x.*y").unwrap()); // x (c=2) before y (c=1)
}
```

- [ ] **Step 2: Run it** — PASS today (pins current order — INV-2).
- [ ] **Step 3: Implement** — precompute each row's (and each group's) sort key(s) once into a `Vec` (decorate), sort on the precomputed keys, then drop them (undecorate), instead of calling `order_key_value`/`compute_aggregate` for both operands per comparison. **Do not change comparison semantics** — keep routing through `compare_values` (the non-total-order issue is the deferred Critical marker, not this task). Preserve NULL placement and tie-breaking exactly.
- [ ] **Step 4: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 5: Commit + strip** — remove B5 block; `git commit -m "fix(caching): decorate-sort-undecorate ORDER BY keys [B5]"`.

---

### Task 6 (B6): Reject non-contained cache rel_paths (poisoned-cache traversal)

**Category:** security · **Risk:** high → **characterization test FIRST**

**Files:**
- Modify: `src/cache.rs` (`records_from` ~774; `refresh_fast` verbatim arm ~697; or `load_cache_under` ~287 at decode time).
- Test: `#[cfg(test)] mod tests` in `src/cache.rs` (unit — build a `CachedDir` with a malicious rel_path directly, since crafting a bincode blob is impractical).

- [ ] **Step 1: RED — characterization test** — in `src/cache.rs` test module, construct a `CachedDir` whose files include a `rel_path` of `../../../../etc/passwd` (and one legitimate `a/b.md`), call `records_from`, assert the escaping entry produces **no** Record (or is filtered) while the legitimate nested one survives. (Match the real `CachedDir`/`CachedFile` field names when writing the test.)
- [ ] **Step 2: Run it** — `cargo test records_from` — expect **FAIL** (today the traversal entry yields a Record pointing outside the vault). Commit RED: `git commit -m "test: characterize poisoned-cache path traversal before fix [B6]"`.
- [ ] **Step 3: Implement** — in `records_from` (and the `refresh_fast` verbatim-reuse arm), skip any `CachedFile` whose `rel_path` is absolute or contains a `..`/root/`.`-escaping component, and verify `dir.join(rel_path)` still `starts_with(dir)` after lexical normalization before building a `Record`. Push a `LoadReport` warning for each skipped entry. Legitimate nested rel_paths (`a/b/c.md`) must still load (INV-4).
- [ ] **Step 4: GREEN** — RED test passes; add/confirm a positive test that `a/b/c.md` still loads.
- [ ] **Step 5: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 6: Commit + strip** — remove B6 block; `git commit -m "fix(security): enforce vault containment on cached rel_paths [B6]"`.

---

### Task 7 (B7): Surface per-file skip reasons in `querymatter init`

**Category:** observability · **Risk:** high → **characterization test FIRST**

**Files:**
- Modify: `src/main.rs` (`run_init` ~378 — iterate `report.warnings` before the summary, matching `build_session` ~974-978).
- Test: `tests/cli.rs`.

- [ ] **Step 1: RED — characterization test** — add to `tests/cli.rs`:

```rust
#[test]
fn init_reports_which_files_were_skipped() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("good.md"), "---\ntitle: ok\n---\n").unwrap();
    fs::write(td.path().join("bad.md"), "---\ntitle: [unterminated\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("init").arg(td.path())
        .assert()
        .stderr(predicate::str::contains("bad.md"));
}
```

- [ ] **Step 2: Run it** — expect **FAIL** (init prints only a count today; `bad.md` not on stderr). Adjust the bad-frontmatter content if needed so it is actually a *skip-with-warning* (invalid YAML) not a silent no-frontmatter case. Commit RED: `git commit -m "test: characterize init hiding skipped-file reasons before fix [B7]"`.
- [ ] **Step 3: Implement** — in `run_init`, before the summary line, iterate `report.warnings` and `eprintln!("querymatter: {warning}")` for each (respect the resolved quiet setting, exactly as `build_session` does).
- [ ] **Step 4: GREEN** — RED test passes.
- [ ] **Step 5: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 6: Commit + strip** — remove B7 block; `git commit -m "fix(observability): init surfaces per-file skip reasons [B7]"`.

---

### Task 8 (B8): Cap file size before whole-file read

**Category:** security · **Risk:** low

**Files:**
- Modify: `src/cache.rs` (`scan_file` ~440 — check size from `stat_file` before `fs::read_to_string`), `src/query/exec.rs` (`read_body` ~1407), and the settings/config layer for the knob (follow existing setting patterns in `src/settings.rs`/`src/config.rs`).
- Test: `tests/cli.rs`.

- [ ] **Step 1: Write the test** — add to `tests/cli.rs` (choose a small test cap via the config knob you add, so the test needn't write a giant file):

```rust
#[test]
fn oversized_file_is_skipped_with_warning() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("big.md"),
              format!("---\ntitle: big\n---\n{}", "x".repeat(20_000))).unwrap();
    fs::write(td.path().join("small.md"), "---\ntitle: small\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    write_config(home.path(), "max_file_bytes = 1000\n"); // knob added in Step 3
    qm(home.path())
        .arg("-e").arg("SELECT title").arg(td.path())
        .assert().success()
        .stdout(predicate::str::contains("small"))
        .stdout(predicate::str::contains("big").not())
        .stderr(predicate::str::contains("big.md"));
}
```

- [ ] **Step 2: Run it** — expect **FAIL** (no cap today; `big` appears). 
- [ ] **Step 3: Implement** — add a `max_file_bytes` setting (default a sane large value, e.g. 8 MiB; document it) resolved like other settings. In `scan_file`, when `stat` size exceeds the cap, skip with a `LoadReport` warning naming the file. Apply the same cap in `read_body` (bounded read or skip). Keep the default high enough not to affect normal vaults.
- [ ] **Step 4: Verify** — `cargo test`, clippy, fmt. Confirm the default (unconfigured) path is unaffected. Green.
- [ ] **Step 5: Commit + strip** — remove B8 block; `git commit -m "fix(security): cap file size before reading to avoid OOM [B8]"`.

---

### Task 9 (B9): Bound frontmatter nesting depth

**Category:** correctness · **Risk:** high → **characterization test FIRST**

**Files:**
- Modify: `src/frontmatter.rs` (`pod_to_value` ~83 — thread a depth counter); optionally defensive caps in Value walkers (`src/model.rs` ~47/111, `src/render.rs` ~328, `src/query/exec.rs` ~880).
- Test: `tests/cli.rs`.

- [ ] **Step 1: RED — characterization test** — add to `tests/cli.rs` (generate frontmatter nested beyond the cap; assert the process stays alive and skips the file with a warning):

```rust
#[test]
fn deeply_nested_frontmatter_is_skipped_not_crashed() {
    let td = TempDir::new().unwrap();
    let depth = 500;
    let nested = format!("{}v{}", "[".repeat(depth), "]".repeat(depth));
    fs::write(td.path().join("deep.md"), format!("---\nx: {nested}\n---\n")).unwrap();
    fs::write(td.path().join("ok.md"), "---\ntitle: ok\n---\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e").arg("SELECT title").arg(td.path())
        .assert()
        .success()                               // process did not abort/overflow
        .stdout(predicate::str::contains("ok"));
}
```

- [ ] **Step 2: Run it** — expect **FAIL** or abort (stack overflow / non-success) today. If yaml-rust2 overflows during parse *before* `pod_to_value`, note it and place the cap at the earliest in-crate point that keeps the process alive (e.g. reject via `Extract::Invalid` when a depth probe trips). Commit RED: `git commit -m "test: characterize unbounded frontmatter-depth recursion before fix [B9]"`.
- [ ] **Step 3: Implement** — add a depth counter to `pod_to_value` (const bound e.g. 128); beyond it, mark the record `Extract::Invalid` (skipped + warned) instead of recursing. Defensively bound the Value walkers or argue (with the test) that the parse-time cap makes deep Values unreachable.
- [ ] **Step 4: GREEN** — RED test passes; `ok.md` still returned.
- [ ] **Step 5: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 6: Commit + strip** — remove B9 block; `git commit -m "fix(correctness): bound frontmatter nesting depth [B9]"`.

---

### Task 10 (B10): Memoize file.body per row

**Category:** caching · **Risk:** low

**Files:**
- Modify: `src/query/exec.rs` (`read_body` ~1403, `resolve_col` ~1383 — cache the parsed body Value for the current record's evaluation).
- Test: `tests/cli.rs` (behavior-equivalence).

**Interfaces:**
- Consumes: composes with Task 5 (B5) so ORDER-BY-over-body reads once.

- [ ] **Step 1: Write the test** — add to `tests/cli.rs`:

```rust
#[test]
fn file_body_referenced_twice_is_consistent() {
    let td = TempDir::new().unwrap();
    fs::write(td.path().join("n.md"), "---\ntitle: t\n---\nhello TODO world\n").unwrap();
    let home = TempDir::new().unwrap();
    qm(home.path())
        .arg("-e").arg("SELECT title WHERE file.body LIKE '%TODO%'")
        .arg(td.path())
        .assert().success()
        .stdout(predicate::str::contains("t"));
}
```

- [ ] **Step 2: Run it** — PASS today (pins behavior).
- [ ] **Step 3: Implement** — memoize the body read+parse for the lifetime of a single record's evaluation (e.g. a per-record `OnceCell`/`Option<Value>` populated on first `file.body` access and reused across filter/projection/order), so a query referencing `file.body` more than once reads the file once. Preserve exact values/order (INV-2).
- [ ] **Step 4: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 5: Commit + strip** — remove B10 block; `git commit -m "fix(caching): memoize file.body per record evaluation [B10]"`.

---

### Task 11 (B14): GC orphaned cache blobs on save

**Category:** caching · **Risk:** medium

**Files:**
- Modify: `src/cache.rs` (`save_cache` ~202/236 — after writing the manifest, unlink unreferenced `*.bin` blobs).
- Test: `#[cfg(test)] mod tests` in `src/cache.rs` or `tests/cli.rs` if drivable via `cache` subcommands.

- [ ] **Step 1: Write the test** — drive a save with dir set {A,B}, then a save with {A} (B removed/renamed), assert B's blob file is gone and the manifest + A's blob still load. Prefer a `tests/cli.rs` test using `querymatter init` on a tree, then removing a subdir and re-running, then inspecting `.querymatter/` file count; fall back to an in-crate unit test calling `save_cache` twice.
- [ ] **Step 2: Run it** — expect **FAIL** (orphan blob lingers today).
- [ ] **Step 3: Implement** — after the manifest rename in `save_cache`, enumerate `.querymatter/*.bin` and `remove_file` any blob not named by a current `ManifestEntry.blob` (keep `manifest.bin`). Do it strictly **after** the manifest is durably written so a crash leaves only harmless orphans (INV-4).
- [ ] **Step 4: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 5: Commit + strip** — remove B14 block; `git commit -m "fix(caching): GC orphaned cache blobs on save [B14]"`.

---

### Task 12 (B15): Move REPL banner to stderr

**Category:** api-surface · **Risk:** low

**Files:**
- Modify: `src/repl.rs` (banner `println!` ~414 → `eprintln!`).
- Test: `tests/cli.rs` (REPL with piped stdin/stdout).

- [ ] **Step 1: Write the test** — add to `tests/cli.rs` (feed a query + exit on stdin; assert banner text is on stderr, not stdout):

```rust
#[test]
fn repl_banner_goes_to_stderr_not_stdout() {
    let td = tree();
    let home = TempDir::new().unwrap();
    let out = qm(home.path())
        .arg(td.path())
        .write_stdin("SELECT status\n.exit\n")   // match the REPL's actual exit command
        .assert()
        .get_output()
        .clone();
    let banner = "querymatter";                   // match a distinctive banner substring
    assert!(String::from_utf8_lossy(&out.stderr).contains(banner));
    assert!(!String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").contains(banner));
}
```

- [ ] **Step 2: Run it** — inspect the real banner text (repl.rs:414) and the REPL exit command; adjust the substring/`.exit` line. Expect **FAIL** (banner on stdout today).
- [ ] **Step 3: Implement** — change the banner `println!` at repl.rs:414 to `eprintln!`.
- [ ] **Step 4: Verify** — `cargo test`, clippy, fmt. Green.
- [ ] **Step 5: Commit + strip** — remove B15 block; `git commit -m "fix(api-surface): emit REPL banner on stderr [B15]"`.

---

## Milestones & final

- Run full `cargo test` after Task 5, Task 10, and Task 12 (every ~5 findings / bucket boundary). On red: bisect within the batch, revert the offender, surface the diagnosis.
- After Task 12: `cargo test` + `cargo clippy --all-targets -- -D warnings` + `cargo fmt --check` all green. Confirm `bughunt.md` still lists only B11, B12, B13 and the three decision-needed markers. Report status. No summary commit.
```
