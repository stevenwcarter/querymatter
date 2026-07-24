//! A [`Session`] bundles a record store with an output [`Format`] and turns
//! SQL text into rendered result strings — the shared core behind one-shot,
//! batch, and interactive modes.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::cache;
use crate::config::{self, Config, ConfigKey};
use crate::query::{self, ResultTable};
use crate::render::{self, Format, Output, TableStyle};
use crate::settings::{Resolved, Settings, Source};
use crate::store::{LoadReport, RecordStore};

/// Owns the queryable store plus the current output format, and runs queries
/// against them.
pub struct Session {
    store: Box<dyn RecordStore>,
    /// Every setting as resolved for this session; `.style`/`.format` mutate
    /// the rendering ones in place.
    settings: Settings,
    /// The same resolution with the config layer removed, so `.unset` can
    /// revert a setting to whatever would apply without the config file.
    fallback: Settings,
    /// The `.querymatter` vault this session's store is backed by, when it
    /// is cache-backed. `None` for a live (no-cache) session, in which case
    /// [`refresh`](Self::refresh) falls back to an in-memory-only reload.
    vault: Option<PathBuf>,
}

impl Session {
    /// Builds a session over `store` with `settings`, keeping `fallback` —
    /// the config-free resolution — for `.unset`.
    pub fn new(
        store: Box<dyn RecordStore>,
        settings: Settings,
        fallback: Settings,
        vault: Option<PathBuf>,
    ) -> Self {
        Session {
            store,
            settings,
            fallback,
            vault,
        }
    }

    /// The format rendered results are produced in.
    pub fn format(&self) -> Format {
        self.settings.format.value
    }

    /// The border style used when rendering [`Format::Table`].
    pub fn style(&self) -> TableStyle {
        self.settings.table_style.value
    }

    /// Every setting, for `.settings`.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Persists `key = value` to the config file and applies it to this
    /// session when it affects rendering.
    ///
    /// Returns the config file's path, for the caller's confirmation message,
    /// and whether the change takes effect immediately — scan settings do
    /// not, because the store is already loaded.
    pub fn persist_set(&mut self, key: ConfigKey, value: &str) -> anyhow::Result<(PathBuf, bool)> {
        let mut config = config::load()?;
        config::set(&mut config, key, value)?;
        let path = config::save(&config)?;
        let immediate = self.apply(key, &config);
        Ok((path, immediate))
    }

    /// Removes `key` from the config file, reverting this session's value to
    /// whatever applies without it.
    pub fn persist_unset(&mut self, key: ConfigKey) -> anyhow::Result<(PathBuf, bool)> {
        let mut config = config::load()?;
        config::unset(&mut config, key);
        let path = config::save(&config)?;
        let immediate = match key {
            ConfigKey::Format => {
                self.settings.format = self.fallback.format.clone();
                true
            }
            ConfigKey::TableStyle => {
                self.settings.table_style = self.fallback.table_style.clone();
                true
            }
            _ => false,
        };
        Ok((path, immediate))
    }

    /// Applies a just-persisted rendering setting to this session. Returns
    /// whether anything changed now; scan settings take effect next run.
    fn apply(&mut self, key: ConfigKey, config: &Config) -> bool {
        match key {
            ConfigKey::Format => {
                if let Some(format) = config.format {
                    self.settings.format = Resolved {
                        value: format,
                        source: Source::Config,
                    };
                }
                true
            }
            ConfigKey::TableStyle => {
                if let Some(style) = config.table_style {
                    self.settings.table_style = Resolved {
                        value: style,
                        source: Source::Config,
                    };
                }
                true
            }
            _ => false,
        }
    }

    /// Parses and executes `sql`, returning the projected result table.
    ///
    /// Parse and execution errors are surfaced as `anyhow` errors carrying
    /// the offending SQL as context.
    pub fn run(&self, sql: &str) -> anyhow::Result<ResultTable> {
        let query = query::parse(sql).with_context(|| format!("failed to parse query: {sql}"))?;
        query::execute(&query, self.store.records())
            .with_context(|| format!("failed to execute query: {sql}"))
    }

    /// Runs `statement` and renders the result: in the session's current
    /// format for a `;`/`\g` terminator, or one record per block for `\G`.
    ///
    /// The returned string carries no trailing newline (see [`render`]); the
    /// caller adds exactly one when printing.
    pub fn render_statement(&self, statement: &Statement) -> anyhow::Result<String> {
        let table = self.run(&statement.sql)?;
        let output = statement.terminator.output(self.format());
        Ok(render::render(&table, output, self.style()))
    }

    /// Switches the output format for the rest of this session only.
    pub fn set_format(&mut self, format: Format) {
        self.settings.format = Resolved {
            value: format,
            source: Source::Session,
        };
    }

    /// Switches the table border style for the rest of this session only.
    pub fn set_style(&mut self, style: TableStyle) {
        self.settings.table_style = Resolved {
            value: style,
            source: Source::Session,
        };
    }

    /// Rescans every tracked root, returning the combined load report.
    pub fn reload(&mut self) -> LoadReport {
        self.store.reload_all()
    }

    /// Forces a fresh scan of `subtree` (or the whole vault, when `None`),
    /// updating the in-memory view and — when this session is vault-backed —
    /// persisting the result to the `.querymatter` cache. A live (no-vault)
    /// session has nothing to persist to, so it falls back to an in-memory
    /// [`reload_all`](RecordStore::reload_all); `subtree` is ignored in that
    /// case, matching `.reload`'s existing whole-store semantics.
    ///
    /// A vault-backed `subtree` is resolved through
    /// [`cache::resolve_refresh_target`] — the SAME canonicalize-and-validate
    /// the CLI's `--refresh` uses — so a relative REPL path (`.refresh plans`)
    /// re-parses that subtree instead of silently no-op-ing against the stale
    /// cache (design spec §10). An unresolvable or outside-vault path is not a
    /// silent no-op either: the refresh is skipped and the returned report
    /// carries a warning naming the problem, which the REPL prints to stderr.
    pub fn refresh(&mut self, subtree: Option<&Path>) -> LoadReport {
        let Some(vault) = &self.vault else {
            return self.store.reload_all();
        };
        let resolved = match subtree {
            Some(path) => match cache::resolve_refresh_target(path, vault) {
                Ok(target) => Some(target),
                Err(err) => {
                    return LoadReport {
                        warnings: vec![format!("{err:#}")],
                        ..LoadReport::default()
                    };
                }
            },
            None => None,
        };
        self.store.refresh(vault, resolved.as_deref())
    }

    /// The current schema: the sorted union of frontmatter field names.
    pub fn schema(&self) -> Vec<String> {
        self.store.schema()
    }
}

/// How a statement was terminated, which selects how its result renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    /// `;` or `\g` — render in the session's current format.
    Semicolon,
    /// `\G` — render one record per block (see [`Output::Vertical`]).
    VerticalG,
}

impl Terminator {
    /// The rendering this terminator selects, given the session's standing
    /// `format`. `\G` overrides the format entirely: it means "show me this
    /// record-wise" whatever `.format` is currently set to.
    fn output(self, format: Format) -> Output {
        match self {
            Terminator::Semicolon => Output::Format(format),
            Terminator::VerticalG => Output::Vertical,
        }
    }
}

/// One statement plus the terminator that ended it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// The statement text, with the terminator stripped and trimmed.
    pub sql: String,
    /// The terminator that ended it.
    pub terminator: Terminator,
}

/// Splits `input` into individual statements on top-level `;`, `\g`, and
/// `\G`, trimming each and dropping the empties.
///
/// "Top-level" means terminators inside single- or double-quoted string
/// literals do not split — so `WHERE title = 'a;b'` stays one statement.
/// `\g` terminates exactly like `;` while `\G` additionally selects vertical
/// rendering; both are case-sensitive, matching `mysql`. Any other backslash
/// sequence is ordinary statement text.
pub fn split_statements(input: &str) -> Vec<Statement> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();

    while let Some(ch) = chars.next() {
        match quote {
            Some(q) => {
                current.push(ch);
                if ch == q {
                    quote = None;
                }
            }
            None => match ch {
                '\'' | '"' => {
                    quote = Some(ch);
                    current.push(ch);
                }
                ';' => {
                    push_statement(&mut statements, &current, Terminator::Semicolon);
                    current.clear();
                }
                '\\' => match chars.peek() {
                    Some('g') => {
                        chars.next();
                        push_statement(&mut statements, &current, Terminator::Semicolon);
                        current.clear();
                    }
                    Some('G') => {
                        chars.next();
                        push_statement(&mut statements, &current, Terminator::VerticalG);
                        current.clear();
                    }
                    _ => current.push(ch),
                },
                _ => current.push(ch),
            },
        }
    }
    // Trailing text with no terminator runs like a `;`-terminated statement.
    push_statement(&mut statements, &current, Terminator::Semicolon);
    statements
}

/// Trims `stmt` and pushes it onto `out` with `terminator` when not empty.
fn push_statement(out: &mut Vec<Statement>, stmt: &str, terminator: Terminator) {
    let trimmed = stmt.trim();
    if !trimmed.is_empty() {
        out.push(Statement {
            sql: trimmed.to_string(),
            terminator,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::{self, Freshness};
    use crate::discover::WalkOpts;
    use crate::model::Value;
    use crate::store::InMemoryStore;
    use std::fs;
    use std::fs::File;
    use tempfile::TempDir;

    fn semi(sql: &str) -> Statement {
        Statement {
            sql: sql.to_string(),
            terminator: Terminator::Semicolon,
        }
    }

    #[test]
    fn split_statements_basic() {
        assert_eq!(
            split_statements(" SELECT 1 ; SELECT 2 ;"),
            vec![semi("SELECT 1"), semi("SELECT 2")]
        );
        assert_eq!(split_statements("SELECT 1"), vec![semi("SELECT 1")]);
        assert_eq!(split_statements("  ;; "), Vec::<Statement>::new());
    }

    #[test]
    fn split_statements_recognizes_vertical_g() {
        assert_eq!(
            split_statements("SELECT 1\\G"),
            vec![Statement {
                sql: "SELECT 1".to_string(),
                terminator: Terminator::VerticalG,
            }]
        );
    }

    /// `\g` terminates exactly like `;`, and the pair is case-sensitive —
    /// both match `mysql`.
    #[test]
    fn split_statements_lowercase_g_is_a_plain_terminator() {
        assert_eq!(split_statements("SELECT 1\\g"), vec![semi("SELECT 1")]);
    }

    #[test]
    fn split_statements_mixes_terminators() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2\\G SELECT 3\\g"),
            vec![
                semi("SELECT 1"),
                Statement {
                    sql: "SELECT 2".to_string(),
                    terminator: Terminator::VerticalG,
                },
                semi("SELECT 3"),
            ]
        );
    }

    /// Terminators inside a string literal are ordinary text, exactly as `;`
    /// already was.
    #[test]
    fn split_statements_ignores_terminators_in_quotes() {
        assert_eq!(
            split_statements("SELECT status WHERE title = 'a\\Gb;c'"),
            vec![semi("SELECT status WHERE title = 'a\\Gb;c'")]
        );
    }

    /// A backslash inside a quoted literal is handled by the quote-mode match
    /// arm, which never inspects backslashes at all — it is just more quoted
    /// text. This does NOT exercise the top-level `'\\' => match chars.peek()`
    /// fallback; see the `split_statements_backslash_*` tests below for that.
    #[test]
    fn split_statements_ignores_backslashes_inside_quotes() {
        assert_eq!(
            split_statements("SELECT status WHERE p = 'a\\b'"),
            vec![semi("SELECT status WHERE p = 'a\\b'")]
        );
    }

    /// At the top level (outside any quote), a backslash not followed by `g`
    /// or `G` is retained verbatim and the following character is preserved
    /// untouched — the peek must not consume it.
    #[test]
    fn split_statements_backslash_before_letter_preserves_next_char() {
        assert_eq!(
            split_statements("a\\zb;"),
            vec![semi("a\\zb")],
            "a top-level backslash before an ordinary letter must keep both \
             the backslash and the letter"
        );
    }

    /// A trailing backslash at end of input (nothing left to peek) must not
    /// panic, and the backslash is retained in the final statement.
    #[test]
    fn split_statements_trailing_backslash_does_not_panic() {
        assert_eq!(split_statements("SELECT 1\\"), vec![semi("SELECT 1\\")]);
    }

    /// A backslash immediately before a quote character is retained as
    /// literal text, and the quote genuinely opens a string — this crate
    /// implements no backslash-escaping of quotes, so the quote character
    /// still flips the quote-mode state machine on its own.
    #[test]
    fn split_statements_backslash_before_quote_opens_a_string() {
        assert_eq!(
            split_statements("a\\'b';"),
            vec![semi("a\\'b'")],
            "the backslash stays literal and 'b' is still a real quoted \
             literal, not an escaped quote"
        );
    }

    /// A vault-backed session's `refresh` must both update the in-memory
    /// view AND persist the change to the on-disk `.querymatter` cache —
    /// unlike `.reload`, which never touches disk.
    #[test]
    fn refresh_with_vault_updates_view_and_persists() {
        let td = TempDir::new().unwrap();
        let a_path = td.path().join("a.md");
        fs::write(&a_path, "---\nstatus: draft\n---\n").unwrap();
        cache::build_vault(td.path(), &WalkOpts::default(), 300).unwrap();

        let (store, _report) =
            InMemoryStore::from_cache(td.path(), WalkOpts::default(), Freshness::PerFile);
        let mut session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            Some(td.path().to_path_buf()),
        );

        fs::write(&a_path, "---\nstatus: final\n---\n").unwrap();

        let report = session.refresh(None);
        assert_eq!(report.skipped, 0);

        let table = session.run("SELECT status").unwrap();
        assert_eq!(
            table.rows[0][0],
            Value::Str("final".into()),
            "the in-memory view must reflect the edit"
        );

        let (_body, loaded) = cache::load_cache(td.path()).unwrap();
        let persisted = loaded
            .iter()
            .flat_map(|dir| &dir.files)
            .find(|file| file.rel_path == "a.md")
            .and_then(|file| file.fields.get("status").cloned())
            .expect("a.md not found in persisted cache");
        assert_eq!(
            persisted,
            Value::Str("final".into()),
            "refresh must persist the edit to the on-disk cache"
        );
    }

    /// A live (no-vault) session's `refresh` falls back to an in-memory
    /// reload: it must still pick up the edit, but there is no cache to
    /// write.
    #[test]
    fn refresh_without_vault_reloads_in_memory_only() {
        let td = TempDir::new().unwrap();
        let a_path = td.path().join("a.md");
        fs::write(&a_path, "---\nstatus: draft\n---\n").unwrap();

        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        let mut session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            None,
        );

        fs::write(&a_path, "---\nstatus: final\n---\n").unwrap();

        let report = session.refresh(None);
        assert_eq!(report.skipped, 0);

        let table = session.run("SELECT status").unwrap();
        assert_eq!(table.rows[0][0], Value::Str("final".into()));
        assert!(
            cache::load_cache(td.path()).is_none(),
            "a vault-less session's refresh must never write a .querymatter cache"
        );
    }

    /// A vault-backed `Session::refresh(Some(subtree))` must canonicalize and
    /// validate the subtree the same way the CLI's `--refresh` does, then
    /// force a re-parse of it (spec §10) — not silently no-op against the
    /// stale cache the way a raw relative path fed straight to
    /// `store.refresh` would.
    #[test]
    fn refresh_subtree_with_vault_reparses_that_subtree() {
        let td = TempDir::new().unwrap();
        let vault = fs::canonicalize(td.path()).unwrap();
        let a = vault.join("plans/a.md");
        fs::create_dir_all(a.parent().unwrap()).unwrap();
        fs::write(&a, "---\nstatus: draft\n---\n").unwrap();
        let original_mtime = fs::metadata(&a).unwrap().modified().unwrap();
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let (store, _report) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile);
        let mut session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            Some(vault.clone()),
        );

        // Equal byte length ("draft" -> "fresh") plus a restored mtime: the
        // default per-file freshness check would REUSE the stale cached value,
        // so only a forced subtree re-parse can surface the edit.
        fs::write(&a, "---\nstatus: fresh\n---\n").unwrap();
        File::open(&a)
            .unwrap()
            .set_modified(original_mtime)
            .unwrap();

        let report = session.refresh(Some(&vault.join("plans")));
        assert_eq!(report.skipped, 0);
        assert!(
            report.warnings.is_empty(),
            "warnings: {:?}",
            report.warnings
        );

        let table = session.run("SELECT status").unwrap();
        assert_eq!(
            table.rows[0][0],
            Value::Str("fresh".into()),
            "a vault-backed subtree refresh must force a re-parse, not reuse the stale cache"
        );
    }

    /// An unresolvable (or outside-vault) `.refresh <path>` must not silently
    /// no-op or crash: the refresh is skipped and a warning naming the problem
    /// is returned for the REPL to print, leaving the store queryable.
    #[test]
    fn refresh_subtree_unresolvable_path_warns_and_does_not_crash() {
        let td = TempDir::new().unwrap();
        let vault = fs::canonicalize(td.path()).unwrap();
        fs::write(vault.join("a.md"), "---\nstatus: draft\n---\n").unwrap();
        cache::build_vault(&vault, &WalkOpts::default(), 300).unwrap();

        let (store, _report) =
            InMemoryStore::from_cache(&vault, WalkOpts::default(), Freshness::PerFile);
        let mut session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            Some(vault.clone()),
        );

        let report = session.refresh(Some(Path::new("definitely-not-here")));
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("definitely-not-here")),
            "an unresolvable .refresh path must surface a warning, got: {:?}",
            report.warnings
        );

        // The store is untouched and still queryable.
        let table = session.run("SELECT status").unwrap();
        assert_eq!(table.rows[0][0], Value::Str("draft".into()));
    }
}
