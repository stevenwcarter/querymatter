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

use crate::cache::Freshness;
use crate::config::ConfigKey;
use crate::render::{Format, TableStyle};

/// Every flag that shapes a directory walk, shared verbatim between query
/// mode and `querymatter init` via `#[command(flatten)]`.
///
/// Grouping them here keeps [`ignore_files`](WalkFlags::ignore_files) — which
/// only ever reads these fields — off [`Cli`], so `init` reuses the exact
/// same discovery semantics. Turning the raw flags into a
/// [`WalkOpts`](crate::discover::WalkOpts) is
/// [`Settings::walk_opts`](crate::settings::Settings::walk_opts)'s job, since
/// only the resolver knows which layer won for each field. Exclude-glob
/// validation ([`crate::discover::validate_excludes`]) runs on the
/// *resolved* setting, not the flag alone, so a bad glob from the config
/// file is caught too — see `run_query`/`run_init` in `main.rs`.
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

    /// Write query results to PATH instead of stdout (one-shot/batch mode
    /// only); PATH is truncated before the first statement's result and
    /// every later statement in the run appends to it. Stdout stays empty.
    /// The interactive REPL has its own `.output` dot-command instead.
    #[arg(long, value_name = "PATH")]
    pub output: Option<PathBuf>,

    /// Disable unknown-column validation, treating an unknown
    /// SELECT/WHERE/GROUP BY/ORDER BY/HAVING column as NULL instead of
    /// failing the query with a suggestion.
    #[arg(long)]
    pub lenient: bool,

    /// Force strict unknown-column validation, overriding a config `lenient
    /// = true`.
    #[arg(long, conflicts_with = "lenient")]
    pub no_lenient: bool,

    /// Suppress the header row in table/csv/tsv/md output.
    #[arg(long)]
    pub no_header: bool,

    /// Force the header row on, overriding a config `header = false`.
    #[arg(long, conflicts_with = "no_header")]
    pub header: bool,

    /// Suppress non-error stderr chatter (skipped-file warnings, scan summaries).
    #[arg(long, short = 'q')]
    pub quiet: bool,

    /// Force chatter on, overriding a config `quiet = true`.
    #[arg(long, conflicts_with = "quiet")]
    pub no_quiet: bool,

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

    /// Exit 0 when the query matched at least one row, 1 when it matched
    /// none, and 2 on a parse/exec/IO error — grep-style, for scripting.
    /// Query mode and `query run` only; `init`/`config`/`query save`/`query
    /// list`/`query get`/`query delete`/`completions` are unaffected.
    #[arg(long)]
    pub exit_code: bool,

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
    /// Show or change the persistent configuration.
    Config(ConfigArgs),
    /// Inspect the `.querymatter` cache.
    Cache(CacheArgs),
    /// Save, list, inspect, run, or delete a saved named query.
    Query(QueryArgs),
    /// Report whether a file would be discovered, and if not, why.
    Explain(ExplainArgs),
    /// Print a shell completion script to stdout, or install it directly
    /// with `--install`.
    Completions(CompletionsArgs),
}

/// Arguments for `querymatter config <ACTION>`.
#[derive(Debug, Args)]
pub struct ConfigArgs {
    /// What to do with the configuration.
    #[command(subcommand)]
    pub action: ConfigAction,
}

/// The `querymatter config` actions.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// List every setting, its value, and where that value came from.
    List,
    /// Show one setting's value and the values it accepts.
    Get {
        /// The setting to show.
        #[arg(value_enum)]
        key: ConfigKey,
    },
    /// Set one setting in the config file.
    Set {
        /// The setting to change.
        #[arg(value_enum)]
        key: ConfigKey,
        /// The new value; a comma-separated list for `ext` and `exclude`.
        value: String,
    },
    /// Remove one setting from the config file.
    Unset {
        /// The setting to remove.
        #[arg(value_enum)]
        key: ConfigKey,
    },
    /// Print the config file's path, whether or not it exists.
    Path,
}

/// Arguments for `querymatter cache <ACTION>`.
#[derive(Debug, Args)]
pub struct CacheArgs {
    /// What to do with the cache.
    #[command(subcommand)]
    pub action: CacheAction,
}

/// The `querymatter cache` actions. Only `status` exists today; the group
/// leaves room for a future `cache clear` without reshaping the CLI surface.
#[derive(Debug, Subcommand)]
pub enum CacheAction {
    /// Show cache location, size, counts, TTL, and per-directory scan times.
    Status {
        /// Directory whose vault to inspect; defaults to the current directory.
        dir: Option<PathBuf>,
    },
}

/// Arguments for `querymatter query <ACTION>`.
#[derive(Debug, Args)]
pub struct QueryArgs {
    /// What to do with saved queries.
    #[command(subcommand)]
    pub action: QueryAction,
}

/// The `querymatter query` actions.
///
/// Saved queries are static SQL text with no parameters (YAGNI): `save`
/// stores it under a name, `run` resolves the name back to SQL and executes
/// it exactly like `-e` would — honoring the same `--format`/`--table-style`/
/// `--output`/`--exit-code`/walk flags, plus an optional `[DIR]` positional
/// that overrides the scan root exactly like query mode's `[DIRS]` would —
/// via [`Command::Query`]'s dispatch in `main`. `save`/`list`/`get`/`delete`
/// never build a record store; they only read and write the `queries.toml`
/// file (see [`crate::queries`]).
#[derive(Debug, Subcommand)]
pub enum QueryAction {
    /// Save SQL under NAME, overwriting any existing query already saved
    /// under that name.
    Save {
        /// The name to save under: letters, digits, `_`, and `-` only.
        name: String,
        /// The SQL to save; rejected up front if it fails to parse.
        sql: String,
    },
    /// List every saved query's name and SQL.
    List,
    /// Print one saved query's SQL.
    Get {
        /// The saved query's name.
        name: String,
    },
    /// Run a saved query, honoring the same output flags as `-e`.
    Run {
        /// The saved query's name.
        name: String,
        /// Directory to scan, overriding the cwd/vault default — exactly
        /// like query mode's positional `[DIRS]` with a single entry.
        dir: Option<PathBuf>,
    },
    /// Remove a saved query.
    Delete {
        /// The saved query's name.
        name: String,
    },
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

/// Arguments for `querymatter explain <PATH>`.
///
/// `PATH` is the only positional — unlike query mode and `init`, `explain`
/// has no `[DIRS]` to root the walk at, since clap cannot parse a top-level
/// positional ahead of a subcommand name (`querymatter <dir> explain <path>`
/// fails to parse `<dir>` as `Cli::dirs`). `run_explain` resolves the scan
/// root itself instead — see its doc comment.
#[derive(Debug, Args)]
pub struct ExplainArgs {
    /// The file to explain.
    pub path: PathBuf,

    /// Walk flags shared with query mode, so `--ext`/`--hidden`/`--exclude`/
    /// etc. affect the explanation the same way they'd affect a real query.
    #[command(flatten)]
    pub walk: WalkFlags,
}

/// Arguments for `querymatter completions [SHELL]`.
///
/// `shell` is required unless `--install` is given — enforced here via
/// `required_unless_present` so a bare `completions` fails fast with clap's
/// own clear usage error, rather than `run_completions` (`main.rs`)
/// discovering the missing shell itself. With `--install` and no shell,
/// `run_completions` auto-detects one from `$SHELL`.
#[derive(Debug, Args)]
pub struct CompletionsArgs {
    /// Shell to generate a completion script for; auto-detected from `$SHELL`
    /// when omitted together with `--install`.
    #[arg(value_enum, required_unless_present = "install")]
    pub shell: Option<clap_complete::Shell>,

    /// Write the script directly into the shell's user completion directory
    /// (bash, zsh, fish) instead of printing it to stdout.
    #[arg(long)]
    pub install: bool,
}

impl Cli {
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

/// Canonicalizes each of `dirs` (resolving symlinks and absolutizing) —
/// falling back to the current directory when `dirs` is empty — and dedups
/// exact-equal canonical roots so scanning the same directory twice (e.g.
/// `querymatter . ./plans` where both resolve to the same place) doesn't
/// double every count. Overlapping-but-unequal roots — a parent and its
/// descendant — are left as-is (see the README caveat).
///
/// Canonicalizing here, at the CLI boundary, matters because
/// [`discover`](crate::discover)'s exclude matching assumes an absolute root
/// and [`RecordStore::reload_dir`](crate::store::RecordStore::reload_dir)
/// keys slices by path equality; doing it once keeps both consistent. A
/// missing or inaccessible directory is a hard error that names the
/// offending path.
///
/// [`crate::main::build_session`]'s live-scan branch calls this with
/// [`Cli::dirs`] (query mode's own `[DIRS]`) or `query run <name> [DIR]`'s
/// single-directory override — the same canonicalize/dedup rules either way.
pub(crate) fn canonicalize_roots(dirs: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    let raw = if dirs.is_empty() {
        vec![env::current_dir().context("failed to determine the current directory")?]
    } else {
        dirs.to_vec()
    };
    let mut roots = Vec::with_capacity(raw.len());
    for dir in raw {
        let canonical = fs::canonicalize(&dir)
            .with_context(|| format!("cannot access directory {}", dir.display()))?;
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::{CacheAction, Cli, Command, ConfigAction, QueryAction, canonicalize_roots};
    use crate::cache::Freshness;
    use crate::config::ConfigKey;
    use crate::render::{Format, TableStyle};
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
    fn canonicalize_roots_dedups_exact_duplicate_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().to_str().unwrap();
        // The same directory passed twice must collapse to a single canonical
        // root, so counts aren't doubled.
        let cli = parse(&["querymatter", path, path]);
        let roots = canonicalize_roots(&cli.dirs).unwrap();
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

    /// Regression: adding `clap::ValueEnum` to `Format` switched `--format`
    /// from a `FromStr`-based parser (which accepts `markdown` as an alias
    /// for `md`) to the `ValueEnum` parser, which by default knows only
    /// canonical spellings. This goes through clap's real parsing, not
    /// `Format::from_str` directly — that's the layer the regression lived
    /// in and the layer `FromStr`-only unit tests couldn't catch.
    #[test]
    fn format_flag_accepts_markdown_alias() {
        let cli = parse(&["querymatter", "--format", "markdown"]);
        assert_eq!(cli.format, Some(Format::Md));
    }

    #[test]
    fn config_list_parses() {
        match parse(&["querymatter", "config", "list"]).command {
            Some(Command::Config(args)) => assert!(matches!(args.action, ConfigAction::List)),
            other => panic!("expected a Config subcommand, got {other:?}"),
        }
    }

    #[test]
    fn config_set_parses_key_and_value() {
        match parse(&["querymatter", "config", "set", "table_style", "unicode"]).command {
            Some(Command::Config(args)) => match args.action {
                ConfigAction::Set { key, value } => {
                    assert_eq!(key, ConfigKey::TableStyle);
                    assert_eq!(value, "unicode");
                }
                other => panic!("expected Set, got {other:?}"),
            },
            other => panic!("expected a Config subcommand, got {other:?}"),
        }
    }

    #[test]
    fn config_get_and_unset_and_path_parse() {
        assert!(matches!(
            parse(&["querymatter", "config", "get", "format"]).command,
            Some(Command::Config(_))
        ));
        assert!(matches!(
            parse(&["querymatter", "config", "unset", "hidden"]).command,
            Some(Command::Config(_))
        ));
        assert!(matches!(
            parse(&["querymatter", "config", "path"]).command,
            Some(Command::Config(_))
        ));
    }

    /// Keys are spelled snake_case, matching the TOML file exactly.
    #[test]
    fn config_key_is_rejected_when_misspelled() {
        assert!(try_parse(&["querymatter", "config", "get", "table-style"]).is_err());
        assert!(try_parse(&["querymatter", "config", "get", "bogus"]).is_err());
    }

    #[test]
    fn cache_status_parses_with_and_without_dir() {
        match parse(&["querymatter", "cache", "status"]).command {
            Some(Command::Cache(args)) => match args.action {
                CacheAction::Status { dir } => assert_eq!(dir, None),
            },
            other => panic!("expected a Cache subcommand, got {other:?}"),
        }
        match parse(&["querymatter", "cache", "status", "somedir"]).command {
            Some(Command::Cache(args)) => match args.action {
                CacheAction::Status { dir } => {
                    assert_eq!(dir.as_deref(), Some(Path::new("somedir")));
                }
            },
            other => panic!("expected a Cache subcommand, got {other:?}"),
        }
    }

    #[test]
    fn query_save_parses_name_and_sql() {
        match parse(&["querymatter", "query", "save", "stale", "SELECT status"]).command {
            Some(Command::Query(args)) => match args.action {
                QueryAction::Save { name, sql } => {
                    assert_eq!(name, "stale");
                    assert_eq!(sql, "SELECT status");
                }
                other => panic!("expected Save, got {other:?}"),
            },
            other => panic!("expected a Query subcommand, got {other:?}"),
        }
    }

    #[test]
    fn query_list_get_run_delete_parse() {
        assert!(matches!(
            parse(&["querymatter", "query", "list"]).command,
            Some(Command::Query(_))
        ));
        match parse(&["querymatter", "query", "get", "stale"]).command {
            Some(Command::Query(args)) => match args.action {
                QueryAction::Get { name } => assert_eq!(name, "stale"),
                other => panic!("expected Get, got {other:?}"),
            },
            other => panic!("expected a Query subcommand, got {other:?}"),
        }
        match parse(&["querymatter", "query", "run", "stale"]).command {
            Some(Command::Query(args)) => match args.action {
                QueryAction::Run { name, dir } => {
                    assert_eq!(name, "stale");
                    assert_eq!(dir, None);
                }
                other => panic!("expected Run, got {other:?}"),
            },
            other => panic!("expected a Query subcommand, got {other:?}"),
        }
        match parse(&["querymatter", "query", "delete", "stale"]).command {
            Some(Command::Query(args)) => match args.action {
                QueryAction::Delete { name } => assert_eq!(name, "stale"),
                other => panic!("expected Delete, got {other:?}"),
            },
            other => panic!("expected a Query subcommand, got {other:?}"),
        }
    }

    /// FOLD-IN: `query run <name> [DIR]` parses the optional trailing
    /// directory positional.
    #[test]
    fn query_run_parses_an_optional_dir() {
        match parse(&["querymatter", "query", "run", "stale", "somedir"]).command {
            Some(Command::Query(args)) => match args.action {
                QueryAction::Run { name, dir } => {
                    assert_eq!(name, "stale");
                    assert_eq!(dir.as_deref(), Some(Path::new("somedir")));
                }
                other => panic!("expected Run, got {other:?}"),
            },
            other => panic!("expected a Query subcommand, got {other:?}"),
        }
    }

    #[test]
    fn explain_subcommand_parses_path_and_walk_flags() {
        let cli = parse(&[
            "querymatter",
            "explain",
            "notes/a.md",
            "--hidden",
            "--ext",
            "md,mdx",
        ]);
        match cli.command {
            Some(Command::Explain(args)) => {
                assert_eq!(args.path, Path::new("notes/a.md"));
                assert!(args.walk.hidden);
                assert_eq!(
                    args.walk.ext,
                    Some(vec!["md".to_string(), "mdx".to_string()])
                );
            }
            other => panic!("expected an Explain subcommand, got {other:?}"),
        }
    }

    #[test]
    fn completions_parses_each_shell() {
        for shell in ["bash", "zsh", "fish", "elvish", "powershell"] {
            match parse(&["querymatter", "completions", shell]).command {
                Some(Command::Completions(_)) => {}
                other => panic!("expected Completions for {shell}, got {other:?}"),
            }
        }
    }

    #[test]
    fn completions_rejects_an_unknown_shell() {
        assert!(try_parse(&["querymatter", "completions", "tcsh"]).is_err());
    }

    /// `--install` makes the shell optional (`main.rs` auto-detects it from
    /// `$SHELL` when it's left out this way).
    #[test]
    fn completions_install_without_a_shell_parses() {
        match parse(&["querymatter", "completions", "--install"]).command {
            Some(Command::Completions(args)) => {
                assert!(args.install);
                assert_eq!(args.shell, None);
            }
            other => panic!("expected Completions, got {other:?}"),
        }
    }

    /// Without `--install`, a shell is still required — clap rejects a bare
    /// `completions` up front rather than leaving `run_completions` to
    /// discover the missing shell itself.
    #[test]
    fn completions_with_neither_shell_nor_install_is_rejected() {
        assert!(try_parse(&["querymatter", "completions"]).is_err());
    }
}
