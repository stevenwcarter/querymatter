//! The interactive REPL: a rustyline-based shell for querymatter, with
//! multi-line `;`-terminated statements, dot-commands, and history.
//!
//! Line *parsing* — what a chunk of raw input resolves to — is split from
//! rustyline's IO loop so it can be unit-tested without a terminal: see
//! [`Line`], [`DotCommand`], [`LineBuffer`], and [`parse_dot`]. [`run`] is
//! the IO driver built on top of them.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use directories::ProjectDirs;
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::history::FileHistory;
use rustyline::{Context as RlContext, Editor, Helper, Highlighter, Hinter, Validator};

use crate::config::ConfigKey;
use crate::model::Value;
use crate::output::OutputSink;
use crate::queries::{self, Queries};
use crate::render::{Format, TableStyle};
use crate::session::{FieldStat, Session, Statement, Terminator, split_statements};
use crate::store::LoadReport;

/// Prompt shown while waiting for a new statement or dot-command.
const PROMPT: &str = "querymatter> ";
/// Prompt shown while a statement is still accumulating (no `;`, `\g`, or
/// `\G` yet).
const CONTINUATION_PROMPT: &str = "   ...> ";
/// The `file.*` pseudo-columns every record exposes, independent of
/// frontmatter (kept in sync with [`crate::model::FileAttr`]'s labels).
const FILE_COLUMNS: [&str; 6] = [
    "file.name",
    "file.path",
    "file.folder",
    "file.ext",
    "file.mtime",
    "file.size",
];
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
    ".header",
    ".timer",
    ".output",
    ".settings",
    ".set",
    ".unset",
    ".reload",
    ".refresh",
    ".refresh-all",
    ".query",
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
    /// `.header [on|off]` — set (`Some`) or report (`None`) whether results
    /// include a header row.
    Header(Option<bool>),
    /// `.timer [on|off]` — set (`Some`) or report (`None`) whether the
    /// `-- N rows` line includes the query's elapsed time.
    Timer(Option<bool>),
    /// `.output [path]` or `.output |cmd` — redirect subsequent statement
    /// results to `path`, or pipe them through a shell command when the
    /// argument starts with `|` (the sqlite3 convention, e.g.
    /// `.output |less`); both cases carried as `Some`. Reset to stdout
    /// (`None`) from a bare `.output` or an explicit `.output stdout`.
    Output(Option<String>),
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
    /// `.query run <name>` / `.query list` / `.query save <name> [sql]` —
    /// run, list, or save saved queries (see [`crate::queries`]). `.query
    /// save` with `sql` omitted defaults to the last successfully-run
    /// statement of this session.
    Query(QueryCmd),
    /// `.query <word>` where `<word>` is neither `run` nor `list`, carrying
    /// the offending word so the error can name it rather than the command.
    BadQueryAction(String),
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

/// A parsed `.query` action, carried by [`DotCommand::Query`].
#[derive(Debug, Clone, PartialEq)]
pub enum QueryCmd {
    /// `.query run <name>` — run a saved query in-session.
    Run(String),
    /// `.query list` — list every saved query's name and SQL.
    List,
    /// `.query save <name> [sql]` — persist `sql` under `name` (see
    /// [`crate::queries`]), or, when omitted (`None`), the last
    /// successfully-run statement of this session.
    Save(String, Option<String>),
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
        "header" => match parse_on_off(words.next()) {
            Some(value) => DotCommand::Header(value),
            None => DotCommand::Unknown(line.to_string()),
        },
        "timer" => match parse_on_off(words.next()) {
            Some(value) => DotCommand::Timer(value),
            None => DotCommand::Unknown(line.to_string()),
        },
        // Verbatim (not `words.next()`) so `.output |column -t -s,` or
        // `.output |grep x | wc -l` survive with their internal spaces intact
        // — a whitespace split would truncate the piped command at its first
        // argument.
        "output" => match rest_after_key(rest, 1) {
            None => DotCommand::Output(None),
            Some(arg) if arg.eq_ignore_ascii_case("stdout") => DotCommand::Output(None),
            Some(arg) => DotCommand::Output(Some(arg)),
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
        "query" => match words.next() {
            Some(action) if action.eq_ignore_ascii_case("list") => {
                DotCommand::Query(QueryCmd::List)
            }
            Some(action) if action.eq_ignore_ascii_case("run") => match words.next() {
                Some(name) => DotCommand::Query(QueryCmd::Run(name.to_string())),
                None => DotCommand::MissingArg("query"),
            },
            Some(action) if action.eq_ignore_ascii_case("save") => match words.next() {
                Some(name) => {
                    // SQL is everything after `query save <name>`, taken
                    // verbatim (may be absent) — like `.set`'s value capture,
                    // so a multi-word/multi-clause SQL survives untouched.
                    let sql = rest_after_key(rest, 3);
                    DotCommand::Query(QueryCmd::Save(name.to_string(), sql))
                }
                None => DotCommand::MissingArg("query"),
            },
            Some(action) => DotCommand::BadQueryAction(action.to_string()),
            None => DotCommand::MissingArg("query"),
        },
        _ => unreachable!("cmd already checked against DOT_COMMAND_NAMES above"),
    }
}

/// Parses an optional `on`/`off` argument (case-insensitive), shared by every
/// dot-command that toggles a boolean setting — `.header` and `.timer`. The
/// outer `Option` is `None` for an unrecognized argument
/// (any word but `on`/`off`), which the caller maps to an error; the inner
/// `Option` is `None` for a bare command with no argument at all, meaning
/// "report the current state" rather than change it.
fn parse_on_off(arg: Option<&str>) -> Option<Option<bool>> {
    match arg {
        None => Some(None),
        Some(a) if a.eq_ignore_ascii_case("on") => Some(Some(true)),
        Some(a) if a.eq_ignore_ascii_case("off") => Some(Some(false)),
        Some(_) => None,
    }
}

/// Parses a config key name, accepting exactly the spellings `ConfigKey`
/// declares — the same ones the TOML file and `querymatter config` use.
fn parse_key(name: &str) -> Option<ConfigKey> {
    ConfigKey::ALL.into_iter().find(|key| key.as_str() == name)
}

/// Everything after the first `skip` whitespace-separated words of `rest`,
/// trimmed — the value of a `.set` (or a `.output` target), taken verbatim so
/// globs, commas, and piped-command arguments survive. `None` when there are
/// fewer than `skip + 1` words.
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
/// without it. Tab-completion is wired via [`ReplHelper`], whose schema and
/// saved-query-name snapshots are taken here, once, at start-up — a saved
/// query added mid-session (e.g. via `querymatter query save` in another
/// terminal) won't tab-complete until the REPL restarts, matching `schema`'s
/// existing snapshot-not-live-refresh behavior. A malformed `queries.toml` is
/// a warning here, not fatal: completion just offers no saved-query names,
/// same as an empty file would. Each statement's rendered result goes to an
/// [`OutputSink`] that starts on stdout and is redirected by `.output` for
/// the rest of the session; the `-- N rows` line and every error stay on
/// stderr regardless of where the sink points.
pub fn run(mut session: Session) -> anyhow::Result<()> {
    let helper = ReplHelper {
        schema: session.schema(),
        query_names: saved_query_names(),
    };
    let mut editor: Editor<ReplHelper, FileHistory> =
        Editor::new().context("failed to initialize the line editor")?;
    editor.set_helper(Some(helper));
    let history_path = history_path();
    if let Some(path) = &history_path {
        prepare_history_dir(path);
        load_history(&mut editor, path);
    }

    println!("{}", banner(record_count(&session)));

    let mut buffer = LineBuffer::new();
    let mut sink = OutputSink::Stdout;
    // The last successfully-run statement's SQL this session — `.query save`
    // with no inline SQL defaults to it (W46).
    let mut last_sql: Option<String> = None;
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
            Line::Statement(statement) => {
                run_statement(&session, &statement, &mut sink);
                last_sql = Some(statement.sql.clone());
            }
            Line::Dot(cmd, _) => {
                if dispatch_dot(cmd, &mut session, &mut sink, last_sql.as_deref()) {
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
    // Reap a piped command still running when the session ends (`.quit`/EOF)
    // so its process doesn't outlive the REPL with a dangling stdin.
    if let Err(err) = sink.finish() {
        eprintln!("querymatter: error finishing output: {err}");
    }
    Ok(())
}

/// Runs one statement against `session`, writing its rendered result to
/// `sink` and the `-- N rows` line to stderr, or the error to stderr on
/// failure. Shared by the main loop (a directly typed statement) and
/// [`dispatch_query`] (each statement inside a `.query run <name>`'s saved
/// SQL), so both paths format results identically.
fn run_statement(session: &Session, statement: &Statement, sink: &mut OutputSink) {
    let start = Instant::now();
    match session.render_statement_counted(statement) {
        Ok((rendered, count)) => {
            if let Err(err) = sink.write_block(&rendered) {
                eprintln!("querymatter: failed to write results: {err}");
            }
            let elapsed = session.timer().then(|| start.elapsed());
            eprintln!("{}", row_count_line(count, elapsed));
        }
        Err(err) => eprintln!("querymatter: {err:#}"),
    }
}

/// The `-- N rows` line printed to stderr after a REPL statement's result,
/// distinguishing a genuinely empty result from a mistake (REPL-only; batch
/// and `-e` mode never print this). Pulled out as a pure function so the
/// singular/plural wording is unit-tested without a TTY. `elapsed` is
/// `Some` only when `.timer` is on, in which case the query's wall-clock
/// time is appended in seconds to 3 decimal places; `None` reproduces
/// exactly the untimed line.
fn row_count_line(n: usize, elapsed: Option<Duration>) -> String {
    let plural = if n == 1 { "" } else { "s" };
    match elapsed {
        Some(d) => format!("-- {n} row{plural} ({:.3}s)", d.as_secs_f64()),
        None => format!("-- {n} row{plural}"),
    }
}

/// The startup banner [`run`] prints to stdout on entering the REPL: the
/// record count plus a `.help`/`.schema` hint. Pulled out as a pure function
/// so its content is unit-tested without a TTY; REPL-only — batch and `-e`
/// mode never print it.
fn banner(record_count: i64) -> String {
    format!("querymatter — {record_count} records. Type .help for commands, .schema for fields.")
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
/// continuations record nothing. A statement typed with a `\g` terminator is
/// re-emitted with a `;` instead (both are [`Terminator::Semicolon`]) —
/// harmless, since the two run identically, but worth knowing if a `\g`
/// history entry ever looks different from what was typed.
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

/// Runs one dot-command against `session`, redirecting `sink` on `.output`,
/// and returning `true` when the REPL should exit (`.quit`/`.exit`).
///
/// stdout/stderr policy: reference/inspection output (`.help`, `.schema`,
/// `.settings`, `.query list`, and `.format`'s/`.style`'s/`.header`'s/
/// `.timer`'s reports of the current format/style/header/timer setting) goes
/// to stdout; the `.reload`/`.refresh`/`.refresh-all` reports, `.set`/`.unset`
/// confirmations, `.output`'s confirmation, `.query save`'s confirmation, and
/// all error messages (unknown command, bad format, bad style, bad key,
/// missing argument, unknown saved-query name) go to stderr, keeping stdout
/// clean for piping. `.query run <name>`'s query results follow a typed
/// statement's own policy instead — the rendered result goes wherever `sink`
/// points, and the `-- N rows` line stays on stderr (see [`run_statement`]).
///
/// `last_sql` is the last successfully-run statement's SQL this session (see
/// [`run`]'s `last_sql` local), threaded through purely so `.query save` with
/// no inline SQL can fall back to it.
fn dispatch_dot(
    cmd: DotCommand,
    session: &mut Session,
    sink: &mut OutputSink,
    last_sql: Option<&str>,
) -> bool {
    match cmd {
        DotCommand::Help => print_help(),
        DotCommand::Schema => print_schema(session),
        DotCommand::Describe(field) => print_describe(session, field.as_deref()),
        DotCommand::Format(Some(fmt)) => session.set_format(fmt),
        DotCommand::Format(None) => println!("format: {}", format_name(session.format())),
        DotCommand::Style(Some(style)) => session.set_style(style),
        DotCommand::Style(None) => println!("style: {}", style_name(session.style())),
        DotCommand::Header(Some(on)) => session.set_header(on),
        DotCommand::Header(None) => {
            println!("header: {}", if session.header() { "on" } else { "off" });
        }
        DotCommand::Timer(Some(on)) => session.set_timer(on),
        DotCommand::Timer(None) => {
            println!("timer: {}", if session.timer() { "on" } else { "off" });
        }
        DotCommand::Output(target) => apply_output(sink, target),
        DotCommand::Reload => report_reload(session),
        DotCommand::Refresh(path) => report_refresh(session, path.as_deref().map(Path::new)),
        DotCommand::RefreshAll => report_refresh(session, None),
        DotCommand::Query(query_cmd) => dispatch_query(query_cmd, session, sink, last_sql),
        DotCommand::Quit => return true,
        DotCommand::BadFormat(name) => {
            eprintln!("querymatter: unknown format '{name}' (try: table, json, csv, tsv, md)");
        }
        DotCommand::BadStyle(name) => {
            eprintln!("querymatter: unknown style '{name}' (try: ascii, unicode, compact, plain)");
        }
        DotCommand::BadQueryAction(name) => {
            eprintln!(
                "querymatter: unknown '.query {name}' (try: .query run <name>, .query list, \
                 .query save <name> [sql])"
            );
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
            "unset" => eprintln!("querymatter: usage: .unset <key>"),
            "query" => eprintln!(
                "querymatter: usage: .query run <name> | .query list | .query save <name> [sql]"
            ),
            _ => unreachable!("every MissingArg cmd name is handled above"),
        },
        DotCommand::Unknown(raw) => {
            eprintln!("querymatter: unknown command {raw:?} (try .help)");
        }
    }
    false
}

/// Runs a `.query` dot-command.
///
/// `.query save` is dispatched straight to [`dispatch_query_save`] — it
/// writes `queries.toml` itself (via [`crate::save_named_query`]) rather than
/// reading the snapshot this function loads for `.query list`/`.query run`,
/// so there's no point loading it twice. Those two load `queries.toml` via
/// [`queries::load`] and delegate the resolve-and-run decision to
/// [`run_query_cmd`]; a malformed `queries.toml` is reported on stderr, not a
/// panic.
fn dispatch_query(cmd: QueryCmd, session: &Session, sink: &mut OutputSink, last_sql: Option<&str>) {
    if let QueryCmd::Save(name, sql) = cmd {
        dispatch_query_save(&name, sql, last_sql);
        return;
    }
    match queries::load() {
        Ok(saved) => run_query_cmd(cmd, &saved, session, sink),
        Err(err) => eprintln!("querymatter: {err:#}"),
    }
}

/// `.query save <name> [sql]`'s dispatch: resolves the SQL to persist via
/// [`resolve_save_sql`] — the inline `sql` when given, else `last_sql` (the
/// last successfully-run statement of this session) — then validates and
/// writes it through [`crate::save_named_query`], the SAME helper
/// `querymatter query save` uses, so the CLI and the REPL can never validate
/// or persist differently. Neither an inline SQL nor a prior statement is a
/// clean stderr error, matching `.set`'s confirmation policy; nothing is
/// written in that case.
fn dispatch_query_save(name: &str, sql: Option<String>, last_sql: Option<&str>) {
    match resolve_save_sql(sql, last_sql) {
        None => {
            eprintln!("querymatter: .query save: no SQL given and no statement has run yet");
        }
        Some(sql) => match crate::save_named_query(name, &sql) {
            Ok(path) => eprintln!("querymatter: saved query '{name}' in {}", path.display()),
            Err(err) => eprintln!("querymatter: {err:#}"),
        },
    }
}

/// Resolves the SQL for `.query save <name> [sql]`: the inline `sql` when
/// given, falling back to `last_sql` — the last successfully-run statement of
/// this session — when omitted. `None` means neither was available, i.e.
/// `.query save <name>` was typed with no statement having run yet this
/// session; pulled out as a pure function so that "no SQL to save" condition
/// is unit-testable without touching `queries.toml`.
fn resolve_save_sql(sql: Option<String>, last_sql: Option<&str>) -> Option<String> {
    sql.or_else(|| last_sql.map(str::to_string))
}

/// The `.query` dot-command's resolve-and-run decision, taking already-loaded
/// `saved` queries directly rather than reading `queries.toml` itself — split
/// out from [`dispatch_query`] so it's unit-testable against an in-memory
/// [`Queries`] map, with no filesystem/env involved.
///
/// `.query list` prints every saved query's name and SQL to stdout.
/// `.query run <name>` resolves `name` in `saved` and runs its SQL in-session
/// through [`run_statement`] — since a saved query may itself contain several
/// `;`-separated statements, each is split out via [`split_statements`] and
/// run (and rendered to `sink`) in order, exactly like the REPL's own
/// multi-line statement handling. An unknown name is reported on stderr, not
/// a panic. `.query save` is never actually passed here — [`dispatch_query`]
/// intercepts it before loading `saved` at all — but the variant must still
/// be matched for exhaustiveness.
fn run_query_cmd(cmd: QueryCmd, saved: &Queries, session: &Session, sink: &mut OutputSink) {
    match cmd {
        QueryCmd::List => print!("{}", query_list_lines(saved)),
        QueryCmd::Run(name) => match queries::get(saved, &name) {
            Some(sql) => {
                for statement in split_statements(sql) {
                    run_statement(session, &statement, sink);
                }
            }
            None => eprintln!("querymatter: no saved query named '{name}'"),
        },
        QueryCmd::Save(..) => {
            unreachable!("Save is dispatched separately, before queries::load — see dispatch_query")
        }
    }
}

/// `.query list`'s output: each saved query as one `name\tsql` line
/// (name-sorted, mirroring [`Queries::iter`]), with a trailing newline after
/// every line including the last. Pulled out as a pure function — like
/// [`banner`]/[`help_text`] — so it's unit-tested without capturing stdout.
fn query_list_lines(saved: &Queries) -> String {
    let mut out = String::new();
    for (name, sql) in saved.iter() {
        writeln!(out, "{name}\t{sql}").expect("String write is infallible");
    }
    out
}

/// Every saved query's name, for [`ReplHelper`]'s tab-completion snapshot. A
/// malformed `queries.toml` is a warning here, not fatal — the REPL still
/// starts, just with no saved-query names to offer.
fn saved_query_names() -> Vec<String> {
    match queries::load() {
        Ok(saved) => saved.names().map(String::from).collect(),
        Err(err) => {
            eprintln!("querymatter: warning: {err:#}");
            Vec::new()
        }
    }
}

/// Applies a `.output` dot-command to `sink`: `Some(path)` truncates and
/// opens `path`, `Some("|cmd")` (the sqlite3 convention) pipes every later
/// statement's result through `cmd` via the shell, and `None` (a bare
/// `.output` or `.output stdout`) resets to stdout. Either way a one-line
/// confirmation goes to stderr.
///
/// The new target is opened (or spawned) *first*, without touching the old
/// sink at all. Only once that succeeds is [`OutputSink::finish`] called on
/// the *old* sink — closing and reaping a prior piped command — and `sink`
/// swapped to the new value. A path that can't be opened, or a command that
/// can't be spawned, is reported on stderr instead, and `sink` is left
/// completely untouched: even a live piped command keeps running and
/// accepting writes exactly as before the failed switch, since it was never
/// finished in the first place.
fn apply_output(sink: &mut OutputSink, target: Option<String>) {
    match target {
        None => {
            finish_old_sink(sink);
            *sink = OutputSink::Stdout;
            eprintln!("querymatter: results on stdout");
        }
        Some(arg) => match arg.strip_prefix('|') {
            Some(cmd) => match OutputSink::open_command(cmd.trim()) {
                Ok(opened) => {
                    finish_old_sink(sink);
                    *sink = opened;
                    eprintln!("querymatter: piping results through {}", cmd.trim());
                }
                Err(err) => eprintln!("querymatter: cannot start pipe: {err}"),
            },
            None => match OutputSink::open_file(Path::new(&arg)) {
                Ok(opened) => {
                    finish_old_sink(sink);
                    *sink = opened;
                    eprintln!("querymatter: writing results to {arg}");
                }
                Err(err) => eprintln!("querymatter: cannot write to {arg}: {err}"),
            },
        },
    }
}

/// Finishes `sink` (closing and reaping a live piped command; a no-op for
/// [`OutputSink::Stdout`]/[`OutputSink::File`]) now that a replacement has
/// already been opened successfully — [`apply_output`]'s shared "the switch
/// is confirmed, retire the old target" step. A failure here is reported but
/// not fatal: the caller is about to overwrite `sink` regardless.
fn finish_old_sink(sink: &mut OutputSink) {
    if let Err(err) = sink.finish() {
        eprintln!("querymatter: error finishing previous output: {err}");
    }
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
    print!("{}", help_text());
}

/// The dot-command reference text `print_help` prints, pulled out as a pure
/// function so a test can assert every name in [`DOT_COMMAND_NAMES`]
/// actually appears here — a future command that's tab-completable but
/// missing from this listing would otherwise go unnoticed.
fn help_text() -> String {
    let mut text = String::new();
    let _ = writeln!(text, "Dot-commands:");
    let _ = writeln!(text, "  .help              show this message");
    let _ = writeln!(
        text,
        "  .schema            list frontmatter fields, file.* columns, and the record count"
    );
    let _ = writeln!(
        text,
        "  .describe [field]  per-field types and coverage, or detail (values) for one field"
    );
    let _ = writeln!(
        text,
        "  .format [fmt]      show, or set, the output format (table, json, csv, tsv, md)"
    );
    let _ = writeln!(
        text,
        "  .style [style]     show, or set, the table border style (ascii, unicode, compact, plain)"
    );
    let _ = writeln!(
        text,
        "  .header [on|off]   show, or set, whether results include a header row"
    );
    let _ = writeln!(
        text,
        "  .timer [on|off]    show, or set, whether the row-count line shows elapsed query time"
    );
    let _ = writeln!(
        text,
        "  .output [path]     redirect results to path, '|cmd' to pipe through a shell command, or 'stdout'/no argument to reset"
    );
    let _ = writeln!(
        text,
        "  .settings          list every setting, its value, and where it came from"
    );
    let _ = writeln!(
        text,
        "  .set <key> <val>   save a setting to the config file"
    );
    let _ = writeln!(
        text,
        "  .unset <key>       remove a setting from the config file"
    );
    let _ = writeln!(
        text,
        "  .reload            rescan every tracked directory (in-memory only)"
    );
    let _ = writeln!(
        text,
        "  .refresh [path]    re-scan path (or all); updates the .querymatter cache, else in memory"
    );
    let _ = writeln!(
        text,
        "  .refresh-all       re-scan the whole vault; updates the cache, or in memory with no vault"
    );
    let _ = writeln!(
        text,
        "  .query run <name>         run a saved query in-session (see 'querymatter query save')"
    );
    let _ = writeln!(
        text,
        "  .query list               list saved queries (name and SQL)"
    );
    let _ = writeln!(
        text,
        "  .query save <name> [sql]  save sql, or the last statement run, as name"
    );
    let _ = writeln!(text, "  .quit / .exit      leave the REPL");
    let _ = writeln!(text);
    let _ = writeln!(
        text,
        "End a statement with ';' to run it, or with '\\G' to print each row as a block of"
    );
    let _ = writeln!(
        text,
        "name: value lines; '\\g' is a synonym for ';'. Statements may span multiple lines."
    );
    text
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
/// The `file.*` pseudo-columns are always present, so naming one directly
/// (`.describe file.name`) gets a trivial one-line note rather than the
/// frontmatter-field detail block — and, per design, their distinct-value
/// counts are never computed (`file.path` is unbounded). Their type is fixed
/// per column rather than uniformly `Str`: `file.size` is `Int`, every other
/// `file.*` column (including `file.mtime`, an ISO-8601 UTC string) is
/// `Str` — see [`describe_file_column_line`]. A name that is neither a
/// frontmatter field nor a `file.*` column is an error, printed to stderr.
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
/// present with a fixed, per-column type — `file.size` is `Int`, every other
/// `file.*` column is `Str` — so there's no coverage or value tally worth
/// computing — least of all for `file.path`, whose distinct values are
/// effectively unbounded.
fn print_describe_file_column(name: &str) {
    println!("{}", describe_file_column_line(name));
}

/// Builds [`print_describe_file_column`]'s one-line text; split out so the
/// per-column type note is unit-testable without capturing stdout.
fn describe_file_column_line(name: &str) -> String {
    let ty = if name == "file.size" { "Int" } else { "Str" };
    format!("{name}: (file.*) always present, type {ty}, 100% coverage")
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

/// True when the word starting at `start` is the `<name>` argument of a
/// `.query run` dot-command — i.e. exactly two words precede it, `.query`
/// then `run` (both case-insensitively, matching [`parse_dot`]'s own
/// case-folding of the command name and action word).
fn is_query_run_name_position(line: &str, start: usize) -> bool {
    let mut words = line[..start].split_whitespace();
    let Some(command) = words.next() else {
        return false;
    };
    let Some(action) = words.next() else {
        return false;
    };
    words.next().is_none()
        && command.eq_ignore_ascii_case(".query")
        && action.eq_ignore_ascii_case("run")
}

/// The entries of `candidates` that start with `prefix`, owned so the result
/// can outlive whichever borrow produced it.
fn filter_prefix<'a>(candidates: impl Iterator<Item = &'a str>, prefix: &str) -> Vec<String> {
    candidates
        .filter(|name| name.starts_with(prefix))
        .map(str::to_string)
        .collect()
}

/// Case-insensitive variant of [`filter_prefix`], for dot-command and
/// config-key completion: [`parse_dot`] itself accepts `.SCHEMA` and
/// `.Schema` alike, so `.SC<tab>` completing nothing (while `.SCHEMA` runs
/// fine) would be a needless surprise. Column completion in SQL position
/// stays case-sensitive — field names genuinely are.
fn filter_prefix_ci<'a>(candidates: impl Iterator<Item = &'a str>, prefix: &str) -> Vec<String> {
    let prefix = prefix.to_ascii_lowercase();
    candidates
        .filter(|name| name.to_ascii_lowercase().starts_with(&prefix))
        .map(str::to_string)
        .collect()
}

/// Tab-completion candidates for the REPL's input `line` with the cursor at
/// byte offset `pos`, given a snapshot of the frontmatter `schema`, the
/// dot-command names to offer, and the saved-query `names` to offer after
/// `.query run `. Pure and independent of rustyline, so it's directly
/// unit-testable; [`ReplHelper::complete`] is a thin adapter onto rustyline's
/// `Completer` trait.
///
/// Dispatches on the current word (see [`current_word`]) and its context:
/// the command word of a `.`-prefixed line completes against `dot_names`;
/// the key word of `.set`/`.unset` completes against [`ConfigKey::ALL`]; the
/// `<name>` argument of `.query run` completes against `query_names`;
/// anything else — a bare word in SQL position — completes against `schema`
/// plus [`FILE_COLUMNS`], and never against SQL keywords. This last case is
/// intentionally approximate (it doesn't parse SQL to know whether a column
/// name even belongs where the cursor is) but harmless either way: rustyline
/// only replaces the word when the user actually accepts a candidate, so an
/// empty or wrong-context result just means nothing is offered.
fn complete_candidates(
    line: &str,
    pos: usize,
    schema: &[String],
    dot_names: &[&str],
    query_names: &[String],
) -> Vec<String> {
    let (start, word) = current_word(line, pos);

    if start == 0 && word.starts_with('.') {
        return filter_prefix_ci(dot_names.iter().copied(), word);
    }
    if is_set_or_unset_key_position(line, start) {
        return filter_prefix_ci(ConfigKey::ALL.iter().map(|key| key.as_str()), word);
    }
    if is_query_run_name_position(line, start) {
        return filter_prefix(query_names.iter().map(String::as_str), word);
    }
    filter_prefix(schema.iter().map(String::as_str).chain(FILE_COLUMNS), word)
}

/// rustyline `Helper`: wires [`complete_candidates`] into the editor as
/// tab-completion, leaving hinting/highlighting/validation as no-ops (their
/// trait defaults, brought in via `#[derive]`).
///
/// `schema` and `query_names` are one-time snapshots — of [`Session::schema`]
/// and [`saved_query_names`] respectively — taken in [`run`] when the REPL
/// starts. Neither refreshes after `.reload`/`.refresh`/`.refresh-all` (for
/// `schema`) or a saved query added mid-session (for `query_names`), so a
/// frontmatter field or saved query added after start won't tab-complete
/// until the REPL is restarted — acceptable for v1; revisit if it's ever a
/// real friction point.
#[derive(Helper, Hinter, Highlighter, Validator)]
struct ReplHelper {
    schema: Vec<String>,
    query_names: Vec<String>,
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
        let candidates = complete_candidates(
            line,
            pos,
            &self.schema,
            DOT_COMMAND_NAMES,
            &self.query_names,
        )
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
    use crate::discover::WalkOpts;
    use crate::render::Format;
    use crate::settings::Settings;
    use crate::store::InMemoryStore;
    use tempfile::tempdir;

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
    fn parses_header_on_off_and_report() {
        assert_eq!(parse_dot(".header on"), DotCommand::Header(Some(true)));
        assert_eq!(parse_dot(".header off"), DotCommand::Header(Some(false)));
        assert_eq!(parse_dot(".header"), DotCommand::Header(None));
    }

    /// An unrecognized `.header` argument is `Unknown` carrying the whole
    /// line — unlike `.format`/`.style`, whose bad-argument case names the
    /// offending value via a dedicated `BadFormat`/`BadStyle` variant. There's
    /// no `on`/`off` equivalent worth naming individually, so the existing
    /// unknown-command error path handles it directly.
    #[test]
    fn header_bad_arg_is_unknown_command() {
        assert_eq!(
            parse_dot(".header maybe"),
            DotCommand::Unknown(".header maybe".to_string())
        );
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
    fn query_run_and_list_parse() {
        assert_eq!(
            parse_dot(".query run stale"),
            DotCommand::Query(QueryCmd::Run("stale".to_string()))
        );
        assert_eq!(parse_dot(".query list"), DotCommand::Query(QueryCmd::List));
        // Case-insensitive on the action word, matching every other dot-command.
        assert_eq!(
            parse_dot(".query RUN stale"),
            DotCommand::Query(QueryCmd::Run("stale".to_string()))
        );
    }

    #[test]
    fn parses_query_save_with_and_without_sql() {
        assert_eq!(
            parse_dot(".query save stale SELECT status"),
            DotCommand::Query(QueryCmd::Save("stale".into(), Some("SELECT status".into())))
        );
        assert_eq!(
            parse_dot(".query save stale"),
            DotCommand::Query(QueryCmd::Save("stale".into(), None))
        );
    }

    /// [`resolve_save_sql`]'s two real inputs: an inline SQL always wins, and
    /// an omitted one falls back to `last_sql` (the last successfully-run
    /// statement of this session).
    #[test]
    fn resolve_save_sql_prefers_inline_sql_then_falls_back_to_last_sql() {
        assert_eq!(
            resolve_save_sql(Some("SELECT 1".to_string()), Some("SELECT 2")),
            Some("SELECT 1".to_string()),
            "inline SQL must win over last_sql when both are present"
        );
        assert_eq!(
            resolve_save_sql(None, Some("SELECT 2")),
            Some("SELECT 2".to_string()),
            "omitted inline SQL must fall back to last_sql"
        );
    }

    /// `.query save <name>` with no inline SQL and no statement having run
    /// yet this session must resolve to "nothing to save" — `dispatch_query_save`
    /// maps that straight to a clean stderr error and never calls
    /// `crate::save_named_query`, so nothing is ever written. Pinned at this
    /// pure resolution step so it's provable without any filesystem/env
    /// involvement; `save_named_query`'s own validate-then-persist behavior is
    /// exercised (identically for the CLI and, after this task, the REPL)
    /// by `tests/cli.rs`'s `query save` tests, isolated there via a
    /// subprocess with its own `HOME`.
    #[test]
    fn query_save_rejects_when_no_prior_statement_and_no_sql() {
        assert_eq!(resolve_save_sql(None, None), None);
    }

    /// `dispatch_dot` wires `.header off` through to `Session::set_header`,
    /// reusing the same `Session` builder the `.query` dispatch tests below
    /// use — the header setting defaults to `on`, so this also proves the
    /// dispatch actually flips it rather than the default coincidentally
    /// matching.
    #[test]
    fn dispatch_header_off_sets_session() {
        let td = tempdir().unwrap();
        fs::write(td.path().join("a.md"), "---\nstatus: draft\n---\n").unwrap();
        let mut session = query_cmd_test_session(td.path());
        assert!(session.header(), "header must default to on");
        let mut sink = OutputSink::Stdout;

        assert!(!dispatch_dot(
            DotCommand::Header(Some(false)),
            &mut session,
            &mut sink,
            None
        ));
        assert!(!session.header());
    }

    /// An unrecognized `.query` action word is `BadQueryAction` (reported as
    /// an unknown *action*), not an unknown command — mirroring
    /// `.format`/`.style`'s `BadFormat`/`BadStyle`.
    #[test]
    fn query_bad_action_is_bad_query_action_not_unknown_command() {
        match parse_dot(".query bogus") {
            DotCommand::BadQueryAction(name) => assert_eq!(name, "bogus"),
            other => panic!("expected BadQueryAction, got {other:?}"),
        }
    }

    #[test]
    fn query_missing_arguments_are_reported_as_missing() {
        assert_eq!(parse_dot(".query"), DotCommand::MissingArg("query"));
        assert_eq!(parse_dot(".query run"), DotCommand::MissingArg("query"));
    }

    /// A session over a tiny real store, for the `run_query_cmd` tests below
    /// (which need an actual queryable session, unlike the parsing tests
    /// above).
    fn query_cmd_test_session(dir: &std::path::Path) -> Session {
        let (store, _report) = InMemoryStore::load(
            vec![fs::canonicalize(dir).unwrap()],
            WalkOpts::default(),
            None,
        );
        Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            None,
        )
    }

    /// MUST-FIX: `run_query_cmd`'s `.query run <name>` resolution/execution,
    /// driven directly against a `Session` + an in-memory `Queries` map — no
    /// filesystem `queries.toml`, no PTY. Also pins that a saved query
    /// containing several `;`-separated statements is split and every
    /// statement's result reaches the sink, in order.
    #[test]
    fn run_query_cmd_run_resolves_and_executes_a_multi_statement_saved_query() {
        let td = tempdir().unwrap();
        fs::write(td.path().join("a.md"), "---\nstatus: draft\n---\n").unwrap();
        fs::write(td.path().join("b.md"), "---\nstatus: synced\n---\n").unwrap();
        let session = query_cmd_test_session(td.path());

        let mut saved = Queries::default();
        queries::set(
            &mut saved,
            "both",
            "SELECT status WHERE status = 'draft';\nSELECT status WHERE status = 'synced'",
        )
        .unwrap();

        let out_path = td.path().join("out.txt");
        let mut sink = OutputSink::open_file(&out_path).unwrap();
        run_query_cmd(
            QueryCmd::Run("both".to_string()),
            &saved,
            &session,
            &mut sink,
        );
        drop(sink);

        let body = fs::read_to_string(&out_path).unwrap();
        assert!(
            body.contains("draft"),
            "missing first statement's result: {body}"
        );
        assert!(
            body.contains("synced"),
            "missing second statement's result: {body}"
        );
    }

    /// An unknown `.query run` name must not run anything — the sink stays
    /// completely untouched (the error itself goes to stderr, matching every
    /// other REPL error, and is covered by `queries::get_is_none_for_an_unknown_name`
    /// for the resolution step itself).
    #[test]
    fn run_query_cmd_run_unknown_name_touches_neither_session_nor_sink() {
        let td = tempdir().unwrap();
        fs::write(td.path().join("a.md"), "---\nstatus: draft\n---\n").unwrap();
        let session = query_cmd_test_session(td.path());
        let saved = Queries::default();

        let out_path = td.path().join("out.txt");
        let mut sink = OutputSink::open_file(&out_path).unwrap();
        run_query_cmd(
            QueryCmd::Run("nope".to_string()),
            &saved,
            &session,
            &mut sink,
        );
        drop(sink);

        assert_eq!(
            fs::read_to_string(&out_path).unwrap(),
            "",
            "an unknown saved-query name must not write any result"
        );
    }

    /// MUST-FIX: `.query list`'s output (via `run_query_cmd` → `query_list_lines`)
    /// names every saved query, one `name\tsql` line each, sorted by name.
    #[test]
    fn query_list_lines_names_every_saved_query() {
        let mut saved = Queries::default();
        queries::set(&mut saved, "zeta", "SELECT 1").unwrap();
        queries::set(&mut saved, "alpha", "SELECT 2").unwrap();
        assert_eq!(
            query_list_lines(&saved),
            "alpha\tSELECT 2\nzeta\tSELECT 1\n"
        );
    }

    #[test]
    fn query_list_lines_is_empty_when_nothing_is_saved() {
        assert_eq!(query_list_lines(&Queries::default()), "");
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
    fn output_command_parses_a_path() {
        assert_eq!(
            parse_dot(".output results.txt"),
            DotCommand::Output(Some("results.txt".to_string()))
        );
    }

    /// A bare `.output` and an explicit `.output stdout` (any casing) both
    /// mean "reset to stdout" — `None` — rather than redirecting to a file
    /// literally named `stdout`.
    #[test]
    fn output_command_with_no_arg_or_stdout_resets() {
        assert_eq!(parse_dot(".output"), DotCommand::Output(None));
        assert_eq!(parse_dot(".output stdout"), DotCommand::Output(None));
        assert_eq!(parse_dot(".output STDOUT"), DotCommand::Output(None));
    }

    /// A `|cmd` target is captured verbatim, spaces and all — a
    /// `words.next()`-style split would truncate at the first argument and
    /// silently drop `-s,`/the rest of the pipeline.
    #[test]
    fn output_command_captures_a_piped_command_verbatim() {
        assert_eq!(
            parse_dot(".output |column -t -s,"),
            DotCommand::Output(Some("|column -t -s,".to_string()))
        );
        assert_eq!(
            parse_dot(".output |grep x | wc -l"),
            DotCommand::Output(Some("|grep x | wc -l".to_string()))
        );
    }

    #[test]
    fn apply_output_redirects_to_a_file_then_resets_to_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.txt");
        let mut sink = OutputSink::Stdout;

        apply_output(&mut sink, Some(path.to_str().unwrap().to_string()));
        assert!(matches!(sink, OutputSink::File(_)));
        sink.write_block("hello").unwrap();

        apply_output(&mut sink, None);
        assert!(matches!(sink, OutputSink::Stdout));

        assert_eq!(fs::read_to_string(&path).unwrap(), "hello\n");
    }

    /// A path that can't be opened for writing (its parent directory doesn't
    /// exist) leaves `sink` exactly as it was, per the ambiguity resolved in
    /// the task brief: the session keeps running on stdout rather than
    /// silently losing subsequent results.
    #[test]
    fn apply_output_leaves_the_sink_untouched_on_an_unwritable_path() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("no-such-dir").join("out.txt");
        let mut sink = OutputSink::Stdout;

        apply_output(&mut sink, Some(bad.to_str().unwrap().to_string()));
        assert!(matches!(sink, OutputSink::Stdout));
    }

    /// `.output |cmd` pipes blocks through a real shell command, and
    /// resetting back to stdout finishes (closes stdin, reaps) the old pipe
    /// first — so the file `cat` was redirected to is complete by the time
    /// this test reads it back.
    #[test]
    fn apply_output_pipes_through_a_shell_command_then_finishes_on_reset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("piped.txt");
        let mut sink = OutputSink::Stdout;

        apply_output(&mut sink, Some(format!("|cat > {}", path.display())));
        assert!(matches!(sink, OutputSink::Command(_)));
        sink.write_block("alpha").unwrap();
        sink.write_block("beta").unwrap();

        apply_output(&mut sink, None);
        assert!(matches!(sink, OutputSink::Stdout));

        assert_eq!(fs::read_to_string(&path).unwrap(), "alpha\nbeta\n");
    }

    /// A failed switch must not disturb a *live* pipe: the new target is
    /// opened before the old sink is touched, so a bad path after
    /// `.output |cmd` leaves the original child's stdin open and still
    /// writable — proving the fix for the "dead pipe after a failed switch"
    /// finding (W45).
    #[test]
    fn apply_output_keeps_the_old_pipe_alive_when_the_new_target_fails_to_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("piped.txt");
        let mut sink = OutputSink::Stdout;

        apply_output(&mut sink, Some(format!("|cat > {}", path.display())));
        assert!(matches!(sink, OutputSink::Command(_)));

        apply_output(&mut sink, Some("/no/such/dir/x".to_string()));
        assert!(matches!(sink, OutputSink::Command(_)));

        // The original pipe is still alive and usable, not a dead sink whose
        // stdin was already closed by a `finish()` on the failed switch.
        sink.write_block("still alive").unwrap();
        sink.finish().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "still alive\n");
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
        assert_eq!(row_count_line(0, None), "-- 0 rows");
        assert_eq!(row_count_line(1, None), "-- 1 row");
        assert_eq!(row_count_line(2, None), "-- 2 rows");
    }

    /// `row_count_line(_, None)` must produce exactly today's output — every
    /// non-timed caller passes `None` — and `Some(d)` appends the elapsed
    /// seconds to 3 decimal places.
    #[test]
    fn row_count_line_appends_elapsed_when_timed() {
        assert_eq!(row_count_line(3, None), "-- 3 rows");
        assert_eq!(row_count_line(1, None), "-- 1 row");
        assert_eq!(
            row_count_line(3, Some(Duration::from_millis(12))),
            "-- 3 rows (0.012s)"
        );
    }

    #[test]
    fn parses_timer_on_off() {
        assert_eq!(parse_dot(".timer on"), DotCommand::Timer(Some(true)));
        assert_eq!(parse_dot(".timer off"), DotCommand::Timer(Some(false)));
        assert_eq!(parse_dot(".timer"), DotCommand::Timer(None));
    }

    /// An unrecognized `.timer` argument is `Unknown` carrying the whole
    /// line — mirrors `header_bad_arg_is_unknown_command`: there's no
    /// `on`/`off` equivalent worth naming individually, so the existing
    /// unknown-command error path handles it directly.
    #[test]
    fn timer_bad_arg_is_unknown_command() {
        assert_eq!(
            parse_dot(".timer maybe"),
            DotCommand::Unknown(".timer maybe".to_string())
        );
    }

    /// `dispatch_dot` wires `.timer on` through to `Session::set_timer`,
    /// reusing the same `Session` builder `dispatch_header_off_sets_session`
    /// uses — the timer setting defaults to `off`, so this also proves the
    /// dispatch actually flips it rather than the default coincidentally
    /// matching.
    #[test]
    fn dispatch_timer_on_sets_session() {
        let td = tempdir().unwrap();
        fs::write(td.path().join("a.md"), "---\nstatus: draft\n---\n").unwrap();
        let mut session = query_cmd_test_session(td.path());
        assert!(!session.timer(), "timer must default to off");
        let mut sink = OutputSink::Stdout;

        assert!(!dispatch_dot(
            DotCommand::Timer(Some(true)),
            &mut session,
            &mut sink,
            None
        ));
        assert!(session.timer());
    }

    /// The startup banner must name the record count and hint at both
    /// `.help` and `.schema` — pinned here since it's stdout-only/REPL-only
    /// and can't be reached by an integration test without a TTY.
    #[test]
    fn banner_contains_record_count_and_hints() {
        let text = banner(42);
        assert!(text.contains("42"), "{text:?}");
        assert!(text.contains(".help"), "{text:?}");
        assert!(text.contains(".schema"), "{text:?}");
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
        let c = complete_candidates(".sc", 3, &schema, DOT_COMMAND_NAMES, &[]);
        assert!(c.iter().any(|x| x == ".schema"));
        // config-key completion after '.set '
        let c = complete_candidates(".set for", 8, &schema, DOT_COMMAND_NAMES, &[]);
        assert!(c.iter().any(|x| x == "format"));
        // column completion for a bare word in SQL position
        let c = complete_candidates("SELECT sta", 10, &schema, DOT_COMMAND_NAMES, &[]);
        assert!(c.iter().any(|x| x == "status"));
    }

    /// SQL-position completion must draw only from `schema` plus
    /// [`FILE_COLUMNS`], never a SQL-keyword list — proven with a prefix
    /// (`"sel"`) that actually contains `select` as a candidate, unlike the
    /// `"sta"` prefix in `completion_candidates_by_position` above (which
    /// can't match `select` regardless of the implementation and so proves
    /// nothing). The schema here deliberately excludes `select`.
    #[test]
    fn sql_position_completion_never_offers_a_sql_keyword() {
        let schema = vec!["status".to_string(), "prd".to_string()];
        let c = complete_candidates("SELECT sel", 10, &schema, DOT_COMMAND_NAMES, &[]);
        assert!(
            !c.iter().any(|x| x.eq_ignore_ascii_case("select")),
            "completion must never draw from a SQL-keyword list: {c:?}"
        );
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

    /// Every name [`DOT_COMMAND_NAMES`] lists must also appear in `.help`'s
    /// output, so a command that tab-completes can never go undocumented —
    /// the sibling drift guard to `dot_command_names_all_parse` above.
    #[test]
    fn print_help_documents_every_dot_command_name() {
        let text = help_text();
        for name in DOT_COMMAND_NAMES {
            assert!(
                text.contains(name),
                "{name} is in DOT_COMMAND_NAMES but missing from .help's output:\n{text}"
            );
        }
    }

    /// `parse_dot` accepts `.SCHEMA`/`.Schema` case-insensitively, so
    /// completion of the dot-command word must too — `.SC<tab>` offers
    /// `.schema` exactly like `.sc<tab>` does.
    #[test]
    fn dot_command_completion_is_case_insensitive() {
        let schema = vec!["status".to_string()];
        let c = complete_candidates(".SC", 3, &schema, DOT_COMMAND_NAMES, &[]);
        assert!(c.iter().any(|x| x == ".schema"), "{c:?}");
    }

    /// `.schema`'s "File pseudo-columns:" section (`print_schema`) does
    /// nothing but iterate `FILE_COLUMNS` verbatim, so pinning the const
    /// itself is a faithful proxy for that printed output — and, since
    /// `print_describe`'s `Some(name) if FILE_COLUMNS.contains(&name) => ...`
    /// guard reads the same const, this also pins that `.describe
    /// file.mtime`/`.describe file.size` route to the pseudo-column path
    /// rather than falling through to "unknown field" (Task 4 follow-up).
    #[test]
    fn file_columns_include_mtime_and_size() {
        assert_eq!(
            FILE_COLUMNS,
            [
                "file.name",
                "file.path",
                "file.folder",
                "file.ext",
                "file.mtime",
                "file.size",
            ]
        );
    }

    /// `.describe file.size` must not falsely claim `Str` (Task 4
    /// follow-up): `file.size` is the one `Int`-typed pseudo-column, and
    /// every other `file.*` column — including the new `file.mtime`, an
    /// ISO-8601 UTC string — stays `Str`.
    #[test]
    fn describe_file_column_reports_accurate_types() {
        assert!(describe_file_column_line("file.size").contains("type Int"));
        assert!(describe_file_column_line("file.mtime").contains("type Str"));
        assert!(describe_file_column_line("file.name").contains("type Str"));
    }

    #[test]
    fn completion_offers_file_columns_and_unset_keys_too() {
        let schema = vec!["status".to_string()];
        // file.* pseudo-columns complete alongside schema fields.
        let c = complete_candidates("SELECT file.n", 13, &schema, DOT_COMMAND_NAMES, &[]);
        assert!(c.iter().any(|x| x == "file.name"));
        // .unset gets the same key completion as .set.
        let c = complete_candidates(".unset hi", 9, &schema, DOT_COMMAND_NAMES, &[]);
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
        let c = complete_candidates(line, line.len(), &schema, DOT_COMMAND_NAMES, &[]);
        assert!(!c.iter().any(|x| x == "format"));
    }

    /// A `.`-looking word that isn't the *first* word of the line (e.g. a
    /// stray value starting with a dot) must not be treated as a
    /// dot-command: only `start == 0` triggers that branch.
    #[test]
    fn completion_dot_prefix_only_matters_as_the_first_word() {
        let schema = vec!["status".to_string()];
        let line = "SELECT .sc";
        let c = complete_candidates(line, line.len(), &schema, DOT_COMMAND_NAMES, &[]);
        assert!(
            !c.iter().any(|x| x == ".schema"),
            "a `.`-looking word that isn't the first word must not complete as a \
             dot-command: {c:?}"
        );
    }

    /// `.query run <name>` completes saved-query names — the deferred
    /// completion the SQL-word branch's old doc comment pointed at, now
    /// realized as its own dedicated context (not folded into the SQL-word
    /// branch, since a saved-query name and a schema field are different
    /// vocabularies).
    #[test]
    fn query_run_completes_saved_query_names() {
        let schema = vec!["status".to_string()];
        let query_names = vec!["stale-plans".to_string(), "synced".to_string()];
        let c = complete_candidates(
            ".query run st",
            13,
            &schema,
            DOT_COMMAND_NAMES,
            &query_names,
        );
        assert!(c.iter().any(|x| x == "stale-plans"), "{c:?}");
        assert!(!c.iter().any(|x| x == "synced"), "{c:?}");
        // Case-insensitive on the "run" action word, matching parse_dot.
        let c = complete_candidates(
            ".query RUN st",
            13,
            &schema,
            DOT_COMMAND_NAMES,
            &query_names,
        );
        assert!(c.iter().any(|x| x == "stale-plans"), "{c:?}");
    }

    /// `.query list` (not `run`) and a bare `.query ` must NOT trigger
    /// saved-query-name completion — only the `<name>` slot of `.query run`
    /// does.
    #[test]
    fn query_list_does_not_complete_saved_query_names() {
        let schema = vec!["status".to_string()];
        let query_names = vec!["stale-plans".to_string()];
        let c = complete_candidates(".query list", 11, &schema, DOT_COMMAND_NAMES, &query_names);
        assert!(!c.iter().any(|x| x == "stale-plans"), "{c:?}");
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
            query_names: Vec::new(),
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
