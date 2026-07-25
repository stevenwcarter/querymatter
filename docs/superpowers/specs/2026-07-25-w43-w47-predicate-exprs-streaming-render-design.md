# querymatter — scalar-expr predicates + streaming render (whats-next W43, W47)

- **Date:** 2026-07-25
- **Status:** Approved (brainstormed via `/ship-it --ask`, ready for planning)
- **Source:** `whats-next --execute` bundle of 2 items: W43, W47.

## 1. Overview

Two independent, adjacent improvements to `querymatter`:

- **W43** widens the tested operand of `LIKE` / `IN` / `IS NULL` / `MEMBER OF`
  from a bare column (`ColRef`) to a general scalar expression (`Expr`), so
  `WHERE lower(status) LIKE '%draft%'` works — exactly as it already does on
  either side of `=`. This is the deferred follow-through the W51 regex spec
  explicitly scoped out ("W43 … is not in scope — only regex gets an `Expr`
  operand"). The `Predicate::Regexp(Expr, …)` node W51 shipped is the template.
- **W47** streams the `json` / `csv` / `tsv` render straight to the output writer
  (stdout / file / piped command) instead of building a fully-materialized
  `String` first — removing the second full-result copy and cutting first-byte
  latency for bulk exports.

They share no code but both touch the query→render→output path, so they ship in
one branch.

### Locked-in decisions (from brainstorming)

1. **W43 covers all four predicates**, including `MEMBER OF`: widen its *tested*
   value (currently a `Literal`) to `Expr` — `MemberOf(Expr, ColRef, negated)`.
   This also enables a bare column or scalar-fn on the left (`status MEMBER
   OF(tags)`, `lower(x) MEMBER OF(tags)`), which is impossible today (the left
   must be a literal). The `OF(col)` array stays a `ColRef` — no scalar function
   yields a `Value::List`, so widening it would be dead capability.
2. **W47 target is the render buffer, not the result set.** The executor already
   materializes the whole `ResultTable` in RAM; eliminating *that* is the
   separate XL item **W59** and is explicitly out of scope. W47 removes only the
   rendered-`String` copy and streams row-by-row to the writer.
3. **`render_to` owns the single trailing newline** (not the sink). This is the
   only way `csv`/`tsv` can stream without buffering the last record to strip its
   terminator.
4. **Byte-identical output is sacred** for both items (the project's standing
   "piped output stays byte-identical" invariant). Every existing query result
   and rendered byte stays unchanged except where an item explicitly changes it.

## 2. Goals / non-goals

**Goals:** implement W43 + W47; keep all existing query results and rendered
output byte-identical; TDD both, with the load-bearing invariant tests written
(not declined); update README.

**Non-goals:** the unchecked whats-next items. W47 does **not** make the result
set out-of-core (that is W59). W43 does **not** widen the `MEMBER OF` array
operand, nor introduce scalar functions returning lists.

## 3. W43 — scalar expressions as the tested predicate operand

### Design

Mirror the existing `Predicate::Regexp(Expr, String, negated)` node end-to-end.

- **`src/query/ast.rs`** — change the four variants:
  - `Like(Expr, String, /* negated */ bool)`
  - `In(Expr, Vec<Literal>, /* negated */ bool)`
  - `IsNull(Expr, /* negated */ bool)`
  - `MemberOf(Expr, ColRef, /* negated */ bool)` — first field (tested value)
    widened; the array stays `ColRef`.
  - Update the variant doc comments; update `collect_predicate_fields`
    (Like/In/IsNull → `collect_expr_fields`; MemberOf → `collect_expr_fields` on
    the value **plus** `collect_col_field` on the array); update `predicate_label`
    to render the widened operands via `expr_label`.
- **`src/query/parse.rs`** — in `lower_predicate`, call `lower_expr` (not
  `lower_col_ref`) for the `Like` / `InList` / `IsNull` / `IsNotNull` operands;
  in `lower_member_of`, call `lower_expr` on `member_of.value` (was
  `lower_literal`). The parser stops rejecting a non-column tested side — the
  sqlparser grammar already accepts these forms; only our lowering was narrow.
- **`src/query/exec.rs`** — three exhaustive `match`es on `Predicate` (the
  compiler enforces each arm is updated):
  - `predicate_columns`: Like/In/IsNull → `expr_columns(expr)`; MemberOf →
    `expr_columns(value)` chained with the array `col` (mirrors the `Regexp` arm).
  - `rewrite_predicate_literals`: the widened operands can now carry literals
    (incl. relative-date literals), so Like/In/IsNull rewrite their operand via
    `rewrite_expr_literals`; MemberOf rewrites its value expr via
    `rewrite_expr_literals` (was `rewrite_literal`).
  - `eval_predicate`: Like/In/IsNull evaluate the operand with `eval_expr`
    instead of `resolve_col`. `Like` keeps `to_cmp_string()` on the evaluated
    value (unchanged for a bare column); `In` keeps the `element_equals` loop;
    `IsNull` keeps `.is_null()`. `MemberOf` evaluates the needle **once**
    (`let needle = eval_expr(record, value, disk_reads_allowed)`), then
    `items.any(|el| eval_compare(el, &CmpOp::Eq, &needle) == Some(true))`.

### Semantics preserved / defined

- **Bare-column & literal byte-identity:** `eval_expr(Expr::Col(c)) ==
  resolve_col(c)` and a literal needle goes through `literal_value` unchanged, so
  every existing `col LIKE …` / `col IN …` / `col IS NULL` / `lit MEMBER OF(col)`
  query returns identical results.
- **3VL / null handling unchanged:** Like/In on a null operand → unknown (`None`);
  `IS NULL` on a null operand → true; `MEMBER OF` unknown only when the *array*
  is null / non-list. A null needle in `MEMBER OF` matches nothing (`false`),
  matching today's `Literal::Null` needle and `IN`'s null-in-list behavior.

### Invariants this feature depends on (pin each with a test)

- **I1 — file.body flows through the widened funnel.** `references_body` detects
  `file.body` via `predicate_columns`; after widening, `WHERE file.body LIKE
  '%TODO%'` lowers to `Like(Expr::Col(File(Body)), …)` and must still be detected,
  so a `--force-cache` query with `file.body LIKE` still fails fast (the W56
  invariant). Pin with an **integration** test (strict `--force-cache` errors),
  not only the AST-shape unit test.
- **I2 — existing bare-column predicates unchanged.** The AST-shape asserts at
  `parse.rs` (the `in_like_isnull`, `member_of`, and `file_body_pseudo_column`
  tests) are updated to the `Expr` shape; end-to-end results for bare-column
  LIKE/IN/IS NULL/MEMBER OF stay identical (characterization).
- **I3 — new capability works.** `lower(status) LIKE '%draft%'`,
  `lower(status) IN ('a','b')`, `trim(x) IS NULL`, `status MEMBER OF(tags)`
  (bare column, newly possible), and `lower(x) MEMBER OF(tags)` all parse and
  evaluate correctly (integration).

### Item → files
`src/query/ast.rs`, `src/query/parse.rs`, `src/query/exec.rs`, `tests/` (CLI
integration).

## 4. W47 — stream json/csv/tsv render to the output writer

### Design

- **`src/render.rs`** — add
  `pub fn render_to(w: &mut impl Write, table: &ResultTable, output: Output,
  style: TableStyle, header: bool) -> io::Result<()>` that writes the complete,
  newline-terminated block for every format:
  - `Vertical` / `Table` / `Md`: build the string as today
    (`render_vertical` / `render_table` / `render_markdown`), then write it
    followed by one `\n`. (These stay buffered — interactive-scale, per scope.)
  - `Json`: `serde_json::to_writer_pretty(&mut w, &JsonRows(table))?` then
    `w.write_all(b"\n")`. `JsonRows<'a>(&'a ResultTable)` is a `Serialize` newtype
    serializing as a sequence: for **each** row it builds the *exact same*
    `serde_json::Map<String, JsonValue>` today's `render_json` builds (cells via
    the existing `to_json` helper) and serializes it as a seq element — removing
    the outer `Vec<JsonValue>` and the `String` while streaming row-by-row.
    **Byte-identity note:** `serde_json` is built **without** `preserve_order`
    here, so its `Map` is a `BTreeMap` and today's JSON emits **sorted** keys
    (not header order). Reusing the `Map` construction reproduces that ordering
    exactly; a hand-rolled `serialize_map` in header order would silently break
    byte-identity — do **not** do that.
  - `Csv` / `Tsv`: `csv::Writer::from_writer(&mut w)` (csv's default record
    terminator is `\n`), write the header record (when `header`) and each row,
    then flush. The final record's `\n` is the block's single trailing newline —
    no strip, no extra append. **Edge:** when `!header && table.rows.is_empty()`
    the writer emits nothing, so write a bare `\n` to match today's
    `println!("")`.
  - Reimplement `render() -> String` as *`render_to` into a `Vec<u8>` with
    exactly one trailing `\n` stripped* — one source of truth, and the existing
    "no trailing newline" `String` contract (and every current `render.rs` test)
    is preserved unchanged.
- **`src/output.rs`** — add
  `pub fn write_result(&mut self, f: impl FnOnce(&mut dyn Write) -> io::Result<()>)
  -> io::Result<()>` that, per variant, locks stdout (`io::stdout().lock()`) /
  borrows the `File` / borrows the child's stdin, calls `f(writer)`, and flushes.
  Reimplement `write_block` on top of it (`write_all(block)` + `write_all(b"\n")`)
  — byte-identical to today's `println!`, so `output.rs`'s own tests pass.
- **`src/session.rs` + callers** — replace `render_statement_counted(&self,
  statement) -> anyhow::Result<(String, usize)>` with
  `render_statement_to(&self, statement, sink: &mut OutputSink) ->
  anyhow::Result<usize>`: run the query, resolve `Output` (`statement.terminator
  .output(self.format())`), call
  `sink.write_result(|w| render::render_to(w, &table, output, self.style(),
  self.header()))`, and return `table.rows.len()`. `session.rs` gains an
  intra-crate dependency on `output::OutputSink` (accepted; alternative was
  caller-orchestration, rejected for churn). Update the two callers:
  - `src/repl.rs::run_statement` — `session.render_statement_to(statement, sink)`
    → count; timer + `-- N rows` line unchanged.
  - `src/main.rs::run_statements` — same; the "statement N of M failed" context
    still wraps the call.

### Invariants this feature depends on (pin each with a test)

- **W1 — render_to == render() + "\n".** For **every** format (table, md,
  vertical, json, csv, tsv), `render_to` into a buffer equals the old
  `render()` string plus one `\n`, including the empty-result-and-no-header csv
  edge, and (json) the sorted-key ordering above.
- **W2 — sink outputs byte-identical.** `--output <file>`, REPL `.output <path>`,
  and `.output |cmd` produce byte-for-byte identical file/pipe contents before vs.
  after (extend the `output.rs` / integration tests).
- **W3 — piped stdout unchanged.** `-e "SELECT …" --format {csv,json,tsv}` piped
  (non-TTY) is byte-for-byte unchanged (integration; compare against `render()`).
- **W4 — json/csv/tsv have exactly one trailing newline; header/style flags and
  the `-- N rows` line behave exactly as before.**

### Item → files
`src/render.rs`, `src/output.rs`, `src/session.rs`, `src/repl.rs`, `src/main.rs`,
`tests/` (CLI integration).

## 5. Test strategy

- **W43:** parser unit tests for the five new lowered shapes + updated
  bare-column shape asserts; `exec` integration for I1–I3 (the `--force-cache`
  `file.body LIKE` fast-fail is the load-bearing one). No test is declined by
  appeal to an invariant — the file.body seam is tested end-to-end.
- **W47:** `render.rs` unit tests for W1 (all six formats, incl. the csv edge);
  `output.rs` tests for `write_result` streaming + `write_block` parity;
  integration for W2/W3 (file + piped byte-identity across csv/json/tsv).
- The full existing suite (insta snapshots, render string tests, REPL tests,
  CLI integration) MUST stay green — proof neither item shifted observable
  behavior. This is a binary-only crate (no `cargo test --lib`); run `cargo test`
  and `cargo clippy`/`cargo fmt` per the project's conventions.

## 6. Item → primary files

| Item | Primary files |
|------|---------------|
| W43 scalar-expr predicates | query/ast.rs, query/parse.rs, query/exec.rs, tests/ |
| W47 streaming render | render.rs, output.rs, session.rs, repl.rs, main.rs, tests/ |
