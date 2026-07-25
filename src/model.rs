use indexmap::IndexMap;
use std::cmp::Ordering;
use std::path::Path;

/// A dynamically-typed value read from Markdown YAML frontmatter.
///
/// This is the common currency for query evaluation: frontmatter fields are
/// parsed into `Value`s, and the query engine (comparisons, `ORDER BY`,
/// `MIN`/`MAX`) operates on them without knowing the original YAML shape.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Map(IndexMap<String, Value>),
}

impl Value {
    /// True for `Value::Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Human-readable rendering used by the REPL's table/CSV output.
    ///
    /// `Null` renders as an empty string; `List` joins each element's own
    /// `display()` with `", "`.
    pub fn display(&self) -> String {
        match self {
            Value::Null => String::new(),
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Str(s) => s.clone(),
            Value::List(items) => items
                .iter()
                .map(Value::display)
                .collect::<Vec<_>>()
                .join(", "),
            Value::Map(_) => compact_value(self),
        }
    }

    /// The variant name, used by `.describe` to report which `Value` kinds a
    /// field has taken on (e.g. `Str`, or both `Int` and `Str` for a
    /// mixed-type field).
    pub fn variant_name(&self) -> &'static str {
        match self {
            Value::Null => "Null",
            Value::Bool(_) => "Bool",
            Value::Int(_) => "Int",
            Value::Float(_) => "Float",
            Value::Str(_) => "Str",
            Value::List(_) => "List",
            Value::Map(_) => "Map",
        }
    }

    /// Coerces this value to `f64` for numeric comparisons, if possible.
    ///
    /// `Int`/`Float` convert directly; `Str` is parsed (after trimming
    /// whitespace) as a float. Everything else — including `Bool`, `Null`,
    /// and `List` — yields `None`.
    pub fn as_number(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Str(s) => s.trim().parse::<f64>().ok(),
            Value::Bool(_) | Value::Null | Value::List(_) | Value::Map(_) => None,
        }
    }

    /// Canonical string form used for lexicographic comparison.
    pub fn to_cmp_string(&self) -> String {
        self.display()
    }
}

/// Compact, deterministic string for a `Value` used when a `Map` (or a map
/// nested in one) is rendered flat (table/CSV): lists render WITH brackets,
/// maps with braces, map keys sorted. This is intentionally different from
/// `Value::display(List)` (bracket-less) — see spec §9.
fn compact_value(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Str(s) => s.clone(),
        Value::List(items) => {
            let rendered: Vec<_> = items.iter().map(compact_value).collect();
            format!("[{}]", rendered.join(", "))
        }
        Value::Map(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let rendered: Vec<_> = entries
                .iter()
                .map(|(k, v)| format!("{k}: {}", compact_value(v)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
    }
}

/// Total-ish ordering used by `ORDER BY`/`MIN`/`MAX`.
///
/// Values that both coerce to a number compare numerically; otherwise they
/// compare lexicographically on `to_cmp_string()`. Comparing `Null` against
/// anything (including another `Null`) returns `None` — callers are
/// responsible for placing NULLs last.
pub fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
    if a.is_null() || b.is_null() {
        return None;
    }
    match (a.as_number(), b.as_number()) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => Some(a.to_cmp_string().cmp(&b.to_cmp_string())),
    }
}

/// A `file.*` pseudo-column: a property of the source file itself rather
/// than a frontmatter field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAttr {
    Name,
    Path,
    Folder,
    Ext,
}

/// One queryable row: a Markdown file's YAML frontmatter fields, plus its
/// `file.*` pseudo-columns resolved relative to the scan root.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    fields: IndexMap<String, Value>,
    name: String,
    path: String,
    folder: String,
    ext: String,
}

impl Record {
    /// Builds a record for the frontmatter `fields` found in the file at
    /// `path`, resolving its `file.*` attributes relative to `root`.
    ///
    /// `path` is stored relative to `root` (via `strip_prefix`, falling back
    /// to the full path when `path` isn't under `root`); `folder` is the
    /// parent of that relative path, or an empty string when there is none.
    /// Path separators are normalized to `/` so output is stable across
    /// platforms.
    pub fn new(root: &Path, path: &Path, fields: IndexMap<String, Value>) -> Self {
        let relative = path.strip_prefix(root).unwrap_or(path);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        let folder = relative.parent().map(join_components).unwrap_or_default();

        Record {
            fields,
            name,
            path: join_components(relative),
            folder,
            ext,
        }
    }

    /// The value at `path` (segment 0 = frontmatter field, each next segment
    /// indexes into a `Value::Map`). Missing key or non-map intermediate →
    /// `Null`. A single-segment path is a plain top-level field lookup.
    pub fn field(&self, path: &[String]) -> Value {
        let Some((head, rest)) = path.split_first() else {
            return Value::Null;
        };
        let mut cur = self.fields.get(head).cloned().unwrap_or(Value::Null);
        for seg in rest {
            cur = match cur {
                Value::Map(m) => m.get(seg).cloned().unwrap_or(Value::Null),
                _ => return Value::Null,
            };
        }
        cur
    }

    /// Resolves a `file.*` pseudo-column to its string value.
    pub fn file_attr(&self, attr: FileAttr) -> Value {
        let s = match attr {
            FileAttr::Name => &self.name,
            FileAttr::Path => &self.path,
            FileAttr::Folder => &self.folder,
            FileAttr::Ext => &self.ext,
        };
        Value::Str(s.clone())
    }

    /// The frontmatter field names for this record.
    ///
    /// Order follows the underlying `IndexMap`, i.e. the order `gray_matter`'s
    /// YAML engine handed back the keys (effectively unordered — not
    /// necessarily source order). Callers that need a deterministic column
    /// order — `SELECT *` and `.schema` — sort the union themselves.
    pub fn field_names(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(String::as_str)
    }
}

/// Joins a path's components with `/`, independent of the host platform's
/// native separator.
fn join_components(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn display_null_is_empty() {
        assert_eq!(Value::Null.display(), "");
    }
    #[test]
    fn display_list_is_comma_joined() {
        let v = Value::List(vec![Value::Str("a".into()), Value::Int(2)]);
        assert_eq!(v.display(), "a, 2");
    }
    #[test]
    fn variant_name_names_each_kind() {
        assert_eq!(Value::Null.variant_name(), "Null");
        assert_eq!(Value::Bool(true).variant_name(), "Bool");
        assert_eq!(Value::Int(1).variant_name(), "Int");
        assert_eq!(Value::Float(1.0).variant_name(), "Float");
        assert_eq!(Value::Str("x".into()).variant_name(), "Str");
        assert_eq!(Value::List(vec![]).variant_name(), "List");
    }
    #[test]
    fn display_scalars() {
        assert_eq!(Value::Str("x".into()).display(), "x");
        assert_eq!(Value::Int(10).display(), "10");
        assert_eq!(Value::Bool(true).display(), "true");
        assert_eq!(Value::Float(1.5).display(), "1.5");
    }
    #[test]
    fn as_number_coerces_numeric_strings() {
        assert_eq!(Value::Int(3).as_number(), Some(3.0));
        assert_eq!(Value::Str("3".into()).as_number(), Some(3.0));
        assert_eq!(Value::Str("x".into()).as_number(), None);
        assert_eq!(Value::Null.as_number(), None);
    }
    #[test]
    fn compare_numbers_numerically() {
        assert_eq!(
            compare_values(&Value::Int(2), &Value::Int(10)),
            Some(Ordering::Less)
        );
        // numeric string vs int compares numerically
        assert_eq!(
            compare_values(&Value::Str("2".into()), &Value::Int(10)),
            Some(Ordering::Less)
        );
    }
    #[test]
    fn compare_strings_lexicographically() {
        assert_eq!(
            compare_values(&Value::Str("a".into()), &Value::Str("b".into())),
            Some(Ordering::Less)
        );
    }
    #[test]
    fn compare_null_is_none() {
        assert_eq!(compare_values(&Value::Null, &Value::Int(1)), None);
        assert_eq!(compare_values(&Value::Int(1), &Value::Null), None);
    }
    #[test]
    fn map_display_uses_compact_value_form() {
        // keys sorted; a nested LIST inside a map renders WITH brackets, per
        // `compact_value` (NOT `Value::display`'s bracket-less list).
        let mut inner = IndexMap::new();
        inner.insert("low".to_string(), Value::Int(5));
        inner.insert("high".to_string(), Value::Int(10));
        inner.insert(
            "tags".to_string(),
            Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
        );
        let v = Value::Map(inner);
        assert_eq!(v.display(), "{high: 10, low: 5, tags: [a, b]}");
    }
    #[test]
    fn map_variant_name_and_as_number() {
        let v = Value::Map(IndexMap::new());
        assert_eq!(v.variant_name(), "Map");
        assert_eq!(v.as_number(), None);
    }
    #[test]
    fn nested_map_display_is_recursive() {
        let mut inner = IndexMap::new();
        inner.insert("x".to_string(), Value::Int(1));
        let mut outer = IndexMap::new();
        outer.insert("a".to_string(), Value::Map(inner));
        assert_eq!(Value::Map(outer).display(), "{a: {x: 1}}");
    }
}

#[cfg(test)]
mod record_tests {
    use super::*;
    use indexmap::IndexMap;
    use std::path::Path;

    fn rec() -> Record {
        let mut f = IndexMap::new();
        f.insert("status".to_string(), Value::Str("draft".into()));
        Record::new(
            Path::new("samples"),
            Path::new("samples/plans/DCP-459.md"),
            f,
        )
    }
    #[test]
    fn file_attrs_relative_to_root() {
        let r = rec();
        assert_eq!(r.file_attr(FileAttr::Name), Value::Str("DCP-459.md".into()));
        assert_eq!(
            r.file_attr(FileAttr::Path),
            Value::Str("plans/DCP-459.md".into())
        );
        assert_eq!(r.file_attr(FileAttr::Folder), Value::Str("plans".into()));
        assert_eq!(r.file_attr(FileAttr::Ext), Value::Str("md".into()));
    }
    #[test]
    fn field_present_and_missing() {
        let r = rec();
        assert_eq!(r.field(&["status".into()]), Value::Str("draft".into()));
        assert_eq!(r.field(&["nope".into()]), Value::Null);
    }
    #[test]
    fn field_walks_dotted_path_into_map() {
        let mut inner = IndexMap::new();
        inner.insert("low".to_string(), Value::Int(5));
        let mut f = IndexMap::new();
        f.insert("estimate".to_string(), Value::Map(inner));
        let r = Record::new(Path::new("v"), Path::new("v/a.md"), f);
        assert_eq!(r.field(&["estimate".into(), "low".into()]), Value::Int(5));
        // missing sub-key -> Null
        assert_eq!(r.field(&["estimate".into(), "nope".into()]), Value::Null);
        // non-map intermediate -> Null
        assert_eq!(
            r.field(&["estimate".into(), "low".into(), "x".into()]),
            Value::Null
        );
        // single segment == today's behavior
        assert_eq!(r.field(&["estimate".into()]).variant_name(), "Map");
    }
    #[test]
    fn field_names_lists_keys() {
        let r = rec();
        assert_eq!(r.field_names().collect::<Vec<_>>(), vec!["status"]);
    }
}
