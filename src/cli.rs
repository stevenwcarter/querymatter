//! The `querymatter` command-line interface: argument parsing plus the
//! translation from raw flags into a canonical set of scan roots and the
//! [`WalkOpts`](crate::discover::WalkOpts) that drive discovery.
//!
//! The surface is an optional subcommand ([`Command`]) layered over the
//! existing query args: with no subcommand the flags on [`Cli`] drive a query
//! run; `querymatter init` builds a `.querymatter` cache instead. The six
//! walk-controlling flags live on a single [`WalkFlags`] struct
//! `#[command(flatten)]`ed into both, so query mode and `init` share one
//! definition rather than duplicating it.

use std::env;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use globset::Glob;

use crate::cache::Freshness;
use crate::render::{Format, TableStyle};

/// The six flags that shape a directory walk, shared verbatim between query
/// mode and `querymatter init` via `#[command(flatten)]`.
///
/// Grouping them here keeps [`validate_excludes`](WalkFlags::validate_excludes)
/// and [`ignore_files`](WalkFlags::ignore_files) — which only ever read these
/// fields — off [`Cli`], so `init` reuses the exact same discovery semantics.
/// Turning the raw flags into a [`WalkOpts`](crate::discover::WalkOpts) is
/// [`Settings::walk_opts`](crate::settings::Settings::walk_opts)'s job, since
/// only the resolver knows which layer won for each field.
#[derive(Debug, Args)]
pub struct WalkFlags {
    /// File extensions (without the leading dot) to include. [default: md,markdown]
    #[arg(long, value_delimiter = ',')]
    pub ext: Option<Vec<String>>,

    /// Honor `.gitignore`/`.ignore` rules while scanning.
    #[arg(long)]
    pub respect_gitignore: bool,

    /// Ignore `.gitignore`/`.ignore` rules, overriding a config `true`.
    #[arg(long, conflicts_with = "respect_gitignore")]
    pub no_respect_gitignore: bool,

    /// Descend into hidden files and directories.
    #[arg(long)]
    pub hidden: bool,

    /// Do not descend into hidden files and directories, overriding a config `true`.
    #[arg(long, conflicts_with = "hidden")]
    pub no_hidden: bool,

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

impl WalkFlags {
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
    /// `--ignore-file` in order. This is the single seam the `.querymatter`
    /// vault extends (prepending the vault-parent file).
    pub fn ignore_files(&self) -> anyhow::Result<Vec<PathBuf>> {
        let cwd = env::current_dir().context("failed to determine the current directory")?;
        self.resolve_ignore_files(&cwd)
    }

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

/// Query Markdown YAML frontmatter with a SQL subset.
#[derive(Debug, Parser)]
#[command(name = "querymatter", version, about)]
pub struct Cli {
    /// Directories to scan recursively. Empty means the current directory.
    pub dirs: Vec<PathBuf>,

    /// Run a single query and exit; `-` reads the query text from stdin.
    #[arg(short = 'e', long = "query")]
    pub query: Option<String>,

    /// Output format for results. [default: table]
    #[arg(long, value_enum)]
    pub format: Option<Format>,

    /// Border style for `--format table`. [default: ascii]
    #[arg(long, value_enum, env = "QUERYMATTER_TABLE_STYLE")]
    pub table_style: Option<TableStyle>,

    /// Flags shared with `querymatter init` that shape the directory walk.
    #[command(flatten)]
    pub walk: WalkFlags,

    /// Ignore any `.querymatter` cache and always live-scan (today's behavior).
    #[arg(long)]
    pub no_cache: bool,

    /// Trust the `.querymatter` cache verbatim with no filesystem access;
    /// errors when no cache is found.
    #[arg(long)]
    pub force_cache: bool,

    /// Use the dir-mtime + TTL hybrid freshness check instead of per-file.
    #[arg(long)]
    pub fast: bool,

    /// Force a re-scan of PATH's subtree before querying; repeatable.
    #[arg(long, value_name = "PATH")]
    pub refresh: Vec<PathBuf>,

    /// Force a re-scan of the whole vault before querying.
    #[arg(long)]
    pub refresh_all: bool,

    /// Subcommand to run instead of a query; `None` means query mode.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The `querymatter` subcommands. Absent (`Cli::command == None`) means query
/// mode, driven by the query flags directly on [`Cli`].
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build a `.querymatter` cache over a directory tree for faster queries.
    Init(InitArgs),
}

/// Arguments for `querymatter init [DIR]`.
#[derive(Debug, Args)]
pub struct InitArgs {
    /// Directory to build the cache under; defaults to the current directory.
    pub dir: Option<PathBuf>,

    /// TTL, in seconds, stored in the cache and consulted only by `--fast`.
    #[arg(long, default_value_t = 300)]
    pub ttl: u64,

    /// Walk flags shared with query mode.
    #[command(flatten)]
    pub walk: WalkFlags,
}

impl Cli {
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

    /// Maps the freshness flags to a [`Freshness`] mode: `--force-cache` wins,
    /// then `--fast`, otherwise the accurate per-file default. Mutually
    /// exclusive combinations are rejected by [`Self::validate`] before this is
    /// consulted, so the precedence here only picks among individually valid
    /// inputs.
    pub fn freshness(&self) -> Freshness {
        if self.force_cache {
            Freshness::ForceCache
        } else if self.fast {
            Freshness::Fast
        } else {
            Freshness::PerFile
        }
    }

    /// Rejects contradictory combinations of the cache flags, with a clear
    /// message naming the offending pair. Called in `main` before any cache
    /// work so a bad combination fails fast with a non-zero exit rather than
    /// silently picking one interpretation.
    pub fn validate(&self) -> anyhow::Result<()> {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use crate::cache::Freshness;
    use crate::render::TableStyle;
    use clap::{CommandFactory, FromArgMatches};
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    /// Parses `args` with the `--table-style` env fallback disabled.
    ///
    /// `Cli::parse_from` consults the real process environment, so a
    /// developer with `QUERYMATTER_TABLE_STYLE` exported would otherwise see
    /// these tests fail — or, for an invalid value, watch clap `exit(2)` take
    /// the whole unit-test binary down with it.
    fn parse(args: &[&str]) -> Cli {
        try_parse(args).expect("valid CLI args")
    }

    /// Like [`parse`], but returns clap's error instead of exiting, for the
    /// tests that assert a bad value is rejected.
    fn try_parse(args: &[&str]) -> Result<Cli, clap::Error> {
        let command = Cli::command().mut_arg("table_style", |a| a.env(None::<&str>));
        let matches = command.try_get_matches_from(args)?;
        Cli::from_arg_matches(&matches)
    }

    #[test]
    fn resolved_roots_dedups_exact_duplicate_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        // The same directory passed twice must collapse to a single canonical
        // root, so counts aren't doubled.
        let cli = parse(&["querymatter", path, path]);
        let roots = cli.resolved_roots().unwrap();
        assert_eq!(roots, vec![fs::canonicalize(dir.path()).unwrap()]);
    }

    #[test]
    fn resolves_cwd_ignore_file_when_present() {
        let td = tempdir().unwrap();
        fs::write(td.path().join(".querymatterignore"), "x\n").unwrap();
        let cli = parse(&["querymatter"]);
        assert_eq!(
            cli.walk.resolve_ignore_files(td.path()).unwrap(),
            vec![td.path().join(".querymatterignore")]
        );
    }

    #[test]
    fn no_ignore_file_skips_cwd() {
        let td = tempdir().unwrap();
        fs::write(td.path().join(".querymatterignore"), "x\n").unwrap();
        let cli = parse(&["querymatter", "--no-ignore-file"]);
        assert!(cli.walk.resolve_ignore_files(td.path()).unwrap().is_empty());
    }

    #[test]
    fn appends_ignore_file_flags_in_order() {
        let td = tempdir().unwrap();
        let a = td.path().join("a.ignore");
        let b = td.path().join("b.ignore");
        fs::write(&a, "x\n").unwrap();
        fs::write(&b, "x\n").unwrap();
        let cli = parse(&[
            "querymatter",
            "--no-ignore-file",
            "--ignore-file",
            a.to_str().unwrap(),
            "--ignore-file",
            b.to_str().unwrap(),
        ]);
        assert_eq!(
            cli.walk.resolve_ignore_files(td.path()).unwrap(),
            vec![a, b]
        );
    }

    #[test]
    fn cwd_file_then_ignore_file_flags() {
        let td = tempdir().unwrap();
        fs::write(td.path().join(".querymatterignore"), "x\n").unwrap();
        let extra = td.path().join("extra.ignore");
        fs::write(&extra, "y\n").unwrap();
        let cli = parse(&["querymatter", "--ignore-file", extra.to_str().unwrap()]);
        assert_eq!(
            cli.walk.resolve_ignore_files(td.path()).unwrap(),
            vec![td.path().join(".querymatterignore"), extra]
        );
    }

    #[test]
    fn missing_ignore_file_errors() {
        let td = tempdir().unwrap();
        let missing = td.path().join("nope.ignore");
        let cli = parse(&["querymatter", "--ignore-file", missing.to_str().unwrap()]);
        assert!(cli.walk.resolve_ignore_files(td.path()).is_err());
    }

    #[test]
    fn absent_cwd_file_is_not_error() {
        let td = tempdir().unwrap(); // no .querymatterignore
        let cli = parse(&["querymatter"]);
        assert!(cli.walk.resolve_ignore_files(td.path()).unwrap().is_empty());
    }

    #[test]
    fn freshness_force_cache_wins_over_fast() {
        // `--force-cache` and `--fast` are individually valid; `freshness()`
        // is the pure mapping (validation of the conflict lives in `validate`).
        let cli = parse(&["querymatter", "--force-cache", "--fast"]);
        assert_eq!(cli.freshness(), Freshness::ForceCache);
    }

    #[test]
    fn freshness_fast_when_only_fast() {
        let cli = parse(&["querymatter", "--fast"]);
        assert_eq!(cli.freshness(), Freshness::Fast);
    }

    #[test]
    fn freshness_defaults_to_per_file() {
        let cli = parse(&["querymatter"]);
        assert_eq!(cli.freshness(), Freshness::PerFile);
    }

    #[test]
    fn force_cache_conflicts_with_refresh() {
        let cli = parse(&["querymatter", "--force-cache", "--refresh", "sub"]);
        assert!(cli.validate().is_err());
    }

    #[test]
    fn force_cache_conflicts_with_refresh_all() {
        let cli = parse(&["querymatter", "--force-cache", "--refresh-all"]);
        assert!(cli.validate().is_err());
    }

    #[test]
    fn no_cache_conflicts_with_force_cache() {
        let cli = parse(&["querymatter", "--no-cache", "--force-cache"]);
        assert!(cli.validate().is_err());
    }

    #[test]
    fn no_cache_conflicts_with_refresh() {
        let cli = parse(&["querymatter", "--no-cache", "--refresh", "sub"]);
        assert!(cli.validate().is_err());
    }

    #[test]
    fn no_cache_conflicts_with_refresh_all() {
        let cli = parse(&["querymatter", "--no-cache", "--refresh-all"]);
        assert!(cli.validate().is_err());
    }

    #[test]
    fn force_cache_conflicts_with_fast() {
        let cli = parse(&["querymatter", "--force-cache", "--fast"]);
        assert!(cli.validate().is_err());
    }

    #[test]
    fn compatible_flags_pass_validation() {
        assert!(parse(&["querymatter"]).validate().is_ok());
        assert!(parse(&["querymatter", "--fast"]).validate().is_ok());
        assert!(
            parse(&["querymatter", "--refresh", "a", "--refresh", "b"])
                .validate()
                .is_ok()
        );
        assert!(parse(&["querymatter", "--no-cache"]).validate().is_ok());
    }

    #[test]
    fn init_subcommand_parses_dir_and_ttl() {
        let cli = parse(&["querymatter", "init", "somedir", "--ttl", "42"]);
        match cli.command {
            Some(Command::Init(args)) => {
                assert_eq!(args.dir.as_deref(), Some(Path::new("somedir")));
                assert_eq!(args.ttl, 42);
            }
            other => panic!("expected an Init subcommand, got {other:?}"),
        }
    }

    #[test]
    fn init_ttl_defaults_to_300() {
        let cli = parse(&["querymatter", "init"]);
        match cli.command {
            Some(Command::Init(args)) => assert_eq!(args.ttl, 300),
            other => panic!("expected an Init subcommand, got {other:?}"),
        }
    }

    #[test]
    fn walk_flags_still_parse_identically_after_flatten() {
        // `flatten` is transparent on the command line: the six walk flags
        // parse exactly as before, now landing on the nested `walk` struct.
        let cli = parse(&[
            "querymatter",
            "--hidden",
            "--respect-gitignore",
            "--ext",
            "md,txt",
            "--exclude",
            "**/x/**",
        ]);
        assert!(cli.walk.hidden);
        assert!(cli.walk.respect_gitignore);
        assert_eq!(
            cli.walk.ext,
            Some(vec!["md".to_string(), "txt".to_string()])
        );
        assert_eq!(cli.walk.exclude, vec!["**/x/**".to_string()]);
    }

    /// With nothing set, `Cli::table_style` stays `None`: clap no longer
    /// supplies the ascii default, so an absent flag is indistinguishable
    /// from an absent config entry — [`crate::settings::Settings`] is the
    /// single place the built-in default is applied.
    #[test]
    fn table_style_absent_when_not_given() {
        let cli = parse(&["querymatter"]);
        assert_eq!(cli.table_style, None);
    }

    #[test]
    fn table_style_flag_parses() {
        let cli = parse(&["querymatter", "--table-style", "unicode"]);
        assert_eq!(cli.table_style, Some(TableStyle::Unicode));
    }

    #[test]
    fn bad_table_style_is_rejected() {
        assert!(try_parse(&["querymatter", "--table-style", "fancy"]).is_err());
    }
}
