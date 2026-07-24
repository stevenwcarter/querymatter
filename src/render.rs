//! Rendering a [`ResultTable`] to the REPL's output formats: an aligned
//! table, JSON, CSV, TSV, a Markdown table, and MySQL-style vertical
//! (`\G`) output.
//!
//! Every format is returned with no trailing newline, so a caller (the REPL
//! printer) can add exactly one when it prints the result.

use std::str::FromStr;

use comfy_table::Table;
use comfy_table::modifiers::{UTF8_ROUND_CORNERS, UTF8_SOLID_INNER_BORDERS};
use comfy_table::presets::{ASCII_MARKDOWN, NOTHING, UTF8_FULL, UTF8_HORIZONTAL_ONLY};
use csv::WriterBuilder;
use serde_json::{Map, Value as JsonValue};

use crate::model::Value;
use crate::query::ResultTable;

/// The output format a rendered [`ResultTable`] can take.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    /// An aligned, bordered table, with borders drawn per the selected
    /// [`TableStyle`].
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

/// The border style used when rendering [`Format::Table`].
///
/// Orthogonal to [`Format`], and consulted for `Format::Table` alone: `md` is
/// a fixed Markdown dialect, and json/csv/tsv are data interchange, so all
/// four ignore it.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum TableStyle {
    /// `comfy-table`'s default ASCII borders (`+---+`), safe on any terminal.
    #[default]
    Ascii,
    /// Rounded UTF-8 box-drawing borders with solid inner lines.
    Unicode,
    /// Horizontal rules only — no vertical borders.
    Compact,
    /// Aligned columns with no borders or rules at all.
    Plain,
}

/// A string that doesn't name a known [`TableStyle`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown table style {0:?} (expected ascii, unicode, compact, or plain)")]
pub struct TableStyleParseError(String);

impl FromStr for TableStyle {
    type Err = TableStyleParseError;

    /// Parses `ascii|unicode|compact|plain`, case-insensitively.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "ascii" => Ok(TableStyle::Ascii),
            "unicode" => Ok(TableStyle::Unicode),
            "compact" => Ok(TableStyle::Compact),
            "plain" => Ok(TableStyle::Plain),
            _ => Err(TableStyleParseError(s.to_string())),
        }
    }
}

/// What a single statement's result set renders as: the session's configured
/// [`Format`], or the per-statement vertical override a `\G` terminator
/// selects.
///
/// Vertical is deliberately *not* a [`Format`] variant. `Format`'s `FromStr`
/// backs `--format` and `.format`, which must stay a closed, round-trippable
/// set; `\G` is the only way to ask for vertical output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// Render in this format, honoring the session's [`TableStyle`].
    Format(Format),
    /// Render one record per block, in `mysql`'s `\G` layout.
    Vertical,
}

/// The number of asterisks flanking a vertical row banner, matching `mysql`.
const BANNER_ASTERISKS: usize = 27;

/// Renders `table` as `output` describes, using `style`'s borders when that
/// resolves to [`Format::Table`].
pub fn render(table: &ResultTable, output: Output, style: TableStyle) -> String {
    match output {
        Output::Vertical => render_vertical(table),
        Output::Format(Format::Table) => render_table(table, style),
        Output::Format(Format::Md) => render_markdown(table),
        Output::Format(Format::Json) => render_json(table),
        Output::Format(Format::Csv) => render_delimited(table, b','),
        Output::Format(Format::Tsv) => render_delimited(table, b'\t'),
    }
}

/// Renders `table` via `comfy-table` with `style`'s borders.
///
/// [`TableStyle::Ascii`] deliberately loads no preset at all rather than
/// loading `ASCII_FULL`: `comfy-table`'s own default is what shipped, so
/// leaving it untouched keeps the default output provably unchanged.
fn render_table(table: &ResultTable, style: TableStyle) -> String {
    let mut ct = new_table(table);
    match style {
        TableStyle::Ascii => {}
        TableStyle::Unicode => {
            ct.load_preset(UTF8_FULL)
                .apply_modifier(UTF8_SOLID_INNER_BORDERS)
                .apply_modifier(UTF8_ROUND_CORNERS);
        }
        TableStyle::Compact => {
            ct.load_preset(UTF8_HORIZONTAL_ONLY);
        }
        TableStyle::Plain => {
            ct.load_preset(NOTHING);
        }
    }
    ct.trim_fmt()
}

/// Renders `table` one record per block, in the layout `mysql`'s `\G`
/// terminator produces: a banner naming the 1-based row number, then one
/// `name: value` line per column, names right-aligned to the widest header.
///
/// Zero rows renders the empty string — there are no headers worth showing
/// without a row, and an empty result must stay distinguishable when piped.
fn render_vertical(table: &ResultTable) -> String {
    let stars = "*".repeat(BANNER_ASTERISKS);
    // Frontmatter keys are overwhelmingly ASCII, so `chars().count()` stands
    // in for display width rather than taking a `unicode-width` dependency
    // for this alone. Rust's `{:>width$}` pads by the same measure.
    let width = table
        .headers
        .iter()
        .map(|header| header.chars().count())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (index, row) in table.rows.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!("{stars} {}. row {stars}", index + 1));
        // Zip rather than index: `ResultTable` guarantees one cell per header
        // (see `query::ResultTable`), but a truncating zip degrades instead of
        // panicking should that ever weaken.
        for (header, value) in table.headers.iter().zip(row) {
            out.push_str(&format!("\n{header:>width$}: {}", value.display()));
        }
    }
    out
}

/// Renders `table` as a Markdown table, independent of any [`TableStyle`].
fn render_markdown(table: &ResultTable) -> String {
    let mut ct = new_table(table);
    ct.load_preset(ASCII_MARKDOWN);
    ct.trim_fmt()
}

/// A `comfy-table` [`Table`] carrying `table`'s headers and rows, with no
/// preset loaded yet.
fn new_table(table: &ResultTable) -> Table {
    let mut ct = Table::new();
    ct.set_header(&table.headers);
    for row in &table.rows {
        ct.add_row(row.iter().map(Value::display));
    }
    ct
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
    // A `ResultTable` guarantees each row has exactly one cell per header
    // (see `query::ResultTable`'s doc comment), so `write_record`'s
    // field-count check can never fail here; the only realistic failure mode
    // left is an OOM writing to the in-memory buffer. Fail loudly rather than
    // silently producing blank output indistinguishable from "zero rows".
    write_delimited(table, delimiter)
        .expect("ResultTable guarantees one cell per header (query::ResultTable)")
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
    let text = String::from_utf8_lossy(&bytes).into_owned();
    // Strip only the writer's final record terminator, not `trim_end()`,
    // which would also eat real trailing whitespace from the last cell's
    // `Value::display()` content (data loss).
    let trimmed = text
        .strip_suffix("\r\n")
        .or_else(|| text.strip_suffix('\n'))
        .unwrap_or(&text);
    Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use clap::ValueEnum;

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
        let s = render(&table(), Output::Format(Format::Json), TableStyle::Ascii);
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["status"], "synced");
        assert_eq!(v[0]["Count"], 2);
    }
    #[test]
    fn csv_has_header_and_rows() {
        let s = render(&table(), Output::Format(Format::Csv), TableStyle::Ascii);
        assert_eq!(s.lines().next().unwrap(), "status,Count");
        assert!(s.contains("synced,2"));
    }
    #[test]
    fn tsv_uses_tabs() {
        let s = render(&table(), Output::Format(Format::Tsv), TableStyle::Ascii);
        assert_eq!(s.lines().next().unwrap(), "status\tCount");
    }
    #[test]
    fn format_from_str() {
        assert_eq!("md".parse::<Format>().unwrap(), Format::Md);
        assert_eq!("markdown".parse::<Format>().unwrap(), Format::Md);
        assert!("bogus".parse::<Format>().is_err());
    }

    /// Every value clap will offer as a completion must also parse through
    /// `FromStr`, which is what the REPL's `.format`/`.style`/`.set` use. If
    /// these ever diverge, a value you can tab-complete becomes a value the
    /// REPL rejects.
    #[test]
    fn format_value_enum_agrees_with_from_str() {
        for variant in <Format as clap::ValueEnum>::value_variants() {
            let possible = variant.to_possible_value().expect("no variant is skipped");
            let name = possible.get_name();
            assert_eq!(
                name.parse::<Format>().expect("clap's name must parse"),
                *variant,
                "clap offers {name:?} but FromStr disagrees"
            );
        }
    }

    #[test]
    fn table_style_value_enum_agrees_with_from_str() {
        for variant in <TableStyle as clap::ValueEnum>::value_variants() {
            let possible = variant.to_possible_value().expect("no variant is skipped");
            let name = possible.get_name();
            assert_eq!(
                name.parse::<TableStyle>().expect("clap's name must parse"),
                *variant,
                "clap offers {name:?} but FromStr disagrees"
            );
        }
    }

    /// The TOML spelling must match the CLI spelling exactly, so a config file
    /// and a command line never disagree about what "md" means.
    #[test]
    fn format_serde_spelling_matches_cli() {
        assert_eq!(toml_value(&Format::Md), "md");
        assert_eq!(toml_value(&Format::Table), "table");
        assert_eq!(toml_value(&Format::Json), "json");
        assert_eq!(toml_value(&Format::Csv), "csv");
        assert_eq!(toml_value(&Format::Tsv), "tsv");
    }

    #[test]
    fn table_style_serde_spelling_matches_cli() {
        assert_eq!(toml_value(&TableStyle::Ascii), "ascii");
        assert_eq!(toml_value(&TableStyle::Unicode), "unicode");
        assert_eq!(toml_value(&TableStyle::Compact), "compact");
        assert_eq!(toml_value(&TableStyle::Plain), "plain");
    }

    /// Serializes `value` the way it will appear in `config.toml`.
    fn toml_value<T: serde::Serialize>(value: &T) -> String {
        serde_json::to_value(value)
            .expect("these enums serialize as plain strings")
            .as_str()
            .expect("as a JSON string")
            .to_string()
    }
    #[test]
    fn table_snapshot() {
        insta::assert_snapshot!(render(
            &table(),
            Output::Format(Format::Table),
            TableStyle::Ascii
        ));
    }
    #[test]
    fn md_snapshot() {
        insta::assert_snapshot!(render(
            &table(),
            Output::Format(Format::Md),
            TableStyle::Ascii
        ));
    }

    /// Regression for a data-loss bug: the delimited-render path used to
    /// `trim_end()` the whole rendered buffer to drop the writer's trailing
    /// record terminator, which also silently stripped real trailing
    /// whitespace from the last cell of the last row.
    #[test]
    fn csv_preserves_trailing_whitespace_in_last_cell() {
        let t = ResultTable {
            headers: vec!["status".into(), "Count".into()],
            rows: vec![
                vec![Value::Str("synced".into()), Value::Int(2)],
                vec![Value::Str("draft".into()), Value::Str("x  ".into())],
            ],
        };
        let s = render(&t, Output::Format(Format::Csv), TableStyle::Ascii);
        assert!(
            s.lines().last().unwrap().ends_with("x  "),
            "trailing whitespace in the last cell must survive rendering, got: {s:?}"
        );
    }

    fn variant_table() -> ResultTable {
        ResultTable {
            headers: vec!["n".into(), "b".into(), "f".into(), "l".into()],
            rows: vec![vec![
                Value::Null,
                Value::Bool(true),
                Value::Float(1.5),
                Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
            ]],
        }
    }

    #[test]
    fn json_renders_null_bool_float_list() {
        let s = render(
            &variant_table(),
            Output::Format(Format::Json),
            TableStyle::Ascii,
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["n"], serde_json::Value::Null);
        assert_eq!(v[0]["b"], serde_json::json!(true));
        assert_eq!(v[0]["f"], serde_json::json!(1.5));
        assert_eq!(v[0]["l"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn csv_renders_null_bool_float_list() {
        let s = render(
            &variant_table(),
            Output::Format(Format::Csv),
            TableStyle::Ascii,
        );
        let data_line = s.lines().nth(1).unwrap();
        // Null -> empty cell; Bool/Float -> Value::display(); List -> its
        // elements' display() joined ", " (and quoted by the csv writer
        // since the joined text itself contains the delimiter).
        assert_eq!(data_line, ",true,1.5,\"a, b\"");
    }

    #[test]
    fn table_renders_null_bool_float_list() {
        let s = render(
            &variant_table(),
            Output::Format(Format::Table),
            TableStyle::Ascii,
        );
        assert!(s.contains("true"));
        assert!(s.contains("1.5"));
        assert!(s.contains("a, b"));
    }

    #[test]
    fn table_style_from_str() {
        assert_eq!("ascii".parse::<TableStyle>().unwrap(), TableStyle::Ascii);
        assert_eq!(
            "UNICODE".parse::<TableStyle>().unwrap(),
            TableStyle::Unicode
        );
        assert_eq!(
            "Compact".parse::<TableStyle>().unwrap(),
            TableStyle::Compact
        );
        assert_eq!("plain".parse::<TableStyle>().unwrap(), TableStyle::Plain);
        assert!("fancy".parse::<TableStyle>().is_err());
    }

    #[test]
    fn table_style_defaults_to_ascii() {
        assert_eq!(TableStyle::default(), TableStyle::Ascii);
    }

    #[test]
    fn unicode_style_draws_box_characters() {
        let s = render(&table(), Output::Format(Format::Table), TableStyle::Unicode);
        assert!(s.contains('╭'), "expected rounded corners, got:\n{s}");
        assert!(
            s.contains('│'),
            "expected solid vertical borders, got:\n{s}"
        );
    }

    #[test]
    fn plain_style_draws_no_borders() {
        let s = render(&table(), Output::Format(Format::Table), TableStyle::Plain);
        assert!(!s.contains('|'), "expected no borders, got:\n{s}");
        assert!(!s.contains('+'), "expected no borders, got:\n{s}");
        assert!(s.contains("synced"), "content must survive, got:\n{s}");
    }

    /// The style knob is for `Format::Table` alone: `md` is a fixed Markdown
    /// dialect and json/csv/tsv are data interchange. Pinning this stops a
    /// future "just pass the style through" refactor from corrupting piped
    /// output.
    #[test]
    fn non_table_formats_ignore_style() {
        let t = table();
        for format in [Format::Json, Format::Csv, Format::Tsv, Format::Md] {
            let baseline = render(&t, Output::Format(format), TableStyle::Ascii);
            for style in [TableStyle::Unicode, TableStyle::Compact, TableStyle::Plain] {
                assert_eq!(
                    render(&t, Output::Format(format), style),
                    baseline,
                    "{format:?} must ignore {style:?}"
                );
            }
        }
    }

    #[test]
    fn table_unicode_snapshot() {
        insta::assert_snapshot!(render(
            &table(),
            Output::Format(Format::Table),
            TableStyle::Unicode
        ));
    }

    #[test]
    fn table_compact_snapshot() {
        insta::assert_snapshot!(render(
            &table(),
            Output::Format(Format::Table),
            TableStyle::Compact
        ));
    }

    #[test]
    fn table_plain_snapshot() {
        insta::assert_snapshot!(render(
            &table(),
            Output::Format(Format::Table),
            TableStyle::Plain
        ));
    }

    #[test]
    fn vertical_snapshot() {
        insta::assert_snapshot!(render(&table(), Output::Vertical, TableStyle::Ascii));
    }

    /// Column names are right-aligned to the widest header, so the `:`
    /// separators line up however uneven the names are.
    #[test]
    fn vertical_alignment_snapshot() {
        let t = ResultTable {
            headers: vec!["id".into(), "file.path".into(), "s".into()],
            rows: vec![vec![
                Value::Int(1),
                Value::Str("notes/a.md".into()),
                Value::Str("draft".into()),
            ]],
        };
        insta::assert_snapshot!(render(&t, Output::Vertical, TableStyle::Ascii));
    }

    /// There are no headers worth showing without a row, and an empty result
    /// must stay distinguishable when piped.
    #[test]
    fn vertical_zero_rows_is_empty() {
        let t = ResultTable {
            headers: vec!["status".into()],
            rows: vec![],
        };
        assert_eq!(render(&t, Output::Vertical, TableStyle::Ascii), "");
    }

    /// Every format returns text with no trailing newline; the printers add
    /// exactly one. Vertical builds its output by hand, so it gets its own
    /// assertion rather than inheriting confidence from the other formats.
    #[test]
    fn vertical_has_no_trailing_newline() {
        let s = render(&table(), Output::Vertical, TableStyle::Ascii);
        assert!(
            !s.ends_with('\n'),
            "must not end with a newline, got: {s:?}"
        );
    }

    #[test]
    fn vertical_renders_null_bool_float_list() {
        let s = render(&variant_table(), Output::Vertical, TableStyle::Ascii);
        assert!(s.contains("b: true"), "got:\n{s}");
        assert!(s.contains("f: 1.5"), "got:\n{s}");
        assert!(s.contains("l: a, b"), "got:\n{s}");
        assert!(
            s.contains("n: \n") || s.ends_with("n: "),
            "null is empty, got:\n{s}"
        );
    }

    /// `\G` means "show me this record-wise" whatever the standing format is,
    /// so the style knob has no say either.
    #[test]
    fn vertical_ignores_table_style() {
        let baseline = render(&table(), Output::Vertical, TableStyle::Ascii);
        for style in [TableStyle::Unicode, TableStyle::Compact, TableStyle::Plain] {
            assert_eq!(render(&table(), Output::Vertical, style), baseline);
        }
    }

    #[test]
    fn vertical_numbers_rows_from_one() {
        let s = render(&table(), Output::Vertical, TableStyle::Ascii);
        assert!(s.contains(" 1. row "), "got:\n{s}");
        assert!(s.contains(" 2. row "), "got:\n{s}");
    }
}
