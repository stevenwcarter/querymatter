# typecheck execution batch: core type strengthening (T1–T9)

**Date:** 2026-07-29 · **Branch:** `typecheck/2026-07-29` (from `main` @ 92eeaa6)
**Source:** `TYPECHECK.md` triage 2026-07-29 — the 9 items checked `[x] execute`
(both Critical, top seven High). Items T10–T40 remain in `TYPECHECK.md` untouched.

## Goal

Nine compiler-driven type migrations that make currently-constructible bad states
fail to compile: three path-role newtypes, a parsed exclude-glob set, compiled
LIKE/REGEXP patterns in the AST, a unified `FileAttr` string mapping, a `Grouping`
sum type in the query AST, a traversal-checked `RelPath`, a `CacheMode` enum
replacing five CLI bools, one canonical `ValueKey` for hashing `Value`, and a
validated `QueryName`.

## Ground rules (from the typecheck skill — non-negotiable)

- **One commit per finding**, format `typecheck(<lens>): <summary> [T<n>]`.
- **Strip the finding's block from `TYPECHECK.md` at commit time.**
  `TYPECHECK.md` is *gitignored* in this repo, so the strip is an edit to the
  untracked working file done alongside each commit (it cannot be part of it).
- **Compiler-driven migration:** introduce the type, change it at the source,
  then fix every break `cargo check --all-targets` reports until green. Do not
  hand-grep for call sites the compiler will find.
- **No public-symbol renames.** Every recipe below is a type change with names
  kept. If a migration turns out to require a rename, STOP that task and convert
  the finding to a `decision-needed` marker in `TYPECHECK.md` instead.
- **Test one-way rule:** never refactor existing test *logic*. Mechanical
  compile fixes in tests forced by a type change are expected and fine
  (constructing the new type, matching new variants); do not restructure test
  intent. New characterization tests are allowed and sometimes required (below).
- **Toolchain gates per task:** `cargo check --all-targets` green →
  `cargo clippy --all-targets -- -D warnings` green → `cargo fmt --all` run
  (repo has NO pre-commit hook; fmt is manual) → targeted `cargo test <name>`
  for touched areas. Full `cargo test` at milestones only.
- Binary-only crate: **`cargo test --lib` does not work.** Use bare `cargo test`
  or `cargo test <filter>`.
- Harness LSP diagnostics are known-stale in this repo; the compiler is
  authoritative.

## Milestones (full `cargo test` + snapshot review)

- **M1** after T2 (end of Critical bucket)
- **M2** after T7
- **M3** after T9 (end of batch)

On red at a milestone: bisect within the batch, revert the offender, surface the
diagnosis before continuing.

## Invariants this batch depends on (each gets a pin, not prose)

Per the spec-discipline rule: any "byte-identical / already covered / no test
needed" claim below must be backed by a test that exists or is added in the same
task. Concretely:

1. **bincode cache-blob layout is unchanged by `#[serde(transparent)]`**
   (T1 `CachedDir::dir` → `DirPath`, T6 `rel_path` → `RelPath`). Pin: a
   round-trip test in `cache.rs` tests encoding the *old shape* (mirror struct
   with plain `PathBuf`/`String` fields) and decoding as the new — byte-for-byte
   compatible in both directions. `SCHEMA_VERSION` stays 3. If the pin cannot be
   made to pass, that's a design failure: stop, don't bump the version silently.
2. **`queries.toml` wire shape (`name = "sql"` top-level)** (T9). Pin: extend the
   existing queries round-trip test to serialize a `Queries` map with a
   `QueryName` key and assert the TOML text shape.
3. **User-facing SQL column spellings `file.*`** (T4). Pinned already by
   `tests/snapshots/sample_queries__sample_queries_output.snap` and the repl pin
   test (retargeted to `FileAttr::ALL.map(label)` — same eight literals).
4. **CLI flag surface & error text for cache-mode conflicts** (T7). Pinned
   already by `tests/cli.rs` integration tests (e.g. lines ~826, 1121, 1215,
   1325, 1377); they must pass unmodified.
5. **Parser error text for DISTINCT+GROUP BY / HAVING-without-GROUP BY** (T5).
   Pinned by existing parse tests; they must pass with assertions unchanged
   (construction-site compile fixes aside).
6. **Invalid exclude-glob error reports the offending pattern** (T2). Pinned by
   existing `tests/cli.rs` (~438, 472, 2243); keep messages byte-identical.
7. **DISTINCT / count(distinct) current behavior on mixed-type columns** (T8).
   NOT currently pinned — T8 *deliberately changes it*. Requires a
   characterization commit BEFORE the migration (see T8).

## Agent dispatch

Per project memory (Steve, 2026-07-22): implementation subagents are
`rust-developer`. Model: **sonnet** by default; **opus** for T3, T5, T8
(non-trivial parser/AST/executor work). Implementers run `cargo fmt --all`
before committing (no pre-commit hook exists).

---

## T1. Path-role newtypes: `VaultRoot` / `DirPath` / `FilePath` (Critical, impact 20, L, risk low)

`pub struct VaultRoot(PathBuf);`, `pub struct DirPath(PathBuf);`,
`pub struct FilePath(PathBuf);` in `model` (or a new `paths` module), each with
`AsRef<Path>`/`Deref<Target = Path>`/`as_path()`. Change leaf signatures first —
`cache::scan_file`, `cache::refresh_one_file`, `cache::file_dir`,
`cache::contained_path`, `model::Record::new` — and let the compiler enumerate
the ~49 call sites across cache/discover/store/model/session/main/cli.

Known ambiguities this surfaces (handle explicitly, do not paper over):
- `store.rs:450` passes the SCAN ROOT where `cache.rs:627/790/1038` pass the
  file's immediate parent into the same `dir` parameter → make it an explicit
  `DirPath::from_root(root)` conversion at that call site.
- `discover.rs` mixes `(subject, root)` and `(root, subject)` argument orders
  across `is_excluded`/`exclude_reason` vs `explain`/`hidden_component`/
  `hidden_reason` → after newtyping, order is compiler-checked; keep names.

`CachedDir::dir` becomes `DirPath` with `#[serde(transparent)]` — bincode layout
pin per invariant 1. `discover::discover`'s public shape stays `&Path`
(constructs newtypes internally). No renames.

## T2. `ExcludeGlob` / `ExcludeSet`: parse globs once (Critical, impact 20, L, risk low)

`pub struct ExcludeGlob { source: String, glob: globset::Glob }` with `FromStr`
(Err = `globset::Error`); `pub struct ExcludeSet { globs: Vec<ExcludeGlob>, set: GlobSet }`
with `TryFrom<&[String]>`, `is_empty()`, `is_match(&Path)`,
`first_match(&self, &Path, Option<&Path>) -> Option<&str>`, `sources()`.

Parse once where the resolved list is produced: `Settings.exclude:
Resolved<ExcludeSet>` (settings.rs:202) and `WalkOpts.excludes: ExcludeSet`.
Deletes `discover::validate_excludes` + its four compile-and-discard call sites
(config.rs:243, main.rs:380/493/927) and `discover::build_exclude_set`
(discover.rs:160), which today *silently drops* non-compiling patterns.
`exclude_reason` (discover.rs:312) — which re-compiles per pattern and can
disagree with the set actually used — becomes `ExcludeSet::first_match`.

Wire form stays `Vec<String>` on `Config.exclude` (TOML) and the CLI arg;
convert at load. `config::set` reports the offending pattern from the same
`globset::Error` (invariant 6: error text byte-identical).

## T3. Compiled LIKE/REGEXP patterns in the AST (High, impact 16, M, risk low) — **opus**

`pub struct LikePattern { source: String, regex: Regex }` /
`pub struct RegexPattern { source: String, regex: Regex }` — manual `PartialEq`
comparing `source` only (`Predicate` derives `PartialEq`; ~15 parse tests compare
nodes), fallible `RegexPattern::new` (maps `regex::Error` into the existing
parse-error channel at `lower_regexp`, parse.rs:688 — which today compiles the
regex to validate it and THROWS IT AWAY), infallible `LikePattern::new` (runs
`compile_like_pattern`'s translation at `lower_predicate`, parse.rs:651).

AST becomes `Predicate::Like(Expr, LikePattern, bool)` / `Regexp(Expr,
RegexPattern, bool)`. Then delete the exec side-table wholesale:
`compile_pattern_regexes` (exec.rs:280), both collector walks (exec.rs:317/357),
`EvalCtx.like_regexes`/`regexp_regexes` (exec.rs:241/244), the transposable
`(HashMap, HashMap)` destructure (exec.rs:202), and both
`.expect("...pre-compiles every...")` lookups (exec.rs:2190/2214).
`eval_predicate` calls `pattern.is_match(...)` directly. Parse-error text for
invalid REGEXP must stay identical (existing parse tests pin it). No
serialization — patterns only ever come from SQL text.

## T4. `FileAttr` owns its string mapping (High, impact 16, M, risk low)

Extend the EXISTING `model::FileAttr`: `pub const ALL: [FileAttr; 8]`,
`pub fn label(self) -> &'static str` (dotted `file.name` form, absorbing
`ast::file_attr_label`), `pub fn from_attr_name(&str) -> Option<FileAttr>` (the
bare lowercased half `parse::lower_compound` hands in, absorbing
`parse::file_attr_from_str`), `pub fn value_kind(self)` for the Int/Str split.
Two spellings stay two explicit fns — NOT one `FromStr`.

Delete `repl::FILE_COLUMNS`; `.schema`/`.describe`/width/table/completion
iterate `FileAttr::ALL`; `describe_file_column_line`'s
`matches!(name, "file.size" | "file.word_count")` (repl.rs:1095) becomes an
exhaustive `match attr` via `value_kind()`. Retarget the repl pin test
(~repl.rs:2288) to `FileAttr::ALL.map(label)` — same eight literals (allowed:
that is a mechanical retarget preserving the assertion). Invariant 3 pins the
spellings.

## T5. `Grouping` sum type in the query AST (High, impact 16, L, risk low) — **opus**

`pub enum Grouping { Ungrouped { distinct: bool }, Grouped { keys: Vec<ColRef>,
having: Option<Having> } }` replacing `Query.distinct`/`group_by`/`having`
(implicit single-group aggregate = `Grouped { keys: vec![], having: None }` —
exactly what `is_grouped_or_aggregate` computes today). Split `OrderKey` so
`OrderTarget::Agg` exists only on the grouped side.

Today only `lower_query` enforces the combinations (parse.rs:95-99, 929-931);
the executor reads `q.distinct` ONLY ungrouped (exec.rs:688) and `q.having` ONLY
grouped (exec.rs:849) — a Query that slips past drops the clause silently, and
`execute_with_schema_at` clones and MUTATES the AST via `rewrite_relative_dates`
(exec.rs:172/412) after the parser's checks ran. `lower_query` constructs the
variant directly (the two `unsupported` checks become unconstructible arms);
`is_grouped_or_aggregate` becomes a `match`. Parser error text unchanged
(invariant 5). AST is never serialized.

## T6. `RelPath`: traversal check as the constructor (High, impact 12, M, risk low)

`#[serde(transparent)] pub struct RelPath(String);` whose fallible `parse` IS
today's `contained_path` component scan (reject `ParentDir`/`RootDir`/`Prefix`,
strip `CurDir`), with `deserialize_with` calling `parse` so a poisoned blob
fails to decode, and `fn resolve(&self, dir: &Path) -> PathBuf` as the ONLY join
path. Fixes the asymmetry: `records_from` (cache.rs:909) checks and warns, but
`refresh_per_file` (cache.rs:616) and `refresh_fast` (cache.rs:756) join raw.
Keep the rejection warning as a `LoadReport` entry on parse/decode failure.
bincode layout pin per invariant 1; `SCHEMA_VERSION` stays 3.

## T7. `CacheMode` replaces five CLI bools (High, impact 12, M, risk low)

`pub enum CacheMode { Live, Cached { freshness: Freshness, refresh: RefreshScope },
TrustCache }` + `pub enum RefreshScope { None, All, Subtrees(Vec<PathBuf>) }`.
Raw clap fields go private; `Cli::cache_mode() -> anyhow::Result<CacheMode>` is
the single fallible translation absorbing the four `ensure!` rejections in
`Cli::validate` (cli.rs:392-413). `Freshness` shrinks to `{ PerFile, Fast }`
(`ForceCache` becomes `TrustCache`, which structurally carries no refresh scope
and no fast). `build_session` matches once instead of re-deriving the mode at
main.rs:913/931/972/995/1019 — the no-vault + TrustCache error and the
disk-reads decision each live in exactly one arm; `--force-cache --refresh` is
non-representable. Clap surface and conflict error text byte-identical
(invariant 4).

## T8. `ValueKey`: one hash key for `Value` (High, impact 12, M, **risk medium**) — **opus**

**Step A (separate commit, first):** characterization test pinning CURRENT
DISTINCT / count(distinct) / GROUP BY behavior on mixed-type columns
(`Int(1)` vs `Str("1")`, `Null` vs `Str("")`, `Float(-0.0)` vs `Float(0.0)`,
`List` vs joined `Str`). Commit: `test: characterize mixed-type DISTINCT/GROUP
BY keys before typecheck [T8]`.

**Step B:** `pub struct ValueKey` in src/model.rs — canonical variant-tagged
recursive encoding of `&Value` deriving `Eq + Hash` (owned tree: `Null / Bool /
Int / Float(u64 /* to_bits after -0.0→0.0 */) / Str / List / Map / Date /
DateTime`), built by one `From<&Value>`. Replace all three ad-hoc keys: GROUP
BY's `hashable_cell_key` `Vec<String>` (exec.rs:1114), SELECT DISTINCT's
`to_cmp_string` `HashSet<Vec<String>>` (exec.rs:794), count(distinct)'s
`to_cmp_string` `BTreeSet<String>` (exec.rs:1388). This DELIBERATELY changes
DISTINCT/count(distinct) on mixed-type columns to agree with GROUP BY —
update the Step-A characterization test to the new semantics in the same commit,
with the change called out in the commit message. `to_cmp_string` returns to its
real job (ordering). Keys are process-local, never persisted. Check whether
`README`/docs describe DISTINCT semantics; update if so.

## T9. `QueryName`: validated saved-query names (High, impact 12, M, risk low)

`#[serde(transparent)] pub struct QueryName(String);` whose `FromStr`/`parse`
carries today's `is_valid_name` rule (queries.rs:129) as its ONLY constructor,
plus `Display`, `AsRef<str>`, `Borrow<str>` so `BTreeMap<QueryName, String>`
keeps `get(&str)`. Validation moves to the boundaries (`main::save_named_query`,
the clap `QueryAction` arms, `repl::parse_dot`'s `.query run|save` lowering);
`queries::set` drops its `ensure!` — today `set` validates while `remove`/`get`
accept anything. `QueryCmd::Run(QueryName)` / `Save(QueryName, Option<String>)`
makes the positional `(name, sql)` pair type-distinct. queries.toml wire-shape
pin per invariant 2. Optional `QueryText` for the SQL side: SKIP it — keep this
task minimal; the name newtype alone kills the transposition (a bare `String`
can no longer sit in the name slot).
