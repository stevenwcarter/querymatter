# querymatter — CLI / REPL / query ergonomics bundle (whats-next W32–W46)

- **Date:** 2026-07-24
- **Status:** Approved (brainstormed, ready for planning)
- **Source:** `whats-next --execute` bundle of 11 items: W32, W33, W34, W35, W36,
  W37, W38, W40, W44, W45, W46.

## 1. Overview

Eleven independent, small-to-medium improvements to `querymatter`'s command
line, REPL, output rendering, query engine, and cache tooling. They share three
seams — the **config/settings** system, the **render** layer, and the **REPL
dot-command dispatch** — so they ship as one branch with one spec, grouped into
five themes. Each item is developed test-first and lands in dependency order.

### Locked-in decisions (from brainstorming)

1. **Config breadth:** `timer`, `header`, and `quiet` all become first-class
   config keys (the `lenient`/`hidden` pattern), not just per-invocation flags.
2. **Table width (W37):** terminal-width fitting applies to `--format table`
   **only**; Markdown/CSV/TSV/JSON stay byte-reproducible regardless of terminal.
3. **Pipe exec (W45):** `.output |cmd` runs the command through the system shell
   (`sh -c "<cmd>"`), matching sqlite3's popen model.

## 2. Goals / non-goals

**Goals:** implement all 11 items with their advertised behavior; extend
existing patterns rather than introduce new abstractions; keep piped/non-TTY
output byte-identical where the item doesn't explicitly change it; TDD every
item; update README + config-key docs.

**Non-goals:** the unchecked whats-next items (W23, W31, W41–W43, W47–W59) are
out of scope. No new cache schema version is required (no item changes
`CachedFile`/`CachedDir` shape). No regex predicate, no body indexing, no
streaming store.

## 3. Group A — Config-backed toggles

The single cross-cutting piece. Three new boolean settings follow the existing
`lenient`/`hidden` pattern end to end:

- `Config` (config.rs): add `Option<bool>` field per key.
- `ConfigKey`: add variant, update `ALL`, `as_str`, `allowed` (→ `true,false`),
  `set`, `unset`, `get`. (The exhaustive matches force most updates; the
  `all_agrees_with_value_variants` test guards `ALL`.)
- `Settings` (settings.rs): add `Resolved<bool>` field, a `Default`, and wiring
  in `resolve` (via `resolve_bool`) and `cells`.
- REPL `parse_key`/`.set`/`.unset` and `.settings` pick the keys up for free
  (they iterate `ConfigKey::ALL`).

Precedence stays `flag > env > config > default`, per key.

### W35 — `timer` (default `false`)

- Config key `timer`; effective only in the REPL (the `-- N rows` line, which it
  augments, exists only there), so **no CLI flag**.
- REPL session toggle `.timer [on|off]`:
  - no arg → report current state;
  - `on`/`off` → set a **session-level** value (`Source::Session`), like
    `.format`/`.style`, overriding config for the session.
- `config set timer true` (or `.set timer true`) makes it the durable default —
  the user's stated want.
- Timing: wrap `render_statement_counted` in `std::time::Instant::now()` /
  `.elapsed()`. When timer is on, append to the existing stderr row-count line:
  `-- 3 rows (0.012s)` (single-space, 3-decimal seconds). Off → line unchanged.
- New `Session` methods: `timer() -> bool`, `set_timer(bool)`.

### W32 — `header` (default `true`)

- CLI flags `--header` / `--no-header` (dual pair, so a config `header = false`
  can be re-enabled for one run), resolved via `resolve_bool`.
- Config key `header`.
- REPL session toggle `.header [on|off]` (mirrors `.timer`), plus `.set header`.
- Threads a `header: bool` into `render::render(...)`. When `false`, suppress the
  header row for **table, md, csv, tsv**:
  - comfy-table paths (table, md): skip `set_header` (rows only).
  - delimited paths (csv, tsv): skip the header `write_record`.
  - **JSON** unaffected (objects are keyed by header — there is no header row).
  - **`\G` vertical** unaffected (per-row `name: value` labels are not a header
    row).
- `render()`'s signature gains the flag; both callers
  (`session.render_statement_counted`, `main::run_statements` via the session)
  pass the resolved `header` setting. New `Session::header() -> bool`.

### W34 — `quiet` (default `false`)

- CLI flags `--quiet` / `-q` / `--no-quiet`; config key `quiet`.
- Suppresses **non-error** stderr chatter: skipped/unparsable-file warnings and
  refresh/scan summaries. Errors and the `--exit-code` mapping are never
  suppressed.
- Implementation: thread the resolved `quiet` flag to the emission points
  (discovery/store/cache-load warning sites and the refresh-summary print). The
  planner will enumerate the exact call sites; the flag is read from `Settings`.
- No dedicated REPL toggle; a persisted `quiet = true` also quiets `.reload` /
  `.refresh` scan chatter.

### Group A tests

- Config round-trip (`set` → `get` → file) for each of `timer`/`header`/`quiet`.
- `all_agrees_with_value_variants` continues to pass with 10 keys.
- Render: each of table/md/csv/tsv has a **with-header** and **no-header** test;
  JSON and vertical are asserted **unchanged** by the header flag.
- `quiet` suppresses a skipped-file warning but an explicit test asserts it does
  **not** suppress a query error.
- Precedence: `--header` overrides a configured `header = false` and
  `--no-header` overrides a configured `header = true`; likewise `--quiet` beats
  `quiet = false` and `--no-quiet` beats `quiet = true`. `timer` has no flag, so
  it is config-beats-default only, plus the `.timer` session override.

## 4. Group B — Output & render

### W37 — terminal-width-aware table (table only)

- In `render_table` (not the shared `new_table`, to keep Markdown reproducible):
  when `std::io::stdout().is_terminal()`, set
  `ContentArrangement::Dynamic` and the detected terminal width; otherwise leave
  today's arrangement so non-TTY output is byte-identical.
- Because insta snapshots and piped runs are non-TTY, existing snapshots are
  untouched. Add a test asserting the non-TTY render is unchanged, and extend the
  "interchange formats ignore terminal state" guard so **md** is asserted
  independent of width, not just style.

### W45 — `.output |cmd` pipe target (REPL only)

- `OutputSink` gains a `Command` variant holding the spawned child (with piped
  stdin; stdout/stderr inherited so pagers draw to the terminal).
- REPL `.output` parsing: an argument beginning with `|` (sqlite3 convention)
  selects the pipe. The command after `|` is run as `sh -c "<cmd>"`.
- `write_block` writes each rendered block (plus newline) to the child's stdin.
- Resetting the sink (`.output`, `.output stdout`, or a new `.output <target>`)
  closes the child's stdin and waits on the child before switching.
- Spawn failure → stderr message; the sink stays on stdout (unchanged).
- CLI `--output` stays file-only (unchanged); this is REPL-only, matching
  today's split.

### Group B tests

- `OutputSink::Command` round-trip: pipe blocks through a trivial filter (e.g.
  `cat`) to a temp file via `sh -c` and assert contents; assert reset closes/reaps.
- Non-TTY table render byte-identical to today (snapshot stays green).

## 5. Group C — REPL authoring

### W46 — `.query save <name> [sql]`

- Extend the REPL `.query` dispatch (today `run`/`list`) with `save`.
- SQL omitted → default to the **last successfully-run statement** this session.
  Add `last_sql: Option<String>` to the REPL loop, set after each successful
  `run_statement`. Error clearly if `.query save name` is used with no prior
  statement.
- Validate the SQL parses (reject up front, like the CLI `query save`), then
  persist via the same `queries::save` the CLI uses (same `queries.toml`).
- `DotCommand::Query` / `QueryCmd` gains `Save(String, Option<String>)`;
  `parse_dot` handles `save <name> [rest-as-sql]` (SQL taken verbatim after the
  name, like `.set`'s value). Name validation reuses the CLI path.
- Tab-completion staleness for the newly saved name is **out of scope** (that is
  W55, unchecked) — `.query run` reads `queries.toml` fresh, so running it works.

### Group C tests

- `.query save foo SELECT status` writes `foo` to `queries.toml`.
- `.query save foo` with no prior statement errors; after a run, it saves the
  last statement's SQL.
- Invalid SQL is rejected without writing.

## 6. Group D — Query engine

### W38 — CASE WHEN expression

- Add `Expr::Case { operand: Option<Box<Expr>>, whens: Vec<(Expr, Expr)>,
  else_expr: Option<Box<Expr>> }` to `query::ast`. Support both **searched**
  (`CASE WHEN cond THEN v ... END`) and **simple** (`CASE x WHEN v THEN ... END`)
  forms — sqlparser supplies both cheaply via `Expr::Case`'s optional operand.
- Lower from sqlparser's `Expr::Case` in the expression lowerer (alongside
  COALESCE/arithmetic).
- Evaluate in `eval_expr`:
  - searched: first `WHEN` whose condition is truthy → its `THEN`; else `ELSE`
    or `NULL`.
  - simple: first `WHEN` whose value equals `operand` (existing `compare_values`
    equality) → its `THEN`; else `ELSE` or `NULL`.
- Because `Case` lives on `Expr`, it is usable anywhere an `Expr` is (SELECT,
  WHERE, ORDER BY, HAVING) with no extra plumbing.

**Invariants this feature depends on (must update + test):**

- The **relative-date literal rewrite** must recurse into CASE operand/arms
  (there is already a test pinning recursion into COALESCE arguments — mirror it
  for CASE).
- **Unknown-column validation** must walk CASE operand/arms (else a bad column
  inside a CASE arm escapes the lenient/strict check).
- Any "which fields does this expr reference" walk must include CASE sub-exprs.

### W38 tests

- Searched CASE and simple CASE each produce the expected labels in a SELECT.
- CASE in WHERE and in ORDER BY.
- Relative-date rewrite recurses into a CASE arm (characterization test at the
  rewrite seam).
- Unknown column inside a CASE arm is caught in strict mode, NULL in lenient.

### W40 — single-pass multi-aggregate GROUP BY

- Refactor `project_group` so a group's aggregates accumulate in **one** pass
  over `group.rows`, instead of calling `compute_aggregate` (each re-scanning
  `group.rows`) once per aggregate SELECT item.
- Pure internal restructuring; no interface or result change.

### W40 tests

- Existing aggregate tests stay green; add a multi-aggregate-per-group query
  (e.g. `COUNT(*), SUM(x), AVG(x), MIN(x), MAX(x)` grouped) asserting identical
  results to the current path.

### W44 — bounded top-k for ORDER BY + LIMIT

- When `q.limit` is set, replace the full `rows.sort_by(order_cmp)` + skip/take
  with a bounded selection keeping only `offset + limit` rows (a max-heap of that
  size under `order_cmp`).
- **Tie-stability catch:** the current `sort_by` is stable (equal-key rows keep
  input order). A naive heap loses that. Carry each row's **original index** as a
  final ascending tiebreaker so the output is byte-identical to the full-sort
  path. Applies to both `execute_ungrouped` and `execute_grouped`.
- No `limit` → unchanged full sort.

### W44 tests

- `ORDER BY ... LIMIT n [OFFSET m]` returns the same rows in the same order as
  the pre-change full-sort path, including a **tie-stability** case where several
  rows share the sort key (pins input-order preservation).
- `OFFSET` + `LIMIT` window correctness.

## 7. Group E — CLI & diagnostics

### W33 — `cache status`

- New subcommand group `Command::Cache(CacheArgs)` with `CacheAction::Status`
  (room for future `cache clear`, etc.), optional `[DIR]` positional defaulting
  to cwd; resolve the vault via `cache::find_vault` (upward walk).
- Report (inspection output → **stdout**, like `config list`):
  - vault root (the resolved `.querymatter` parent);
  - cached directory count and cached-file count (from the loaded blobs);
  - on-disk cache size (sum of blob file sizes + `manifest.bin`);
  - TTL (`ManifestBody.ttl_secs`) and crate version (`ManifestBody.crate_version`);
  - per-directory last-scanned time (`ManifestEntry.scanned_at`).
- No vault found → a clear error naming `querymatter init`. Needs no config
  content (can run alongside the `config path`-style early handling if
  convenient; planner's call).

### W33 tests

- Against a temp vault built by `init`: status reports the right file/dir counts,
  a non-zero size, the TTL, and each directory's scanned time.
- No `.querymatter` present → error mentioning `init`.

### W36 — statement attribution in multi-statement failures

- In `run_statements`, materialize the statement list first (count `M`),
  enumerate, and on a statement's error wrap it with `statement N of M failed:
  …` — but only when `M > 1` (a lone statement needs no "1 of 1").
- Batch / `-e` / `query run` only; the REPL runs statements one at a time and is
  unaffected.

### W36 tests

- A 3-statement batch whose 2nd statement errors reports `statement 2 of 3`.
- A single-statement failure is **not** prefixed with "statement 1 of 1".

## 8. Item → primary files

| Item | Primary files |
|------|---------------|
| W32 header | config.rs, settings.rs, cli.rs, render.rs, session.rs, repl.rs, main.rs |
| W33 cache status | cache.rs, cli.rs, main.rs |
| W34 quiet | config.rs, settings.rs, cli.rs, discover.rs/store.rs/cache.rs (emit sites), main.rs |
| W35 timer | config.rs, settings.rs, session.rs, repl.rs |
| W36 attribution | main.rs |
| W37 table width | render.rs |
| W38 CASE | query/ast.rs, query/parse.rs, query/exec.rs |
| W40 aggregates | query/exec.rs |
| W44 top-k | query/exec.rs |
| W45 pipe output | output.rs, repl.rs |
| W46 query save | repl.rs, queries.rs |

## 9. Documentation updates (part of this branch)

- **README:** new flags (`--header`/`--no-header`, `--quiet`/`-q`/`--no-quiet`),
  new commands (`cache status`), new REPL commands (`.timer`, `.header`, `.query
  save`, `.output |cmd`), CASE WHEN in the DSL section, and the three new config
  keys in the config-keys table.
- **CLAUDE.md / project docs:** only if a convention changes (none expected;
  these follow existing patterns). Add nothing gratuitous.

## 10. Test strategy summary

- Unit tests co-located per module (config/settings/render/query as today).
- Integration tests in `tests/cli.rs` (assert_cmd + predicates + tempfile) for
  the CLI-visible items: `--no-header` output shape, `--quiet` stderr, `cache
  status`, statement attribution, and a CASE query end-to-end.
- insta snapshots extended only where a new *stable* (non-TTY) rendering is
  introduced; existing snapshots must stay green (proof W37 didn't disturb
  non-TTY output).
- Every declined-because-"an invariant makes it safe" test is instead written at
  the seam it crosses (per project spec-discipline): CASE-arm recursion in the
  date rewrite and in column validation; top-k tie-stability; quiet-does-not-eat-
  errors; header flag leaves JSON/vertical alone.
