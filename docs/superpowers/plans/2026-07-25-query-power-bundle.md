# Query-power bundle — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the whats-next W51–W57 bundle: regex predicate, `completions
--install`, work-stealing scan, vault-level config, live REPL completion, lazy
queryable Markdown body, and a hybrid (auto-ISO + `DATE()`) date type.

**Architecture:** Extend querymatter's `Value`/`compare_values`, the query
`ast`/`parse`/`exec` pipeline, the frontmatter ingest + `.querymatter` cache, the
`Settings` precedence layers, the parallel scanner, and the REPL helper — in
place, following existing patterns. W56+W57 share one `SCHEMA_VERSION` bump.

**Tech Stack:** Rust 2024, chrono (add `serde` feature), regex (already a dep),
clap, comfy-table, bincode/serde, rustyline. Tests via `cargo test`, insta,
assert_cmd + tempfile.

## Global Constraints

- Edition 2024; keep `cargo fmt --check` and `cargo clippy --all-targets -- -D
  warnings` clean (no pre-commit hook — run them yourself, via `cargo fmt`).
- Binary-only crate: full suite is `cargo test` (NOT `cargo test --lib`).
- stdout carries data; stderr carries diagnostics. Non-TTY/piped output stays
  byte-identical unless an item explicitly changes it (insta snapshots + tests/cli pin this).
- **W57 must not change observable behavior for existing ISO-date fields** — the
  I1–I5 invariants (spec §3) are load-bearing; pin each with a test.
- Config keys are snake_case identically on CLI and in TOML.
- A "declined because an invariant makes it safe" test is instead written at the
  seam it crosses (project spec-discipline rule).
- `SCHEMA_VERSION` bumps exactly once (2→3), in Task 6.

---

### Task 1: `Value::Date`/`DateTime` type — comparison + rendering (W57 core)

**Files:**
- Modify: `Cargo.toml` (chrono `serde` feature)
- Modify: `src/model.rs` (`Value` variants, `compare_values`, `display`/`to_cmp_string`/`type_name`, `as_number`)
- Modify: `src/render.rs` (`to_json` date arm)
- Test: inline in model.rs / render.rs

**Interfaces:**
- Produces: `Value::Date(chrono::NaiveDate)`, `Value::DateTime(chrono::DateTime<chrono::Utc>)`;
  `compare_values` handles date/datetime vs date/datetime and vs string; dates
  render as canonical ISO text everywhere.

- [ ] **Step 1: Cargo.toml — enable chrono serde**

Change the chrono dep to: `chrono = { version = "0.4.45", default-features = false, features = ["std", "serde"] }`. Run `cargo build` to confirm it resolves.

- [ ] **Step 2: Write failing model tests**

```rust
#[test]
fn dates_compare_by_instant_and_render_iso() {
    use chrono::NaiveDate;
    let a = Value::Date(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
    let b = Value::Date(NaiveDate::from_ymd_opt(2026, 7, 24).unwrap());
    assert_eq!(compare_values(&a, &b), Some(std::cmp::Ordering::Less));
    assert_eq!(a.display(), "2026-01-01");
    // date vs ISO string coerces (relative-date literals compare correctly)
    assert_eq!(
        compare_values(&b, &Value::Str("2026-01-01".into())),
        Some(std::cmp::Ordering::Greater)
    );
    // date vs a non-date string: defined, panic-free (fallback to text compare)
    assert!(compare_values(&b, &Value::Str("draft".into())).is_some());
    // NULL still unordered
    assert_eq!(compare_values(&a, &Value::Null), None);
}
```

- [ ] **Step 3: Run, confirm failure**

Run: `cargo test --quiet dates_compare_by_instant_and_render_iso`
Expected: FAIL — no `Value::Date`.

- [ ] **Step 4: Implement the variants + behavior**

In `src/model.rs`:
- Add `Date(chrono::NaiveDate)` and `DateTime(chrono::DateTime<chrono::Utc>)` to `enum Value` (derive already includes Serialize/Deserialize — chrono's serde feature makes these work).
- `display`: `Value::Date(d) => d.format("%Y-%m-%d").to_string()`, `Value::DateTime(dt) => dt.to_rfc3339_opts(SecondsFormat::Secs, true)`. (Import `chrono::SecondsFormat`.)
- `to_cmp_string`: same ISO rendering (so a date and its ISO string share a sort key).
- `type_name`: `"Date"` / `"DateTime"`.
- `as_number`: dates return `None`.
- `compact_value`/Map rendering: dates render as their ISO string.
- `compare_values`: add a case before the numeric branch — if BOTH are date/datetime, compare chronologically (a `NaiveDate` and a `DateTime` compare via their date part, or normalize both to `to_cmp_string`); if one is a date and the other a string, compare `to_cmp_string()` of both (ISO text — correct for ISO strings, defined for others). Keep the existing numeric and NULL rules.
- `render.rs` `to_json`: `Value::Date`/`DateTime` → `JsonValue::String(<iso>)` (stable, not a number).

- [ ] **Step 5: Run tests + snapshots**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
Expected: PASS; existing render snapshots unchanged (no dates in those fixtures).

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/model.rs src/render.rs
git commit -m "feat(model): Value::Date/DateTime with chrono comparison + ISO rendering (W57)"
```

---

### Task 2: Auto-detect strict ISO dates at ingest (W57)

**Files:**
- Modify: `src/frontmatter.rs` (`pod_to_value`'s `Pod::String` arm)
- Test: inline in frontmatter.rs

**Interfaces:**
- Consumes: `Value::Date`/`DateTime` (Task 1).
- Produces: a frontmatter string that is strict `YYYY-MM-DD` → `Value::Date`; strict RFC3339 → `Value::DateTime`; everything else unchanged.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn strict_iso_strings_become_dates_others_stay_strings() {
    use chrono::NaiveDate;
    assert_eq!(detect_scalar("2026-07-24"),
        Value::Date(NaiveDate::from_ymd_opt(2026,7,24).unwrap()));
    assert!(matches!(detect_scalar("2026-07-24T10:00:00Z"), Value::DateTime(_)));
    // non-dates stay strings/ints — I5
    for s in ["2026", "2026-07", "1.2.3", "v1", "draft", "2026-13-01", "2026-07-99"] {
        assert_eq!(detect_scalar(s), Value::Str(s.to_string()), "{s} must stay a string");
    }
}
```
(`detect_scalar` is the helper the `Pod::String` arm delegates to; expose it `pub(crate)`/module-private for the test.)

- [ ] **Step 2: Run, confirm failure**

Run: `cargo test --quiet strict_iso_strings_become_dates_others_stay_strings`
Expected: FAIL — `detect_scalar` undefined.

- [ ] **Step 3: Implement**

In `src/frontmatter.rs`, replace `Pod::String(s) => Value::Str(s)` (line ~61) with `Pod::String(s) => detect_scalar(&s)`, and add:
```rust
/// A frontmatter scalar string becomes a Value::Date (strict `%Y-%m-%d`) or
/// Value::DateTime (strict RFC3339); anything else stays a Value::Str. Strict:
/// chrono's own parse must accept the WHOLE string, so partial forms (`2026`,
/// `2026-07`) and invalid dates fall through to Str.
fn detect_scalar(s: &str) -> Value {
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Value::Date(d);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Value::DateTime(dt.with_timezone(&chrono::Utc));
    }
    Value::Str(s.to_string())
}
```
Note: `NaiveDate::parse_from_str` with `%Y-%m-%d` already rejects `2026`, `2026-07`, and out-of-range months/days. Verify `2026-7-4` (no leading zeros) behavior and decide (chrono accepts it; that's still an unambiguous date — acceptable to treat as a date, or tighten with a regex pre-check if you want strict zero-padding — document whichever you choose).

- [ ] **Step 4: Run tests**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/frontmatter.rs
git commit -m "feat(frontmatter): auto-detect strict ISO dates at ingest (W57)"
```

---

### Task 3: `DATE()` / `DATE(x, fmt)` scalar function (W57)

**Files:**
- Modify: `src/query/ast.rs` (`ScalarFn::Date`, its name), `src/query/parse.rs` (`lower_scalar_call` name match + arity), `src/query/exec.rs` (eval)
- Test: inline

**Interfaces:**
- Produces: `ScalarFn::Date`; `DATE(expr)` parses ISO, `DATE(expr, 'fmt')` parses with a chrono format; unparseable → `Value::Null`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn date_cast_parses_iso_and_custom_format() {
    // via full parse+eval, mirroring existing scalar-fn eval tests
    // DATE('2026-07-24') -> Value::Date(2026-07-24)
    // DATE('07/24/2026','%m/%d/%Y') -> Value::Date(2026-07-24)
    // DATE('nonsense') -> Value::Null
}
```
Write against the existing scalar-fn eval test scaffolding (grep `ScalarFn::Lower` tests).

- [ ] **Step 2: Run, confirm failure** — `cargo test --quiet date_cast_parses_iso_and_custom_format` → FAIL.

- [ ] **Step 3: Implement**

- `ast.rs`: add `Date` to `ScalarFn`; `scalar_fn_name`: `ScalarFn::Date => "date"`.
- `parse.rs` `lower_scalar_call`/name match: `"date" => Some(ScalarFn::Date)`; allow arity 1 or 2 (add to the arity-validation arm — Date takes 1 or 2 args).
- `exec.rs` `eval_scalar` (grep the `ScalarFn::Lower` eval arm): add `ScalarFn::Date` — eval arg0 to a string; if a 2nd arg present, use it as a chrono format via `NaiveDate::parse_from_str`; else try `%Y-%m-%d` then RFC3339; success → `Value::Date`/`DateTime`, failure → `Value::Null`. A `Value::Date`/`DateTime` arg passes through unchanged.
- Update any exhaustive `match ScalarFn` (compiler will flag them).

- [ ] **Step 4: Run tests + commit**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
```bash
git add src/query/ast.rs src/query/parse.rs src/query/exec.rs
git commit -m "feat(query): DATE()/DATE(x,fmt) cast scalar function (W57)"
```

---

### Task 4: W57 invariant characterization (I1–I4)

Pins the backward-compat invariants auto-detection depends on.

**Files:** Test-only — `src/query/exec.rs` tests and/or `tests/cli.rs`.

- [ ] **Step 1: I2 — relative-date filter unchanged.** A vault with an ISO `created` field: `WHERE created > '-7d'` returns the same rows as before dates existed (the field is now `Value::Date`, the literal resolves to an ISO string, and the comparison coerces). Assert the correct rows.
- [ ] **Step 2: I3 — ORDER BY / MIN / MAX on the date field** match chronological (== lexical for ISO) order; NULLs last.
- [ ] **Step 3: I4 — mixed column** (some files `created: 2026-07-24`, one `created: someday`) sorts panic-free with a defined order.
- [ ] **Step 4: I1 — rendering** a date field to csv/json/table yields its ISO text (byte-identical to the string it came from).
- [ ] **Step 5: Run + commit**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
```bash
git add src/query/exec.rs tests/cli.rs
git commit -m "test(query): pin W57 date invariants (relative-date, order, mixed, render)"
```

---

### Task 5: Regex predicate (W51)

**Files:** `src/query/ast.rs` (`Predicate::Regexp`), `src/query/parse.rs` (lowering + pattern validation), `src/query/exec.rs` (eval + the exhaustive-match arms)

**Interfaces:**
- Produces: `Predicate::Regexp(Expr, String, /* negated */ bool)`.
- **Grep `Predicate::Like` — every exhaustive `match Predicate` needs a `Regexp` arm:** `ast.rs:533` (`collect_predicate_fields` — use `collect_expr_fields` on the Expr operand), `ast.rs:677` (label), `exec.rs:225` (`rewrite_predicate_literals` — recurse into the Expr operand via `rewrite_expr_literals`; the pattern String has no literals), `exec.rs:632` (`predicate_columns` — `expr_columns` on the operand), `exec.rs:1458` (`eval_predicate`).

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn regexp_matches_and_negates_and_over_scalar_fn() {
    // WHERE jira REGEXP '^DCP-[0-9]+$'  -> matches DCP-459
    // WHERE jira NOT REGEXP '^DCP-'     -> excludes it
    // WHERE lower(status) REGEXP 'draft'  -> Expr operand works
    // a bad pattern '(' is rejected at parse time
}
```

- [ ] **Step 2: Run, confirm failure.**

- [ ] **Step 3: Implement**

- `ast.rs`: `Regexp(Expr, String, bool)` on `Predicate`; label + `collect_predicate_fields` arms.
- `parse.rs` `lower_predicate` (grep the `Predicate::Like` lowering ~line 646): handle sqlparser's `REGEXP`/`RLIKE` binary op (or `BinaryOperator::PGRegexMatch`, whichever sqlparser 0.62 emits — verify) and `NOT REGEXP`; lower the left to an `Expr` via `lower_expr`, the pattern to a `String`. Reject an un-compilable pattern up front: `regex::Regex::new(&pat).map_err(|e| unsupported(format!("invalid regex `{pat}`: {e}")))?` (compile to validate, discard).
- `exec.rs` `eval_predicate` `Regexp` arm: compile the pattern (or reuse a compiled cache), eval the operand to a `Value`, test `regex.is_match(&value.display())`; NULL operand → no match; apply `negated`. Add the `rewrite_predicate_literals` + `predicate_columns` arms.

- [ ] **Step 4: Run + commit**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
```bash
git add src/query/ast.rs src/query/parse.rs src/query/exec.rs
git commit -m "feat(query): REGEXP predicate over expressions (W51)"
```

---

### Task 6: `file.word_count` + SCHEMA_VERSION bump (W56 part 1)

**Files:** `src/frontmatter.rs` (return body/word count), `src/cache.rs` (`CachedFile.word_count` + `SCHEMA_VERSION` 2→3 + scan), `src/model.rs` (`FileAttr::WordCount` + `Record.word_count`), `src/query/exec.rs`/`src/query/parse.rs` (`file.word_count` column), `src/store.rs` (thread word_count onto Record)

**Interfaces:**
- Produces: `FileAttr::WordCount`; `CachedFile.word_count: usize`; `SCHEMA_VERSION = 3`.

- [ ] **Step 1: Write failing tests** — `frontmatter::extract` (or a sibling) reports a body word count for a known fixture; `file.word_count` is queryable and returns that count. `SCHEMA_VERSION == 3`.
- [ ] **Step 2: Run, confirm failure.**
- [ ] **Step 3: Implement**
  - `frontmatter.rs`: capture `parsed.content`; compute `word_count = content.split_whitespace().count()`. Return it alongside the fields (extend `Extract`, or add a function).
  - `cache.rs`: add `pub word_count: usize` to `CachedFile`; set it in the scan path; bump `SCHEMA_VERSION` to `3`.
  - `model.rs`: add `FileAttr::WordCount`; `Record` gains `word_count`; `file_attr(WordCount) => Value::Int(self.word_count as i64)`.
  - `parse.rs`: recognize `file.word_count` as `ColRef::File(FileAttr::WordCount)` (grep how `file.mtime`/`file.size` are recognized).
  - `store.rs`: thread `word_count` from `CachedFile`/scan onto `Record`.
- [ ] **Step 4: Run + commit** (verify a stale v2 cache is rejected/rebuilt cleanly — add/confirm a test).

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
```bash
git add src/frontmatter.rs src/cache.rs src/model.rs src/query/parse.rs src/query/exec.rs src/store.rs
git commit -m "feat(cache): file.word_count + SCHEMA_VERSION 3 (W56)"
```

---

### Task 7: `file.body` lazy disk read (W56 part 2)

The one task with eval-time I/O. `Record::file_attr` is a pure `&self -> Value`;
`file.body` must read the file, so it needs the absolute path and a disk-access
gate — do NOT route it through the pure `file_attr`.

**Files:** `src/model.rs` (`FileAttr::Body`, abs-path access), `src/query/exec.rs` (body-read helper + eval), `src/store.rs`/`src/query/mod.rs` (thread root/abs-path + a `disk_reads_allowed` flag from freshness)

**Interfaces:**
- Consumes: the record's absolute path (root + rel path) and whether disk reads are permitted (false under `Freshness::ForceCache`).
- Produces: `FileAttr::Body`; `file.body` → the on-disk body text, or `Value::Null` (lenient) / a clear error (strict) when unreadable / under `--force-cache`.

- [ ] **Step 1: Write failing tests**
  - `WHERE file.body LIKE '%TODO%'` matches a fixture whose body contains TODO.
  - Under `--force-cache`, a `file.body` query yields NULL/diagnostic, not a wrong answer.
  - A frontmatter-only query performs NO body read (guard the I/O regression — e.g. assert via a body-read counter or that a query without `file.body` succeeds against a vault whose bodies were deleted after caching).
- [ ] **Step 2: Run, confirm failure.**
- [ ] **Step 3: Implement**
  - `model.rs`: add `FileAttr::Body`. Ensure a `Record` can yield its absolute path (add `abs_path: PathBuf`, or expose root + rel_path). Do NOT implement Body in the pure `file_attr` — return a sentinel or handle it at the eval site.
  - `exec.rs`: at the `ColRef::File(attr)` eval site (line ~1222), special-case `FileAttr::Body`: if disk reads are allowed, `std::fs::read_to_string(abs_path)` → `Value::Str(body)` (Err → Null/diagnostic); if not allowed (ForceCache), Null (lenient) or a diagnostic (strict). Isolate in a `read_body(record, allowed) -> Value` helper.
  - Thread `disk_reads_allowed` (derived from `Freshness`) into the eval context. Check how the executor already receives config (lenient flag is already threaded — follow that path).
- [ ] **Step 4: Run + commit**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
```bash
git add src/model.rs src/query/exec.rs src/store.rs src/query/mod.rs
git commit -m "feat(query): file.body lazy disk read with --force-cache guard (W56)"
```

---

### Task 8: Work-stealing parallel scan (W53)

**Files:** `src/parallel.rs`

- [ ] **Step 1: Write failing/characterization test** — a size-skewed workload (paths where `f` takes wildly different time — simulate with an index-based delay or just varied work) returns results sorted by path, byte-identical to the serial map. (This test should PASS on the current code too — it's characterization; keep it green through the refactor.)
- [ ] **Step 2: Confirm it passes today** (characterization), then refactor.
- [ ] **Step 3: Implement** — replace the `paths.chunks(chunk_size)` static split with a shared `std::sync::atomic::AtomicUsize` cursor: spawn `workers` threads in the existing `thread::scope`, each looping `let i = cursor.fetch_add(1, Ordering::Relaxed); if i >= paths.len() { break }` and processing `paths[i]`, pushing `(path, f(path))` into a per-worker Vec; merge and `sort_by(path)` as today. Preserve panic propagation.
- [ ] **Step 4: Run + commit**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
```bash
git add src/parallel.rs
git commit -m "perf(parallel): work-stealing scan via shared atomic cursor (W53)"
```

---

### Task 9: Vault-level config layer (W54)

**Files:** `src/settings.rs` (`Source::Vault` + resolve layers), `src/config.rs` (load a vault config path), `src/cache.rs` or a small discovery helper (walk up for `.querymatter.toml`), `src/main.rs` (load + pass the vault config)

**Interfaces:**
- Produces: `Source::Vault`; a vault-root `.querymatter.toml` loaded with the existing `Config` schema; precedence `flag > env > vault > config > default`.

- [ ] **Step 1: Write failing tests** (settings.rs, mirroring existing precedence tests) — vault config beats user config; a flag/env beats vault; absent vault file is a no-op; malformed vault file errors naming the path.
- [ ] **Step 2: Run, confirm failure.**
- [ ] **Step 3: Implement**
  - Discovery: walk up from the scan root (or cwd) for a `.querymatter.toml` file; return its path if found.
  - `config.rs`: reuse `load_from(path)` to parse it into `Config`.
  - `settings.rs`: add `Source::Vault`; change `resolve`/`resolve_walk`/`resolve_value`/`resolve_bool` to accept a `vault: &Config` layer consulted after env and before the user config (i.e. `cli/flag`, then env, then `vault`, then `config`, then default). Update `cells()`/`rows()` to report `(vault)`.
  - `main.rs`: discover + load the vault config and pass it into `Settings::resolve`.
- [ ] **Step 4: Run + commit**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
```bash
git add src/settings.rs src/config.rs src/cache.rs src/main.rs
git commit -m "feat(config): vault-level .querymatter.toml layer (W54)"
```

---

### Task 10: `completions --install [shell]` (W52)

**Files:** `src/cli.rs` (`CompletionsArgs` + `--install`), `src/main.rs` (`run_completions`)

- [ ] **Step 1: Write failing integration test** (`tests/cli.rs`) — `completions --install bash` with an overridden `$HOME`/dir writes the script to the expected path; an unwritable target errors clearly (and the plain `completions bash` still prints to stdout).
- [ ] **Step 2: Run, confirm failure.**
- [ ] **Step 3: Implement**
  - `cli.rs`: add `#[arg(long)] install: bool` to `CompletionsArgs`; make `shell` optional (auto-detect from `$SHELL` when omitted + `--install`).
  - `main.rs` `run_completions`: when `--install`, resolve the shell's user completion dir (bash: `~/.local/share/bash-completion/completions/<name>`; zsh: `~/.zsh/completions/_<name>` or a writable fpath dir; fish: `~/.config/fish/completions/<name>.fish`), `create_dir_all`, write the generated script, confirm on stderr. On a non-writable/undetectable dir: clear stderr error + fall back to printing the script to stdout. Keep the no-`--install` path exactly as today.
- [ ] **Step 4: Run + commit**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
```bash
git add src/cli.rs src/main.rs tests/cli.rs
git commit -m "feat(cli): completions --install writes the script into place (W52)"
```

---

### Task 11: Live REPL tab-completion refresh (W55)

**Files:** `src/repl.rs`

- [ ] **Step 1: Write failing test** — the schema/query-name snapshot recomputation (a pure `fn` producing the lists from a session) reflects a saved query added mid-session and a schema change after reload. (The live `editor.helper_mut()` push isn't drivable headless — test the recomputation, per the codebase's REPL-test convention.)
- [ ] **Step 2: Run, confirm failure.**
- [ ] **Step 3: Implement** — extract a `refresh_helper(&mut editor, &session)` that recomputes `schema` + `query_names` and writes them into `editor.helper_mut()`. Call it after `.reload`, `.refresh`, `.refresh-all`, and `.query save` succeed. (Saved-query names come from `saved_query_names()`; schema from `session.schema()`.)
- [ ] **Step 4: Run + commit**

Run: `cargo test --quiet && cargo fmt --check && cargo clippy --quiet --all-targets -- -D warnings`
```bash
git add src/repl.rs
git commit -m "feat(repl): refresh tab-completion after reload/refresh/query-save (W55)"
```

---

### Task 12: Documentation (README)

**Files:** `README.md`

- [ ] **Step 1: Update README** — DSL section: `REGEXP`/`NOT REGEXP`; `DATE()`/`DATE(x,fmt)` + the auto-ISO date behavior and the mixed-column note. Pseudo-columns: `file.body` (lazy, with the `--force-cache` caveat) and `file.word_count`. Subcommands/flags: `completions --install [shell]`. Config: the vault-level `.querymatter.toml` layer + `flag > env > vault > config > default` precedence. REPL: note `.reload`/`.refresh`/`.query save` now refresh completion. Note the `SCHEMA_VERSION` bump (caches rebuild on next run). Verify names/defaults against the code.
- [ ] **Step 2: Verify no stale claims** — e.g. any "only LIKE" or "dates compared as strings" statements.
- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: document W51-W57 (regex, dates, body, completions, vault config)"
```

---

## Self-Review

**Spec coverage:** W51→T5; W52→T10; W53→T8; W54→T9; W55→T11; W56→T6 (word_count+bump) + T7 (body); W57→T1 (type) + T2 (ingest) + T3 (DATE()) + T4 (invariants). Docs→T12. No gaps.

**Placeholders:** remaining "verify sqlparser 0.62's REGEXP operator variant" (T5), "verify `2026-7-4` behavior" (T2), and "follow how the lenient flag is threaded" (T7) are pointers to exact code/decisions the implementer confirms, not missing content.

**Type consistency:** `Value::Date(NaiveDate)`/`DateTime(DateTime<Utc>)` (T1) consumed by T2/T3/T4; `compare_values` extension (T1) relied on by T4; `Predicate::Regexp(Expr,String,bool)` (T5) arms match across the four exhaustive sites; `FileAttr::WordCount` (T6) precedes `FileAttr::Body` (T7); `SCHEMA_VERSION=3` set once in T6; `Source::Vault` (T9) threaded through resolve. Consistent.
