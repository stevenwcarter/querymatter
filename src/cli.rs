//! The `querymatter` command-line interface: argument parsing plus the
//! translation from raw flags into a canonical set of scan roots and the
//! [`WalkOpts`] that drive discovery.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;
use globset::Glob;

use crate::discover::WalkOpts;
use crate::render::Format;

/// Query Markdown YAML frontmatter with a SQL subset.
#[derive(Debug, Parser)]
#[command(name = "querymatter", version, about)]
pub struct Cli {
    /// Directories to scan recursively. Empty means the current directory.
    pub dirs: Vec<PathBuf>,

    /// Run a single query and exit; `-` reads the query text from stdin.
    #[arg(short = 'e', long = "query")]
    pub query: Option<String>,

    /// Output format for results.
    #[arg(long, default_value = "table")]
    pub format: Format,

    /// File extensions (without the leading dot) to include.
    #[arg(long, value_delimiter = ',', default_value = "md,markdown")]
    pub ext: Vec<String>,

    /// Honor `.gitignore`/`.ignore` rules while scanning.
    #[arg(long)]
    pub respect_gitignore: bool,

    /// Descend into hidden files and directories.
    #[arg(long)]
    pub hidden: bool,

    /// Glob pattern excluding matching files; repeatable.
    #[arg(long)]
    pub exclude: Vec<String>,

    /// Apply a gitignore-style ignore file. Repeatable; applied in order after
    /// the auto-discovered cwd `.querymatterignore`.
    #[arg(long)]
    pub ignore_file: Vec<PathBuf>,

    /// Do not auto-discover a `.querymatterignore` in the current directory.
    /// Explicit `--ignore-file`s still apply.
    #[arg(long)]
    pub no_ignore_file: bool,
}

impl Cli {
    /// Builds the [`WalkOpts`] described by these flags.
    pub fn walk_opts(&self) -> WalkOpts {
        WalkOpts {
            exts: self.ext.clone(),
            respect_gitignore: self.respect_gitignore,
            hidden: self.hidden,
            excludes: self.exclude.clone(),
            ignore_files: Vec::new(),
        }
    }

    /// Resolves the scan roots: the positional `dirs`, or the current
    /// directory when none were given.
    ///
    /// Each root is canonicalized once here at the CLI boundary — resolving
    /// symlinks and absolutizing — because [`discover`](crate::discover)'s
    /// exclude matching assumes an absolute root and
    /// [`RecordStore::reload_dir`](crate::store::RecordStore::reload_dir)
    /// keys slices by path equality; canonicalizing once keeps both
    /// consistent. A missing or inaccessible directory is a hard error that
    /// names the offending path.
    pub fn resolved_roots(&self) -> anyhow::Result<Vec<PathBuf>> {
        let raw = if self.dirs.is_empty() {
            vec![env::current_dir().context("failed to determine the current directory")?]
        } else {
            self.dirs.clone()
        };
        let mut roots = Vec::with_capacity(raw.len());
        for dir in raw {
            let canonical = fs::canonicalize(&dir)
                .with_context(|| format!("cannot access directory {}", dir.display()))?;
            // Dedup exact-equal canonical roots so `querymatter . ./plans`
            // (both resolving to the same directory) doesn't scan it twice and
            // double every count. Overlapping-but-unequal roots — a parent and
            // its descendant — are left as-is (see the README caveat).
            if !roots.contains(&canonical) {
                roots.push(canonical);
            }
        }
        Ok(roots)
    }

    /// Rejects any `--exclude` glob that `globset` cannot compile, naming the
    /// bad pattern, so invalid input surfaces up front instead of being
    /// silently ignored deeper in discovery.
    pub fn validate_excludes(&self) -> anyhow::Result<()> {
        for pat in &self.exclude {
            Glob::new(pat).with_context(|| format!("invalid --exclude glob {pat:?}"))?;
        }
        Ok(())
    }

    /// Ordered list of gitignore-style ignore files to apply, earliest first:
    /// the cwd `.querymatterignore` (unless `--no-ignore-file`) followed by each
    /// `--ignore-file` in order. This is the single seam a future `.querymatter`
    /// vault extends (prepending the vault-parent file).
    // First consumed by Task 3's `main` wiring; `cli` is a private module
    // (`mod cli;`), so this `pub` method is unreachable-so-far and clippy's
    // dead-code lint correctly flags it until then. Remove once Task 3 calls it.
    #[allow(dead_code)]
    pub fn ignore_files(&self) -> anyhow::Result<Vec<PathBuf>> {
        let cwd = env::current_dir().context("failed to determine the current directory")?;
        self.resolve_ignore_files(&cwd)
    }

    // Exercised directly by the tests below; only reachable through
    // `ignore_files` (above) in non-test builds, so it's dead there too until
    // Task 3 wires `main` to call `ignore_files`.
    #[allow(dead_code)]
    fn resolve_ignore_files(&self, cwd: &Path) -> anyhow::Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        if !self.no_ignore_file {
            let candidate = cwd.join(".querymatterignore");
            if candidate.is_file() {
                files.push(candidate);
            }
        }
        for f in &self.ignore_file {
            anyhow::ensure!(f.is_file(), "cannot read --ignore-file {}", f.display());
            files.push(f.clone());
        }
        Ok(files)
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn resolved_roots_dedups_exact_duplicate_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        // The same directory passed twice must collapse to a single canonical
        // root, so counts aren't doubled.
        let cli = Cli::parse_from(["querymatter", path, path]);
        let roots = cli.resolved_roots().unwrap();
        assert_eq!(roots, vec![fs::canonicalize(dir.path()).unwrap()]);
    }

    #[test]
    fn resolves_cwd_ignore_file_when_present() {
        let td = tempdir().unwrap();
        fs::write(td.path().join(".querymatterignore"), "x\n").unwrap();
        let cli = Cli::parse_from(["querymatter"]);
        assert_eq!(
            cli.resolve_ignore_files(td.path()).unwrap(),
            vec![td.path().join(".querymatterignore")]
        );
    }

    #[test]
    fn no_ignore_file_skips_cwd() {
        let td = tempdir().unwrap();
        fs::write(td.path().join(".querymatterignore"), "x\n").unwrap();
        let cli = Cli::parse_from(["querymatter", "--no-ignore-file"]);
        assert!(cli.resolve_ignore_files(td.path()).unwrap().is_empty());
    }

    #[test]
    fn appends_ignore_file_flags_in_order() {
        let td = tempdir().unwrap();
        let a = td.path().join("a.ignore");
        let b = td.path().join("b.ignore");
        fs::write(&a, "x\n").unwrap();
        fs::write(&b, "x\n").unwrap();
        let cli = Cli::parse_from([
            "querymatter",
            "--no-ignore-file",
            "--ignore-file",
            a.to_str().unwrap(),
            "--ignore-file",
            b.to_str().unwrap(),
        ]);
        assert_eq!(cli.resolve_ignore_files(td.path()).unwrap(), vec![a, b]);
    }

    #[test]
    fn cwd_file_then_ignore_file_flags() {
        let td = tempdir().unwrap();
        fs::write(td.path().join(".querymatterignore"), "x\n").unwrap();
        let extra = td.path().join("extra.ignore");
        fs::write(&extra, "y\n").unwrap();
        let cli = Cli::parse_from(["querymatter", "--ignore-file", extra.to_str().unwrap()]);
        assert_eq!(
            cli.resolve_ignore_files(td.path()).unwrap(),
            vec![td.path().join(".querymatterignore"), extra]
        );
    }

    #[test]
    fn missing_ignore_file_errors() {
        let td = tempdir().unwrap();
        let missing = td.path().join("nope.ignore");
        let cli = Cli::parse_from(["querymatter", "--ignore-file", missing.to_str().unwrap()]);
        assert!(cli.resolve_ignore_files(td.path()).is_err());
    }

    #[test]
    fn absent_cwd_file_is_not_error() {
        let td = tempdir().unwrap(); // no .querymatterignore
        let cli = Cli::parse_from(["querymatter"]);
        assert!(cli.resolve_ignore_files(td.path()).unwrap().is_empty());
    }
}
