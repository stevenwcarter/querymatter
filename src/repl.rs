//! The interactive REPL: a rustyline-based shell for querymatter, with
//! multi-line `;`-terminated statements, dot-commands, and history.
//!
//! Line *parsing* — what a chunk of raw input resolves to — is split from
//! rustyline's IO loop so it can be unit-tested without a terminal: see
//! [`Line`], [`DotCommand`], [`LineBuffer`], and [`parse_dot`]. [`run`] is
//! the IO driver built on top of them.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use directories::ProjectDirs;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

use crate::model::Value;
use crate::render::Format;
use crate::session::Session;
use crate::store::LoadReport;

/// Prompt shown while waiting for a new statement or dot-command.
const PROMPT: &str = "querymatter> ";
/// Prompt shown while a statement is still accumulating (no `;` yet).
const CONTINUATION_PROMPT: &str = "   ...> ";
/// The `file.*` pseudo-columns every record exposes, independent of
/// frontmatter (kept in sync with [`crate::model::FileAttr`]'s labels).
const FILE_COLUMNS: [&str; 4] = ["file.name", "file.path", "file.folder", "file.ext"];
/// The history file's name under the REPL's state/data directory.
const HISTORY_FILE: &str = "history.txt";

/// What one line of raw input resolved to, once fed to a [`LineBuffer`].
#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// The line arrived (with an empty buffer behind it) empty/whitespace.
    Blank,
    /// A statement is still accumulating; more input is needed.
    More,
    /// A complete statement, `;`-terminated (the `;` is stripped).
    Statement(String),
    /// A `.`-prefixed dot-command.
    Dot(DotCommand),
}

/// A REPL dot-command, parsed from a line beginning with `.`.
#[derive(Debug, Clone, PartialEq)]
pub enum DotCommand {
    /// `.help` — list the dot-commands.
    Help,
    /// `.schema` — list frontmatter fields, `file.*` columns, and the record count.
    Schema,
    /// `.format [fmt]` — set (`Some`) or report (`None`) the output format.
    Format(Option<Format>),
    /// `.reload` — rescan every tracked directory (in-memory only; never
    /// touches a `.querymatter` cache).
    Reload,
    /// `.refresh [path]` — force a re-scan of `path`'s subtree, or the
    /// whole vault when omitted, persisting the update when a
    /// `.querymatter` cache backs the session.
    Refresh(Option<String>),
    /// `.refresh-all` — force a re-scan of the whole vault, persisting the
    /// update; an explicit alias for `.refresh` with no path.
    RefreshAll,
    /// `.quit` / `.exit` — leave the REPL.
    Quit,
    /// `.format <name>` where `<name>` is not a known [`Format`], carrying the
    /// offending name so the error can name the format rather than the command.
    BadFormat(String),
    /// Any other `.`-prefixed line, carried verbatim for the error message.
    Unknown(String),
}

/// Accumulates raw input lines into complete SQL statements, splitting on a
/// trailing `;`.
///
/// A line starting with `.` or an empty/whitespace line is only recognized
/// as a dot-command/blank line when the buffer is currently empty —
/// mid-statement it's just more statement text, so a `.` inside a string
/// literal or a blank line between clauses doesn't derail accumulation.
#[derive(Debug, Default)]
pub struct LineBuffer {
    buf: String,
}

impl LineBuffer {
    /// An empty buffer, ready for the first line.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one line of raw input, returning how it resolved.
    pub fn push(&mut self, raw: &str) -> Line {
        if self.buf.is_empty() {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Line::Blank;
            }
            if trimmed.starts_with('.') {
                return Line::Dot(parse_dot(trimmed));
            }
            self.buf.push_str(raw);
        } else {
            self.buf.push('\n');
            self.buf.push_str(raw);
        }

        match self.buf.trim_end().strip_suffix(';') {
            Some(stmt) => {
                let statement = stmt.trim().to_string();
                self.buf.clear();
                Line::Statement(statement)
            }
            None => Line::More,
        }
    }

    /// True once a statement has started accumulating; picks the
    /// continuation prompt in [`run`].
    fn is_pending(&self) -> bool {
        !self.buf.is_empty()
    }
}

/// Parses a `.`-prefixed line into a [`DotCommand`].
///
/// An unrecognized command name falls back to [`DotCommand::Unknown`] carrying
/// the whole original line; a recognized `.format` with an unknown format name
/// becomes [`DotCommand::BadFormat`] carrying just that name, so the error can
/// name the format rather than imply `.format` itself is unknown.
pub fn parse_dot(line: &str) -> DotCommand {
    let rest = line.strip_prefix('.').unwrap_or(line);
    let mut words = rest.split_whitespace();
    let cmd = words.next().unwrap_or("").to_ascii_lowercase();
    match cmd.as_str() {
        "help" => DotCommand::Help,
        "schema" => DotCommand::Schema,
        "reload" => DotCommand::Reload,
        "refresh" => DotCommand::Refresh(words.next().map(str::to_string)),
        "refresh-all" => DotCommand::RefreshAll,
        "quit" | "exit" => DotCommand::Quit,
        "format" => match words.next() {
            None => DotCommand::Format(None),
            Some(arg) => match arg.parse() {
                Ok(fmt) => DotCommand::Format(Some(fmt)),
                Err(_) => DotCommand::BadFormat(arg.to_string()),
            },
        },
        _ => DotCommand::Unknown(line.to_string()),
    }
}

/// Runs the interactive REPL over `session` until `.quit`/`.exit`/EOF.
///
/// History persists under the OS's per-app state (falling back to data)
/// directory via [`ProjectDirs`]; a missing or unwritable history file is at
/// most a warning on stderr, never a fatal error — the REPL keeps working
/// without it.
pub fn run(mut session: Session) -> anyhow::Result<()> {
    let mut editor = DefaultEditor::new().context("failed to initialize the line editor")?;
    let history_path = history_path();
    if let Some(path) = &history_path {
        prepare_history_dir(path);
        load_history(&mut editor, path);
    }

    let mut buffer = LineBuffer::new();
    loop {
        let prompt = if buffer.is_pending() {
            CONTINUATION_PROMPT
        } else {
            PROMPT
        };
        let line = match editor.readline(prompt) {
            Ok(line) => line,
            Err(ReadlineError::Interrupted) => {
                buffer = LineBuffer::new();
                continue;
            }
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("querymatter: readline error: {err}");
                break;
            }
        };
        let _ = editor.add_history_entry(line.as_str());

        match buffer.push(&line) {
            Line::Blank | Line::More => {}
            Line::Statement(sql) => match session.render_query(&sql) {
                Ok(rendered) => println!("{rendered}"),
                Err(err) => eprintln!("querymatter: {err:#}"),
            },
            Line::Dot(cmd) => {
                if dispatch_dot(cmd, &mut session) {
                    break;
                }
            }
        }
    }

    if let Some(path) = &history_path
        && let Err(err) = editor.save_history(path)
    {
        eprintln!(
            "querymatter: could not save history to {}: {err}",
            path.display()
        );
    }
    Ok(())
}

/// Runs one dot-command against `session`, returning `true` when the REPL
/// should exit (`.quit`/`.exit`).
///
/// stdout/stderr policy: reference/inspection output (`.help`, `.schema`, and
/// `.format`'s report of the current format) goes to stdout; the
/// `.reload`/`.refresh`/`.refresh-all` reports and all error messages
/// (unknown command, bad format) go to stderr, keeping stdout clean for
/// piping.
fn dispatch_dot(cmd: DotCommand, session: &mut Session) -> bool {
    match cmd {
        DotCommand::Help => print_help(),
        DotCommand::Schema => print_schema(session),
        DotCommand::Format(Some(fmt)) => session.set_format(fmt),
        DotCommand::Format(None) => println!("format: {}", format_name(session.format)),
        DotCommand::Reload => report_reload(session),
        DotCommand::Refresh(path) => report_refresh(session, path.as_deref().map(Path::new)),
        DotCommand::RefreshAll => report_refresh(session, None),
        DotCommand::Quit => return true,
        DotCommand::BadFormat(name) => {
            eprintln!("querymatter: unknown format '{name}' (try: table, json, csv, tsv, md)");
        }
        DotCommand::Unknown(raw) => {
            eprintln!("querymatter: unknown command {raw:?} (try .help)");
        }
    }
    false
}

/// Prints the dot-command reference to stdout.
fn print_help() {
    println!("Dot-commands:");
    println!("  .help              show this message");
    println!("  .schema            list frontmatter fields, file.* columns, and the record count");
    println!("  .format [fmt]      show, or set, the output format (table, json, csv, tsv, md)");
    println!("  .reload            rescan every tracked directory (in-memory only)");
    println!("  .refresh [path]    force a re-scan of path (or the whole vault) and persist it");
    println!("  .refresh-all       force a re-scan of the whole vault and persist it");
    println!("  .quit / .exit      leave the REPL");
    println!();
    println!("End a statement with ';' to run it; statements may span multiple lines.");
}

/// Prints the discovered frontmatter fields, the `file.*` pseudo-columns,
/// and the current record count to stdout.
fn print_schema(session: &Session) {
    println!("Frontmatter fields:");
    for field in session.schema() {
        println!("  {field}");
    }
    println!("File pseudo-columns:");
    for column in FILE_COLUMNS {
        println!("  {column}");
    }
    println!("{} record(s) loaded", record_count(session));
}

/// The number of records currently loaded, via a `count(*)` query so
/// `Session` needs no dedicated accessor for it.
fn record_count(session: &Session) -> i64 {
    let Ok(table) = session.run("SELECT count(*) AS n") else {
        return 0;
    };
    match table.rows.first().and_then(|row| row.first()) {
        Some(Value::Int(n)) => *n,
        _ => 0,
    }
}

/// Reloads every tracked root and reports the outcome to stderr.
fn report_reload(session: &mut Session) {
    report_summary("reloaded", &session.reload());
}

/// Forces a re-scan of `subtree` (or the whole vault, when `None`) and
/// reports the outcome to stderr.
fn report_refresh(session: &mut Session, subtree: Option<&Path>) {
    report_summary("refreshed", &session.refresh(subtree));
}

/// Prints a [`LoadReport`] summary to stderr in the shared `.reload`/
/// `.refresh`/`.refresh-all` format, differing only in `verb`.
fn report_summary(verb: &str, report: &LoadReport) {
    eprintln!(
        "querymatter: {verb} {} record(s), skipped {}",
        report.loaded, report.skipped
    );
    for warning in &report.warnings {
        eprintln!("querymatter: {warning}");
    }
}

/// The name `.format` reports/accepts for `format`, kept in sync with
/// [`Format`]'s `FromStr` impl.
fn format_name(format: Format) -> &'static str {
    match format {
        Format::Table => "table",
        Format::Json => "json",
        Format::Csv => "csv",
        Format::Tsv => "tsv",
        Format::Md => "md",
    }
}

/// The history file path under the OS's per-app state (or data) directory,
/// or `None` when no valid home directory can be found — history is simply
/// not persisted in that case.
fn history_path() -> Option<PathBuf> {
    let dirs = ProjectDirs::from("", "", "querymatter")?;
    let dir = dirs.state_dir().unwrap_or_else(|| dirs.data_dir());
    Some(dir.join(HISTORY_FILE))
}

/// Best-effort creation of `path`'s parent directory; failure is a warning,
/// not fatal (history simply won't load/save this run).
fn prepare_history_dir(path: &Path) {
    if let Some(parent) = path.parent()
        && let Err(err) = fs::create_dir_all(parent)
    {
        eprintln!(
            "querymatter: could not create history directory {}: {err}",
            parent.display()
        );
    }
}

/// Loads history from `path` into `editor`, ignoring a missing file (normal
/// on first run) and warning on any other IO error.
fn load_history(editor: &mut DefaultEditor, path: &Path) {
    if let Err(err) = editor.load_history(path) {
        let missing =
            matches!(&err, ReadlineError::Io(io_err) if io_err.kind() == io::ErrorKind::NotFound);
        if !missing {
            eprintln!(
                "querymatter: could not load history from {}: {err}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Format;

    #[test]
    fn buffers_until_semicolon() {
        let mut b = LineBuffer::new();
        assert!(matches!(b.push("SELECT status"), Line::More));
        assert!(matches!(b.push("FROM 'x' ;"), Line::Statement(_)));
    }
    #[test]
    fn single_line_statement() {
        let mut b = LineBuffer::new();
        match b.push("SELECT 1;") {
            Line::Statement(s) => assert_eq!(s, "SELECT 1"),
            _ => panic!(),
        }
    }
    #[test]
    fn blank_line_is_blank() {
        let mut b = LineBuffer::new();
        assert!(matches!(b.push("   "), Line::Blank));
    }
    #[test]
    fn dot_line_midstatement_is_statement_text_not_dot() {
        // The `.`-prefix is only a dot-command with an EMPTY buffer; once a
        // statement is accumulating, a `.`-line is ordinary statement text.
        let mut b = LineBuffer::new();
        assert!(matches!(b.push("SELECT status"), Line::More));
        assert!(
            matches!(b.push(".schema"), Line::More),
            ".schema mid-statement must accumulate, not dispatch as Line::Dot"
        );
        match b.push(";") {
            Line::Statement(s) => assert_eq!(s, "SELECT status\n.schema"),
            other => panic!("expected the accumulated Statement, got {other:?}"),
        }
    }
    #[test]
    fn blank_line_midstatement_is_more_not_blank() {
        // A blank/whitespace line only resets to Line::Blank with an empty
        // buffer; mid-statement it keeps accumulating as Line::More.
        let mut b = LineBuffer::new();
        assert!(matches!(b.push("SELECT status"), Line::More));
        assert!(
            matches!(b.push("   "), Line::More),
            "a blank line mid-statement must be Line::More, not Line::Blank"
        );
        match b.push("WHERE prd = '010';") {
            Line::Statement(s) => assert_eq!(s, "SELECT status\n   \nWHERE prd = '010'"),
            other => panic!("expected the accumulated Statement, got {other:?}"),
        }
    }
    #[test]
    fn bad_format_arg_is_bad_format_not_unknown_command() {
        // A known `.format` with an unknown name is BadFormat (which the
        // dispatcher reports as an unknown *format*), not an unknown command.
        match parse_dot(".format bogus") {
            DotCommand::BadFormat(name) => assert_eq!(name, "bogus"),
            other => panic!("expected BadFormat, got {other:?}"),
        }
    }
    #[test]
    fn dot_commands_parse() {
        assert!(matches!(parse_dot(".help"), DotCommand::Help));
        assert!(matches!(parse_dot(".schema"), DotCommand::Schema));
        assert!(matches!(parse_dot(".reload"), DotCommand::Reload));
        assert!(matches!(parse_dot(".quit"), DotCommand::Quit));
        assert!(matches!(parse_dot(".exit"), DotCommand::Quit));
        assert!(matches!(
            parse_dot(".format json"),
            DotCommand::Format(Some(Format::Json))
        ));
        assert!(matches!(parse_dot(".format"), DotCommand::Format(None)));
        assert!(matches!(parse_dot(".bogus"), DotCommand::Unknown(_)));
    }
    #[test]
    fn dot_line_detected_by_buffer() {
        let mut b = LineBuffer::new();
        assert!(matches!(b.push(".schema"), Line::Dot(DotCommand::Schema)));
    }
    #[test]
    fn refresh_commands_parse() {
        assert_eq!(parse_dot(".refresh"), DotCommand::Refresh(None));
        assert_eq!(
            parse_dot(".refresh plans"),
            DotCommand::Refresh(Some("plans".to_string()))
        );
        assert!(matches!(parse_dot(".refresh-all"), DotCommand::RefreshAll));
    }
}
