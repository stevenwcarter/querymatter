//! The interactive REPL: a rustyline-based shell for querymatter, with
//! multi-line `;`-terminated statements, dot-commands, and history.
//!
//! Line *parsing* — what a chunk of raw input resolves to — is split from
//! rustyline's IO loop so it can be unit-tested without a terminal: see
//! [`Line`], [`DotCommand`], [`LineBuffer`], and [`parse_dot`]. [`run`] is
//! the IO driver built on top of them.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Context;
use directories::ProjectDirs;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Context as RlContext, Editor, Helper, Highlighter, Hinter, Validator};

use crate::config::ConfigKey;
use crate::model::Value;
use crate::render::{Format, TableStyle};
use crate::session::{FieldStat, Session, Statement, Terminator};
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
/// The dot-command names, each including its leading `.` — the single source
/// of truth for both [`parse_dot`] (whose leading guard clause rejects
/// anything not listed here, so a name present in one but not handled by the
/// other trips either that guard or the match's `unreachable!()`) and REPL
/// tab-completion ([`complete_candidates`]). Kept as one list so the two
/// can't silently drift apart.
///
/// `print_help`'s lines are not derived from this list — each carries
/// per-command argument syntax (`.describe [field]`, `.set <key> <val>`)
/// that doesn't fit a flat name list — so keep it in sync by hand when adding
/// a command.
const DOT_COMMAND_NAMES: &[&str] = &[
    ".help",
    ".schema",
    ".describe",
    ".format",
    ".style",
    ".settings",
    ".set",
    ".unset",
    ".reload",
    ".refresh",
    ".refresh-all",
    ".quit",
    ".exit",
];

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
    /// `.describe [field]` — detailed type/coverage/value info for `field`,
    /// or (with no argument) a one-line summary of every field.
    Describe(Option<String>),
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
/// name the format rather than imply `.format` itself is unknown. The command
/// name is checked against [`DOT_COMMAND_NAMES`] up front, so the match below
/// only ever runs for a name that list already vouches for.
pub fn parse_dot(line: &str) -> DotCommand {
    let rest = line.strip_prefix('.').unwrap_or(line);
    let mut words = rest.split_whitespace();
    let cmd = words.next().unwrap_or("").to_ascii_lowercase();
    if !DOT_COMMAND_NAMES
        .iter()
        .any(|name| name.trim_start_matches('.') == cmd)
    {
        return DotCommand::Unknown(line.to_string());
    }
    match cmd.as_str() {
        "help" => DotCommand::Help,
        "schema" => DotCommand::Schema,
        "describe" => DotCommand::Describe(words.next().map(str::to_string)),
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
        _ => unreachable!("cmd already checked against DOT_COMMAND_NAMES above"),
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
/// without it. Tab-completion is wired via [`ReplHelper`], whose schema
/// snapshot is taken here, once, at start-up.
pub fn run(mut session: Session) -> anyhow::Result<()> {
    let helper = ReplHelper {
        schema: session.schema(),
    };
    let mut editor: Editor<ReplHelper, FileHistory> =
        Editor::new().context("failed to initialize the line editor")?;
    editor.set_helper(Some(helper));
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
            Line::Statement(statement) => match session.render_statement_counted(&statement) {
                Ok((rendered, count)) => {
                    println!("{rendered}");
                    eprintln!("{}", row_count_line(count));
                }
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

/// The `-- N rows` line printed to stderr after a REPL statement's result,
/// distinguishing a genuinely empty result from a mistake (REPL-only; batch
/// and `-e` mode never print this). Pulled out as a pure function so the
/// singular/plural wording is unit-tested without a TTY.
fn row_count_line(n: usize) -> String {
    format!("-- {n} row{}", if n == 1 { "" } else { "s" })
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
        DotCommand::Describe(field) => print_describe(session, field.as_deref()),
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
    println!("  .describe [field]  per-field types and coverage, or detail (values) for one field");
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

/// Prints `.describe`'s output to stdout: the detailed block for one field
/// when `field` names one, or a one-line summary of every field (plus the
/// `file.*` pseudo-columns) when it's `None`.
///
/// The `file.*` pseudo-columns are always `Str`-typed and always present, so
/// naming one directly (`.describe file.name`) gets a trivial one-line note
/// rather than the frontmatter-field detail block — and, per design, their
/// distinct-value counts are never computed (`file.path` is unbounded). A
/// name that is neither a frontmatter field nor a `file.*` column is an
/// error, printed to stderr.
fn print_describe(session: &Session, field: Option<&str>) {
    match field {
        Some(name) if FILE_COLUMNS.contains(&name) => print_describe_file_column(name),
        Some(name) => print_describe_field(session, name),
        None => print_describe_all(session),
    }
}

/// The detailed `.describe <field>` block: variant(s), non-null coverage,
/// and either the capped most-frequent-first value list or a bare distinct
/// count when the field is over the cap.
fn print_describe_field(session: &Session, name: &str) {
    let report = session.describe();
    let Some(stat) = report.get(name) else {
        eprintln!("querymatter: unknown field '{name}' (try .schema to list fields)");
        return;
    };
    println!("{name}:");
    println!("  type: {}", variants_display(&stat.variants));
    println!(
        "  non_null: {}/{} ({}%)",
        stat.non_null,
        stat.total,
        coverage_pct(stat)
    );
    match &stat.values {
        Some(values) if !values.is_empty() => {
            let list = values
                .iter()
                .map(|(value, count)| format!("{value}({count})"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("  values: {list}");
        }
        // A field null in every record has nothing to list — its coverage
        // line above already says so.
        Some(_) => {}
        None => println!("  {} distinct values", stat.distinct),
    }
}

/// The trivial `.describe file.*` block: these pseudo-columns are always
/// `Str`-typed and always present, so there's no coverage or value tally
/// worth computing — least of all for `file.path`, whose distinct values are
/// effectively unbounded.
fn print_describe_file_column(name: &str) {
    println!("{name}: (file.*) always present, type Str, 100% coverage");
}

/// `.describe`'s no-argument summary: one aligned line per frontmatter
/// field's type and coverage, followed by the `file.*` pseudo-columns noted
/// as `(file.*)`.
fn print_describe_all(session: &Session) {
    let report = session.describe();
    let width = report
        .keys()
        .map(String::len)
        .chain(FILE_COLUMNS.iter().map(|c| c.len()))
        .max()
        .unwrap_or(0);

    for (name, stat) in &report {
        println!(
            "  {name:width$}  {:<12}  {}/{} ({}%)",
            variants_display(&stat.variants),
            stat.non_null,
            stat.total,
            coverage_pct(stat),
        );
    }
    for column in FILE_COLUMNS {
        println!("  {column:width$}  (file.*)");
    }
}

/// `stat`'s non-null coverage as an integer percentage: `100` when every
/// record has a non-null value, `0` when none do (including when `total` is
/// `0`, which can't currently happen but is handled rather than panicking).
fn coverage_pct(stat: &FieldStat) -> u64 {
    if stat.total == 0 {
        return 0;
    }
    (stat.non_null as u64 * 100) / stat.total as u64
}

/// Renders a field's variant set for `.describe`, e.g. `Str` or `Int, Str`
/// for a mixed-type field.
fn variants_display(variants: &BTreeSet<&'static str>) -> String {
    variants.iter().copied().collect::<Vec<_>>().join(", ")
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
fn load_history(editor: &mut Editor<ReplHelper, FileHistory>, path: &Path) {
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

/// The word ending at byte offset `pos` in `line` — its start index and text
/// — found by scanning back to the previous whitespace character, or the
/// start of the line. `pos` is walked back to the nearest char boundary at or
/// before itself first, so a `pos` that (unexpectedly) isn't one still can't
/// panic the slicing below.
fn current_word(line: &str, pos: usize) -> (usize, &str) {
    let mut pos = pos.min(line.len());
    while pos > 0 && !line.is_char_boundary(pos) {
        pos -= 1;
    }
    let start = line[..pos]
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    (start, &line[start..pos])
}

/// True when the word starting at `start` is the key argument of a `.set` or
/// `.unset` dot-command — i.e. exactly one word precedes it, and that word is
/// `.set`/`.unset` (case-insensitively). A later word (the *value* half of
/// `.set <key> <value>`) is deliberately excluded: only the key is a closed
/// set worth completing.
fn is_set_or_unset_key_position(line: &str, start: usize) -> bool {
    let mut words = line[..start].split_whitespace();
    let Some(command) = words.next() else {
        return false;
    };
    words.next().is_none() && matches!(command.to_ascii_lowercase().as_str(), ".set" | ".unset")
}

/// The entries of `candidates` that start with `prefix`, owned so the result
/// can outlive whichever borrow produced it.
fn filter_prefix<'a>(candidates: impl Iterator<Item = &'a str>, prefix: &str) -> Vec<String> {
    candidates
        .filter(|name| name.starts_with(prefix))
        .map(str::to_string)
        .collect()
}

/// Tab-completion candidates for the REPL's input `line` with the cursor at
/// byte offset `pos`, given a snapshot of the frontmatter `schema` and the
/// dot-command names to offer. Pure and independent of rustyline, so it's
/// directly unit-testable; [`ReplHelper::complete`] is a thin adapter onto
/// rustyline's `Completer` trait.
///
/// Dispatches on the current word (see [`current_word`]) and its context:
/// the command word of a `.`-prefixed line completes against `dot_names`;
/// the key word of `.set`/`.unset` completes against [`ConfigKey::ALL`];
/// anything else — a bare word in SQL position — completes against `schema`
/// plus [`FILE_COLUMNS`], and never against SQL keywords. This last case is
/// intentionally approximate (it doesn't parse SQL to know whether a column
/// name even belongs where the cursor is) but harmless either way: rustyline
/// only replaces the word when the user actually accepts a candidate, so an
/// empty or wrong-context result just means nothing is offered.
///
/// A future sub-project may add saved-query-name completion; the SQL-word
/// branch is the natural place to fold that in once saved queries exist.
fn complete_candidates(
    line: &str,
    pos: usize,
    schema: &[String],
    dot_names: &[&str],
) -> Vec<String> {
    let (start, word) = current_word(line, pos);

    if start == 0 && word.starts_with('.') {
        return filter_prefix(dot_names.iter().copied(), word);
    }
    if is_set_or_unset_key_position(line, start) {
        return filter_prefix(ConfigKey::ALL.iter().map(|key| key.as_str()), word);
    }
    filter_prefix(schema.iter().map(String::as_str).chain(FILE_COLUMNS), word)
}

/// rustyline `Helper`: wires [`complete_candidates`] into the editor as
/// tab-completion, leaving hinting/highlighting/validation as no-ops (their
/// trait defaults, brought in via `#[derive]`).
///
/// `schema` is a one-time snapshot of [`Session::schema`], taken in [`run`]
/// when the REPL starts. It does *not* refresh after `.reload`/`.refresh`/
/// `.refresh-all`, so a frontmatter field discovered mid-session won't
/// tab-complete until the REPL is restarted — acceptable for v1; revisit if
/// it's ever a real friction point.
#[derive(Helper, Hinter, Highlighter, Validator)]
struct ReplHelper {
    schema: Vec<String>,
}

impl Completer for ReplHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &RlContext<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, _) = current_word(line, pos);
        let candidates = complete_candidates(line, pos, &self.schema, DOT_COMMAND_NAMES)
            .into_iter()
            .map(|text| Pair {
                display: text.clone(),
                replacement: text,
            })
            .collect();
        Ok((start, candidates))
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
    fn parse_dot_describe() {
        assert_eq!(
            parse_dot(".describe status"),
            DotCommand::Describe(Some("status".into()))
        );
        assert_eq!(parse_dot(".describe"), DotCommand::Describe(None));
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
    fn coverage_pct_edges() {
        let stat = |non_null, total| FieldStat {
            variants: BTreeSet::new(),
            non_null,
            total,
            values: None,
            distinct: 0,
        };
        assert_eq!(coverage_pct(&stat(4, 4)), 100, "all non-null is 100%");
        assert_eq!(coverage_pct(&stat(0, 4)), 0, "all null is 0%");
        assert_eq!(coverage_pct(&stat(3, 4)), 75);
        assert_eq!(
            coverage_pct(&stat(0, 0)),
            0,
            "no records must not divide by zero"
        );
    }

    #[test]
    fn row_count_line_pluralizes_correctly() {
        assert_eq!(row_count_line(0), "-- 0 rows");
        assert_eq!(row_count_line(1), "-- 1 row");
        assert_eq!(row_count_line(2), "-- 2 rows");
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

    #[test]
    fn completion_candidates_by_position() {
        let schema = vec!["status".to_string(), "prd".to_string()];
        // dot-command completion after '.'
        let c = complete_candidates(".sc", 3, &schema, DOT_COMMAND_NAMES);
        assert!(c.iter().any(|x| x == ".schema"));
        // config-key completion after '.set '
        let c = complete_candidates(".set for", 8, &schema, DOT_COMMAND_NAMES);
        assert!(c.iter().any(|x| x == "format"));
        // column completion for a bare word in SQL position
        let c = complete_candidates("SELECT sta", 10, &schema, DOT_COMMAND_NAMES);
        assert!(c.iter().any(|x| x == "status"));
        // no SQL-keyword noise
        assert!(!c.iter().any(|x| x.eq_ignore_ascii_case("select")));
    }

    /// Every name [`DOT_COMMAND_NAMES`] lists must actually parse to
    /// something other than `Unknown` — the drift guard `parse_dot`'s leading
    /// check (and its trailing `unreachable!()`) depends on.
    #[test]
    fn dot_command_names_all_parse() {
        for name in DOT_COMMAND_NAMES {
            assert!(
                !matches!(parse_dot(name), DotCommand::Unknown(_)),
                "{name} is in DOT_COMMAND_NAMES but parse_dot doesn't recognize it"
            );
        }
    }

    #[test]
    fn completion_offers_file_columns_and_unset_keys_too() {
        let schema = vec!["status".to_string()];
        // file.* pseudo-columns complete alongside schema fields.
        let c = complete_candidates("SELECT file.n", 13, &schema, DOT_COMMAND_NAMES);
        assert!(c.iter().any(|x| x == "file.name"));
        // .unset gets the same key completion as .set.
        let c = complete_candidates(".unset hi", 9, &schema, DOT_COMMAND_NAMES);
        assert!(c.iter().any(|x| x == "hidden"));
    }

    /// Only the word immediately after `.set`/`.unset` is a key candidate —
    /// a later word (the third word overall, i.e. the *value*) must not
    /// trigger config-key completion. Using a value prefix ("fo") that a real
    /// key ("format") also starts with makes this a real regression check,
    /// not a vacuous one.
    #[test]
    fn completion_does_not_offer_keys_for_the_value_word() {
        let schema = vec!["status".to_string()];
        let line = ".set table_style fo";
        let c = complete_candidates(line, line.len(), &schema, DOT_COMMAND_NAMES);
        assert!(!c.iter().any(|x| x == "format"));
    }

    /// A `.`-looking word that isn't the *first* word of the line (e.g. a
    /// stray value starting with a dot) must not be treated as a
    /// dot-command: only `start == 0` triggers that branch.
    #[test]
    fn completion_dot_prefix_only_matters_as_the_first_word() {
        let schema = vec!["status".to_string()];
        let line = "SELECT .sc";
        let c = complete_candidates(line, line.len(), &schema, DOT_COMMAND_NAMES);
        assert!(
            !c.iter().any(|x| x == ".schema"),
            "a `.`-looking word that isn't the first word must not complete as a \
             dot-command: {c:?}"
        );
    }

    /// A non-char-boundary or out-of-range `pos` must never panic — only
    /// rustyline is expected to hand out valid positions, but the completer
    /// must be defensive regardless.
    #[test]
    fn current_word_never_panics_on_bad_positions() {
        let line = "SELECT 'caf\u{e9}' WHERE x";
        for pos in 0..=line.len() + 5 {
            let _ = current_word(line, pos);
        }
    }

    #[test]
    fn repl_helper_complete_delegates_to_complete_candidates() {
        let helper = ReplHelper {
            schema: vec!["status".to_string()],
        };
        let history = FileHistory::new();
        let ctx = RlContext::new(&history);
        let (start, pairs) = helper
            .complete(".sc", 3, &ctx)
            .expect("completion never errors");
        assert_eq!(start, 0);
        assert!(pairs.iter().any(|p| p.replacement == ".schema"));
    }
}
