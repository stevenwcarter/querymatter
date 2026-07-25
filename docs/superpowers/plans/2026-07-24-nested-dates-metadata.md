# Nested values, dates & file metadata — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add nested-map `Value`s with dotted-path queries + full-fidelity JSON export, `file.mtime`/`file.size` pseudo-columns, relative-date literals, `COALESCE(...)`, and subtree-scoped cache loading to querymatter.

**Architecture:** Extend the existing `Value`/`Expr`/`Literal`/`ColRef`/`FileAttr` types and their lowering (`query/parse.rs`) + evaluation (`query/exec.rs`) seams. Dates stay ISO-8601 strings compared lexicographically (no `Value::Date`). Subtree scoping filters the per-directory cache manifest before decoding blobs.

**Tech Stack:** Rust edition 2024, `sqlparser`, `gray_matter`, `indexmap`, `bincode`, `serde_json`, `csv`, `comfy-table`. New: `chrono` (clock + RFC3339 formatting).

**Reference spec:** `docs/superpowers/specs/2026-07-24-nested-dates-metadata-design.md` — read §9 (invariants) before every task.

## Global Constraints

- **Edition 2024**; all code `cargo clippy --all-targets` clean and `cargo fmt` clean (the `rust-developer` agent enforces this).
- **Dates are ISO-8601 strings, compared lexicographically.** No `Value::Date` type. `file.mtime` and relative literals live in the ISO string space.
- **New dependency:** `chrono = { version = "0.4", features = ["clock"] }` — UTC only, no `chrono-tz`.
- **Cache `SCHEMA_VERSION` bumps `1 → 2`** exactly once (Task 2), never per-task.
- **Byte-identical render invariant:** table/CSV/TSV/markdown output for existing data is unchanged; only `-o json` gains nested objects, and only queries that explicitly select a new column (`file.mtime`, `file.size`, a dotted path) show new output. The committed snapshots under `src/snapshots/` must stay byte-identical unless a test deliberately adds a new-column case.
- **`referenced_fields()` returns top-level field names only** (a dotted path contributes its root segment) — W12 validation + W17 push-down both consume it.
- Every task ends green: `cargo test` passes, `cargo clippy` clean, then commit.

---

## File Structure

| file | responsibility in this plan |
|---|---|
| `Cargo.toml` | add `chrono` (Task 4) |
| `src/model.rs` | `Value::Map` + `compact_value` + rendering (T1); `Record.mtime/size` + `FileAttr::Mtime/Size` + `system_time_to_iso` (T4); `Record::field` path-walk (T3) |
| `src/frontmatter.rs` | `pod_to_value` → `Value::Map` (T2) |
| `src/render.rs` | `to_json` `Value::Map` arm (T2) |
| `src/cache.rs` | `SCHEMA_VERSION` bump (T2); thread `(mtime,size)` in `records_from` (T4); scoped decode (T7) |
| `src/store.rs` | thread `(mtime,size)` in `scan_root` (T4); `from_cache` subtree param + scoped schema (T7) |
| `src/query/ast.rs` | `ColRef::Field(Vec<String>)` (T3); `Literal::RelativeDate` (T5); `Expr::Coalesce` (T6); `FileAttr` label (T4) |
| `src/query/parse.rs` | dotted-path lowering (T3); `file.*` mtime/size (T4); relative-date recognition (T5); `COALESCE` lowering (T6) |
| `src/query/exec.rs` | path resolve (T3); `execute_at(now)` + relative rewrite (T5); `Coalesce` eval + `expr_columns` (T6) |
| `src/main.rs` | pass subtree to `from_cache`, drop post-hoc `retain_under` on scoped path (T7) |
| `README.md` | document the new surface (T8) |

Sequence (shared files force serial execution): **T1 → T2 → T3 → T4 → T5 → T6 → T7 → T8.**

---

## Task 1: `Value::Map` variant + compact rendering

**Files:**
- Modify: `src/model.rs` (the `Value` enum + its `impl`)
- Test: `src/model.rs` `#[cfg(test)] mod tests`

**Interfaces:**
- Produces: `Value::Map(IndexMap<String, Value>)`; `pub fn compact_value(v: &Value) -> String` (module-private is fine — used by `display`); `Value::display`, `variant_name`, `as_number`, `to_cmp_string` handle `Map`.
- Consumes: nothing new (`IndexMap` already imported).

**Critical invariant (spec §9):** `Value::Map`'s display MUST equal what `frontmatter::compact_pod` produces today — **nested lists render with brackets** (`[a, b]`), nested maps with braces (`{k: v}`), keys sorted. This is NOT the same as `Value::display(List)` (which is bracket-less `a, b`). So `Map` display uses a dedicated `compact_value`, not `Value::display` recursion.

- [ ] **Step 1: Write the failing tests**

Add to `src/model.rs` tests:

```rust
#[test]
fn map_display_matches_compact_pod_form() {
    // keys sorted; a nested LIST inside a map renders WITH brackets,
    // matching the old compact_pod output (NOT Value::display's bracket-less list).
    let mut inner = IndexMap::new();
    inner.insert("low".to_string(), Value::Int(5));
    inner.insert("high".to_string(), Value::Int(10));
    inner.insert(
        "tags".to_string(),
        Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
    );
    let v = Value::Map(inner);
    assert_eq!(v.display(), "{high: 10, low: 5, tags: [a, b]}");
}

#[test]
fn map_variant_name_and_as_number() {
    let v = Value::Map(IndexMap::new());
    assert_eq!(v.variant_name(), "Map");
    assert_eq!(v.as_number(), None);
}

#[test]
fn nested_map_display_is_recursive() {
    let mut inner = IndexMap::new();
    inner.insert("x".to_string(), Value::Int(1));
    let mut outer = IndexMap::new();
    outer.insert("a".to_string(), Value::Map(inner));
    assert_eq!(Value::Map(outer).display(), "{a: {x: 1}}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib model:: 2>&1 | tail -20`
Expected: FAIL — `no variant Map`.

- [ ] **Step 3: Add the variant + rendering**

In `src/model.rs`, add `Map(IndexMap<String, Value>)` to `enum Value` (after `List`). Add a module-private helper mirroring `compact_pod`:

```rust
/// Compact, deterministic string for a `Value` used when a `Map` (or a map
/// nested in one) is rendered flat (table/CSV). Mirrors
/// `frontmatter::compact_pod` exactly: lists render WITH brackets, maps with
/// braces, map keys sorted. This is intentionally different from
/// `Value::display(List)` (bracket-less) — see spec §9.
fn compact_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Str(s) => s.clone(),
        Value::List(items) => {
            let rendered: Vec<_> = items.iter().map(compact_value).collect();
            format!("[{}]", rendered.join(", "))
        }
        Value::Map(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let rendered: Vec<_> = entries
                .iter()
                .map(|(k, v)| format!("{k}: {}", compact_value(v)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
    }
}
```

Add arms to the existing methods:
- `display`: `Value::Map(_) => compact_value(self),`
- `variant_name`: `Value::Map(_) => "Map",`
- `as_number`: fold `Map` into the `None` arm (`Value::Bool(_) | Value::Null | Value::List(_) | Value::Map(_) => None`).
- `to_cmp_string` is `self.display()` already — no change needed.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib model:: 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy --all-targets 2>&1 | tail -5
git add src/model.rs
git commit -m "feat(model): add Value::Map variant with compact_pod-matching render (W25)"
```

---

## Task 2: `pod_to_value` → nested `Value::Map`, cache bump, JSON export

**Files:**
- Modify: `src/frontmatter.rs` (`pod_to_value`)
- Modify: `src/cache.rs` (`SCHEMA_VERSION`)
- Modify: `src/render.rs` (`to_json`)
- Test: `src/frontmatter.rs` tests, `src/render.rs` tests

**Interfaces:**
- Consumes: `Value::Map` (Task 1).
- Produces: `frontmatter::extract` now yields `Value::Map` for nested mappings; `render::to_json` emits nested `JsonValue::Object`.

- [ ] **Step 1: Write the failing tests**

In `src/frontmatter.rs` tests:

```rust
#[test]
fn nested_mapping_becomes_value_map() {
    let c = "---\nestimate:\n  low: 5\n  high: 10\n---\n";
    let Extract::Fields(m) = extract(c) else { panic!("expected Fields") };
    let mut expected = IndexMap::new();
    expected.insert("low".to_string(), Value::Int(5));
    expected.insert("high".to_string(), Value::Int(10));
    // Compare structurally regardless of key order (Pod::Hash is unordered).
    let Some(Value::Map(got)) = m.get("estimate") else { panic!("expected Map") };
    assert_eq!(got.get("low"), expected.get("low"));
    assert_eq!(got.get("high"), expected.get("high"));
}
```

In `src/render.rs` tests (find the existing json test module and mirror its style):

```rust
#[test]
fn json_export_emits_nested_object_for_map() {
    use crate::model::Value;
    use indexmap::IndexMap;
    let mut inner = IndexMap::new();
    inner.insert("low".to_string(), Value::Int(5));
    let v = Value::Map(inner);
    // to_json is module-private; assert via the JsonValue it builds.
    let j = super::to_json(&v);
    assert_eq!(j, serde_json::json!({ "low": 5 }));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib frontmatter:: render:: 2>&1 | tail -20`
Expected: FAIL — `pod_to_value` still returns `Str`; `to_json` has no `Map` arm.

- [ ] **Step 3: Implement**

`src/frontmatter.rs` — change the `Pod::Hash` arm of `pod_to_value`:

```rust
Pod::Hash(map) => Value::Map(
    map.into_iter().map(|(k, v)| (k, pod_to_value(v))).collect(),
),
```

`compact_pod` stays (it is still referenced by tests and documents the shape) — but if `cargo clippy` now flags it as dead code, delete it and update its doc-referencing tests; `Value::compact_value` (Task 1) is now the live renderer. Prefer deletion if unused.

`src/cache.rs` — bump the constant:

```rust
pub const SCHEMA_VERSION: u32 = 2;
```

`src/render.rs` — add to `to_json`:

```rust
Value::Map(m) => JsonValue::Object(
    m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect(),
),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib frontmatter:: render:: 2>&1 | tail -20`
Then the full suite to confirm the schema bump + render change broke nothing:
Run: `cargo test 2>&1 | tail -20`
Expected: PASS. If a snapshot test fails, inspect the diff — a nested-map field previously rendered `{high: 10, low: 5}` via `Value::Str` and must still render identically via `Value::Map`; if it differs, `compact_value` (Task 1) is wrong, not the snapshot.

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy --all-targets 2>&1 | tail -5
git add src/frontmatter.rs src/cache.rs src/render.rs
git commit -m "feat(frontmatter): parse nested YAML into Value::Map; JSON export nested; cache v2 (W25)"
```

---

## Task 3: Dotted-path column references

**Files:**
- Modify: `src/query/ast.rs` (`ColRef`, `label`, `collect_col_field`)
- Modify: `src/query/parse.rs` (`lower_col_ref`, `lower_compound`)
- Modify: `src/model.rs` (`Record::field`)
- Modify: `src/query/exec.rs` (the `resolve_col`/field-resolution site)
- Test: `src/query/parse.rs`, `src/model.rs`, `src/query/ast.rs` tests

**Interfaces:**
- Consumes: `Value::Map` (Task 1).
- Produces: `ColRef::Field(Vec<String>)` (was `Field(String)`); `Record::field(path: &[String]) -> Value`.

**Note:** widening `ColRef::Field(String)` → `Field(Vec<String>)` is compiler-guided — every construction/match site will error until updated. Single-segment paths preserve today's behavior.

- [ ] **Step 1: Write the failing tests**

In `src/model.rs` record tests:

```rust
#[test]
fn field_walks_dotted_path_into_map() {
    let mut inner = IndexMap::new();
    inner.insert("low".to_string(), Value::Int(5));
    let mut f = IndexMap::new();
    f.insert("estimate".to_string(), Value::Map(inner));
    let r = Record::new(Path::new("v"), Path::new("v/a.md"), f);
    assert_eq!(r.field(&["estimate".into(), "low".into()]), Value::Int(5));
    // missing sub-key -> Null
    assert_eq!(r.field(&["estimate".into(), "nope".into()]), Value::Null);
    // non-map intermediate -> Null
    assert_eq!(r.field(&["estimate".into(), "low".into(), "x".into()]), Value::Null);
    // single segment == today's behavior
    assert_eq!(r.field(&["estimate".into()]).variant_name(), "Map");
}
```

In `src/query/parse.rs` tests:

```rust
#[test]
fn dotted_identifier_lowers_to_path() {
    let q = parse("SELECT estimate.low WHERE estimate.high > 10").unwrap();
    assert!(q.referenced_fields().contains("estimate"));
    // referenced_fields returns the TOP-LEVEL segment only
    assert!(!q.referenced_fields().contains("high"));
    assert!(!q.referenced_fields().contains("low"));
}

#[test]
fn file_attr_still_special_and_no_nesting() {
    assert!(parse("SELECT file.name").is_ok());
    assert!(parse("SELECT file.name.x").is_err()); // file.* has no nesting
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib model::record parse:: 2>&1 | tail -20`
Expected: FAIL / does-not-compile (`Field` signature).

- [ ] **Step 3: Implement**

`src/query/ast.rs`:
- `ColRef::Field(String)` → `ColRef::Field(Vec<String>)`.
- `ColRef::label`: `ColRef::Field(path) => path.join("."),`
- `collect_col_field`: `if let ColRef::Field(path) = col { fields.insert(path[0].clone()); }` (top-level segment only — spec §3.4).

`src/query/parse.rs`:
- `lower_col_ref`: bare `Identifier(ident)` → `ColRef::Field(vec![ident.value.clone()])`.
- `lower_compound`: keep the `[prefix, attr]` where `prefix == "file"` → `ColRef::File(...)` branch, but add: a `file` first segment with ≠2 parts is an error (`file.*` has no nesting); any other compound → `ColRef::Field(parts.iter().map(|p| p.value.clone()).collect())`.

`src/model.rs` — `Record::field` walks a path:

```rust
/// The value at `path` (segment 0 = frontmatter field, each next segment
/// indexes into a `Value::Map`). Missing key or non-map intermediate → Null.
pub fn field(&self, path: &[String]) -> Value {
    let Some((head, rest)) = path.split_first() else { return Value::Null };
    let mut cur = self.fields.get(head).cloned().unwrap_or(Value::Null);
    for seg in rest {
        cur = match cur {
            Value::Map(m) => m.get(seg).cloned().unwrap_or(Value::Null),
            _ => return Value::Null,
        };
    }
    cur
}
```

`src/query/exec.rs`: update the site(s) that call `record.field(name)` (the `resolve_col`/`ColRef::Field` arm) to pass the path slice: `ColRef::Field(path) => record.field(path),`. Update every other `ColRef::Field(...)` match/constructor the compiler flags (e.g. tests building `ColRef::Field("x".into())` → `ColRef::Field(vec!["x".into()])`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test 2>&1 | tail -20`
Expected: PASS (whole suite — this touches shared types).

- [ ] **Step 5: Clippy + commit**

```bash
cargo clippy --all-targets 2>&1 | tail -5
git add src/query/ast.rs src/query/parse.rs src/model.rs src/query/exec.rs
git commit -m "feat(query): dotted-path column references into nested maps (W25)"
```

---

## Task 4: `file.mtime` / `file.size` pseudo-columns

**Files:**
- Modify: `Cargo.toml` (add `chrono`)
- Modify: `src/model.rs` (`Record` fields + `Record::new`, `FileAttr`, `file_attr`, `system_time_to_iso`)
- Modify: `src/query/ast.rs` (`file_attr_label`)
- Modify: `src/query/parse.rs` (`file_attr_from_str`)
- Modify: `src/store.rs` (`scan_root` → `Record::new`)
- Modify: `src/cache.rs` (`records_from` → `Record::new`)
- Test: `src/model.rs`, plus a store/cache producer test

**Interfaces:**
- Produces: `FileAttr::Mtime`, `FileAttr::Size`; `Record::new(root, path, fields, mtime: SystemTime, size: u64)`; `model::system_time_to_iso(SystemTime) -> String`.

- [ ] **Step 1: Add chrono**

```bash
cargo add chrono --no-default-features --features clock
```
(`clock` pulls the minimal std clock support; no `chrono-tz`.) Verify it resolves: `cargo build 2>&1 | tail -5`.

- [ ] **Step 2: Write the failing tests**

In `src/model.rs`:

```rust
#[test]
fn system_time_to_iso_is_rfc3339_utc() {
    use std::time::{Duration, UNIX_EPOCH};
    // 2021-01-01T00:00:00Z == 1609459200 secs
    let t = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
    assert_eq!(system_time_to_iso(t), "2021-01-01T00:00:00Z");
}

#[test]
fn file_attr_mtime_and_size() {
    use std::time::{Duration, UNIX_EPOCH};
    let t = UNIX_EPOCH + Duration::from_secs(1_609_459_200);
    let r = Record::new(Path::new("v"), Path::new("v/a.md"), IndexMap::new(), t, 42);
    assert_eq!(r.file_attr(FileAttr::Size), Value::Int(42));
    assert_eq!(r.file_attr(FileAttr::Mtime), Value::Str("2021-01-01T00:00:00Z".into()));
}

#[test]
fn pre_epoch_mtime_does_not_panic() {
    use std::time::{Duration, UNIX_EPOCH};
    let t = UNIX_EPOCH - Duration::from_secs(60);
    let _ = system_time_to_iso(t); // must not panic
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib model:: 2>&1 | tail -20`
Expected: FAIL / does-not-compile.

- [ ] **Step 4: Implement**

`src/model.rs`:
- Add to `enum FileAttr`: `Mtime`, `Size`.
- Add fields to `struct Record`: `mtime: SystemTime`, `size: u64` (add `use std::time::SystemTime;`).
- `Record::new(root, path, fields, mtime, size)` — store the two new params.
- `file_attr`: return `Value::Str(system_time_to_iso(self.mtime))` for `Mtime`, `Value::Int(self.size as i64)` for `Size`. NOTE: `file_attr` currently returns `Value::Str` uniformly; restructure it to build the right `Value` per attr (the Name/Path/Folder/Ext arms still return `Value::Str`).
- Add:

```rust
use chrono::{DateTime, SecondsFormat, Utc};

/// An `mtime` as an RFC3339 UTC string (`2021-01-01T00:00:00Z`), seconds
/// precision. Pre-epoch times format to their real negative timestamp; never
/// panics.
pub fn system_time_to_iso(t: SystemTime) -> String {
    DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Secs, true)
}
```

`src/query/ast.rs` — `file_attr_label`: add `FileAttr::Mtime => "file.mtime"`, `FileAttr::Size => "file.size"`.

`src/query/parse.rs` — `file_attr_from_str`: add `"mtime" => Ok(FileAttr::Mtime)`, `"size" => Ok(FileAttr::Size)`.

`src/store.rs` (`scan_root`) and `src/cache.rs` (`records_from`): pass the already-available `(mtime, size)` into `Record::new`. In `records_from`, use `CachedFile.mtime` / `.size`. In `scan_root`, use the stat already read for the file (the `ScanResult`/metadata in scope). Update the `rec()` helper in `model.rs` tests and any other `Record::new` callers the compiler flags (pass `UNIX_EPOCH, 0` in unrelated tests).

- [ ] **Step 5: Write the producer-parity test**

Add an integration-style test (in `src/store.rs` tests or `tests/`) asserting **both** producers expose the columns. Minimal version — a live scan over a temp file:

```rust
#[test]
fn scan_root_exposes_file_mtime_and_size() {
    // build a temp vault with one .md file, scan it live, run
    // `SELECT file.size, file.mtime`, assert size > 0 and mtime parses as
    // an RFC3339 string (starts with a 4-digit year + '-').
    // (Mirror the existing store test harness for temp-vault setup.)
}
```
Add the mirror for the cache path (`from_cache`/`records_from`) if the existing test harness has a cache fixture; otherwise assert `records_from` maps `CachedFile{mtime,size}` through in a unit test.

- [ ] **Step 6: Run tests + clippy + commit**

Run: `cargo test 2>&1 | tail -20` (expect PASS)

```bash
cargo clippy --all-targets 2>&1 | tail -5
git add Cargo.toml Cargo.lock src/model.rs src/query/ast.rs src/query/parse.rs src/store.rs src/cache.rs
git commit -m "feat(query): file.mtime (ISO-8601 UTC) and file.size pseudo-columns (W24)"
```

---

## Task 5: Relative-date literals

**Files:**
- Modify: `src/query/ast.rs` (`Literal`, `RelDate`, `DateUnit`, `RelDate::parse`, `literal_label`)
- Modify: `src/query/parse.rs` (string-literal lowering)
- Modify: `src/query/exec.rs` (`execute_at(now)` seam + relative-date rewrite)
- Test: `src/query/ast.rs`, `src/query/parse.rs`, `src/query/exec.rs` tests

**Interfaces:**
- Consumes: `chrono` (Task 4).
- Produces: `Literal::RelativeDate(RelDate)`; `RelDate::parse(&str) -> Option<RelDate>`; `exec::execute_at(query, store, wanted, now: SystemTime)` internal seam (public `execute` delegates with `SystemTime::now()`).

- [ ] **Step 1: Write the failing tests**

In `src/query/ast.rs`:

```rust
#[test]
fn reldate_parse_grammar() {
    use super::{RelDate, DateUnit};
    assert_eq!(RelDate::parse("today"), Some(RelDate::Today));
    assert_eq!(RelDate::parse("now"), Some(RelDate::Now));
    assert_eq!(RelDate::parse("-7d"), Some(RelDate::Offset { n: -7, unit: DateUnit::Day }));
    assert_eq!(RelDate::parse("+3w"), Some(RelDate::Offset { n: 3, unit: DateUnit::Week }));
    assert_eq!(RelDate::parse("-2mo"), Some(RelDate::Offset { n: -2, unit: DateUnit::Month }));
    assert_eq!(RelDate::parse("-1y"), Some(RelDate::Offset { n: -1, unit: DateUnit::Year }));
    // rejects
    for bad in ["7d", "-7m", "-7x", "tomorrow", "draft", "-7", ""] {
        assert_eq!(RelDate::parse(bad), None, "should reject {bad}");
    }
}
```

In `src/query/parse.rs`:

```rust
#[test]
fn relative_date_string_lowers_to_reldate_literal() {
    use crate::query::ast::{Literal, RelDate, DateUnit};
    let q = parse("SELECT file.name WHERE created >= '-7d'").unwrap();
    // dig the RHS literal out of the WHERE Compare — assert it is RelativeDate.
    // (helper: match q.filter -> Predicate::Compare(_, _, Expr::Lit(lit)))
    // A non-matching string stays Str:
    let q2 = parse("SELECT file.name WHERE status = 'draft'").unwrap();
    // assert that literal is Literal::Str("draft")
}
```

In `src/query/exec.rs`:

```rust
#[test]
fn relative_date_resolves_against_injected_now() {
    use std::time::{Duration, UNIX_EPOCH};
    // now = 2026-07-24T00:00:00Z ; '-7d' must resolve to "2026-07-17".
    let now = UNIX_EPOCH + Duration::from_secs(1_784_246_400); // 2026-07-24T00:00:00Z
    // build a store with one record { created: "2026-07-20" }, run
    // `SELECT file.name WHERE created >= '-7d'` via execute_at(.., now),
    // assert the record matches (2026-07-20 >= 2026-07-17).
    // Also run with created "2026-07-10" and assert it is filtered out.
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib query:: 2>&1 | tail -20`
Expected: FAIL / does-not-compile.

- [ ] **Step 3: Implement the AST + parser**

`src/query/ast.rs` — extend `Literal` and add types:

```rust
pub enum Literal { Str(String), Int(i64), Float(f64), Bool(bool), Null,
    RelativeDate(RelDate),
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RelDate { Today, Now, Offset { n: i64, unit: DateUnit } }
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DateUnit { Day, Week, Month, Year }

impl RelDate {
    /// Strict, anchored grammar: `today | now | [+-]<int>(d|w|mo|y)`,
    /// case-insensitive. Anything else is not a relative date.
    pub fn parse(s: &str) -> Option<RelDate> {
        let s = s.trim();
        match s.to_ascii_lowercase().as_str() {
            "today" => return Some(RelDate::Today),
            "now" => return Some(RelDate::Now),
            _ => {}
        }
        let (sign, rest) = match s.strip_prefix('-') {
            Some(r) => (-1, r),
            None => (1, s.strip_prefix('+')?),
        };
        let (digits, unit) = if let Some(d) = rest.strip_suffix("mo") {
            (d, DateUnit::Month)
        } else if let Some(d) = rest.strip_suffix('d') {
            (d, DateUnit::Day)
        } else if let Some(d) = rest.strip_suffix('w') {
            (d, DateUnit::Week)
        } else if let Some(d) = rest.strip_suffix('y') {
            (d, DateUnit::Year)
        } else {
            return None;
        };
        let n: i64 = digits.parse().ok()?;
        Some(RelDate::Offset { n: sign * n, unit })
    }
}
```

`literal_label`: `Literal::RelativeDate(rd) => format!("'{}'", reldate_source(rd)),` where `reldate_source` renders back the token (`today`/`now`/`-7d`).

`src/query/parse.rs` — where a single-quoted string literal is lowered to `Literal::Str(s)`, first try `RelDate::parse(&s)`:

```rust
match RelDate::parse(&s) {
    Some(rd) => Ok(Literal::RelativeDate(rd)),
    None => Ok(Literal::Str(s)),
}
```

- [ ] **Step 4: Implement the exec seam + rewrite**

`src/query/exec.rs`:
- Split `execute` into a public `execute(query, store, wanted)` that calls `execute_at(query, store, wanted, SystemTime::now())`, and the inner `execute_at`.
- At the top of `execute_at`, run a rewrite that walks the (mutable clone of the) `Query` and replaces every `Literal::RelativeDate(rd)` with `Literal::Str(resolve_reldate(rd, now))`. Then the rest of the pipeline is unchanged.

```rust
use chrono::{DateTime, Datelike, Duration, Months, SecondsFormat, Utc};

fn resolve_reldate(rd: RelDate, now: SystemTime) -> String {
    let now_utc: DateTime<Utc> = now.into();
    match rd {
        RelDate::Now => now_utc.to_rfc3339_opts(SecondsFormat::Secs, true),
        RelDate::Today => now_utc.date_naive().format("%Y-%m-%d").to_string(),
        RelDate::Offset { n, unit } => {
            let d = now_utc.date_naive();
            let shifted = match unit {
                DateUnit::Day => d + Duration::days(n),
                DateUnit::Week => d + Duration::weeks(n),
                DateUnit::Month if n >= 0 => d + Months::new(n as u32),
                DateUnit::Month => d - Months::new((-n) as u32),
                DateUnit::Year if n >= 0 => d + Months::new(12 * n as u32),
                DateUnit::Year => d - Months::new(12 * (-n) as u32),
            };
            shifted.format("%Y-%m-%d").to_string()
        }
    }
}
```

Walk every `Literal` position (WHERE `Compare` operand literals, `In` lists, `MemberOf`, `HAVING` — the rewrite is position-agnostic; a small recursive walk over the `Query`'s `Predicate`/`Expr`/`Having` trees). Keep it in one `rewrite_relative_dates(&mut Query, now)` helper.

- [ ] **Step 5: Run tests + clippy + commit**

Run: `cargo test 2>&1 | tail -20` (PASS)

```bash
cargo clippy --all-targets 2>&1 | tail -5
git add src/query/ast.rs src/query/parse.rs src/query/exec.rs
git commit -m "feat(query): relative-date literals ('-7d','today','now') resolved at exec (W29)"
```

---

## Task 6: `COALESCE(...)`

**Files:**
- Modify: `src/query/ast.rs` (`Expr::Coalesce`, `collect_expr_fields`, `expr_label`)
- Modify: `src/query/parse.rs` (`lower_expr` function arm)
- Modify: `src/query/exec.rs` (`eval_expr`, `expr_columns`)
- Test: `src/query/exec.rs`, `src/query/parse.rs`, `src/query/ast.rs` tests

**Interfaces:**
- Produces: `Expr::Coalesce(Vec<Expr>)`.

- [ ] **Step 1: Write the failing tests**

In `src/query/exec.rs`:

```rust
#[test]
fn coalesce_returns_first_non_null() {
    // record { jira: "DCP-1", epic: null } ; SELECT COALESCE(epic, jira, 'none')
    // -> "DCP-1". record with epic + jira null -> 'none'. all null -> Null.
    // (use the existing single-record exec test harness)
}
```

In `src/query/parse.rs`:

```rust
#[test]
fn coalesce_parses_and_references_all_columns() {
    let q = parse("SELECT COALESCE(epic, 'none') AS e").unwrap();
    assert!(q.referenced_fields().contains("epic"));
    assert_eq!(q.select[0].header(), "e");
    // zero-arg is an error
    assert!(parse("SELECT COALESCE() AS e").is_err());
    // aggregate inside coalesce rejected
    assert!(parse("SELECT COALESCE(count(*), 0)").is_err());
}

#[test]
fn coalesce_default_header() {
    let q = parse("SELECT COALESCE(epic, 'none')").unwrap();
    assert_eq!(q.select[0].header(), "coalesce(epic, 'none')");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib query:: 2>&1 | tail -20`
Expected: FAIL / does-not-compile.

- [ ] **Step 3: Implement**

`src/query/ast.rs`:
- Add `Coalesce(Vec<Expr>)` to `enum Expr`.
- `collect_expr_fields`: `Expr::Coalesce(args) => for a in args { collect_expr_fields(a, fields) }`.
- `expr_label`: `Expr::Coalesce(args) => format!("coalesce({})", args.iter().map(expr_label).collect::<Vec<_>>().join(", "))`.

`src/query/parse.rs` — in `lower_expr`'s `sql::Expr::Function` arm, before the scalar-fn dispatch, match a function named `coalesce` (case-insensitive):

```rust
if name.eq_ignore_ascii_case("coalesce") {
    let args = /* the function's unnamed arg exprs */;
    if args.is_empty() {
        return Err(unsupported("coalesce() requires at least one argument"));
    }
    let lowered = args.iter().map(lower_expr).collect::<Result<Vec<_>, _>>()?;
    return Ok(Expr::Coalesce(lowered));
}
```
(An aggregate argument routes through `lower_expr`, which rejects a bare aggregate as an expression — reuse the existing "aggregate not valid here" path so `COALESCE(count(*), 0)` errors.)

`src/query/exec.rs`:
- `eval_expr`: `Expr::Coalesce(args) => { for a in args { let v = eval_expr(record, a); if !v.is_null() { return v; } } Value::Null }`.
- `expr_columns`: `Expr::Coalesce(args) => args.iter().flat_map(expr_columns).collect()`.

- [ ] **Step 4: Run tests + clippy + commit**

Run: `cargo test 2>&1 | tail -20` (PASS)

```bash
cargo clippy --all-targets 2>&1 | tail -5
git add src/query/ast.rs src/query/parse.rs src/query/exec.rs
git commit -m "feat(query): COALESCE(...) variadic null-coalescing SELECT expression (W22)"
```

---

## Task 7: Subtree-scoped cache load

**Files:**
- Modify: `src/cache.rs` (`load_cache` scoped variant + scoped refresh)
- Modify: `src/store.rs` (`from_cache` subtree param; schema from loaded records)
- Modify: `src/main.rs` (pass subtree; drop post-hoc `retain_under` on scoped path)
- Test: `src/cache.rs`, `src/store.rs` tests

**Interfaces:**
- Consumes: `ManifestEntry { dir: PathBuf, blob: String, .. }`, `refresh_subtree`.
- Produces: `cache::load_cache_under(vault_dir, subtree: Option<&Path>)` (or a subtree param on `load_cache`); `InMemoryStore::from_cache(..., scope: Option<&[PathBuf]>)`.

- [ ] **Step 1: Write the failing tests**

In `src/cache.rs`:

```rust
#[test]
fn load_cache_under_skips_out_of_subtree_blobs() {
    // build a cache with dirs `plans/` and `product/`. Delete/corrupt the
    // product/ blob FILE on disk, then load_cache_under(vault, Some("plans")).
    // It must succeed and return only the plans/ CachedDir — proving the
    // product/ blob was never read.
}
```

In `src/store.rs`:

```rust
#[test]
fn scoped_load_matches_whole_vault_then_retain() {
    // For an in-subtree query, from_cache(vault, scope=Some([plans]))
    // yields the SAME records as from_cache(vault, None) + retain_under([plans]).
}

#[test]
fn scoped_schema_is_subtree_only() {
    // A field present only under product/ is NOT in schema() when scoped to
    // plans/, so a default-mode query for it errors (unknown column);
    // --lenient bypasses. (Assert schema() contents.)
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib cache:: store:: 2>&1 | tail -20`
Expected: FAIL / does-not-compile.

- [ ] **Step 3: Implement**

`src/cache.rs` — add a scoped loader that filters manifest entries before reading blobs:

```rust
/// Like `load_cache`, but when `subtree` is `Some`, only entries whose `dir`
/// is at/under `subtree` are read + decoded — out-of-subtree blob files are
/// never touched.
pub fn load_cache_under(
    vault_dir: &Path,
    subtree: Option<&Path>,
) -> Option<(ManifestBody, Vec<CachedDir>)> {
    let cache_dir = vault_dir.join(CACHE_DIR_NAME);
    let manifest_bytes = fs::read(cache_dir.join(MANIFEST_FILE_NAME)).ok()?;
    let body = read_manifest_bytes(&manifest_bytes)?;
    let loaded = body
        .dirs
        .iter()
        .filter(|e| subtree.is_none_or(|s| e.dir.starts_with(s)))
        .filter_map(|entry| {
            let bytes = fs::read(cache_dir.join(&entry.blob)).ok()?;
            decode::<CachedDir>(&bytes)
        })
        .collect();
    Some((body, loaded))
}
```
Keep `load_cache` as `load_cache_under(vault_dir, None)`. Scope the freshness re-walk the same way (pass `subtree` into the refresh path — reuse `refresh_subtree`'s `starts_with` scoping instead of walking the whole vault).

`src/store.rs` — `from_cache` gains a `scope: Option<&[PathBuf]>` param: when `Some`, call the scoped loader + scoped refresh; the store's schema is derived from the (scoped) loaded records, as it already is. When `None`, behavior is unchanged.

`src/main.rs` — in the query path (around the current `from_cache` + `retain_under` at lines ~616/636): when `dirs`/FROM name a subtree, pass the canonicalized subtree(s) as `scope` into `from_cache` and DROP the subsequent `retain_under`. When there is no subtree, keep the whole-vault path. The REPL path (whole-vault, long-lived store) is untouched.

- [ ] **Step 4: Run tests + clippy + commit**

Run: `cargo test 2>&1 | tail -20` (PASS)

```bash
cargo clippy --all-targets 2>&1 | tail -5
git add src/cache.rs src/store.rs src/main.rs
git commit -m "perf(cache): subtree-scoped load — decode only in-subtree blobs (W26)"
```

---

## Task 8: Documentation

**Files:**
- Modify: `README.md`

**Interfaces:** none (docs only).

- [ ] **Step 1: Update README**

Document, in the existing query-surface section(s):
- **Nested frontmatter:** dotted-path columns (`SELECT estimate.low WHERE estimate.high > 10`); nested maps render compactly in table/CSV and as real objects in `-o json`.
- **File metadata:** `file.mtime` (ISO-8601 UTC string) and `file.size` (bytes), e.g. `WHERE file.mtime >= '-7d' ORDER BY file.mtime DESC`.
- **Relative-date literals:** quoted `today` / `now` / `[+-]N(d|w|mo|y)` (e.g. `'-7d'`, `'-2mo'`), assuming ISO-8601 dates in frontmatter.
- **COALESCE:** `SELECT jira, COALESCE(epic, 'none') AS epic`.
- **Subtree-scoped load (behavior note):** a query scoped to a subtree (`FROM`/`[DIRS]`) validates unknown columns against that subtree's schema, not the whole vault; `--lenient` bypasses validation.

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: nested/dotted queries, file.mtime/size, relative dates, COALESCE, subtree-load note"
```

---

## Self-Review (completed by planner)

**Spec coverage:** W25 → T1+T2 (type/render) + T3 (paths); W24 → T4; W29 → T5; W22 → T6; W26 → T7; docs → T8. All spec §3–§7 covered. §9 invariants each map to a named test (compact-render characterization T1/T2; two producers T4; ISO lexicographic T4/T5; referenced_fields top-level T3; scoped==retain T7; strict RelDate::parse T5).

**Placeholder scan:** Step 5/6 store+cache producer tests and the exec harness tests reference "the existing test harness" — this is a real, discoverable fixture, not a placeholder; the code the engineer must write (behavior + assertions) is specified. All type/method names are defined where introduced.

**Type consistency:** `Record::field(&[String])`, `Record::new(root,path,fields,mtime,size)`, `ColRef::Field(Vec<String>)`, `Literal::RelativeDate(RelDate)`, `RelDate::{Today,Now,Offset{n,unit}}`, `DateUnit::{Day,Week,Month,Year}`, `Expr::Coalesce(Vec<Expr>)`, `execute_at(..,now)`, `load_cache_under(vault,subtree)`, `from_cache(..,scope)` — used consistently across tasks.
