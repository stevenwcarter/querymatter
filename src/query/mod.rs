//! The querymatter query language: the AST and the SQL-subset parser.
//!
//! [`ast`] defines the query AST; [`parse`] lowers a SQL string into it. The
//! executor (added in a later task) consumes the AST produced here.

pub mod ast;
pub mod parse;

pub use parse::{ParseError, parse};
