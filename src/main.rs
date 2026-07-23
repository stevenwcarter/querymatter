//! `querymatter` entry point: parse the CLI, then either build a
//! `.querymatter` cache (`init`) or run a query — loading the record store
//! from an ancestor cache when one is found, or live-scanning otherwise — and
//! dispatch to one-shot, batch, or interactive mode.
//!
//! Output discipline: **stdout carries query results only.** Every
//! diagnostic, warning, and prompt goes to stderr, so a pipeline like
//! `querymatter -e '…' --format json | jq` sees pure JSON.

pub mod cache;
mod cli;
pub mod discover;
pub mod frontmatter;
mod gitignore;
pub mod model;
pub mod query;
pub mod render;
mod repl;
mod session;
pub mod store;

use std::env;
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::Parser;

use crate::cli::{Cli, Command, InitArgs};
use crate::session::{Session, split_statements};
use crate::store::{InMemoryStore, RecordStore};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Some(Command::Init(args)) => run_init(args),
        None => run_query(&cli),
    }
}

/// Builds a `.querymatter` cache under the requested directory (or the cwd),
/// honoring the shared walk flags, and prints a one-line summary to stderr.
///
/// All summary output goes to stderr; `init` produces no stdout so it composes
/// cleanly in scripts.
fn run_init(args: &InitArgs) -> anyhow::Result<()> {
    args.walk.validate_excludes()?;

    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let target = args.dir.clone().unwrap_or(cwd);
    let base = fs::canonicalize(&target)
        .with_context(|| format!("cannot access directory {}", target.display()))?;

    let mut opts = args.walk.walk_opts();
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

/// Runs a query: loads the store from an ancestor `.querymatter` cache when one
/// is found (unless `--no-cache`), or live-scans the resolved roots otherwise,
/// then dispatches to one-shot, batch, or interactive mode.
fn run_query(cli: &Cli) -> anyhow::Result<()> {
    cli.validate()?;
    cli.walk.validate_excludes()?;

    let mut opts = cli.walk.walk_opts();
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
    let session = Session::new(Box::new(store), cli.format, cli.table_style, session_vault);

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
