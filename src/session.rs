//! A [`Session`] bundles a record store with an output [`Format`] and turns
//! SQL text into rendered result strings — the shared core behind one-shot,
//! batch, and interactive modes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
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

    /// Persists `key = value` to the user's config file and applies it to
    /// this session when it affects rendering.
    ///
    /// Returns the config file's path, for the caller's confirmation message,
    /// and whether the change takes effect immediately — scan settings do
    /// not, because the store is already loaded. Thin wrapper around
    /// [`persist_set_at`](Self::persist_set_at) that resolves the real path;
    /// tests exercise the path-taking version directly against a temp file.
    pub fn persist_set(&mut self, key: ConfigKey, value: &str) -> anyhow::Result<(PathBuf, bool)> {
        let path =
            config::config_path().context("cannot determine a config directory for this user")?;
        let immediate = self.persist_set_at(&path, key, value)?;
        Ok((path, immediate))
    }

    /// Persists `key = value` to the config file at `path`, applying it to
    /// this session when it affects rendering.
    ///
    /// Re-reads `path` on every call rather than reusing an in-memory
    /// snapshot, so a prior `persist_set_at`/`persist_unset_at` call to a
    /// *different* key in the same session is preserved rather than
    /// clobbered. Returns whether the change takes effect immediately.
    pub fn persist_set_at(
        &mut self,
        path: &Path,
        key: ConfigKey,
        value: &str,
    ) -> anyhow::Result<bool> {
        let mut config = config::load_from(path)?;
        config::set(&mut config, key, value)?;
        config::save_to(path, &config)?;
        Ok(self.apply(key, &config))
    }

    /// Removes `key` from the user's config file, if present, reverting this
    /// session's value to whatever applies without it.
    ///
    /// Returns the config file's path, whether the key had actually been
    /// present (so the caller can report "removed" only when something
    /// really changed, rather than always), and whether the change takes
    /// effect immediately. Thin wrapper around
    /// [`persist_unset_at`](Self::persist_unset_at) that resolves the real
    /// path; tests exercise the path-taking version directly against a temp
    /// file.
    pub fn persist_unset(&mut self, key: ConfigKey) -> anyhow::Result<(PathBuf, bool, bool)> {
        let path =
            config::config_path().context("cannot determine a config directory for this user")?;
        let (removed, immediate) = self.persist_unset_at(&path, key)?;
        Ok((path, removed, immediate))
    }

    /// Removes `key` from the config file at `path`, if present, reverting
    /// this session's value to whatever applies without it.
    ///
    /// A key that is already absent is a no-op: `path` is read to check, but
    /// never written back, so a `.unset` of a never-set key never
    /// materializes a config file (or its parent directory) for what is
    /// semantically a no-op. Returns whether the key had actually been
    /// present, and — only when it was — whether reverting it takes effect
    /// immediately.
    pub fn persist_unset_at(
        &mut self,
        path: &Path,
        key: ConfigKey,
    ) -> anyhow::Result<(bool, bool)> {
        let mut config = config::load_from(path)?;
        if config::get(&config, key).is_none() {
            return Ok((false, false));
        }
        config::unset(&mut config, key);
        config::save_to(path, &config)?;
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
        Ok((true, immediate))
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
                    true
                } else {
                    false
                }
            }
            ConfigKey::TableStyle => {
                if let Some(style) = config.table_style {
                    self.settings.table_style = Resolved {
                        value: style,
                        source: Source::Config,
                    };
                    true
                } else {
                    false
                }
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
        query::execute(&query, self.store.records(), self.settings.lenient.value)
            .with_context(|| format!("failed to execute query: {sql}"))
    }

    /// Runs `statement` once and returns both its rendered string — in the
    /// session's current format for a `;`/`\g` terminator, or one record per
    /// block for `\G`, with no trailing newline (see [`render`]); the caller
    /// adds exactly one when printing — and the row count from that same
    /// [`ResultTable`]. The REPL prints the count as a `-- N rows` line after
    /// the result, and one-shot/batch callers sum it for `--exit-code`,
    /// without either needing a second query run.
    pub fn render_statement_counted(
        &self,
        statement: &Statement,
    ) -> anyhow::Result<(String, usize)> {
        let table = self.run(&statement.sql)?;
        let output = statement.terminator.output(self.format());
        let rendered = render::render(&table, output, self.style());
        Ok((rendered, table.rows.len()))
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

    /// Computes per-field type and coverage statistics for `.describe`:
    /// which `Value` variant(s) each frontmatter field has taken on, its
    /// non-null coverage, and — for fields with at most [`VALUE_CAP`]
    /// distinct non-null values — a most-frequent-first tally of them.
    ///
    /// Walks `self.store.records()` exactly once. A field's variant set and
    /// non-null count only reflect records where the field is actually
    /// present in that record's frontmatter: a record that omits the field
    /// entirely leaves it untouched, distinct from a record that sets it
    /// explicitly to `null` (a present `Value::Null`, which counts toward
    /// the variant set as `"Null"` without counting as non-null).
    pub fn describe(&self) -> DescribeReport {
        let mut raw: BTreeMap<String, RawFieldStat> = BTreeMap::new();
        let mut total = 0usize;
        for record in self.store.records() {
            total += 1;
            for name in record.field_names() {
                let value = record.field(name);
                let stat = raw.entry(name.to_string()).or_default();
                stat.variants.insert(value.variant_name());
                if !value.is_null() {
                    stat.non_null += 1;
                    *stat.counts.entry(value.display()).or_insert(0) += 1;
                }
            }
        }
        raw.into_iter()
            .map(|(name, stat)| (name, stat.finish(total)))
            .collect()
    }
}

/// The number of distinct non-null values a field's [`FieldStat::values`]
/// keeps before `.describe` falls back to reporting a bare distinct count.
const VALUE_CAP: usize = 12;

/// Per-field `.describe` statistics, keyed by frontmatter field name in
/// sorted order (matching [`Session::schema`]).
pub type DescribeReport = BTreeMap<String, FieldStat>;

/// One field's `.describe` statistics, computed by [`Session::describe`].
#[derive(Debug, Clone, PartialEq)]
pub struct FieldStat {
    /// The distinct `Value` variant names seen for this field, e.g.
    /// `{"Str"}`, or `{"Int", "Str"}` for a mixed-type field.
    pub variants: BTreeSet<&'static str>,
    /// Records where this field is present with a non-null value.
    pub non_null: usize,
    /// The total record count (the same for every field).
    pub total: usize,
    /// The field's distinct non-null values, most-frequent-first, when
    /// there are at most [`VALUE_CAP`] of them; `None` when `distinct`
    /// exceeds the cap, in which case the formatter shows the bare count
    /// instead of the list.
    pub values: Option<Vec<(String, usize)>>,
    /// The number of distinct non-null values seen — accurate even when
    /// `values` is `None`.
    pub distinct: usize,
}

/// Accumulator for one field's `.describe` stats while walking records;
/// [`RawFieldStat::finish`] turns it into the capped, public [`FieldStat`].
#[derive(Default)]
struct RawFieldStat {
    variants: BTreeSet<&'static str>,
    non_null: usize,
    counts: HashMap<String, usize>,
}

impl RawFieldStat {
    /// Sorts the accumulated counts most-frequent-first (ties broken
    /// lexicographically, for deterministic output), then caps the list at
    /// [`VALUE_CAP`] — recording the true distinct count either way.
    fn finish(self, total: usize) -> FieldStat {
        let distinct = self.counts.len();
        let mut values: Vec<(String, usize)> = self.counts.into_iter().collect();
        values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        FieldStat {
            variants: self.variants,
            non_null: self.non_null,
            total,
            values: (distinct <= VALUE_CAP).then_some(values),
            distinct,
        }
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

    /// `render_statement_counted` must run the query exactly once and return
    /// both the rendered string and the row count read off that same
    /// `ResultTable` — not re-run the query separately to count rows.
    #[test]
    fn render_statement_counted_returns_row_count() {
        let td = TempDir::new().unwrap();
        for (name, body) in [
            ("a.md", "---\nstatus: draft\n---\n"),
            ("b.md", "---\nstatus: synced\n---\n"),
            ("c.md", "---\nstatus: synced\n---\n"),
        ] {
            fs::write(td.path().join(name), body).unwrap();
        }
        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        let session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            None,
        );

        let (rendered, count) = session
            .render_statement_counted(&semi("SELECT status"))
            .unwrap();
        assert_eq!(count, 3);
        assert!(!rendered.is_empty());

        let (_, count) = session
            .render_statement_counted(&semi("SELECT status WHERE status = 'draft'"))
            .unwrap();
        assert_eq!(count, 1);

        let (_, count) = session
            .render_statement_counted(&semi("SELECT status WHERE status = 'missing'"))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// `.describe`'s core computation: per-field `Value` variant set,
    /// non-null coverage against the FULL record count (not just records
    /// where the field is present), and a most-frequent-first value tally.
    ///
    /// Record 4 omits `status` entirely — it must lower `non_null` without
    /// adding to `variants`. Contrast with
    /// `describe_all_null_field_reports_null_variant` below, where the field
    /// is present but explicitly `null` in every record.
    #[test]
    fn describe_tallies_variants_coverage_and_values() {
        let td = TempDir::new().unwrap();
        for (name, body) in [
            ("a.md", "---\nstatus: draft\nn: 1\n---\n"),
            ("b.md", "---\nstatus: draft\nn: two\n---\n"),
            ("c.md", "---\nstatus: done\n---\n"),
            ("d.md", "---\ntitle: untagged\n---\n"),
        ] {
            fs::write(td.path().join(name), body).unwrap();
        }
        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        let session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            None,
        );

        let report = session.describe();

        let status = &report["status"];
        assert_eq!(status.variants, BTreeSet::from(["Str"]));
        assert_eq!(status.non_null, 3);
        assert_eq!(status.total, 4);
        assert_eq!(
            status.values,
            Some(vec![("draft".to_string(), 2), ("done".to_string(), 1)]),
            "most-frequent-first, ties broken lexicographically"
        );
        assert_eq!(status.distinct, 2);

        let n = &report["n"];
        assert_eq!(n.variants, BTreeSet::from(["Int", "Str"]));
        assert_eq!(n.non_null, 2);
        assert_eq!(n.total, 4);
    }

    /// A field explicitly `null` in every record (present, not merely
    /// absent) reports variant set `{"Null"}` and `0` non-null — the
    /// "present but always null" case, distinct from a field a record's
    /// frontmatter omits entirely.
    #[test]
    fn describe_all_null_field_reports_null_variant() {
        let td = TempDir::new().unwrap();
        for (name, body) in [
            ("a.md", "---\nstatus: null\n---\n"),
            ("b.md", "---\nstatus: null\n---\n"),
        ] {
            fs::write(td.path().join(name), body).unwrap();
        }
        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        let session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            None,
        );

        let report = session.describe();
        let status = &report["status"];
        assert_eq!(status.variants, BTreeSet::from(["Null"]));
        assert_eq!(status.non_null, 0);
        assert_eq!(status.total, 2);
        assert_eq!(status.values, Some(Vec::new()));
        assert_eq!(status.distinct, 0);
    }

    /// A field with more than `VALUE_CAP` distinct non-null values reports
    /// `values: None` while still tracking the true `distinct` count — the
    /// formatter falls back to "N distinct values" for these instead of
    /// listing them all.
    #[test]
    fn describe_caps_value_list_but_keeps_true_distinct_count() {
        let td = TempDir::new().unwrap();
        for i in 0..(VALUE_CAP + 1) {
            fs::write(
                td.path().join(format!("f{i}.md")),
                format!("---\ntag: v{i}\n---\n"),
            )
            .unwrap();
        }
        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        let session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            None,
        );

        let report = session.describe();
        let tag = &report["tag"];
        assert_eq!(tag.distinct, VALUE_CAP + 1);
        assert!(
            tag.values.is_none(),
            "a field over the cap must report no value list"
        );
    }

    /// A field with EXACTLY `VALUE_CAP` distinct non-null values sits right at
    /// the cap boundary, which `describe_caps_value_list_but_keeps_true_distinct_count`
    /// (at `VALUE_CAP + 1`) doesn't exercise: `values` must still be `Some`
    /// (shown), not fall to the over-cap `None` branch — pinning `<=`, not
    /// `<`, in `RawFieldStat::finish`.
    #[test]
    fn describe_at_exact_cap_still_shows_the_value_list() {
        let td = TempDir::new().unwrap();
        for i in 0..VALUE_CAP {
            fs::write(
                td.path().join(format!("f{i}.md")),
                format!("---\ntag: v{i}\n---\n"),
            )
            .unwrap();
        }
        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        let session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            None,
        );

        let report = session.describe();
        let tag = &report["tag"];
        assert_eq!(tag.distinct, VALUE_CAP);
        assert!(
            tag.values.is_some(),
            "a field exactly at the cap must still report its value list"
        );
        assert_eq!(tag.values.as_ref().unwrap().len(), VALUE_CAP);
    }

    /// Two values tied on count must be ordered lexicographically by value —
    /// pinning `RawFieldStat::finish`'s `.then_with(|| a.0.cmp(&b.0))`
    /// tie-break, not merely its primary sort by count.
    #[test]
    fn describe_breaks_count_ties_lexicographically() {
        let td = TempDir::new().unwrap();
        for (name, tag) in [("a.md", "zeta"), ("b.md", "alpha")] {
            fs::write(td.path().join(name), format!("---\ntag: {tag}\n---\n")).unwrap();
        }
        let (store, _report) =
            InMemoryStore::load(vec![td.path().to_path_buf()], WalkOpts::default());
        let session = Session::new(
            Box::new(store),
            Settings::default(),
            Settings::default(),
            None,
        );

        let report = session.describe();
        let tag = &report["tag"];
        assert_eq!(
            tag.values,
            Some(vec![("alpha".to_string(), 1), ("zeta".to_string(), 1)]),
            "equal-count values must be ordered lexicographically"
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

    /// A session over an empty in-memory store, for the
    /// `persist_set_at`/`persist_unset_at` tests below, which exercise the
    /// config-file plumbing rather than querying.
    fn empty_session(settings: Settings, fallback: Settings) -> Session {
        let (store, _report) = InMemoryStore::load(Vec::new(), WalkOpts::default());
        Session::new(Box::new(store), settings, fallback, None)
    }

    /// `apply`'s return value must be DERIVED from whether `config` actually
    /// carries a value for the rendering key, not hardcoded. Unreachable via
    /// `persist_set_at` today (it always calls `apply` right after
    /// `config::set`, so the key is always present) but not guaranteed by
    /// the type system — a `Some`-less `Config` must report `false`, never
    /// silently claim a change happened.
    #[test]
    fn apply_reports_false_when_the_config_lacks_the_rendering_key() {
        let mut session = empty_session(Settings::default(), Settings::default());
        assert!(!session.apply(ConfigKey::Format, &Config::default()));
        assert!(!session.apply(ConfigKey::TableStyle, &Config::default()));
    }

    /// Property 1 (re-read, not snapshot): two successive `persist_set_at`
    /// calls for different keys must both survive in the file — a call built
    /// on an in-memory snapshot taken before the first write would silently
    /// clobber it instead.
    #[test]
    fn persist_set_at_rereads_the_file_leaving_prior_keys_intact() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("config.toml");
        let mut session = empty_session(Settings::default(), Settings::default());

        session
            .persist_set_at(&path, ConfigKey::TableStyle, "unicode")
            .unwrap();
        session
            .persist_set_at(&path, ConfigKey::Format, "json")
            .unwrap();

        let config = config::load_from(&path).unwrap();
        assert_eq!(config.table_style, Some(TableStyle::Unicode));
        assert_eq!(config.format, Some(Format::Json));
    }

    /// Property 2 (revert to fallback, not default): a session whose
    /// `table_style` came from a flag — with `fallback` reflecting that same
    /// flag — must revert to the FLAG's value on `.unset`, not to the
    /// built-in `TableStyle::Ascii` default.
    #[test]
    fn persist_unset_at_reverts_to_fallback_not_default() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("config.toml");

        let flagged = Resolved {
            value: TableStyle::Compact,
            source: Source::Flag,
        };
        let settings = Settings {
            table_style: flagged.clone(),
            ..Settings::default()
        };
        let fallback = Settings {
            table_style: flagged,
            ..Settings::default()
        };

        let mut session = empty_session(settings, fallback);
        session
            .persist_set_at(&path, ConfigKey::TableStyle, "unicode")
            .unwrap();
        assert_eq!(session.style(), TableStyle::Unicode);

        let (removed, immediate) = session
            .persist_unset_at(&path, ConfigKey::TableStyle)
            .unwrap();
        assert!(removed);
        assert!(immediate);
        assert_eq!(
            session.style(),
            TableStyle::Compact,
            "must revert to the flag's value, not the Ascii default"
        );
    }

    /// Property 3 (immediate): `persist_set_at` on a rendering key
    /// (`format`, `table_style`) reports immediate and changes the
    /// corresponding session accessor right away.
    #[test]
    fn persist_set_at_rendering_keys_take_effect_immediately() {
        let td = TempDir::new().unwrap();
        let mut session = empty_session(Settings::default(), Settings::default());

        let immediate = session
            .persist_set_at(&td.path().join("format.toml"), ConfigKey::Format, "json")
            .unwrap();
        assert!(immediate, "format is a rendering key and must be immediate");
        assert_eq!(session.format(), Format::Json);

        let immediate = session
            .persist_set_at(
                &td.path().join("style.toml"),
                ConfigKey::TableStyle,
                "unicode",
            )
            .unwrap();
        assert!(
            immediate,
            "table_style is a rendering key and must be immediate"
        );
        assert_eq!(session.style(), TableStyle::Unicode);
    }

    /// Property 3 (deferred): `persist_set_at` on a scan key (`ext`,
    /// `hidden`, `respect_gitignore`, `exclude`) reports deferred and leaves
    /// the rendering accessors untouched — the store is already loaded, so a
    /// scan setting can't retroactively change what's in it.
    #[test]
    fn persist_set_at_scan_keys_are_deferred_and_do_not_touch_rendering() {
        let td = TempDir::new().unwrap();
        for (key, value) in [
            (ConfigKey::Ext, "mdx"),
            (ConfigKey::Hidden, "true"),
            (ConfigKey::RespectGitignore, "true"),
            (ConfigKey::Exclude, "**/x/**"),
        ] {
            let path = td.path().join(format!("{}.toml", key.as_str()));
            let mut session = empty_session(Settings::default(), Settings::default());
            let before_format = session.format();
            let before_style = session.style();

            let immediate = session.persist_set_at(&path, key, value).unwrap();

            assert!(
                !immediate,
                "{} must be deferred, not immediate",
                key.as_str()
            );
            assert_eq!(
                session.format(),
                before_format,
                "{} must not touch format",
                key.as_str()
            );
            assert_eq!(
                session.style(),
                before_style,
                "{} must not touch style",
                key.as_str()
            );
        }
    }

    /// `.unset` on a key that was never set must be a true no-op: no config
    /// file (or parent directory) materializes for what is semantically a
    /// no-op query, matching `querymatter config unset`'s CLI behavior.
    #[test]
    fn persist_unset_at_absent_key_does_not_create_the_file() {
        let td = TempDir::new().unwrap();
        let path = td.path().join("nested").join("config.toml");
        let mut session = empty_session(Settings::default(), Settings::default());

        let (removed, immediate) = session
            .persist_unset_at(&path, ConfigKey::TableStyle)
            .unwrap();
        assert!(!removed);
        assert!(!immediate);
        assert!(
            !path.exists(),
            "no config file must be created for a no-op unset"
        );
        assert!(
            !path.parent().unwrap().exists(),
            "no parent directory must be created for a no-op unset"
        );
    }
}
