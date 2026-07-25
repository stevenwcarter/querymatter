# W43 scalar-expr predicates + W47 streaming render — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Widen `LIKE`/`IN`/`IS NULL`/`MEMBER OF` to accept a scalar `Expr` on the tested side (W43), and stream `json`/`csv`/`tsv` rendering straight to the output writer instead of buffering a full `String` (W47).

**Architecture:** W43 mirrors the existing `Predicate::Regexp(Expr, …)` node end-to-end — the compiler forces every `match Predicate` arm to be updated. W47 adds `render::render_to(&mut impl Write, …)` (streams json/csv/tsv, buffers table/md/vertical), gives `OutputSink` a `write_result(|w| …)` primitive, and replaces `Session::render_statement_counted` (returns `String`) with `render_statement_to(statement, sink)` (writes into the sink, returns the row count).

**Tech Stack:** Rust (edition 2024), `sqlparser` (parse), `serde_json` + `csv` (render), `assert_cmd`/`predicates`/`tempfile` (integration tests). Binary-only crate.

## Global Constraints

- **Byte-identical output is sacred.** Every existing query result and rendered byte stays identical except where an item explicitly changes it. Piped/redirected output especially.
- **`serde_json` has NO `preserve_order` feature here** → its `Map` is a `BTreeMap`, so JSON objects emit **sorted** keys. Reproduce that exactly (reuse the per-row `Map` construction); never hand-roll `serialize_map` in header order.
- **csv's default record terminator is `\n`** — rely on it for the trailing newline; do not set a custom terminator.
- **Edition 2024 always.** Keep it clippy-clean and rustfmt-clean (`cargo clippy --all-targets`, `cargo fmt`). Binary-only crate: test with `cargo test` (there is no `cargo test --lib`). No pre-commit hook — run `cargo fmt` yourself before committing.
- **TDD:** failing test first (must actually fail/red), minimal implementation, green, commit.
- Do not touch the unchecked whats-next items. W47 does not make the result set out-of-core (that is W59). W43 does not widen the `MEMBER OF` array operand.

---

### Task 1: W43 — scalar `Expr` as the tested operand of LIKE / IN / IS NULL / MEMBER OF

Widening `Predicate::Like`/`In`/`IsNull`/`MemberOf` to carry an `Expr` is a single atomic compile unit: the AST change breaks `parse.rs`, `exec.rs`, and the `ast.rs` helpers simultaneously, so they are updated together. Red-first tests are CLI integration tests (they compile against the binary and fail at runtime today with "expected a column reference").

**Files:**
- Modify: `src/query/ast.rs` (the four `Predicate` variants + `collect_predicate_fields` + `predicate_label`)
- Modify: `src/query/parse.rs` (`lower_predicate`, `lower_member_of`, and the AST-shape unit tests)
- Modify: `src/query/exec.rs` (`predicate_columns`, `rewrite_predicate_literals`, `eval_predicate`)
- Test: `tests/cli.rs` (new integration tests), plus updated unit tests in `parse.rs`

**Interfaces:**
- Consumes: `lower_expr` (parse.rs, already used by `Compare`/`Regexp`), `eval_expr` / `expr_columns` / `rewrite_expr_literals` (exec.rs, already used by `Compare`/`Regexp`), `eval_compare` (exec.rs).
- Produces: `Predicate::Like(Expr, String, bool)`, `Predicate::In(Expr, Vec<Literal>, bool)`, `Predicate::IsNull(Expr, bool)`, `Predicate::MemberOf(Expr, ColRef, bool)`.

- [ ] **Step 1: Write the failing integration tests** in `tests/cli.rs` (append at end of file):

```rust
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

/// W43: IN and IS NULL accept a scalar expression on the tested side.
#[test]
fn where_scalar_expr_in_and_isnull() {
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

/// W43: MEMBER OF now accepts a bare column (and any expression) on the left —
/// impossible before (the left had to be a literal).
#[test]
fn where_column_member_of_list() {
    let home = TempDir::new().unwrap();
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("x.md", "---\nlead: mobile\ntags:\n  - mobile\n  - web\n---\n"),
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
        .stderr(predicate::str::contains("force-cache"));
}
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `cargo test --test cli where_scalar_expr_like where_scalar_expr_in_and_isnull where_column_member_of_list`
Expected: FAIL — today's binary rejects `lower(status) LIKE …` / `lower(status) IN …` and `lead MEMBER OF(tags)` (a column value) with a parse error ("expected a column reference" / "this MEMBER OF form"). (`force_cache_file_body_like_still_errors_after_widening` PASSES today — it's the characterization guard that must STAY green after the change.)

- [ ] **Step 3: Widen the four `Predicate` variants in `src/query/ast.rs`**

Change the variant definitions (keep the doc comments accurate — note the operand is now a general `Expr`, mirroring `Regexp`):

```rust
    /// `<expr> [NOT] LIKE '<pattern>'`; the `bool` is `true` when negated. The
    /// tested side is a general [`Expr`] (e.g. `lower(status) LIKE '%draft%'`),
    /// matching `Compare`/`Regexp`.
    Like(Expr, String, /* negated */ bool),
    /// `<expr> [NOT] REGEXP '<pattern>'` … (unchanged)
    Regexp(Expr, String, /* negated */ bool),
    /// `<expr> [NOT] IN (<literals>)`; the `bool` is `true` when negated.
    In(Expr, Vec<Literal>, /* negated */ bool),
    /// `<expr> MEMBER OF(col)` / `NOT <expr> MEMBER OF(col)`; the `bool` is
    /// `true` when negated. The tested value is a general [`Expr`] (a literal,
    /// a column, or a scalar call). `col` must resolve to a `Value::List`; a
    /// `Null`/non-list value makes the predicate unknown (see
    /// `exec::eval_predicate`).
    MemberOf(Expr, ColRef, /* negated */ bool),
    /// `<expr> IS [NOT] NULL`; the `bool` is `true` for `IS NOT NULL`.
    IsNull(Expr, /* negated */ bool),
```

Update `collect_predicate_fields` (same file) — Like/In/IsNull now walk an `Expr`, and MemberOf walks its value expr plus its array column:

```rust
        Predicate::Like(expr, _, _) | Predicate::In(expr, _, _) | Predicate::IsNull(expr, _) => {
            collect_expr_fields(expr, fields);
        }
        Predicate::Regexp(expr, _, _) => collect_expr_fields(expr, fields),
        Predicate::MemberOf(value, col, _) => {
            collect_expr_fields(value, fields);
            collect_col_field(col, fields);
        }
```

Update `predicate_label` (same file) — render the widened operands via `expr_label`:

```rust
        Predicate::Like(expr, pattern, negated) => format!(
            "{} {}like '{pattern}'",
            expr_label(expr),
            if *negated { "not " } else { "" }
        ),
        // Regexp arm unchanged (already uses expr_label)
        Predicate::In(expr, lits, negated) => {
            let rendered: Vec<String> = lits.iter().map(literal_label).collect();
            format!(
                "{} {}in ({})",
                expr_label(expr),
                if *negated { "not " } else { "" },
                rendered.join(", ")
            )
        }
        Predicate::MemberOf(value, col, negated) => format!(
            "{}{} member of({})",
            if *negated { "not " } else { "" },
            expr_label(value),
            col.label()
        ),
        Predicate::IsNull(expr, negated) => format!(
            "{} is {}null",
            expr_label(expr),
            if *negated { "not " } else { "" }
        ),
```

- [ ] **Step 4: Update lowering in `src/query/parse.rs`**

In `lower_predicate`, swap `lower_col_ref` → `lower_expr` for the four forms:

```rust
        sql::Expr::Like {
            negated, expr, pattern, ..
        } => Ok(Predicate::Like(
            lower_expr(expr)?,
            string_literal(pattern, "LIKE")?,
            *negated,
        )),
        // RLike arm unchanged (already lower_regexp -> lower_expr)
        sql::Expr::InList { expr, list, negated } => {
            let literals = list.iter().map(lower_literal).collect::<Result<_, _>>()?;
            Ok(Predicate::In(lower_expr(expr)?, literals, *negated))
        }
        sql::Expr::IsNull(inner) => Ok(Predicate::IsNull(lower_expr(inner)?, false)),
        sql::Expr::IsNotNull(inner) => Ok(Predicate::IsNull(lower_expr(inner)?, true)),
```

Rewrite `lower_member_of` to lower the value as an `Expr` (propagating each side's real error rather than the generic "this MEMBER OF form"):

```rust
fn lower_member_of(member_of: &sql::MemberOf, negated: bool) -> Result<Predicate, ParseError> {
    let value = lower_expr(&member_of.value)?;
    let col = lower_col_ref(&member_of.array)?;
    Ok(Predicate::MemberOf(value, col, negated))
}
```

- [ ] **Step 5: Update the executor in `src/query/exec.rs`**

`predicate_columns`:

```rust
        Predicate::Like(expr, _, _) | Predicate::In(expr, _, _) | Predicate::IsNull(expr, _) => {
            expr_columns(expr)
        }
        Predicate::Regexp(expr, _, _) => expr_columns(expr),
        Predicate::MemberOf(value, col, _) => {
            let mut cols = expr_columns(value);
            cols.push(col);
            cols
        }
```

`rewrite_predicate_literals` (the widened operands can now carry relative-date literals):

```rust
        Predicate::Compare(l, _, r) => {
            rewrite_expr_literals(l, now);
            rewrite_expr_literals(r, now);
        }
        Predicate::Like(expr, _, _) | Predicate::IsNull(expr, _) => rewrite_expr_literals(expr, now),
        Predicate::In(expr, literals, _) => {
            rewrite_expr_literals(expr, now);
            for lit in literals {
                rewrite_literal(lit, now);
            }
        }
        Predicate::MemberOf(value, _, _) => rewrite_expr_literals(value, now),
        Predicate::Regexp(expr, _, _) => rewrite_expr_literals(expr, now),
```
(Leave the `And`/`Or`/`Not` recursion arms untouched.)

`eval_predicate`:

```rust
        Predicate::Like(expr, pattern, negated) => {
            let value = eval_expr(record, expr, disk_reads_allowed);
            if value.is_null() {
                return None;
            }
            let base = Some(like_matches(&value.to_cmp_string(), pattern));
            maybe_negate(base, *negated)
        }
        // Regexp arm unchanged
        Predicate::In(expr, literals, negated) => {
            let value = eval_expr(record, expr, disk_reads_allowed);
            if value.is_null() {
                return None;
            }
            let base = Some(literals.iter().any(|lit| element_equals(&value, lit)));
            maybe_negate(base, *negated)
        }
        Predicate::MemberOf(value_expr, col, negated) => {
            let needle = eval_expr(record, value_expr, disk_reads_allowed);
            let value = resolve_col(record, col, disk_reads_allowed);
            let Value::List(items) = &value else {
                return None;
            };
            let base = Some(items.iter().any(|el| eval_compare(el, &CmpOp::Eq, &needle) == Some(true)));
            maybe_negate(base, *negated)
        }
        Predicate::IsNull(expr, negated) => {
            Some(eval_expr(record, expr, disk_reads_allowed).is_null() != *negated)
        }
```
(For a bare-column operand `eval_expr(Expr::Col)` delegates to `resolve_col`, and a literal needle equals `literal_value(lit)` — so existing queries stay byte-identical.)

- [ ] **Step 6: Update the AST-shape unit tests in `src/query/parse.rs`**

The `in_like_isnull`, `member_of…`, and `file_body_pseudo_column` tests assert the old `ColRef`/`Literal` shapes. Wrap each in `Expr::Col(...)` (or `Expr::Lit(...)` for a literal value). Concretely, the expected values become e.g.:

```rust
// in_like_isnull:
Some(Predicate::In(
    Expr::Col(ColRef::Field(vec!["status".into()])),
    vec![Literal::Str("a".into()), Literal::Str("b".into())],
    false,
))
// ...
Some(Predicate::Like(
    Expr::Col(ColRef::Field(vec!["slice".into()])),
    "mobile%".into(),
    false,
))
// ...
Some(Predicate::IsNull(Expr::Col(ColRef::Field(vec!["epic".into()])), false))

// file_body_pseudo_column match arm:
Predicate::Like(Expr::Col(ColRef::File(FileAttr::Body)), pattern, false) => {
    assert_eq!(pattern, "%TODO%")
}

// member_of tests: the literal value becomes Expr::Lit(...):
Some(Predicate::MemberOf(
    Expr::Lit(Literal::Str("mobile".into())),
    ColRef::Field(vec!["tags".into()]),
    false,
))
```
Add a positive parser assert that a scalar/column value now lowers (e.g. `parse("SELECT x WHERE lower(status) LIKE '%d%'")` → `Predicate::Like(Expr::Scalar(ScalarFn::Lower, _), "%d%".into(), false)`, and `parse("SELECT x WHERE lead MEMBER OF(tags)")` → `Predicate::MemberOf(Expr::Col(ColRef::Field(vec!["lead".into()])), …)`). Import `Expr` into the test module if not already in scope.

- [ ] **Step 7: Run the full suite and fix any remaining match arms / tests**

Run: `cargo test`
Expected: PASS. The compiler will point at any `match Predicate` arm not yet updated (there should be none beyond the three functions above plus the `collect_predicate_fields`/`predicate_label` in ast.rs). Also run:
Run: `cargo clippy --all-targets && cargo fmt --check`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
cargo fmt
git add src/query/ast.rs src/query/parse.rs src/query/exec.rs tests/cli.rs
git commit -m "feat(query): scalar Expr on the tested side of LIKE/IN/IS NULL/MEMBER OF (W43)"
```

---

### Task 2: W47a — `render::render_to(&mut impl Write, …)` + `render()` on top of it

Self-contained in `render.rs`: add the streaming entrypoint, keep `render()` as a thin buffered wrapper so every existing string test passes unchanged.

**Files:**
- Modify: `src/render.rs` (add `render_to`, `JsonRows`, `stream_delimited`; reimplement `render`)

**Interfaces:**
- Consumes: `render_vertical`/`render_table`/`render_markdown` (existing), `to_json` (existing), `WriterBuilder` (existing import), `ResultTable`/`Value`/`Output`/`Format`/`TableStyle`.
- Produces: `pub fn render_to(w: &mut impl Write, table: &ResultTable, output: Output, style: TableStyle, header: bool) -> std::io::Result<()>`.

- [ ] **Step 1: Write the failing test** in `src/render.rs`'s `mod tests`:

```rust
    /// W47 W1: render_to writes exactly what render() returns plus one newline,
    /// for every format — the byte-identity contract the streaming path rests on.
    #[test]
    fn render_to_equals_render_plus_newline_all_formats() {
        let t = table();
        for output in [
            Output::Format(Format::Table),
            Output::Format(Format::Md),
            Output::Format(Format::Json),
            Output::Format(Format::Csv),
            Output::Format(Format::Tsv),
            Output::Vertical,
        ] {
            let buffered = render(&t, output, TableStyle::Ascii, true);
            let mut streamed = Vec::new();
            render_to(&mut streamed, &t, output, TableStyle::Ascii, true).unwrap();
            assert_eq!(
                String::from_utf8(streamed).unwrap(),
                format!("{buffered}\n"),
                "{output:?} render_to must equal render() + newline"
            );
        }
    }

    /// W47 W1 edge: an empty result with the header suppressed still emits one
    /// bare newline for csv/tsv, matching the old `println!("")`.
    #[test]
    fn render_to_empty_no_header_csv_emits_one_newline() {
        let empty = ResultTable {
            headers: vec!["status".into()],
            rows: vec![],
        };
        for fmt in [Format::Csv, Format::Tsv] {
            let mut streamed = Vec::new();
            render_to(&mut streamed, &empty, Output::Format(fmt), TableStyle::Ascii, false).unwrap();
            assert_eq!(String::from_utf8(streamed).unwrap(), "\n", "{fmt:?}");
            // and it still equals render() + "\n"
            let buffered = render(&empty, Output::Format(fmt), TableStyle::Ascii, false);
            assert_eq!(buffered, "");
        }
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test --test '' 2>/dev/null; cargo test render_to_equals_render_plus_newline_all_formats`
Expected: FAIL to compile — `render_to` does not exist yet.

- [ ] **Step 3: Implement `render_to`, `JsonRows`, and `stream_delimited`; reimplement `render`**

Extend the imports at the top of `src/render.rs`:

```rust
use std::io::{self, IsTerminal, Write};
```

Replace the body of `pub fn render(...)` with a buffered wrapper, and add the streaming functions next to it:

```rust
/// Renders `table` per `output` and returns it as a `String` with no trailing
/// newline (the printers add exactly one). Thin buffered wrapper over
/// [`render_to`] — the single source of truth — minus its trailing newline, so
/// the historical "no trailing newline" contract is preserved.
pub fn render(table: &ResultTable, output: Output, style: TableStyle, header: bool) -> String {
    let mut buf = Vec::new();
    render_to(&mut buf, table, output, style, header)
        .expect("writing to an in-memory Vec is infallible");
    let s = String::from_utf8_lossy(&buf);
    s.strip_suffix('\n').unwrap_or(&s).to_string()
}

/// Renders `table` per `output` directly into `w`, newline-terminated. The
/// bulk-export formats (json/csv/tsv) stream row-by-row so a large export
/// starts writing immediately and never materializes a second full copy; the
/// interactive formats (table/md/vertical) build their string first (they are
/// interactive-scale). The single trailing newline is owned here (not the
/// sink), which is what lets csv/tsv stream without buffering the last record.
pub fn render_to(
    w: &mut impl Write,
    table: &ResultTable,
    output: Output,
    style: TableStyle,
    header: bool,
) -> io::Result<()> {
    match output {
        Output::Vertical => writeln_block(w, &render_vertical(table)),
        Output::Format(Format::Table) => writeln_block(w, &render_table(table, style, header)),
        Output::Format(Format::Md) => writeln_block(w, &render_markdown(table, header)),
        Output::Format(Format::Json) => {
            serde_json::to_writer_pretty(&mut *w, &JsonRows(table)).map_err(io::Error::other)?;
            w.write_all(b"\n")
        }
        Output::Format(Format::Csv) => stream_delimited(w, table, b',', header),
        Output::Format(Format::Tsv) => stream_delimited(w, table, b'\t', header),
    }
}

/// Writes a pre-built block followed by exactly one newline.
fn writeln_block(w: &mut impl Write, block: &str) -> io::Result<()> {
    w.write_all(block.as_bytes())?;
    w.write_all(b"\n")
}

/// Serializes a [`ResultTable`] as a JSON array of objects keyed by column
/// header, one object per row, streamed row-by-row. Each row rebuilds the
/// SAME `serde_json::Map` [`render_json`] used, so key ordering is byte-identical
/// to the buffered path regardless of serde_json's `preserve_order` feature
/// (absent here → sorted keys).
struct JsonRows<'a>(&'a ResultTable);

impl serde::Serialize for JsonRows<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.rows.len()))?;
        for row in &self.0.rows {
            let fields: Map<String, JsonValue> = self
                .0
                .headers
                .iter()
                .zip(row)
                .map(|(header, value)| (header.clone(), to_json(value)))
                .collect();
            seq.serialize_element(&JsonValue::Object(fields))?;
        }
        seq.end()
    }
}

/// Streams `table` as `delimiter`-separated records directly into `w`. csv's
/// default record terminator is `\n`, so the final record's terminator is the
/// block's single trailing newline — matching the buffered path's
/// strip-then-append. When nothing is written (`!header && no rows`), emit one
/// bare newline to match the old `println!("")`.
fn stream_delimited(
    w: &mut impl Write,
    table: &ResultTable,
    delimiter: u8,
    header: bool,
) -> io::Result<()> {
    {
        let mut writer = WriterBuilder::new().delimiter(delimiter).from_writer(&mut *w);
        if header {
            writer.write_record(&table.headers).map_err(io::Error::other)?;
        }
        for row in &table.rows {
            writer
                .write_record(row.iter().map(Value::display))
                .map_err(io::Error::other)?;
        }
        writer.flush()?;
        // `writer` is dropped at the end of this block, releasing its `&mut *w`
        // borrow so the empty-case newline below can write to `w`.
    }
    if !header && table.rows.is_empty() {
        w.write_all(b"\n")?;
    }
    Ok(())
}
```

The old `render_json`, `render_delimited`, and `write_delimited` become unused. Delete `render_delimited` and `write_delimited` (they were only called by the old `render`). Keep `render_json`'s logic only if a test references it — the `json_export_emits_nested_object_for_map` test calls `super::to_json` directly (kept), so `render_json` can be deleted too. Delete `render_json`. Run clippy to confirm nothing else references them.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test render_to_equals_render_plus_newline_all_formats render_to_empty_no_header_csv_emits_one_newline`
Expected: PASS.
Run: `cargo test` (the whole `render` module — existing json/csv/tsv/table/md/vertical string tests must all stay green, proving byte-identity)
Expected: PASS.
Run: `cargo clippy --all-targets && cargo fmt --check`
Expected: clean (no dead-code warnings for the deleted functions).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/render.rs
git commit -m "feat(render): render_to streams json/csv/tsv to a writer; render() wraps it (W47)"
```

---

### Task 3: W47b — `OutputSink::write_result(|w| …)` streaming primitive

Self-contained in `output.rs`. `write_block` stays (still used by repl/main until Task 4) and is reimplemented on top of `write_result`, so its existing tests keep passing.

**Files:**
- Modify: `src/output.rs` (add `write_result`, reimplement `write_block`, add a test)

**Interfaces:**
- Produces: `pub fn write_result(&mut self, f: impl FnOnce(&mut dyn Write) -> io::Result<()>) -> io::Result<()>`.

- [ ] **Step 1: Write the failing test** in `src/output.rs`'s `mod tests`:

```rust
    #[test]
    fn write_result_streams_into_the_same_sink() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let mut sink = OutputSink::open_file(&path).unwrap();
        sink.write_result(|w| {
            w.write_all(b"a,b\n")?;
            w.write_all(b"1,2\n")
        })
        .unwrap();
        sink.write_result(|w| w.write_all(b"tail\n")).unwrap();
        drop(sink);
        assert_eq!(fs::read_to_string(&path).unwrap(), "a,b\n1,2\ntail\n");
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test write_result_streams_into_the_same_sink`
Expected: FAIL to compile — `write_result` does not exist.

- [ ] **Step 3: Implement `write_result`; reimplement `write_block`**

Add to `impl OutputSink` (the `Write` trait is already imported):

```rust
    /// Runs `f` with a writer aimed at this sink's destination — a locked
    /// stdout handle, the redirected file, or a piped command's stdin — then
    /// flushes. The streaming counterpart of [`write_block`]: the caller writes
    /// the fully-formatted, newline-terminated block itself (see
    /// `render::render_to`), so no intermediate `String` is built.
    pub fn write_result(
        &mut self,
        f: impl FnOnce(&mut dyn Write) -> io::Result<()>,
    ) -> io::Result<()> {
        match self {
            OutputSink::Stdout => {
                let stdout = io::stdout();
                let mut lock = stdout.lock();
                f(&mut lock)?;
                lock.flush()
            }
            OutputSink::File(file) => {
                f(file)?;
                file.flush()
            }
            OutputSink::Command(child) => {
                let stdin = child.stdin.as_mut().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "child stdin closed")
                })?;
                f(stdin)?;
                stdin.flush()
            }
        }
    }
```

Reimplement `write_block` on top of it (byte-identical to the old `println!`/`writeln!` — block bytes + one newline):

```rust
    pub fn write_block(&mut self, block: &str) -> io::Result<()> {
        self.write_result(|w| {
            w.write_all(block.as_bytes())?;
            w.write_all(b"\n")
        })
    }
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test write_result_streams_into_the_same_sink`
Expected: PASS.
Run: `cargo test` (the existing `output` module tests — `write_block_appends_within_the_same_sink`, `reopening_the_same_path_truncates_prior_contents`, `command_sink_pipes_blocks_through_the_shell` — must stay green)
Expected: PASS.
Run: `cargo clippy --all-targets && cargo fmt --check`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add src/output.rs
git commit -m "feat(output): OutputSink::write_result streaming primitive; write_block on top (W47)"
```

---

### Task 4: W47c — wire streaming through `Session` and both callers

Replaces `Session::render_statement_counted` with `render_statement_to`, threads the sink through `repl.rs` and `main.rs`, removes the now-unused `write_block`, and adds byte-identity integration tests. Depends on Task 2 (`render_to`) and Task 3 (`write_result`). **Shares `tests/cli.rs` with Task 1 — do not run this task's implementer while Task 1's is still in flight.**

**Files:**
- Modify: `src/session.rs` (replace `render_statement_counted` → `render_statement_to`; update its unit test)
- Modify: `src/repl.rs` (`run_statement`)
- Modify: `src/main.rs` (`run_statements`)
- Modify: `src/output.rs` (delete `write_block` + its now-redundant tests, once no caller remains)
- Test: `tests/cli.rs` (byte-identity for csv/json/tsv + `--output` file)

**Interfaces:**
- Consumes: `render::render_to` (Task 2), `OutputSink::write_result` (Task 3), `Statement::terminator`, `Session::{format,style,header,run}`.
- Produces: `pub fn render_statement_to(&self, statement: &Statement, sink: &mut OutputSink) -> anyhow::Result<usize>`.

- [ ] **Step 1: Write the failing integration tests** in `tests/cli.rs`:

```rust
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
    assert_eq!(fs::read_to_string(&out).unwrap(), "status\ndraft\nsynced\nsynced\n");
}
```

- [ ] **Step 2: Run to verify they fail (or, if green today, keep as characterization)**

Run: `cargo test --test cli streaming_formats_are_byte_identical streaming_output_flag_writes_file_bytes`
Expected: These pass on the pre-refactor binary too (they characterize today's exact bytes). They are the guard that the refactor keeps output byte-identical — they MUST stay green through Steps 3–5. (If any fails now, the expected bytes are wrong — fix the expectation before refactoring.)

- [ ] **Step 3: Replace `render_statement_counted` in `src/session.rs`**

Add the import near the other `crate::` uses:

```rust
use crate::output::OutputSink;
```

Replace `pub fn render_statement_counted(...)` with:

```rust
    /// Runs `statement` once and writes its rendered result — in the session's
    /// current format for `;`/`\g`, or one record per block for `\G` — directly
    /// into `sink` (stdout/file/piped command), returning the row count from
    /// that same [`ResultTable`]. Streaming replaces the old
    /// build-a-`String`-then-`write_block` two-step (design W47). The REPL
    /// prints the count as a `-- N rows` line; one-shot/batch callers sum it
    /// for `--exit-code`.
    pub fn render_statement_to(
        &self,
        statement: &Statement,
        sink: &mut OutputSink,
    ) -> anyhow::Result<usize> {
        let table = self.run(&statement.sql)?;
        let output = statement.terminator.output(self.format());
        sink.write_result(|w| render::render_to(w, &table, output, self.style(), self.header()))
            .context("failed to write query results")?;
        Ok(table.rows.len())
    }
```
(`Context`/`with_context` is already imported in `session.rs`; `render` is already imported.)

- [ ] **Step 4: Update the `session.rs` unit test**

The `render_statement_counted_returns_row_count` test (`session.rs`, currently ~line 634) builds a `String` and asserts `count`. Point it at a file-backed sink and keep the exact 3/1/0 counts. Replace the whole test with (reusing the same `InMemoryStore`/`Session`/`semi` construction it already uses — `TempDir`, `fs`, `InMemoryStore`, `WalkOpts`, `Settings`, `Session` are already imported in that test module):

```rust
    /// `render_statement_to` runs the query exactly once and returns its row
    /// count, writing the rendered block into the sink (design W47).
    #[test]
    fn render_statement_to_returns_row_count() {
        let td = TempDir::new().unwrap();
        for (name, body) in [
            ("a.md", "---\nstatus: draft\n---\n"),
            ("b.md", "---\nstatus: synced\n---\n"),
            ("c.md", "---\nstatus: synced\n---\n"),
        ] {
            fs::write(td.path().join(name), body).unwrap();
        }
        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default(), None);
        let session = Session::new(Box::new(store), Settings::default(), Settings::default(), None);

        let out = TempDir::new().unwrap();
        let mut sink = crate::output::OutputSink::open_file(&out.path().join("o.txt")).unwrap();
        assert_eq!(
            session.render_statement_to(&semi("SELECT status"), &mut sink).unwrap(),
            3
        );
        assert_eq!(
            session
                .render_statement_to(&semi("SELECT status WHERE status = 'draft'"), &mut sink)
                .unwrap(),
            1
        );
        assert_eq!(
            session
                .render_statement_to(&semi("SELECT status WHERE status = 'missing'"), &mut sink)
                .unwrap(),
            0
        );
    }
```
(`tempfile` is already a `dev-dependency` — the integration tests and this module's other tests use `TempDir` — so no `Cargo.toml` change is needed.)

- [ ] **Step 5: Update `src/repl.rs::run_statement`**

```rust
fn run_statement(session: &Session, statement: &Statement, sink: &mut OutputSink) {
    let start = Instant::now();
    match session.render_statement_to(statement, sink) {
        Ok(count) => {
            let elapsed = session.timer().then(|| start.elapsed());
            eprintln!("{}", row_count_line(count, elapsed));
        }
        Err(err) => eprintln!("querymatter: {err:#}"),
    }
}
```
(A write failure now flows through the same `Err` arm, its message prefixed by the `"failed to write query results"` context — previously it was a separate `"failed to write results: {err}"` line. Equivalent user-facing behavior.)

- [ ] **Step 6: Update `src/main.rs::run_statements`**

```rust
    for (i, statement) in statements.iter().enumerate() {
        let rows = session
            .render_statement_to(statement, &mut sink)
            .map_err(|e| match total {
                1 => e,
                _ => e.context(format!("statement {} of {total} failed", i + 1)),
            })?;
        total_rows += rows;
    }
```

- [ ] **Step 7: Remove the now-unused `write_block` from `src/output.rs`**

No caller remains (repl/main now use `write_result` via the session). Delete `pub fn write_block` and the tests that only exercised it (`write_block_appends_within_the_same_sink`, `reopening_the_same_path_truncates_prior_contents`) — or convert them to `write_result` (preferred: convert `write_block_appends_within_the_same_sink` and `reopening_the_same_path_truncates_prior_contents` to call `write_result(|w| write!(w, "{}\n", s))`, keeping their file-content assertions). This keeps truncation/append coverage while removing the dead method. `command_sink_pipes_blocks_through_the_shell` and `write_result_streams_into_the_same_sink` remain.

- [ ] **Step 8: Run everything and verify green + byte-identity**

Run: `cargo test`
Expected: PASS — including the Step-1 byte-identity tests (still green → refactor preserved output) and the full existing suite.
Run: `cargo clippy --all-targets && cargo fmt --check`
Expected: clean (no dead-code warning; `write_block` is gone).

- [ ] **Step 9: Commit**

```bash
cargo fmt
git add src/session.rs src/repl.rs src/main.rs src/output.rs tests/cli.rs
git commit -m "feat(render): stream statement results through the OutputSink, no String buffer (W47)"
```

---

### Task 5: README — document scalar exprs on the tested predicate side (W43)

W47 is internal (output is byte-identical), so it needs no README change. W43 adds user-visible capability.

**Files:**
- Modify: `README.md` (the `WHERE` bullet, lines ~80–89)

- [ ] **Step 1: Update the `WHERE` bullet**

The bullet already says scalar expressions work on either side of a comparison and that `REGEXP` takes a general expression. Extend it so `LIKE`/`IN`/`IS NULL`/`MEMBER OF` say the same, and change the `MEMBER OF` example so the tested side is shown as an expression/column. Concretely, edit the prose so it reads (adjust wording to fit the surrounding sentence):

> … plus `LIKE`/`NOT LIKE` (`%`/`_` wildcards), `[NOT] REGEXP '<pattern>'` …, `IN (...)`/`NOT IN (...)`, `IS NULL`/`IS NOT NULL`, and `[NOT] <value> MEMBER OF(<col>)` for a list-valued field. **The tested side of `LIKE`, `IN`, `IS NULL`, and `MEMBER OF` is a full scalar expression** (like each side of a comparison), so `WHERE lower(status) LIKE '%draft%'`, `WHERE trim(x) IS NULL`, and `WHERE lead MEMBER OF(tags)` (a column on the left — previously it had to be a literal) all work, e.g. `WHERE 'mobile' MEMBER OF(tags)` still does too.

- [ ] **Step 2: Verify the doc builds / links**

Run: `cargo build` (sanity; README isn't compiled but confirms the tree is intact)
Expected: success. Re-read the edited paragraph to confirm it is accurate and not self-contradictory.

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: scalar expressions on the tested side of LIKE/IN/IS NULL/MEMBER OF (W43)"
```

---

## Final: review + strip WHATS-NEXT items + finish branch

- [ ] Dispatch the final code-reviewer over the whole branch diff (spec-compliance + quality). Resolve any Critical/Important findings.
- [ ] Strip W43 and W47 from `WHATS-NEXT.md` (they are marked in-flight) and add a dated shipped-note stanza, matching the existing "shipped and stripped" convention. `WHATS-NEXT.md` is gitignored — this is a local-only edit, not a commit.
- [ ] Invoke `superpowers:finishing-a-development-branch`.

## Self-review notes (author)

- **Spec coverage:** W43 §3 → Task 1 (+ README Task 5); its I1 (file.body funnel) is the `force_cache_file_body_like_still_errors_after_widening` test; I2/I3 covered by updated unit asserts + the new integration tests. W47 §4 → Tasks 2 (render_to/W1), 3 (write_result), 4 (wiring + W2/W3 byte-identity). Non-goals respected (no out-of-core; MEMBER OF array stays ColRef).
- **Type consistency:** `render_to(&mut impl Write, &ResultTable, Output, TableStyle, bool) -> io::Result<()>` and `render_statement_to(&self, &Statement, &mut OutputSink) -> anyhow::Result<usize>` are used identically wherever referenced. `JsonRows`/`stream_delimited`/`writeln_block` are private to `render.rs`.
- **Ordering:** 1, 2, 3 are disjoint-file and independently reviewable; 4 requires 2+3; 5 requires 1. 1 and 4 both edit `tests/cli.rs` — sequence them (never two implementers on that file at once).
