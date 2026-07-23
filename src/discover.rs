//! Recursively finds Markdown files under a root directory.
//!
//! Wraps [`ignore::WalkBuilder`] with gitignore/hidden-file handling that
//! defaults to "everything" — this is a query tool, not a Git tool, so a
//! stray `.gitignore` shouldn't silently hide content from queries unless
//! the caller opts in via [`WalkOpts::respect_gitignore`].

use std::path::{Path, PathBuf};

use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;

/// Options controlling how [`discover`] walks a directory tree.
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
}
