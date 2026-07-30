use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use indexmap::IndexMap;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::paths::{FilePath, VaultRoot};

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
    /// A calendar date with no time component (e.g. from a `YYYY-MM-DD`
    /// frontmatter field once ingest auto-detects it — not yet produced by
    /// any code path as of this variant's introduction).
    ///
    /// Declared after `List`/`Map` (not alongside the other scalars) so this
    /// serde-derive enum's variant indices — which `bincode` uses as the
    /// on-disk tag for the `.querymatter` cache — don't shift `List`/`Map`'s
    /// existing discriminants; see `bincode_tags_are_stable_for_list_and_map`.
    Date(NaiveDate),
    /// A UTC instant (e.g. from an RFC3339 frontmatter field once ingest
    /// auto-detects it — not yet produced by any code path as of this
    /// variant's introduction). Kept after `Date` for the same
    /// discriminant-stability reason.
    DateTime(DateTime<Utc>),
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
            Value::Date(d) => d.format("%Y-%m-%d").to_string(),
            Value::DateTime(dt) => dt.to_rfc3339_opts(SecondsFormat::Secs, true),
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
            Value::Date(_) => "Date",
            Value::DateTime(_) => "DateTime",
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
            Value::Bool(_)
            | Value::Null
            | Value::Date(_)
            | Value::DateTime(_)
            | Value::List(_)
            | Value::Map(_) => None,
        }
    }

    /// Canonical string form used for lexicographic comparison.
    pub fn to_cmp_string(&self) -> String {
        self.display()
    }
}

/// The canonical "do these two cells count as the same value" key — the ONE
/// encoding `GROUP BY`, `SELECT DISTINCT`, and `count(distinct col)` all hash
/// on, so the three can no longer drift apart on a mixed-type column.
///
/// [`Value`] itself can't be the key: it holds an `f64`, so it has neither
/// `Eq` nor `Hash`. This is its structural, variant-tagged stand-in — built by
/// the one [`From<&Value>`] below — under which `Int(1)` never collides with
/// `Str("1")` and `Null` never collides with `Str("")`, unlike the
/// [`Value::to_cmp_string`] display form (which renders both pairs
/// identically, and which stays behind for its real job: ordering).
///
/// Two deliberate departures from a plain structural [`Value`] `PartialEq`,
/// both inherited from the encodings this type replaced:
///
/// - `-0.0` is normalized to `0.0`, matching `f64`'s own `0.0 == -0.0`.
/// - Every `NaN` keys equal to every other `NaN`, where `f64::PartialEq` says
///   a `NaN` differs even from itself. Reflexivity is what `Eq`/`Hash` require
///   of a hash key, so a `NaN` cell groups with itself rather than spawning a
///   fresh bucket per row.
///
/// Keys are process-local — never serialized, so their encoding is free to
/// change.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ValueKey {
    Null,
    Bool(bool),
    Int(i64),
    /// `f64::to_bits` after normalizing `-0.0` to `0.0`.
    Float(u64),
    Str(String),
    /// Order-sensitive, matching `Vec`'s equality (so `[a, b]` and `["a, b"]`
    /// are different keys, unlike their shared `", "`-joined display form).
    List(Vec<ValueKey>),
    /// Key-sorted, so the two insertion orders of a structurally equal map
    /// produce one key (matching `IndexMap`'s order-insensitive equality).
    Map(Vec<(String, ValueKey)>),
    Date(NaiveDate),
    DateTime(DateTime<Utc>),
}

impl From<&Value> for ValueKey {
    fn from(value: &Value) -> Self {
        match value {
            Value::Null => ValueKey::Null,
            Value::Bool(b) => ValueKey::Bool(*b),
            Value::Int(i) => ValueKey::Int(*i),
            Value::Float(f) => {
                // Fold both of `f64`'s equality quirks away before hashing the
                // bits: `-0.0` into `0.0` (which `f64` calls equal), and every
                // `NaN` payload into one canonical `NaN` (which `f64` calls
                // unequal, including to itself).
                let normalized = if f.is_nan() {
                    f64::NAN
                } else if *f == 0.0 {
                    0.0
                } else {
                    *f
                };
                ValueKey::Float(normalized.to_bits())
            }
            Value::Str(s) => ValueKey::Str(s.clone()),
            Value::List(items) => ValueKey::List(items.iter().map(ValueKey::from).collect()),
            Value::Map(map) => {
                let mut entries: Vec<(String, ValueKey)> = map
                    .iter()
                    .map(|(name, v)| (name.clone(), ValueKey::from(v)))
                    .collect();
                entries.sort_by(|a, b| a.0.cmp(&b.0));
                ValueKey::Map(entries)
            }
            Value::Date(d) => ValueKey::Date(*d),
            Value::DateTime(dt) => ValueKey::DateTime(*dt),
        }
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
        Value::Date(_) | Value::DateTime(_) => v.display(),
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
/// Values that both coerce to a number compare numerically; a `Date`/
/// `DateTime` on either side is handled first by [`compare_dates`] (see its
/// doc for exactly which pairings compare natively vs. via ISO text);
/// otherwise they compare lexicographically on `to_cmp_string()`. Comparing
/// `Null` against anything (including another `Null`) returns `None` —
/// callers are responsible for placing NULLs last.
pub fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
    if a.is_null() || b.is_null() {
        return None;
    }
    if let Some(ord) = compare_dates(a, b) {
        return Some(ord);
    }
    match (a.as_number(), b.as_number()) {
        (Some(x), Some(y)) => x.partial_cmp(&y),
        _ => Some(a.to_cmp_string().cmp(&b.to_cmp_string())),
    }
}

/// Chronological comparison for a `Date`/`DateTime` on either side.
///
/// `Date` vs `Date` compares directly via chrono's own `Ord` — `NaiveDate` has
/// no sub-second (or even sub-day) component, so its native `Ord` already
/// agrees with a `%Y-%m-%d` text compare. Every other pairing that involves a
/// `Date`/`DateTime` — `DateTime` vs `DateTime`, a mixed `Date`/`DateTime`
/// pair, or either against a `Str` — compares `to_cmp_string()` of both (ISO
/// text, rendered at whole-second granularity — see `Value::display`)
/// instead. This is required, not just convenient, for two reasons:
///
/// - Comparing a mixed pair via the `DateTime`'s date part (`.date_naive()`)
///   is not a strict-weak ordering — a `Date` could compare `Equal` to two
///   different `DateTime`s (same day, different times) that are themselves
///   unequal, violating transitivity.
/// - Comparing `DateTime` vs `DateTime` at full sub-second precision (instead
///   of via the same second-granular text every other arm uses) breaks
///   transitivity the same way: two `DateTime`s differing only in
///   sub-seconds would compare non-equal to each other yet each compare
///   `Equal` to a `Str`/`Date` holding their shared seconds-granular text.
///
/// Both failure modes are undefined behavior for the `sort_by` that backs
/// `ORDER BY` (it can panic on a non-transitive comparator). ISO text at
/// second granularity stays a total order: a same-day `Date`'s string
/// (`"2026-07-24"`) is a strict prefix of the `DateTime`'s
/// (`"2026-07-24T…"`), so it always sorts before it. This also sorts
/// identically to a chronological compare for well-formed ISO strings, and
/// stays defined (never panics) for anything else (e.g. `"draft"`). Returns
/// `None` when neither side is a `Date`/`DateTime`, so the caller falls
/// through to the existing numeric/string rules unchanged.
fn compare_dates(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Date(x), Value::Date(y)) => Some(x.cmp(y)),
        (Value::DateTime(_), Value::DateTime(_))
        | (
            Value::Date(_) | Value::DateTime(_),
            Value::Str(_) | Value::Date(_) | Value::DateTime(_),
        )
        | (Value::Str(_), Value::Date(_) | Value::DateTime(_)) => {
            Some(a.to_cmp_string().cmp(&b.to_cmp_string()))
        }
        _ => None,
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
    Mtime,
    Size,
    /// The Markdown body's word count (see [`crate::frontmatter::extract`]).
    WordCount,
    /// The Markdown body text itself (the content after the frontmatter
    /// fence), read from disk lazily at query-eval time — never cached (see
    /// [`crate::query::exec::read_body`]). This is the one `FileAttr` this
    /// module's own pure [`Record::file_attr`] cannot resolve.
    Body,
}

impl FileAttr {
    /// Every pseudo-column, in the display order `.schema`/`.describe` use.
    pub const ALL: [FileAttr; 8] = [
        FileAttr::Name,
        FileAttr::Path,
        FileAttr::Folder,
        FileAttr::Ext,
        FileAttr::Mtime,
        FileAttr::Size,
        FileAttr::WordCount,
        FileAttr::Body,
    ];

    /// The dotted, user-facing SQL spelling (`file.name`, …). These strings
    /// are pinned by the sample_queries snapshot — do not change them.
    pub fn label(self) -> &'static str {
        match self {
            FileAttr::Name => "file.name",
            FileAttr::Path => "file.path",
            FileAttr::Folder => "file.folder",
            FileAttr::Ext => "file.ext",
            FileAttr::Mtime => "file.mtime",
            FileAttr::Size => "file.size",
            FileAttr::WordCount => "file.word_count",
            FileAttr::Body => "file.body",
        }
    }

    /// Parses the bare, already-lowercased attribute half (`name`,
    /// `word_count`, …) that `parse::lower_compound` splits off — the
    /// counterpart spelling to [`Self::label`]'s dotted form. Two explicit
    /// fns rather than one `FromStr`, because the two spellings differ.
    pub fn from_attr_name(name: &str) -> Option<FileAttr> {
        match name {
            "name" => Some(FileAttr::Name),
            "path" => Some(FileAttr::Path),
            "folder" => Some(FileAttr::Folder),
            "ext" => Some(FileAttr::Ext),
            "mtime" => Some(FileAttr::Mtime),
            "size" => Some(FileAttr::Size),
            "word_count" => Some(FileAttr::WordCount),
            "body" => Some(FileAttr::Body),
            _ => None,
        }
    }

    /// The `.describe` type column for this pseudo-column.
    pub fn value_kind(self) -> &'static str {
        match self {
            FileAttr::Size | FileAttr::WordCount => "Int",
            FileAttr::Name
            | FileAttr::Path
            | FileAttr::Folder
            | FileAttr::Ext
            | FileAttr::Mtime
            | FileAttr::Body => "Str",
        }
    }
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
    mtime: SystemTime,
    size: u64,
    word_count: usize,
    /// The file's absolute path, for [`FileAttr::Body`]'s eval-time disk
    /// read (see [`crate::query::exec::read_body`]) — every other `file.*`
    /// attribute is resolved relative to `root` instead (see `path`/`folder`
    /// below), but a real read needs the whole path. Both live producers
    /// ([`crate::store::scan_root`]) and cache-backed ones
    /// ([`crate::cache::records_from`]) always pass an already-canonicalized
    /// `path` into [`Record::new`], so this is absolute in practice; a
    /// hand-built test `Record` that passes a relative `path` just gets that
    /// path back (harmless, since no test constructs a `Record` this way and
    /// then reads its body).
    abs_path: PathBuf,
}

impl Record {
    /// Builds a record for the frontmatter `fields` found in the file at
    /// `path`, resolving its `file.*` attributes relative to `root`.
    ///
    /// `path` is stored relative to `root` (via `strip_prefix`, falling back
    /// to the full path when `path` isn't under `root`); `folder` is the
    /// parent of that relative path, or an empty string when there is none.
    /// Path separators are normalized to `/` so output is stable across
    /// platforms. `mtime`/`size` are the file's on-disk stat, already read
    /// by the caller for cache-freshness purposes — this is zero extra I/O,
    /// not a fresh stat. `word_count` is the body's word count (see
    /// [`crate::frontmatter::extract`]), likewise already computed by the
    /// caller.
    pub fn new(
        root: &VaultRoot,
        path: &FilePath,
        fields: IndexMap<String, Value>,
        mtime: SystemTime,
        size: u64,
        word_count: usize,
    ) -> Self {
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
            mtime,
            size,
            word_count,
            abs_path: path.to_path_buf(),
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

    /// Resolves a `file.*` pseudo-column to its value: `Name`/`Path`/`Folder`/
    /// `Ext` are strings, `Mtime` is an RFC3339 UTC string, and `Size`/
    /// `WordCount` are integer counts.
    ///
    /// `Body` always resolves to `Value::Null` here — this function is pure
    /// (`&self -> Value`, no I/O, no disk-access gate), so it cannot perform
    /// the real read `file.body` needs. The query executor special-cases
    /// `ColRef::File(FileAttr::Body)` *before* it would ever reach this
    /// match, resolving it via [`crate::query::exec::read_body`] against
    /// [`Record::abs_path`] instead (see `query::exec::resolve_col`). This
    /// arm exists only so the match stays exhaustive over [`FileAttr`];
    /// reaching it for a real `file.body` query would be a caller bug.
    pub fn file_attr(&self, attr: FileAttr) -> Value {
        match attr {
            FileAttr::Name => Value::Str(self.name.clone()),
            FileAttr::Path => Value::Str(self.path.clone()),
            FileAttr::Folder => Value::Str(self.folder.clone()),
            FileAttr::Ext => Value::Str(self.ext.clone()),
            FileAttr::Mtime => Value::Str(system_time_to_iso(self.mtime)),
            FileAttr::Size => Value::Int(self.size as i64),
            FileAttr::WordCount => Value::Int(self.word_count as i64),
            FileAttr::Body => Value::Null,
        }
    }

    /// This file's absolute path, for `file.body`'s eval-time disk read (see
    /// [`crate::query::exec::read_body`]). Every other `file.*` attribute is
    /// resolved relative to the scan root instead; only a real read needs
    /// the whole path.
    pub fn abs_path(&self) -> &Path {
        &self.abs_path
    }

    /// The cached Markdown body word count, as a raw `usize` rather than a
    /// `Value` — used by callers reconstructing a [`crate::cache::CachedFile`]
    /// from an in-memory `Record` (e.g. [`crate::store`]'s no-on-disk-cache
    /// fallback), which need the count itself, not its query-facing form.
    pub fn word_count(&self) -> usize {
        self.word_count
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

/// An `mtime` as an RFC3339 UTC string (`2021-01-01T00:00:00Z`), seconds
/// precision. Pre-epoch times format to their real negative timestamp; never
/// panics.
pub fn system_time_to_iso(t: SystemTime) -> String {
    DateTime::<Utc>::from(t).to_rfc3339_opts(SecondsFormat::Secs, true)
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
    fn value_key_is_variant_tagged_and_structural() {
        let key = |v: &Value| ValueKey::from(v);
        // The two collisions `Value::display` (and so `to_cmp_string`) has,
        // which every hashing caller relies on this key to avoid.
        assert_ne!(key(&Value::Int(1)), key(&Value::Str("1".into())));
        assert_ne!(key(&Value::Null), key(&Value::Str("".into())));
        // Lists are order-sensitive and don't flatten into their join.
        let ab = Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]);
        let ba = Value::List(vec![Value::Str("b".into()), Value::Str("a".into())]);
        let joined = Value::List(vec![Value::Str("a, b".into())]);
        assert_ne!(key(&ab), key(&ba));
        assert_ne!(key(&ab), key(&joined));
        assert_eq!(key(&ab), key(&ab.clone()));
    }
    #[test]
    fn value_key_of_map_is_insertion_order_insensitive() {
        let map = |pairs: &[(&str, i64)]| {
            let mut m = IndexMap::new();
            for (k, v) in pairs {
                m.insert((*k).to_string(), Value::Int(*v));
            }
            Value::Map(m)
        };
        // `IndexMap`'s equality ignores insertion order, so the key must too.
        assert_eq!(
            ValueKey::from(&map(&[("a", 1), ("b", 2)])),
            ValueKey::from(&map(&[("b", 2), ("a", 1)]))
        );
        assert_ne!(
            ValueKey::from(&map(&[("a", 1), ("b", 2)])),
            ValueKey::from(&map(&[("a", 2), ("b", 1)]))
        );
    }
    #[test]
    fn value_key_normalizes_float_zero_and_nan() {
        // `0.0 == -0.0` in `f64`, so they must key equal…
        assert_eq!(
            ValueKey::from(&Value::Float(0.0)),
            ValueKey::from(&Value::Float(-0.0))
        );
        // …while a `NaN`, which `f64` calls unequal even to itself, must key
        // equal to itself (`Eq`/`Hash` need reflexivity) regardless of which
        // payload or sign bit produced it — otherwise every `NaN` row would
        // open its own group.
        assert_eq!(
            ValueKey::from(&Value::Float(f64::NAN)),
            ValueKey::from(&Value::Float(-f64::NAN))
        );
        let odd_payload_nan = f64::from_bits(f64::NAN.to_bits() | 0x7);
        assert!(odd_payload_nan.is_nan());
        assert_eq!(
            ValueKey::from(&Value::Float(f64::NAN)),
            ValueKey::from(&Value::Float(odd_payload_nan))
        );
        assert_ne!(
            ValueKey::from(&Value::Float(f64::NAN)),
            ValueKey::from(&Value::Float(0.0))
        );
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
    #[test]
    fn system_time_to_iso_is_rfc3339_utc() {
        use std::time::Duration;
        // 2021-01-01T00:00:00Z == 1609459200 secs
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        assert_eq!(system_time_to_iso(t), "2021-01-01T00:00:00Z");
    }
    #[test]
    fn pre_epoch_mtime_does_not_panic() {
        use std::time::Duration;
        let t = SystemTime::UNIX_EPOCH - Duration::from_secs(60);
        let _ = system_time_to_iso(t); // must not panic
    }
    /// Pins the `Value` enum's bincode wire format: `.querymatter` caches
    /// serialize `Value` via serde derive + bincode, which tags each variant
    /// by its DECLARATION-ORDER index. `List`/`Map` must keep the
    /// discriminants (5/6) an existing on-disk cache blob was written with —
    /// inserting a new variant earlier in the enum (as `Date`/`DateTime` once
    /// were, between `Str` and `List`) silently shifts every later variant's
    /// tag, so an old cache blob holding a `List`/`Map` value would decode as
    /// the WRONG variant with no error. `Date`/`DateTime` are declared last
    /// specifically so they land on brand-new tags (7/8) instead of
    /// disturbing `List`/`Map`. This test fails loudly if a future edit
    /// reorders the enum: the leading tag byte is the first thing bincode's
    /// serde bridge writes for a serde-derived enum (one byte here since
    /// bincode's standard config varint-encodes small indices verbatim).
    #[test]
    fn bincode_tags_are_stable_for_list_and_map() {
        let list = Value::List(vec![Value::Int(1), Value::Str("a".into())]);
        let mut map = IndexMap::new();
        map.insert("k".to_string(), Value::Int(1));
        let map = Value::Map(map);

        let list_bytes = crate::cache::encode(&list);
        let map_bytes = crate::cache::encode(&map);
        assert_eq!(list_bytes[0], 5, "Value::List must keep bincode tag 5");
        assert_eq!(map_bytes[0], 6, "Value::Map must keep bincode tag 6");

        assert_eq!(crate::cache::decode::<Value>(&list_bytes), Some(list));
        assert_eq!(crate::cache::decode::<Value>(&map_bytes), Some(map));
    }
    #[test]
    fn dates_compare_by_instant_and_render_iso() {
        use chrono::NaiveDate;
        let a = Value::Date(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        let b = Value::Date(NaiveDate::from_ymd_opt(2026, 7, 24).unwrap());
        assert_eq!(compare_values(&a, &b), Some(Ordering::Less));
        assert_eq!(a.display(), "2026-01-01");
        // date vs ISO string coerces (relative-date literals compare correctly)
        assert_eq!(
            compare_values(&b, &Value::Str("2026-01-01".into())),
            Some(Ordering::Greater)
        );
        // date vs a non-date string: defined, panic-free (fallback to text compare)
        assert!(compare_values(&b, &Value::Str("draft".into())).is_some());
        // NULL still unordered
        assert_eq!(compare_values(&a, &Value::Null), None);
    }
    #[test]
    fn datetime_renders_as_rfc3339() {
        use chrono::TimeZone;
        let dt = Value::DateTime(Utc.with_ymd_and_hms(2026, 7, 24, 9, 30, 0).unwrap());
        // `display()` is what both the REPL's table/CSV output and
        // `render::to_json` (which calls `value.display()` for this variant)
        // render, so this pins the ISO text both paths depend on.
        assert_eq!(dt.display(), "2026-07-24T09:30:00Z");
    }
    #[test]
    fn datetimes_compare_chronologically() {
        use chrono::TimeZone;
        let earlier = Value::DateTime(Utc.with_ymd_and_hms(2026, 7, 24, 9, 0, 0).unwrap());
        let later = Value::DateTime(Utc.with_ymd_and_hms(2026, 7, 24, 17, 0, 0).unwrap());
        assert_eq!(compare_values(&earlier, &later), Some(Ordering::Less));
        assert_eq!(compare_values(&later, &earlier), Some(Ordering::Greater));
        assert_eq!(compare_values(&earlier, &earlier), Some(Ordering::Equal));
    }
    /// Fix 2 (W57 review): a mixed `Date`/`DateTime` pair must compare via
    /// ISO text, not the `DateTime`'s date part — the date-part compare was
    /// non-transitive (a `Date` could be `Equal` to two different same-day
    /// `DateTime`s that are themselves unequal), which is undefined behavior
    /// for the `sort_by` backing `ORDER BY`. This pins the resulting total
    /// order: a same-day `Date` sorts before its `DateTime` (ISO-text
    /// prefix), and cross-day pairs sort by calendar day as expected.
    #[test]
    fn mixed_date_and_datetime_compare_via_iso_text() {
        use chrono::{NaiveDate, TimeZone};
        let date = Value::Date(NaiveDate::from_ymd_opt(2026, 7, 24).unwrap());
        let same_day_morning = Value::DateTime(Utc.with_ymd_and_hms(2026, 7, 24, 0, 0, 1).unwrap());
        let same_day_evening =
            Value::DateTime(Utc.with_ymd_and_hms(2026, 7, 24, 23, 59, 0).unwrap());

        // "2026-07-24" is a strict text prefix of "2026-07-24T...", so the
        // bare date sorts before any same-day datetime — a total order, not
        // an `Equal` that would break transitivity.
        assert_eq!(
            compare_values(&date, &same_day_morning),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&date, &same_day_evening),
            Some(Ordering::Less)
        );
        // Transitivity check: the two same-day datetimes are themselves
        // ordered relative to each other, proving `date` isn't `Equal` to
        // both (which is exactly the bug the ISO-text fix rules out).
        assert_eq!(
            compare_values(&same_day_morning, &same_day_evening),
            Some(Ordering::Less)
        );

        // Different-day pair: still sorts by calendar day.
        let earlier_date = Value::Date(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap());
        let later_datetime = Value::DateTime(Utc.with_ymd_and_hms(2026, 7, 24, 0, 0, 0).unwrap());
        assert_eq!(
            compare_values(&earlier_date, &later_datetime),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&later_datetime, &earlier_date),
            Some(Ordering::Greater)
        );
    }
    /// Fix 3 (W57 review): the `DateTime`/`DateTime` arm must compare via the
    /// same second-granular ISO text every cross-type arm uses, not chrono's
    /// full sub-second `Ord`. Otherwise two `DateTime`s differing only in
    /// sub-seconds compare non-`Equal` to each other (arm 1) yet each compare
    /// `Equal` to a `Str` holding their shared seconds-only text (arm 2) — a
    /// non-transitive comparator (A<B, A==C, B==C), which is undefined
    /// behavior for the `sort_by` backing `ORDER BY` (e.g. via `ORDER BY
    /// COALESCE(<subsecond_field>, file.mtime)`).
    #[test]
    fn datetime_ordering_is_second_granular_and_transitive() {
        let whole_second = DateTime::parse_from_rfc3339("2026-07-24T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let sub_second = DateTime::parse_from_rfc3339("2026-07-24T00:00:00.900Z")
            .unwrap()
            .with_timezone(&Utc);
        let a = Value::DateTime(whole_second);
        let b = Value::DateTime(sub_second);
        let shared_text = Value::Str("2026-07-24T00:00:00Z".to_string());

        // Same-type arm now agrees with the cross-type arm: both truncate to
        // whole seconds, so a sub-second-only difference is `Equal`.
        assert_eq!(compare_values(&a, &b), Some(Ordering::Equal));
        // ...and consistently `Equal` to their shared seconds-text on both
        // sides — no A<B while A==C==B.
        assert_eq!(compare_values(&a, &shared_text), Some(Ordering::Equal));
        assert_eq!(compare_values(&b, &shared_text), Some(Ordering::Equal));
        assert_eq!(compare_values(&shared_text, &a), Some(Ordering::Equal));
        assert_eq!(compare_values(&shared_text, &b), Some(Ordering::Equal));
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
            &VaultRoot::new(Path::new("samples").to_path_buf()),
            &FilePath::new(Path::new("samples/plans/DCP-459.md").to_path_buf()),
            f,
            SystemTime::UNIX_EPOCH,
            0,
            0,
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
    fn file_attr_mtime_and_size() {
        use std::time::Duration;
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_609_459_200);
        let r = Record::new(
            &VaultRoot::new(Path::new("v").to_path_buf()),
            &FilePath::new(Path::new("v/a.md").to_path_buf()),
            IndexMap::new(),
            t,
            42,
            0,
        );
        assert_eq!(r.file_attr(FileAttr::Size), Value::Int(42));
        assert_eq!(
            r.file_attr(FileAttr::Mtime),
            Value::Str("2021-01-01T00:00:00Z".into())
        );
    }
    #[test]
    fn file_attr_word_count() {
        let r = Record::new(
            &VaultRoot::new(Path::new("v").to_path_buf()),
            &FilePath::new(Path::new("v/a.md").to_path_buf()),
            IndexMap::new(),
            SystemTime::UNIX_EPOCH,
            0,
            5,
        );
        assert_eq!(r.file_attr(FileAttr::WordCount), Value::Int(5));
        assert_eq!(r.word_count(), 5);
    }
    #[test]
    fn abs_path_is_the_path_record_new_was_given() {
        // `abs_path` carries whatever `path` `Record::new` was constructed
        // with verbatim (both real producers always pass an already
        // canonicalized/absolute path — see the field's doc comment).
        let r = Record::new(
            &VaultRoot::new(Path::new("/vault").to_path_buf()),
            &FilePath::new(Path::new("/vault/plans/a.md").to_path_buf()),
            IndexMap::new(),
            SystemTime::UNIX_EPOCH,
            0,
            0,
        );
        assert_eq!(r.abs_path(), Path::new("/vault/plans/a.md"));
    }
    #[test]
    fn file_attr_body_is_a_null_sentinel() {
        // `file_attr` is pure and cannot perform the real disk read
        // `file.body` needs; reaching this arm directly (rather than through
        // `query::exec::resolve_col`'s `read_body` special case) always
        // yields `Null`, never the real body text.
        let r = rec();
        assert_eq!(r.file_attr(FileAttr::Body), Value::Null);
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
        let r = Record::new(
            &VaultRoot::new(Path::new("v").to_path_buf()),
            &FilePath::new(Path::new("v/a.md").to_path_buf()),
            f,
            SystemTime::UNIX_EPOCH,
            0,
            0,
        );
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
