//! The `querymatter` command-line interface: argument parsing plus the
//! translation from raw flags into a canonical set of scan roots and the
//! [`WalkOpts`] that drive discovery.

use std::env;
use std::fs;
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
}

impl Cli {
    /// Builds the [`WalkOpts`] described by these flags.
    pub fn walk_opts(&self) -> WalkOpts {
        WalkOpts {
            exts: self.ext.clone(),
            respect_gitignore: self.respect_gitignore,
            hidden: self.hidden,
            excludes: self.exclude.clone(),
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
}
