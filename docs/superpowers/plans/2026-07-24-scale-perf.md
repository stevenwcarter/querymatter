# Scale & Performance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Three performance-only improvements — hashed `GROUP BY` bucketing (W6), a parallel file scan (W16), and projection push-down for narrow one-shot queries (W17) — with query results and default output byte-identical.

**Architecture:** W6 replaces the linear group lookup in `exec::group_rows` with a hash-keyed one; W16 parallelizes the per-file read/parse in the scan path and re-sorts by path for determinism; W17 threads the query's `referenced_fields()` into store materialization so only referenced field values are cloned, while the store keeps the full schema for W12 validation.

**Tech Stack:** Rust edition 2024, existing `ignore`/`Value`/`Record`/`RecordStore`/`Query::referenced_fields`, `std::thread`+`mpsc` (no new dependency), `insta`, `assert_cmd`.

**Spec:** `docs/superpowers/specs/2026-07-24-scale-perf-design.md`

## Global Constraints

- Edition 2024; `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean, no `#[allow]`.
- **All three items are performance-only: query results, row order, warnings order, column validation, and default rendered output are byte-identical.** `git diff main -- src/snapshots/` empty; the entire existing test suite passes unchanged.
- **No new dependency** — use `std::thread` + `std::sync::mpsc` or `ignore::WalkParallel` for W16. If a threadpool crate seems needed, STOP and flag it.
- Binary-only crate: `cargo test <filter>`, never `--lib`. No pre-commit hook — **actually run `cargo fmt --all`** then `cargo clippy --all-targets -- -D warnings` before every commit; both clean, no `#[allow]`.
- If you run the binary, set HOME/XDG_CONFIG_HOME/XDG_STATE_HOME/XDG_DATA_HOME to a temp dir; do NOT run the interactive REPL through a PTY.
- Seams: `exec.rs` — `group_rows` (~372, `groups.iter_mut().find(|g| g.key == key)` at ~385), `Group` (~361), `execute_grouped` (~249, sorts groups by key). `discover.rs` — `discover(root, opts) -> Vec<PathBuf>` (~49, `WalkBuilder...build()`). `store.rs` — `scan_root(root, opts)` (~326, `for path in discover(...)` → `cache::scan_file` → `Record::new(root,&path,file.fields)`), `InMemoryStore::load` (~88), `slices_from_cached` (~233). `cache.rs` — `scan_file(dir, path) -> ScanResult` (~368, pure), `refresh_per_file` (~419), `refresh_fast` (~516), `records_from(root, dirs)` (~608, clones `file.fields`). `ast.rs` — `Query::referenced_fields() -> BTreeSet<String>` (~283). `exec.rs` — `execute(q, records)` (~75) validates via `referenced_fields` against schema.

---

### Task 1: Hashed GROUP BY bucketing (W6)

**Files:** Modify `src/query/exec.rs`.

**Interfaces:** internal only — `group_rows`'s implementation changes; its signature and output are identical.

- [ ] **Step 1: Failing test** (collision-free key):

```rust
    #[test]
    fn group_by_key_does_not_collide_ambiguous_concatenations() {
        // Two records whose grouping-column values are ("a","b") and ("ab","")
        // must form TWO distinct groups, not one — a naive join-on-empty-sep key
        // would collide them.
        // Build records with two group columns c1,c2:
        //   r1: c1="a",  c2="b"
        //   r2: c1="ab", c2=""      (empty string, present)
        // SELECT c1, c2, count(*) GROUP BY c1, c2  -> 2 rows, each count 1.
        // (assert the result has 2 groups)
    }
```

Also add/keep a test that a high-cardinality grouped query returns the same rows as before (the existing grouped tests already cover correctness; add one many-distinct-groups case if not present).

- [ ] **Step 2:** Run — fails if the current code already collides (it may not, since it uses `key == key` equality on `Vec<Value>`; this test PINS non-collision for the new hashed impl). If the current impl passes it, that's fine — it's a guard for the change.

- [ ] **Step 3:** Rewrite `group_rows` to bucket via a `HashMap<Vec<String>, usize>` mapping the per-cell `to_cmp_string()` vector (a `Vec<String>` is `Hash`+`Eq`, and per-cell vectors cannot collide the way a joined string can) to the group's index in `Vec<Group>`. On an unseen key, push a new `Group` (preserving first-occurrence order) and record its index; on a seen key, push the record into the existing group. Keep the `Group` struct and `execute_grouped`'s subsequent sort unchanged.

- [ ] **Step 4:** Run tests — the new test + all existing grouped/DISTINCT/HAVING/order tests pass. **Step 5:** fmt+clippy+`git diff main -- src/snapshots/` empty. **Step 6:** commit `perf(query): hash-keyed GROUP BY bucketing`.

---

### Task 2: Parallel file scan (W16)

**Files:** Modify `src/discover.rs` and/or `src/store.rs`, `src/cache.rs`.

**Interfaces:**
- Produces: a parallel scan helper, e.g. `fn scan_paths_parallel(root, paths, ...) -> (Vec<(PathBuf, T)>, Vec<Warning>)` or an internal change to `scan_root`/`refresh_*` that parallelizes the per-file `scan_file` and re-sorts by path. Signatures of `scan_root`/`InMemoryStore::load` stay the same.

- [ ] **Step 1: Failing/guard tests:**

```rust
    #[test]
    fn parallel_scan_matches_serial_records_and_order() {
        // Build a temp tree with N (e.g. 20) .md files with frontmatter,
        // a couple of no-frontmatter files (skipped), and a malformed one.
        // InMemoryStore::load(...) -> assert the records are in path-sorted
        // order and the set matches what a serial scan produces; assert the
        // warnings are in path-sorted order too.
    }
    #[test]
    fn scan_is_deterministic_across_runs() {
        // Load the same tree twice; SELECT file.path (no ORDER BY) -> identical
        // row order both times.
    }
```

- [ ] **Step 2:** Run — pass against the current serial code (they're guards for the parallel change). If order isn't already path-sorted deterministically, note it.

- [ ] **Step 3:** Parallelize the per-file scan. Preferred approach (no new dep): after `discover()` returns the path list, run `scan_file` across paths on `std::thread::available_parallelism()` workers, collecting `(path, ScanResult)` through an `mpsc` channel (or a scoped-thread chunked map). Then **sort the collected results by path** before building records/slices, and **sort warnings by path**, reproducing the serial order exactly. Apply the same pattern to `store::scan_root`, `cache::refresh_per_file`, `cache::refresh_fast`, and `cache::build_vault` (extract a shared `scan_paths_parallel` helper so the four sites don't duplicate the threading). Keep each function's external behavior identical.

- [ ] **Step 4:** Run tests — the two new tests + the whole suite pass; `git diff main -- src/snapshots/` empty. Confirm no new dependency was added (`git diff main -- Cargo.toml` empty). **Step 5:** fmt+clippy. **Step 6:** commit `perf(scan): parallelize per-file read/parse with deterministic path order`.

---

### Task 3: Projection push-down (W17)

**Files:** Modify `src/store.rs`, `src/cache.rs`, `src/main.rs` (+ `src/session.rs` if the seam runs through it).

**Interfaces:**
- Produces: a `wanted: Option<&BTreeSet<String>>` parameter threaded into the store-materialization path (`InMemoryStore::load`, `InMemoryStore::from_cache`, `records_from`, `scan_root`); `None` = keep all fields (today), `Some(set)` = keep only those field values. The store's `schema()` returns the FULL field-name union regardless.

- [ ] **Step 1: Failing tests** — the load-bearing correctness ones:

```rust
    // store.rs (or wherever records are materialized)
    #[test]
    fn pruning_keeps_only_wanted_field_values_but_full_schema() {
        // Build records from files with fields {status, prd, tags}.
        // Materialize with wanted = {"status"} -> each Record has only `status`
        // (prd/tags absent from the value map), BUT store.schema() still lists
        // status, prd, tags (full name union).
    }
```

```rust
    // tests/cli.rs — the W12-equivalence guard (THE load-bearing test)
    #[test]
    fn typo_under_pushdown_still_errors_with_didyoumean() {
        // one-shot `-e 'SELECT staus'` over a vault whose files have `status`
        // (and other fields NOT referenced) -> still errors naming `staus` and
        // suggesting `status`, identical to without push-down. Proves the full
        // schema (not the pruned set) backs validation + suggestion.
    }
    #[test]
    fn pushdown_output_is_byte_identical() {
        // `-e 'SELECT status, count(*) GROUP BY status'` output identical to the
        // same run on `main` (or: identical whether pruning is on or off — e.g.
        // compare a narrow query's output to a `SELECT *`-forced full run's
        // projection of the same columns).
    }
    #[test]
    fn select_star_disables_pruning() {
        // `-e 'SELECT *'` returns every field (pruning off when Star present).
    }
```

- [ ] **Step 2:** Run — fail (no `wanted` param / pruning).

- [ ] **Step 3:** Implement:
  - **Full-schema retention:** ensure the store computes `schema()` from the union of every file's frontmatter field NAMES during the scan, independent of which field VALUES are materialized. (If `schema()` currently derives from the materialized records' `field_names`, change it to retain the full name set separately when pruning is active — a `Vec<String>`/`BTreeSet<String>` collected during scan.)
  - **Value pruning:** in `Record` materialization (`scan_root`'s `Record::new(root, &path, file.fields)` and `records_from`'s `file.fields.clone()`), when `wanted = Some(set)`, keep only the entries whose key is in `set`; when `None`, keep all. Thread `wanted` through `InMemoryStore::load`/`from_cache` to those sites.
  - **Wiring in `main`:** in the one-shot/`-e`/piped-batch/`query run` paths (NOT the REPL), parse the statement(s), compute `wanted = union of referenced_fields()` — BUT set `wanted = None` if any statement projects `SelectExpr::Star` (or if parsing is deferred; if computing wanted before building the store is awkward, a two-pass "parse to get referenced_fields, then build store" is acceptable). Pass `wanted` into store construction. The REPL and `init`/cache-write paths pass `None`.
  - **Cache write untouched:** `build_vault`/`refresh_*` keep ALL fields (they write the cache; pruning is read-side only).

- [ ] **Step 4:** Run tests — the four new tests + the whole suite pass; `git diff main -- src/snapshots/` empty. Verify the REPL still has all fields (an existing REPL/`.describe`/`.schema` test, or add a note). **Step 5:** fmt+clippy. **Step 6:** commit `perf(store): projection push-down for narrow one-shot queries`.

---

### Task 4: Docs, final review, finish branch

- [ ] **Step 1:** Add a short `README.md` performance note: large-vault scans are parallelized across cores, and a narrow one-shot query (`SELECT count(*)`, `SELECT status`) prunes unread fields — both performance-only, no behavior change. (Keep it brief — one short paragraph or a bullet in an existing performance/caching section.)
- [ ] **Step 2:** Full verification: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`; `git diff main -- src/snapshots/` empty; `git diff main -- Cargo.toml` empty (no new dep).
- [ ] **Step 3:** Dispatch the final whole-branch reviewer (emphasis: byte-identical output/order/warnings, W12-validation preserved under push-down, no data race in the parallel scan, no new dependency), apply any pre-merge fixes, then finish the branch per `superpowers:finishing-a-development-branch` (merge to local `main`).
