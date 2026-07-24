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

use crate::config::ConfigKey;
use crate::model::Value;
use crate::render::{Format, TableStyle};
use crate::session::{Session, Statement, Terminator};
use crate::store::LoadReport;

/// Prompt shown while waiting for a new statement or dot-command.
const PROMPT: &str = "querymatter> ";
/// Prompt shown while a statement is still accumulating (no `;`, `\g`, or
/// `\G` yet).
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
    /// A complete statement plus the terminator that ended it (the terminator
    /// itself is stripped).
    Statement(Statement),
    /// A `.`-prefixed dot-command, paired with the original (trimmed) line
    /// text. [`DotCommand`] alone loses casing and whitespace that parsing
    /// normalizes away, so the raw text rides along for callers — namely
    /// [`history_entry`] — that want an exact record of what was typed.
    Dot(DotCommand, String),
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
    /// `.style [style]` — set (`Some`) or report (`None`) the table style.
    Style(Option<TableStyle>),
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
    /// `.style <name>` where `<name>` is not a known [`TableStyle`], carrying
    /// the offending name so the error can name the style rather than the
    /// command.
    BadStyle(String),
    /// `.settings` — list every setting, its value, and its source.
    Settings,
    /// `.set <key> <value>` — persist a setting to the config file.
    Set(ConfigKey, String),
    /// `.unset <key>` — remove a setting from the config file.
    Unset(ConfigKey),
    /// `.set`/`.unset` naming a key that isn't configurable, carrying the
    /// offending name so the error can name the key rather than the command.
    BadKey(String),
    /// `.set`/`.unset` with a missing argument, carrying the command name.
    MissingArg(&'static str),
    /// Any other `.`-prefixed line, carried verbatim for the error message.
    Unknown(String),
}

/// Accumulates raw input lines into complete SQL statements, splitting on a
/// trailing `;`, `\g`, or `\G`.
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
                return Line::Dot(parse_dot(trimmed), trimmed.to_string());
            }
            self.buf.push_str(raw);
        } else {
            self.buf.push('\n');
            self.buf.push_str(raw);
        }

        match take_terminated(self.buf.trim_end()) {
            Some((sql, terminator)) => {
                let statement = Statement {
                    sql: sql.trim().to_string(),
                    terminator,
                };
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

/// Splits a trailing `;`, `\g`, or `\G` terminator off `text`, returning the
/// statement body and which terminator ended it.
///
/// Like the bare `;` check this replaces, it looks only at the suffix and is
/// not quote-aware: a terminator that ends the line ends the statement even
/// inside a string literal. [`crate::session::split_statements`] is the
/// quote-aware seam for batch input; the asymmetry predates these terminators
/// and is left as-is.
fn take_terminated(text: &str) -> Option<(&str, Terminator)> {
    if let Some(sql) = text.strip_suffix("\\G") {
        Some((sql, Terminator::VerticalG))
    } else if let Some(sql) = text.strip_suffix("\\g") {
        Some((sql, Terminator::Semicolon))
    } else {
        text.strip_suffix(';')
            .map(|sql| (sql, Terminator::Semicolon))
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
        "style" => match words.next() {
            None => DotCommand::Style(None),
            Some(arg) => match arg.parse() {
                Ok(style) => DotCommand::Style(Some(style)),
                Err(_) => DotCommand::BadStyle(arg.to_string()),
            },
        },
        "settings" => DotCommand::Settings,
        "set" => match (words.next(), rest_after_key(rest, 2)) {
            (Some(key), Some(value)) => match parse_key(key) {
                Some(key) => DotCommand::Set(key, value),
                None => DotCommand::BadKey(key.to_string()),
            },
            _ => DotCommand::MissingArg("set"),
        },
        "unset" => match words.next() {
            Some(key) => match parse_key(key) {
                Some(key) => DotCommand::Unset(key),
                None => DotCommand::BadKey(key.to_string()),
            },
            None => DotCommand::MissingArg("unset"),
        },
        _ => DotCommand::Unknown(line.to_string()),
    }
}

/// Parses a config key name, accepting exactly the spellings `ConfigKey`
/// declares — the same ones the TOML file and `querymatter config` use.
fn parse_key(name: &str) -> Option<ConfigKey> {
    ConfigKey::ALL.into_iter().find(|key| key.as_str() == name)
}

/// Everything after the first `skip` whitespace-separated words of `rest`,
/// trimmed — the value of a `.set`, taken verbatim so globs and commas
/// survive. `None` when there are fewer than `skip + 1` words.
fn rest_after_key(rest: &str, skip: usize) -> Option<String> {
    let mut remainder = rest.trim_start();
    for _ in 0..skip {
        let end = remainder.find(char::is_whitespace)?;
        remainder = remainder[end..].trim_start();
    }
    (!remainder.is_empty()).then(|| remainder.trim_end().to_string())
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

    println!(
        "querymatter — {} records. Type .help for commands, .schema for fields.",
        record_count(&session)
    );

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

        let resolved = buffer.push(&line);
        if let Some(entry) = history_entry(&resolved) {
            let _ = editor.add_history_entry(entry);
        }

        match resolved {
            Line::Blank | Line::More => {}
            Line::Statement(statement) => match session.render_statement(&statement) {
                Ok(rendered) => println!("{rendered}"),
                Err(err) => eprintln!("querymatter: {err:#}"),
            },
            Line::Dot(cmd, _) => {
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

/// What to add to the line editor's history for one resolved [`Line`], or
/// `None` when nothing should be recorded.
///
/// A statement accumulated across several raw lines ([`Line::More`] then
/// [`Line::Statement`]) records exactly once, as the assembled SQL plus its
/// terminator — the W5 fix: previously every raw line got its own history
/// entry, so a statement typed across N lines left N unrunnable fragments in
/// history instead of one entry that replays the whole thing. A dot-command
/// records its original line text verbatim, carried on [`Line::Dot`], rather
/// than a reconstruction from the parsed [`DotCommand`] (which lowercases the
/// command name and collapses whitespace). Blank lines and mid-statement
/// continuations record nothing.
fn history_entry(line: &Line) -> Option<String> {
    match line {
        Line::Blank | Line::More => None,
        Line::Statement(statement) => {
            let terminator = match statement.terminator {
                Terminator::Semicolon => ";",
                Terminator::VerticalG => "\\G",
            };
            Some(format!("{}{terminator}", statement.sql))
        }
        Line::Dot(_, raw) => Some(raw.clone()),
    }
}

/// Runs one dot-command against `session`, returning `true` when the REPL
/// should exit (`.quit`/`.exit`).
///
/// stdout/stderr policy: reference/inspection output (`.help`, `.schema`,
/// `.settings`, and `.format`'s/`.style`'s reports of the current
/// format/style) goes to stdout; the `.reload`/`.refresh`/`.refresh-all`
/// reports, `.set`/`.unset` confirmations, and all error messages (unknown
/// command, bad format, bad style, bad key, missing argument) go to stderr,
/// keeping stdout clean for piping.
fn dispatch_dot(cmd: DotCommand, session: &mut Session) -> bool {
    match cmd {
        DotCommand::Help => print_help(),
        DotCommand::Schema => print_schema(session),
        DotCommand::Format(Some(fmt)) => session.set_format(fmt),
        DotCommand::Format(None) => println!("format: {}", format_name(session.format())),
        DotCommand::Style(Some(style)) => session.set_style(style),
        DotCommand::Style(None) => println!("style: {}", style_name(session.style())),
        DotCommand::Reload => report_reload(session),
        DotCommand::Refresh(path) => report_refresh(session, path.as_deref().map(Path::new)),
        DotCommand::RefreshAll => report_refresh(session, None),
        DotCommand::Quit => return true,
        DotCommand::BadFormat(name) => {
            eprintln!("querymatter: unknown format '{name}' (try: table, json, csv, tsv, md)");
        }
        DotCommand::BadStyle(name) => {
            eprintln!("querymatter: unknown style '{name}' (try: ascii, unicode, compact, plain)");
        }
        DotCommand::Settings => println!("{}", session.settings().rows()),
        DotCommand::Set(key, value) => report_set(session.persist_set(key, &value), key),
        DotCommand::Unset(key) => report_unset(session.persist_unset(key), key),
        DotCommand::BadKey(name) => {
            eprintln!(
                "querymatter: unknown setting '{name}' (try: {})",
                ConfigKey::ALL
                    .iter()
                    .map(|key| key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        DotCommand::MissingArg(cmd) => match cmd {
            "set" => eprintln!("querymatter: usage: .set <key> <value>"),
            _ => eprintln!("querymatter: usage: .unset <key>"),
        },
        DotCommand::Unknown(raw) => {
            eprintln!("querymatter: unknown command {raw:?} (try .help)");
        }
    }
    false
}

/// Reports the outcome of a `.set` on stderr: the file written, and a note
/// when the change only takes effect on the next run.
fn report_set(outcome: anyhow::Result<(PathBuf, bool)>, key: ConfigKey) {
    match outcome {
        Ok((path, immediate)) => {
            eprintln!("querymatter: saved {} in {}", key.as_str(), path.display());
            report_deferred(immediate);
        }
        Err(err) => eprintln!("querymatter: {err:#}"),
    }
}

/// Reports the outcome of a `.unset` on stderr: "removed" (plus the file
/// written, and a note when the change only takes effect on the next run)
/// when the key had actually been set, or an accurate "was not set" —
/// matching `querymatter config unset`'s wording — when it was already
/// absent, since that case never wrote anything.
fn report_unset(outcome: anyhow::Result<(PathBuf, bool, bool)>, key: ConfigKey) {
    match outcome {
        Ok((path, true, immediate)) => {
            eprintln!(
                "querymatter: removed {} from {}",
                key.as_str(),
                path.display()
            );
            report_deferred(immediate);
        }
        Ok((path, false, _)) => {
            eprintln!(
                "querymatter: {} was not set in {}",
                key.as_str(),
                path.display()
            );
        }
        Err(err) => eprintln!("querymatter: {err:#}"),
    }
}

/// Prints the "takes effect on the next run" note when a change was deferred
/// (a scan setting: the store is already loaded from before the change).
fn report_deferred(immediate: bool) {
    if !immediate {
        eprintln!("querymatter: takes effect on the next run (the store is already loaded)");
    }
}

/// Prints the dot-command reference to stdout.
fn print_help() {
    println!("Dot-commands:");
    println!("  .help              show this message");
    println!("  .schema            list frontmatter fields, file.* columns, and the record count");
    println!("  .format [fmt]      show, or set, the output format (table, json, csv, tsv, md)");
    println!(
        "  .style [style]     show, or set, the table border style (ascii, unicode, compact, plain)"
    );
    println!("  .settings          list every setting, its value, and where it came from");
    println!("  .set <key> <val>   save a setting to the config file");
    println!("  .unset <key>       remove a setting from the config file");
    println!("  .reload            rescan every tracked directory (in-memory only)");
    println!(
        "  .refresh [path]    re-scan path (or all); updates the .querymatter cache, else in memory"
    );
    println!(
        "  .refresh-all       re-scan the whole vault; updates the cache, or in memory with no vault"
    );
    println!("  .quit / .exit      leave the REPL");
    println!();
    println!("End a statement with ';' to run it, or with '\\G' to print each row as a block of");
    println!("name: value lines; '\\g' is a synonym for ';'. Statements may span multiple lines.");
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

/// The name `.style` reports/accepts for `style`, kept in sync with
/// [`TableStyle`]'s `FromStr` impl.
fn style_name(style: TableStyle) -> &'static str {
    match style {
        TableStyle::Ascii => "ascii",
        TableStyle::Unicode => "unicode",
        TableStyle::Compact => "compact",
        TableStyle::Plain => "plain",
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
    fn buffers_until_vertical_g() {
        let mut b = LineBuffer::new();
        assert_eq!(b.push("SELECT status"), Line::More);
        match b.push("FROM files\\G") {
            Line::Statement(stmt) => {
                assert_eq!(stmt.sql, "SELECT status\nFROM files");
                assert_eq!(stmt.terminator, Terminator::VerticalG);
            }
            other => panic!("expected a Statement, got {other:?}"),
        }
    }

    #[test]
    fn lowercase_g_terminates_like_a_semicolon() {
        let mut b = LineBuffer::new();
        match b.push("SELECT 1\\g") {
            Line::Statement(stmt) => {
                assert_eq!(stmt.sql, "SELECT 1");
                assert_eq!(stmt.terminator, Terminator::Semicolon);
            }
            other => panic!("expected a Statement, got {other:?}"),
        }
    }

    #[test]
    fn single_line_statement() {
        let mut b = LineBuffer::new();
        match b.push("SELECT 1;") {
            Line::Statement(s) => {
                assert_eq!(s.sql, "SELECT 1");
                assert_eq!(s.terminator, Terminator::Semicolon);
            }
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
            Line::Statement(s) => {
                assert_eq!(s.sql, "SELECT status\n.schema");
                assert_eq!(s.terminator, Terminator::Semicolon);
            }
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
            Line::Statement(s) => {
                assert_eq!(s.sql, "SELECT status\n   \nWHERE prd = '010'");
                assert_eq!(s.terminator, Terminator::Semicolon);
            }
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
        assert!(matches!(
            b.push(".schema"),
            Line::Dot(DotCommand::Schema, _)
        ));
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

    #[test]
    fn style_command_parses() {
        assert_eq!(
            parse_dot(".style unicode"),
            DotCommand::Style(Some(TableStyle::Unicode))
        );
        assert!(matches!(parse_dot(".style"), DotCommand::Style(None)));
    }

    /// A known `.style` with an unknown name is BadStyle (reported as an
    /// unknown *style*), not an unknown command — mirroring `.format`.
    #[test]
    fn bad_style_arg_is_bad_style_not_unknown_command() {
        match parse_dot(".style fancy") {
            DotCommand::BadStyle(name) => assert_eq!(name, "fancy"),
            other => panic!("expected BadStyle, got {other:?}"),
        }
    }

    #[test]
    fn settings_command_parses() {
        assert_eq!(parse_dot(".settings"), DotCommand::Settings);
    }

    #[test]
    fn set_command_parses_key_and_value() {
        assert_eq!(
            parse_dot(".set table_style unicode"),
            DotCommand::Set(ConfigKey::TableStyle, "unicode".to_string())
        );
    }

    /// A list value may contain commas but no spaces; the rest of the line is
    /// taken verbatim so `exclude` globs survive.
    #[test]
    fn set_takes_the_rest_of_the_line_as_the_value() {
        assert_eq!(
            parse_dot(".set exclude **/a/**,**/b/**"),
            DotCommand::Set(ConfigKey::Exclude, "**/a/**,**/b/**".to_string())
        );
    }

    #[test]
    fn unset_command_parses() {
        assert_eq!(
            parse_dot(".unset hidden"),
            DotCommand::Unset(ConfigKey::Hidden)
        );
    }

    #[test]
    fn bad_key_is_bad_key_not_unknown_command() {
        match parse_dot(".set bogus x") {
            DotCommand::BadKey(name) => assert_eq!(name, "bogus"),
            other => panic!("expected BadKey, got {other:?}"),
        }
    }

    #[test]
    fn missing_arguments_are_reported_as_missing() {
        assert_eq!(parse_dot(".set"), DotCommand::MissingArg("set"));
        assert_eq!(parse_dot(".set table_style"), DotCommand::MissingArg("set"));
        assert_eq!(parse_dot(".unset"), DotCommand::MissingArg("unset"));
    }

    #[test]
    fn history_records_one_entry_per_statement_not_per_line() {
        // Feed a multi-line statement through LineBuffer and assert the
        // history hook yields exactly one entry equal to the joined statement.
        let mut b = LineBuffer::new();
        assert_eq!(history_entry(&b.push("SELECT status")), None); // More
        assert_eq!(history_entry(&b.push("FROM 'x'")), None); // More
        let done = b.push("WHERE status = 'draft';"); // Statement
        let entry = history_entry(&done).expect("a completed statement is recorded");
        assert!(entry.contains("SELECT status") && entry.contains("WHERE status = 'draft'"));
        assert!(
            entry.ends_with(';'),
            "the terminator is re-appended so the entry is directly re-runnable: {entry:?}"
        );
        // a dot-command records one entry; blank records none
        let mut b2 = LineBuffer::new();
        assert_eq!(history_entry(&b2.push("")), None);
        assert!(history_entry(&b2.push(".schema")).is_some());
    }

    #[test]
    fn history_entry_for_dot_command_is_the_original_line_verbatim() {
        // history_entry must not reconstruct dot-command text from the
        // parsed DotCommand (which lowercases the command and collapses
        // whitespace) — it should carry the original line through unchanged.
        let mut b = LineBuffer::new();
        let entry = history_entry(&b.push("  .FORMAT   json  ")).expect("a dot-command records");
        assert_eq!(entry, ".FORMAT   json");
    }

    #[test]
    fn history_entry_for_vertical_g_statement_reappends_it() {
        let mut b = LineBuffer::new();
        let entry =
            history_entry(&b.push("SELECT 1\\G")).expect("a completed statement is recorded");
        assert_eq!(entry, "SELECT 1\\G");
    }
}
