# Scripting & Output Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Four scripting/reuse features — `--exit-code`, `--output`/`.output`, saved queries (`query` subcommand + `queries.toml` + REPL `.query`), and `explain <path>`.

**Architecture:** `main` returns `ExitCode` and maps a row-count/error to grep-style codes under `--exit-code`; an output sink redirects the rendered result stream to a file; a new `src/queries.rs` (mirroring `config.rs`) backs saved queries surfaced via a `query` subcommand and REPL `.query`; `explain` ground-truths against `discover()` and attributes the excluding layer. The query engine and default rendering are untouched.

**Tech Stack:** Rust edition 2024, `clap` derive, `toml`, `serde`, `directories`, `ignore` (via `discover`), existing `write_atomic`/`ProjectDirs`, `insta`, `assert_cmd`.

**Spec:** `docs/superpowers/specs/2026-07-24-scripting-design.md`

## Global Constraints

- Edition 2024; `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean, no `#[allow]`.
- **Default render output byte-identical:** `git diff main -- src/snapshots/` empty. Printed output is byte-identical with and without `--exit-code` (the flag changes only the exit status).
- Binary-only crate: `cargo test <filter>`, never `--lib`. No pre-commit hook — **run `cargo fmt --all` (not just --check) then `cargo clippy --all-targets -- -D warnings` yourself before every commit; both clean, no `#[allow]`.**
- **stdout carries data; stderr carries diagnostics.** `--output` redirects the data stream only; the row-count line and `.output`/`query`/`explain` confirmations stay on stderr (except `query get`/`query list`/`explain` verdicts, whose data IS their stdout).
- `queries.toml` is a SEPARATE file from `config.toml`, both under `ProjectDirs`.
- Every test isolates `HOME`+`XDG_CONFIG_HOME` (and `XDG_STATE_HOME`+`XDG_DATA_HOME` if a REPL path runs) to a temp dir. **Do NOT run the interactive REPL through a PTY** (it writes the real history file); test REPL logic via pure helpers.
- Seams: `main.rs` — `main` (39), `run_query` (250), `run_statements` (346), `read_stdin` (355), the `Command` dispatch (50-75); `run_statements` renders each statement and can use `render_statement_counted` for counts. `cli.rs` — `Command` enum (162), `Cli` struct. `config.rs` — `config_path`/`ProjectDirs` (129), `save_to`/`write_atomic` (172). `discover.rs` — `WalkOpts` (15), `discover(root, opts)` (48). `repl.rs` — `DotCommand`, `parse_dot`, `dispatch_dot`, the completer's `complete_candidates`/`DOT_COMMAND_NAMES`, the `Line::Statement` arm.

---

### Task 1: `--exit-code` (W2)

**Files:** Modify `src/main.rs`, `src/cli.rs`, `tests/cli.rs`.

**Interfaces:**
- Produces: `Cli::exit_code: bool` (`--exit-code`); `main() -> std::process::ExitCode`; `run_statements` (or `run_query`) returns the total row count so the exit code can be derived.

- [ ] **Step 1: Failing integration tests** in `tests/cli.rs`:

```rust
#[test]
fn exit_code_zero_when_rows_match() {
    let td = tree();
    let mut c = Command::cargo_bin("querymatter").unwrap();
    qm_env(&mut c) // isolates HOME/XDG dirs; see existing helper
        .args(["-e", "SELECT status WHERE prd = '010'", "--exit-code"]).arg(td.path())
        .assert().code(0);
}
#[test]
fn exit_code_one_when_no_rows() {
    let td = tree();
    let mut c = Command::cargo_bin("querymatter").unwrap();
    qm_env(&mut c)
        .args(["-e", "SELECT status WHERE prd = 'nope'", "--exit-code"]).arg(td.path())
        .assert().code(1);
}
#[test]
fn exit_code_two_on_error() {
    let td = tree();
    let mut c = Command::cargo_bin("querymatter").unwrap();
    qm_env(&mut c)
        .args(["-e", "SELECT (", "--exit-code"]).arg(td.path())
        .assert().code(2);
}
#[test]
fn without_exit_code_zero_rows_still_exits_zero() {
    let td = tree();
    let mut c = Command::cargo_bin("querymatter").unwrap();
    qm_env(&mut c)
        .args(["-e", "SELECT status WHERE prd = 'nope'"]).arg(td.path())
        .assert().code(0);
}
```

(Use the existing config-home isolation helper in `tests/cli.rs`; if it's named differently than `qm_env`, use the real name.)

- [ ] **Step 2:** Run — they fail (no `--exit-code`, `main` returns `Result`).

- [ ] **Step 3:** Add `Cli::exit_code: bool` (`#[arg(long)]`). Change `main` to return `std::process::ExitCode`. `run_query`/`run_statements` compute the total row count (sum `render_statement_counted` counts). Map: when `cli.exit_code`, return `ExitCode::SUCCESS` if total > 0 else `ExitCode::from(1)`; on an `Err` from the query pipeline, print it to stderr (`eprintln!("querymatter: {err:#}")`) and return `ExitCode::from(2)`. Without `--exit-code`, preserve today's behavior: success → `ExitCode::SUCCESS`, and an error prints + returns `ExitCode::from(1)` (or keep the anyhow propagation for the non-exit-code path — but since `main` now returns `ExitCode`, catch the error uniformly and choose 1 vs 2 by the flag). The `init`/`config`/`completions` arms return `ExitCode::SUCCESS`/error-mapped as before.

- [ ] **Step 4:** Run tests — pass. **Step 5:** fmt+clippy+snapshot-guard. **Step 6:** commit `feat(cli): --exit-code for grep-style scripting`.

---

### Task 2: `--output` and REPL `.output` (W3)

**Files:** Modify `src/main.rs`, `src/cli.rs`, `src/repl.rs`, `tests/cli.rs`.

**Interfaces:**
- Produces: `Cli::output: Option<PathBuf>` (`--output`); a small `OutputSink` (or equivalent) with a testable write/reset; REPL `DotCommand::Output(Option<String>)`.

- [ ] **Step 1: Failing tests** — unit-test the sink logic and an integration test:

```rust
// tests/cli.rs
#[test]
fn output_flag_writes_file_and_stdout_is_empty() {
    let td = tree();
    let out = td.path().join("res.txt");
    let mut c = Command::cargo_bin("querymatter").unwrap();
    qm_env(&mut c)
        .args(["-e", "SELECT status WHERE prd = '010'", "--format", "csv",
               "--output", out.to_str().unwrap()]).arg(td.path())
        .assert().success().stdout(predicates::str::is_empty());
    let body = std::fs::read_to_string(&out).unwrap();
    assert!(body.contains("status"));
}
```

- [ ] **Steps 2–4:** Add `Cli::output: Option<PathBuf>`. In the one-shot/batch path, when `--output` is set, write each statement's rendered block (+newline) to the file (truncate on first write, then append within the run) instead of `println!`; stdout stays empty. Keep it a small helper — an `OutputSink { Stdout, File(File) }` or collect blocks and write once. For the REPL: add `DotCommand::Output(Option<String>)`; `parse_dot(".output x")` → `Output(Some("x"))`, `.output`/`.output stdout` → `Output(None)`. In `repl::run`, hold a sink; a `Line::Statement`'s rendered result writes to the sink (append within session); print a stderr confirmation on `.output` change. The row-count line and errors stay on stderr regardless. Add `.output` to `print_help` and `DOT_COMMAND_NAMES`.

- [ ] **Step 5:** commit `feat(io): --output flag and REPL .output for file redirection`.

---

### Task 3: Saved queries — `queries.rs` + `query` subcommand + `.query` (W15)

**Files:** Create `src/queries.rs`; modify `src/cli.rs`, `src/main.rs`, `src/repl.rs`, `tests/cli.rs`.

**Interfaces:**
- Produces: `queries::{queries_path, load, load_from, save_to, set, remove, get}` over a `BTreeMap<String,String>`-backed store; `Command::Query(QueryArgs)` with `QueryAction { Save{name,sql}, List, Get{name}, Run{name}, Delete{name} }`; REPL `DotCommand::Query(QueryCmd)` for `run`/`list`.

- [ ] **Step 1: Failing tests** for `queries.rs` (mirror `config.rs`'s test style):

```rust
    #[test]
    fn round_trips() {
        let td = tempdir().unwrap();
        let p = td.path().join("queries.toml");
        let mut q = Queries::default();
        set(&mut q, "stale", "SELECT file.name WHERE status='draft'").unwrap();
        save_to(&p, &q).unwrap();
        assert_eq!(load_from(&p).unwrap(), q);
    }
    #[test]
    fn rejects_bad_name() {
        let mut q = Queries::default();
        assert!(set(&mut q, "has space", "SELECT 1").is_err());
        assert!(set(&mut q, "ok-name_1", "SELECT 1").is_ok());
    }
    #[test]
    fn missing_file_is_empty_malformed_errors() {
        let td = tempdir().unwrap();
        assert_eq!(load_from(&td.path().join("nope.toml")).unwrap(), Queries::default());
        let p = td.path().join("q.toml"); std::fs::write(&p, "= = broken").unwrap();
        assert!(load_from(&p).unwrap_err().to_string().contains("q.toml"));
    }
```

- [ ] **Steps 2–4:** Create `src/queries.rs`: a `Queries` newtype over `BTreeMap<String,String>` (serde flatten or a field), `queries_path()` = `<config_dir>/querymatter/queries.toml` via `ProjectDirs::from("","","querymatter")`, `load`/`load_from` (missing → default, malformed → error naming the path), `save`/`save_to` (via `write_atomic`), `set` (validates the name `^[A-Za-z0-9_-]+$`), `remove`, `get`. Add `mod queries;` to `main.rs`.

  Add `Command::Query(QueryArgs)` + `QueryAction` to `cli.rs`. Dispatch in `main`: `Save` validates the SQL parses (`query::parse`) then `set`+`save`, confirm on stderr; `List` prints `NAME\tSQL` per line to stdout; `Get` prints the SQL to stdout (error if absent); `Delete` removes (absent → reported, not error); `Run` resolves NAME→SQL (error if absent) then runs it through the SAME path `run_query` uses for `-e` (feed the resolved SQL as the query text; honors `--format`/`--output`/`--exit-code`/walk flags). Add integration tests: save→run matches `-e`, unknown run errors, list/get output, bad name rejected, malformed-parse save rejected — all under isolated config home.

  REPL: `DotCommand::Query` for `.query run <name>` and `.query list`; `.query run` resolves and runs via `render_statement`/`render_statement_counted` in-session (honor the output sink); unknown name → stderr. Add `.query` to `print_help`/`DOT_COMMAND_NAMES`, and extend the completer so `.query run ` completes saved-query names (`queries::load()` keys).

- [ ] **Step 5:** commit `feat(query): saved named queries (query subcommand, .query, queries.toml)`.

---

### Task 4: `explain <path>` (W21)

**Files:** Modify `src/cli.rs`, `src/main.rs`, `src/discover.rs`; optionally `src/repl.rs`; `tests/cli.rs`.

**Interfaces:**
- Produces: `Command::Explain(ExplainArgs { path: PathBuf })`; a `discover`-side attribution helper, e.g. `discover::explain(root, path, opts) -> ExplainVerdict` where `ExplainVerdict` is `Included` or `Excluded(reason: String)`.

- [ ] **Step 1: Failing tests** in `tests/cli.rs`:

```rust
#[test]
fn explain_included_and_excluded() {
    let td = TempDir::new().unwrap();
    std::fs::create_dir_all(td.path().join(".draft")).unwrap();
    std::fs::write(td.path().join("keep.md"), "---\nstatus: x\n---\n").unwrap();
    std::fs::write(td.path().join("todo.txt"), "x").unwrap();
    std::fs::write(td.path().join(".draft/h.md"), "---\nstatus: y\n---\n").unwrap();
    let run = |args: &[&str]| {
        let mut c = Command::cargo_bin("querymatter").unwrap();
        qm_env(&mut c).arg("explain").args(args).arg(td.path()).assert().success()
            .get_output().stdout.clone()
    };
    // NOTE: adapt to the real explain arg order (explain <path> [DIRS])
}
```

Replace with concrete assertions once the arg shape is set: `explain keep.md` → `included`; `explain todo.txt` → `excluded: extension`; `explain .draft/h.md` → `excluded: hidden`.

- [ ] **Steps 2–4:** Add `Command::Explain(ExplainArgs)` (a positional `path`, and it resolves the same walk flags as query mode via `Settings`/`WalkOpts`). Implement `discover::explain(root, target, opts)`: (1) verdict = does `discover(root, opts)` contain the canonicalized `target`? (2) if excluded, attribute the reason by testing layers in discovery order — extension not in `opts.exts`; a hidden path component when `!opts.hidden`; a matching `opts.excludes` glob; a match in an applied ignore file (build the same ignore matchers `discover` uses, or re-run discovery with each layer relaxed to find which one flips the verdict — the "relax one layer" approach is the most faithful attribution). Report the first excluding layer; if none isolates it, "excluded by the ignore rules". Print the verdict to stdout. A nonexistent/outside-root path → clean error. Optionally add REPL `.explain <path>` (state in the report whether you did).

- [ ] **Step 5:** commit `feat(cli): explain <path> to diagnose discovery/ignore exclusion`.

---

### Task 5: Docs, final review, finish branch

- [ ] **Step 1:** Update `README.md`: `--exit-code` (the 0/1/2 contract + multi-statement rule) in the flags table; `--output`/`.output`; the `query` subcommand + `queries.toml` location + `.query run/list`; `explain <path>`. Add `.output`/`.query` to the dot-commands table.
- [ ] **Step 2:** Full verification: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`; `git diff main -- src/snapshots/` empty.
- [ ] **Step 3:** Dispatch the final whole-branch reviewer, apply any pre-merge fixes, then finish the branch per `superpowers:finishing-a-development-branch` (merge to local `main`).
