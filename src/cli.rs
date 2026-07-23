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
        raw.into_iter()
            .map(|dir| {
                fs::canonicalize(&dir)
                    .with_context(|| format!("cannot access directory {}", dir.display()))
            })
            .collect()
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
