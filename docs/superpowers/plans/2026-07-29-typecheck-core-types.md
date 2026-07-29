# typecheck Core Types (T1–T9) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Execute the nine selected typecheck findings (T1–T9) as compiler-driven type migrations, one commit per finding, so the bad states they describe fail to compile.

**Architecture:** Each task introduces a stronger type at its defining module, changes the source signature/field, then fixes every break `cargo check --all-targets` reports until green. No behavior changes except T8's deliberate DISTINCT/count(distinct) unification (characterized first) and T5's explicit rejection of `SELECT DISTINCT <aggregate>` (silently ignored today).

**Tech Stack:** Rust edition 2024, binary-only crate `querymatter`. No new dependencies — `regex`, `globset`, `bincode` (serde bridge), `thiserror` already in Cargo.toml.

**Spec:** `docs/superpowers/specs/2026-07-29-typecheck-core-types-design.md`

## Global Constraints

- Branch: `typecheck/2026-07-29`. One commit per finding: `typecheck(<lens>): <summary> [T<n>]`.
- After each task's commit, **delete that finding's `### T<n>. …` block (heading through its `- [ ] execute` line) from `TYPECHECK.md`**. The file is gitignored — edit it in the working tree at commit time; it cannot be part of the commit.
- **Compiler-driven migration:** after changing the source type, run `cargo check --all-targets 2>&1 | head -50` repeatedly and fix what it reports. Do NOT hand-grep for call sites the compiler will find. (Blast-radius lists below are context, not to-do lists.)
- **No public-symbol renames.** If a migration turns out to require one, STOP the task, restore the finding in `TYPECHECK.md` as a `decision-needed` marker, and report back.
- **Test one-way rule:** mechanical compile fixes in existing tests are fine (constructing new types, matching new variants, retargeting a pin test to an equivalent assertion); never restructure test intent. New tests are required where a task says so.
- Gates per task, in order: `cargo check --all-targets` → `cargo clippy --all-targets -- -D warnings` → `cargo fmt --all` (NO pre-commit hook exists; fmt is manual) → the task's named tests via `cargo test <filter>`. Full `cargo test` only at milestone tasks.
- Binary-only crate: `cargo test --lib` does NOT work. Use bare `cargo test` or `cargo test <filter>`.
- Harness LSP diagnostics are known-stale in this repo — trust only `cargo check`/`clippy`/`test` output.
- Error messages, CLI surface, TOML wire shapes, and the `sample_queries` snapshot are behavioral contracts. Tasks call out each one they touch; keep them byte-identical unless the task explicitly says the behavior changes.

---

### Task 1: Path-role newtypes `VaultRoot` / `DirPath` / `FilePath` [T1]

**Files:**
- Create: `src/paths.rs` (new module; registered in `src/main.rs`'s module list)
- Modify: `src/cache.rs` (scan_file:506, file_dir:591, refresh_one_file:682, contained_path:845, `CachedDir.dir`:66), `src/model.rs` (Record::new:257, abs_path), `src/discover.rs`, `src/store.rs`, `src/session.rs`, `src/main.rs`, `src/cli.rs` — as the compiler directs
- Test: new `#[cfg(test)]` tests in `src/cache.rs` (bincode layout pin)

**Interfaces (Produces — later tasks use these exact types):**
- `paths::VaultRoot(PathBuf)`, `paths::DirPath(PathBuf)`, `paths::FilePath(PathBuf)` — all `pub`, each with `pub fn new(PathBuf) -> Self`, `pub fn as_path(&self) -> &Path`, `impl AsRef<Path>`, `impl Deref<Target = Path>`, `Debug + Clone + PartialEq + Eq`
- `DirPath` additionally: `Serialize + Deserialize` with `#[serde(transparent)]`, and `pub fn from_root(root: &VaultRoot) -> DirPath`
- `cache::file_dir(vault: &VaultRoot, path: &FilePath) -> DirPath` (stays private-in-module if it is today)
- `model::Record::new(root: &VaultRoot, path: &FilePath, …)` (rest of the signature unchanged)

- [ ] **Step 1: Write the bincode layout pin test first** (in `src/cache.rs` tests; it must FAIL to compile until `DirPath` exists — that is its red state):

```rust
#[test]
fn cached_dir_bincode_layout_unchanged_by_dirpath_newtype() {
    // Pin: `CachedDir { dir: DirPath, .. }` must encode byte-identically to
    // the pre-T1 shape with a plain `PathBuf`, so existing caches decode
    // and SCHEMA_VERSION stays 3.
    #[derive(serde::Serialize)]
    struct OldCachedDir<'a> {
        dir: &'a Path,
        scanned_at: SystemTime,
        dir_mtime: SystemTime,
        files: &'a [CachedFile],
    }
    let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    let new = CachedDir {
        dir: DirPath::new(PathBuf::from("notes/projects")),
        scanned_at: t,
        dir_mtime: t,
        files: vec![],
    };
    let old = OldCachedDir { dir: Path::new("notes/projects"), scanned_at: t, dir_mtime: t, files: &[] };
    assert_eq!(encode(&new), encode(&old));
}
```

- [ ] **Step 2: Create `src/paths.rs`** with the three newtypes (shape below; repeat for all three, `serde` derives on `DirPath` only):

```rust
//! Role-distinct wrappers for the three path meanings the cache/discover
//! pipeline threads around, so a vault root, a containing directory, and a
//! concrete file can no longer be swapped at a call site.

use std::ops::Deref;
use std::path::{Path, PathBuf};

/// The vault / scan root every `Record`'s `file.*` attrs are resolved against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaultRoot(PathBuf);

/// The immediate containing directory a `CachedFile::rel_path` is relative to.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct DirPath(PathBuf);

/// One concrete markdown file on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePath(PathBuf);

impl VaultRoot {
    pub fn new(path: PathBuf) -> Self {
        VaultRoot(path)
    }
    pub fn as_path(&self) -> &Path {
        &self.0
    }
}
impl AsRef<Path> for VaultRoot {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}
impl Deref for VaultRoot {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}
// …identical impl blocks for DirPath and FilePath, plus:
impl DirPath {
    /// The explicit conversion for the one call site (store.rs:450) that
    /// legitimately scans files directly under the vault root.
    pub fn from_root(root: &VaultRoot) -> DirPath {
        DirPath(root.as_path().to_path_buf())
    }
}
```

Register `mod paths;` in `src/main.rs` next to the existing module declarations, `pub use` nothing extra (call sites use `crate::paths::…`).

- [ ] **Step 3: Change the leaf signatures at the source.** In `src/cache.rs`: `scan_file(dir: &DirPath, path: &FilePath, max_file_bytes: u64)`, `refresh_one_file(dir: &DirPath, path: &FilePath, …)`, `file_dir(vault: &VaultRoot, path: &FilePath) -> DirPath`, `contained_path(dir: &DirPath, rel_path: &str)`, and `CachedDir.dir: DirPath`. In `src/model.rs`: `Record::new(root: &VaultRoot, path: &FilePath, …)`.

- [ ] **Step 4: Compiler-driven fixout.** Loop `cargo check --all-targets` until green, fixing each reported site. Decisions made in advance:
  - `store.rs:450` (scan root passed as `dir`): write `&DirPath::from_root(&root)` — this is intentional, not a bug; the conversion documents it.
  - `discover.rs`'s mixed `(subject, root)` / `(root, subject)` orders: parameters take `&FilePath`/`&VaultRoot` (or `&Path` where the function is genuinely role-agnostic, e.g. walking arbitrary entries); `discover::discover`'s public signature stays `&Path` and constructs newtypes internally where it calls role-typed helpers.
  - `main.rs`/`cli.rs` boundary: construct `VaultRoot::new(...)` where the vault/scan root is first resolved (the canonicalized root in `build_session` and the `init`/`explain` paths), not deeper in.
  - Tests that construct `Record`/`CachedDir` etc. get mechanical wraps (`VaultRoot::new`, `DirPath::new`, `FilePath::new`).

- [ ] **Step 5: Run the pin test and neighbors:** `cargo test cached_dir_bincode` then `cargo test cache` — all pass. The layout pin (Step 1) must pass WITHOUT touching `SCHEMA_VERSION` (stays 3); if it fails, the serde shape drifted — fix the type, never the version.

- [ ] **Step 6: Gates:** `cargo clippy --all-targets -- -D warnings` → `cargo fmt --all`.

- [ ] **Step 7: Commit + strip:**

```bash
git add -A
git commit -m "typecheck(newtype): role-typed VaultRoot/DirPath/FilePath for the cache pipeline [T1]"
```

Then delete the `### T1. …` block from `TYPECHECK.md`.

---

### Task 2: `ExcludeGlob` / `ExcludeSet` — parse exclude globs once [T2]

**Files:**
- Modify: `src/discover.rs` (WalkOpts.excludes:37, is_excluded:129, validate_excludes:149 — DELETE, build_exclude_set:160 — DELETE, exclude_reason:312), `src/settings.rs` (Settings.exclude:77, Default:97, resolve_walk:~202, walk_opts:~231, cells/rows rendering), `src/config.rs` (parse_exclude_list:~243, set), `src/main.rs` (380, 493, 927), `src/cli.rs` as compiler directs
- Test: existing `tests/cli.rs` glob-error tests (~438, 472, 2243) must pass unmodified; existing `validate_excludes_*` unit tests retarget mechanically to `ExcludeSet::try_from`

**Interfaces (Produces):**
- `discover::ExcludeGlob { source: String, glob: globset::Glob }` — `FromStr<Err = anyhow::Error>`, `pub fn source(&self) -> &str`
- `discover::ExcludeSet` — `pub fn empty() -> Self`, `impl TryFrom<&[String]> for ExcludeSet { type Error = anyhow::Error }`, `pub fn is_empty(&self) -> bool`, `pub fn is_match(&self, path: &Path) -> bool`, `pub fn first_match(&self, target: &Path, relative: Option<&Path>) -> Option<&str>`, `pub fn sources(&self) -> impl Iterator<Item = &str>`, manual `PartialEq`/`Eq` comparing sources, `Debug + Clone + Default`
- `Settings.exclude: Resolved<ExcludeSet>`; `WalkOpts.excludes: ExcludeSet`

- [ ] **Step 1: Add the types to `src/discover.rs`:**

```rust
/// One exclude pattern together with its compiled glob — constructing it IS
/// the validation, so an invalid pattern cannot reach the walk.
#[derive(Debug, Clone)]
pub struct ExcludeGlob {
    source: String,
    glob: Glob,
}

impl std::str::FromStr for ExcludeGlob {
    type Err = anyhow::Error;
    fn from_str(pattern: &str) -> anyhow::Result<Self> {
        // Byte-identical error contract with the deleted `validate_excludes`
        // (pinned by tests/cli.rs).
        let glob = Glob::new(pattern).with_context(|| format!("invalid exclude glob {pattern:?}"))?;
        Ok(ExcludeGlob { source: pattern.to_string(), glob })
    }
}

/// The full exclude list, parsed once: individual globs (for attribution in
/// `explain`) plus the combined `GlobSet` the walk matches against.
#[derive(Debug, Clone, Default)]
pub struct ExcludeSet {
    globs: Vec<ExcludeGlob>,
    set: GlobSet,
}

impl ExcludeSet {
    pub fn empty() -> Self {
        Self::default()
    }
    pub fn is_empty(&self) -> bool {
        self.globs.is_empty()
    }
    pub fn is_match(&self, path: &Path) -> bool {
        self.set.is_match(path)
    }
    /// The first pattern matching `target` (or its root-relative form) —
    /// `exclude_reason`'s attribution, now guaranteed to agree with the set
    /// the walk actually used.
    pub fn first_match(&self, target: &Path, relative: Option<&Path>) -> Option<&str> {
        self.globs
            .iter()
            .find(|g| {
                let m = g.glob.compile_matcher();
                m.is_match(target) || relative.is_some_and(|rel| m.is_match(rel))
            })
            .map(|g| g.source.as_str())
    }
    pub fn sources(&self) -> impl Iterator<Item = &str> {
        self.globs.iter().map(|g| g.source.as_str())
    }
}

impl TryFrom<&[String]> for ExcludeSet {
    type Error = anyhow::Error;
    fn try_from(patterns: &[String]) -> anyhow::Result<Self> {
        let globs: Vec<ExcludeGlob> = patterns.iter().map(|p| p.parse()).collect::<anyhow::Result<_>>()?;
        let mut builder = GlobSetBuilder::new();
        for g in &globs {
            builder.add(g.glob.clone());
        }
        let set = builder.build()?;
        Ok(ExcludeSet { globs, set })
    }
}

impl PartialEq for ExcludeSet {
    fn eq(&self, other: &Self) -> bool {
        // Settings derives PartialEq/Eq; GlobSet has neither, and equal
        // sources imply an equal compiled set.
        self.sources().eq(other.sources())
    }
}
impl Eq for ExcludeSet {}
```

(`Default` for `GlobSet` is `GlobSet::empty()` — verify; if not, hand-write `Default`.)

- [ ] **Step 2: Change the sources:** `WalkOpts.excludes: ExcludeSet`; `Settings.exclude: Resolved<ExcludeSet>` (Default: `Resolved::new(ExcludeSet::empty(), Source::Default)`). Precedence in `resolve_walk` still resolves over the `Vec<String>` layers (CLI/vault/config wire form is unchanged), then parses the winning list ONCE: `ExcludeSet::try_from(winning.as_slice())?` — `resolve_walk` (and `Settings::resolve` if it calls it) becomes `-> anyhow::Result<…>`.

- [ ] **Step 3: Compiler-driven fixout.** Loop `cargo check --all-targets`. Decisions:
  - DELETE `validate_excludes` + its call sites (config.rs:243, main.rs:380/493/927) — the `?` on `try_from`/`resolve_walk` replaces them. DELETE `build_exclude_set`.
  - `is_excluded(path, root, excludes: &ExcludeSet)`; `exclude_reason` takes `&ExcludeSet` and uses `first_match(target, relative)` — keep both output strings byte-identical (`matches --exclude glob '{pattern}'` / `matches an --exclude glob`).
  - `config::set`'s exclude arm parses via `ExcludeSet::try_from` for validation but stores the raw `Vec<String>` (wire form unchanged).
  - `Settings::cells`/`rows` render the exclude list from `sources()` joined exactly as today.
  - Existing unit tests `validate_excludes_accepts_good_globs` / `_rejects_a_bad_glob_naming_it` retarget mechanically to `ExcludeSet::try_from(&[…])` with identical assertions.

- [ ] **Step 4: Tests:** `cargo test exclude` and `cargo test --test cli exclude` — all pass, zero assertion edits in `tests/cli.rs`.

- [ ] **Step 5: Gates:** clippy `-D warnings` → `cargo fmt --all`.

- [ ] **Step 6: Commit + strip:**

```bash
git add -A
git commit -m "typecheck(parse-dont-validate): ExcludeSet parses exclude globs once at resolve time [T2]"
```

Then delete the `### T2. …` block from `TYPECHECK.md`.

---

### Task 3: MILESTONE M1 — full suite after the Critical bucket

- [ ] **Step 1:** `cargo test` (full). Expected: all green, including `sample_queries` snapshots and `tests/cli.rs`.
- [ ] **Step 2:** `git status --porcelain` — expect only `TYPECHECK.md` (ignored) modifications; tree otherwise clean.
- [ ] **Step 3:** On red: bisect between T1 and T2 (`git stash`/`git revert` the newer), identify the offender, revert it, surface the diagnosis in the task report. Do NOT proceed to Task 4 on red.

---

### Task 4: Compiled LIKE/REGEXP patterns in the AST [T3] — **model: opus**

**Files:**
- Modify: `src/query/ast.rs` (Predicate::Like:198 / Regexp:207, predicate_label ~698/703, + new pattern types), `src/query/parse.rs` (lower_predicate:~651, lower_regexp:688), `src/query/exec.rs` (DELETE compile_pattern_regexes:280 + collector walks:317/357 + EvalCtx.like_regexes/regexp_regexes:241/244 + destructure:202 + like_matches/regexp_matches lookup bodies:2190/2214; MOVE compile_like_pattern:2201 translation into `LikePattern::new`)
- Test: existing parse tests (~15 comparing `Predicate` nodes) get mechanical construction fixes; existing exec LIKE/REGEXP behavior tests must pass unchanged

**Interfaces (Produces):**
- `ast::LikePattern` — `pub fn new(source: &str) -> Self` (infallible), `pub fn source(&self) -> &str`, `pub fn is_match(&self, value: &str) -> bool`, manual `PartialEq` on `source`, `Debug + Clone`
- `ast::RegexPattern` — `pub fn new(source: &str) -> Result<Self, regex::Error>`, same accessors/impls
- `Predicate::Like(Expr, LikePattern, bool)` / `Predicate::Regexp(Expr, RegexPattern, bool)`

- [ ] **Step 1: Add the pattern types to `src/query/ast.rs`:**

```rust
/// A `LIKE` pattern together with its compiled anchored regex. Compiled once
/// at parse time; evaluation can no longer see an uncompiled pattern.
#[derive(Debug, Clone)]
pub struct LikePattern {
    source: String,
    regex: Regex,
}

impl LikePattern {
    /// `%` → `.*`, `_` → `.`, everything else literal, anchored `^…$` —
    /// verbatim the translation exec::compile_like_pattern performed.
    pub fn new(source: &str) -> Self {
        let escaped = regex::escape(source);
        let translated = escaped.replace('%', ".*").replace('_', ".");
        // `translated` is `regex::escape` output with only `.*`/`.`
        // substituted in, so wrapping it in `^…$` is always a valid regex.
        let regex = Regex::new(&format!("^{translated}$"))
            .expect("LIKE pattern translates to a well-formed regex");
        LikePattern { source: source.to_string(), regex }
    }
    pub fn source(&self) -> &str {
        &self.source
    }
    pub fn is_match(&self, value: &str) -> bool {
        self.regex.is_match(value)
    }
}

impl PartialEq for LikePattern {
    fn eq(&self, other: &Self) -> bool {
        // Predicate derives PartialEq and parse tests compare nodes; the
        // compiled regex is a pure function of `source`.
        self.source == other.source
    }
}
```

`RegexPattern` is identical except `pub fn new(source: &str) -> Result<Self, regex::Error>` wrapping `Regex::new(source)?` with no translation.

- [ ] **Step 2: Change the AST variants** to `Like(Expr, LikePattern, bool)` / `Regexp(Expr, RegexPattern, bool)`. In `lower_regexp` (parse.rs:688) replace validate-and-discard with `RegexPattern::new(&pat).map_err(|e| unsupported(format!("invalid regex `{pat}`: {e}")))?` — error text byte-identical. In the LIKE lowering (parse.rs:~651) construct `LikePattern::new(&pat)`. Update `lower_regexp`'s stale doc comment (it claims exec recompiles per record).

- [ ] **Step 3: Compiler-driven fixout, deleting the side-table.** Loop `cargo check --all-targets`. The compiler will point at: `compile_pattern_regexes`, both collector walks, the `EvalCtx` fields + the exec.rs:202 destructure + 206–207 wiring, `like_matches`/`regexp_matches` (their map+expect bodies become direct `pattern.is_match(value)` — inline them into `eval_predicate` if that leaves them trivial), `predicate_label`'s pattern rendering (uses `.source()`), `collect_expr_fields`' Like/Regexp arms (bind the pattern with `_`), and ~15 parse tests constructing `Predicate` nodes (wrap literals in `LikePattern::new(…)` / `RegexPattern::new(…).unwrap()`). Delete `compile_like_pattern` from exec.rs.

- [ ] **Step 4: Tests:** `cargo test like`, `cargo test regexp`, `cargo test parse` — behavior assertions unchanged.

- [ ] **Step 5: Gates:** clippy `-D warnings` → `cargo fmt --all`.

- [ ] **Step 6: Commit + strip:**

```bash
git add -A
git commit -m "typecheck(parse-dont-validate): compile LIKE/REGEXP patterns once at parse time, drop the exec side-table [T3]"
```

Then delete the `### T3. …` block from `TYPECHECK.md`.

---

### Task 5: `FileAttr` owns its string mapping [T4]

**Files:**
- Modify: `src/model.rs` (FileAttr:203 — add impl), `src/query/ast.rs` (file_attr_label:811 — DELETE, callers → `attr.label()`), `src/query/parse.rs` (file_attr_from_str:626 — DELETE, caller ~618/620 → `FileAttr::from_attr_name`), `src/repl.rs` (FILE_COLUMNS:38 — DELETE; sites 1023/1044/1095/1111/1124/1345; pin test ~2288 retarget)
- Test: repl pin test retargeted to `FileAttr::ALL.map(FileAttr::label)` asserting the same eight literals; snapshot `tests/snapshots/sample_queries__sample_queries_output.snap` unchanged

**Interfaces (Produces):**
- `impl FileAttr { pub const ALL: [FileAttr; 8]; pub fn label(self) -> &'static str; pub fn from_attr_name(name: &str) -> Option<FileAttr>; pub fn value_kind(self) -> &'static str }`

- [ ] **Step 1: Add the impl to `src/model.rs`** (the enum itself is unchanged):

```rust
impl FileAttr {
    /// Every pseudo-column, in the display order `.schema`/`.describe` use.
    pub const ALL: [FileAttr; 8] = [
        FileAttr::Name,
        FileAttr::Path,
        FileAttr::Folder,
        FileAttr::Ext,
        FileAttr::Mtime,
        FileAttr::Size,
        FileAttr::WordCount,
        FileAttr::Body,
    ];

    /// The dotted, user-facing SQL spelling (`file.name`, …). These strings
    /// are pinned by the sample_queries snapshot — do not change them.
    pub fn label(self) -> &'static str {
        match self {
            FileAttr::Name => "file.name",
            FileAttr::Path => "file.path",
            FileAttr::Folder => "file.folder",
            FileAttr::Ext => "file.ext",
            FileAttr::Mtime => "file.mtime",
            FileAttr::Size => "file.size",
            FileAttr::WordCount => "file.word_count",
            FileAttr::Body => "file.body",
        }
    }

    /// Parses the bare, already-lowercased attribute half (`name`,
    /// `word_count`, …) that `parse::lower_compound` splits off — the
    /// counterpart spelling to [`Self::label`]'s dotted form. Two explicit
    /// fns rather than one `FromStr`, because the two spellings differ.
    pub fn from_attr_name(name: &str) -> Option<FileAttr> {
        match name {
            "name" => Some(FileAttr::Name),
            "path" => Some(FileAttr::Path),
            "folder" => Some(FileAttr::Folder),
            "ext" => Some(FileAttr::Ext),
            "mtime" => Some(FileAttr::Mtime),
            "size" => Some(FileAttr::Size),
            "word_count" => Some(FileAttr::WordCount),
            "body" => Some(FileAttr::Body),
        _ => None,
        }
    }

    /// The `.describe` type column for this pseudo-column.
    pub fn value_kind(self) -> &'static str {
        match self {
            FileAttr::Size | FileAttr::WordCount => "Int",
            FileAttr::Name
            | FileAttr::Path
            | FileAttr::Folder
            | FileAttr::Ext
            | FileAttr::Mtime
            | FileAttr::Body => "Str",
        }
    }
}
```

Copy the exact bodies of `ast::file_attr_label` / `parse::file_attr_from_str` if they differ from the above in any spelling — the moved code must be verbatim.

- [ ] **Step 2: Delete the duplicates and fix out.** DELETE `ast::file_attr_label` (callers → `attr.label()`), `parse::file_attr_from_str` (caller keeps its `ParseError::BadColumn` wrapping at the call site), `repl::FILE_COLUMNS` (sites iterate `FileAttr::ALL`; the `.describe` guard at repl.rs:1044 becomes `name.strip_prefix("file.").and_then(FileAttr::from_attr_name)` — feed the parsed attr down so `describe_file_column_line`'s `matches!` at 1095 becomes `attr.value_kind()`). Loop `cargo check --all-targets`.

- [ ] **Step 3: Retarget the repl pin test** (~repl.rs:2288) to assert `FileAttr::ALL.map(FileAttr::label)` equals the same eight dotted literals it asserts today (assertion values unchanged — that's the pin).

- [ ] **Step 4: Tests:** `cargo test file_attr`, `cargo test describe`, `cargo test schema`, `cargo test --test sample_queries` — snapshot must be byte-identical (any snapshot diff = a broken spelling; fix the code, never re-bless).

- [ ] **Step 5: Gates:** clippy `-D warnings` → `cargo fmt --all`.

- [ ] **Step 6: Commit + strip:**

```bash
git add -A
git commit -m "typecheck(parse-dont-validate): FileAttr owns its label/parse/kind mapping, drop FILE_COLUMNS [T4]"
```

Then delete the `### T4. …` block from `TYPECHECK.md`.

---

### Task 6: `Grouping` sum type in the query AST [T5] — **model: opus**

**Files:**
- Modify: `src/query/ast.rs` (Query:27 — replace `distinct`/`group_by`/`having` with `grouping: Grouping`; OrderTarget:405 — remove `Agg`, add `GroupedOrderTarget`/`GroupedOrderKey`), `src/query/parse.rs` (lower_query:93–114, DISTINCT check:95-99, HAVING check:929-931, ORDER BY lowering), `src/query/exec.rs` (is_grouped_or_aggregate:597 — DELETE, dispatch:229/598, distinct:688, having:849, rewrite_relative_dates:412, resolve_group_order_targets, AggregateOrderWithoutGroupBy:2255 — DELETE)
- Test: existing parse/exec tests get mechanical construction/match fixes; parser error-text assertions unchanged; ONE new parse test (Step 3)

**Interfaces (Produces):**
- `ast::Grouping` — see Step 1; `Query.grouping: Grouping` replaces the three fields
- `ast::GroupedOrderKey { target: GroupedOrderTarget, /* same direction field OrderKey has */ }`; `ast::GroupedOrderTarget::{Scalar(OrderTarget), Agg(Aggregate)}`; `OrderTarget` loses its `Agg` variant

- [ ] **Step 1: Add the sum type to `src/query/ast.rs`:**

```rust
/// How the query groups rows — replacing the former independent
/// `distinct` / `group_by` / `having` fields, whose invalid combinations
/// (DISTINCT+GROUP BY, HAVING without GROUP BY, aggregate ORDER BY when
/// ungrouped) were representable and silently ignored by the executor.
#[derive(Debug, Clone, PartialEq)]
pub enum Grouping {
    /// No GROUP BY and no aggregate SELECT item. `order_by` targets are
    /// scalar only — a bare aggregate ORDER BY is now unrepresentable here.
    Ungrouped {
        distinct: bool,
        order_by: Vec<OrderKey>,
    },
    /// GROUP BY (or the implicit single group of an aggregate-only SELECT,
    /// where `keys` is empty). DISTINCT is unrepresentable here; HAVING and
    /// aggregate ORDER BY targets exist only here.
    Grouped {
        keys: Vec<ColRef>,
        having: Option<Having>,
        order_by: Vec<GroupedOrderKey>,
    },
}
```

`OrderTarget` loses `Agg(Aggregate)`; add `GroupedOrderTarget { Scalar(OrderTarget), Agg(Aggregate) }` and `GroupedOrderKey` mirroring `OrderKey`'s fields with the grouped target. `Query.order_by` moves inside the variants (delete the top-level field); `Query.select`/`from_glob`/`filter`/`limit`/`offset` stay put.

- [ ] **Step 2: Rebuild `lower_query` construction.** Compute grouped-ness exactly as `is_grouped_or_aggregate` did (non-empty GROUP BY, or any aggregate SELECT item), THEN lower ORDER BY into the matching target type. The two `unsupported(...)` rejections become structurally impossible where they can be, and stay as parse errors where user input can still request them:
  - `DISTINCT` + explicit `GROUP BY` → keep the existing parse error, text byte-identical (pinned by parse tests).
  - `HAVING` without GROUP BY → keep the existing parse error, text byte-identical.
  - Aggregate ORDER BY target on an ungrouped query → keep the existing parse-or-exec error message text, now raised at parse time when lowering into `OrderKey` fails to find an aggregate slot; DELETE exec's `AggregateOrderWithoutGroupBy` variant if the parser now catches every path to it (compiler will confirm — if any runtime path remains, keep the variant and note why in the commit).

- [ ] **Step 3: New behavior decision (spec-mandated): `SELECT DISTINCT count(*) …` (DISTINCT + implicit aggregate grouping) is silently ignored today — it must now be REJECTED at parse** with error text mirroring the existing DISTINCT+GROUP BY message (e.g. `DISTINCT cannot be combined with aggregate functions`, reusing the `unsupported` channel). Write the parse test FIRST:

```rust
#[test]
fn distinct_with_implicit_aggregate_grouping_is_rejected() {
    let err = parse("select distinct count(*) from x").unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("DISTINCT"), "got: {msg}");
}
```

(Adapt `parse(…)` to this module's actual test helper; assert on the final chosen message.) Run it, confirm it FAILS on the unchanged parser (DISTINCT is currently dropped), then make it pass via the `Grouping` construction.

- [ ] **Step 4: Compiler-driven fixout.** Loop `cargo check --all-targets`. Expect: exec dispatch becomes `match q.grouping { Grouping::Ungrouped { .. } => execute_ungrouped(…), Grouping::Grouped { .. } => execute_grouped(…) }` with each arm receiving exactly its own fields (distinct only ungrouped, having only grouped); `rewrite_relative_dates` walks `having` through the `Grouped` arm; `resolve_group_order_targets` consumes `GroupedOrderKey`; parse tests constructing `Query { distinct, group_by, having, order_by, .. }` literals get mechanical rewrites to the variant form.

- [ ] **Step 5: Tests:** `cargo test group`, `cargo test having`, `cargo test distinct`, `cargo test order`, `cargo test --test sample_queries` — parser error assertions and the snapshot unchanged (except the new Step 3 test).

- [ ] **Step 6: Gates:** clippy `-D warnings` → `cargo fmt --all`.

- [ ] **Step 7: Commit + strip:**

```bash
git add -A
git commit -m "typecheck(illegal-states): Grouping sum type makes DISTINCT/HAVING/agg-ORDER-BY misuse unrepresentable [T5]"
```

Then delete the `### T5. …` block from `TYPECHECK.md`.

---

### Task 7: `RelPath` — traversal check as the constructor [T6]

**Files:**
- Modify: `src/cache.rs` (CachedFile.rel_path:53, scan_file:~523, contained_path:845 — body becomes `RelPath`'s internals, records_from:~909, refresh_per_file:~616, refresh_fast:~756), `src/store.rs` (~272/281/807/898), `src/session.rs` (~1071)
- Test: new bincode layout pin + rejection test in `src/cache.rs` tests

**Interfaces (Produces):**
- `cache::RelPath` — `pub fn parse(s: &str) -> Option<RelPath>` (rejects `ParentDir`/`RootDir`/`Prefix` components, strips `CurDir` — verbatim `contained_path`'s component scan), infallible `pub(crate) fn from_scan(String) -> RelPath` for the `strip_prefix`-derived construction in `scan_file`, `pub fn resolve(&self, dir: &DirPath) -> PathBuf` (the ONLY join path), `pub fn as_str(&self) -> &str`, `Display`, `Serialize` + custom `Deserialize` that validates via `parse`, `Debug + Clone + PartialEq + Eq`
- `CachedFile.rel_path: RelPath`

- [ ] **Step 1: Write the two pin tests first** (red until `RelPath` exists):

```rust
#[test]
fn relpath_bincode_layout_matches_plain_string() {
    // serde(transparent)-style encoding: RelPath must encode exactly as the
    // String it wraps, so cache blobs and SCHEMA_VERSION are untouched.
    assert_eq!(
        encode(&RelPath::parse("notes/a.md").unwrap()),
        encode(&"notes/a.md".to_string()),
    );
}

#[test]
fn relpath_with_parent_traversal_fails_to_decode() {
    let poisoned = encode(&"../escape.md".to_string());
    assert!(decode::<RelPath>(&poisoned).is_none());
    assert!(RelPath::parse("../escape.md").is_none());
}
```

- [ ] **Step 2: Add `RelPath` to `src/cache.rs`:** move `contained_path`'s component scan into `RelPath::parse` (normalizing `CurDir` away, storing the normalized string); `resolve(&self, dir: &DirPath) -> PathBuf` does `dir.join(normalized)` plus the existing belt-and-suspenders `starts_with` check (return the joined path; the check can't fail on a parsed value — keep it as a `debug_assert!`). Custom `Deserialize`: deserialize a `String`, then `RelPath::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("rel_path escapes its directory: {s:?}")))`. `Serialize` delegates to the inner string (implement by hand or `#[serde(into = …)]`-free manual impl — keep it a plain `serializer.serialize_str`).

- [ ] **Step 3: Compiler-driven fixout.** `CachedFile.rel_path: RelPath`. `scan_file` constructs via `RelPath::from_scan(...)` (its input is already a `strip_prefix` result). The raw joins in `refresh_per_file`/`refresh_fast` become `file.rel_path.resolve(&cached_dir.dir)`. `records_from`'s check-and-warn: the parse now happened at decode; keep the `LoadReport` warning path for a blob whose decode FAILS (the deserializer error surfaces through the existing skip-and-report handling at load — verify a poisoned blob produces the same style of warning `records_from` emitted, and keep its message text). `store.rs`/`session.rs` sites take mechanical `as_str()`/`resolve` fixes. DELETE `contained_path` once nothing calls it.

- [ ] **Step 4: Tests:** `cargo test relpath`, `cargo test cache` — including Step 1's pins; `SCHEMA_VERSION` untouched at 3.

- [ ] **Step 5: Gates:** clippy `-D warnings` → `cargo fmt --all`.

- [ ] **Step 6: Commit + strip:**

```bash
git add -A
git commit -m "typecheck(newtype): RelPath makes the cache's traversal check the constructor and the only join path [T6]"
```

Then delete the `### T6. …` block from `TYPECHECK.md`.

---

### Task 8: `CacheMode` replaces the five cache CLI bools [T7]

**Files:**
- Modify: `src/cli.rs` (fields 163–180 go private, freshness:378 — DELETE, validate:392 — absorbed), `src/cache.rs` (Freshness:453 — remove `ForceCache`), `src/main.rs` (build_session 913/931/972/995/1019), `src/store.rs` (173/202 `ForceCache` arms)
- Test: existing `src/cli.rs` unit tests (~559–615) retarget mechanically to `cache_mode()`; `tests/cli.rs` (826/1121/1215/1325/1377) pass unmodified

**Interfaces (Produces):**
- `cli::CacheMode { Live, Cached { freshness: Freshness, refresh: RefreshScope }, TrustCache }`, `cli::RefreshScope { None, All, Subtrees(Vec<PathBuf>) }` — both `Debug + Clone + PartialEq`
- `Cli::cache_mode(&self) -> anyhow::Result<CacheMode>` — the ONLY reader of the five raw flags
- `cache::Freshness` shrinks to `{ PerFile, Fast }`

- [ ] **Step 1: Add the mode types + translation to `src/cli.rs`:**

```rust
/// How this invocation uses the `.querymatter` cache — derived once from the
/// five raw flags, so their four contradictory combinations are rejected in
/// exactly one place and everything downstream matches on a valid mode.
#[derive(Debug, Clone, PartialEq)]
pub enum CacheMode {
    /// `--no-cache`: always live-scan.
    Live,
    /// Normal cached operation.
    Cached { freshness: Freshness, refresh: RefreshScope },
    /// `--force-cache`: trust the cache verbatim, no filesystem access —
    /// structurally carries no refresh scope and no fast mode.
    TrustCache,
}

/// Which part of the vault a `--refresh`/`--refresh-all` re-scans first.
#[derive(Debug, Clone, PartialEq)]
pub enum RefreshScope {
    None,
    All,
    Subtrees(Vec<PathBuf>),
}

impl Cli {
    /// The single fallible translation of the raw cache flags. The four
    /// `ensure!` messages are byte-identical to the deleted `validate`'s
    /// (pinned by tests/cli.rs).
    pub fn cache_mode(&self) -> anyhow::Result<CacheMode> {
        let wants_refresh = self.refresh_all || !self.refresh.is_empty();
        anyhow::ensure!(
            !(self.force_cache && wants_refresh),
            "--force-cache cannot be combined with --refresh/--refresh-all: \
             force-cache never touches the filesystem, so there is nothing to refresh"
        );
        anyhow::ensure!(
            !(self.no_cache && self.force_cache),
            "--no-cache cannot be combined with --force-cache"
        );
        anyhow::ensure!(
            !(self.no_cache && wants_refresh),
            "--no-cache cannot be combined with --refresh/--refresh-all: \
             a refresh needs a cache, which --no-cache disables"
        );
        anyhow::ensure!(
            !(self.force_cache && self.fast),
            "--force-cache cannot be combined with --fast"
        );
        if self.no_cache {
            return Ok(CacheMode::Live);
        }
        if self.force_cache {
            return Ok(CacheMode::TrustCache);
        }
        let freshness = if self.fast { Freshness::Fast } else { Freshness::PerFile };
        let refresh = if self.refresh_all {
            RefreshScope::All
        } else if !self.refresh.is_empty() {
            RefreshScope::Subtrees(self.refresh.clone())
        } else {
            RefreshScope::None
        };
        Ok(CacheMode::Cached { freshness, refresh })
    }
}
```

Make the five raw fields `pub(crate)`-private as far as the compiler allows without renames (clap derive keeps working on private fields); DELETE `Cli::freshness` and `Cli::validate` (its caller in `main` switches to the `?` on `cache_mode()` at the same point, preserving fail-fast ordering).

- [ ] **Step 2: Remove `Freshness::ForceCache`.** Loop `cargo check --all-targets`; every `ForceCache` match arm (store.rs:173/202, cache.rs:579, main.rs:1019) moves to a `CacheMode::TrustCache` decision made ONCE in `build_session`'s single `match cli.cache_mode()?` — the no-vault + TrustCache error and the disk-reads-allowed decision each end up in exactly one arm. Functions below `build_session` receive the already-decided values they need (a `Freshness`, a bool, a `RefreshScope`) rather than re-deriving from `Cli`.

- [ ] **Step 3: Retarget `src/cli.rs` unit tests mechanically:** assertions about `freshness()`/`validate()` become assertions about `cache_mode()`'s variant/error — same inputs, same expected error strings.

- [ ] **Step 4: Tests:** `cargo test cache_mode`, `cargo test --test cli -- cache` plus `cargo test --test cli -- force` — the integration tests pass unmodified (invariant 4).

- [ ] **Step 5: Gates:** clippy `-D warnings` → `cargo fmt --all`.

- [ ] **Step 6: Commit + strip:**

```bash
git add -A
git commit -m "typecheck(illegal-states): CacheMode enum makes contradictory cache flags unrepresentable past the CLI boundary [T7]"
```

Then delete the `### T7. …` block from `TYPECHECK.md`.

---

### Task 9: MILESTONE M2 — full suite after five findings

- [ ] **Step 1:** `cargo test` (full). Expected: green.
- [ ] **Step 2:** On red: bisect T3→T7 commits (`git revert` newest-first until green), revert the offender, surface the diagnosis. Do NOT proceed on red.

---

### Task 10: `ValueKey` — one hash key for `Value` [T8] — **model: opus**

**Files:**
- Modify: `src/model.rs` (add `ValueKey` + `From<&Value>`; `to_cmp_string` keeps its ordering job), `src/query/exec.rs` (hashable_cell_key:1169 — DELETE, group key:1105/1114, dedup_rows:792/794, AggState::CountDistinct:1299/1304/1388)
- Test: new characterization test module (Step 1) — committed separately BEFORE the migration

**Interfaces (Produces):**
- `model::ValueKey` — `#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)] pub enum ValueKey { Null, Bool(bool), Int(i64), Float(u64), Str(String), Date(NaiveDate), DateTime(DateTime<Utc>), List(Vec<ValueKey>), Map(Vec<(String, ValueKey)>) }` with `impl From<&Value> for ValueKey`

- [ ] **Step 1 (SEPARATE COMMIT — characterization first):** add an exec test pinning CURRENT behavior of GROUP BY vs SELECT DISTINCT vs count(distinct) over a mixed-type column containing `Int(1)`, `Str("1")`, `Null`, `Str("")`, `Float(-0.0)`, `Float(0.0)`. Use the module's existing test helpers for building records/running queries. Assert the CURRENT counts (run the test to discover them — expected shape: GROUP BY separates `Int(1)` from `Str("1")` and `Null` from `Str("")`; DISTINCT/count(distinct) merge them). Each assertion gets a comment stating whether T8 will change it. Confirm green on unchanged code, then:

```bash
git add -A
git commit -m "test: characterize mixed-type DISTINCT/GROUP BY/count(distinct) keys before typecheck [T8]"
```

- [ ] **Step 2: Add `ValueKey` to `src/model.rs`:**

```rust
/// The canonical "do these two cells count as the same value" key — the ONE
/// encoding GROUP BY, SELECT DISTINCT, and count(distinct) all hash on.
/// Structural and variant-tagged, so `Int(1)` never collides with `Str("1")`
/// and `Null` never collides with `Str("")`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueKey {
    Null,
    Bool(bool),
    Int(i64),
    /// `f64::to_bits` after normalizing `-0.0` to `0.0` (the same
    /// normalization the deleted `hashable_cell_key` applied).
    Float(u64),
    Str(String),
    Date(NaiveDate),
    DateTime(DateTime<Utc>),
    /// Order-sensitive, matching `Vec` equality.
    List(Vec<ValueKey>),
    /// Key-sorted, so two insertion orders of a structurally equal map
    /// produce one key (matching `IndexMap` equality semantics).
    Map(Vec<(String, ValueKey)>),
}

impl From<&Value> for ValueKey {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => ValueKey::Null,
            Value::Bool(b) => ValueKey::Bool(*b),
            Value::Int(i) => ValueKey::Int(*i),
            Value::Float(f) => {
                let normalized = if *f == 0.0 { 0.0 } else { *f };
                ValueKey::Float(normalized.to_bits())
            }
            Value::Str(s) => ValueKey::Str(s.clone()),
            Value::Date(d) => ValueKey::Date(*d),
            Value::DateTime(dt) => ValueKey::DateTime(*dt),
            Value::List(items) => ValueKey::List(items.iter().map(ValueKey::from).collect()),
            Value::Map(map) => {
                let mut entries: Vec<(String, ValueKey)> =
                    map.iter().map(|(k, v)| (k.clone(), ValueKey::from(v))).collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                ValueKey::Map(entries)
            }
        }
    }
}
```

(Match `Value`'s exact variant list — if `DateTime`'s inner type differs from `DateTime<Utc>`, mirror what `Value` holds.)

- [ ] **Step 3: Replace all three ad-hoc keys.** Loop `cargo check --all-targets` after each: GROUP BY's `HashMap<Vec<String>, usize>` (exec.rs:1105) → `HashMap<Vec<ValueKey>, usize>` built via `ValueKey::from` (DELETE `hashable_cell_key` and its helper docs); dedup_rows' `HashSet<Vec<String>>` (792/794) → `HashSet<Vec<ValueKey>>`; `AggState::CountDistinct`'s `BTreeSet<String>` (1304/1388) → `BTreeSet<ValueKey>`. `to_cmp_string` stays for ORDER BY comparison only — verify remaining callers are ordering/display and leave them.

- [ ] **Step 4: Update the Step-1 characterization test to the NEW semantics in this same commit** — the assertions marked "will change" flip to the unified behavior (DISTINCT and count(distinct) now agree with GROUP BY); the commit message calls the behavior change out.

- [ ] **Step 5: Docs check:** `grep -in 'distinct' README.md docs/*.md` — if any doc states the old mixed-type DISTINCT behavior, update it here.

- [ ] **Step 6: Tests:** `cargo test distinct`, `cargo test group`, `cargo test --test sample_queries` (snapshot should be unaffected — sample data has no mixed-type columns; if it diffs, STOP and diagnose rather than re-bless).

- [ ] **Step 7: Gates:** clippy `-D warnings` → `cargo fmt --all`.

- [ ] **Step 8: Commit + strip:**

```bash
git add -A
git commit -m "typecheck(parse-dont-validate): one ValueKey for GROUP BY/DISTINCT/count(distinct) — mixed-type keys now agree [T8]"
```

Then delete the `### T8. …` block from `TYPECHECK.md`.

---

### Task 11: `QueryName` — validated saved-query names [T9]

**Files:**
- Modify: `src/queries.rs` (Queries:27, set:105, is_valid_name:129 — moves into the ctor), `src/main.rs` (save_named_query:758, QueryAction arms:703/715/722/779), `src/repl.rs` (QueryCmd:175/181, parse_dot .query arms:338/347, dispatch sites ~690/710)
- Test: existing queries round-trip test extended with the wire-shape pin (Step 3)

**Interfaces (Produces):**
- `queries::QueryName` — `#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)] #[serde(transparent)] pub struct QueryName(String);`, `impl FromStr for QueryName { type Err = InvalidQueryName }`, `impl Display`, `impl AsRef<str>`, `impl Borrow<str>`
- `queries::InvalidQueryName(String)` — thiserror, Display exactly: `invalid query name {0:?} (expected letters, digits, '_', or '-' only)`
- `Queries(BTreeMap<QueryName, String>)`; `queries::set(queries: &mut Queries, name: QueryName, sql: &str)` (infallible, returns `()`); `remove`/`get` keep `&str` params via `Borrow`
- `repl::QueryCmd::Run(QueryName)` / `QueryCmd::Save(QueryName, Option<String>)`

- [ ] **Step 1: Add `QueryName` to `src/queries.rs`:**

```rust
/// A saved-query name, valid by construction: `FromStr` is the ONLY public
/// constructor and carries the former `is_valid_name` rule.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueryName(String);

/// Rejection carrying the exact message `queries::set` used to produce.
#[derive(Debug, thiserror::Error)]
#[error("invalid query name {0:?} (expected letters, digits, '_', or '-' only)")]
pub struct InvalidQueryName(String);

impl std::str::FromStr for QueryName {
    type Err = InvalidQueryName;
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let valid = !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if valid {
            Ok(QueryName(name.to_string()))
        } else {
            Err(InvalidQueryName(name.to_string()))
        }
    }
}

impl std::fmt::Display for QueryName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl AsRef<str> for QueryName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl std::borrow::Borrow<str> for QueryName {
    fn borrow(&self) -> &str {
        &self.0
    }
}
```

NOTE: `Deserialize` stays non-validating (plain transparent) deliberately — a hand-edited `queries.toml` with an odd name still loads, exactly as today; validation guards the WRITE boundaries. Do not add a validating deserializer.

- [ ] **Step 2: Change the map and `set`.** `Queries(BTreeMap<QueryName, String>)`; `set(queries: &mut Queries, name: QueryName, sql: &str)` drops the `ensure!` and returns `()`. Loop `cargo check --all-targets`; boundaries call `.parse::<QueryName>()?` (main.rs's clap `QueryAction` arms and `save_named_query`; repl's `.query run`/`.query save` lowering — its parse failure routes to the existing bad-name error path with the same message text, via `anyhow::Error::from(InvalidQueryName)`). `names()`/`iter()` keep returning `&str` via `as_ref()`. `QueryCmd::Run(QueryName)` / `Save(QueryName, Option<String>)` — repl tests constructing these get mechanical `.parse().unwrap()` fixes.

- [ ] **Step 3: Extend the existing round-trip test in `src/queries.rs`** with the wire-shape pin (keep the existing assertions; add):

```rust
// Wire-shape pin: queries.toml stays flat `name = "sql"` lines — the
// QueryName newtype must be invisible in the serialized form.
let toml_text = std::fs::read_to_string(&path).unwrap();
assert!(
    toml_text.contains("drafts = "),
    "expected flat top-level key, got:\n{toml_text}"
);
```

(Adapt the saved name to whatever the existing round-trip test already saves.)

- [ ] **Step 4: Tests:** `cargo test queries`, `cargo test query_name`, `cargo test --test cli -- query` — the CLI error text for an invalid name is byte-identical (same message, now from `InvalidQueryName`).

- [ ] **Step 5: Gates:** clippy `-D warnings` → `cargo fmt --all`.

- [ ] **Step 6: Commit + strip:**

```bash
git add -A
git commit -m "typecheck(newtype): QueryName is valid by construction; (name, sql) can no longer transpose [T9]"
```

Then delete the `### T9. …` block from `TYPECHECK.md`.

---

### Task 12: MILESTONE M3 — end of batch

- [ ] **Step 1:** `cargo test` (full) — green, snapshots byte-identical except changes T5/T8 explicitly made.
- [ ] **Step 2:** `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check` — both clean.
- [ ] **Step 3:** Verify `TYPECHECK.md` no longer contains blocks T1–T9 (T10–T40 remain) and that `git log --oneline main..` shows one commit per finding plus the T8 characterization commit.
- [ ] **Step 4:** On red: bisect T8→T9, revert the offender, surface the diagnosis.
