# Query-language Expansion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Widen querymatter's SQL subset with an expression tree (scalar functions, arithmetic, concat), `MEMBER OF`, `HAVING`, GROUP BY aliases, `DISTINCT`, `ORDER BY` aggregate, unknown-column validation, and friendly error messages — all inside `src/query/`, plus a `--lenient` setting.

**Architecture:** A new `Expr` AST replaces the bare-`ColRef` operand positions in `SelectItem` and `Predicate::Compare`, evaluated by `apply_scalar`/`apply_binary` in `exec.rs`. The remaining items reuse AST nodes `sqlparser` already produces but querymatter currently rejects (HAVING, DISTINCT, MEMBER OF). Column validation runs once at the top of `execute` using a new `Query::referenced_fields()` helper (also consumed by sub-project 4).

**Tech Stack:** Rust edition 2024, `sqlparser` 0.62 (GenericDialect), existing `Value`/`Record`/`RecordStore`, `insta`, `assert_cmd`.

**Spec:** `docs/superpowers/specs/2026-07-24-query-language-design.md`

## Global Constraints

- Edition 2024. Every file stays `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean, with no `#[allow(...)]`.
- **Default render output must not change by a byte.** The committed snapshots `src/snapshots/querymatter__render__tests__table_snapshot.snap` and `..._md_snapshot.snap` stay byte-identical to `main`; verify `git diff main -- src/snapshots/` is empty before every commit.
- **Behavior preservation:** every query that worked before this batch still works and returns the same result. The pre-existing `src/query/` test suite is the regression guard; a pre-existing test that changes is a red flag.
- Binary-only crate (no `src/lib.rs`): use `cargo test <filter>`, never `cargo test --lib`. `cargo-insta` is not installed; accept snapshots with `INSTA_UPDATE=always cargo test`.
- No pre-commit hook; run fmt + clippy yourself before each commit.
- SQL null semantics are 3-valued (`Option<bool>`, `None` = unknown); every new predicate/operand follows the existing `eval_predicate` model. A `Null`/absent/typed-mismatch operand yields `Null` (scalar) or unknown (predicate), never a panic.
- Scalar function names and their spellings: `lower, upper, length, trim, ltrim, rtrim, substr, replace`. Arithmetic ops: `+ - * / %` and concat `||`.
- Unknown-column validation is **error by default, `--lenient` restores unknown→Null**, and is **skipped when the store has zero records**.

---

### Task 1: Friendly "not supported" messages (W7)

Isolated warm-up in `parse.rs`: replace `{:?}` AST dumps in `Unsupported` fallbacks with clean phrases.

**Files:**
- Modify: `src/query/parse.rs` (the fallback arms + tests)

**Interfaces:**
- Consumes: the existing `unsupported(what: impl Into<String>) -> ParseError` helper (`parse.rs:559`).
- Produces: no new API; only message text changes.

- [ ] **Step 1: Write the failing test**

Add to `parse.rs`'s test module:

```rust
    /// Unsupported constructs must produce a human phrase, never a raw
    /// sqlparser AST Debug dump (which contains struct-literal braces).
    #[test]
    fn unsupported_messages_have_no_ast_debug_dump() {
        // A CAST in WHERE, a subquery, and a non-literal IN value all route
        // through catch-all arms that used to `{:?}` the node.
        for sql in [
            "SELECT status WHERE CAST(prd AS INT) = 1",
            "SELECT status WHERE status IN (SELECT status)",
            "SELECT status WHERE status = status + 1",
        ] {
            let err = crate::query::parse(sql).unwrap_err();
            let msg = err.to_string();
            assert!(
                !msg.contains('{') && !msg.contains('}'),
                "message leaked an AST dump for {sql:?}: {msg}"
            );
            assert!(
                msg.to_lowercase().contains("support"),
                "message should say what isn't supported for {sql:?}: {msg}"
            );
        }
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test parse::tests::unsupported_messages_have_no_ast_debug_dump`
Expected: FAIL — at least one message contains `{`/`}`.

- [ ] **Step 3: Replace the `{:?}` fallbacks**

In `parse.rs`, find every `unsupported(format!("… {other:?}"))` (and any bare `format!("{x:?}")` feeding `unsupported`) — the WHERE-expression, value-literal, query-body, and count-argument fallbacks. Replace each with a fixed human phrase, e.g.:
- WHERE fallback → `unsupported("this WHERE expression")`
- value literal → `unsupported("this literal value")`
- query body → `unsupported("this query form")`
- count argument → `unsupported("this count(...) argument")`

Grep to be exhaustive: `grep -n '{other:?}\|:?})' src/query/parse.rs`. No `{:?}` of a `sqlparser` node may remain in any `unsupported(...)` path.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test parse::` — the new test passes and every pre-existing parse test still passes.

- [ ] **Step 5: fmt, clippy, snapshot guard, commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git diff main -- src/snapshots/   # must be empty
git add src/query/parse.rs
git commit -m "feat(query): replace unsupported-clause AST dumps with plain phrases"
```

---

### Task 2: Expression tree in SELECT — scalar functions, arithmetic, concat (W30 + W14)

The foundational task. Introduces `Expr`, `ScalarFn`, `BinOp`, their evaluators, and rewires `SelectExpr`.

**Files:**
- Modify: `src/query/ast.rs` (new types; `SelectExpr::Col` → `SelectExpr::Expr`)
- Modify: `src/query/parse.rs` (`lower_select_expr`, new `lower_expr`, arity checks)
- Modify: `src/query/exec.rs` (`apply_scalar`, `apply_binary`, `eval_expr`; `expand_select`/`project_group` use it)

**Interfaces:**
- Consumes: `Literal`, `ColRef`, `Value`, `Record`, `resolve_col` (`exec.rs:435`).
- Produces:
  - `pub enum Expr { Col(ColRef), Lit(Literal), Scalar(ScalarFn, Vec<Expr>), Binary(BinOp, Box<Expr>, Box<Expr>) }` — `Debug, Clone, PartialEq`.
  - `pub enum ScalarFn { Lower, Upper, Length, Trim, Ltrim, Rtrim, Substr, Replace }` — same derives.
  - `pub enum BinOp { Add, Sub, Mul, Div, Mod, Concat }` — same derives.
  - `pub enum SelectExpr { Star, Expr(Expr), Agg(Aggregate) }` (the `Col(ColRef)` variant is gone; a bare column is `Expr(Expr::Col(_))`).
  - `exec::eval_expr(record: &Record, expr: &Expr) -> Value` (crate-visible; used by later tasks).
  - `exec::apply_scalar(f: ScalarFn, args: &[Value]) -> Value` and `exec::apply_binary(op: BinOp, l: &Value, r: &Value) -> Value`.

- [ ] **Step 1: Write the failing exec unit tests**

Add to `exec.rs`'s test module:

```rust
    use crate::query::ast::{BinOp, Expr, ScalarFn};
    use crate::model::Value;

    #[test]
    fn scalar_string_functions() {
        let s = |t: &str| Value::Str(t.into());
        assert_eq!(apply_scalar(ScalarFn::Lower, &[s("DrAfT")]), s("draft"));
        assert_eq!(apply_scalar(ScalarFn::Upper, &[s("draft")]), s("DRAFT"));
        assert_eq!(apply_scalar(ScalarFn::Length, &[s("héllo")]), Value::Int(5));
        assert_eq!(apply_scalar(ScalarFn::Trim, &[s("  x  ")]), s("x"));
        assert_eq!(apply_scalar(ScalarFn::Ltrim, &[s("  x  ")]), s("x  "));
        assert_eq!(apply_scalar(ScalarFn::Rtrim, &[s("  x  ")]), s("  x"));
        assert_eq!(apply_scalar(ScalarFn::Substr, &[s("abcdef"), Value::Int(2), Value::Int(3)]), s("bcd"));
        assert_eq!(apply_scalar(ScalarFn::Substr, &[s("abcdef"), Value::Int(4)]), s("def"));
        assert_eq!(apply_scalar(ScalarFn::Replace, &[s("a-b-c"), s("-"), s("_")]), s("a_b_c"));
    }

    #[test]
    fn scalar_null_propagates_and_stringifies_numbers() {
        assert_eq!(apply_scalar(ScalarFn::Lower, &[Value::Null]), Value::Null);
        // a non-string arg stringifies first (same conversion the renderer uses)
        assert_eq!(apply_scalar(ScalarFn::Length, &[Value::Int(100)]), Value::Int(3));
    }

    #[test]
    fn substr_clamps_out_of_range() {
        let s = |t: &str| Value::Str(t.into());
        assert_eq!(apply_scalar(ScalarFn::Substr, &[s("abc"), Value::Int(10)]), s(""));
        assert_eq!(apply_scalar(ScalarFn::Substr, &[s("abc"), Value::Int(1), Value::Int(99)]), s("abc"));
    }

    #[test]
    fn arithmetic_types_and_null_safety() {
        assert_eq!(apply_binary(BinOp::Add, &Value::Int(2), &Value::Int(3)), Value::Int(5));
        assert_eq!(apply_binary(BinOp::Div, &Value::Int(3), &Value::Int(2)), Value::Float(1.5));
        assert_eq!(apply_binary(BinOp::Mul, &Value::Int(2), &Value::Float(1.5)), Value::Float(3.0));
        assert_eq!(apply_binary(BinOp::Div, &Value::Int(1), &Value::Int(0)), Value::Null);
        assert_eq!(apply_binary(BinOp::Mod, &Value::Int(1), &Value::Int(0)), Value::Null);
        assert_eq!(apply_binary(BinOp::Add, &Value::Null, &Value::Int(1)), Value::Null);
        assert_eq!(apply_binary(BinOp::Add, &Value::Str("x".into()), &Value::Int(1)), Value::Null);
    }

    #[test]
    fn concat_joins_and_propagates_null() {
        assert_eq!(
            apply_binary(BinOp::Concat, &Value::Str("a".into()), &Value::Int(1)),
            Value::Str("a1".into())
        );
        assert_eq!(apply_binary(BinOp::Concat, &Value::Str("a".into()), &Value::Null), Value::Null);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test exec::tests::scalar_string_functions`
Expected: FAIL — `apply_scalar`/`apply_binary`/`ScalarFn`/`BinOp` not found.

- [ ] **Step 3: Add the AST types**

In `ast.rs`, add `Expr`, `ScalarFn`, `BinOp` (per the Interfaces block) and change `SelectExpr`:

```rust
pub enum SelectExpr {
    /// The `*` wildcard, expanding to every frontmatter field.
    Star,
    /// A scalar expression: a column, literal, scalar-fn call, or arithmetic.
    Expr(Expr),
    /// An aggregate over the current group.
    Agg(Aggregate),
}
```

- [ ] **Step 4: Implement the evaluators in exec.rs**

Add `eval_expr`, `apply_scalar`, `apply_binary`. `eval_expr` recurses: `Col` → `resolve_col`, `Lit` → the literal's `Value`, `Scalar` → eval args then `apply_scalar`, `Binary` → eval both sides then `apply_binary`. Stringification for scalar/concat args uses the same conversion `Value::display` uses (extract or reuse it so a field renders identically bare or wrapped). Number coercion for arithmetic mirrors the numeric coercion `eval_compare`/`apply_cmp` already do. Follow the null/typed-mismatch rules from the spec §2.2–2.3.

- [ ] **Step 5: Rewire the SELECT lowering and projection**

`lower_select_expr` (`parse.rs:205`): a `Function` that is an aggregate stays `SelectExpr::Agg`; otherwise lower via a new `lower_expr` that produces `Expr` (handling `sqlparser` `Identifier`/`CompoundIdentifier` → `Expr::Col`, `Value` → `Expr::Lit`, `Function` (scalar names) → `Expr::Scalar` with an arity check, `BinaryOp` (`+ - * / %`, `StringConcat`) → `Expr::Binary`). An aggregate nested inside an expression (e.g. `count(*) + 1`) is `unsupported("an aggregate inside an expression")`.

In `exec.rs`, the ungrouped projection (`expand_select` path) and grouped projection (`project_group`) must evaluate `SelectExpr::Expr(e)` via `eval_expr` on the row (ungrouped) or the group's representative row for a grouping-key expression (grouped). Preserve the existing column-header naming: a bare `Expr::Col` keeps its field/`file.*` label; a computed expression's header is its alias if present, else a rendered form of the expression (define a small `expr_label(&Expr) -> String`).

- [ ] **Step 6: Round-trip integration tests**

Add to `exec.rs` tests (using the module's existing record-building helpers — read them first):

```rust
    #[test]
    fn select_scalar_and_arithmetic_round_trip() {
        // build records with fields e.g. status="Draft", a=3, b=2
        // SELECT lower(status)  -> "draft"
        // SELECT (a / b) AS r    -> 1.5
        // SELECT a || '-' || status -> "3-Draft"
        // (assert against ResultTable rows)
    }
```

Fill in with the module's real record helpers and `execute`/`parse` entry points.

- [ ] **Step 7: fmt, clippy, snapshot guard, full test, commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git diff main -- src/snapshots/   # empty
git add src/query/
git commit -m "feat(query): expression tree in SELECT — scalar fns, arithmetic, concat"
```

---

### Task 3: Expressions in WHERE — column-to-column and scalar comparisons (W30 cont.)

**Files:**
- Modify: `src/query/ast.rs` (`Predicate::Compare` operands)
- Modify: `src/query/parse.rs` (`lower_binary`/comparison lowering)
- Modify: `src/query/exec.rs` (`eval_predicate` Compare arm, `eval_compare`)

**Interfaces:**
- Consumes: `Expr`, `eval_expr`, `apply_cmp` from Task 2 and existing.
- Produces: `Predicate::Compare(Expr, CmpOp, Expr)` (was `(ColRef, CmpOp, Literal)`).

- [ ] **Step 1: Failing tests**

```rust
    #[test]
    fn where_column_to_column_and_scalar() {
        // records: {start: 1, end: 5}, {start: 5, end: 5}
        // WHERE start < end  -> only the first row
        // WHERE lower(status) = 'draft' with status "Draft" -> matches
        // WHERE start + 1 = end with start=4,end=5 -> matches
    }

    #[test]
    fn where_null_operand_is_unknown_not_match() {
        // WHERE missingfield = status  -> no rows (unknown, not error) under --lenient;
        // (validation is Task 9; here use a field present in some records)
    }
```

- [ ] **Step 2–4:** Change `Predicate::Compare` to hold two `Expr`s. Update `lower_binary` (`parse.rs:367`) so both comparison sides lower through `lower_expr` (Task 2) instead of `lower_col_ref`/`lower_literal`. Update `eval_predicate`'s Compare arm (`exec.rs`) to `eval_expr` both sides and compare via the existing ordering/`apply_cmp` logic, keeping 3VL (`Null` on either side → unknown). Update all pre-existing `Predicate::Compare(col, op, lit)` constructions in tests to the new shape. Run tests.

- [ ] **Step 5:** fmt, clippy, snapshot guard (empty), commit `feat(query): expressions on both sides of a WHERE comparison`.

---

### Task 4: `MEMBER OF` list membership (W1)

**Files:** `src/query/ast.rs`, `src/query/parse.rs`, `src/query/exec.rs`.

**Interfaces:**
- Produces: `Predicate::MemberOf(Literal, ColRef, /* negated */ bool)`.

- [ ] **Step 1: Failing test**

```rust
    #[test]
    fn member_of_list_field() {
        // record tags = Value::List([Str("mobile"), Str("backend")])
        // WHERE 'mobile' MEMBER OF(tags)   -> matches
        // WHERE 'ios' MEMBER OF(tags)      -> no match
        // WHERE 'ios' NOT MEMBER OF(tags)  -> matches
        // a record with tags = Null       -> unknown (not matched)
        // a record with tags = Str("x")   -> unknown (non-list)
    }
```

- [ ] **Steps 2–4:** Add the `MemberOf` variant. In `lower_predicate` (`parse.rs:333`) add an arm for `sqlparser`'s `Expr::MemberOf { value, array }`: require `value` to lower to a `Literal` and `array` to a single `ColRef` (else `unsupported("this MEMBER OF form")`); wrap negation like `In`/`Like` do. In `eval_predicate`, resolve the column; if `Value::List`, membership is `iter().any(|el| element_equals(el, lit))` reusing `In`'s element-equality; a `Null` or non-list value → `None` (unknown). Run tests.

- [ ] **Step 5:** fmt/clippy/snapshot/commit `feat(query): MEMBER OF list-membership predicate`.

---

### Task 5: `HAVING` group filtering (W11)

**Files:** `src/query/ast.rs`, `src/query/parse.rs`, `src/query/exec.rs`.

**Interfaces:**
- Produces: `Query.having: Option<Having>`, `enum Having { Compare(HavingLeaf, CmpOp, Literal), And/Or/Not(Box<Having>...) }`, `enum HavingLeaf { Group(ColRef), Agg(Aggregate) }`.

- [ ] **Step 1: Failing test**

```rust
    #[test]
    fn having_filters_groups() {
        // records with status draft x2, synced x1
        // SELECT status, count(*) AS n GROUP BY status HAVING count(*) > 1
        //   -> only the draft group (n=2)
    }
    #[test]
    fn having_can_reference_aggregate_not_selected() {
        // SELECT status GROUP BY status HAVING count(*) > 1 -> only draft
    }
    #[test]
    fn having_without_group_by_errors() {
        assert!(crate::query::parse("SELECT status HAVING count(*) > 1").is_err());
    }
```

- [ ] **Steps 2–4:** Add `Having`/`HavingLeaf` and `Query.having`. Remove HAVING from `reject_unsupported_select_clauses` (`parse.rs:128`); add `lower_having(expr, &select_items)` that builds a `Having` tree, lowering a comparison's aggregate side via `lower_aggregate` and a column side via `lower_col_ref` (validated to be a grouping key). HAVING present on an ungrouped query → `unsupported("HAVING requires GROUP BY")`. In `execute_grouped` (`exec.rs:135`), after `project_group`/`compute_aggregate` and before ORDER BY/LIMIT, keep only groups where the `Having` evaluates *true* (compute referenced aggregates from the group's rows via `compute_aggregate`; a grouping-key leaf resolves from the group key). Run tests.

- [ ] **Step 5:** commit `feat(query): HAVING for group-level filtering`.

---

### Task 6: SELECT aliases in `GROUP BY` (W8)

**Files:** `src/query/parse.rs` (lowering only).

- [ ] **Step 1: Failing test**

```rust
    #[test]
    fn group_by_resolves_select_alias() {
        // SELECT status AS s, count(*) AS n GROUP BY s ORDER BY s
        //  -> groups by status (same as GROUP BY status)
    }
    #[test]
    fn group_by_alias_on_aggregate_or_expr_is_rejected() {
        assert!(crate::query::parse("SELECT count(*) AS n GROUP BY n").is_err());
    }
```

- [ ] **Steps 2–4:** Thread `select_items` (or the `aliases` list already built in `lower_query`, `parse.rs:66`) into `lower_group_by` (`parse.rs:466`). When a bare GROUP BY identifier equals a SELECT alias whose item is a bare `Expr(Expr::Col(c))`, resolve to `c`; an alias on an aggregate or computed expression is not a grouping key → error. Otherwise fall back to the current column lowering. Exec unchanged. Run tests.

- [ ] **Step 5:** commit `feat(query): resolve SELECT aliases in GROUP BY`.

---

### Task 7: `DISTINCT` on the projection (W19)

**Files:** `src/query/ast.rs`, `src/query/parse.rs`, `src/query/exec.rs`.

**Interfaces:**
- Produces: `Query.distinct: bool`.

- [ ] **Step 1: Failing test**

```rust
    #[test]
    fn distinct_dedups_projection() {
        // records with folder a,a,b  ->  SELECT DISTINCT file.folder  yields 2 rows
    }
    #[test]
    fn distinct_with_group_by_is_rejected() {
        assert!(crate::query::parse("SELECT DISTINCT status, count(*) GROUP BY status").is_err());
    }
```

- [ ] **Steps 2–4:** Add `Query.distinct`. Remove DISTINCT from `reject_unsupported_select_clauses`; set the flag from `select.distinct` (only the plain `Distinct::Distinct` form — `DISTINCT ON (...)` stays `unsupported`). `DISTINCT` with a non-empty `GROUP BY` → `unsupported("DISTINCT combined with GROUP BY")`. In `execute_ungrouped` (`exec.rs:86`), after projection and before the ORDER BY sort, dedup rows keyed on their cells joined via `to_cmp_string()` (preserve first-occurrence order). Run tests.

- [ ] **Step 5:** commit `feat(query): DISTINCT projection dedup`.

---

### Task 8: `ORDER BY` a bare aggregate (W20)

**Files:** `src/query/ast.rs`, `src/query/parse.rs`, `src/query/exec.rs`.

**Interfaces:**
- Produces: an `OrderKey` target variant for an aggregate (extend the existing `OrderKey`/its target enum — read `ast.rs` for its current shape and add `Agg(Aggregate)`).

- [ ] **Step 1: Failing test**

```rust
    #[test]
    fn order_by_bare_aggregate() {
        // SELECT status, count(*) GROUP BY status ORDER BY count(*) DESC
        //  -> groups sorted by count desc, no AS alias needed
    }
    #[test]
    fn order_by_aggregate_ungrouped_errors() {
        assert!(crate::query::parse("SELECT status ORDER BY count(*)").is_err());
    }
```

- [ ] **Steps 2–4:** In `lower_order_expr` (`parse.rs:498`), before the alias/`lower_col_ref` fallback, recognize a `Function` order expression and lower it via `lower_aggregate` into the new aggregate `OrderKey` target. In the grouped-order resolution (`exec.rs` — read `execute_grouped`'s ordering step), match an aggregate order target structurally against the group's aggregates, computing it if not already a SELECT item. An aggregate `ORDER BY` with no GROUP BY → `unsupported("ORDER BY an aggregate requires GROUP BY")`. Run tests.

- [ ] **Step 5:** commit `feat(query): ORDER BY a bare aggregate`.

---

### Task 9: Unknown-column validation + `referenced_fields` + `--lenient` (W12)

**Files:**
- Modify: `src/query/ast.rs` (`Query::referenced_fields`)
- Modify: `src/query/exec.rs` (validation in `execute`, `ExecError` variant)
- Modify: `src/query/mod.rs` (thread a `lenient` flag / schema into `execute`)
- Modify: `src/cli.rs` (`--lenient`), `src/settings.rs` + `src/config.rs` (`lenient` setting + `ConfigKey::Lenient`), `src/session.rs` (pass it through)

**Interfaces:**
- Consumes: the `Expr`/`Having`/`MemberOf` AST from Tasks 2–8; `RecordStore::schema()`.
- Produces:
  - `impl Query { pub fn referenced_fields(&self) -> std::collections::BTreeSet<String> }` — every `ColRef::Field` name across SELECT (incl. nested expr/agg args), WHERE, GROUP BY, ORDER BY, HAVING, MEMBER OF; excludes `file.*` and `*`.
  - `execute(query, records, lenient: bool)` (or a schema/opts param) validating unknown columns unless `lenient` or the record set is empty.
  - `ExecError::UnknownColumn { name: String, suggestion: Option<String> }`.
  - `Cli::lenient: bool`; `ConfigKey::Lenient`; `Settings.lenient: Resolved<bool>`.

- [ ] **Step 1: Failing tests (helper + validation)**

```rust
    #[test]
    fn referenced_fields_covers_all_positions() {
        let q = crate::query::parse(
            "SELECT lower(a), count(b) WHERE c = 'x' GROUP BY a HAVING count(b) > 0 ORDER BY a"
        ).unwrap();
        let f = q.referenced_fields();
        for name in ["a", "b", "c"] { assert!(f.contains(name), "missing {name}"); }
    }

    #[test]
    fn unknown_column_errors_with_suggestion() {
        // records with field "status"; SELECT staus -> ExecError::UnknownColumn { suggestion: Some("status") }
    }

    #[test]
    fn lenient_restores_null_for_unknown_column() {
        // execute(..., lenient=true) -> unknown column renders as Null, no error
    }

    #[test]
    fn empty_store_skips_validation() {
        // zero records: SELECT anything -> Ok (no UnknownColumn), empty result
    }

    #[test]
    fn typo_inside_scalar_and_having_is_caught() {
        // SELECT lower(staus) -> UnknownColumn; SELECT a GROUP BY a HAVING count(staus)>0 -> UnknownColumn
    }
```

Plus an integration test in `tests/cli.rs`:

```rust
#[test]
fn unknown_column_exits_nonzero_with_suggestion() {
    let td = tree(); // existing helper
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, TempDir::new().unwrap().path()) // isolate config
        .args(["-e", "SELECT staus"]).arg(td.path())
        .assert().failure()
        .stderr(predicates::str::contains("staus"))
        .stderr(predicates::str::contains("status"));
}

#[test]
fn lenient_flag_allows_unknown_column() {
    let td = tree();
    let mut cmd = Command::cargo_bin("querymatter").unwrap();
    with_config_home(&mut cmd, TempDir::new().unwrap().path())
        .args(["-e", "SELECT staus", "--lenient"]).arg(td.path())
        .assert().success();
}
```

(Use the `qm`/`with_config_home` isolation helper already in `tests/cli.rs`.)

- [ ] **Steps 2–4:** Implement `Query::referenced_fields` (walk SELECT exprs incl. nested `Expr`/aggregate args, WHERE `Expr`s, GROUP BY, ORDER BY, HAVING leaves, MEMBER OF col). Add the `ExecError::UnknownColumn` variant with a `Display` naming the column and, when `suggestion` is `Some`, "did you mean '<x>'?". At the top of `execute`, when `!lenient` and `!records.is_empty()`, compute the schema (union of field names — reuse the store's schema if threaded in, else derive from the records), and for each `referenced_fields()` name not in the schema, return `UnknownColumn` with the nearest schema name by Levenshtein distance (≤2 or ≤⌈len/3⌉; write a small `nearest(name, &schema)` helper) or `None`. Thread `lenient` from `Cli`/`Settings` through `Session::run`/`render_statement` into `execute`. Add `--lenient` to `Cli`, `ConfigKey::Lenient` (updates the `ConfigKey::ALL`↔`ValueEnum` agreement test automatically — verify it), and `Settings.lenient` resolved like `hidden`.

- [ ] **Step 5:** Run `cargo test`; `git diff main -- src/snapshots/` empty; fmt + clippy clean. Commit `feat(query): validate unknown columns with --lenient escape hatch`.

---

### Task 10: Docs, final review, finish branch

- [ ] **Step 1:** Update `README.md`: the query-language surface (scalar functions with their names, arithmetic/concat, `MEMBER OF`, `HAVING`, GROUP BY aliases, `DISTINCT`, `ORDER BY` aggregate), the `--lenient` flag and `lenient` config key, and a note that an unknown column is now an error by default. Update any "SQL subset" description that enumerates the supported surface.

- [ ] **Step 2:** Full verification: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test`; `git diff main -- src/snapshots/` empty.

- [ ] **Step 3:** Dispatch the final whole-branch code reviewer, apply any pre-merge fixes, then finish the branch per `superpowers:finishing-a-development-branch` (merge to local `main`).
