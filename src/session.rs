//! A [`Session`] bundles a record store with an output [`Format`] and turns
//! SQL text into rendered result strings — the shared core behind one-shot,
//! batch, and (Task 11) interactive modes.

use anyhow::Context;

use crate::query::{self, ResultTable};
use crate::render::{self, Format};
use crate::store::{LoadReport, RecordStore};

/// Owns the queryable store plus the current output format, and runs queries
/// against them.
pub struct Session {
    store: Box<dyn RecordStore>,
    /// The format rendered results are produced in; mutable at runtime (the
    /// REPL's `\format` command, Task 11).
    pub format: Format,
}

impl Session {
    /// Builds a session over `store`, rendering results in `format`.
    pub fn new(store: Box<dyn RecordStore>, format: Format) -> Self {
        Session { store, format }
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
        Ok(render::render(&table, self.format))
    }

    /// Switches the output format used by [`render_query`](Self::render_query).
    // Part of the Session API surface; first called by the interactive REPL
    // (Task 11), so unreferenced in this bin crate until then.
    #[allow(dead_code)]
    pub fn set_format(&mut self, f: Format) {
        self.format = f;
    }

    /// Rescans every tracked root, returning the combined load report.
    // Part of the Session API surface; first called by the interactive REPL
    // (Task 11), so unreferenced in this bin crate until then.
    #[allow(dead_code)]
    pub fn reload(&mut self) -> LoadReport {
        self.store.reload_all()
    }

    /// The current schema: the sorted union of frontmatter field names.
    // Part of the Session API surface; first called by the interactive REPL
    // (Task 11), so unreferenced in this bin crate until then.
    #[allow(dead_code)]
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
}
