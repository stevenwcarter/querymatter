# `.querymatterignore` — design spec

**Date:** 2026-07-23
**Status:** Approved (brainstorming complete)
**Builds on:** `docs/superpowers/specs/2026-07-22-querymatter-design.md`

## 1. Summary

Add a **`.querymatterignore`** file: a gitignore-style ignore file that
`querymatter` **always honors** when present. It is auto-discovered in the
current working directory, can be augmented (or replaced) with explicit
`--ignore-file <PATH>` flags, and can have its cwd auto-discovery turned off with
`--no-ignore-file`. All ignore sources layer with the existing `--exclude`
globs and the opt-in `--respect-gitignore`.

This complements `--exclude` (ad-hoc per-invocation globs) with a persistent,
project-level, gitignore-syntax ignore file — the natural way to say "never
query the `templates/` and `archive/` trees in this project."

### Goals
- A `.querymatterignore` file using full **gitignore semantics** (`!` negation,
  `dir/`, leading-`/` anchoring, `**`, `#` comments), applied via the `ignore`
  crate we already depend on.
- **Always honored** when present — independent of `--respect-gitignore` (which
  stays specific to `.gitignore`).
- Auto-discovered in **cwd**; augmentable via repeatable **`--ignore-file`**;
  cwd auto-discovery disabled by **`--no-ignore-file`**.
- Architected so the future `.querymatter` vault (`TODO.md`) can add a
  **vault-parent** `.querymatterignore` at a single seam — v1 leaves the seam,
  does not implement vault discovery.

### Non-goals (this change)
- No `.querymatter` vault marker / upward search / TTL cache (that is the
  separate `TODO.md` feature; only the ignore-file resolution seam is prepared).
- No **hierarchical/per-directory** `.querymatterignore` discovery within the
  scanned trees (a single project-level file at cwd, plus explicit files). If
  wanted later, it is an additive change to `discover`.
- No new pattern dialect — gitignore semantics via the `ignore` crate only.

## 2. CLI additions

Two new flags on `Cli` (`src/cli.rs`):

| Flag | Meaning |
| --- | --- |
| `--ignore-file <PATH>` | Apply this gitignore-style ignore file. **Repeatable** (applied in the order given). A missing/unreadable path is a hard error naming the path (validated up front, like `--exclude`). |
| `--no-ignore-file` | Skip auto-discovery of the cwd `.querymatterignore`. Explicit `--ignore-file`s still apply. |

No other flags change. `--exclude`, `--respect-gitignore`, `--hidden`, `--ext`,
`--format`, `-e/--query`, and `[DIRS]...` keep their current behavior.

## 3. Ignore-file resolution (the seam)

A single method on `Cli` builds the ordered list of ignore files to apply:

```rust
/// Ordered list of gitignore-style ignore files to apply, earliest first.
/// v1: the cwd `.querymatterignore` (unless `--no-ignore-file`) followed by
/// each `--ignore-file` in order. A future `.querymatter` vault prepends the
/// vault-parent `.querymatterignore` here — this is the only place that changes.
pub fn ignore_files(&self) -> anyhow::Result<Vec<PathBuf>>
```

Resolution order (v1):
1. **Unless `--no-ignore-file`:** `<cwd>/.querymatterignore` **if it exists**
   (silently omitted when absent — a missing cwd ignore file is normal, not an
   error). `cwd` is `std::env::current_dir()`.
2. **Each `--ignore-file <path>` in the order given.** Each must exist and be
   readable; otherwise return an `anyhow` error naming the path (checked here so
   the failure is a clean startup error, not a silent no-op deep in discovery).

The returned paths are handed to `WalkOpts` (see §4). This resolver is the
**design-for-extension seam** (§7): the future vault-parent file is prepended
in exactly this function.

**Validation** mirrors `validate_excludes`: a `--ignore-file` that does not
exist / cannot be read is rejected up front with a named error. (The cwd file's
absence is *not* an error — it is optional.)

## 4. `discover` / `WalkOpts` changes

Add one field to `WalkOpts` (`src/discover.rs`):

```rust
pub struct WalkOpts {
    pub exts: Vec<String>,
    pub respect_gitignore: bool,
    pub hidden: bool,
    pub excludes: Vec<String>,
    /// Gitignore-style ignore files to apply (earliest first), always honored
    /// regardless of `respect_gitignore`. Paths as resolved by `Cli::ignore_files`.
    pub ignore_files: Vec<PathBuf>,
}
```
`Default` sets `ignore_files: Vec::new()`.

In `discover`, apply each ignore file to the `WalkBuilder` **so its patterns are
honored even though `standard_filters(false)` / `ignore(false)` are set** (the
`.querymatterignore` is our own always-on mechanism, not `.gitignore`).

### Implementation approach + risk (must verify — the `require_git` lesson)
The clean path is `ignore::WalkBuilder::add_ignore(path)` for each ignore file.
**But** `discover` deliberately runs with `standard_filters(false)` and
`ignore(false)` (gitignore off by default), and it is *not verified* that an
explicitly-added `add_ignore` file is still applied under that configuration.
This is exactly the class of `ignore`-crate surprise that produced the Task-4
`require_git(false)` fix.

The plan MUST verify empirically (a throwaway probe / the first test) whether
`add_ignore` is honored with our toggles:
- **If yes:** use `add_ignore` per file; on the (rare) load error surface it —
  but since §3 pre-validates existence/readability, load errors should not occur
  for the resolved paths; a per-line parse issue is tolerated by the crate.
- **If no** (the toggles suppress `add_ignore` too): build an explicit
  `ignore::gitignore::Gitignore` matcher per file via `GitignoreBuilder` (with
  the ignore file's directory as the anchor root), and filter discovered paths
  against the combined matchers inside `discover` (full control; unaffected by
  the walker toggles). Negation/anchoring come from the crate's gitignore
  matcher either way.

Either way the **observable behavior is identical** and is pinned by the tests
in §10; the choice is an internal detail. `discover` keeps returning `Vec<PathBuf>`
(no new `Result`) — ignore-file loadability is guaranteed by §3's up-front
validation.

### Precedence within discovery
For each discovered file: (a) the ignore-file matchers decide keep/drop first
(gitignore negation resolves within/across the files, earliest-first), then
(b) the extension filter, then (c) the `--exclude` globset. A file dropped by any
stage is excluded.

## 5. Semantics

- **Syntax:** exact gitignore semantics via the `ignore` crate — `!` negation,
  `dir/` (directory match), leading `/` (anchor to the ignore file's directory),
  `**`, and `#` comments. A non-anchored pattern (`templates/`) matches that name
  at **any depth** under the scanned tree; an anchored pattern (`/templates/`)
  matches only relative to the ignore file's directory.
- **Always-on:** a present `.querymatterignore` (cwd or `--ignore-file`) is
  applied regardless of `--respect-gitignore`. `--respect-gitignore` remains
  solely about `.gitignore`/`.ignore` auto-discovery.
- **Composition:** ignore files + `--exclude` globs + (optional) `.gitignore` +
  `--hidden` all layer. `--no-ignore-file` disables only the cwd auto-discovery,
  not explicit `--ignore-file`s.
- **Anchoring across scan roots (§8.1):** patterns are relative to the ignore
  file's directory (cwd for the auto-discovered file). Behavior is pinned by a
  characterization test.

## 6. Wiring (`main.rs`)

`main` already calls `cli.walk_opts()` and `cli.resolved_roots()`. Add:
`cli.ignore_files()?` → set on the `WalkOpts` before `InMemoryStore::load`.
`walk_opts()` includes `ignore_files` (so it must either take the resolved list
as an argument, or `main` sets the field after calling `walk_opts()`).
Chosen shape: `walk_opts(&self) -> WalkOpts` stays flag-only (returns
`ignore_files: Vec::new()`); `main` then sets
`opts.ignore_files = cli.ignore_files()?` before `InMemoryStore::load`, keeping
the fallible resolution at the `main`/anyhow boundary, consistent with how
`resolved_roots()` is called.

## 7. Design-for-extension: vault-parent (future)

The future `.querymatter` vault feature (`TODO.md`) resolves a vault directory by
walking up from cwd. When it lands, the vault-parent `.querymatterignore` is
added to the ignore list by **prepending it inside `Cli::ignore_files()`** (or a
successor that takes the resolved vault path). Nothing in `discover`, `WalkOpts`,
or the matching logic changes — the ignore-file list is already the abstraction.
This mirrors the existing single-root-resolution seam (`resolved_roots`).

## 8. Edge cases & decisions

### 8.1 Anchoring of a cwd ignore file vs. scan roots elsewhere
A cwd `.querymatterignore` is anchored at cwd. Non-anchored patterns (`templates/`)
match at any depth, so `querymatter ./docs` (cwd = project root) ignores
`docs/**/templates/` intuitively. A scan root **outside** cwd (e.g.
`querymatter /other/tree` from an unrelated cwd) means cwd-anchored patterns may
not apply to `/other/tree`. This is accepted (a project ignore file governs the
project); documented in the README and pinned by a characterization test.

### 8.2 Missing cwd `.querymatterignore`
Not an error — silently omitted. Only an explicit `--ignore-file <path>` that is
missing/unreadable is an error.

### 8.3 `--no-ignore-file` with no `--ignore-file`
Valid: disables the cwd file, applies no ignore files (only `--exclude`/gitignore
if set). Effectively "ignore my project ignore file for this run."

### 8.4 Empty / comment-only ignore file
No-op (no patterns) — all files pass the ignore stage.

## 9. Invariants this feature depends on
Per repo discipline, a change to these funnels must re-verify these producers:
- **`discover` filter pipeline:** the existing behaviors must still hold with the
  new ignore stage present — gitignore-off-by-default (`gitignored_files_found_by_default`),
  `--respect-gitignore` opt-in, `--exclude` globs, hidden-skip, case-insensitive
  ext. The new ignore stage must not regress them (they run with empty
  `ignore_files`). Existing discover tests guard this.
- **Always-on vs. `--respect-gitignore`:** a `.querymatterignore` must exclude
  its matches even when `respect_gitignore: false` — a dedicated test (this is the
  load-bearing invariant that distinguishes it from `.gitignore`).
- **stdout cleanliness:** ignore-file *resolution errors* (missing `--ignore-file`)
  go to stderr via the `anyhow`/`main` boundary and exit non-zero; they never
  reach stdout. An integration test asserts a missing `--ignore-file` exits
  non-zero with a named error.

## 10. Testing (TDD)

- **Unit — `discover`:** a `.querymatterignore`-style file passed via
  `ignore_files` excludes its matches (temp tree; assert an ignored `.md` is
  absent); `!` negation re-includes a file; the ignore file applies with
  `respect_gitignore: false` (always-on); an empty ignore file is a no-op;
  ignore + `--exclude` + ext filter compose. Include the **characterization test**
  for anchoring (non-anchored `templates/` matches at depth; document the observed
  cwd-vs-root behavior).
- **Unit — `cli`:** `ignore_files()` returns the cwd file when it exists;
  omits it under `--no-ignore-file`; appends `--ignore-file`s in order; errors on
  a missing `--ignore-file` path; the cwd file's absence is not an error. (Use a
  temp dir + set/restore cwd, or inject cwd — the plan picks the mechanism.)
- **Integration — `tests/cli.rs`:** a fixtures tree containing a
  `.querymatterignore` that ignores `templates/`; run the headline query and
  assert the template rows are absent (parity with the `--exclude` template test).
  A `--ignore-file` pointing at a nonexistent path exits non-zero with a clear
  stderr error.

## 11. Docs
- **README:** a "`.querymatterignore`" section — gitignore syntax with a small
  example (incl. a `!` negation), discovery in cwd (vault-parent "later"),
  `--ignore-file` (repeatable) and `--no-ignore-file`, always-on semantics, and
  how it composes with `--exclude`/`--respect-gitignore`. Add the two flags to the
  flags list.
- **Spec cross-reference:** note in the base spec (or here) that ignore-file
  resolution is the seam the `TODO.md` vault feature extends.

## 12. Crates
No new dependencies — `ignore` (already present) provides `WalkBuilder::add_ignore`
and `gitignore::GitignoreBuilder`. `anyhow` for the resolution errors.
