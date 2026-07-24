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
pub mod query;
pub mod render;
mod repl;
mod session;
mod settings;
pub mod store;

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{ArgMatches, CommandFactory, FromArgMatches};

use crate::cli::{Cli, Command, CompletionsArgs, ConfigAction, ConfigArgs, InitArgs};
use crate::config::Config;
use crate::session::{Session, split_statements};
use crate::settings::Settings;
use crate::store::{InMemoryStore, RecordStore};

fn main() -> anyhow::Result<()> {
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    // Completions and `config path` must both work even with a broken config
    // file: completions is how a user installs the completion that helps
    // them type `config set` correctly, and `config path` is the one command
    // a user with a broken config reaches for to find the file worth fixing.
    // Neither needs config *content* — completions only needs the parser
    // shape, and `path` only reports where the file would be — so both are
    // dispatched here, before `config::load()` can fail on them.
    match &cli.command {
        Some(Command::Completions(args)) => {
            run_completions(args);
            return Ok(());
        }
        Some(Command::Config(ConfigArgs {
            action: ConfigAction::Path,
        })) => return run_config_path(),
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
            run_init(args, &config, sub_matches)
        }
        Some(Command::Config(args)) => run_config(&args.action, &cli, &config, &matches),
        Some(Command::Completions(_)) => unreachable!("handled above"),
        None => run_query(&cli, &config, &matches),
    }
}

/// Writes a shell completion script for `args.shell` to stdout.
///
/// The script is data, so it goes to stdout for redirection into the shell's
/// completion directory (see the README).
fn run_completions(args: &CompletionsArgs) {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(args.shell, &mut command, name, &mut io::stdout());
}

/// Builds a `.querymatter` cache under the requested directory (or the cwd),
/// honoring the shared walk flags, and prints a one-line summary to stderr.
///
/// All summary output goes to stderr; `init` produces no stdout so it composes
/// cleanly in scripts.
fn run_init(args: &InitArgs, config: &Config, matches: &ArgMatches) -> anyhow::Result<()> {
    let settings = Settings::resolve_walk(&args.walk, config, matches);
    // Validated on the RESOLVED exclude list (flag, config, or default —
    // whichever won), not `args.walk.exclude` alone, so a bad glob from a
    // hand-edited config file is caught too, not just one typed on the
    // command line (IMPORTANT 1).
    discover::validate_excludes(&settings.exclude.value)?;

    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let target = args.dir.clone().unwrap_or(cwd);
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
            println!("{}", Settings::resolve(cli, config, matches).rows());
        }
        ConfigAction::Get { key } => {
            let settings = Settings::resolve(cli, config, matches);
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

/// Runs a query: loads the store from an ancestor `.querymatter` cache when one
/// is found (unless `--no-cache`), or live-scans the resolved roots otherwise,
/// then dispatches to one-shot, batch, or interactive mode.
fn run_query(cli: &Cli, config: &Config, matches: &ArgMatches) -> anyhow::Result<()> {
    cli.validate()?;

    let settings = Settings::resolve(cli, config, matches);
    // Validated on the RESOLVED exclude list (flag, config, or default —
    // whichever won), not `cli.walk.exclude` alone: a hand-edited config
    // file's `exclude` must be rejected here too. `config::set` already
    // rejects a bad glob up front for the normal `config set exclude` path,
    // but a hand-edited file bypasses that, and `discover`'s own glob
    // compiler has no error channel and would otherwise silently drop it
    // (IMPORTANT 1).
    discover::validate_excludes(&settings.exclude.value)?;
    let mut opts = settings.walk_opts();
    opts.ignore_files = cli.walk.ignore_files()?;

    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let vault = if cli.no_cache {
        None
    } else {
        cache::find_vault(&cwd)
    };

    let (store, report, session_vault) = match vault {
        Some(vault) => {
            let (mut store, mut report) = InMemoryStore::from_cache(&vault, opts, cli.freshness());
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
            // subtrees. The vault is loaded whole above, then narrowed here at
            // slice granularity. A dir entirely outside the vault matches no
            // slice (its records are absent) — v1 does not live-scan
            // outside-vault dirs, a known limitation.
            if !cli.dirs.is_empty() {
                store.retain_under(&canonicalize_dirs(&cli.dirs)?);
            }
            (store, report, Some(vault))
        }
        None => {
            anyhow::ensure!(
                !cli.force_cache,
                "--force-cache: no .querymatter cache found (run `querymatter init` first)"
            );
            let (store, report) = InMemoryStore::load(cli.resolved_roots()?, opts);
            (store, report, None)
        }
    };

    for warning in &report.warnings {
        eprintln!("querymatter: {warning}");
    }
    let fallback = Settings::resolve(cli, &Config::default(), matches);
    let session = Session::new(Box::new(store), settings, fallback, session_vault);

    match cli.query.as_deref() {
        // `-e -`: read the query text from stdin, then run it.
        Some("-") => run_statements(&session, &read_stdin()?),
        // `-e <sql>`: run the given query text.
        Some(sql) => run_statements(&session, sql),
        // No `-e`: batch mode when stdin is piped, otherwise the REPL.
        None if !io::stdin().is_terminal() => run_statements(&session, &read_stdin()?),
        None => repl::run(session),
    }
}

/// Canonicalizes each positional `[DIRS]` entry (resolving symlinks and
/// absolutizing) so they can restrict a vault query via
/// [`InMemoryStore::retain_under`]. A missing or inaccessible directory is a
/// hard error naming the offending path, matching [`Cli::resolved_roots`]'s
/// live-scan behavior.
fn canonicalize_dirs(dirs: &[PathBuf]) -> anyhow::Result<Vec<PathBuf>> {
    dirs.iter()
        .map(|dir| {
            fs::canonicalize(dir)
                .with_context(|| format!("cannot access directory {}", dir.display()))
        })
        .collect()
}

/// Runs every top-level statement in `input` — `;`/`\g`-terminated, or
/// `\G`-terminated for vertical output — printing each rendered result to
/// stdout (with exactly one trailing newline via `println!`).
///
/// The first statement that fails aborts the run: its error propagates to
/// `main`, which reports it on stderr and exits non-zero. Statements that ran
/// before it have already printed their results.
fn run_statements(session: &Session, input: &str) -> anyhow::Result<()> {
    for statement in split_statements(input) {
        let rendered = session.render_statement(&statement)?;
        println!("{rendered}");
    }
    Ok(())
}

/// Reads all of stdin as UTF-8 query text.
fn read_stdin() -> anyhow::Result<String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .context("failed to read query text from stdin")?;
    Ok(buf)
}
