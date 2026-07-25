//! `querymatter` entry point: parse the CLI, then either build a
//! `.querymatter` cache (`init`) or run a query — loading the record store
//! from an ancestor cache when one is found, or live-scanning otherwise — and
//! dispatch to one-shot, batch, or interactive mode.
//!
//! Output discipline: **stdout carries data** — query results, `config
//! list`/`get`/`path` output, and completion scripts. Every diagnostic,
//! warning, confirmation, and prompt goes to stderr, so a pipeline like
//! `querymatter -e '…' --format json | jq` sees pure JSON.

pub mod cache;
mod cli;
mod config;
pub mod discover;
pub mod frontmatter;
mod gitignore;
pub mod model;
mod output;
mod parallel;
mod queries;
pub mod query;
pub mod render;
mod repl;
mod session;
mod settings;
pub mod store;

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::{ArgMatches, CommandFactory, FromArgMatches};
use directories::BaseDirs;

use crate::cache::CacheSummary;
use crate::cli::{
    CacheAction, CacheArgs, Cli, Command, CompletionsArgs, ConfigAction, ConfigArgs, ExplainArgs,
    InitArgs, QueryAction, QueryArgs,
};
use crate::config::Config;
use crate::output::OutputSink;
use crate::query::ast::SelectExpr;
use crate::session::{Session, split_statements};
use crate::settings::Settings;
use crate::store::{InMemoryStore, RecordStore};

/// Parses arguments, dispatches, and maps the outcome to a process exit code.
///
/// `--exit-code` only ever changes the code chosen here — never what
/// [`dispatch`] prints — and only applies to query mode and `query run`
/// ([`is_query_like`]): an `init`/`config`/`query save`/`query list`/`query
/// get`/`query delete`/`completions` error always exits 1, matching today's
/// behavior, since `--exit-code`'s grep-style 0/1/2 mapping is a query-result
/// concept those subcommands have no analog for — `query run` is the one
/// subcommand that genuinely is a query (it resolves a saved name to SQL and
/// runs it the same way `-e` does), so it participates like query mode does.
fn main() -> ExitCode {
    let matches = Cli::command().get_matches();
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(cli) => cli,
        Err(err) => {
            eprintln!("querymatter: {err:#}");
            return ExitCode::from(1);
        }
    };
    let query_mode = is_query_like(&cli.command);
    match dispatch(&cli, &matches) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("querymatter: {err:#}");
            if cli.exit_code && query_mode {
                ExitCode::from(2)
            } else {
                ExitCode::from(1)
            }
        }
    }
}

/// Whether `command` is a query-result concept `--exit-code`'s grep-style
/// 0/1/2 error mapping applies to: no subcommand (ordinary query mode) or
/// `query run` (which resolves a saved name to SQL and runs it exactly like
/// `-e` would, via [`run_query_run`]). Every other subcommand keeps today's
/// plain "error exits 1" behavior.
fn is_query_like(command: &Option<Command>) -> bool {
    matches!(
        command,
        None | Some(Command::Query(QueryArgs {
            action: QueryAction::Run { .. },
        }))
    )
}

/// Runs the parsed command and reports its exit code.
///
/// A query-mode success already carries the right code (0, or 1 under
/// `--exit-code` when no row matched); every other success is
/// [`ExitCode::SUCCESS`]. Any error is left to propagate — `main` prints it
/// once and picks 1 vs. 2 uniformly, so callers here just use `?`.
fn dispatch(cli: &Cli, matches: &ArgMatches) -> anyhow::Result<ExitCode> {
    // Completions, `config path`, and every config-free `query` action must
    // all work even with a broken config file: completions is how a user
    // installs the completion that helps them type `config set` correctly,
    // `config path` is the one command a user with a broken config reaches
    // for to find the file worth fixing, and `query save`/`list`/`get`/
    // `delete` operate on the SEPARATE `queries.toml` file and never read
    // `config.toml` at all (see [`run_query_action`]). None of these need
    // config *content*, so all are dispatched here, before `config::load()`
    // can fail on them. `query run` DOES build a session — it is dispatched
    // below, after config loads, like every other config-needing command.
    match &cli.command {
        Some(Command::Completions(args)) => {
            run_completions(args)?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(Command::Config(ConfigArgs {
            action: ConfigAction::Path,
        })) => {
            run_config_path()?;
            return Ok(ExitCode::SUCCESS);
        }
        Some(Command::Query(QueryArgs { action }))
            if !matches!(action, QueryAction::Run { .. }) =>
        {
            return run_query_action(action);
        }
        _ => {}
    }
    let config = config::load()?;
    match &cli.command {
        Some(Command::Init(args)) => {
            // `init`'s walk flags are matched under the "init" subcommand's
            // own nested `ArgMatches`, not the top-level one: `WalkFlags` is
            // flattened separately onto `Cli` and `InitArgs`, so each gets
            // its own arg registration, and `ArgMatches::value_source` only
            // sees the level it's called on. Handing `Settings::resolve_walk`
            // the top-level `matches` here would read every init walk flag's
            // source as absent, silently dropping `--hidden` and friends.
            let sub_matches = matches
                .subcommand_matches("init")
                .expect("Command::Init parsed implies the init subcommand matched");
            run_init(args, &config, sub_matches)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Config(args)) => {
            run_config(&args.action, cli, &config, matches)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Cache(args)) => {
            run_cache(args)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Query(QueryArgs {
            action: QueryAction::Run { name, dir },
        })) => run_query_run(name, dir.as_deref(), cli, &config, matches),
        Some(Command::Query(_)) => unreachable!("non-Run actions are dispatched above"),
        Some(Command::Explain(args)) => {
            // Same reasoning as `init` above: `ExplainArgs`'s flattened
            // `WalkFlags` is matched under the "explain" subcommand's own
            // nested `ArgMatches`, not the top-level one.
            let sub_matches = matches
                .subcommand_matches("explain")
                .expect("Command::Explain parsed implies the explain subcommand matched");
            run_explain(args, &config, sub_matches)?;
            Ok(ExitCode::SUCCESS)
        }
        Some(Command::Completions(_)) => unreachable!("handled above"),
        None => run_query(cli, &config, matches),
    }
}

/// Writes a shell completion script for `args.shell` to stdout — or, with
/// `--install`, straight into that shell's user completion directory.
///
/// The plain (no `--install`) path is exactly what this did before `--install`
/// existed: the script is data, so it goes to stdout for redirection into the
/// shell's completion directory (see the README).
///
/// A shell is required either way. When `args.shell` is absent — only
/// possible together with `--install`, since [`CompletionsArgs::shell`] is
/// otherwise `required_unless_present` — it's auto-detected from `$SHELL` via
/// [`detect_shell`]. Failing that IS a hard error: with no shell known at
/// all, there is no script to produce, install or otherwise.
fn run_completions(args: &CompletionsArgs) -> anyhow::Result<()> {
    let mut command = Cli::command();
    let bin_name = command.get_name().to_string();

    let shell = match args.shell {
        Some(shell) => shell,
        None => detect_shell().ok_or_else(|| {
            anyhow::anyhow!(
                "completions --install needs a shell: $SHELL is unset or not one this crate \
                 recognizes; pass one explicitly, e.g. `completions --install bash`"
            )
        })?,
    };

    if args.install {
        install_completions(shell, &mut command, &bin_name);
    } else {
        clap_complete::generate(shell, &mut command, bin_name, &mut io::stdout());
    }
    Ok(())
}

/// Auto-detects the user's shell from `$SHELL` (e.g. `/bin/zsh` → `Zsh`), for
/// `completions --install` with no shell named explicitly. `None` when
/// `$SHELL` is unset or names a shell this crate doesn't generate completions
/// for.
fn detect_shell() -> Option<clap_complete::Shell> {
    let shell_path = env::var_os("SHELL")?;
    match Path::new(&shell_path).file_name()?.to_str()? {
        "bash" => Some(clap_complete::Shell::Bash),
        "zsh" => Some(clap_complete::Shell::Zsh),
        "fish" => Some(clap_complete::Shell::Fish),
        _ => None,
    }
}

/// Writes `shell`'s completion script into its user completion directory
/// under the real home directory ([`BaseDirs`], which itself honors `$HOME` —
/// see its docs — so tests can redirect it exactly like they already do for
/// config).
///
/// `--install` is pure convenience, so nothing here is a hard error: a shell
/// with no known install directory ([`install_path`] returning `None` — e.g.
/// `elvish`/`powershell`), an undeterminable home directory, or a
/// `create_dir_all`/write failure (e.g. permissions) all print a clear reason
/// to stderr and fall back to printing the script to stdout — today's
/// behavior — so the caller is never left empty-handed.
fn install_completions(shell: clap_complete::Shell, command: &mut clap::Command, bin_name: &str) {
    let Some(base_dirs) = BaseDirs::new() else {
        eprintln!(
            "querymatter: --install could not determine your home directory; \
             printing the script to stdout instead"
        );
        clap_complete::generate(shell, command, bin_name, &mut io::stdout());
        return;
    };
    let Some(path) = install_path(shell, base_dirs.home_dir(), bin_name) else {
        eprintln!(
            "querymatter: --install has no known completion directory for {shell}; \
             printing the script to stdout instead"
        );
        clap_complete::generate(shell, command, bin_name, &mut io::stdout());
        return;
    };

    if let Err(err) = write_completion_script(shell, command, bin_name, &path) {
        eprintln!(
            "querymatter: could not install completions to {} ({err}); printing the script to \
             stdout instead",
            path.display()
        );
        clap_complete::generate(shell, command, bin_name, &mut io::stdout());
        return;
    }
    eprintln!(
        "querymatter: installed {shell} completions to {}",
        path.display()
    );
}

/// The path `shell`'s completion script installs to under `home`, when this
/// crate knows a user completion directory for it: bash uses
/// `~/.local/share/bash-completion/completions/<bin>`; zsh uses
/// `~/.zsh/completions/_<bin>` — an fpath directory of our own, since not
/// every distro's default `$fpath` entry is user-writable (see the README);
/// fish uses `~/.config/fish/completions/<bin>.fish`. `clap_complete::Shell`
/// also includes `elvish`/`powershell`, which have no such convention to
/// target — `None` there tells [`install_completions`] to fall back to
/// stdout.
fn install_path(shell: clap_complete::Shell, home: &Path, bin_name: &str) -> Option<PathBuf> {
    match shell {
        clap_complete::Shell::Bash => Some(
            home.join(".local/share/bash-completion/completions")
                .join(bin_name),
        ),
        clap_complete::Shell::Zsh => {
            Some(home.join(".zsh/completions").join(format!("_{bin_name}")))
        }
        clap_complete::Shell::Fish => Some(
            home.join(".config/fish/completions")
                .join(format!("{bin_name}.fish")),
        ),
        _ => None,
    }
}

/// `create_dir_all`s the target's parent directory, then writes the
/// generated script — split out of [`install_completions`] so its one
/// fallible step has a single error type (`io::Error`) to match on, rather
/// than `create_dir_all` and `fs::write` needing separate handling.
fn write_completion_script(
    shell: clap_complete::Shell,
    command: &mut clap::Command,
    bin_name: &str,
    path: &Path,
) -> io::Result<()> {
    let parent = path
        .parent()
        .expect("install_path always returns a path with a parent");
    fs::create_dir_all(parent)?;
    let mut script = Vec::new();
    clap_complete::generate(shell, command, bin_name, &mut script);
    fs::write(path, script)
}

/// Discovers and loads the vault-level `.querymatter.toml` a team can commit
/// at the vault root (design W54): walks up from `start` via
/// [`cache::find_vault_config`], independent of whether a `.querymatter/`
/// cache directory exists anywhere in that ancestry.
///
/// No such file found is not an error — it's simply absent, so
/// [`Settings::resolve`]/[`Settings::resolve_walk`] fall straight through to
/// the per-user config/default layers, exactly as if this feature didn't
/// exist. A file that IS found but fails to parse is a hard error naming its
/// path, via the same [`config::load_from`] the per-user config file goes
/// through — reusing its schema and its "reject loudly" behavior rather than
/// a second, easily-drifting parser.
fn load_vault_config(start: &Path) -> anyhow::Result<Config> {
    match cache::find_vault_config(start) {
        Some(path) => config::load_from(&path),
        None => Ok(Config::default()),
    }
}

/// Like [`load_vault_config`], discovering from the current directory — for
/// callers with no other scan root to prefer, e.g. `querymatter config
/// list`/`get`, which aren't tied to a directory the way a query or `init` is.
fn load_vault_config_from_cwd() -> anyhow::Result<Config> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    load_vault_config(&cwd)
}

/// Builds a `.querymatter` cache under the requested directory (or the cwd),
/// honoring the shared walk flags, and prints a one-line summary to stderr.
///
/// All summary output goes to stderr; `init` produces no stdout so it composes
/// cleanly in scripts.
fn run_init(args: &InitArgs, config: &Config, matches: &ArgMatches) -> anyhow::Result<()> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let target = args.dir.clone().unwrap_or(cwd);
    // Discovered from `target` — the directory `init` is about to make the
    // vault root of — rather than `cwd`, so `querymatter init --dir
    // ../other-vault` finds `../other-vault`'s own `.querymatter.toml`
    // ancestry, not the invoking shell's.
    let vault_config = load_vault_config(&target)?;

    let settings = Settings::resolve_walk(&args.walk, &vault_config, config, matches);
    // Validated on the RESOLVED exclude list (flag, vault, config, or
    // default — whichever won), not `args.walk.exclude` alone, so a bad glob
    // from a hand-edited config file is caught too, not just one typed on the
    // command line (IMPORTANT 1).
    discover::validate_excludes(&settings.exclude.value)?;

    let base = fs::canonicalize(&target)
        .with_context(|| format!("cannot access directory {}", target.display()))?;

    let mut opts = settings.walk_opts();
    opts.ignore_files = args.walk.ignore_files()?;

    let report = cache::build_vault(&base, &opts, args.ttl)?;

    // The cache build already succeeded; a prompt hiccup (e.g. a stdin read
    // error) must not fail the command, so the git-ignore offer is
    // best-effort — downgraded to a warning rather than propagated.
    if let Err(err) = offer_gitignore(&base) {
        eprintln!("querymatter: warning: {err:#}");
    }

    eprintln!(
        "querymatter: cached {} file(s) under {} ({} skipped)",
        report.loaded,
        base.display(),
        report.skipped
    );
    Ok(())
}

/// Offers to add `.querymatter/` to the enclosing git repo's `.gitignore`
/// (design spec §7). A no-op outside a git working tree, or when
/// `.querymatter` is already ignored. Otherwise: an interactive TTY gets the
/// yes/no prompt; a piped/non-interactive stdin gets a one-line stderr hint
/// instead, and `.gitignore` is left untouched.
fn offer_gitignore(base: &Path) -> anyhow::Result<()> {
    let Some(root) = gitignore::git_root(base) else {
        return Ok(());
    };
    if gitignore::querymatter_ignored(&root) {
        return Ok(());
    }

    if io::stdin().is_terminal() {
        prompt_add_gitignore(&root)
    } else {
        eprintln!("hint: add .querymatter/ to .gitignore");
        Ok(())
    }
}

/// Prompts on stderr and reads one line of stdin; on an affirmative
/// `y`/`yes` answer (case-insensitive, trimmed), appends `.querymatter/` to
/// `root`'s `.gitignore` and confirms on stderr. Any other answer leaves
/// `.gitignore` untouched; a stdin read failure propagates as an error, which
/// [`run_init`] downgrades to a non-fatal warning (the cache is already built).
fn prompt_add_gitignore(root: &Path) -> anyhow::Result<()> {
    eprint!("Add .querymatter/ to .gitignore? [y/N] ");
    io::stderr()
        .flush()
        .context("failed to flush the git-ignore prompt")?;

    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .context("failed to read the git-ignore prompt answer")?;

    if matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
        gitignore::append_gitignore(root)?;
        eprintln!(
            "querymatter: added .querymatter/ to {}",
            root.join(".gitignore").display()
        );
    }
    Ok(())
}

/// Reports whether `args.path` would be discovered under the walk
/// configuration resolved from `args.walk`/`config`, and — when it would be
/// excluded — which filter layer is responsible, via [`discover::explain`].
/// The verdict/reason line is printed to stdout: it's the command's data,
/// not a diagnostic.
///
/// Root resolution: `explain` roots at the ancestor `.querymatter` vault when
/// one is found via [`cache::find_vault`], or the current directory
/// otherwise — the SAME resolution [`build_session`] applies to a real query
/// with no `[DIRS]` given, so "explain this path" always agrees with what a
/// real query run right here would actually scan. (`explain` cannot itself
/// accept a `[DIRS]` positional — see [`ExplainArgs`]'s doc comment — so
/// there is no user-supplied root to prefer over the vault.) `args.path` is
/// canonicalized and checked against that root; a path that doesn't exist,
/// or resolves outside it, is a clean error rather than a silently wrong
/// verdict.
fn run_explain(args: &ExplainArgs, config: &Config, matches: &ArgMatches) -> anyhow::Result<()> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let root = match cache::find_vault(&cwd) {
        Some(vault) => vault,
        None => fs::canonicalize(&cwd)
            .with_context(|| format!("cannot access directory {}", cwd.display()))?,
    };
    // Discovered from `root` — the same vault-or-cwd `explain` scans against
    // below — rather than `cwd`, so a vault-rooted `.querymatter.toml` is
    // found even when `explain` is invoked from a subdirectory of the vault.
    let vault_config = load_vault_config(&root)?;

    let settings = Settings::resolve_walk(&args.walk, &vault_config, config, matches);
    discover::validate_excludes(&settings.exclude.value)?;

    let mut opts = settings.walk_opts();
    opts.ignore_files = args.walk.ignore_files()?;

    let target = fs::canonicalize(&args.path)
        .with_context(|| format!("cannot access path {}", args.path.display()))?;
    anyhow::ensure!(
        target.starts_with(&root),
        "{} is outside {} (explain's scan root: the enclosing .querymatter vault, or the \
         current directory when there is none)",
        args.path.display(),
        root.display()
    );

    match discover::explain(&root, &target, &opts) {
        discover::Explanation::Included => println!("included"),
        discover::Explanation::Excluded(reason) => println!("excluded: {reason}"),
    }
    Ok(())
}

/// Runs a `querymatter config` action.
///
/// Output discipline: the data (`list` rows, `get`'s value, `path`) goes to
/// stdout so it can be piped; `set`/`unset` confirmations go to stderr,
/// matching `init`'s no-stdout convention.
fn run_config(
    action: &ConfigAction,
    cli: &Cli,
    config: &Config,
    matches: &ArgMatches,
) -> anyhow::Result<()> {
    match action {
        ConfigAction::List => {
            let vault_config = load_vault_config_from_cwd()?;
            println!(
                "{}",
                Settings::resolve(cli, &vault_config, config, matches).rows()
            );
        }
        ConfigAction::Get { key } => {
            let vault_config = load_vault_config_from_cwd()?;
            let settings = Settings::resolve(cli, &vault_config, config, matches);
            println!("{}", settings.value_of(*key));
            println!("values: {}", key.allowed());
        }
        ConfigAction::Set { key, value } => {
            let mut updated = config.clone();
            config::set(&mut updated, *key, value)?;
            let path = config::save(&updated)?;
            eprintln!(
                "querymatter: set {} = {value} in {}",
                key.as_str(),
                path.display()
            );
        }
        ConfigAction::Unset { key } => {
            // A key that is already absent is a no-op: writing the file (and
            // creating its parent directory) for nothing would surprise a
            // user who has never run `config set` at all.
            if config::get(config, *key).is_some() {
                let mut updated = config.clone();
                config::unset(&mut updated, *key);
                let path = config::save(&updated)?;
                eprintln!(
                    "querymatter: removed {} from {}",
                    key.as_str(),
                    path.display()
                );
            } else {
                let path = config::config_path()
                    .context("cannot determine a config directory for this user")?;
                eprintln!(
                    "querymatter: {} was not set in {}",
                    key.as_str(),
                    path.display()
                );
            }
        }
        ConfigAction::Path => unreachable!("dispatched in main before config::load"),
    }
    Ok(())
}

/// Prints the config file's path to stdout, whether or not it exists or is
/// valid. Dispatched from `main` before `config::load()` (alongside
/// completions), since `config path` needs no config *content* — only its
/// location — and must keep working even when the file is malformed, matching
/// the README's promise that a broken config is always recoverable via
/// `querymatter config path`.
fn run_config_path() -> anyhow::Result<()> {
    let path =
        config::config_path().context("cannot determine a config directory for this user")?;
    println!("{}", path.display());
    Ok(())
}

/// Runs a `querymatter cache` action; only `status` exists today.
fn run_cache(args: &CacheArgs) -> anyhow::Result<()> {
    match &args.action {
        CacheAction::Status { dir } => run_cache_status(dir.as_deref()),
    }
}

/// Reports the `.querymatter` cache's health for the vault enclosing `dir`
/// (or the cwd when `dir` is absent): its location, size, counts, TTL, and
/// each cached directory's last scan time. Printed to stdout — inspection
/// data, matching `config list`'s convention. Errors, naming `querymatter
/// init`, both when no vault is found at or above the resolved directory
/// and when one is found but its `manifest.bin` is unreadable (corrupt, or
/// written by an incompatible crate version) — [`cache::cache_summary`]'s
/// own error doesn't mention `init`, so this wraps it with context that
/// does.
fn run_cache_status(dir: Option<&Path>) -> anyhow::Result<()> {
    let start = match dir {
        Some(dir) => fs::canonicalize(dir)
            .with_context(|| format!("cannot access directory {}", dir.display()))?,
        None => env::current_dir().context("failed to determine the current directory")?,
    };
    let vault = cache::find_vault(&start).ok_or_else(|| {
        anyhow::anyhow!(
            "no .querymatter cache found at or above {} (run `querymatter init` first)",
            start.display()
        )
    })?;
    let summary = cache::cache_summary(&vault).with_context(|| {
        format!(
            "cache at {} may be corrupt or from an incompatible querymatter version \
             (run `querymatter init` to rebuild it)",
            cache::cache_dir(&vault).display()
        )
    })?;
    println!("{}", render_cache_summary(&summary));
    Ok(())
}

/// Renders a [`CacheSummary`] exactly as `querymatter cache status` prints
/// it: aligned key/value fields (location, counts, size, TTL, crate
/// version), followed by each cached directory's last scan time in the
/// order [`CacheSummary::dirs`] already carries (`manifest.bin`'s own
/// order).
fn render_cache_summary(summary: &CacheSummary) -> String {
    let cache_dir = cache::cache_dir(&summary.root);
    let mut lines = vec![
        format!("root:        {}", summary.root.display()),
        format!("cache dir:   {}", cache_dir.display()),
        format!("directories: {}", summary.dir_count),
        format!("files:       {}", summary.file_count),
        format!("size:        {}", human_bytes(summary.bytes)),
        format!("ttl:         {}s", summary.ttl_secs),
        format!("built with:  querymatter {}", summary.crate_version),
    ];

    if !summary.dirs.is_empty() {
        lines.push(String::new());
        lines.push("directories scanned:".to_string());
        let width = summary
            .dirs
            .iter()
            .map(|(dir, _)| dir.display().to_string().len())
            .max()
            .unwrap_or(0);
        for (dir, scanned_at) in &summary.dirs {
            lines.push(format!(
                "  {:width$}  {}",
                dir.display(),
                format_scan_time(*scanned_at)
            ));
        }
    }

    lines.join("\n")
}

/// Renders `bytes` human-readably: whole bytes below 1 KiB, one decimal
/// place for KiB, and one decimal place for MiB and above — a `.querymatter`
/// cache is not expected to reach GiB scale, so there's no step past MiB.
fn human_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    let bytes_f = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes_f < MIB {
        format!("{:.1} KiB", bytes_f / KIB)
    } else {
        format!("{:.1} MiB", bytes_f / MIB)
    }
}

/// Renders a cached directory's scan time as `YYYY-MM-DD HH:MM` UTC, matching
/// [`crate::model::system_time_to_iso`]'s UTC convention for on-disk
/// timestamps.
fn format_scan_time(t: SystemTime) -> String {
    DateTime::<Utc>::from(t)
        .format("%Y-%m-%d %H:%M")
        .to_string()
}

/// Runs a config-free `querymatter query` action: `save`/`list`/`get`/
/// `delete` operate only on the `queries.toml` file (see [`queries`]) and
/// never build a record store — matching `config`'s stdout/stderr split,
/// data goes to stdout, confirmations and errors to stderr.
///
/// Dispatched from [`dispatch`] BEFORE `config::load()` (see its doc
/// comment); `Run` is handled separately by [`run_query_run`], after config
/// loads, since it's the one action that builds a session.
fn run_query_action(action: &QueryAction) -> anyhow::Result<ExitCode> {
    match action {
        QueryAction::Save { name, sql } => {
            let path = save_named_query(name, sql)?;
            eprintln!("querymatter: saved query '{name}' in {}", path.display());
            Ok(ExitCode::SUCCESS)
        }
        QueryAction::List => {
            let saved = queries::load()?;
            for (name, sql) in saved.iter() {
                println!("{name}\t{sql}");
            }
            Ok(ExitCode::SUCCESS)
        }
        QueryAction::Get { name } => {
            let saved = queries::load()?;
            let sql = queries::get(&saved, name)
                .with_context(|| format!("no saved query named '{name}'"))?;
            println!("{sql}");
            Ok(ExitCode::SUCCESS)
        }
        QueryAction::Delete { name } => {
            let mut saved = queries::load()?;
            // Absent is reported, not an error, matching `config unset`'s
            // no-op-on-absent-key behavior.
            if queries::remove(&mut saved, name) {
                let path = queries::save(&saved)?;
                eprintln!(
                    "querymatter: deleted query '{name}' from {}",
                    path.display()
                );
            } else {
                let path = queries::queries_path()
                    .context("cannot determine a config directory for this user")?;
                eprintln!(
                    "querymatter: no saved query named '{name}' in {}",
                    path.display()
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        QueryAction::Run { .. } => {
            unreachable!("Run is dispatched separately, after config::load — see run_query_run")
        }
    }
}

/// Validates and persists `sql` under `name` in the user's saved-queries
/// file: rejects `sql` up front (naming the parse error), so a saved query
/// can never be a broken one that only fails later at `query run`, then
/// rejects `name` per [`queries::set`]'s allowed-character rule, then loads
/// the current file, inserts (overwriting any existing SQL already saved
/// under the same name), and saves it back.
///
/// Shared by the CLI's `query save` ([`run_query_action`]'s `Save` arm) and
/// the REPL's `.query save` ([`repl`]'s dispatch), so the two surfaces can
/// never validate or persist differently.
pub(crate) fn save_named_query(name: &str, sql: &str) -> anyhow::Result<PathBuf> {
    query::parse(sql).with_context(|| format!("failed to parse query: {sql}"))?;
    let mut saved = queries::load()?;
    queries::set(&mut saved, name, sql)?;
    queries::save(&saved)
}

/// Runs `query run <name> [DIR]`: resolves `name` to its saved SQL — a clean
/// error naming `name` when nothing is saved under it — and executes it
/// through the SAME [`build_session`]/[`run_batch`] machinery `-e` uses (fed
/// the resolved SQL as the query text), so it honors `--format`/
/// `--table-style`/`--output`/`--exit-code` and the walk flags exactly like a
/// one-shot query would, rather than a parallel, easily-drifting
/// implementation. `dir`, when given, overrides the scan root exactly like a
/// single positional `[DIRS]` entry would in query mode. Dispatched from
/// [`dispatch`] after `config::load()`, since (unlike its `query` siblings)
/// it builds a session.
///
/// `name`'s resolved SQL is known before the store is built, so — like `-e`
/// and piped batch mode — it computes projection push-down's `wanted` field
/// set ([`compute_wanted`]) up front and threads it into [`build_session`].
fn run_query_run(
    name: &str,
    dir: Option<&Path>,
    cli: &Cli,
    config: &Config,
    matches: &ArgMatches,
) -> anyhow::Result<ExitCode> {
    let saved = queries::load()?;
    let sql =
        queries::get(&saved, name).with_context(|| format!("no saved query named '{name}'"))?;
    let dirs: Vec<PathBuf> = dir.map(Path::to_path_buf).into_iter().collect();
    let wanted = compute_wanted(sql);
    let session = build_session(cli, config, matches, &dirs, wanted.as_ref())?;
    run_batch(&session, sql, cli.exit_code, cli.output.as_deref())
}

/// Runs a query: loads the store from an ancestor `.querymatter` cache when one
/// is found (unless `--no-cache`), or live-scans the resolved roots otherwise,
/// then dispatches to one-shot, batch, or interactive mode.
///
/// One-shot and batch runs (`-e`, or piped stdin) map to an exit code under
/// `--exit-code`: [`ExitCode::SUCCESS`] when the statements' row counts sum to
/// more than zero, [`ExitCode::from(1)`] otherwise. Without the flag, or in
/// the interactive REPL — which has no single "total rows" to report — a
/// clean run is always [`ExitCode::SUCCESS`], matching today's behavior.
///
/// The store is built AFTER the query text is known for every branch except
/// the REPL: a one-shot `-e`/`-e -`/piped-batch run's statement text is
/// parsed up front to compute projection push-down's `wanted` field set
/// ([`compute_wanted`]), which [`build_session`] then threads into the
/// store's materialization. The REPL's store outlives any one query, so it
/// always builds with `wanted = None` (every field) — matching today's
/// behavior exactly. Reading stdin still only ever happens in the same
/// branch it always did (right before that branch's `run_batch`), so a
/// non-piped, `-e`-less invocation never blocks on stdin any earlier than
/// before.
fn run_query(cli: &Cli, config: &Config, matches: &ArgMatches) -> anyhow::Result<ExitCode> {
    let output = cli.output.as_deref();
    match cli.query.as_deref() {
        // `-e -`: read the query text from stdin, then run it.
        Some("-") => {
            let sql = read_stdin()?;
            let wanted = compute_wanted(&sql);
            let session = build_session(cli, config, matches, &cli.dirs, wanted.as_ref())?;
            run_batch(&session, &sql, cli.exit_code, output)
        }
        // `-e <sql>`: run the given query text.
        Some(sql) => {
            let wanted = compute_wanted(sql);
            let session = build_session(cli, config, matches, &cli.dirs, wanted.as_ref())?;
            run_batch(&session, sql, cli.exit_code, output)
        }
        // No `-e`: batch mode when stdin is piped, otherwise the REPL.
        None if !io::stdin().is_terminal() => {
            let input = read_stdin()?;
            let wanted = compute_wanted(&input);
            let session = build_session(cli, config, matches, &cli.dirs, wanted.as_ref())?;
            run_batch(&session, &input, cli.exit_code, output)
        }
        None => {
            let session = build_session(cli, config, matches, &cli.dirs, None)?;
            repl::run(session)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// Computes projection push-down's field set (design W17) for a one-shot/
/// batch/`query run` invocation, where every statement's SQL is known before
/// the record store is built: the union of [`Query::referenced_fields`]
/// across every statement in `input`.
///
/// Returns `None` — disabling push-down, so the store keeps every field
/// (today's behavior) — when any statement projects `SELECT *`
/// ([`SelectExpr::Star`], which needs every field), or when any statement
/// fails to parse. A parse failure is not reported here: the same text gets
/// parsed again, and its real error surfaced, when [`run_statements`]
/// actually runs it — silently keeping every field in that case just costs
/// the optimization, never correctness.
///
/// [`Query::referenced_fields`]: crate::query::ast::Query::referenced_fields
fn compute_wanted(input: &str) -> Option<BTreeSet<String>> {
    let mut wanted = BTreeSet::new();
    for statement in split_statements(input) {
        let parsed = query::parse(&statement.sql).ok()?;
        let has_star = parsed
            .select
            .iter()
            .any(|item| matches!(item.expr, SelectExpr::Star));
        if has_star {
            return None;
        }
        wanted.extend(parsed.referenced_fields());
    }
    Some(wanted)
}

/// Builds the queryable [`Session`] shared by [`run_query`] (`-e`/stdin/the
/// REPL) and [`run_query_run`] (a saved query): resolves settings, validates
/// the resolved exclude globs, loads the record store from an ancestor
/// `.querymatter` cache when one is found (unless `--no-cache`) or live-scans
/// the resolved roots otherwise, and applies any `--refresh`/`--refresh-all`/
/// `dirs` vault narrowing. Pulled out so `query run` reuses exactly this —
/// not a second, easily-drifting implementation of the same cache/walk/vault
/// logic.
///
/// `dirs` is the positional root list to resolve/narrow by — [`run_query`]
/// passes `cli.dirs` (query mode's own `[DIRS]`) and [`run_query_run`] passes
/// `query run <name> [DIR]`'s single-element override (or an empty slice when
/// `[DIR]` was omitted) — rather than this function reading `cli.dirs`
/// itself, so the two callers' independent positionals can't be conflated.
///
/// `wanted` is projection push-down's field set ([`compute_wanted`]),
/// threaded straight into the store's materialization
/// ([`InMemoryStore::load`]/[`InMemoryStore::from_cache`]); `None` (the REPL,
/// always) keeps every field, matching pre-push-down behavior.
///
/// Forced to `None` regardless of what was passed whenever `--refresh`/
/// `--refresh-all` is set: `RecordStore::refresh` always rebuilds slices with
/// `wanted = None` afterward (see its doc comment), so a pruned initial
/// `wanted` gives zero benefit on a refreshing run — and in the narrow case
/// where `refresh` falls back to reconstructing its on-disk cache from the
/// store's own (pruned) slices, a non-`None` `wanted` here would silently
/// persist out-of-subtree records with fields missing. Disabling push-down
/// on a refresh is free and closes that gap.
fn build_session(
    cli: &Cli,
    config: &Config,
    matches: &ArgMatches,
    dirs: &[PathBuf],
    wanted: Option<&BTreeSet<String>>,
) -> anyhow::Result<Session> {
    cli.validate()?;

    let wants_refresh = cli.refresh_all || !cli.refresh.is_empty();
    let wanted = if wants_refresh { None } else { wanted };

    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let vault_config = load_vault_config(&cwd)?;

    let settings = Settings::resolve(cli, &vault_config, config, matches);
    // Validated on the RESOLVED exclude list (flag, vault, config, or
    // default — whichever won), not `cli.walk.exclude` alone: a hand-edited
    // config file's `exclude` must be rejected here too. `config::set`
    // already rejects a bad glob up front for the normal `config set
    // exclude` path, but a hand-edited file bypasses that, and `discover`'s
    // own glob compiler has no error channel and would otherwise silently
    // drop it (IMPORTANT 1).
    discover::validate_excludes(&settings.exclude.value)?;
    let mut opts = settings.walk_opts();
    opts.ignore_files = cli.walk.ignore_files()?;

    let vault = if cli.no_cache {
        None
    } else {
        cache::find_vault(&cwd)
    };

    let (store, report, session_vault) = match vault {
        Some(vault) => {
            // Subtree scoping (design W26 / spec §7): when the query names a
            // subtree via positional `[DIRS]`, load ONLY that subtree from the
            // cache instead of loading the whole vault and narrowing after.
            // Gated on `wanted.is_some() && !dirs.is_empty()`:
            //
            // * `wanted.is_some()` is exactly the W17 push-down boundary
            //   (spec §7.2). The REPL always passes `wanted = None` — its
            //   store outlives a query and may later target a different
            //   subtree, so it must stay whole-vault — and a `--refresh`/
            //   `--refresh-all` run forces `wanted = None` above (its
            //   post-load `RecordStore::refresh` rebuilds every slice from the
            //   whole on-disk cache, which would undo a scoped load). Both
            //   therefore fall through to the unchanged whole-vault load +
            //   post-hoc `retain_under` path below: a refreshing run refreshes
            //   the WHOLE vault (`--refresh-all`) or its named target
            //   (`--refresh <path>`), then the RESULT is narrowed to `[DIRS]`
            //   — no record the user asked to refresh is dropped.
            // * A scoped load makes the store's schema the subtree's,
            //   narrowing W12 validation to it (spec §7.3, an accepted, tested
            //   behavior change).
            //
            // Correctness: `scope = None` always falls back to the proven
            // whole-load + `retain_under` path, so gating here can only ever
            // disable the optimization, never yield a wrong result.
            let scope = match wanted {
                Some(_) if !dirs.is_empty() => Some(canonicalize_dirs(dirs)?),
                _ => None,
            };
            let (mut store, mut report) =
                InMemoryStore::from_cache(&vault, opts, cli.freshness(), wanted, scope.as_deref());
            // A forced refresh runs against the just-loaded cache; only its
            // warnings need surfacing (the counts are informational and the
            // store already reflects the refreshed records).
            if cli.refresh_all {
                report.warnings.extend(store.refresh(&vault, None).warnings);
            } else {
                for path in &cli.refresh {
                    let target = cache::resolve_refresh_target(path, &vault)?;
                    report
                        .warnings
                        .extend(store.refresh(&vault, Some(&target)).warnings);
                }
            }
            // Spec §5: positional `[DIRS]` restrict a vault query to the named
            // subtrees. When `scope` is `None` the vault was loaded whole (the
            // REPL / refreshing path), so narrow here at slice granularity;
            // when `scope` is `Some` the store was built already scoped and
            // this is skipped. A dir entirely outside the vault matches no
            // slice (its records are absent) — v1 does not live-scan
            // outside-vault dirs, a known limitation.
            if scope.is_none() && !dirs.is_empty() {
                store.retain_under(&canonicalize_dirs(dirs)?);
            }
            (store, report, Some(vault))
        }
        None => {
            anyhow::ensure!(
                !cli.force_cache,
                "--force-cache: no .querymatter cache found (run `querymatter init` first)"
            );
            let (store, report) = InMemoryStore::load(cli::canonicalize_roots(dirs)?, opts, wanted);
            (store, report, None)
        }
    };

    if !settings.quiet.value {
        for warning in &report.warnings {
            eprintln!("querymatter: {warning}");
        }
    }
    // The user config layer removed, NOT the vault layer: `.unset` only ever
    // touches the per-user config file, so reverting a key must still honor
    // a vault-supplied value for it.
    let fallback = Settings::resolve(cli, &vault_config, &Config::default(), matches);
    Ok(Session::new(
        Box::new(store),
        settings,
        fallback,
        session_vault,
    ))
}

/// Runs `input` via [`run_statements`] and maps its total row count to an
/// exit code: 1 only when `exit_code` is set and no statement matched a row,
/// [`ExitCode::SUCCESS`] otherwise (including every run where the flag is
/// off, preserving today's always-zero-on-success behavior).
///
/// `output`, when set, is `--output`'s target file (see [`run_statements`]);
/// `None` keeps results on stdout, unchanged from before `--output` existed.
fn run_batch(
    session: &Session,
    input: &str,
    exit_code: bool,
    output: Option<&Path>,
) -> anyhow::Result<ExitCode> {
    let total_rows = run_statements(session, input, output)?;
    Ok(if exit_code && total_rows == 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Canonicalizes each positional `[DIRS]` entry (resolving symlinks and
/// absolutizing) so they can restrict a vault query via
/// [`InMemoryStore::retain_under`]. A missing or inaccessible directory is a
/// hard error naming the offending path, matching
/// [`crate::cli::canonicalize_roots`]'s live-scan behavior.
fn canonicalize_dirs(dirs: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    dirs.iter()
        .map(|dir| {
            fs::canonicalize(dir)
                .with_context(|| format!("cannot access directory {}", dir.display()))
        })
        .collect()
}

/// Runs every top-level statement in `input` — `;`/`\g`-terminated, or
/// `\G`-terminated for vertical output — writing each rendered result (with
/// exactly one trailing newline) to `output` when set, or to stdout otherwise
/// (matching the plain `println!` this replaced), and returns the sum of
/// their row counts (for `--exit-code`'s grep-style mapping).
///
/// `output` is opened once — creating it and truncating any existing content
/// — before the first statement runs; every statement in the run then
/// appends to that same handle, so stdout stays completely empty. The first
/// statement that fails aborts the run: its error propagates to `main`,
/// which reports it on stderr and exits non-zero. Statements that ran before
/// it have already been written.
///
/// A failing statement's error is prefixed with its 1-based position —
/// `statement N of M failed: ...` — whenever the batch has more than one
/// statement (design W36), so a partial `--output` run tells the caller
/// exactly which statement to blame. A lone statement needs no "1 of 1"
/// noise, so the prefix is omitted when `M == 1`. This is batch/`-e`/`query
/// run` only; the REPL runs statements one at a time via its own loop and is
/// unaffected.
fn run_statements(session: &Session, input: &str, output: Option<&Path>) -> anyhow::Result<usize> {
    let mut sink = match output {
        Some(path) => OutputSink::open_file(path)
            .with_context(|| format!("cannot write results to {}", path.display()))?,
        None => OutputSink::Stdout,
    };
    let statements = split_statements(input);
    let total = statements.len();
    let mut total_rows = 0;
    for (i, statement) in statements.iter().enumerate() {
        let (rendered, rows) =
            session
                .render_statement_counted(statement)
                .map_err(|e| match total {
                    1 => e,
                    _ => e.context(format!("statement {} of {total} failed", i + 1)),
                })?;
        sink.write_block(&rendered)
            .context("failed to write query results")?;
        total_rows += rows;
    }
    Ok(total_rows)
}

/// Reads all of stdin as UTF-8 query text.
fn read_stdin() -> anyhow::Result<String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read query text from stdin")?;
    Ok(buf)
}
