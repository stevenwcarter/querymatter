# querymatter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Implementers are `rust-developer` agents: all code must be clippy-clean (`cargo clippy --all-targets -- -D warnings`) and rustfmt-clean (`cargo fmt --all`), pre-empting `/tidy` and `/typecheck`.

**Goal:** A REPL-first Rust CLI (`querymatter`) that queries Markdown YAML frontmatter with a SQL subset and renders results as table/JSON/CSV/TSV/Markdown.

**Architecture:** Load phase (`discover` → `frontmatter` → `model::Record`) fills a directory-keyed `store::RecordStore`. Query phase (`query::parse` via `sqlparser` → `query::exec`) produces a `ResultTable` that `render` prints. A `session` glues store + format; `main` dispatches REPL / one-shot / batch. The `RecordStore` trait and directory-keyed slices are the seams a future TTL cache reuses.

**Tech Stack:** Rust edition 2024; `clap` (derive), `ignore`, `gray_matter`, `sqlparser`, `comfy-table`, `serde_json`, `csv`, `rustyline`, `directories`, `indexmap`, `regex`, `anyhow`, `thiserror`. Dev: `assert_cmd`, `predicates`, `insta`, `tempfile`.

**Spec:** `docs/superpowers/specs/2026-07-22-querymatter-design.md`

## Global Constraints

- **Edition 2024** for the crate; `rustfmt.toml` has `edition = "2024"`. Crate name and binary name are both `querymatter` (repo dir remains `hub-reader/`).
- **Commit `Cargo.lock`** in the same commit that adds any dependency (CI uses `--locked`).
- **clippy/fmt clean** at every commit: `cargo fmt --all` and `cargo clippy --all-targets --all-features -- -D warnings` both pass.
- **OpenSSL-free** (trivially satisfied — no TLS deps here; do not add any that pull `openssl`/`native-tls`).
- **stdout is for query results only.** All warnings, prompts, and diagnostics go to **stderr**, so piping (`querymatter -e '…' | jq`) stays valid. (Spec §10.)
- **`.gitignore` is NOT honored by default** while walking; opt in via `--respect-gitignore`. Hidden dirs skipped unless `--hidden`. (Spec §8.1.)
- **Files with no frontmatter block are skipped** (not all-NULL rows); files with an unparseable YAML block are skipped with a stderr warning. (Spec §8.4, §7.)
- Every commit message ends with:
  `Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6`

## File Structure

```
Cargo.toml, Cargo.lock, rustfmt.toml
src/
  main.rs        # wiring + mode dispatch (Task 10)
  cli.rs         # clap Parser (Task 10)
  model.rs       # Value, FileAttr, Record, coercion/compare/display (Tasks 1-2)
  frontmatter.rs # extract(content) -> fields | none | invalid (Task 3)
  discover.rs    # WalkOpts, discover(root, opts) (Task 4)
  store.rs       # RecordStore trait, InMemoryStore, DirSlice, LoadReport (Task 5)
  session.rs     # Session: store + format; run/set_format/reload (Task 10)
  repl.rs        # rustyline loop + testable line-processing core + dot-commands (Task 11)
  query/
    mod.rs       # re-exports; ResultTable (Task 7)
    ast.rs       # Query AST types (Task 6)
    parse.rs     # sqlparser -> Query; FROM preprocess (Task 6)
    exec.rs      # execute Query over records -> ResultTable (Tasks 7-8)
  render.rs      # ResultTable -> table/json/csv/tsv/md (Task 9)
tests/
  fixtures/…     # committed sample tree (Task 12)
  cli.rs         # assert_cmd integration tests (Tasks 10, 12)
```

---

### Task 1: Crate scaffold + `model::Value`

**Files:**
- Create: `Cargo.toml`, `rustfmt.toml`, `src/main.rs`, `src/model.rs`
- Test: inline `#[cfg(test)]` in `src/model.rs`

**Interfaces:**
- Produces:
  - `pub enum Value { Null, Bool(bool), Int(i64), Float(f64), Str(String), List(Vec<Value>) }`
  - `impl Value { pub fn is_null(&self) -> bool; pub fn display(&self) -> String; pub fn as_number(&self) -> Option<f64>; pub fn to_cmp_string(&self) -> String; }`
  - `pub fn compare_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering>` — total-ish order used by `ORDER BY`/`MIN`/`MAX`: numbers numerically, else lexicographic on `to_cmp_string`; `Null` compared with anything returns `None` (caller places NULLs last).

- [ ] **Step 1: Scaffold the crate**

```bash
cargo init --name querymatter --bin /home/steve/src/hub-reader
printf 'edition = "2024"\n' > /home/steve/src/hub-reader/rustfmt.toml
cd /home/steve/src/hub-reader
# set edition in Cargo.toml to 2024 (cargo init may emit 2021)
cargo add indexmap
cargo add anyhow thiserror
```
Ensure `Cargo.toml` `[package]` has `edition = "2024"`. Add:
```toml
[profile.release]
codegen-units = 1
lto = "thin"
opt-level = 3
```

- [ ] **Step 2: Write the failing tests** (`src/model.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn display_null_is_empty() {
        assert_eq!(Value::Null.display(), "");
    }
    #[test]
    fn display_list_is_comma_joined() {
        let v = Value::List(vec![Value::Str("a".into()), Value::Int(2)]);
        assert_eq!(v.display(), "a, 2");
    }
    #[test]
    fn display_scalars() {
        assert_eq!(Value::Str("x".into()).display(), "x");
        assert_eq!(Value::Int(10).display(), "10");
        assert_eq!(Value::Bool(true).display(), "true");
        assert_eq!(Value::Float(1.5).display(), "1.5");
    }
    #[test]
    fn as_number_coerces_numeric_strings() {
        assert_eq!(Value::Int(3).as_number(), Some(3.0));
        assert_eq!(Value::Str("3".into()).as_number(), Some(3.0));
        assert_eq!(Value::Str("x".into()).as_number(), None);
        assert_eq!(Value::Null.as_number(), None);
    }
    #[test]
    fn compare_numbers_numerically() {
        assert_eq!(compare_values(&Value::Int(2), &Value::Int(10)), Some(Ordering::Less));
        // numeric string vs int compares numerically
        assert_eq!(compare_values(&Value::Str("2".into()), &Value::Int(10)), Some(Ordering::Less));
    }
    #[test]
    fn compare_strings_lexicographically() {
        assert_eq!(compare_values(&Value::Str("a".into()), &Value::Str("b".into())), Some(Ordering::Less));
    }
    #[test]
    fn compare_null_is_none() {
        assert_eq!(compare_values(&Value::Null, &Value::Int(1)), None);
        assert_eq!(compare_values(&Value::Int(1), &Value::Null), None);
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --lib model`
Expected: FAIL (compile error — `Value` not defined).

- [ ] **Step 4: Implement `Value`** to satisfy the tests. Key rules: `display` → `Null` empty, `List` joins element `display()` with `", "`, `Float` uses `{}` (so `1.5` not `1.50`). `as_number` parses `Int`/`Float` directly and `Str` via `trim().parse::<f64>()`. `compare_values`: if both `as_number().is_some()` compare as `f64` (`partial_cmp`); else if either is `Null` return `None`; else compare `to_cmp_string()`. `to_cmp_string` = `display()`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib model` → PASS. Then `cargo fmt --all && cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "feat: scaffold querymatter crate and Value model

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 2: `model::Record` + `file.*` resolution

**Files:**
- Modify: `src/model.rs`
- Test: inline in `src/model.rs`

**Interfaces:**
- Consumes: `Value` (Task 1).
- Produces:
  - `pub enum FileAttr { Name, Path, Folder, Ext }`
  - `pub struct Record { fields: indexmap::IndexMap<String, Value>, name: String, path: String, folder: String, ext: String }`
  - `impl Record { pub fn new(root: &std::path::Path, path: &std::path::Path, fields: IndexMap<String, Value>) -> Self; pub fn field(&self, name: &str) -> Value; pub fn file_attr(&self, attr: FileAttr) -> Value; pub fn field_names(&self) -> impl Iterator<Item = &str>; }`
  - `field` returns `Value::Null` when the key is absent (clone otherwise). `file_attr` returns the corresponding path component as `Value::Str`.

`Record::new` computes `path` as the file path **relative to `root`** (via `path.strip_prefix(root)`, falling back to the full path if not a prefix), `name` = file name, `folder` = parent of the relative path (empty string if none), `ext` = extension without the dot (empty string if none). Store as strings.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod record_tests {
    use super::*;
    use indexmap::IndexMap;
    use std::path::Path;

    fn rec() -> Record {
        let mut f = IndexMap::new();
        f.insert("status".to_string(), Value::Str("draft".into()));
        Record::new(Path::new("samples"), Path::new("samples/plans/DCP-459.md"), f)
    }
    #[test]
    fn file_attrs_relative_to_root() {
        let r = rec();
        assert_eq!(r.file_attr(FileAttr::Name), Value::Str("DCP-459.md".into()));
        assert_eq!(r.file_attr(FileAttr::Path), Value::Str("plans/DCP-459.md".into()));
        assert_eq!(r.file_attr(FileAttr::Folder), Value::Str("plans".into()));
        assert_eq!(r.file_attr(FileAttr::Ext), Value::Str("md".into()));
    }
    #[test]
    fn field_present_and_missing() {
        let r = rec();
        assert_eq!(r.field("status"), Value::Str("draft".into()));
        assert_eq!(r.field("nope"), Value::Null);
    }
    #[test]
    fn field_names_lists_keys() {
        let r = rec();
        assert_eq!(r.field_names().collect::<Vec<_>>(), vec!["status"]);
    }
}
```

- [ ] **Step 2: Run to verify fail** — `cargo test --lib record` → FAIL.
- [ ] **Step 3: Implement `FileAttr` + `Record`.** Use `std::path` for the components; normalize separators to `/` (use `to_string_lossy()` and, on the relative path, `components()` joined by `/` for cross-platform-stable output — folder is the parent joined by `/`).
- [ ] **Step 4: Run to verify pass** — `cargo test --lib record` → PASS; fmt + clippy clean.
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat: Record with file.* pseudo-column resolution

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 3: `frontmatter::extract`

**Files:**
- Create: `src/frontmatter.rs`; Modify: `src/main.rs` (add `mod frontmatter;`)
- Test: inline in `src/frontmatter.rs`

**Interfaces:**
- Consumes: `Value` (Task 1).
- Produces:
  - `pub enum Extract { None, Invalid(String), Fields(indexmap::IndexMap<String, Value>) }`
  - `pub fn extract(content: &str) -> Extract` — `None` when there is no `---` frontmatter fence, `Invalid(msg)` when a fence exists but its YAML fails to parse, `Fields(map)` otherwise.

- [ ] **Step 1: Add the dependency**
```bash
cargo add gray_matter
```
Use `gray_matter`'s YAML engine to split the fence and parse the block, then convert its dynamic value (`Pod`) into our `Value`. Confirm the exact `gray_matter` 0.3 API against docs.rs (Matter::<YAML>::new().parse(...)); map `Pod` → `Value` as: string→`Str`, integer→`Int`, float→`Float`, boolean→`Bool`, array→`List` (recursively), hash/mapping→`Str` of its compact form, null/absent→`Null`. A file whose content has no leading `---` fence → `Extract::None` (gray_matter yields empty/None data). A fence with malformed YAML → `Extract::Invalid`.

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;

    #[test]
    fn scalars_parse_to_fields() {
        let c = "---\njira: DCP-459\nstatus: draft\n---\n# body\n";
        match extract(c) {
            Extract::Fields(m) => {
                assert_eq!(m.get("jira"), Some(&Value::Str("DCP-459".into())));
                assert_eq!(m.get("status"), Some(&Value::Str("draft".into())));
            }
            other => panic!("expected Fields, got {other:?}"),
        }
    }
    #[test]
    fn no_fence_is_none() {
        assert!(matches!(extract("# just a heading\n"), Extract::None));
    }
    #[test]
    fn empty_string_is_none() {
        assert!(matches!(extract(""), Extract::None));
    }
    #[test]
    fn invalid_yaml_is_invalid() {
        let c = "---\nkey: : : broken\n  bad indent\n---\n";
        assert!(matches!(extract(c), Extract::Invalid(_)));
    }
    #[test]
    fn list_value_becomes_list() {
        let c = "---\ntags:\n  - a\n  - b\n---\n";
        match extract(c) {
            Extract::Fields(m) => assert_eq!(
                m.get("tags"),
                Some(&Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]))
            ),
            other => panic!("expected Fields, got {other:?}"),
        }
    }
    // Characterization test: pin how our gray_matter version parses a leading-zero
    // scalar. RUN THIS, observe the actual Value, then lock the assertion to it and
    // leave a comment stating the observed behavior (spec §8.3). Both `Int(10)` and
    // `Str("010")` are legitimate observed outcomes depending on the YAML engine.
    #[test]
    fn leading_zero_characterization() {
        let c = "---\nprd: 010\n---\n";
        let Extract::Fields(m) = extract(c) else { panic!("expected Fields") };
        let got = m.get("prd").cloned().unwrap();
        // After first run, replace the line below with the observed value and a note,
        // e.g.: assert_eq!(got, Value::Int(10)); // gray_matter parses 010 as int 10
        assert!(matches!(got, Value::Int(_) | Value::Str(_)), "unexpected: {got:?}");
    }
    // Definitive: a *quoted* leading-zero stays a string (this is the invariant the
    // docs promise — spec §8.3).
    #[test]
    fn quoted_leading_zero_is_string() {
        let c = "---\nprd: \"010\"\n---\n";
        let Extract::Fields(m) = extract(c) else { panic!("expected Fields") };
        assert_eq!(m.get("prd"), Some(&Value::Str("010".into())));
    }
}
```
`Extract` must `derive(Debug)` for the `{other:?}` panics.

- [ ] **Step 3: Run to verify fail** — `cargo test --lib frontmatter` → FAIL.
- [ ] **Step 4: Implement `extract`** + the `Pod`→`Value` conversion. After it compiles, **run the characterization test, observe the printed value, and tighten `leading_zero_characterization` to assert the exact observed `Value` with an explanatory comment.**
- [ ] **Step 5: Run to verify pass** — `cargo test --lib frontmatter` → PASS; fmt + clippy clean.
- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "feat: frontmatter extraction with leading-zero characterization

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 4: `discover` — directory walk

**Files:**
- Create: `src/discover.rs`; Modify: `src/main.rs` (`mod discover;`)
- Test: inline in `src/discover.rs` (use `tempfile`)

**Interfaces:**
- Produces:
  - `pub struct WalkOpts { pub exts: Vec<String>, pub respect_gitignore: bool, pub hidden: bool, pub excludes: Vec<String> }`
  - `impl Default for WalkOpts` → `exts: ["md","markdown"]`, all bools `false`, no excludes.
  - `pub fn discover(root: &std::path::Path, opts: &WalkOpts) -> Vec<std::path::PathBuf>` — files under `root` (recursive) whose extension is in `opts.exts` and which are not matched by any `opts.excludes` glob. Sorted for determinism.

- [ ] **Step 1: Dependencies**
```bash
cargo add ignore globset
cargo add --dev tempfile
```
Build the walker with `ignore::WalkBuilder::new(root)`, then:
`.git_ignore(opts.respect_gitignore).git_exclude(opts.respect_gitignore).git_global(opts.respect_gitignore).ignore(opts.respect_gitignore).parents(opts.respect_gitignore).hidden(!opts.hidden).standard_filters(false)` — set the individual toggles explicitly so gitignore is OFF and hidden is skipped by default. Filter entries to files whose extension is in `exts`. Compile `opts.excludes` into a `globset::GlobSet` and skip any path that matches. Sort the result.

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(dir: &std::path::Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn finds_md_skips_others_and_hidden() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "a.md", "x");
        touch(td.path(), "sub/b.markdown", "x");
        touch(td.path(), "c.txt", "x");
        touch(td.path(), ".hidden/d.md", "x");
        let got = discover(td.path(), &WalkOpts::default());
        let names: Vec<_> = got.iter().map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["a.md", "b.markdown"]); // c.txt and hidden dir excluded
    }
    #[test]
    fn gitignored_files_found_by_default() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".gitignore", "ignored.md\n");
        touch(td.path(), "ignored.md", "x");
        touch(td.path(), "kept.md", "x");
        let got = discover(td.path(), &WalkOpts::default());
        assert_eq!(got.len(), 2, "gitignore must be ignored by default");
    }
    #[test]
    fn respect_gitignore_hides_them() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".gitignore", "ignored.md\n");
        touch(td.path(), "ignored.md", "x");
        touch(td.path(), "kept.md", "x");
        let opts = WalkOpts { respect_gitignore: true, ..Default::default() };
        let got = discover(td.path(), &opts);
        let names: Vec<_> = got.iter().map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["kept.md"]);
    }
    #[test]
    fn exclude_glob_skips() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "keep.md", "x");
        touch(td.path(), "templates/t.md", "x");
        let opts = WalkOpts { excludes: vec!["**/templates/**".into()], ..Default::default() };
        let got = discover(td.path(), &opts);
        let names: Vec<_> = got.iter().map(|p| p.file_name().unwrap().to_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["keep.md"]);
    }
}
```

- [ ] **Step 3: Run to verify fail** — `cargo test --lib discover` → FAIL.
- [ ] **Step 4: Implement `discover` + `WalkOpts`.** Note `.gitignore` files themselves are dotfiles; with `hidden(true)` (skip hidden) a top-level `.gitignore` is not returned anyway because its extension isn't in `exts` — fine. Ensure excludes match on the path relative to `root` and the absolute path both (compile globs, test against `path` and `path.strip_prefix(root)`).
- [ ] **Step 5: Run to verify pass** — `cargo test --lib discover` → PASS; fmt + clippy clean.
- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "feat: directory walk with gitignore-off default and excludes

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 5: `store` — RecordStore trait + InMemoryStore

**Files:**
- Create: `src/store.rs`; Modify: `src/main.rs` (`mod store;`)
- Test: inline in `src/store.rs` (use `tempfile`)

**Interfaces:**
- Consumes: `Record` (Task 2), `Extract`/`extract` (Task 3), `WalkOpts`/`discover` (Task 4).
- Produces:
  - `pub struct LoadReport { pub loaded: usize, pub skipped: usize, pub warnings: Vec<String> }`
  - `pub struct DirSlice { pub root: std::path::PathBuf, pub records: Vec<Record>, pub scanned_at: std::time::SystemTime }`
  - `pub trait RecordStore { fn records(&self) -> Box<dyn Iterator<Item = &Record> + '_>; fn schema(&self) -> Vec<String>; fn reload_dir(&mut self, root: &std::path::Path) -> LoadReport; fn reload_all(&mut self) -> LoadReport; fn roots(&self) -> Vec<std::path::PathBuf>; }`
  - `pub struct InMemoryStore { slices: Vec<DirSlice>, opts: WalkOpts }`
  - `impl InMemoryStore { pub fn load(roots: Vec<PathBuf>, opts: WalkOpts) -> (Self, LoadReport); }`
  - `schema()` returns the **sorted** union of frontmatter field names across all records (deterministic; spec §6 note — sorted rather than first-seen because the YAML engine's map is unordered).

Loading a root: `discover(root, opts)` → for each file, read to string, `extract(content)`: `Fields` → build `Record::new(root, path, fields)` (loaded += 1); `None` → skip silently; `Invalid(msg)` → skip, push a warning `"<path>: <msg>"` (skipped += 1). `reload_dir(root)` rebuilds only the matching slice's `records` and refreshes `scanned_at`, leaving other slices untouched; if no slice matches `root`, it appends a new one. `reload_all` reloads every existing root.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &std::path::Path, rel: &str, body: &str) {
        let p = dir.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    }

    #[test]
    fn loads_records_and_skips_no_frontmatter() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\n---\n");
        write(td.path(), "b.md", "no frontmatter here\n");
        let (store, report) = InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        assert_eq!(report.loaded, 1);
        assert_eq!(store.records().count(), 1);
    }
    #[test]
    fn schema_is_sorted_union() {
        let td = TempDir::new().unwrap();
        write(td.path(), "a.md", "---\nstatus: draft\njira: X\n---\n");
        write(td.path(), "b.md", "---\nepic: E\nstatus: synced\n---\n");
        let (store, _) = InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        assert_eq!(store.schema(), vec!["epic", "jira", "status"]);
    }
    #[test]
    fn reload_dir_overwrites_only_that_slice() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        write(a.path(), "x.md", "---\nstatus: draft\n---\n");
        write(b.path(), "y.md", "---\nstatus: synced\n---\n");
        let (mut store, _) = InMemoryStore::load(
            vec![a.path().to_path_buf(), b.path().to_path_buf()], WalkOpts::default());
        assert_eq!(store.records().count(), 2);
        // add a file to A, reload only A
        write(a.path(), "z.md", "---\nstatus: draft\n---\n");
        let report = store.reload_dir(a.path());
        assert_eq!(report.loaded, 2);          // A now has 2
        assert_eq!(store.records().count(), 3); // A(2) + B(1) unchanged
    }
    #[test]
    fn invalid_yaml_is_skipped_with_warning() {
        let td = TempDir::new().unwrap();
        write(td.path(), "bad.md", "---\n: : broken\n  x\n---\n");
        let (_store, report) = InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        assert_eq!(report.skipped, 1);
        assert_eq!(report.warnings.len(), 1);
    }
}
```

- [ ] **Step 2: Run to verify fail** — `cargo test --lib store` → FAIL.
- [ ] **Step 3: Implement `RecordStore`, `InMemoryStore`, `DirSlice`, `LoadReport`.** `scanned_at` via `SystemTime::now()` (allowed in normal runtime code — only workflow *scripts* forbid it). `schema()`: collect field names into a `BTreeSet<String>` across records, return as `Vec`.
- [ ] **Step 4: Run to verify pass** — `cargo test --lib store` → PASS; fmt + clippy clean.
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat: directory-keyed RecordStore with reload_dir overwrite

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 6: `query::ast` + `query::parse`

**Files:**
- Create: `src/query/mod.rs`, `src/query/ast.rs`, `src/query/parse.rs`; Modify: `src/main.rs` (`mod query;`)
- Test: inline in `src/query/parse.rs`

**Interfaces:**
- Consumes: `model::FileAttr` (Task 2).
- Produces (in `ast.rs`, all `#[derive(Debug, Clone, PartialEq)]`):
```rust
pub struct Query {
    pub select: Vec<SelectItem>,
    pub from_glob: Option<String>,
    pub filter: Option<Predicate>,
    pub group_by: Vec<ColRef>,
    pub order_by: Vec<OrderKey>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}
pub struct SelectItem { pub expr: SelectExpr, pub alias: Option<String> }
pub enum SelectExpr { Star, Col(ColRef), Agg(Aggregate) }
pub enum ColRef { Field(String), File(crate::model::FileAttr) }
pub enum Aggregate {
    CountStar, Count(ColRef, /*distinct*/ bool),
    Min(ColRef), Max(ColRef), Sum(ColRef), Avg(ColRef), GroupConcat(ColRef),
}
pub enum Predicate {
    Compare(ColRef, CmpOp, Literal),
    Like(ColRef, String, /*negated*/ bool),
    In(ColRef, Vec<Literal>, /*negated*/ bool),
    IsNull(ColRef, /*negated*/ bool),
    And(Box<Predicate>, Box<Predicate>),
    Or(Box<Predicate>, Box<Predicate>),
    Not(Box<Predicate>),
}
pub enum CmpOp { Eq, Ne, Lt, Le, Gt, Ge }
pub enum Literal { Str(String), Int(i64), Float(f64), Bool(bool), Null }
pub struct OrderKey { pub target: OrderTarget, pub desc: bool }
pub enum OrderTarget { Alias(String), Col(ColRef) }

impl SelectItem { pub fn header(&self) -> String { /* alias, else default from expr */ } }
```
- Produces (in `parse.rs`):
  - `#[derive(Debug, thiserror::Error)] pub enum ParseError { … }` with variants like `Sql(String)`, `Unsupported(String)`, `BadColumn(String)`.
  - `pub fn parse(sql: &str) -> Result<Query, ParseError>`

`header()`: `alias` if present; else `Field(n)` → `n`; `File(attr)` → `file.name`/`file.path`/`file.folder`/`file.ext`; `Star` → `*`; aggregates → SQL-ish text (`count(*)`, `count(status)`, `min(prd)`, `group_concat(jira)`).

**FROM preprocessing** (spec §3): before handing SQL to `sqlparser`, extract an optional `FROM '<glob>'` / `FROM "<glob>"` clause with a regex and strip it, capturing the glob:
```rust
// (?i) FROM followed by a quoted string; only the quoted-glob form is stripped.
// Bare-identifier FROM is left for sqlparser and read from the AST table name.
static FROM_GLOB: Lazy<Regex> = ...; // r#"(?i)\bfrom\s+('([^']*)'|"([^"]*)")"#
```
Then parse the remaining SQL with `sqlparser::parser::Parser::parse_sql(&GenericDialect, rest)`; expect exactly one `Statement::Query` whose body is a `Select`. sqlparser accepts a `SELECT` with no `FROM` and a `WHERE`/`GROUP BY`, so no synthetic table is needed. If the AST `from` is non-empty, take the first table's name as `from_glob` (only when the regex didn't already capture one). Reject (`ParseError::Unsupported`) any JOIN, subquery, `HAVING`, `DISTINCT` on the whole select, set operations, or multiple statements.

- [ ] **Step 1: Dependencies**
```bash
cargo add sqlparser regex once_cell
```

- [ ] **Step 2: Write the failing tests** (representative — cover every clause)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::ast::*;
    use crate::model::FileAttr;

    #[test]
    fn select_fields_with_alias_no_from() {
        let q = parse("SELECT status, count(*) AS Count GROUP BY status").unwrap();
        assert_eq!(q.select[0], SelectItem { expr: SelectExpr::Col(ColRef::Field("status".into())), alias: None });
        assert_eq!(q.select[1], SelectItem { expr: SelectExpr::Agg(Aggregate::CountStar), alias: Some("Count".into()) });
        assert_eq!(q.group_by, vec![ColRef::Field("status".into())]);
        assert_eq!(q.from_glob, None);
    }
    #[test]
    fn file_pseudo_columns() {
        let q = parse("SELECT file.name, file.folder WHERE file.ext = 'md'").unwrap();
        assert_eq!(q.select[0].expr, SelectExpr::Col(ColRef::File(FileAttr::Name)));
        assert_eq!(q.select[1].expr, SelectExpr::Col(ColRef::File(FileAttr::Folder)));
        match q.filter.unwrap() {
            Predicate::Compare(ColRef::File(FileAttr::Ext), CmpOp::Eq, Literal::Str(s)) => assert_eq!(s, "md"),
            p => panic!("unexpected {p:?}"),
        }
    }
    #[test]
    fn where_ops_and_boolean() {
        let q = parse("SELECT jira WHERE prd = '010' AND (status = 'draft' OR status = 'synced')").unwrap();
        assert!(matches!(q.filter, Some(Predicate::And(_, _))));
    }
    #[test]
    fn in_like_isnull() {
        assert!(parse("SELECT jira WHERE status IN ('a','b')").is_ok());
        assert!(parse("SELECT jira WHERE slice LIKE 'mobile%'").is_ok());
        assert!(parse("SELECT jira WHERE epic IS NOT NULL").is_ok());
    }
    #[test]
    fn order_and_limit() {
        let q = parse("SELECT status, count(*) AS n GROUP BY status ORDER BY n DESC LIMIT 5 OFFSET 2").unwrap();
        assert_eq!(q.order_by, vec![OrderKey { target: OrderTarget::Alias("n".into()), desc: true }]);
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.offset, Some(2));
    }
    #[test]
    fn from_quoted_glob_is_stripped() {
        let q = parse("SELECT jira FROM 'plans/**' WHERE status = 'draft'").unwrap();
        assert_eq!(q.from_glob.as_deref(), Some("plans/**"));
        assert!(matches!(q.filter, Some(Predicate::Compare(..))));
    }
    #[test]
    fn from_bare_ident() {
        let q = parse("SELECT jira FROM plans").unwrap();
        assert_eq!(q.from_glob.as_deref(), Some("plans"));
    }
    #[test]
    fn star_select() {
        let q = parse("SELECT *").unwrap();
        assert_eq!(q.select[0].expr, SelectExpr::Star);
    }
    #[test]
    fn aggregates_all_kinds() {
        assert!(parse("SELECT min(prd), max(prd), sum(prd), avg(prd), group_concat(jira), count(distinct status) GROUP BY epic").is_ok());
    }
    #[test]
    fn unsupported_join_errors() {
        assert!(matches!(parse("SELECT a FROM x JOIN y ON x.i=y.i"), Err(ParseError::Unsupported(_))));
    }
    #[test]
    fn garbage_errors() {
        assert!(parse("SELCT nonsense").is_err());
    }
}
```

- [ ] **Step 3: Run to verify fail** — `cargo test --lib query::parse` → FAIL.
- [ ] **Step 4: Implement `ast.rs` then `parse.rs`.** Lower `sqlparser` nodes: projection items → `SelectItem` (`Expr::Identifier`/`CompoundIdentifier` → `ColRef`; `expr.value == "file"` compound → `File(FileAttr)`; `Function` named count/min/max/sum/avg/group_concat → `Aggregate`; `Wildcard` → `Star`; `ExprWithAlias`/projection alias → `alias`). WHERE `Expr` → `Predicate` (BinaryOp comparisons/AND/OR, `Like`, `InList`, `IsNull`/`IsNotNull`, `Nested`, `UnaryOp::Not`). GROUP BY exprs → `ColRef`. ORDER BY → resolve name to a SELECT alias if it matches one, else `ColRef`. Map `sqlparser` value literals → `Literal`. Any node kind you do not translate → `ParseError::Unsupported(describe)`.
- [ ] **Step 5: Run to verify pass** — `cargo test --lib query::parse` → PASS; fmt + clippy clean.
- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "feat: SQL-subset parser (sqlparser) to query AST

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 7: `query::exec` — filter / project / order / limit (non-grouped)

**Files:**
- Create: `src/query/exec.rs`; Modify: `src/query/mod.rs` (define `ResultTable`, re-export)
- Test: inline in `src/query/exec.rs`

**Interfaces:**
- Consumes: `ast::*` (Task 6), `Record`/`Value`/`compare_values` (Tasks 1-2).
- Produces:
  - `pub struct ResultTable { pub headers: Vec<String>, pub rows: Vec<Vec<crate::model::Value>> }` (in `query/mod.rs`)
  - `#[derive(Debug, thiserror::Error)] pub enum ExecError { … }` (e.g. `NonGroupedColumn(String)`, `UnknownAlias(String)`)
  - `pub fn execute<'a>(q: &Query, records: impl Iterator<Item = &'a Record>) -> Result<ResultTable, ExecError>`

This task handles queries **without** `GROUP BY` and **without** aggregates (aggregates come in Task 8; if `q.group_by` is empty AND no select item is an aggregate, use this path). Steps: collect records to a `Vec<&Record>`; apply `from_glob` (if set, keep records whose `file.path` matches the glob); apply `WHERE` (`eval_predicate(record, pred) -> bool`); project each surviving record to a row by evaluating each `SelectExpr` (`Star` expands to the sorted union of the *result set's* field names — compute headers accordingly); apply `ORDER BY` (resolve `OrderTarget::Alias` to the projected column index, `Col` to a fresh field lookup; NULLs last regardless of `desc`); apply `OFFSET`/`LIMIT`.

`eval_predicate` comparison rule (spec §4): for `Compare(col, op, lit)` fetch the field/file `Value`; if `lit` is `Literal::Str`, compare `value.to_cmp_string()` against the string; if numeric literal, compare `value.as_number()` numerically (both must be numeric else the row fails the predicate); `Null` value → predicate is false (except handled by `IsNull`). `Like` translates `%`→`.*`, `_`→`.` on the escaped pattern (case-sensitive), matches `value.to_cmp_string()`. `In` = OR of equality with the given literals. `IsNull(col, negated)` tests `value.is_null()`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Record, Value, FileAttr};
    use crate::query::parse::parse;
    use indexmap::IndexMap;
    use std::path::Path;

    fn rec(root: &str, path: &str, kv: &[(&str, Value)]) -> Record {
        let mut m = IndexMap::new();
        for (k, v) in kv { m.insert((*k).to_string(), v.clone()); }
        Record::new(Path::new(root), Path::new(path), m)
    }
    fn recs() -> Vec<Record> {
        vec![
            rec("s", "s/plans/a.md", &[("status", Value::Str("draft".into())), ("prd", Value::Str("010".into()))]),
            rec("s", "s/plans/b.md", &[("status", Value::Str("synced".into())), ("prd", Value::Str("010".into()))]),
            rec("s", "s/product/c.md", &[("status", Value::Str("synced".into())), ("prd", Value::Str("011".into()))]),
        ]
    }

    #[test]
    fn filter_and_project_with_alias() {
        let q = parse("SELECT status AS S, file.name WHERE prd = '010'").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.headers, vec!["S", "file.name"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0], vec![Value::Str("draft".into()), Value::Str("a.md".into())]);
    }
    #[test]
    fn order_desc_and_limit() {
        let q = parse("SELECT status WHERE prd = '010' ORDER BY status DESC LIMIT 1").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("synced".into())]]);
    }
    #[test]
    fn star_expands_sorted_union() {
        let q = parse("SELECT * WHERE status = 'draft'").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.headers, vec!["prd", "status"]);
    }
    #[test]
    fn from_glob_filters_by_path() {
        let q = parse("SELECT file.name FROM 'plans/**'").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows.len(), 2);
    }
    #[test]
    fn like_and_in() {
        let q = parse("SELECT status WHERE status LIKE 'syn%'").unwrap();
        assert_eq!(execute(&q, recs().iter()).unwrap().rows.len(), 2);
        let q2 = parse("SELECT status WHERE prd IN ('011')").unwrap();
        assert_eq!(execute(&q2, recs().iter()).unwrap().rows.len(), 1);
    }
}
```

- [ ] **Step 2: Run to verify fail** — `cargo test --lib query::exec` → FAIL.
- [ ] **Step 3: Implement `ResultTable`, `ExecError`, `execute` (non-grouped path).** Use `globset::Glob` for `from_glob` (match against `file.path` string). Put NULLs last in ordering by treating `compare_values` `None`-with-null as "null is greater".
- [ ] **Step 4: Run to verify pass** — `cargo test --lib query::exec` → PASS; fmt + clippy clean.
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat: query executor for filter/project/order/limit

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 8: `query::exec` — GROUP BY + aggregates

**Files:**
- Modify: `src/query/exec.rs`
- Test: inline in `src/query/exec.rs`

**Interfaces:**
- Consumes: everything from Task 7.
- Produces: `execute` now also handles queries with `GROUP BY` and/or aggregate select items (same signature — dispatch internally).

Dispatch: if `q.group_by` is non-empty OR any select item is `SelectExpr::Agg`, use the aggregate path. Group rows by the tuple of `group_by` column `Value`s (after WHERE + from_glob filtering). With aggregates but no `GROUP BY`, treat all rows as a single group. **Validation:** every non-aggregate select item must be a `ColRef` that appears in `group_by`, else `ExecError::NonGroupedColumn(header)`. Per group, project: grouping `ColRef` → its group-key value; `Aggregate` → computed value:
- `CountStar` → row count (`Value::Int`)
- `Count(col, false)` → count of non-null values; `Count(col, true)` → count of distinct non-null `to_cmp_string()` values
- `Sum`/`Avg` → over `as_number()` of non-null values (skip non-numeric); `Avg` `Null` if no numerics; results `Value::Float`
- `Min`/`Max` → via `compare_values` over non-null values (`Null` if none)
- `GroupConcat(col)` → non-null `display()` values joined `", "` (`Value::Str`)
Then apply `ORDER BY` (alias resolves to the projected aggregate/group column) and `LIMIT`/`OFFSET`.

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod agg_tests {
    use super::*;
    use crate::model::{Record, Value};
    use crate::query::parse::parse;
    use indexmap::IndexMap;
    use std::path::Path;

    fn rec(path: &str, status: &str, prd: &str) -> Record {
        let mut m = IndexMap::new();
        m.insert("status".into(), Value::Str(status.into()));
        m.insert("prd".into(), Value::Str(prd.into()));
        Record::new(Path::new("s"), Path::new(path), m)
    }
    fn recs() -> Vec<Record> {
        vec![rec("s/a.md","draft","010"), rec("s/b.md","synced","010"), rec("s/c.md","synced","011")]
    }

    #[test]
    fn count_per_status_renamed_ordered() {
        let q = parse("SELECT status, count(*) AS Count GROUP BY status ORDER BY Count DESC").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.headers, vec!["status", "Count"]);
        assert_eq!(t.rows, vec![
            vec![Value::Str("synced".into()), Value::Int(2)],
            vec![Value::Str("draft".into()),  Value::Int(1)],
        ]);
    }
    #[test]
    fn bare_count_star_single_group() {
        let q = parse("SELECT count(*) AS n").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(3)]]);
    }
    #[test]
    fn count_distinct() {
        let q = parse("SELECT count(distinct status) AS d").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(2)]]);
    }
    #[test]
    fn group_concat() {
        let q = parse("SELECT prd, group_concat(status) AS ss GROUP BY prd ORDER BY prd").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows[0], vec![Value::Str("010".into()), Value::Str("draft, synced".into())]);
    }
    #[test]
    fn non_grouped_column_errors() {
        let q = parse("SELECT status, prd, count(*) GROUP BY status").unwrap();
        assert!(matches!(execute(&q, recs().iter()), Err(ExecError::NonGroupedColumn(_))));
    }
}
```

- [ ] **Step 2: Run to verify fail** — `cargo test --lib query::exec` (agg_tests) → FAIL.
- [ ] **Step 3: Implement the aggregate path.** Preserve deterministic group ordering before `ORDER BY` (e.g. iterate groups in first-appearance order, or sort by key) so results without an explicit `ORDER BY` are stable — sort groups by their key tuple for determinism.
- [ ] **Step 4: Run to verify pass** — all `query::exec` tests PASS; fmt + clippy clean.
- [ ] **Step 5: Commit**
```bash
git add -A && git commit -m "feat: GROUP BY and aggregate functions in executor

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 9: `render` — output formats

**Files:**
- Create: `src/render.rs`; Modify: `src/main.rs` (`mod render;`)
- Test: inline in `src/render.rs` (use `insta` for table/md snapshots)

**Interfaces:**
- Consumes: `ResultTable` (Task 7), `Value` (Task 1).
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Format { Table, Json, Csv, Tsv, Md }`
  - `impl std::str::FromStr for Format` (accepts `table|json|csv|tsv|md`, case-insensitive; `markdown` alias for `md`).
  - `pub fn render(table: &ResultTable, format: Format) -> String`

Rules: `Table` and `Md` via `comfy-table` (Md uses the Markdown preset `comfy_table::presets::ASCII_MARKDOWN`). `Json` → array of objects keyed by header, values converted `Value`→`serde_json::Value` (`Null`→null, `Int`→number, `Float`→number, `Bool`→bool, `Str`→string, `List`→array). `Csv`/`Tsv` via the `csv` crate with a header row; cell text is `Value::display()` (so `Null` is empty). Trailing newline trimmed consistently (tests assert exact strings).

- [ ] **Step 1: Dependencies**
```bash
cargo add comfy-table serde_json csv
cargo add --dev insta
```

- [ ] **Step 2: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use crate::query::ResultTable;

    fn table() -> ResultTable {
        ResultTable {
            headers: vec!["status".into(), "Count".into()],
            rows: vec![
                vec![Value::Str("synced".into()), Value::Int(2)],
                vec![Value::Str("draft".into()), Value::Int(1)],
            ],
        }
    }
    #[test]
    fn json_roundtrips() {
        let s = render(&table(), Format::Json);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["status"], "synced");
        assert_eq!(v[0]["Count"], 2);
    }
    #[test]
    fn csv_has_header_and_rows() {
        let s = render(&table(), Format::Csv);
        assert_eq!(s.lines().next().unwrap(), "status,Count");
        assert!(s.contains("synced,2"));
    }
    #[test]
    fn tsv_uses_tabs() {
        let s = render(&table(), Format::Tsv);
        assert_eq!(s.lines().next().unwrap(), "status\tCount");
    }
    #[test]
    fn format_from_str() {
        assert_eq!("md".parse::<Format>().unwrap(), Format::Md);
        assert_eq!("markdown".parse::<Format>().unwrap(), Format::Md);
        assert!("bogus".parse::<Format>().is_err());
    }
    #[test]
    fn table_snapshot() { insta::assert_snapshot!(render(&table(), Format::Table)); }
    #[test]
    fn md_snapshot() { insta::assert_snapshot!(render(&table(), Format::Md)); }
}
```

- [ ] **Step 3: Run to verify fail** — `cargo test --lib render` → FAIL.
- [ ] **Step 4: Implement `Format` + `render`.** For snapshot tests, run `cargo insta test --accept` (or `INSTA_UPDATE=always cargo test --lib render`) once to record the initial snapshots, then eyeball the `.snap` files for sanity before committing.
- [ ] **Step 5: Run to verify pass** — `cargo test --lib render` → PASS; fmt + clippy clean. Commit the `.snap` files too.
- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "feat: render ResultTable to table/json/csv/tsv/markdown

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 10: `session` + `cli` + `main` — one-shot & batch dispatch

**Files:**
- Create: `src/session.rs`, `src/cli.rs`; Modify: `src/main.rs`
- Test: inline in `src/session.rs`; integration `tests/cli.rs` (assert_cmd)

**Interfaces:**
- Consumes: `InMemoryStore`/`RecordStore` (Task 5), `parse` (Task 6), `execute` (Task 8), `render`/`Format` (Task 9), `WalkOpts` (Task 4).
- Produces:
  - `src/session.rs`: `pub struct Session { store: Box<dyn RecordStore>, pub format: Format }` with `pub fn new(store: Box<dyn RecordStore>, format: Format) -> Self; pub fn run(&self, sql: &str) -> anyhow::Result<ResultTable>; pub fn render_query(&self, sql: &str) -> anyhow::Result<String>; pub fn set_format(&mut self, f: Format); pub fn reload(&mut self) -> LoadReport; pub fn schema(&self) -> Vec<String>;`
  - `pub fn split_statements(input: &str) -> Vec<String>` — split on top-level `;`, trimming; ignore empties. (Shared by batch mode and REPL.)
  - `src/cli.rs`: `#[derive(clap::Parser)] pub struct Cli { pub dirs: Vec<PathBuf>, pub query: Option<String> /* -e/--query */, pub format: Format /* --format, default Table */, pub ext: ... , pub respect_gitignore: bool, pub hidden: bool, pub exclude: Vec<String> }` plus `impl Cli { pub fn walk_opts(&self) -> WalkOpts; pub fn roots(&self) -> Vec<PathBuf>; /* dirs or [cwd] */ }`

`main` flow: parse `Cli`; build roots (dirs or cwd); `InMemoryStore::load` → print the `LoadReport` warnings to **stderr**; build `Session`. Mode: if `cli.query == Some("-")` → read query text from stdin; if `Some(sql)` → run each `split_statements` statement, print `render` to stdout, exit non-zero on first error. Else if stdin is **not** a TTY (`std::io::stdin().is_terminal() == false`) → batch: read all stdin, run each statement. Else → call `repl::run(session)` (Task 11).

- [ ] **Step 1: Dependencies**
```bash
cargo add clap --features derive
cargo add --dev assert_cmd predicates
```

- [ ] **Step 2: Write the failing tests**

`src/session.rs` unit tests:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn split_statements_basic() {
        assert_eq!(split_statements(" SELECT 1 ; SELECT 2 ;"), vec!["SELECT 1", "SELECT 2"]);
        assert_eq!(split_statements("SELECT 1"), vec!["SELECT 1"]);
        assert_eq!(split_statements("  ;; "), Vec::<String>::new());
    }
}
```

`tests/cli.rs` integration (build a temp tree, run the binary):
```rust
use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;

fn tree() -> TempDir {
    let td = TempDir::new().unwrap();
    for (p, s) in [
        ("plans/a.md", "---\nstatus: draft\nprd: '010'\n---\n"),
        ("plans/b.md", "---\nstatus: synced\nprd: '010'\n---\n"),
        ("product/c.md", "---\nstatus: synced\nprd: '011'\n---\n"),
    ] {
        let f = td.path().join(p);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, s).unwrap();
    }
    td
}

#[test]
fn oneshot_group_count_table() {
    let td = tree();
    Command::cargo_bin("querymatter").unwrap()
        .arg("-e").arg("SELECT status, count(*) AS Count GROUP BY status ORDER BY Count DESC")
        .arg(td.path())
        .assert().success()
        .stdout(predicates::str::contains("Count"))
        .stdout(predicates::str::contains("synced"));
}
#[test]
fn oneshot_json_is_clean_stdout() {
    let td = tree();
    let out = Command::cargo_bin("querymatter").unwrap()
        .args(["-e", "SELECT status WHERE prd = '010'", "--format", "json"])
        .arg(td.path())
        .assert().success().get_output().stdout.clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap(); // stdout must be pure JSON
    assert_eq!(v.as_array().unwrap().len(), 2);
}
#[test]
fn batch_mode_from_stdin() {
    let td = tree();
    Command::cargo_bin("querymatter").unwrap()
        .arg(td.path())
        .write_stdin("SELECT count(*) AS n;\n")
        .assert().success()
        .stdout(predicates::str::contains("n"));
}
#[test]
fn query_error_exits_nonzero() {
    let td = tree();
    Command::cargo_bin("querymatter").unwrap()
        .args(["-e", "SELCT bad"]).arg(td.path())
        .assert().failure();
}
```
(Integration test needs `serde_json` available as a dev-dep too: `cargo add --dev serde_json`.)

- [ ] **Step 3: Run to verify fail** — `cargo test` → FAIL (binary/args not implemented).
- [ ] **Step 4: Implement `cli.rs`, `session.rs`, and `main.rs` dispatch.** Use `std::io::IsTerminal`. Keep all diagnostics on stderr.
- [ ] **Step 5: Run to verify pass** — `cargo test` → PASS; fmt + clippy clean.
- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "feat: CLI, session, and one-shot/batch dispatch

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 11: `repl` — rustyline loop + dot-commands

**Files:**
- Create: `src/repl.rs`; Modify: `src/main.rs` (`mod repl;`)
- Test: inline in `src/repl.rs` (test the pure line-processing core, not rustyline IO)

**Interfaces:**
- Consumes: `Session` (Task 10), `split_statements` (Task 10), `Format` (Task 9).
- Produces:
  - A testable enum + function separating parsing from IO:
    ```rust
    pub enum Line { Blank, More, Statement(String), Dot(DotCommand) }
    pub enum DotCommand { Help, Schema, Format(Option<Format>), Reload, Quit, Unknown(String) }
    pub struct LineBuffer { buf: String }
    impl LineBuffer { pub fn new() -> Self; pub fn push(&mut self, raw: &str) -> Line; }
    // push returns Statement when the accumulated buffer ends with ';' (clearing it),
    // More when mid-statement, Dot for a line beginning with '.', Blank for empty.
    pub fn parse_dot(line: &str) -> DotCommand;
    ```
  - `pub fn run(session: Session) -> anyhow::Result<()>` — the rustyline driver: prompt `querymatter> ` / continuation `   ...> `, history file from `directories::ProjectDirs::from("", "", "querymatter")` under the state/data dir, feeding lines to `LineBuffer`, executing statements via `session.render_query`, handling dot-commands, printing query errors to stderr and continuing, exiting on `.quit`/`.exit`/EOF.

- [ ] **Step 1: Dependencies**
```bash
cargo add rustyline directories
```

- [ ] **Step 2: Write the failing tests** (pure core only)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Format;

    #[test]
    fn buffers_until_semicolon() {
        let mut b = LineBuffer::new();
        assert!(matches!(b.push("SELECT status"), Line::More));
        assert!(matches!(b.push("FROM 'x' ;"), Line::Statement(_)));
    }
    #[test]
    fn single_line_statement() {
        let mut b = LineBuffer::new();
        match b.push("SELECT 1;") { Line::Statement(s) => assert_eq!(s, "SELECT 1"), _ => panic!() }
    }
    #[test]
    fn blank_line_is_blank() {
        let mut b = LineBuffer::new();
        assert!(matches!(b.push("   "), Line::Blank));
    }
    #[test]
    fn dot_commands_parse() {
        assert!(matches!(parse_dot(".help"), DotCommand::Help));
        assert!(matches!(parse_dot(".schema"), DotCommand::Schema));
        assert!(matches!(parse_dot(".reload"), DotCommand::Reload));
        assert!(matches!(parse_dot(".quit"), DotCommand::Quit));
        assert!(matches!(parse_dot(".exit"), DotCommand::Quit));
        assert!(matches!(parse_dot(".format json"), DotCommand::Format(Some(Format::Json))));
        assert!(matches!(parse_dot(".format"), DotCommand::Format(None)));
        assert!(matches!(parse_dot(".bogus"), DotCommand::Unknown(_)));
    }
    #[test]
    fn dot_line_detected_by_buffer() {
        let mut b = LineBuffer::new();
        assert!(matches!(b.push(".schema"), Line::Dot(DotCommand::Schema)));
    }
}
```

- [ ] **Step 3: Run to verify fail** — `cargo test --lib repl` → FAIL.
- [ ] **Step 4: Implement `LineBuffer`, `parse_dot`, `DotCommand`, `Line`, and the `run` driver.** A line starting with `.` (when the buffer is empty) is a dot-command; otherwise accumulate and split on trailing `;`. `.format` with no arg prints the current format (driver concern). Wire history load/save around the loop; ignore history IO errors (warn to stderr at most).
- [ ] **Step 5: Run to verify pass** — `cargo test --lib repl` → PASS; fmt + clippy clean. Manual smoke: `printf 'SELECT count(*) AS n;\n.quit\n' | cargo run -- samples` (batch path) and an interactive `cargo run -- samples` sanity check.
- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "feat: interactive REPL with dot-commands and history

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

### Task 12: Integration over sample fixtures + README

**Files:**
- Create: `tests/fixtures/` (committed sample tree), extend `tests/cli.rs`, create `README.md`
- Modify: `Cargo.toml` (metadata: description, categories/keywords)

**Interfaces:** none new — exercises the built binary end-to-end.

Copy a curated fixture tree into `tests/fixtures/` (mirroring `samples/`: `plans/`, `product/stories/`, `templates/`) so tests do not depend on the gitignored `samples/`. Fixtures **are** committed.

- [ ] **Step 1: Create the committed fixtures**
```bash
mkdir -p /home/steve/src/hub-reader/tests/fixtures/plans \
         /home/steve/src/hub-reader/tests/fixtures/product/stories \
         /home/steve/src/hub-reader/tests/fixtures/templates
```
Populate with small deterministic files, e.g. `tests/fixtures/plans/DCP-459.md`:
```markdown
---
jira: DCP-459
prd: '010'
epic: DCP-458
status: draft
slice: mobile portion of work
---
```
and analogous `DCP-461.md` (`status: synced`), a `product/stories/DCP-459.md` (`status: synced`), and a `templates/bug-template.md` with the placeholder frontmatter. Keep values fixed so assertions are stable.

- [ ] **Step 2: Write the failing integration tests** (append to `tests/cli.rs`)

```rust
const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

#[test]
fn headline_status_counts() {
    let out = assert_cmd::Command::cargo_bin("querymatter").unwrap()
        .args(["-e", "SELECT status, count(*) AS Count WHERE prd = '010' GROUP BY status ORDER BY Count DESC", "--format", "csv"])
        .arg(format!("{FIX}/plans")).arg(format!("{FIX}/product"))
        .assert().success().get_output().stdout.clone();
    let s = String::from_utf8(out).unwrap();
    assert_eq!(s.lines().next().unwrap(), "status,Count");
    // prd '010': draft x1 (plans/DCP-459), synced x1 (plans/DCP-461); product story is prd 010 synced too
    assert!(s.contains("synced,"));
    assert!(s.contains("draft,1"));
}
#[test]
fn group_by_file_folder() {
    let out = assert_cmd::Command::cargo_bin("querymatter").unwrap()
        .args(["-e", "SELECT file.folder, count(*) AS n GROUP BY file.folder", "--format", "json"])
        .arg(FIX)
        .assert().success().get_output().stdout.clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert!(v.as_array().unwrap().len() >= 2); // plans, product/stories, templates
}
#[test]
fn exclude_templates() {
    let out = assert_cmd::Command::cargo_bin("querymatter").unwrap()
        .args(["-e", "SELECT count(*) AS n", "--exclude", "**/templates/**", "--format", "csv"])
        .arg(FIX)
        .assert().success().get_output().stdout.clone();
    let s = String::from_utf8(out).unwrap();
    // 3 real docs, templates excluded
    assert!(s.contains("n\n3") || s.trim().ends_with("3"));
}
```
Adjust the exact counts to match the fixtures you created (run once, read the output, lock the assertions).

- [ ] **Step 3: Run to verify fail then implement/adjust** — create fixtures, run `cargo test --test cli`, tune fixture contents/assertions until green. No production code should need changing; if it does, that is a real gap — fix it.
- [ ] **Step 4: Write `README.md`** — purpose, install (`cargo install --path .`), the mode truth table, the query DSL surface with the headline example, `file.*` columns, flags (`--format`, `--exclude`, `--respect-gitignore`, `--hidden`, `--ext`), REPL dot-commands, and a "Design & roadmap" pointer to the spec and `TODO.md` (TTL cache). Add `description`/`keywords` to `Cargo.toml`.
- [ ] **Step 5: Run full suite** — `cargo test`, `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings` all clean.
- [ ] **Step 6: Commit**
```bash
git add -A && git commit -m "test: end-to-end integration over fixtures; add README

Claude-Session: https://claude.ai/code/session_01MENQxRJ1UKsdgk48MmHxx6"
```

---

## Self-Review

**Spec coverage:**
- §2 CLI (dirs positional, `-e/--query`, `--format`, `--ext`, `--respect-gitignore`, `--hidden`, `--exclude`) → Task 10 (+ Task 4 for walk opts). ✅
- §2 mode truth table (REPL / one-shot / stdin `-` / batch) → Task 10 dispatch + Task 11 REPL. ✅
- §3 DSL (SELECT/alias, FROM glob, WHERE ops, GROUP BY, aggregates, ORDER BY, LIMIT/OFFSET, `*`, `file.*`) → Tasks 6 (parse) + 7-8 (exec). ✅
- §3 FROM-less + FROM-glob normalization → Task 6 (regex strip + sqlparser no-FROM). ✅
- §4 data model (Value, missing→Null, file.* pseudo, coercion, aggregate NULL rules) → Tasks 1-2, 7-8. ✅
- §6 modules/pipeline → Tasks 1-12 map 1:1 to modules. ✅
- §7 error handling (anyhow boundary, thiserror query errors, per-file warn+skip) → Tasks 3/5/6/7/10. ✅
- §8.1 gitignore-off default → Task 4 tests. §8.2 templates → Task 12 exclude test. §8.3 leading-zero characterization + quoted-string → Task 3 tests. §8.4 no-frontmatter skip → Tasks 3/5. ✅
- §9 TTL-cache seams (dir-keyed slices, RecordStore trait, reload_dir, scanned_at, single root resolution) → Task 5 (+ Task 10 roots()). ✅
- §10 invariants (coercion producers, file.* resolution, stdout cleanliness) → Tasks 1/3/7 unit + Task 10/12 integration (`oneshot_json_is_clean_stdout`, `group_by_file_folder`). ✅
- §11 testing strategy → each task is TDD; integration in Tasks 10/12. ✅
- §12 crates → introduced across tasks with `cargo add`. ✅

**Placeholder scan:** The two "run once, then lock the assertion" steps (leading-zero characterization in Task 3; fixture counts in Task 12) are deliberate characterization-test methodology, not vague placeholders — each gives the full test body and a concrete tightening instruction. No `TBD`/`implement later`.

**Type consistency:** `Value`, `FileAttr`, `Record`, `Extract`, `WalkOpts`, `discover`, `RecordStore`/`InMemoryStore`/`DirSlice`/`LoadReport`, `Query`/`ColRef`/`Aggregate`/`Predicate`/`Literal`, `ResultTable`, `Format`, `Session`/`split_statements`, `LineBuffer`/`DotCommand` are used consistently across the tasks that consume them (checked against each Interfaces block).
