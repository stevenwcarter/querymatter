# `.querymatterignore` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax. Implementers are `rust-developer` agents: clippy-clean (`cargo clippy --all-targets --all-features -- -D warnings`) and rustfmt-clean.

**Goal:** Add a gitignore-style `.querymatterignore` file that querymatter always honors — auto-discovered in cwd, augmentable with repeatable `--ignore-file`, disableable with `--no-ignore-file`.

**Architecture:** `discover` applies each resolved ignore file via `ignore::WalkBuilder::add_ignore` (verified to work under our `standard_filters(false)` toggles). `Cli::ignore_files()` resolves the ordered list (cwd file + `--ignore-file`s) at the CLI/anyhow boundary — the single seam a future `.querymatter` vault extends. `main` sets the list on `WalkOpts` before load.

**Tech Stack:** Rust edition 2024; `ignore` (already a dep — `WalkBuilder::add_ignore`), `clap`, `anyhow`. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-07-23-querymatterignore-design.md`

## Global Constraints

- **Edition 2024**; crate/binary name `querymatter`. clippy-clean (`cargo clippy --all-targets --all-features -- -D warnings`) and rustfmt-clean at every commit.
- **Run `cargo fmt --all` yourself** before committing — this repo has no pre-commit format hook.
- **No new dependencies** — `ignore` already provides `add_ignore`.
- **`.querymatterignore` is always honored** regardless of `--respect-gitignore` (which stays specific to `.gitignore`). This is the load-bearing invariant.
- Bin-only crate: use `cargo test <name>` (no `--lib`).
- Verified fact (probe): `WalkBuilder::add_ignore(path)` DOES apply an ignore file under discover's config (`standard_filters(false)`, `git_ignore(false)`, `ignore(false)`, `require_git(false)`, `hidden(true)`) — a file listed in the added ignore file is excluded from the walk. Use `add_ignore`; no `GitignoreBuilder` fallback is needed.
- Every commit message ends with:
  `Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6`

## File Structure

```
src/discover.rs   # WalkOpts.ignore_files + apply via add_ignore (Task 1)
src/cli.rs        # --ignore-file / --no-ignore-file + ignore_files() resolver (Task 2)
                  #   (+ walk_opts() gains ignore_files: Vec::new() in Task 1)
src/main.rs       # set opts.ignore_files = cli.ignore_files()? before load (Task 3)
tests/cli.rs      # integration: ignore file excludes; missing --ignore-file errors (Task 3)
README.md         # .querymatterignore section + flags (Task 3)
```

---

### Task 1: `discover` applies ignore files

**Files:**
- Modify: `src/discover.rs` (WalkOpts + discover + tests), `src/cli.rs` (one line in `walk_opts`)
- Test: inline in `src/discover.rs`

**Interfaces:**
- Consumes: nothing new.
- Produces: `WalkOpts` gains `pub ignore_files: Vec<std::path::PathBuf>` (Default `Vec::new()`); `discover` applies each via `WalkBuilder::add_ignore`.

- [ ] **Step 1: Write the failing tests** (add to `src/discover.rs`'s `mod tests`)

```rust
    #[test]
    fn ignore_file_excludes_matches() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".querymatterignore", "drafts/\n");
        touch(td.path(), "keep.md", "x");
        touch(td.path(), "drafts/d.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".querymatterignore")],
            ..Default::default()
        };
        let names: Vec<_> = discover(td.path(), &opts)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["keep.md"]); // drafts/d.md excluded
    }

    #[test]
    fn ignore_file_negation_reincludes() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "*.draft.md\n!keep.draft.md\n");
        touch(td.path(), "a.draft.md", "x");
        touch(td.path(), "keep.draft.md", "x");
        touch(td.path(), "b.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".qmi")],
            ..Default::default()
        };
        let names: Vec<_> = discover(td.path(), &opts)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["b.md", "keep.draft.md"]); // a.draft.md excluded, keep re-included
    }

    #[test]
    fn ignore_file_applies_even_when_gitignore_off() {
        // Load-bearing: always-on. respect_gitignore is false (default), yet the
        // ignore file still excludes — this is what makes it NOT just .gitignore.
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "secret.md\n");
        touch(td.path(), "secret.md", "x");
        touch(td.path(), "public.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".qmi")],
            respect_gitignore: false,
            ..Default::default()
        };
        let names: Vec<_> = discover(td.path(), &opts)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["public.md"]);
    }

    #[test]
    fn ignore_file_nonanchored_pattern_matches_at_depth() {
        // Characterization: a non-anchored gitignore pattern matches at ANY depth.
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "templates/\n");
        touch(td.path(), "a.md", "x");
        touch(td.path(), "sub/templates/t.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".qmi")],
            ..Default::default()
        };
        let names: Vec<_> = discover(td.path(), &opts)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["a.md"]); // sub/templates/t.md excluded at depth
    }

    #[test]
    fn empty_ignore_file_is_noop() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "# only a comment\n\n");
        touch(td.path(), "a.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".qmi")],
            ..Default::default()
        };
        assert_eq!(discover(td.path(), &opts).len(), 1);
    }
```

- [ ] **Step 2: Run to verify fail**

Run: `cargo test ignore_file` and `cargo test empty_ignore_file` → FAIL (WalkOpts has no `ignore_files` field; won't compile).

- [ ] **Step 3: Add the field + apply logic**

In `WalkOpts` add:
```rust
    /// Gitignore-style ignore files to apply (earliest first), always honored
    /// regardless of `respect_gitignore`. Resolved by `Cli::ignore_files`.
    pub ignore_files: Vec<std::path::PathBuf>,
```
In `impl Default for WalkOpts`, add `ignore_files: Vec::new(),`.

In `discover`, after the toggle chain (`.require_git(false);`) and before `.build()`, apply each ignore file:
```rust
    for ignore_path in &opts.ignore_files {
        // add_ignore applies a gitignore-style file as an explicit source,
        // honored even with standard_filters(false)/ignore(false) (verified).
        // Paths are pre-validated at the CLI boundary, so a load error here is
        // not expected; ignore the returned Option<Error> rather than aborting
        // discovery of everything else.
        walker.add_ignore(ignore_path);
    }
```
(`walker` is already `let mut walker`.)

In `src/cli.rs` `walk_opts()`, add `ignore_files: Vec::new(),` to the `WalkOpts { .. }` literal so the crate still compiles (Task 2 fills in the real value via `main`; `walk_opts` stays flag-only).

- [ ] **Step 4: Run to verify pass** — `cargo test ignore_file`, `cargo test empty_ignore_file`, then full `cargo test` (existing discover tests must still pass) → PASS. `cargo fmt --all` + `cargo clippy --all-targets --all-features -- -D warnings` clean.

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(discover): apply .querymatterignore-style ignore files

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 2: `cli` flags + `ignore_files()` resolver

**Files:**
- Modify: `src/cli.rs` (flags, resolver, validation, tests)
- Test: inline in `src/cli.rs`

**Interfaces:**
- Consumes: `WalkOpts.ignore_files` (Task 1) is set by `main` (Task 3), not here.
- Produces:
  - `Cli` gains `pub ignore_file: Vec<PathBuf>` (`#[arg(long)]`, repeatable) and `pub no_ignore_file: bool` (`#[arg(long)]`).
  - `pub fn ignore_files(&self) -> anyhow::Result<Vec<PathBuf>>` — resolves cwd file + `--ignore-file`s; delegates to a testable `resolve_ignore_files(&self, cwd: &Path)`.

- [ ] **Step 1: Write the failing tests** (add to `src/cli.rs`'s `mod tests`)

```rust
    use std::fs;

    #[test]
    fn resolves_cwd_ignore_file_when_present() {
        let td = tempdir().unwrap();
        fs::write(td.path().join(".querymatterignore"), "x\n").unwrap();
        let cli = Cli::parse_from(["querymatter"]);
        assert_eq!(
            cli.resolve_ignore_files(td.path()).unwrap(),
            vec![td.path().join(".querymatterignore")]
        );
    }

    #[test]
    fn no_ignore_file_skips_cwd() {
        let td = tempdir().unwrap();
        fs::write(td.path().join(".querymatterignore"), "x\n").unwrap();
        let cli = Cli::parse_from(["querymatter", "--no-ignore-file"]);
        assert!(cli.resolve_ignore_files(td.path()).unwrap().is_empty());
    }

    #[test]
    fn appends_ignore_file_flags_in_order() {
        let td = tempdir().unwrap();
        let a = td.path().join("a.ignore");
        let b = td.path().join("b.ignore");
        fs::write(&a, "x\n").unwrap();
        fs::write(&b, "x\n").unwrap();
        let cli = Cli::parse_from([
            "querymatter",
            "--no-ignore-file",
            "--ignore-file",
            a.to_str().unwrap(),
            "--ignore-file",
            b.to_str().unwrap(),
        ]);
        assert_eq!(cli.resolve_ignore_files(td.path()).unwrap(), vec![a, b]);
    }

    #[test]
    fn cwd_file_then_ignore_file_flags() {
        let td = tempdir().unwrap();
        fs::write(td.path().join(".querymatterignore"), "x\n").unwrap();
        let extra = td.path().join("extra.ignore");
        fs::write(&extra, "y\n").unwrap();
        let cli = Cli::parse_from(["querymatter", "--ignore-file", extra.to_str().unwrap()]);
        assert_eq!(
            cli.resolve_ignore_files(td.path()).unwrap(),
            vec![td.path().join(".querymatterignore"), extra]
        );
    }

    #[test]
    fn missing_ignore_file_errors() {
        let td = tempdir().unwrap();
        let missing = td.path().join("nope.ignore");
        let cli = Cli::parse_from(["querymatter", "--ignore-file", missing.to_str().unwrap()]);
        assert!(cli.resolve_ignore_files(td.path()).is_err());
    }

    #[test]
    fn absent_cwd_file_is_not_error() {
        let td = tempdir().unwrap(); // no .querymatterignore
        let cli = Cli::parse_from(["querymatter"]);
        assert!(cli.resolve_ignore_files(td.path()).unwrap().is_empty());
    }
```
(`tempdir`, `Cli`, `Parser` are already imported in the test module; add `use std::fs;`.)

- [ ] **Step 2: Run to verify fail** — `cargo test ignore_file` (the cli ones) → FAIL (fields/methods missing; won't compile).

- [ ] **Step 3: Add the flags + resolver**

Add fields to `Cli` (after `exclude`):
```rust
    /// Apply a gitignore-style ignore file. Repeatable; applied in order after
    /// the auto-discovered cwd `.querymatterignore`.
    #[arg(long)]
    pub ignore_file: Vec<PathBuf>,

    /// Do not auto-discover a `.querymatterignore` in the current directory.
    /// Explicit `--ignore-file`s still apply.
    #[arg(long)]
    pub no_ignore_file: bool,
```
Add `use std::path::Path;` alongside the existing `PathBuf` import. Add the resolver in `impl Cli`:
```rust
    /// Ordered list of gitignore-style ignore files to apply, earliest first:
    /// the cwd `.querymatterignore` (unless `--no-ignore-file`) followed by each
    /// `--ignore-file` in order. This is the single seam a future `.querymatter`
    /// vault extends (prepending the vault-parent file).
    pub fn ignore_files(&self) -> anyhow::Result<Vec<PathBuf>> {
        let cwd = env::current_dir().context("failed to determine the current directory")?;
        self.resolve_ignore_files(&cwd)
    }

    fn resolve_ignore_files(&self, cwd: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        if !self.no_ignore_file {
            let candidate = cwd.join(".querymatterignore");
            if candidate.is_file() {
                files.push(candidate);
            }
        }
        for f in &self.ignore_file {
            anyhow::ensure!(f.is_file(), "cannot read --ignore-file {}", f.display());
            files.push(f.clone());
        }
        Ok(files)
    }
```

- [ ] **Step 4: Run to verify pass** — `cargo test ignore_file` (cli) + full `cargo test` → PASS. `cargo fmt --all` + clippy clean.

- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat(cli): --ignore-file / --no-ignore-file and ignore-file resolution

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 3: wire into `main`, integration tests, README

**Files:**
- Modify: `src/main.rs`, `tests/cli.rs`, `README.md`
- Test: `tests/cli.rs`

**Interfaces:**
- Consumes: `Cli::ignore_files()` (Task 2), `WalkOpts.ignore_files` (Task 1).

- [ ] **Step 1: Wire `main`**

Read `src/main.rs`. Where it currently builds the walk options and loads the store (it calls `cli.walk_opts()` and `cli.resolved_roots()?`), set the resolved ignore files onto the opts BEFORE `InMemoryStore::load`:
```rust
    let mut walk_opts = cli.walk_opts();
    walk_opts.ignore_files = cli.ignore_files()?;
```
Use `walk_opts` where the code previously passed `cli.walk_opts()` into `InMemoryStore::load`. (`cli.ignore_files()?` propagates a missing-`--ignore-file` error to the anyhow/`main` boundary → stderr, non-zero exit — never stdout.)

- [ ] **Step 2: Write the failing integration tests** (append to `tests/cli.rs`; reuse the file's existing `assert_cmd`/`tempfile` helpers — read the file first to match its `tree()`/write helper style)

```rust
#[test]
fn querymatterignore_in_cwd_excludes_matches() {
    let td = tempfile::TempDir::new().unwrap();
    let w = |rel: &str, body: &str| {
        let p = td.path().join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    };
    w("plans/a.md", "---\nstatus: draft\n---\n");
    w("templates/t.md", "---\nstatus: draft\n---\n");
    w(".querymatterignore", "templates/\n");

    // Run with cwd = td so the cwd .querymatterignore is auto-discovered; scan ".".
    let out = assert_cmd::Command::cargo_bin("querymatter")
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
fn missing_ignore_file_flag_exits_nonzero() {
    let td = tempfile::TempDir::new().unwrap();
    std::fs::write(td.path().join("a.md"), "---\nstatus: draft\n---\n").unwrap();
    assert_cmd::Command::cargo_bin("querymatter")
        .unwrap()
        .args(["--ignore-file", "definitely-nonexistent.ignore", "-e", "SELECT count(*) AS n"])
        .arg(td.path())
        .assert()
        .failure();
}
```

- [ ] **Step 3: Run to verify fail then pass** — `cargo test --test cli` → the two new tests fail before Step 1's wiring is complete / pass after. Adjust the `count` assertion to the real output if the fixture differs (run once, read, lock). No production logic beyond Step 1 should be needed; if a test reveals a real bug, STOP and report it.

- [ ] **Step 4: README**

Add a `## Ignoring files (`.querymatterignore`)` section to `README.md`:
- It uses **gitignore syntax** (show a small example with a `!` negation) and is **always honored** when present (unlike `.gitignore`, which needs `--respect-gitignore`).
- Auto-discovered as `.querymatterignore` in the **current directory** (a future `.querymatter` vault will also look in the vault's parent — see `TODO.md`).
- `--ignore-file <PATH>` (repeatable) applies additional ignore files, in order, after the cwd file.
- `--no-ignore-file` skips the cwd auto-discovery (explicit `--ignore-file`s still apply).
- It composes with `--exclude <glob>` (ad-hoc globs) and `--respect-gitignore`.
Also add `--ignore-file` and `--no-ignore-file` to the README's flags list.

- [ ] **Step 5: Full verification** — `cargo test`, `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings` all clean.

- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "feat: wire .querymatterignore into main; integration tests; README

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

## Self-Review

**Spec coverage:**
- §2 flags (`--ignore-file` repeatable, `--no-ignore-file`) → Task 2. ✅
- §3 `ignore_files()` resolver (cwd file unless `--no-ignore-file`; append `--ignore-file`s in order; missing errors; absent cwd not an error) → Task 2 tests. ✅
- §4 `WalkOpts.ignore_files` + `add_ignore` application (verified via probe) → Task 1. ✅
- §5 always-on regardless of `respect_gitignore` → Task 1 `ignore_file_applies_even_when_gitignore_off`. ✅ Negation → `ignore_file_negation_reincludes`. ✅
- §6 `main` sets `opts.ignore_files = cli.ignore_files()?` → Task 3 Step 1. ✅
- §7 vault seam = `Cli::ignore_files` (documented in code) → Task 2. ✅
- §8.1 anchoring characterization (non-anchored matches at depth) → Task 1 `ignore_file_nonanchored_pattern_matches_at_depth`. ✅ §8.2 absent cwd not an error → Task 2. §8.4 empty file no-op → Task 1. ✅
- §9 invariants: existing discover tests still pass (run full suite each task); always-on test; missing-`--ignore-file` → stderr non-zero (Task 3 integration). ✅
- §10 testing → unit (Tasks 1-2) + integration (Task 3). ✅
- §11 README → Task 3. ✅

**Placeholder scan:** the Task 3 "adjust the count assertion to the real output" is standard fixture-locking, with the full test body given — not a vague placeholder. No TBD.

**Type consistency:** `WalkOpts.ignore_files: Vec<PathBuf>` (Task 1) is set in `main` from `Cli::ignore_files() -> anyhow::Result<Vec<PathBuf>>` (Task 2), consumed by `discover`'s `add_ignore` loop (Task 1). `resolve_ignore_files(&self, cwd: &Path)` is the tested private helper. Names line up across tasks.
