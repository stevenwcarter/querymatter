# Query-language expansion — design

Date: 2026-07-24
Status: approved
Batch: WHATS-NEXT sub-project 1 of 4 (query language)

## 1. Problem

querymatter's SQL subset is deliberately narrow. This sub-project widens the
most-requested parts of that boundary, all inside `src/query/`. Nine items,
selected from `WHATS-NEXT.md`:

- **W30** expression tree — SELECT/WHERE operands are bare columns only.
- **W14** scalar string functions.
- **W1** list-membership (`MEMBER OF`).
- **W11** `HAVING`.
- **W8** SELECT aliases usable in `GROUP BY`.
- **W19** `DISTINCT` on the projection.
- **W20** `ORDER BY` a bare aggregate.
- **W12** unknown-column validation with a did-you-mean.
- **W7** friendly "not supported" messages.

Several of these are cheap because `sqlparser` already produces the AST node
querymatter currently rejects; the work is lowering + exec, not new parsing.

## 2. The expression tree (W30 + W14 + arithmetic/concat)

### 2.1 The type

Today `SelectExpr` is `Star | Col(ColRef) | Agg(Aggregate)` and
`Predicate::Compare` is `(ColRef, CmpOp, Literal)` — both leaves are bare. A
new `Expr` replaces the scalar-operand positions:

```rust
/// A scalar expression: evaluates to one `Value` per row (ungrouped) or per
/// group cell (grouped, over the group's representative row where a bare
/// column is a grouping key).
pub enum Expr {
    /// A column or `file.*` pseudo-column.
    Col(ColRef),
    /// A literal constant.
    Lit(Literal),
    /// A scalar function applied to argument expressions.
    Scalar(ScalarFn, Vec<Expr>),
    /// A binary arithmetic or string-concat operation.
    Binary(BinOp, Box<Expr>, Box<Expr>),
}

pub enum ScalarFn { Lower, Upper, Length, Trim, Ltrim, Rtrim, Substr, Replace }

pub enum BinOp { Add, Sub, Mul, Div, Mod, Concat }
```

`SelectExpr` becomes `Star | Expr(Expr) | Agg(Aggregate)`. The former
`Col(ColRef)` projection is now `Expr(Expr::Col(...))`. `Predicate::Compare`
becomes `Compare(Expr, CmpOp, Expr)` — both sides are expressions, which is
what gives column-to-column comparison (`WHERE start < end`) for free.

`Like`, `In`, `IsNull` keep a `ColRef` left operand for now (widening them is
not in this batch and buys little).

### 2.2 Scalar functions (W14)

Eight functions, evaluated by `apply_scalar(fn, &[Value]) -> Value`:

| fn | arity | behavior |
|---|---|---|
| `lower(s)` / `upper(s)` | 1 | ASCII-aware case fold of the string form; `Null` → `Null` |
| `length(s)` | 1 | `chars().count()` as `Int`; `Null` → `Null` |
| `trim(s)` / `ltrim(s)` / `rtrim(s)` | 1 | whitespace trim; `Null` → `Null` |
| `substr(s, start)` / `substr(s, start, len)` | 2–3 | 1-based start, char-indexed, clamped; out-of-range → `""` |
| `replace(s, from, to)` | 3 | all non-overlapping occurrences |

Arguments are coerced to their string form via the same display conversion the
renderer uses (so a numeric field passed to `lower` stringifies first). Any
`Null` argument yields `Null` (SQL-standard scalar null propagation). Wrong
arity is a parse-time error naming the function and its expected arity.

### 2.3 Arithmetic and concat (folds in W27)

`apply_binary(op, &Value, &Value) -> Value`, SQL 3-valued:

- **Arithmetic** (`+ - * / %`): both operands coerced to numbers. A
  non-numeric operand → `Null`. Two `Int`s stay `Int` for `+ - * %`; `/`
  always yields `Float`; any `Float` operand promotes the result to `Float`.
  Divide- or mod-by-zero → `Null` (never a panic). Either operand `Null` →
  `Null`.
- **Concat** (`||`): each operand rendered to its string form and joined;
  either operand `Null` → `Null`.

Precedence and associativity come from `sqlparser`'s parse tree, which
querymatter lowers structurally — querymatter does not re-implement operator
precedence.

### 2.4 Where expressions may and may not appear

- **SELECT**: any `Expr` (with optional alias). A bare aggregate stays
  `SelectExpr::Agg`; an expression *containing* an aggregate (e.g. `count(*) +
  1`) is **not** supported in this batch — a parse-time "not supported"
  error, since mixing agg and scalar in one tree needs group-context
  threading beyond this scope.
- **WHERE**: `Compare(Expr, CmpOp, Expr)` — both sides scalar expressions over
  columns and literals; aggregates are not valid in WHERE (as today).
- **GROUP BY validation**: a non-aggregate SELECT expression must be composed
  entirely of grouping-key columns and literals (extending today's
  "every non-aggregate SELECT item is a grouping key" rule to expression
  trees — every *column* the expression references must be a grouping key).

## 3. `MEMBER OF` (W1)

`sqlparser` parses `<value> MEMBER OF(<column>)` into `Expr::MemberOf`.
`Predicate` gains:

```rust
    /// `<literal> [NOT] MEMBER OF(<col>)` — true when the list-valued column
    /// contains a matching element. `bool` is `true` when negated.
    MemberOf(Literal, ColRef, /* negated */ bool),
```

`eval_predicate` resolves the column; if it is `Value::List`, membership is a
scan for an element equal to the literal (reusing `In`'s element-equality and
3-valued-logic rules — a `Null` column or a non-list value yields *unknown*,
not *false*). Only the `<literal> MEMBER OF(<col>)` shape is supported; a
non-literal left side or a non-column right side is a "not supported" parse
error. `NOT ... MEMBER OF` is accepted (negation flag).

## 4. `HAVING` (W11)

`Query` gains `having: Option<Predicate>`. `select.having` is already parsed
and currently rejected in `reject_unsupported_select_clauses`; that rejection
is removed and replaced by lowering.

HAVING is a predicate whose comparison leaves may be either a grouping-key
column or an aggregate — so it needs its own lowering that routes function
calls through `lower_aggregate` (unlike WHERE, which forbids aggregates). It
gets its own predicate type, `Having`, a `Predicate`-shaped tree (And/Or/Not +
comparisons) whose comparison leaves are `HavingLeaf` rather than bare columns:

```rust
    pub having: Option<Having>,

/// A HAVING predicate: the same boolean structure as `Predicate`, but its
/// comparison leaves may reference an aggregate, not only a column.
pub enum Having {
    Compare(HavingLeaf, CmpOp, Literal),
    And(Box<Having>, Box<Having>),
    Or(Box<Having>, Box<Having>),
    Not(Box<Having>),
}
/// One side of a HAVING comparison: a grouping-key column or an aggregate.
pub enum HavingLeaf { Group(ColRef), Agg(Aggregate) }
```

Execution: after `compute_aggregate`/`project_group` produce each group's
computed cells but **before** `ORDER BY` and `LIMIT`/`OFFSET`, evaluate the
HAVING predicate per group and drop groups where it is not *true*. An
aggregate referenced by HAVING need not appear in SELECT (standard SQL) — it
is computed on demand from the group's rows. HAVING on an ungrouped query is a
parse error ("HAVING requires GROUP BY").

## 5. `GROUP BY` aliases (W8)

`lower_query` already computes `aliases: Vec<&str>` and threads it into
`lower_order_by` so `ORDER BY <alias>` resolves. `lower_group_by` receives no
such list. Pass the projection's `select_items`/`aliases` into
`lower_group_by`; when a bare `GROUP BY` identifier matches a SELECT alias,
resolve it to that item's underlying `ColRef` before the literal-column
fallback. Only a SELECT item that is a bare `Expr::Col` may be a grouping key
via its alias — an alias on a computed expression or an aggregate is not a
valid grouping key (parse error, matching the existing "grouping key must be a
column" rule). Exec is unchanged (it only ever sees resolved `ColRef`s).

## 6. `DISTINCT` (W19)

`Query` gains `distinct: bool`. `select.distinct` is already parsed and
rejected; replace the rejection with lowering to the flag. In
`execute_ungrouped`, after projection and before the `ORDER BY` sort, drop
duplicate rows keyed on the row's cells joined through the existing
`to_cmp_string()` conversion (the same keying `count(distinct col)` already
uses — `Value` has no `Eq`/`Hash`).

`DISTINCT` combined with `GROUP BY` is rejected as unsupported (a grouped query
already yields distinct group keys; the combination is redundant and its
semantics confusing). `DISTINCT` with `ORDER BY` is fine — dedup precedes sort.

## 7. `ORDER BY` a bare aggregate (W20)

`OrderKey`'s target is today a resolved column or a projection-alias
reference. Add an aggregate target so `ORDER BY count(*) DESC` works without an
`AS` alias: lower an `ORDER BY` function expression the same way
`lower_aggregate` does, and in the grouped-order resolution match it
structurally against the group's computed aggregates (a `SELECT`-side
aggregate need not exist; it is computed for ordering like HAVING's). An
aggregate `ORDER BY` on an ungrouped query is a parse error.

## 8. Column validation (W12)

### 8.1 Behavior

Today `Record::field()` returns `Value::Null` for any unknown field, so a
typo'd column silently yields an all-`Null` column or an empty result.

New default: an unknown column reference is a **hard error** naming it, with a
did-you-mean suggestion, unless `--lenient` is set. A `--lenient` flag (and a
`lenient` config key, resolved through the existing `Settings` precedence)
restores the unknown→`Null` behavior.

- "Unknown" means: a `ColRef::Field(name)` whose `name` is not in the store's
  **schema** (the union of every record's field names) and is not a `file.*`
  pseudo-column. `file.*` names are already validated at parse time and are
  unaffected.
- The suggestion is the closest schema name by Levenshtein distance within a
  small threshold (distance ≤ 2, or ≤ ⌈len/3⌉); if none is close, list the
  known columns (capped).
- **Empty vault:** when the store has zero records, the schema is empty and
  *every* column would be "unknown"; validation is **skipped** entirely in
  that case so an empty vault does not make every query fail.
- Applies in all modes (`-e`, REPL, piped batch) and to every column position:
  SELECT expressions (including columns nested inside scalar/arithmetic exprs
  and aggregate arguments), WHERE, `GROUP BY`, `ORDER BY`, `HAVING`, and
  `MEMBER OF`'s column.

### 8.2 Where the check lives

The parser has no access to the record schema, so validation is **not** a
parse-time step. It runs at execution time, once, at the start of
`query::execute`, before the filter/group pipeline: walk the lowered `Query`,
collect every `ColRef::Field` it references (a reusable
`Query::referenced_fields()` helper — **also consumed by sub-project 4's
projection push-down**, so it lives in `query/` and is public within the
crate), and check each against the store schema unless `lenient` is set. The
error is a new `ExecError::UnknownColumn { name, suggestion, known }` (or the
existing error enum extended), surfaced with the same `querymatter: {err:#}`
discipline as every other query error.

### 8.3 The helper

```rust
impl Query {
    /// Every distinct frontmatter field name this query references, across
    /// SELECT (incl. nested expr/agg args), WHERE, GROUP BY, ORDER BY,
    /// HAVING, and MEMBER OF. `file.*` pseudo-columns are excluded (they are
    /// validated at parse time). Used by column validation (W12) and by
    /// projection push-down (sub-project 4, W17).
    pub fn referenced_fields(&self) -> BTreeSet<String>;
}
```

`SELECT *` contributes no specific field (it means "all"), which
sub-project 4 will treat as "cannot prune"; for validation, `*` references
nothing to typo, so it is simply skipped.

## 9. Friendly "not supported" messages (W7)

Several `ParseError::Unsupported` fallback arms format the offending
`sqlparser` node with `{:?}`, leaking a multi-line Rust struct dump (WHERE
expression, value literal, query body, count argument, and any new fallbacks
this batch adds). Replace each with the clean phrasing the named-clause
rejections already use via the `unsupported()` helper — a short human phrase
naming what is not supported, no `{:?}`. Every new "not supported" path this
sub-project introduces (agg-inside-expr, non-literal `MEMBER OF`, HAVING
without GROUP BY, etc.) uses the same clean phrasing from the start.

## 10. Invariants this batch depends on

- **`to_cmp_string()` is the canonical no-`Eq`/`Hash` keying** for DISTINCT and
  `count(distinct)`; DISTINCT reuses it rather than inventing a second scheme.
- **`Value::display`-equivalent string conversion** is what scalar/concat use
  to stringify non-string operands, so a field renders identically whether
  selected bare or passed through `lower`/`||`.
- **The store schema is the union of field names** (`RecordStore::schema()`);
  W12 validates against it, and it is the same set `.schema` shows and
  sub-project 2's `.describe` will enrich.
- **Existing NULL 3-valued logic** in `eval_predicate` is the model
  `MemberOf` and expression comparisons follow — a `Null`/absent operand
  yields *unknown*, not *false*.

## 11. Testing

Per item, plus cross-cutting:

- **Expr/scalar/arithmetic:** unit tests for `apply_scalar` (each function,
  incl. null propagation, arity errors, substr clamping, replace) and
  `apply_binary` (each op, int/float promotion, div/mod-by-zero → Null,
  null propagation, concat null). Parse+exec round-trip for `SELECT lower(x)`,
  `SELECT (a/b) AS r`, `SELECT p || '-' || j`, `WHERE start < end`,
  `WHERE lower(status)='draft'`.
- **MEMBER OF:** a list field with/without the element, negation, a `Null`
  and a non-list field (unknown, not matched), `NOT MEMBER OF`.
- **HAVING:** `HAVING count(*) > 1` drops small groups; HAVING referencing an
  aggregate not in SELECT; HAVING without GROUP BY errors.
- **GROUP BY alias:** `GROUP BY s` resolves a SELECT `AS s`; an alias on an
  aggregate/computed expr rejected.
- **DISTINCT:** dedups a projection; DISTINCT+GROUP BY rejected; DISTINCT
  before ORDER BY.
- **ORDER BY agg:** `ORDER BY count(*) DESC`; ungrouped rejected.
- **W12:** unknown column errors with a suggestion; a near-miss suggests the
  right name; `--lenient` restores Null; empty vault skips validation; a
  typo inside `lower(...)`/an aggregate arg/`GROUP BY`/`HAVING` is caught;
  `file.*` still validated at parse time; `referenced_fields()` unit test.
- **W7:** an unsupported WHERE expression / literal / query body produces a
  clean phrase, asserted to **not** contain `{`/`}` AST-dump characters.
- **Regression guard:** the committed render snapshots
  (`table_snapshot.snap`, `md_snapshot.snap`) stay byte-identical — this batch
  changes the query engine, not rendering.

## 12. Files touched

| file | change |
|---|---|
| `src/query/ast.rs` | `Expr`, `ScalarFn`, `BinOp`, `SelectExpr::Expr`, `Predicate::Compare`/`MemberOf`, `Having`/`HavingLeaf`, `Query.having`/`distinct`, `OrderKey` agg target |
| `src/query/parse.rs` | lower the above; remove HAVING/DISTINCT rejections; alias-aware `lower_group_by`; friendly `unsupported()` phrasing; arity checks |
| `src/query/exec.rs` | `apply_scalar`, `apply_binary`, expr evaluation, `MemberOf` eval, HAVING filter step, DISTINCT dedup, agg ORDER BY, `execute`-time column validation, `Query::referenced_fields` |
| `src/query/mod.rs` | re-exports / error type extension if needed |
| `src/cli.rs` | `--lenient` flag |
| `src/settings.rs`, `src/config.rs` | `lenient` setting through the precedence resolver + `ConfigKey` |
| `src/session.rs` | thread `lenient` into `execute` |
| `README.md` | document the new query surface, `--lenient`, the config key |
