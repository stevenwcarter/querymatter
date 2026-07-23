//! `querymatter` entry point: parse the CLI, load the record store, and
//! dispatch to one-shot, batch, or interactive mode.
//!
//! Output discipline: **stdout carries query results only.** Every
//! diagnostic, warning, and prompt goes to stderr, so a pipeline like
//! `querymatter -e '…' --format json | jq` sees pure JSON.

pub mod cache;
mod cli;
pub mod discover;
pub mod frontmatter;
pub mod model;
pub mod query;
pub mod render;
mod repl;
mod session;
pub mod store;

use std::io::{self, IsTerminal, Read};

use anyhow::Context;
use clap::Parser;

use crate::cli::Cli;
use crate::session::{Session, split_statements};
use crate::store::InMemoryStore;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli.validate_excludes()?;
    let roots = cli.resolved_roots()?;
    let mut walk_opts = cli.walk_opts();
    walk_opts.ignore_files = cli.ignore_files()?;

    let (store, report) = InMemoryStore::load(roots, walk_opts);
    for warning in &report.warnings {
        eprintln!("querymatter: {warning}");
    }
    let session = Session::new(Box::new(store), cli.format);

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

/// Runs every top-level `;`-separated statement in `input`, printing each
/// rendered result to stdout (with exactly one trailing newline via
/// `println!`).
///
/// The first statement that fails aborts the run: its error propagates to
/// `main`, which reports it on stderr and exits non-zero. Statements that ran
/// before it have already printed their results.
fn run_statements(session: &Session, input: &str) -> anyhow::Result<()> {
    for statement in split_statements(input) {
        let rendered = session.render_query(&statement)?;
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
