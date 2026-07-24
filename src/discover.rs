//! Recursively finds Markdown files under a root directory.
//!
//! Wraps [`ignore::WalkBuilder`] with gitignore/hidden-file handling that
//! defaults to "everything" — this is a query tool, not a Git tool, so a
//! stray `.gitignore` shouldn't silently hide content from queries unless
//! the caller opts in via [`WalkOpts::respect_gitignore`].

use std::path::{Path, PathBuf};

use anyhow::Context;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

/// Options controlling how [`discover`] walks a directory tree.
#[derive(Debug, Clone)]
pub struct WalkOpts {
    /// File extensions (without the leading `.`) to include.
    pub exts: Vec<String>,
    /// When `true`, honor `.gitignore`/`.ignore`/global git-ignore rules.
    /// Off by default: frontmatter queries should see gitignored content
    /// unless the caller explicitly asks to respect it.
    pub respect_gitignore: bool,
    /// When `true`, descend into hidden directories/files (dotfiles).
    pub hidden: bool,
    /// Glob patterns (matched against both the absolute path and the path
    /// relative to `root`); any match excludes the file from the result.
    pub excludes: Vec<String>,
    /// Gitignore-style ignore files to apply (earliest first), always honored
    /// regardless of `respect_gitignore`. Resolved by `Cli::ignore_files`.
    pub ignore_files: Vec<PathBuf>,
}

impl Default for WalkOpts {
    fn default() -> Self {
        WalkOpts {
            exts: vec!["md".to_string(), "markdown".to_string()],
            respect_gitignore: false,
            hidden: false,
            excludes: Vec::new(),
            ignore_files: Vec::new(),
        }
    }
}

/// Recursively finds files under `root` whose extension is in
/// `opts.exts` and which aren't matched by any of `opts.excludes`.
///
/// Results are sorted for deterministic output.
pub fn discover(root: &Path, opts: &WalkOpts) -> Vec<PathBuf> {
    let excludes = build_exclude_set(&opts.excludes);

    let mut walker = WalkBuilder::new(root);
    // Set every filtering toggle explicitly rather than relying on
    // `standard_filters`'s own default (which is "on"): gitignore/git-exclude/
    // global-gitignore/`.ignore`/parent-directory rules only apply when the
    // caller opts in via `respect_gitignore`, while hidden entries are
    // skipped unless `opts.hidden` is set. `standard_filters(false)` first
    // clears the builder's defaults so these are the only filters in effect.
    //
    // `require_git(false)` is unconditional: the `ignore` crate otherwise
    // only applies git-related ignore rules inside an actual git working
    // tree (i.e. one with a `.git` directory somewhere above `root`). A
    // caller opting into `respect_gitignore` expects their ignore files
    // honored wherever they live, not silently only inside a real checkout.
    // This is harmless when `respect_gitignore` is false, since
    // `git_ignore(false)` etc. already disable gitignore processing
    // entirely regardless of `require_git`.
    walker
        .standard_filters(false)
        .git_ignore(opts.respect_gitignore)
        .git_exclude(opts.respect_gitignore)
        .git_global(opts.respect_gitignore)
        .ignore(opts.respect_gitignore)
        .parents(opts.respect_gitignore)
        .hidden(!opts.hidden)
        .require_git(false);

    for ignore_path in &opts.ignore_files {
        // add_ignore applies a gitignore-style file as an explicit source,
        // honored even with standard_filters(false)/ignore(false) (verified).
        // Paths are pre-validated at the CLI boundary, so a load error here is
        // not expected; ignore the returned Option<Error> rather than aborting
        // discovery of everything else.
        walker.add_ignore(ignore_path);
    }

    let mut found: Vec<PathBuf> = walker
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_file()))
        .map(ignore::DirEntry::into_path)
        .filter(|path| has_wanted_extension(path, &opts.exts))
        .filter(|path| !is_excluded(path, root, &excludes))
        .collect();

    found.sort();
    found
}

/// True when `path`'s extension (without the leading `.`, compared
/// case-insensitively so `README.MD` matches `exts: ["md"]`) appears in
/// `exts`.
fn has_wanted_extension(path: &Path, exts: &[String]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| exts.iter().any(|wanted| wanted.eq_ignore_ascii_case(ext)))
}

/// True when `path` (either as given, which is absolute since it was
/// yielded by a walk rooted at an absolute-ified `root`, or relative to
/// `root`) matches any glob in `excludes`.
fn is_excluded(path: &Path, root: &Path, excludes: &GlobSet) -> bool {
    if excludes.is_empty() {
        return false;
    }
    if excludes.is_match(path) {
        return true;
    }
    path.strip_prefix(root)
        .is_ok_and(|relative| excludes.is_match(relative))
}

/// Rejects any exclude glob `globset` cannot compile, naming the offending
/// pattern.
///
/// [`build_exclude_set`] below has no error channel back to its caller and
/// silently drops what doesn't compile, so every producer of an exclude list
/// — `config::set` at config-write time, and query/init's resolved
/// [`crate::settings::Settings::exclude`] at run time, which also catches a
/// hand-edited config file that bypassed `config set` — validates through
/// this one function up front instead.
pub fn validate_excludes(patterns: &[String]) -> anyhow::Result<()> {
    for pattern in patterns {
        Glob::new(pattern).with_context(|| format!("invalid exclude glob {pattern:?}"))?;
    }
    Ok(())
}

/// Compiles `patterns` into a [`GlobSet`], silently dropping any pattern
/// that fails to parse as a glob (there's no error channel back to the
/// caller in `discover`'s signature, and an unusable exclude shouldn't
/// abort discovery of everything else).
fn build_exclude_set(patterns: &[String]) -> GlobSet {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        if let Ok(glob) = Glob::new(pattern) {
            builder.add(glob);
        }
    }
    builder.build().unwrap_or_else(|_| GlobSet::empty())
}

/// The result of [`explain`]: whether `target` would be discovered under a
/// given [`WalkOpts`], and — when not — which filter layer excluded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Explanation {
    /// `target` is discovered.
    Included,
    /// `target` is excluded; the string names the responsible filter layer.
    Excluded(String),
}

/// Explains whether `target` — already canonicalized, and a descendant of
/// `root` — would be discovered by [`discover`] under `opts`.
///
/// The verdict is ground truth: it is exactly whether `discover(root, opts)`
/// contains `target`, so it can never disagree with a real query run. When
/// excluded, the reason is attributed by relaxing one filtering layer at a
/// time — in the order discovery applies them (extension, hidden, `--exclude`
/// globs, then ignore files) — and re-checking membership: the first
/// relaxation that flips the verdict names the responsible layer. Each
/// candidate reason is only reported after confirming it actually would have
/// changed the outcome, so the reason can never contradict the verdict. When
/// no single-layer relaxation isolates it (several layers exclude it
/// independently), the generic "excluded by the ignore rules" is reported
/// rather than guessing.
pub fn explain(root: &Path, target: &Path, opts: &WalkOpts) -> Explanation {
    if contains(&discover(root, opts), target) {
        return Explanation::Included;
    }

    if let Some(reason) = extension_reason(target, opts) {
        return Explanation::Excluded(reason);
    }

    if !opts.hidden {
        let relaxed = WalkOpts {
            hidden: true,
            ..opts.clone()
        };
        if contains(&discover(root, &relaxed), target) {
            return Explanation::Excluded(hidden_reason(root, target));
        }
    }

    if !opts.excludes.is_empty() {
        let relaxed = WalkOpts {
            excludes: Vec::new(),
            ..opts.clone()
        };
        if contains(&discover(root, &relaxed), target) {
            return Explanation::Excluded(exclude_reason(target, root, &opts.excludes));
        }
    }

    if !opts.ignore_files.is_empty() {
        let relaxed = WalkOpts {
            ignore_files: Vec::new(),
            ..opts.clone()
        };
        if contains(&discover(root, &relaxed), target) {
            return Explanation::Excluded(ignore_file_reason(&opts.ignore_files));
        }
    }

    if opts.respect_gitignore {
        let relaxed = WalkOpts {
            respect_gitignore: false,
            ..opts.clone()
        };
        if contains(&discover(root, &relaxed), target) {
            return Explanation::Excluded(
                "excluded by .gitignore rules (--respect-gitignore)".to_string(),
            );
        }
    }

    Explanation::Excluded("excluded by the ignore rules".to_string())
}

/// Whether `found` (as returned by [`discover`]) contains `target`.
fn contains(found: &[PathBuf], target: &Path) -> bool {
    found.iter().any(|path| path == target)
}

/// The extension-layer reason `target` is excluded, or `None` when its
/// extension is already in `opts.exts` (so some other layer is responsible).
fn extension_reason(target: &Path, opts: &WalkOpts) -> Option<String> {
    if has_wanted_extension(target, &opts.exts) {
        return None;
    }
    let wanted = opts.exts.join(", ");
    match target.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => Some(format!("extension '{ext}' not in --ext ({wanted})")),
        None => Some(format!("no file extension (want: {wanted})")),
    }
}

/// Names the first dot-prefixed path component between `root` and `target`
/// and whether it's `target` itself (a hidden file) or an ancestor directory.
fn hidden_component(root: &Path, target: &Path) -> Option<(String, bool)> {
    let relative = target.strip_prefix(root).ok()?;
    let names: Vec<&str> = relative
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect();
    let last = names.len().checked_sub(1)?;
    names.iter().enumerate().find_map(|(i, name)| {
        (name.starts_with('.') && *name != "." && *name != "..")
            .then(|| (name.to_string(), i == last))
    })
}

/// The hidden-layer reason `target` is excluded, once relaxing `opts.hidden`
/// has confirmed the layer is responsible.
fn hidden_reason(root: &Path, target: &Path) -> String {
    match hidden_component(root, target) {
        Some((name, true)) => format!("hidden file '{name}' (pass --hidden to include)"),
        Some((name, false)) => format!("hidden directory '{name}' (pass --hidden to include)"),
        None => "excluded by a hidden path component (pass --hidden to include)".to_string(),
    }
}

/// The `--exclude`-layer reason `target` is excluded, naming the first
/// pattern in `patterns` that matches it (either as given or relative to
/// `root`), once relaxing `opts.excludes` has confirmed the layer is
/// responsible. Falls back to a generic phrasing on the (surprising) case
/// that no individual pattern matches despite the layer as a whole being
/// responsible — e.g. an unparsable pattern [`build_exclude_set`] silently
/// dropped.
fn exclude_reason(target: &Path, root: &Path, patterns: &[String]) -> String {
    let relative = target.strip_prefix(root).ok();
    let matched = patterns.iter().find(|pattern| {
        let Ok(matcher) = Glob::new(pattern).map(|glob| glob.compile_matcher()) else {
            return false;
        };
        matcher.is_match(target) || relative.is_some_and(|rel| matcher.is_match(rel))
    });
    match matched {
        Some(pattern) => format!("matches --exclude glob '{pattern}'"),
        None => "matches an --exclude glob".to_string(),
    }
}

/// The ignore-file-layer reason `target` is excluded, naming the file when
/// exactly one was applied, or a generic phrasing when several were (the
/// specific file responsible isn't isolated any further).
fn ignore_file_reason(ignore_files: &[PathBuf]) -> String {
    match ignore_files {
        [only] => format!("excluded by ignore file {}", only.display()),
        _ => "excluded by an ignore file (.querymatterignore/--ignore-file)".to_string(),
    }
}

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
        let names: Vec<_> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
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
        let opts = WalkOpts {
            respect_gitignore: true,
            ..Default::default()
        };
        let got = discover(td.path(), &opts);
        let names: Vec<_> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["kept.md"]);
    }
    #[test]
    fn matches_extension_case_insensitively() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "E.MD", "x");
        touch(td.path(), "f.Markdown", "x");
        touch(td.path(), "g.md", "x");
        let got = discover(td.path(), &WalkOpts::default());
        let names: Vec<_> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert!(
            names.contains(&"E.MD".to_string()),
            "uppercase .MD extension should match case-insensitively: {names:?}"
        );
        assert!(
            names.contains(&"f.Markdown".to_string()),
            "mixed-case .Markdown extension should match case-insensitively: {names:?}"
        );
    }
    #[test]
    fn exclude_glob_skips() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "keep.md", "x");
        touch(td.path(), "templates/t.md", "x");
        let opts = WalkOpts {
            excludes: vec!["**/templates/**".into()],
            ..Default::default()
        };
        let got = discover(td.path(), &opts);
        let names: Vec<_> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["keep.md"]);
    }

    #[test]
    fn ignore_file_excludes_matches() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".querymatterignore", "drafts/\n");
        touch(td.path(), "keep.md", "x");
        touch(td.path(), "drafts/d.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".querymatterignore")],
            ..Default::default()
        };
        let names: Vec<_> = discover(td.path(), &opts)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["keep.md"]); // drafts/d.md excluded
    }

    #[test]
    fn ignore_file_negation_reincludes() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "*.draft.md\n!keep.draft.md\n");
        touch(td.path(), "a.draft.md", "x");
        touch(td.path(), "keep.draft.md", "x");
        touch(td.path(), "b.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".qmi")],
            ..Default::default()
        };
        let names: Vec<_> = discover(td.path(), &opts)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["b.md", "keep.draft.md"]); // a.draft.md excluded, keep re-included
    }

    #[test]
    fn ignore_file_applies_even_when_gitignore_off() {
        // Load-bearing: always-on. respect_gitignore is false (default), yet the
        // ignore file still excludes — this is what makes it NOT just .gitignore.
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "secret.md\n");
        touch(td.path(), "secret.md", "x");
        touch(td.path(), "public.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".qmi")],
            respect_gitignore: false,
            ..Default::default()
        };
        let names: Vec<_> = discover(td.path(), &opts)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["public.md"]);
    }

    #[test]
    fn ignore_file_nonanchored_pattern_matches_at_depth() {
        // Characterization: a non-anchored gitignore pattern matches at ANY depth.
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "templates/\n");
        touch(td.path(), "a.md", "x");
        touch(td.path(), "sub/templates/t.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".qmi")],
            ..Default::default()
        };
        let names: Vec<_> = discover(td.path(), &opts)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["a.md"]); // sub/templates/t.md excluded at depth
    }

    #[test]
    fn ignore_exclude_and_ext_filters_compose() {
        // All three filtering stages in one tree, proving they compose:
        // - ignore file drops drafts/dropped-by-ignore.md
        // - --exclude glob drops excluded/dropped-by-exclude.md
        // - extension filter drops notes.txt
        // - keep.md survives all three
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "drafts/\n");
        touch(td.path(), "drafts/dropped-by-ignore.md", "x");
        touch(td.path(), "excluded/dropped-by-exclude.md", "x");
        touch(td.path(), "notes.txt", "x");
        touch(td.path(), "keep.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".qmi")],
            excludes: vec!["**/excluded/**".into()],
            ..Default::default()
        };
        let names: Vec<_> = discover(td.path(), &opts)
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["keep.md"]);
    }

    #[test]
    fn validate_excludes_accepts_good_globs() {
        assert!(
            validate_excludes(&["**/templates/**".to_string(), "*.draft.md".to_string()]).is_ok()
        );
    }

    #[test]
    fn validate_excludes_rejects_a_bad_glob_naming_it() {
        let err = format!(
            "{:#}",
            validate_excludes(&["[plans/**".to_string()]).unwrap_err()
        );
        assert!(
            err.contains("[plans/**"),
            "must name the bad pattern, got: {err}"
        );
    }

    #[test]
    fn empty_ignore_file_is_noop() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "# only a comment\n\n");
        touch(td.path(), "a.md", "x");
        let opts = WalkOpts {
            ignore_files: vec![td.path().join(".qmi")],
            ..Default::default()
        };
        assert_eq!(discover(td.path(), &opts).len(), 1);
    }

    #[test]
    fn explain_included_for_a_discovered_file() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "keep.md", "x");
        let target = fs::canonicalize(td.path().join("keep.md")).unwrap();
        assert_eq!(
            explain(td.path(), &target, &WalkOpts::default()),
            Explanation::Included
        );
    }

    #[test]
    fn explain_attributes_wrong_extension() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "todo.txt", "x");
        let target = fs::canonicalize(td.path().join("todo.txt")).unwrap();
        match explain(td.path(), &target, &WalkOpts::default()) {
            Explanation::Excluded(reason) => {
                assert_eq!(reason, "extension 'txt' not in --ext (md, markdown)");
            }
            other => panic!("expected Excluded, got {other:?}"),
        }
    }

    #[test]
    fn explain_attributes_a_missing_extension() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "README", "x");
        let target = fs::canonicalize(td.path().join("README")).unwrap();
        match explain(td.path(), &target, &WalkOpts::default()) {
            Explanation::Excluded(reason) => {
                assert_eq!(reason, "no file extension (want: md, markdown)");
            }
            other => panic!("expected Excluded, got {other:?}"),
        }
    }

    #[test]
    fn explain_attributes_a_hidden_directory() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".draft/h.md", "x");
        let target = fs::canonicalize(td.path().join(".draft/h.md")).unwrap();
        match explain(td.path(), &target, &WalkOpts::default()) {
            Explanation::Excluded(reason) => {
                assert_eq!(
                    reason,
                    "hidden directory '.draft' (pass --hidden to include)"
                );
            }
            other => panic!("expected Excluded, got {other:?}"),
        }
    }

    #[test]
    fn explain_attributes_a_hidden_file() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".secret.md", "x");
        let target = fs::canonicalize(td.path().join(".secret.md")).unwrap();
        match explain(td.path(), &target, &WalkOpts::default()) {
            Explanation::Excluded(reason) => {
                assert_eq!(
                    reason,
                    "hidden file '.secret.md' (pass --hidden to include)"
                );
            }
            other => panic!("expected Excluded, got {other:?}"),
        }
    }

    #[test]
    fn explain_attributes_an_exclude_glob() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "templates/t.md", "x");
        let target = fs::canonicalize(td.path().join("templates/t.md")).unwrap();
        let opts = WalkOpts {
            excludes: vec!["**/templates/**".into()],
            ..Default::default()
        };
        match explain(td.path(), &target, &opts) {
            Explanation::Excluded(reason) => {
                assert_eq!(reason, "matches --exclude glob '**/templates/**'");
            }
            other => panic!("expected Excluded, got {other:?}"),
        }
    }

    #[test]
    fn explain_attributes_a_single_ignore_file() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".qmi", "drafts/\n");
        touch(td.path(), "drafts/d.md", "x");
        let ignore_path = td.path().join(".qmi");
        let target = fs::canonicalize(td.path().join("drafts/d.md")).unwrap();
        let opts = WalkOpts {
            ignore_files: vec![ignore_path.clone()],
            ..Default::default()
        };
        match explain(td.path(), &target, &opts) {
            Explanation::Excluded(reason) => {
                assert_eq!(
                    reason,
                    format!("excluded by ignore file {}", ignore_path.display())
                );
            }
            other => panic!("expected Excluded, got {other:?}"),
        }
    }

    #[test]
    fn explain_attributes_gitignore_under_respect_gitignore() {
        let td = TempDir::new().unwrap();
        touch(td.path(), ".gitignore", "ignored.md\n");
        touch(td.path(), "ignored.md", "x");
        let target = fs::canonicalize(td.path().join("ignored.md")).unwrap();
        let opts = WalkOpts {
            respect_gitignore: true,
            ..Default::default()
        };
        match explain(td.path(), &target, &opts) {
            Explanation::Excluded(reason) => {
                assert_eq!(reason, "excluded by .gitignore rules (--respect-gitignore)");
            }
            other => panic!("expected Excluded, got {other:?}"),
        }
    }

    /// Property: for a small tree with every kind of filter in play,
    /// `explain`'s verdict must agree with `discover()`'s actual membership
    /// for every file — the ground-truth invariant `explain` promises.
    #[test]
    fn explain_verdict_matches_discover_membership_for_every_file() {
        let td = TempDir::new().unwrap();
        touch(td.path(), "keep.md", "x");
        touch(td.path(), "todo.txt", "x");
        touch(td.path(), ".draft/h.md", "x");
        touch(td.path(), "templates/t.md", "x");
        touch(td.path(), ".qmi", "skip/\n");
        touch(td.path(), "skip/s.md", "x");
        let opts = WalkOpts {
            excludes: vec!["**/templates/**".into()],
            ignore_files: vec![td.path().join(".qmi")],
            ..Default::default()
        };

        let discovered = discover(td.path(), &opts);
        for path in every_file(td.path()) {
            let target = fs::canonicalize(&path).unwrap();
            let included = discovered.contains(&target);
            let verdict = explain(td.path(), &target, &opts);
            assert_eq!(
                matches!(verdict, Explanation::Included),
                included,
                "explain disagreed with discover() for {}: {verdict:?}",
                target.display()
            );
        }
    }

    /// Every regular file under `dir`, recursively, with no filtering at all
    /// (unlike [`discover`]) — used to build the exhaustive candidate set for
    /// [`explain_verdict_matches_discover_membership_for_every_file`].
    fn every_file(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                files.extend(every_file(&path));
            } else if file_type.is_file() {
                files.push(path);
            }
        }
        files
    }
}
