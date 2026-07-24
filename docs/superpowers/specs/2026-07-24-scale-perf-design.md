# Scale & performance — design

Date: 2026-07-24
Status: approved
Batch: WHATS-NEXT sub-project 4 of 4 (scale-perf, FINAL)

## 1. Problem

Three forward-looking capacity improvements — headroom for vaults an order of
magnitude larger than typical use today. None changes query results or default
output; they change how fast/cheaply the same results are produced.

- **W6** `GROUP BY` bucketing is a linear scan (`groups.iter_mut().find`),
  O(records × distinct-groups) — quadratic at high group cardinality.
- **W16** discovery reads + YAML-parses every file serially on one core.
- **W17** every field of every file is cloned into a `Record` even for a
  narrow query (`SELECT count(*)`, `SELECT status`).

Changes are confined to `src/query/exec.rs` (W6), `src/discover.rs`/`src/store.rs`/
`src/cache.rs` (W16), and the store-materialization path (W17). The query engine's
*results* and the default rendered output are untouched.

## 2. Hashed GROUP BY bucket key (W6)

`exec::group_rows` today builds groups with:

```rust
match groups.iter_mut().find(|group| group.key == key) { ... }
```

— a linear scan over all existing groups for every record. Replace the lookup
with a hash map from a hashable form of the group key to the group's index (or
to the accumulating group), so bucketing is amortized O(records).

- The group **key** is a `Vec<Value>` (the grouping columns' values); `Value`
  has no `Hash`/`Eq`. Reuse the existing canonical string form —
  `to_cmp_string()` per cell, joined (the same keying `count(distinct)` and
  `DISTINCT` already use) — as the `HashMap<String, usize>` key mapping to the
  group's position in the `Vec<Group>`. This preserves first-occurrence group
  order (push a new group when the key is unseen; the map only accelerates the
  "have I seen this key" test).
- **Output order must be byte-identical:** `execute_grouped` sorts groups by
  key afterward (`groups.sort_by(compare_key_tuple)`), so the result is
  unchanged regardless of insertion order; but keep first-occurrence insertion
  order anyway so any pre-sort behavior and the ungrouped fallbacks are
  unaffected. The committed snapshots must stay byte-identical.
- Confirm the `to_cmp_string()`-join key cannot collide two distinct keys
  (`("a","b")` vs `("ab","")`) — join with a separator that can't appear in a
  cell, or key on the `Vec<String>` of per-cell strings (a `Vec<String>` is
  `Hash`), which avoids the ambiguity entirely. Prefer the `Vec<String>` key.

This is a pure internal optimization: no API, flag, or behavior change.

## 3. Parallel file scan (W16)

Discovery's expensive step — `fs::read_to_string` + YAML parse per file — runs
serially in `store::scan_root`, `cache::refresh_per_file`, `cache::refresh_fast`,
and `cache::build_vault`, each looping `for path in discover(root, opts)` and
calling `cache::scan_file` one file at a time. `cache::scan_file(dir, path)` is
already a pure, side-effect-free `path -> ScanResult` unit — embarrassingly
parallel.

Parallelize the per-file scan using the `ignore` crate already in the tree (no
new dependency), OR a simple threadpool over the discovered paths:

- **Approach:** keep `discover()` producing the path list (or use `ignore`'s
  `build_parallel()`), then run `scan_file` across the paths concurrently,
  collecting results through a channel (`std::sync::mpsc`) or a parallel
  iterator. Use `std::thread::available_parallelism()` for the worker count.
- **Determinism:** the current serial scan yields records in discovery order
  (discover returns a globally path-sorted list). A parallel scan completes in
  arbitrary order, so **the collected records/warnings MUST be sorted by path
  afterward** to reproduce the exact same order — otherwise a query without a
  total `ORDER BY` could change row order and break the byte-identical-output
  invariant. Sort collected `(path, record)` (and warnings) by path before
  building the slices, matching today's order exactly.
- **Warnings order:** the per-file skip/parse warnings (`report.warnings`) must
  also be deterministic — sort them by path too, so stderr diagnostics don't
  reorder run-to-run.
- **No new dependency** unless `ignore`'s parallel walker proves insufficient;
  if a threadpool crate is genuinely needed, STOP and flag it (prefer
  `std::thread` + `mpsc`, or `ignore::WalkParallel`, both dependency-free).

The scan is CPU/IO-bound and pure per file; the only shared state is the result
collector. No query-result change: same records, same order, same warnings.

## 4. Projection push-down (W17)

### 4.1 The optimization

When the query (or queries) are known **before** the store is built, only the
fields the query references need to be materialized into each `Record` — a
`SELECT count(*)` needs no field values, `SELECT status` needs one. Today
`store::scan_root` and `cache::records_from` clone **every** field of every file
into the `Record`.

Reuse `Query::referenced_fields()` (built in sub-project 1, already public in
the crate and designed for this second consumer): compute the union of
referenced fields across every statement in the run, and materialize only those
field **values** into each `Record`.

### 4.2 Where it applies (and where it must NOT)

Push-down applies **only** when every statement of the run is known up front and
none needs all fields:

- **Applies:** one-shot `-e`, piped batch, and `query run` — the statement text
  is known before store construction.
- **Does NOT apply (materialize all fields):** the interactive REPL — the store
  outlives any single query, and later queries reference different fields; and
  any run where a statement projects `*` (`SelectExpr::Star`) — `SELECT *`
  needs every field.
- If pruning is disabled, behavior is exactly today's (full materialization).

**The on-disk `.querymatter` cache always stores all fields.** Pruning happens
only when materializing `Record`s *from* the cache (`records_from`) or *from* a
live scan (`scan_root`) at query time — never when *building*/refreshing the
cache (`build_vault`/`refresh_*` keep every field, so a later query for a
different field still finds it). W17 threads `wanted` into the read/materialize
path, not the cache-write path.

The wanted-field set is `Option<&BTreeSet<String>>` threaded into the
store-building path (`InMemoryStore::load`, `InMemoryStore::from_cache`,
`records_from`, `scan_root`): `None` → keep all fields (today's behavior);
`Some(set)` → clone only the fields in `set` when building each `Record`.

### 4.3 The load-bearing correctness constraint: preserve W12 validation

Sub-project 1's unknown-column validation (`--lenient` off by default) checks a
query's `referenced_fields()` against `RecordStore::schema()` (the union of all
field NAMES) and offers a did-you-mean from the full schema. **Pruning field
VALUES must not shrink the field-NAME universe** the store reports, or:
- a typo'd column could stop erroring (if the schema only listed referenced
  fields), and
- the did-you-mean suggestion pool would collapse to just the query's own
  fields.

Therefore: the store retains the **full schema** (the union of every file's
frontmatter field names), computed cheaply during the scan (the parser already
reads every field name; collecting the name set costs nothing extra), while
pruning only the per-`Record` field **value** map. `store.schema()` returns the
full name set regardless of pruning.

Concretely: `scan_file` still parses the whole frontmatter block (that cost is
unavoidable); the store records each field NAME into its schema set, but the
`Record` it builds keeps only the wanted field VALUES. So `SELECT staus` (a
typo) under push-down still errors with a did-you-mean drawn from the full
schema, identical to without push-down. **This equivalence gets an explicit
test.**

### 4.4 What push-down changes and doesn't

- **Changes:** memory footprint and `Value`-clone count for narrow one-shot
  queries; the materialized `Record`s carry only referenced fields.
- **Does NOT change:** query results, row order, column validation behavior,
  the did-you-mean pool, the schema surface, or any REPL behavior. A one-shot
  query's output is byte-identical with and without push-down.

## 5. Invariants this batch depends on

- **`to_cmp_string()` is the canonical no-`Eq`/`Hash` keying** (W6 reuses it;
  prefer the collision-free `Vec<String>` key).
- **`discover()` yields a path-sorted list** and the serial scan preserves that
  order — W16 must re-sort after parallelizing to reproduce it exactly.
- **`Query::referenced_fields()` is complete** (verified in sub-project 1's
  final review across every column position) — W17's pruning correctness rests
  on it; a missing position would prune a needed field.
- **`RecordStore::schema()` is the union of field NAMES** and backs W12
  validation + did-you-mean — W17 keeps it full even when values are pruned.
- **The committed render snapshots stay byte-identical** — all three items are
  performance-only.

## 6. Testing

- **W6:** a grouped query over records with many distinct group keys returns
  the identical result to today (unit + the existing grouped-query tests
  unchanged); a group key collision case (`("a","b")` vs `("ab","")`) buckets
  into two distinct groups (the collision-free-key test); insertion/first-
  occurrence order preserved; the committed snapshots byte-identical.
- **W16:** the parallel scan produces the SAME records as the serial scan for a
  multi-file tree (compare the record set + order); a tree with some skipped
  (no-frontmatter) and some malformed files yields the same records AND the
  same warnings in the same (path-sorted) order; a determinism test: two runs
  over the same tree produce identical row order for a query with no
  `ORDER BY`. `init` (build_vault) and a PerFile refresh both still produce a
  cache that a query reads identically.
- **W17:** `referenced_fields`-driven pruning materializes only the referenced
  fields (unit test on the store/records-from path with a wanted-set);
  **a typo'd column under push-down still errors with the correct did-you-mean**
  (the W12-equivalence test — the load-bearing one); `SELECT *` disables pruning
  (all fields present); `count(*)` prunes to zero fields yet counts correctly;
  a one-shot query's rendered output is byte-identical with and without
  push-down; the REPL path is unaffected (store keeps all fields).
- **Cross-cutting:** the full existing suite passes unchanged; `git diff main --
  src/snapshots/` empty; every scale change is behavior-preserving.

## 7. Files touched

| file | change |
|---|---|
| `src/query/exec.rs` | W6: hash-keyed `group_rows` bucketing |
| `src/discover.rs` | W16: parallel walk/scan seam (or `WalkParallel`) |
| `src/store.rs` | W16: parallel `scan_root` + sort-by-path; W17: `wanted`-field pruning + full-schema retention |
| `src/cache.rs` | W16: parallel `refresh_per_file`/`refresh_fast`/`build_vault` + sort; W17: `records_from` pruning |
| `src/main.rs` | W17: compute the wanted-field union for one-shot/`-e`/batch/`query run` and thread it into store construction (None for the REPL / `*`) |
| `src/session.rs` | W17: thread the wanted-field set if the seam runs through `Session` |
| `README.md` | a brief note that large-vault scans are parallelized and narrow one-shot queries prune unread fields (performance notes; no user-facing behavior change) |
