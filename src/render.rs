//! Rendering a [`ResultTable`] to the REPL's output formats: an aligned
//! table, JSON, CSV, TSV, a Markdown table, and MySQL-style vertical
//! (`\G`) output.
//!
//! Every format is returned with no trailing newline, so a caller (the REPL
//! printer) can add exactly one when it prints the result.

use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};
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
    #[value(alias = "markdown")]
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
/// resolves to [`Format::Table`], and omitting the header row for
/// table/md/csv/tsv when `header` is `false`.
///
/// JSON is keyed by column header regardless — dropping the header would
/// leave nothing to key objects by — and vertical (`\G`) output shows
/// right-aligned labels rather than a header row, so both ignore `header`
/// entirely.
pub fn render(table: &ResultTable, output: Output, style: TableStyle, header: bool) -> String {
    let mut buf = Vec::new();
    render_to(&mut buf, table, output, style, header)
        .expect("writing to an in-memory Vec is infallible");
    let s = String::from_utf8_lossy(&buf);
    s.strip_suffix('\n').unwrap_or(&s).to_string()
}

/// Renders `table` per `output` directly into `w`, newline-terminated. The
/// bulk-export formats (json/csv/tsv) stream row-by-row so a large export
/// starts writing immediately and never materializes a second full copy; the
/// interactive formats (table/md/vertical) build their string first (they are
/// interactive-scale). The single trailing newline is owned here (not the
/// sink), which is what lets csv/tsv stream without buffering the last record.
pub fn render_to(
    w: &mut (impl Write + ?Sized),
    table: &ResultTable,
    output: Output,
    style: TableStyle,
    header: bool,
) -> io::Result<()> {
    match output {
        Output::Vertical => writeln_block(w, &render_vertical(table)),
        Output::Format(Format::Table) => writeln_block(w, &render_table(table, style, header)),
        Output::Format(Format::Md) => writeln_block(w, &render_markdown(table, header)),
        Output::Format(Format::Json) => {
            serde_json::to_writer_pretty(&mut *w, &JsonRows(table)).map_err(io::Error::other)?;
            w.write_all(b"\n")
        }
        Output::Format(Format::Csv) => stream_delimited(w, table, b',', header),
        Output::Format(Format::Tsv) => stream_delimited(w, table, b'\t', header),
    }
}

/// Writes a pre-built block followed by exactly one newline.
fn writeln_block(w: &mut (impl Write + ?Sized), block: &str) -> io::Result<()> {
    w.write_all(block.as_bytes())?;
    w.write_all(b"\n")
}

/// Serializes a [`ResultTable`] as a JSON array of objects keyed by column
/// header, one object per row, streamed row-by-row. Each row rebuilds the
/// same `serde_json::Map` construction [`to_json`]'s caller used, so key
/// ordering is byte-identical to the buffered path regardless of
/// serde_json's `preserve_order` feature (absent here → sorted keys).
struct JsonRows<'a>(&'a ResultTable);

impl serde::Serialize for JsonRows<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(self.0.rows.len()))?;
        for row in &self.0.rows {
            let fields: Map<String, JsonValue> = self
                .0
                .headers
                .iter()
                .zip(row)
                .map(|(header, value)| (header.clone(), to_json(value)))
                .collect();
            seq.serialize_element(&JsonValue::Object(fields))?;
        }
        seq.end()
    }
}

/// Streams `table` as `delimiter`-separated records directly into `w`. csv's
/// default record terminator is `\n`, so the final record's terminator is the
/// block's single trailing newline — matching the buffered path's
/// strip-then-append. When nothing is written (`!header && no rows`), emit one
/// bare newline to match the old `println!("")`.
fn stream_delimited(
    w: &mut (impl Write + ?Sized),
    table: &ResultTable,
    delimiter: u8,
    header: bool,
) -> io::Result<()> {
    {
        let mut writer = WriterBuilder::new()
            .delimiter(delimiter)
            .from_writer(&mut *w);
        if header {
            writer
                .write_record(&table.headers)
                .map_err(io::Error::other)?;
        }
        for row in &table.rows {
            writer
                .write_record(row.iter().map(Value::display))
                .map_err(io::Error::other)?;
        }
        writer.flush()?;
        // `writer` is dropped at the end of this block, releasing its `&mut *w`
        // borrow so the empty-case newline below can write to `w`.
    }
    if !header && table.rows.is_empty() {
        w.write_all(b"\n")?;
    }
    Ok(())
}

/// Table width-fitting is enabled only when stdout is a real terminal, so
/// piped/redirected output stays byte-identical (the interchange-format rule).
fn want_dynamic_width(is_tty: bool) -> bool {
    is_tty
}

/// Renders `table` via `comfy-table` with `style`'s borders.
///
/// [`TableStyle::Ascii`] deliberately loads no preset at all rather than
/// loading `ASCII_FULL`: `comfy-table`'s own default is what shipped, so
/// leaving it untouched keeps the default output provably unchanged.
///
/// Width fitting to the terminal is applied here (gated by
/// [`want_dynamic_width`]), not in [`new_table`]/[`render_markdown`]: Markdown
/// is a fixed interchange format and must stay terminal-independent.
fn render_table(table: &ResultTable, style: TableStyle, header: bool) -> String {
    let mut ct = new_table(table, header);
    if want_dynamic_width(std::io::stdout().is_terminal()) {
        ct.set_content_arrangement(comfy_table::ContentArrangement::Dynamic);
    }
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
            let header = sanitize_for_terminal(header);
            out.push_str(&format!("\n{header:>width$}: {}", sanitized_display(value)));
        }
    }
    out
}

/// Renders `table` as a Markdown table, independent of any [`TableStyle`].
fn render_markdown(table: &ResultTable, header: bool) -> String {
    let mut ct = new_table(table, header);
    ct.load_preset(ASCII_MARKDOWN);
    ct.trim_fmt()
}

/// Neutralizes C0/C1 control characters — ESC, `\r`, and their kin — so a
/// frontmatter value can't forge terminal escape sequences (screen clears,
/// cursor moves, forged rows) when rendered into a table cell or vertical
/// (`\G`) line. `\t` is left untouched; everything else `char::is_control`
/// reports becomes U+FFFD.
///
/// Used **only** by the table ([`new_table`]) and vertical
/// ([`render_vertical`]) paths. Never call this from the csv/tsv/json paths —
/// those are a stable, byte-identity interchange contract (see the module
/// doc) and must keep receiving raw [`Value::display`] output untouched.
fn sanitize_for_terminal(s: &str) -> Cow<'_, str> {
    if !s.chars().any(|c| c.is_control() && c != '\t') {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .map(|c| {
                if c.is_control() && c != '\t' {
                    '\u{FFFD}'
                } else {
                    c
                }
            })
            .collect(),
    )
}

/// [`Value::display`], sanitized for terminal display via
/// [`sanitize_for_terminal`]. Used by the table and vertical (`\G`) paths
/// only — see that function's doc for why.
fn sanitized_display(value: &Value) -> String {
    sanitize_for_terminal(&value.display()).into_owned()
}

/// A `comfy-table` [`Table`] carrying `table`'s headers (unless `header` is
/// `false`) and rows, with no preset loaded yet.
fn new_table(table: &ResultTable, header: bool) -> Table {
    let mut ct = Table::new();
    if header {
        ct.set_header(table.headers.iter().map(|h| sanitize_for_terminal(h)));
    }
    for row in &table.rows {
        ct.add_row(row.iter().map(sanitized_display));
    }
    ct
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
        Value::Date(_) | Value::DateTime(_) => JsonValue::String(value.display()),
        Value::List(items) => JsonValue::Array(items.iter().map(to_json).collect()),
        Value::Map(m) => {
            JsonValue::Object(m.iter().map(|(k, v)| (k.clone(), to_json(v))).collect())
        }
    }
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
    /// Task 3 (B3) RED: characterizes the sanitizer's contract before it's
    /// implemented. `sanitize_for_terminal` is currently a no-op placeholder,
    /// so this fails until the real implementation lands.
    #[test]
    fn sanitize_for_terminal_neutralizes_control_bytes_but_keeps_tab() {
        let s = "before\u{1b}[2Jafter\rend\ttab";
        let sanitized = sanitize_for_terminal(s);
        assert!(
            !sanitized.contains('\u{1b}'),
            "ESC must be neutralized, got {sanitized:?}"
        );
        assert!(
            !sanitized.contains('\r'),
            "CR must be neutralized, got {sanitized:?}"
        );
        assert!(
            sanitized.contains('\t'),
            "tab must survive, got {sanitized:?}"
        );
        assert!(
            sanitized.contains("before")
                && sanitized.contains("after")
                && sanitized.contains("tab"),
            "normal text must survive, got {sanitized:?}"
        );
    }

    #[test]
    fn json_roundtrips() {
        let s = render(
            &table(),
            Output::Format(Format::Json),
            TableStyle::Ascii,
            true,
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v[0]["status"], "synced");
        assert_eq!(v[0]["Count"], 2);
    }
    /// Spec W1: a zero-row table renders to the bare empty JSON array — the
    /// pretty-printer's empty-array form, not `"[\n]"` or `"[\n\n]"`. Pins
    /// the value `render_to`'s empty case is built on, complementing the
    /// end-to-end `empty_result_json_is_bracket_bracket_newline` CLI test in
    /// `tests/cli.rs`.
    #[test]
    fn json_empty_table_is_bracket_bracket() {
        let empty = ResultTable {
            headers: vec!["status".into()],
            rows: vec![],
        };
        assert_eq!(
            render(
                &empty,
                Output::Format(Format::Json),
                TableStyle::Ascii,
                true
            ),
            "[]"
        );
    }

    #[test]
    fn csv_has_header_and_rows() {
        let s = render(
            &table(),
            Output::Format(Format::Csv),
            TableStyle::Ascii,
            true,
        );
        assert_eq!(s.lines().next().unwrap(), "status,Count");
        assert!(s.contains("synced,2"));
    }
    #[test]
    fn tsv_uses_tabs() {
        let s = render(
            &table(),
            Output::Format(Format::Tsv),
            TableStyle::Ascii,
            true,
        );
        assert_eq!(s.lines().next().unwrap(), "status\tCount");
    }

    #[test]
    fn csv_no_header_omits_header_row() {
        let s = render(
            &table(),
            Output::Format(Format::Csv),
            TableStyle::Ascii,
            false,
        );
        assert_eq!(s.lines().next().unwrap(), "synced,2");
        assert!(!s.contains("status,Count"));
    }

    #[test]
    fn tsv_no_header_omits_header_row() {
        let s = render(
            &table(),
            Output::Format(Format::Tsv),
            TableStyle::Ascii,
            false,
        );
        assert_eq!(s.lines().next().unwrap(), "synced\t2");
    }

    #[test]
    fn md_no_header_omits_header_row() {
        let s = render(
            &table(),
            Output::Format(Format::Md),
            TableStyle::Ascii,
            false,
        );
        assert!(!s.contains("status"), "header must be gone, got:\n{s}");
        assert!(
            !s.contains("---"),
            "orphan header separator row must be gone, got:\n{s}"
        );
        assert!(s.contains("synced"));
    }

    #[test]
    fn table_no_header_omits_header_row() {
        let s = render(
            &table(),
            Output::Format(Format::Table),
            TableStyle::Ascii,
            false,
        );
        assert!(!s.contains("status"), "header must be gone, got:\n{s}");
        assert!(s.contains("synced"));
    }

    #[test]
    fn json_and_vertical_ignore_header_flag() {
        let j_on = render(
            &table(),
            Output::Format(Format::Json),
            TableStyle::Ascii,
            true,
        );
        let j_off = render(
            &table(),
            Output::Format(Format::Json),
            TableStyle::Ascii,
            false,
        );
        assert_eq!(
            j_on, j_off,
            "JSON is keyed by header; the flag must not change it"
        );
        let v_on = render(&table(), Output::Vertical, TableStyle::Ascii, true);
        let v_off = render(&table(), Output::Vertical, TableStyle::Ascii, false);
        assert_eq!(v_on, v_off, "vertical labels are not a header row");
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
            TableStyle::Ascii,
            true
        ));
    }
    #[test]
    fn md_snapshot() {
        insta::assert_snapshot!(render(
            &table(),
            Output::Format(Format::Md),
            TableStyle::Ascii,
            true
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
        let s = render(&t, Output::Format(Format::Csv), TableStyle::Ascii, true);
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
    fn json_export_emits_nested_object_for_map() {
        use crate::model::Value;
        use indexmap::IndexMap;
        let mut inner = IndexMap::new();
        inner.insert("low".to_string(), Value::Int(5));
        let v = Value::Map(inner);
        // to_json is module-private; assert via the JsonValue it builds.
        let j = super::to_json(&v);
        assert_eq!(j, serde_json::json!({ "low": 5 }));
    }

    #[test]
    fn json_renders_null_bool_float_list() {
        let s = render(
            &variant_table(),
            Output::Format(Format::Json),
            TableStyle::Ascii,
            true,
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
            true,
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
            true,
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
        let s = render(
            &table(),
            Output::Format(Format::Table),
            TableStyle::Unicode,
            true,
        );
        assert!(s.contains('╭'), "expected rounded corners, got:\n{s}");
        assert!(
            s.contains('│'),
            "expected solid vertical borders, got:\n{s}"
        );
    }

    #[test]
    fn plain_style_draws_no_borders() {
        let s = render(
            &table(),
            Output::Format(Format::Table),
            TableStyle::Plain,
            true,
        );
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
            let baseline = render(&t, Output::Format(format), TableStyle::Ascii, true);
            for style in [TableStyle::Unicode, TableStyle::Compact, TableStyle::Plain] {
                assert_eq!(
                    render(&t, Output::Format(format), style, true),
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
            TableStyle::Unicode,
            true
        ));
    }

    #[test]
    fn table_compact_snapshot() {
        insta::assert_snapshot!(render(
            &table(),
            Output::Format(Format::Table),
            TableStyle::Compact,
            true
        ));
    }

    #[test]
    fn table_plain_snapshot() {
        insta::assert_snapshot!(render(
            &table(),
            Output::Format(Format::Table),
            TableStyle::Plain,
            true
        ));
    }

    #[test]
    fn vertical_snapshot() {
        insta::assert_snapshot!(render(&table(), Output::Vertical, TableStyle::Ascii, true));
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
        insta::assert_snapshot!(render(&t, Output::Vertical, TableStyle::Ascii, true));
    }

    /// There are no headers worth showing without a row, and an empty result
    /// must stay distinguishable when piped.
    #[test]
    fn vertical_zero_rows_is_empty() {
        let t = ResultTable {
            headers: vec!["status".into()],
            rows: vec![],
        };
        assert_eq!(render(&t, Output::Vertical, TableStyle::Ascii, true), "");
    }

    /// Every format returns text with no trailing newline; the printers add
    /// exactly one. Vertical builds its output by hand, so it gets its own
    /// assertion rather than inheriting confidence from the other formats.
    #[test]
    fn vertical_has_no_trailing_newline() {
        let s = render(&table(), Output::Vertical, TableStyle::Ascii, true);
        assert!(
            !s.ends_with('\n'),
            "must not end with a newline, got: {s:?}"
        );
    }

    #[test]
    fn vertical_renders_null_bool_float_list() {
        let s = render(&variant_table(), Output::Vertical, TableStyle::Ascii, true);
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
        let baseline = render(&table(), Output::Vertical, TableStyle::Ascii, true);
        for style in [TableStyle::Unicode, TableStyle::Compact, TableStyle::Plain] {
            assert_eq!(render(&table(), Output::Vertical, style, true), baseline);
        }
    }

    #[test]
    fn vertical_numbers_rows_from_one() {
        let s = render(&table(), Output::Vertical, TableStyle::Ascii, true);
        assert!(s.contains(" 1. row "), "got:\n{s}");
        assert!(s.contains(" 2. row "), "got:\n{s}");
    }

    #[test]
    fn table_uses_dynamic_arrangement_only_on_a_tty() {
        // non-TTY: no dynamic width (byte-identical to today).
        assert!(!want_dynamic_width(false));
        // TTY: dynamic width on.
        assert!(want_dynamic_width(true));
    }

    /// `render_markdown` never consults the terminal, so its output is fixed
    /// regardless of whether stdout is a TTY.
    #[test]
    fn markdown_render_is_terminal_independent() {
        let a = render(
            &table(),
            Output::Format(Format::Md),
            TableStyle::Ascii,
            true,
        );
        let b = render(
            &table(),
            Output::Format(Format::Md),
            TableStyle::Ascii,
            true,
        );
        assert_eq!(a, b);
        assert!(
            a.contains("status"),
            "md keeps its header/content, got:\n{a}"
        );
    }

    /// W47 W1: render_to writes exactly what render() returns plus one newline,
    /// for every format — the byte-identity contract the streaming path rests on.
    #[test]
    fn render_to_equals_render_plus_newline_all_formats() {
        let t = table();
        for output in [
            Output::Format(Format::Table),
            Output::Format(Format::Md),
            Output::Format(Format::Json),
            Output::Format(Format::Csv),
            Output::Format(Format::Tsv),
            Output::Vertical,
        ] {
            let buffered = render(&t, output, TableStyle::Ascii, true);
            let mut streamed = Vec::new();
            render_to(&mut streamed, &t, output, TableStyle::Ascii, true).unwrap();
            assert_eq!(
                String::from_utf8(streamed).unwrap(),
                format!("{buffered}\n"),
                "{output:?} render_to must equal render() + newline"
            );
        }
    }

    /// W47 W1 edge: an empty result with the header suppressed still emits one
    /// bare newline for csv/tsv, matching the old `println!("")`.
    #[test]
    fn render_to_empty_no_header_csv_emits_one_newline() {
        let empty = ResultTable {
            headers: vec!["status".into()],
            rows: vec![],
        };
        for fmt in [Format::Csv, Format::Tsv] {
            let mut streamed = Vec::new();
            render_to(
                &mut streamed,
                &empty,
                Output::Format(fmt),
                TableStyle::Ascii,
                false,
            )
            .unwrap();
            assert_eq!(String::from_utf8(streamed).unwrap(), "\n", "{fmt:?}");
            // and it still equals render() + "\n"
            let buffered = render(&empty, Output::Format(fmt), TableStyle::Ascii, false);
            assert_eq!(buffered, "");
        }
    }
}
