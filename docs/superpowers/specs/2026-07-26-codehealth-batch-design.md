# code-health batch — execution spec (2026-07-26)

Fixes the 12 findings the user checked `[x] execute` in `bughunt.md` (triage of
`main` @ be3b4cf). One commit per finding, on branch `codehealth/2026-07-26`.
Ranking is `impact = severity × blast`; execution order below follows the
`bughunt.md` order (High → Medium → Low) so the highest-impact fixes land first.

**Not in this batch** (left in `bughunt.md`): B11, B12, B13 (unchecked). The
three `decision-needed` markers (`compare_values` total-order, CSV formula
injection, TSV escaping contract) are **out of scope** — they need a semantics
decision from the user before any fix.

## Toolchain (every task must stay green under all of these)
- build: `cargo build`
- test: `cargo test`
- lint: `cargo clippy --all-targets -- -D warnings`
- format: `cargo fmt` (no pre-commit hook — run it yourself before each commit)

## Per-task contract (enforced by subagent-driven-development)
1. Read the finding.
2. **If `risk: high`** — write a regression/characterization test that reproduces
   the bug, confirm it FAILS on unchanged code (RED), commit as
   `test: characterize <unit> before fix [B<n>]`.
3. Apply the fix; the regression test now passes (GREEN). Land a test with every
   fix regardless of risk.
4. Run build + lint + test; fix any new warnings the change introduced.
5. Commit `fix(<category>): <summary> [B<n>]` and **strip the finding from
   `bughunt.md` in the same commit**.

`risk: high` tasks (characterization test first): **B3, B4, B6, B7, B9.**

## Invariants this batch depends on (pin these with tests, per spec discipline)
- **INV-1 (interchange byte-identity):** CSV/JSON/TSV output is a stable
  interchange contract — recent work (W1/W2) pins empty-JSON and piped-command
  byte-identity. **B3's terminal sanitizer must NOT touch the CSV/JSON/TSV paths
  and must NOT alter piped (non-tty) output.** B3's test must pin that piping a
  value containing an ESC byte through an interchange format is byte-identical
  (unsanitized), so a future change to the sanitizer can't silently corrupt
  interchange output.
- **INV-2 (deterministic output ordering):** results are deterministically ordered
  (sorted/`IndexMap`, never `HashMap` iteration). B5's decorate-sort-undecorate and
  B10's body memoization must preserve the exact row order and values the current
  suite pins.
- **INV-3 (LIKE/REGEXP semantics):** the `like_and_in` tests pin LIKE/IN
  semantics. B1/B2 (hoist regex compilation) must be behavior-preserving against
  them — same matches, same case-sensitivity, same anchoring.
- **INV-4 (cache round-trip):** the cache is written and re-read byte-losslessly
  for legitimate vaults. B6's containment check and B14's orphan GC must not
  reject or delete any blob a legitimate vault produces (enumerate: nested dirs,
  renamed-then-rescanned dirs, the manifest itself).

---

## B1 — LIKE recompiles its regex once per row (caching, impact 16, effort M, risk low)
- **Symbol/site:** `like_matches` (src/query/exec.rs:1820), called from
  `eval_predicate` (exec.rs:1696) per record (`filter_records` exec.rs:417) and per
  row in projection/CASE (`Expr::Predicate` exec.rs:1480).
- **Fix:** compile the LIKE pattern → `Regex` exactly once instead of per call.
  Preferred: lower LIKE at parse time to a compiled matcher stored on the
  `Predicate` (so both the filter and projection passes reuse it); acceptable
  alternative: pre-walk `q.filter`/projection and build a `pattern → Regex` map
  `eval_predicate` borrows. Keep the escape + `%`→`.*` / `_`→`.` translation
  identical.
- **Test:** a query-level test over N>0 rows asserting identical results to today
  (`like_and_in` guards semantics — INV-3); optionally a micro-assertion that the
  translated regex is built once (e.g. via a counter in a unit test) if cheap.

## B2 — REGEXP recompiles its regex once per row (caching, impact 12, effort M, risk low)
- **Symbol/site:** `regexp_matches` (src/query/exec.rs:1834), called from
  `eval_predicate` (exec.rs:1704) per record and per row in projection.
- **Fix:** mirror B1 — compile the REGEXP pattern once at parse time (attach the
  compiled `Regex` to the `Predicate`; `parse::lower_regexp` already proves it
  compiles) or memoize before the filter loop. Behavior-preserving.
- **Test:** query-level equivalence test for `WHERE col REGEXP '…'` before/after.

## B3 — Terminal ANSI/control-escape injection from frontmatter (frontend, impact 12, effort M, risk high)
- **Symbol/site:** `new_table` add_row/set_header (src/render.rs:322), vertical
  `\G` format (render.rs:301); `Value::display` (model.rs:47) returns `Str`
  verbatim.
- **Fix:** add a pure, unit-testable sanitizer that replaces C0/C1 control bytes
  (ESC `0x1b`, `\r`, other control chars except `\t`) with a visible escape or
  U+FFFD, and apply it to each cell/header string **only in the human-readable
  table and vertical formats**. Do **not** touch CSV/JSON/TSV (INV-1). Match the
  existing `is_terminal` gating used by `want_dynamic_width` if the current design
  keeps piped table output byte-identical; if the table format is already
  sanitized unconditionally elsewhere, follow that. Update any table/vertical
  snapshots; do not change interchange snapshots.
- **Tests (RED first):** (1) unit test: sanitizer maps ESC/`\r`/newline/other
  control chars to the chosen safe form and leaves normal text + `\t` untouched;
  (2) integration test: table/vertical rendering of a value containing an ESC byte
  contains no raw `0x1b`; (3) INV-1 pin: the same value through `--format json`
  (and csv) is byte-identical to today (control char still ``-escaped by
  serde, not altered by our sanitizer).

## B4 — No broken-pipe handling (api-surface, impact 9, effort M, risk high)
- **Symbol/site:** streaming result path returns io::Error wrapped as fatal
  (session.rs:253 → main.rs:73); `println!` command sinks panic (exit 101) —
  `query list` (main.rs:681) and explain/config/cache-status writes.
- **Fix:** reset `SIGPIPE` to `SIG_DFL` at process start (single `unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) }`
  in `main`, adding the `libc` dep — cleanest, fixes every site incl. the panicking
  `println!` paths). This makes a closed reader terminate the process via SIGPIPE
  (exit 141) with no spurious stderr, the canonical Unix-filter behavior. (Confirm
  `libc` is acceptable per the crate-decisions menu; if not, fall back to the
  two-part approach: BrokenPipe → `ExitCode::SUCCESS` in main's Err arm + a
  stdout-writer helper for the `println!` sinks.)
- **Tests (RED first):** integration test piping a multi-row result to a reader
  that closes early (e.g. `head -n1`) asserting no `Broken pipe` on stderr and a
  non-101 exit; and the same for `query list`. (Use assert_cmd with a truncating
  reader / `.arg` pipeline harness; if the harness can't close the pipe mid-stream,
  characterize via a unit test of the exit-code mapping.)

## B5 — ORDER BY recomputes sort key per comparison (caching, impact 9, effort M, risk low)
- **Symbol/site:** sort comparator (exec.rs:448-458) calls `order_key_value`
  (exec.rs:1883) for both operands per comparison; grouped path recomputes `Agg`
  via `compute_aggregate` per comparison (exec.rs:1314).
- **Fix:** decorate-sort-undecorate — precompute each row's (and group's) sort
  key(s) once into a Vec, sort on the precomputed keys, drop them. Preserve exact
  ordering incl. NULL placement and tie-breaking (INV-2). NB: today's comparator
  routes through `compare_values`, which is non-total (B-Critical marker) — do
  **not** change comparison semantics here; only stop recomputing the key. Large
  intransitive inputs may still panic (that's the deferred marker, not this task).
- **Test:** ordering-equivalence test over a moderate row set incl. `ORDER BY
  count(*) DESC LIMIT k`; assert identical output to today.

## B6 — Poisoned cache path traversal via file.body (security, impact 8, effort M, risk high)
- **Symbol/site:** `records_from` (cache.rs:774) builds `dir.join(&file.rel_path)`
  from decoded cache data; `--fast` verbatim reuse (cache.rs:697); read at
  exec.rs:1407.
- **Fix:** in `records_from` (and the `refresh_fast` verbatim arm), reject/skip any
  `CachedFile` whose `rel_path` is absolute or contains a `..`/`.`/root component,
  and verify `dir.join(rel_path)` still `starts_with(dir)` after lexical
  normalization before constructing a Record. Surface skipped entries as a
  `LoadReport` warning. Must not reject legitimate nested rel_paths (INV-4).
- **Tests (RED first):** unit/integration test that a cache blob containing
  `rel_path = "../../../../etc/passwd"` yields no Record for it (and a warning),
  while legitimate nested rel_paths (`a/b/c.md`) still load.

## B7 — `init` reports only a skipped count, not which/why (observability, impact 6, effort S, risk high)
- **Symbol/site:** `run_init` (main.rs:378) discards `report.warnings`;
  `build_session` (main.rs:974-978) already `eprintln!`s them.
- **Fix:** in `run_init`, before the summary line, iterate `report.warnings` and
  `eprintln!("querymatter: {warning}")` for each (respect the resolved quiet
  setting, matching `build_session`).
- **Tests (RED first):** integration test: `querymatter init` over a dir with one
  invalid-frontmatter file emits the per-file warning to stderr (not just the
  count).

## B8 — Unbounded whole-file read → OOM (security, impact 6, effort M, risk low)
- **Symbol/site:** `scan_file` `fs::read_to_string` (cache.rs:440); `read_body`
  (exec.rs:1407).
- **Fix:** enforce a configurable max file size before reading (size is already
  available via `stat_file` in `scan_file`): skip oversized files with a
  `LoadReport` warning, or bound the read (`Read::take(limit)`). Pick a sane
  default (document it); make it a config/setting knob consistent with existing
  settings patterns. Apply the same cap to `read_body`.
- **Test:** a file exceeding the cap is skipped with a warning and does not appear
  in results; a file just under the cap loads normally.

## B9 — Unbounded frontmatter-depth recursion → stack overflow (correctness, impact 6, effort M, risk high)
- **Symbol/site:** `pod_to_value` (frontmatter.rs:83) and Value walkers
  (`Value::display`/`compact_value` model.rs:47,111; `render::to_json`
  render.rs:328; `hashable_cell_key` exec.rs:880).
- **Fix:** cap nesting depth in `pod_to_value` with a depth counter; beyond a sane
  bound (e.g. 64/128) reject the record as `Extract::Invalid` (skipped + warned)
  rather than crashing. Defensively bound the Value walkers too, or rely on the
  parse-time cap making deep Values unreachable — justify whichever with a test.
- **Tests (RED first):** a file whose frontmatter nests beyond the cap is skipped
  as invalid with a warning and does not crash the process; a file at a normal
  depth loads. (If gray_matter/yaml-rust2 overflows before `pod_to_value`, note it
  and cap at the earliest in-crate point that keeps the process alive.)

## B10 — file.body re-read + re-parsed per reference (caching, impact 6, effort M, risk low)
- **Symbol/site:** `read_body` (exec.rs:1403) `fs::read_to_string` + fresh
  `Matter::<YAML>` (frontmatter.rs:70) per call; `resolve_col` (exec.rs:1383) calls
  it per evaluation.
- **Fix:** memoize the body read for the lifetime of a single row's evaluation —
  read+parse once per record and share the Value across filter/projection/order.
  Compose with B5 so ORDER-BY-over-body reads once. Preserve values/order (INV-2).
- **Test:** a query referencing `file.body` in both WHERE and SELECT returns the
  same rows as today; (optionally) assert the file is read once per row via a
  counter in a unit-level harness if cheap.

## B14 — Orphaned cache blobs never deleted (caching, impact 4, effort M, risk medium)
- **Symbol/site:** `save_cache` (cache.rs:202) rewrites `manifest.bin` but never
  unlinks unreferenced blobs (cache.rs:164 blob naming; drop at cache.rs:906).
- **Fix:** after writing the new manifest, enumerate `.querymatter/*.bin` and
  unlink any blob not named by a current `ManifestEntry.blob` (keep `manifest.bin`).
  Do it **after** the manifest rename so a crash can only leave harmless orphans,
  never a manifest pointing at a deleted blob (INV-4).
- **Test:** after a dir is removed/renamed and the cache re-saved, the orphaned
  blob file is gone and the manifest + remaining blobs still load correctly.

## B15 — REPL banner on stdout (api-surface, impact 2, effort S, risk low)
- **Symbol/site:** `repl::run` banner `println!` (repl.rs:414); other REPL
  diagnostics already use stderr (run_statement 487-489).
- **Fix:** emit the banner with `eprintln!` so chrome stays on stderr, matching the
  crate's stdout=data / stderr=diagnostics rule.
- **Test:** REPL invocation with piped stdout puts the banner on stderr, and stdout
  contains only result data (no banner).

## Milestones
- Full `cargo test` at every 5th finding and at each bucket boundary. On red:
  bisect within the batch, revert the offender, surface the diagnosis.
- Final `cargo test` + `cargo clippy --all-targets -- -D warnings` green before
  reporting done.
