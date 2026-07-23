//! Rendering a [`ResultTable`] to the REPL's output formats: an aligned
//! table, JSON, CSV, TSV, and a Markdown table.
//!
//! Every format is returned with no trailing newline, so a caller (the REPL
//! printer) can add exactly one when it prints the result.

use std::str::FromStr;

use comfy_table::Table;
use comfy_table::presets::ASCII_MARKDOWN;
use csv::WriterBuilder;
use serde_json::{Map, Value as JsonValue};

use crate::model::Value;
use crate::query::ResultTable;

/// The output format a rendered [`ResultTable`] can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// An aligned, bordered table (via `comfy-table`'s default ASCII style).
    Table,
    /// A JSON array of objects keyed by column header.
    Json,
    /// Comma-separated values with a header row.
    Csv,
    /// Tab-separated values with a header row.
    Tsv,
    /// A Markdown table.
    Md,
}

/// A string that doesn't name a known [`Format`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown output format {0:?} (expected table, json, csv, tsv, or md)")]
pub struct FormatParseError(String);

impl FromStr for Format {
    type Err = FormatParseError;

    /// Parses `table|json|csv|tsv|md`, case-insensitively; `markdown` is
    /// accepted as an alias for `md`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "table" => Ok(Format::Table),
            "json" => Ok(Format::Json),
            "csv" => Ok(Format::Csv),
            "tsv" => Ok(Format::Tsv),
            "md" | "markdown" => Ok(Format::Md),
            _ => Err(FormatParseError(s.to_string())),
        }
    }
}

/// Renders `table` in the given output `format`.
pub fn render(table: &ResultTable, format: Format) -> String {
    match format {
        Format::Table => render_table(table, None),
        Format::Md => render_table(table, Some(ASCII_MARKDOWN)),
        Format::Json => render_json(table),
        Format::Csv => render_delimited(table, b','),
        Format::Tsv => render_delimited(table, b'\t'),
    }
}

/// Renders `table` via `comfy-table`, loading `preset` when given (otherwise
/// keeping `comfy-table`'s default ASCII style).
fn render_table(table: &ResultTable, preset: Option<&str>) -> String {
    let mut ct = Table::new();
    if let Some(preset) = preset {
        ct.load_preset(preset);
    }
    ct.set_header(&table.headers);
    for row in &table.rows {
        ct.add_row(row.iter().map(Value::display));
    }
    ct.trim_fmt()
}

/// Renders `table` as a JSON array of objects keyed by column header.
fn render_json(table: &ResultTable) -> String {
    let rows: Vec<JsonValue> = table
        .rows
        .iter()
        .map(|row| {
            let fields: Map<String, JsonValue> = table
                .headers
                .iter()
                .zip(row)
                .map(|(header, value)| (header.clone(), to_json(value)))
                .collect();
            JsonValue::Object(fields)
        })
        .collect();
    // A JSON array built entirely from these scalar/array conversions can't
    // fail to serialize; fall back to an empty string rather than panicking.
    serde_json::to_string_pretty(&rows).unwrap_or_default()
}

/// Converts a query [`Value`] to its `serde_json::Value` equivalent.
fn to_json(value: &Value) -> JsonValue {
    match value {
        Value::Null => JsonValue::Null,
        Value::Bool(b) => JsonValue::Bool(*b),
        Value::Int(i) => JsonValue::Number((*i).into()),
        Value::Float(f) => {
            serde_json::Number::from_f64(*f).map_or(JsonValue::Null, JsonValue::Number)
        }
        Value::Str(s) => JsonValue::String(s.clone()),
        Value::List(items) => JsonValue::Array(items.iter().map(to_json).collect()),
    }
}

/// Renders `table` as `delimiter`-separated text with a header row.
fn render_delimited(table: &ResultTable, delimiter: u8) -> String {
    // A `ResultTable` guarantees each row has exactly one cell per header,
    // so `write_record`'s field-count check can never fail here; the only
    // realistic failure mode left is an OOM writing to the in-memory buffer.
    write_delimited(table, delimiter).unwrap_or_default()
}

fn write_delimited(table: &ResultTable, delimiter: u8) -> csv::Result<String> {
    let mut writer = WriterBuilder::new()
        .delimiter(delimiter)
        .from_writer(Vec::new());
    writer.write_record(&table.headers)?;
    for row in &table.rows {
        writer.write_record(row.iter().map(Value::display))?;
    }
    let bytes = writer
        .into_inner()
        .map_err(csv::IntoInnerError::into_error)?;
    Ok(String::from_utf8_lossy(&bytes).trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use crate::query::ResultTable;

    fn table() -> ResultTable {
        ResultTable {
            headers: vec!["status".into(), "Count".into()],
            rows: vec![
                vec![Value::Str("synced".into()), Value::Int(2)],
                vec![Value::Str("draft".into()), Value::Int(1)],
            ],
        }
    }
    #[test]
    fn json_roundtrips() {
        let s = render(&table(), Format::Json);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["status"], "synced");
        assert_eq!(v[0]["Count"], 2);
    }
    #[test]
    fn csv_has_header_and_rows() {
        let s = render(&table(), Format::Csv);
        assert_eq!(s.lines().next().unwrap(), "status,Count");
        assert!(s.contains("synced,2"));
    }
    #[test]
    fn tsv_uses_tabs() {
        let s = render(&table(), Format::Tsv);
        assert_eq!(s.lines().next().unwrap(), "status\tCount");
    }
    #[test]
    fn format_from_str() {
        assert_eq!("md".parse::<Format>().unwrap(), Format::Md);
        assert_eq!("markdown".parse::<Format>().unwrap(), Format::Md);
        assert!("bogus".parse::<Format>().is_err());
    }
    #[test]
    fn table_snapshot() {
        insta::assert_snapshot!(render(&table(), Format::Table));
    }
    #[test]
    fn md_snapshot() {
        insta::assert_snapshot!(render(&table(), Format::Md));
    }
}
