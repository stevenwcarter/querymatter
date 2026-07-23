//! A [`Session`] bundles a record store with an output [`Format`] and turns
//! SQL text into rendered result strings — the shared core behind one-shot,
//! batch, and interactive modes.

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::cache;
use crate::query::{self, ResultTable};
use crate::render::{self, Format, Output, TableStyle};
use crate::store::{LoadReport, RecordStore};

/// Owns the queryable store plus the current output format, and runs queries
/// against them.
pub struct Session {
    store: Box<dyn RecordStore>,
    /// The format rendered results are produced in; mutable at runtime (the
    /// REPL's `.format` command).
    pub format: Format,
    /// The border style used when rendering [`Format::Table`]; mutable at
    /// runtime (the REPL's `.style` command).
    pub style: TableStyle,
    /// The `.querymatter` vault this session's store is backed by, when it
    /// is cache-backed. `None` for a live (no-cache) session, in which case
    /// [`refresh`](Self::refresh) falls back to an in-memory-only reload.
    vault: Option<PathBuf>,
}

impl Session {
    /// Builds a session over `store`, rendering results in `format` with
    /// `style`'s table borders.
    ///
    /// `vault` is the `.querymatter` directory backing `store`, when it was
    /// built via [`crate::store::InMemoryStore::from_cache`]; pass `None`
    /// for a live (no-cache) store.
    pub fn new(
        store: Box<dyn RecordStore>,
        format: Format,
        style: TableStyle,
        vault: Option<PathBuf>,
    ) -> Self {
        Session {
            store,
            format,
            style,
            vault,
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

    /// Runs `sql` and renders the result in the session's current format.
    ///
    /// The returned string carries no trailing newline (see [`render`]); the
    /// caller adds exactly one when printing.
    pub fn render_query(&self, sql: &str) -> anyhow::Result<String> {
        let table = self.run(sql)?;
        Ok(render::render(
            &table,
            Output::Format(self.format),
            self.style,
        ))
    }

    /// Switches the output format used by [`render_query`](Self::render_query).
    pub fn set_format(&mut self, f: Format) {
        self.format = f;
    }

    /// Switches the table border style used by
    /// [`render_query`](Self::render_query).
    pub fn set_style(&mut self, style: TableStyle) {
        self.style = style;
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

/// Splits `input` into individual statements on top-level `;`, trimming each
/// and dropping the empties.
///
/// "Top-level" means semicolons inside single- or double-quoted string
/// literals do not split — so `WHERE title = 'a;b'` stays one statement.
pub fn split_statements(input: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for ch in input.chars() {
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
                    push_statement(&mut statements, &current);
                    current.clear();
                }
                _ => current.push(ch),
            },
        }
    }
    push_statement(&mut statements, &current);
    statements
}

/// Trims `stmt` and pushes it onto `out` when it is not empty.
fn push_statement(out: &mut Vec<String>, stmt: &str) {
    let trimmed = stmt.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
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

    #[test]
    fn split_statements_basic() {
        assert_eq!(
            split_statements(" SELECT 1 ; SELECT 2 ;"),
            vec!["SELECT 1", "SELECT 2"]
        );
        assert_eq!(split_statements("SELECT 1"), vec!["SELECT 1"]);
        assert_eq!(split_statements("  ;; "), Vec::<String>::new());
    }

    #[test]
    fn semicolon_inside_quotes_does_not_split() {
        assert_eq!(
            split_statements("SELECT status WHERE title = 'a;b'"),
            vec!["SELECT status WHERE title = 'a;b'"]
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
            Format::Table,
            TableStyle::Ascii,
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
        let mut session = Session::new(Box::new(store), Format::Table, TableStyle::Ascii, None);

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
            Format::Table,
            TableStyle::Ascii,
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
            Format::Table,
            TableStyle::Ascii,
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
