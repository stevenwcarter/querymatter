# querymatter — query-power bundle (whats-next W51–W57)

- **Date:** 2026-07-25
- **Status:** Approved (brainstormed, ready for planning)
- **Source:** `whats-next --execute` bundle of 7 items: W51, W52, W53, W54, W55, W56, W57.

## 1. Overview

Seven forward-looking improvements to `querymatter`: a regex predicate, one-step
completion install, a work-stealing parallel scan, a vault-level config layer,
live REPL tab-completion, a queryable Markdown body, and a real date type. Two of
them (W56 body word-count, W57 date values in cached fields) change the on-disk
cache and share a single `SCHEMA_VERSION` bump; the other five are independent.

### Locked-in decisions (from brainstorming)

1. **W57 date model = Hybrid:** auto-detect *strict* ISO-8601 at ingest
   (`YYYY-MM-DD` → `Value::Date`, RFC3339 → `Value::DateTime`); everything else
   stays `Value::Str`. `DATE(x)` / `DATE(x, fmt)` casts any string on demand.
2. **W56 body storage = Lazy:** cache `file.word_count` only; read `file.body`
   from disk at eval time. Under `--force-cache` (or a since-moved/changed file),
   `file.body` yields a clear diagnostic/NULL, never a silent wrong answer.
3. **W51 regex operand = general `Expr`** (`Predicate::Regexp(Expr, String,
   negated)`), per the item — more capable than today's column-only `LIKE`.
4. **W54 precedence:** `flag > env > vault > config > default`.
5. **W56+W57 share one `SCHEMA_VERSION` bump** (2 → 3).

## 2. Goals / non-goals

**Goals:** implement all 7 items; keep existing query results and rendered output
byte-identical where an item doesn't explicitly change them; TDD every item;
update README + config docs.

**Non-goals:** the unchecked whats-next items. `EXTRACT`/`DATEDIFF`/day-of-week
(W57 stretch) are out of core scope — only the `Value::Date`/`DateTime` type,
auto-ISO detection, `DATE()` cast, chrono comparison, and stable rendering are
required. W43 (widening `LIKE`/`IN`/etc. to `Expr`) is not in scope — only regex
gets an `Expr` operand.

## 3. W57 — `Value::Date` / `DateTime` (hybrid)

The highest-risk item: auto-detection changes the *type* of existing ISO-date
frontmatter fields, so its correctness rests on preserving observable behavior.

### Design
- Add `Value::Date(NaiveDate)` and `Value::DateTime(DateTime<Utc>)` to
  `src/model.rs`'s `Value` (serde/bincode — chrono types serialize; confirm the
  `serde` chrono feature is available or store as a normalized string internally).
- **Ingest auto-detection** (`frontmatter::pod_to_value` / the string→Value path):
  a string that parses as **strict** `YYYY-MM-DD` → `Value::Date`; strict RFC3339
  (`…T…Z`/offset) → `Value::DateTime`. Strictness: reject partial forms
  (`2026`, `2026-07`), leading-zero ints (already `Value::Int`), and anything
  chrono's strict parse rejects. Everything else stays `Value::Str`.
- **`DATE(x)` / `DATE(x, fmt)` scalar fn** (`ScalarFn`, lowered + evaluated):
  parses `x` (any string/date) to `Value::Date` using ISO by default or the
  explicit chrono `fmt`; unparseable → `Value::Null`.
- **Comparison:** extend `compare_values` (`src/model.rs`) so two dates/datetimes
  compare by instant; a date/datetime vs. a string coerces (parse the string as a
  date, or render the date as ISO text and compare) so relative-date literals keep
  working; NULLs still last.
- **Rendering:** `Value::display` / `to_json` / `to_cmp_string` render a date as
  its canonical ISO text (`%Y-%m-%d` / RFC3339), so table/csv/tsv/md/json/vertical
  output is unchanged for fields that used to be ISO strings.

### Invariants this feature depends on (pin each with a test)
- **I1 — stable rendering:** a `Value::Date`/`DateTime` renders byte-identically
  to its source ISO text across every format (existing insta snapshots stay green;
  add explicit date-render tests).
- **I2 — relative-date filters unchanged:** `WHERE created > '-7d'` (and the
  CASE / ORDER-BY-expr paths from the prior bundle) still filter correctly when
  `created` is now `Value::Date`. Characterization test at that seam.
- **I3 — ORDER BY / MIN / MAX on an ISO-date field:** results identical to the
  pre-change string ordering (lexical == chronological for strict ISO), NULLs last.
- **I4 — mixed Date/Str column:** a field that is a date in some files and a
  non-date string in others has a defined, panic-free total order.
- **I5 — non-dates untouched:** `010`, `1.2.3`, `2026`, `2026-07`, `v1` stay
  `Value::Str`/`Value::Int` (explicit tests).

### Cache
- Dates live inside `CachedFile.fields` (already `IndexMap<String, Value>`), so
  the bincode blob now contains date variants → `SCHEMA_VERSION` 2 → 3 (shared
  with W56). A pre-bump cache is discarded/rebuilt as usual.

## 4. W56 — queryable Markdown body (lazy)

### Design
- `frontmatter::extract` already parses `parsed.content` (the body after the
  fence) and discards it. Capture it; compute a **word count** (whitespace-split)
  at scan time.
- `CachedFile` gains `word_count: usize` (the schema bump, shared with W57). No
  body text is stored.
- Expose two `file.*` pseudo-columns via `FileAttr` (`src/model.rs`):
  - `file.word_count` → the cached count (pure, always available).
  - `file.body` → **reads the file from disk at eval time** using the record's
    absolute path. This is the first `FileAttr` that performs I/O; isolate the
    read behind a single helper and handle failure explicitly.
- **`--force-cache` / missing file:** a query referencing `file.body` when the
  filesystem is off-limits (ForceCache) or the file is unreadable returns a clear
  diagnostic (strict) or `Value::Null` (lenient) — never a silent wrong answer.

### Invariants (pin with tests)
- `file.word_count` matches the body's whitespace-word count for a known fixture.
- `file.body` returns the on-disk body for a normal query; returns NULL/diagnostic
  under `--force-cache`; and its LIKE/scalar-fn use works (`WHERE file.body LIKE
  '%TODO%'`).
- Frontmatter-only queries do NOT read bodies (no I/O regression): a query that
  doesn't reference `file.body` never touches the file at eval time.

### Item → files: `src/frontmatter.rs`, `src/cache.rs` (CachedFile + scan +
SCHEMA_VERSION), `src/model.rs` (FileAttr), `src/query/exec.rs` (FileAttr eval +
the body-read helper), `src/store.rs` (thread path if needed).

## 5. W51 — regex predicate

- Add `Predicate::Regexp(Expr, String, /* negated */ bool)` to `src/query/ast.rs`
  (general `Expr` left operand — intentionally more capable than `LIKE`'s
  `ColRef`).
- Parse `expr REGEXP 'pat'` and `expr NOT REGEXP 'pat'` in `lower_predicate`
  (`src/query/parse.rs`), alongside `LIKE`. Reject an un-compilable pattern at
  parse time with a clear message (mirror the exclude-glob validation pattern).
- Evaluate in `eval_predicate` (`src/query/exec.rs`): compile the `regex::Regex`
  (case-sensitive; user writes `(?i)` / anchors explicitly), test against the
  left operand's `Value::display`. NULL left operand → no match (3VL).
- **Every exhaustive `match` on `Predicate` must gain the `Regexp` arm** (compiler
  enforces: `eval_predicate`, `rewrite_predicate_literals`, `predicate_columns`/
  `collect_predicate_fields`). Tests: match/non-match, `NOT REGEXP`, bad pattern
  rejected, regex over a scalar-fn operand (`lower(status) REGEXP …`).

## 6. W52 — one-step completion install

- Add `completions --install [shell]` (extend `CompletionsArgs` in `src/cli.rs`
  with an `--install` flag; shell optional, auto-detected from `$SHELL` when
  omitted).
- `run_completions` (`src/main.rs`): resolve the shell's user completion dir
  (bash: `~/.local/share/bash-completion/completions/`; zsh: a writable `fpath`
  entry or `~/.zsh/completions`; fish: `~/.config/fish/completions/`), create it,
  write the generated script there, and confirm on stderr.
- **Non-writable / undetectable dir:** fail with a clear message and fall back to
  printing the script to stdout (today's behavior) so the user is never stuck.
- Tests: `--install bash` writes to a temp `$HOME`'s expected path; unwritable
  target errors clearly. (Use a `$HOME`/dir override for testability.)

## 7. W53 — work-stealing parallel scan

- Replace `map_paths`'s static contiguous chunking (`src/parallel.rs`) with a
  shared `AtomicUsize` cursor: each worker `fetch_add`s the next index and
  processes that path until the cursor passes the end.
- **Determinism invariant (already pinned by the module's tests):** results are
  still sorted by path and byte-identical to the serial path regardless of worker
  count/scheduling. The final `sort_by(path)` stays. Add a test that a
  size-skewed workload still returns identical, correctly-ordered results.
- Preserve the existing panic-propagation semantics.

## 8. W54 — vault-level config layer

- Add `Source::Vault` to `src/settings.rs`'s `Source` (between `Env` and
  `Config`, so precedence is `flag > env > vault > config > default`).
- Discover a vault config: walk up from the scan root / cwd for a
  `.querymatter.toml` file (independent of whether a `.querymatter/` cache dir
  exists). Load it with the same `Config` schema/parse as the per-user config.
- Splice it into `Settings::resolve` / `resolve_walk` as a layer between env and
  the user config: a key set in the vault config beats the user config but loses
  to a flag/env.
- `config list` / `.settings` show `(vault)` as the source when a vault config
  wins. Tests: vault beats config, flag/env beat vault, absent vault file is a
  no-op, malformed vault file errors naming the path.

## 9. W55 — live REPL tab-completion

- Today `ReplHelper`'s `schema` + `query_names` are snapshotted once at REPL
  start (`src/repl.rs`). After a mutation, recompute and push into the live helper
  via `editor.helper_mut()`.
- Trigger points: `.reload`, `.refresh`, `.refresh-all` (schema may change), and
  `.query save` (a new saved-query name — **the user's note**). Extract a
  `refresh_helper(&mut editor, &session)` step and call it after each.
- Tests: unit-test the snapshot recomputation (schema/query-name lists) since the
  live editor push isn't drivable headless (matches the codebase's REPL-test
  convention); assert `.query save` adds the name to the recomputed snapshot.

## 10. Test strategy

- Unit tests co-located per module. Integration (`tests/cli.rs`) for CLI-visible
  items: `REGEXP` end-to-end, `completions --install`, `file.body`/`file.word_count`
  queries (incl. the `--force-cache` diagnostic), a `DATE()` query, and a
  vault-config precedence run.
- **W57 is characterization-heavy:** every I1–I5 invariant gets a test; existing
  insta snapshots and relative-date/CASE/ORDER-BY tests MUST stay green (proof
  auto-detect didn't shift observable behavior).
- The cache SCHEMA_VERSION bump: a stale (v2) cache is rejected/rebuilt cleanly.

## 11. Documentation

- README: `REGEXP`/`NOT REGEXP` in the DSL; `DATE()` + the auto-ISO date behavior
  (and the mixed-column note); `file.body` / `file.word_count` pseudo-columns
  (with the lazy/`--force-cache` caveat); `completions --install`; the vault-level
  `.querymatter.toml` config layer + precedence; a note that `.reload`/`.refresh`/
  `.query save` now refresh completion.
- Note the `SCHEMA_VERSION` bump (existing caches rebuild on next run).

## 12. Item → primary files

| Item | Primary files |
|------|---------------|
| W51 regex | query/ast.rs, query/parse.rs, query/exec.rs |
| W52 completions install | cli.rs, main.rs |
| W53 work-stealing | parallel.rs |
| W54 vault config | settings.rs, config.rs, cache.rs (or new discovery), main.rs |
| W55 live completion | repl.rs |
| W56 body (lazy) | frontmatter.rs, cache.rs, model.rs, query/exec.rs, store.rs |
| W57 dates | model.rs, frontmatter.rs, cache.rs, query/ast.rs, query/parse.rs, query/exec.rs, render.rs |
