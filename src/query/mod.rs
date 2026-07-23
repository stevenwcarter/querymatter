//! The querymatter query language: the AST, the SQL-subset parser, and the
//! executor.
//!
//! [`ast`] defines the query AST; [`parse`] lowers a SQL string into it;
//! [`exec`] evaluates a parsed [`ast::Query`] against a set of records.

pub mod ast;
pub mod exec;
pub mod parse;

pub use exec::{ExecError, execute};
pub use parse::{ParseError, parse};

/// The output of running a query: column headers plus the projected rows, in
/// final (filtered / ordered / limited) order.
///
/// Each row has exactly one [`crate::model::Value`] per header, in the same
/// order as `headers`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultTable {
    /// Column headers, in projection order.
    pub headers: Vec<String>,
    /// Projected rows.
    pub rows: Vec<Vec<crate::model::Value>>,
}
