//! Pure helpers for the `init` git-ignore prompt (design spec §7): locating
//! an enclosing git working tree, checking whether `.querymatter` is already
//! ignored, and appending a `.querymatter/` line to `.gitignore`. Reading the
//! interactive answer and printing the prompt itself live in `main`, which
//! wires these into the `init` path; these are the pure, unit-testable pieces.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// Name of the directory marking a git working tree root.
const GIT_DIR_NAME: &str = ".git";

/// Line appended to `.gitignore` when the user opts in.
const GITIGNORE_LINE: &str = ".querymatter/";

/// Returns the nearest ancestor of `start` (inclusive) containing a `.git`
/// directory, or `None` if `start` is not inside a git working tree.
pub fn git_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start.canonicalize().ok()?;
    loop {
        if dir.join(GIT_DIR_NAME).is_dir() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Returns whether `<git_root>/.gitignore` already has a line ignoring
/// `.querymatter` — a simple trimmed-line match against the handful of
/// common spellings, not full gitignore pattern semantics.
pub fn querymatter_ignored(git_root: &Path) -> bool {
    let Ok(contents) = fs::read_to_string(git_root.join(".gitignore")) else {
        return false;
    };
    contents.lines().map(str::trim).any(|line| {
        matches!(
            line,
            ".querymatter" | ".querymatter/" | "/.querymatter" | "/.querymatter/"
        )
    })
}

/// Appends a `.querymatter/` line to `<git_root>/.gitignore`, creating the
/// file if absent. Internally idempotent — a call that finds
/// [`querymatter_ignored`] already true is a no-op — even though callers
/// also gate on it themselves before offering the prompt.
pub fn append_gitignore(git_root: &Path) -> anyhow::Result<()> {
    if querymatter_ignored(git_root) {
        return Ok(());
    }

    let path = git_root.join(".gitignore");
    let mut updated = fs::read_to_string(&path).unwrap_or_default();
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str(GITIGNORE_LINE);
    updated.push('\n');

    fs::write(&path, updated).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn git_root_finds_ancestor() {
        let td = TempDir::new().unwrap();
        fs::create_dir_all(td.path().join(".git")).unwrap();
        let deep = td.path().join("a/b/c");
        fs::create_dir_all(&deep).unwrap();

        assert_eq!(git_root(&deep), Some(fs::canonicalize(td.path()).unwrap()));
    }

    #[test]
    fn git_root_none_outside_a_repo() {
        let td = TempDir::new().unwrap();
        assert_eq!(git_root(td.path()), None);
    }

    #[test]
    fn querymatter_ignored_true_for_known_line_variants() {
        for line in [
            ".querymatter",
            ".querymatter/",
            "/.querymatter",
            "/.querymatter/",
        ] {
            let td = TempDir::new().unwrap();
            fs::write(
                td.path().join(".gitignore"),
                format!("node_modules\n{line}\n"),
            )
            .unwrap();
            assert!(querymatter_ignored(td.path()), "expected {line:?} to match");
        }
    }

    #[test]
    fn querymatter_ignored_false_when_absent_or_no_file() {
        let td = TempDir::new().unwrap();
        assert!(!querymatter_ignored(td.path()));

        fs::write(td.path().join(".gitignore"), "node_modules\ntarget/\n").unwrap();
        assert!(!querymatter_ignored(td.path()));
    }

    #[test]
    fn append_gitignore_creates_file_when_absent() {
        let td = TempDir::new().unwrap();
        append_gitignore(td.path()).unwrap();

        let contents = fs::read_to_string(td.path().join(".gitignore")).unwrap();
        assert_eq!(contents, ".querymatter/\n");
        assert!(querymatter_ignored(td.path()));
    }

    #[test]
    fn append_gitignore_adds_leading_newline_when_missing() {
        let td = TempDir::new().unwrap();
        fs::write(td.path().join(".gitignore"), "node_modules").unwrap();

        append_gitignore(td.path()).unwrap();

        let contents = fs::read_to_string(td.path().join(".gitignore")).unwrap();
        assert_eq!(contents, "node_modules\n.querymatter/\n");
    }

    #[test]
    fn append_gitignore_is_idempotent() {
        let td = TempDir::new().unwrap();
        append_gitignore(td.path()).unwrap();
        append_gitignore(td.path()).unwrap();

        let contents = fs::read_to_string(td.path().join(".gitignore")).unwrap();
        assert_eq!(contents, ".querymatter/\n");
    }
}
