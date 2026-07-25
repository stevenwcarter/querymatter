# Nested values, dates & file metadata — design

Date: 2026-07-24
Status: approved
Batch: WHATS-NEXT follow-up (leftover items after sub-projects 1–4)

## 1. Problem

Five forward-looking items from `WHATS-NEXT.md`, bundled into one spec because
they share plumbing (the `Value` type, the `file.*` seam, the query lowering
path). The original triage was written against a pre-sub-project snapshot; this
spec is reconciled against the **current** code (W27 arithmetic/concat and the
W30 expression tree already shipped — dropped from scope).

- **W25** `Value` collapses nested YAML mappings (`Pod::Hash`) into an opaque
  string, so nested frontmatter can't be queried by dotted path or exported
  with fidelity.
- **W24** every file's `(mtime, size)` is already stat'd for cache freshness,
  then discarded when building a `Record` — no `file.mtime` / `file.size`
  columns exist despite the data being in hand.
- **W29** dates can only be filtered against a hard-coded literal
  (`WHERE created >= '2026-07-17'`); there's no "last 7 days"-style relative
  literal.
- **W22** `COALESCE(a, b, 'none')` is rejected as an unsupported function.
- **W26** a query scoped to one subtree still decodes the entire vault cache and
  re-walks the whole vault for freshness before narrowing — query cost tracks
  vault size, not subtree size.

Scope is confined to `src/model.rs`, `src/frontmatter.rs`, `src/query/{ast,
parse,exec}.rs`, `src/render.rs`, `src/cache.rs`, `src/store.rs`, `src/main.rs`,
plus `Cargo.toml` (one new dependency) and `README.md`.

## 2. Decisions locked in brainstorming

1. **One combined spec** (not decomposed into sub-projects).
2. **Date model: ISO-8601 strings, lexicographic comparison.** No `Value::Date`
   type. `file.mtime` is an ISO-8601 UTC string; relative-date literals resolve
   to concrete ISO date strings; comparison stays querymatter's existing
   numeric-else-lexicographic rule, which already orders ISO-8601 correctly.
   Frontmatter dates are **assumed ISO-8601** — documented as a load-bearing
   invariant (§9).
3. **W26 narrows the validation surface** to the queried subtree, accepted as
   the cost of not reading the rest of the vault (§7).
4. **New dependency: `chrono`** (crate-policy default for civil time) — used
   only to format `SystemTime` → ISO-8601 UTC and to do relative-literal
   calendar arithmetic (days/weeks/months/years). No `chrono-tz`; UTC only.

## 3. W25 — nested-map `Value` + dotted paths + JSON export

### 3.1 The type

`Value` gains a map variant, mirroring the existing `List` recursion:

```rust
pub enum Value {
    Null, Bool(bool), Int(i64), Float(f64), Str(String),
    List(Vec<Value>),
    Map(IndexMap<String, Value>),   // NEW — insertion-ordered nested mapping
}
```

`frontmatter::pod_to_value` stops collapsing `Pod::Hash`:

```rust
Pod::Hash(map) => Value::Map(map.into_iter().map(|(k, v)| (k, pod_to_value(v))).collect()),
```

`compact_pod` is **retained** and reused as `Value::Map`'s display form, so the
rendered `{k: v}` string is byte-for-byte what it is today (§9 invariant). A new
`Value::Map` arm is added to `display()` (compact `{k: v}`, keys sorted for
determinism — matching today's `compact_pod`), `variant_name()` (`"Map"`),
`as_number()` (`None`), and `to_cmp_string()` (via `display()`). `compare_values`
needs no new arm: a `Map` isn't a number, so it falls to the lexicographic
`to_cmp_string()` branch (well-defined, not a meaningful sort key — same status
as `List` today).

### 3.2 Dotted-path column references

`ColRef::Field(String)` becomes `ColRef::Field(Vec<String>)` — a path of one or
more segments. A single-segment path is exactly today's behavior.

- **Parse:** `lower_compound` already special-cases `file.<attr>`. For any other
  compound identifier (`estimate.low`, `a.b.c`), it now returns
  `ColRef::Field(vec!["estimate", "low"])` instead of erroring `unsupported
  compound column`. A bare `Identifier` becomes `ColRef::Field(vec![name])`.
  The `file.` prefix stays reserved: a first segment equal to `file`
  (case-insensitive) with exactly two segments routes to `file_attr_from_str`;
  a `file.x.y` three-segment form is a parse error (`file.*` has no nesting).
- **Resolve:** `Record::field` takes the path and walks it: segment 0 looks up
  the top-level `IndexMap`; each subsequent segment indexes into a
  `Value::Map`. A missing key, or a non-`Map` intermediate, yields `Value::Null`
  (same "absent → Null" contract as today's single-field lookup). Under
  `--lenient` this is the terminal behavior; under default mode see §3.4.

### 3.3 JSON export fidelity

`render::to_json` gains a `Value::Map` arm producing a real nested object:

```rust
Value::Map(m) => JsonValue::Object(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect()),
```

So `-o json` emits full-fidelity nested frontmatter. Table, CSV, and TSV keep
using `Value::display()` (the compact `{k: v}` string) — CSV/TSV are flat by
definition and the table view is unchanged (§9 invariant).

### 3.4 Interaction with W12 unknown-column validation

`referenced_fields()` and `collect_col_field()` contribute only the **top-level
segment** of a path (`estimate` for `estimate.low`) — that is the name that
exists in the store schema. Sub-keys are dynamic and are **not** validated
(nested shapes vary file to file; validating them would produce false
"unknown column" errors). So `WHERE estimate.high > 10` validates that
`estimate` exists; if `estimate` is absent from the schema, the existing W12
error fires on `estimate`; if present but not a map (or the sub-key is absent),
resolution yields `Null` per §3.2. `ColRef::label()` renders a path
dot-joined (`estimate.low`) for headers/errors.

### 3.5 Cache

Adding `Value::Map` changes the bincode encoding of `CachedFile.fields`. Bump
`SCHEMA_VERSION` `1 → 2`. The existing `MAGIC ++ SCHEMA_VERSION` header check
already discards a mismatched cache with a warning and rebuilds, so an old
`.querymatter` is invalidated cleanly with no migration code.

## 4. W24 — `file.mtime` / `file.size` pseudo-columns

### 4.1 Threading the already-collected stat

`FileAttr` gains `Mtime` and `Size`. `Record` gains two fields (`mtime:
SystemTime`, `size: u64`); `Record::new` takes them as parameters. The
`(mtime, size)` are already produced by `cache::scan_file` (live scan and cache
build) and stored on `CachedFile` — thread them into `Record` at **both**
construction sites (§9 enumerates the two producers that must survive):

- `store::scan_root` (live-scan path) — has the `fs::Metadata` in hand.
- `cache::records_from` (cache-materialization path) — reads `CachedFile.mtime`
  / `.size`.

No new I/O: the stat is already read for freshness.

### 4.2 Representation

- `file.size` → `Value::Int(bytes as i64)`.
- `file.mtime` → `Value::Str(<ISO-8601 UTC>)` via a new
  `model::system_time_to_iso(SystemTime) -> String` helper (chrono:
  `DateTime::<Utc>::from(st).to_rfc3339_opts(SecondsFormat::Secs, true)` →
  `2026-07-20T10:30:00Z`). A pre-epoch mtime (clock skew / archive extraction —
  the cache already tolerates these) formats to its real negative-year/pre-1970
  RFC3339 string; it never panics.

`file_attr_from_str` gains `"mtime"` / `"size"`; `file_attr_label` gains
`file.mtime` / `file.size`. These are the sanctioned single extension points
already used by the four existing attrs.

### 4.3 Why mtime-as-string composes

An ISO-8601 UTC datetime string sorts lexicographically consistently with date
strings: `ORDER BY file.mtime DESC` works; `WHERE file.mtime < '2026-01-01'`
works (a `2026-…T…` datetime is lexicographically ≥ the `2026-01-01` date
prefix); and it composes with W29's relative-date literals (§5), which resolve
to the same ISO string space.

## 5. W29 — relative-date literals

### 5.1 Syntax

A **quoted** literal whose string matches the relative-date grammar
(case-insensitive), so it tokenizes as an ordinary SQL string and no new
sqlparser support is needed:

```
today | now | [+-]<int>(d|w|mo|y)
```

- `today` → current date, date form `YYYY-MM-DD`.
- `now` → current instant, datetime form `YYYY-MM-DDTHH:MM:SSZ`.
- `-7d`, `+3w`, `-2mo`, `-1y` → offset from **today** (calendar arithmetic:
  `mo`/`y` are calendar months/years via chrono, not fixed 30/365-day spans),
  date form `YYYY-MM-DD`. Sign is required for offsets. `d`/`w` = days/weeks;
  `m` is intentionally NOT a unit (month/minute ambiguity) — months are `mo`.

Any string not matching this grammar stays a plain `Literal::Str` (no behavior
change for existing string literals — the grammar is strict and anchored).

### 5.2 AST + parse

`Literal` gains a variant:

```rust
pub enum Literal { Str(String), Int(i64), Float(f64), Bool(bool), Null,
    RelativeDate(RelDate),   // NEW
}
pub enum RelDate { Today, Now, Offset { n: i64, unit: DateUnit } }
pub enum DateUnit { Day, Week, Month, Year }
```

String-literal lowering (`string_literal` / the `Value::SingleQuotedString`
arm) tries `RelDate::parse(&s)` first; on match it produces
`Literal::RelativeDate`, else `Literal::Str(s)`. `parse()` is pure (no clock),
so the parser stays deterministic and unit-testable.

### 5.3 Exec — resolution against an injected `now`

Relative dates resolve to concrete `Value::Str` at execution time, where a clock
is available and injectable for tests:

- `execute()` gains an internal seam: the public entry computes
  `now = SystemTime::now()` and calls an inner `execute_at(query, store, now)`;
  tests call `execute_at` with a fixed instant.
- Before validation/pipeline, a pass rewrites every `Literal::RelativeDate` in
  the query to `Literal::Str(<resolved ISO>)` using `now` (UTC). After the
  rewrite, `eval_expr` never sees a `RelativeDate` — evaluation and comparison
  are unchanged (plain string vs field, numeric-else-lexicographic).
- Resolution: `today`/offsets → `chrono::Utc::now().date_naive()` (± the offset)
  formatted `%Y-%m-%d`; `now` → full RFC3339 `…Z`.

`Literal::RelativeDate` is accepted anywhere a `Literal` appears syntactically
(the rewrite is position-agnostic), but is only **meaningful** in a comparison
against a date-shaped field; used elsewhere it simply compares as its resolved
string. `default_header`/`literal_label` render it back as its source token
(`'-7d'`).

## 6. W22 — `COALESCE(...)`

`Expr` gains a variadic variant (COALESCE is n-ary and short-circuits on the
first non-null — neither the fixed-arity `ScalarFn` nor `Binary` fit):

```rust
pub enum Expr { Col(ColRef), Lit(Literal), Scalar(ScalarFn, Vec<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    Coalesce(Vec<Expr>),   // NEW
}
```

- **Parse:** `lower_expr`'s `sql::Expr::Function` arm matches a name of
  `coalesce` (case-insensitive) with ≥1 argument and lowers each argument via
  `lower_expr` (so `COALESCE(lower(a), b, 'none')` works). Zero args is a
  parse-time arity error naming the function.
- **Exec:** `eval_expr` evaluates arguments left-to-right and returns the first
  non-`Null`; all-`Null` (or no non-null) → `Null`.
- **Plumbing (three existing recursive walkers gain a `Coalesce` arm):**
  `exec::expr_columns` (GROUP BY validation + push-down), `ast::collect_expr_
  fields` (`referenced_fields`), and `ast::expr_label` (default header, e.g.
  `coalesce(epic, 'none')`).
- **GROUP BY validation** is unchanged in shape: a `COALESCE` in a non-aggregate
  SELECT item must be composed of grouping-key columns/literals, enforced by the
  existing "every column the expression references is a grouping key" rule via
  `expr_columns`.
- An **aggregate inside** `COALESCE` is rejected, consistent with today's
  agg-in-expr rule (the existing "not supported" phrasing).

## 7. W26 — subtree-scoped cache load

### 7.1 The change

`main.rs` today (query path): `InMemoryStore::from_cache(&vault, …)` decodes
**every** blob and re-walks the **entire** vault for freshness, then
`store.retain_under(&dirs)` drops out-of-subtree slices afterward.

New: when the query carries a `[DIRS]` / `FROM` subtree, thread the
canonicalized subtree(s) into `from_cache` so it:

1. decodes only the `CachedDir` blobs whose directory is at/under a requested
   subtree. The cache stores one blob file per directory (`{hash}.bin`) and the
   small `manifest.bin` lists every `ManifestEntry { dir: PathBuf, blob, … }`.
   So `load_cache` reads the manifest once (cheap) and then filters
   `body.dirs` by `entry.dir.starts_with(subtree)` **before** the per-blob
   `fs::read`/`decode` — out-of-subtree blobs are never touched. (A new
   `load_cache_under(vault_dir, subtree)` or a subtree param on `load_cache`.)
   And
2. scopes the freshness re-walk to those subtrees (the existing
   `refresh_subtree` already proves and implements scoped walking with
   `starts_with`).

The post-hoc `retain_under` is dropped on the scoped path (the store is built
already-scoped). The whole-vault path (no `[DIRS]`/`FROM`) is unchanged.

### 7.2 Applicability boundary

Subtree scoping applies **only** where the subtree is known up front and the
store is short-lived — one-shot `-e`, piped batch, and `query run` — exactly the
W17 push-down boundary. The **REPL is unaffected**: it builds a whole-vault
store that outlives individual queries (a later query may target a different
subtree), so it keeps loading the whole vault.

### 7.3 Accepted behavior change — validation surface

Because the scoped load never decodes out-of-subtree files, the store's
`schema()` (the union of field names, backing W12 unknown-column validation and
the did-you-mean pool) becomes the **subtree's** schema, not the whole vault's.
Consequence: under default (non-`--lenient`) mode, `SELECT foo FROM plans` where
`foo` exists only under `product/` now errors as an unknown column (with a
did-you-mean drawn from `plans/`'s schema). This is accepted (brainstorming
decision): it's arguably more correct — the query only reads that subtree — and
`--lenient` still bypasses validation entirely. It is a documented behavior
change, not behavior-preserving; it gets an explicit test (§8).

## 8. Testing

Per item, plus cross-cutting invariant guards. TDD: each behavior below is a
failing test first.

- **W25 (nested maps):** `pod_to_value` builds a nested `Value::Map` (round-trip
  from YAML with a nested mapping); `estimate.low` resolves the inner value;
  `estimate.high` where `estimate` is absent / a scalar / a list → `Null`;
  `a.b.c` deep path; `file.x.y` is a parse error; `referenced_fields()` for
  `WHERE estimate.high > 10` returns `{estimate}` (top-level only); `-o json`
  emits a nested object for a map field; **the committed table/CSV render for a
  map field is byte-identical to the pre-change `{k: v}` compact string**
  (characterization test — the load-bearing one, §9); the schema-version bump
  invalidates a v1 cache (header-mismatch path).
- **W24 (file meta):** `file.size` = byte count as `Int`; `file.mtime` = the
  expected ISO-8601 UTC string for a known mtime; `ORDER BY file.mtime DESC`
  orders newest-first; `WHERE file.mtime < '<date>'` filters; **both producers
  carry the stat** — one test through the live-scan path (`scan_root`) and one
  through the cache path (`records_from`) both expose `file.mtime`/`file.size`
  (§9 enumerated producers); a pre-epoch mtime formats without panic.
- **W29 (relative dates):** `RelDate::parse` accepts `today`/`now`/`-7d`/`+3w`/
  `-2mo`/`-1y` and rejects `7d` (no sign), `-7m`, `-7x`, `tomorrow`; with a
  fixed injected `now`, `WHERE created >= '-7d'` resolves to the expected ISO
  date and filters; `'today'` and `'now'` resolve to the expected forms;
  `'-7d'` composes with `file.mtime` (`WHERE file.mtime >= '-7d'`); a
  non-matching quoted string (`'draft'`) stays a plain string literal (no
  regression); `parse()` produces `Literal::RelativeDate` with no clock.
- **W22 (COALESCE):** first-non-null across columns; all-null → `Null`; a
  literal fallback (`COALESCE(epic, 'none')`); a nested expr arg
  (`COALESCE(lower(a), b)`); default header `coalesce(epic, 'none')`;
  `referenced_fields` includes both `epic` args; a `COALESCE` of a grouping key
  is a valid GROUP BY projection, and a `COALESCE` over a non-grouping column is
  rejected; zero-arg arity error; agg-inside-coalesce rejected.
- **W26 (subtree load):** a scoped query returns the identical result to today
  for an in-subtree query (compare against the whole-vault-then-retain result);
  the scoped load does **not** read out-of-subtree files (assert via a file
  planted outside the subtree that would surface as a record/warning if read, or
  a decode counter); **the validation surface is subtree-scoped** — a column
  present only outside the subtree errors under default mode and is accepted
  under `--lenient` (the §7.3 behavior-change test, load-bearing); the REPL /
  whole-vault path is unchanged.
- **Cross-cutting:** the full existing suite passes; the committed render
  snapshots (`table_snapshot`, `md_snapshot`, etc.) stay byte-identical except
  where a test explicitly selects a new column; `cargo clippy`/`fmt` clean.

## 9. Invariants this feature depends on

Per the project's spec discipline, the seams a later change could silently break,
each pinned by a test in §8:

- **`Value::Map::display()` == the old `compact_pod` string.** Table/CSV/TSV
  render a nested map identically to before; only `-o json` changes. A future
  change to `display()`/`compact_pod` must keep this or it silently reshapes
  every flat render of a map field. (Test: byte-identical render characterization.)
- **The `(mtime, size)` stat has exactly two producers** — `store::scan_root`
  (live) and `cache::records_from` (cache). W24 must thread the stat through
  **both**; a query that hits the cache and one that hits a live scan must expose
  identical `file.mtime`/`file.size`. (Test: one per producer.)
- **ISO-8601 lexicographic ordering** is the entire basis of the date model: date
  and datetime strings compare correctly under the existing
  numeric-else-lexicographic rule, and `file.mtime` + relative literals live in
  that same ISO string space. A change to `compare_values` or to the mtime/
  relative-literal formatting that breaks ISO lexicographic ordering breaks every
  date query. (Tests: mtime ordering, relative-date filtering.)
- **`referenced_fields()` returns top-level field names only.** W12 validation
  and W17 push-down both consume it; a dotted path contributes its root segment,
  never a sub-key. (Test: `referenced_fields` for a dotted path.)
- **`from_cache`'s scoped path and `retain_under`'s post-hoc path produce the
  same in-subtree records.** W26 replaces the second with the first for scoped
  queries; results must match. (Test: scoped vs whole-vault-then-retain equality.)
- **`RelDate::parse` is strict and anchored** — only the four keyword/offset
  forms match, so no pre-existing plain string literal is reinterpreted as a
  date. (Test: non-matching strings stay `Literal::Str`.)

## 10. Files touched

| file | change |
|---|---|
| `Cargo.toml` | add `chrono = { version = "0.4", features = ["clock"] }` |
| `src/model.rs` | `Value::Map`; `Record.mtime`/`size` + `Record::new` params; `FileAttr::Mtime`/`Size` + `file_attr`; `system_time_to_iso` helper; `display`/`variant_name`/`as_number`/`to_cmp_string` map arms |
| `src/frontmatter.rs` | `pod_to_value` recurses `Pod::Hash` → `Value::Map`; `compact_pod` retained as the map display form |
| `src/query/ast.rs` | `ColRef::Field(Vec<String>)`; `Expr::Coalesce`; `Literal::RelativeDate`/`RelDate`/`DateUnit`; `collect_col_field`/`collect_expr_fields`/`expr_label`/`ColRef::label` updates; `file_attr_label` mtime/size |
| `src/query/parse.rs` | `lower_compound` dotted paths; `file_attr_from_str` mtime/size; `lower_expr` COALESCE; relative-date recognition in string-literal lowering |
| `src/query/exec.rs` | `Record::field` path walk; `Expr::Coalesce` eval; `expr_columns` Coalesce arm; `execute_at(now)` seam + relative-date rewrite pass |
| `src/render.rs` | `to_json` `Value::Map` → nested object |
| `src/cache.rs` | `SCHEMA_VERSION` 1 → 2; thread `(mtime,size)` in `records_from`; scoped decode for W26 |
| `src/store.rs` | `scan_root` threads `(mtime,size)`; `from_cache` scoped-subtree param; schema derives from loaded (scoped) records |
| `src/main.rs` | pass the query's subtree into `from_cache`; drop post-hoc `retain_under` on the scoped path |
| `README.md` | document nested/dotted-path queries, `file.mtime`/`file.size`, relative-date literals, `COALESCE`, and the subtree-scoped-load validation-surface note |
