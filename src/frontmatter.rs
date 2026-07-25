//! Extracts YAML frontmatter from a Markdown document and converts it into
//! the query engine's dynamic [`Value`] map.

use gray_matter::engine::YAML;
use gray_matter::{Matter, Pod};
use indexmap::IndexMap;

use crate::model::Value;

/// The result of pulling YAML frontmatter out of a Markdown document.
#[derive(Debug, Clone, PartialEq)]
pub enum Extract {
    /// No `---` frontmatter fence was found (or the fence was empty).
    None,
    /// A fence was found but its contents didn't yield a field map — either
    /// the YAML failed to parse, or it parsed to something other than a
    /// top-level mapping. The message is human-readable.
    Invalid(String),
    /// A fence was found and parsed into field name/value pairs, plus the
    /// word count of the body that follows the fence.
    Fields {
        /// Order is whatever gray_matter's YAML engine hands back: its
        /// `Pod::Hash` is a plain `HashMap`, so this is *not* guaranteed to
        /// match the order fields appeared in the source document.
        fields: IndexMap<String, Value>,
        /// The number of whitespace-separated words in the document's body
        /// (everything after the closing `---` fence), per
        /// `str::split_whitespace`.
        word_count: usize,
    },
}

/// Pulls the leading `---`-fenced YAML block out of `content` and converts
/// it into a field map, alongside the body's word count.
///
/// Returns [`Extract::None`] when there is no fence, [`Extract::Invalid`]
/// when a fence exists but its contents aren't a valid YAML mapping, and
/// [`Extract::Fields`] otherwise.
pub fn extract(content: &str) -> Extract {
    let parsed = match Matter::<YAML>::new().parse::<Pod>(content) {
        Ok(parsed) => parsed,
        Err(err) => return Extract::Invalid(err.to_string()),
    };
    let Some(pod) = parsed.data else {
        return Extract::None;
    };
    let Pod::Hash(hash) = pod else {
        return Extract::Invalid("frontmatter is not a YAML mapping".to_string());
    };

    let fields = hash
        .into_iter()
        .map(|(key, value)| (key, pod_to_value(value)))
        .collect();
    let word_count = parsed.content.split_whitespace().count();
    Extract::Fields { fields, word_count }
}

/// Converts gray_matter's dynamic `Pod` into our `Value`.
///
/// Scalars map directly; arrays recurse into `Value::List`; a nested mapping
/// recurses into `Value::Map`, preserving its structure rather than
/// collapsing it to a string (its compact string form is available on demand
/// via [`Value::display`]).
fn pod_to_value(pod: Pod) -> Value {
    match pod {
        Pod::Null => Value::Null,
        Pod::String(s) => detect_scalar(&s),
        Pod::Integer(i) => Value::Int(i),
        Pod::Float(f) => Value::Float(f),
        Pod::Boolean(b) => Value::Bool(b),
        Pod::Array(items) => Value::List(items.into_iter().map(pod_to_value).collect()),
        Pod::Hash(map) => Value::Map(map.into_iter().map(|(k, v)| (k, pod_to_value(v))).collect()),
    }
}

/// A frontmatter scalar string becomes a [`Value::Date`] (strict `%Y-%m-%d`)
/// or [`Value::DateTime`] (strict RFC3339); anything else stays a
/// [`Value::Str`]. Strict: chrono's own parse must accept the whole string,
/// so partial forms (`2026`, `2026-07`) and invalid dates (bad month/day)
/// fall through to `Str`.
///
/// `%Y-%m-%d` doesn't require zero-padded month/day — `2026-7-4` parses as a
/// valid date and is deliberately accepted as `Value::Date`: it's
/// unambiguous, and rejecting it would need an extra regex pre-check for no
/// real benefit (see `non_zero_padded_date_is_still_a_date` below).
fn detect_scalar(s: &str) -> Value {
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Value::Date(d);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Value::DateTime(dt.with_timezone(&chrono::Utc));
    }
    Value::Str(s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;

    #[test]
    fn scalars_parse_to_fields() {
        let c = "---\njira: DCP-459\nstatus: draft\n---\n# body\n";
        match extract(c) {
            Extract::Fields { fields: m, .. } => {
                assert_eq!(m.get("jira"), Some(&Value::Str("DCP-459".into())));
                assert_eq!(m.get("status"), Some(&Value::Str("draft".into())));
            }
            other => panic!("expected Fields, got {other:?}"),
        }
    }
    #[test]
    fn no_fence_is_none() {
        assert!(matches!(extract("# just a heading\n"), Extract::None));
    }
    #[test]
    fn empty_string_is_none() {
        assert!(matches!(extract(""), Extract::None));
    }
    #[test]
    fn invalid_yaml_is_invalid() {
        let c = "---\nkey: : : broken\n  bad indent\n---\n";
        assert!(matches!(extract(c), Extract::Invalid(_)));
    }
    #[test]
    fn list_value_becomes_list() {
        let c = "---\ntags:\n  - a\n  - b\n---\n";
        match extract(c) {
            Extract::Fields { fields: m, .. } => assert_eq!(
                m.get("tags"),
                Some(&Value::List(vec![
                    Value::Str("a".into()),
                    Value::Str("b".into())
                ]))
            ),
            other => panic!("expected Fields, got {other:?}"),
        }
    }
    // Characterization test: pin how our gray_matter version parses a leading-zero
    // scalar. RUN THIS, observe the actual Value, then lock the assertion to it and
    // leave a comment stating the observed behavior (spec §8.3). Both `Int(10)` and
    // `Str("010")` are legitimate observed outcomes depending on the YAML engine.
    #[test]
    fn leading_zero_characterization() {
        let c = "---\nprd: 010\n---\n";
        let Extract::Fields { fields: m, .. } = extract(c) else {
            panic!("expected Fields")
        };
        let got = m.get("prd").cloned().unwrap();
        assert_eq!(got, Value::Int(10)); // gray_matter (yaml-rust2) parses unquoted 010 as decimal int 10, not octal
    }
    // Definitive: a *quoted* leading-zero stays a string (this is the invariant the
    // docs promise — spec §8.3).
    #[test]
    fn quoted_leading_zero_is_string() {
        let c = "---\nprd: \"010\"\n---\n";
        let Extract::Fields { fields: m, .. } = extract(c) else {
            panic!("expected Fields")
        };
        assert_eq!(m.get("prd"), Some(&Value::Str("010".into())));
    }
    #[test]
    fn strict_iso_strings_become_dates_others_stay_strings() {
        use chrono::NaiveDate;
        assert_eq!(
            detect_scalar("2026-07-24"),
            Value::Date(NaiveDate::from_ymd_opt(2026, 7, 24).unwrap())
        );
        assert!(matches!(
            detect_scalar("2026-07-24T10:00:00Z"),
            Value::DateTime(_)
        ));
        // non-dates stay strings/ints — I5
        for s in [
            "2026",
            "2026-07",
            "1.2.3",
            "v1",
            "draft",
            "2026-13-01",
            "2026-07-99",
        ] {
            assert_eq!(
                detect_scalar(s),
                Value::Str(s.to_string()),
                "{s} must stay a string"
            );
        }
    }

    // Decision (task 2, W57): non-zero-padded month/day is unambiguous, so we
    // accept it as a Date rather than adding a regex pre-check to force
    // zero-padding. Pinned here so a future change to detect_scalar can't
    // silently flip this.
    #[test]
    fn non_zero_padded_date_is_still_a_date() {
        use chrono::NaiveDate;
        assert_eq!(
            detect_scalar("2026-7-4"),
            Value::Date(NaiveDate::from_ymd_opt(2026, 7, 4).unwrap())
        );
    }

    // Task 6 (W56): the body's word count, split on whitespace, is returned
    // alongside the fields rather than silently discarded.
    #[test]
    fn word_count_counts_the_body_after_the_fence() {
        let c = "---\nstatus: draft\n---\none two three four five\n";
        let Extract::Fields { word_count, .. } = extract(c) else {
            panic!("expected Fields")
        };
        assert_eq!(word_count, 5);
    }

    #[test]
    fn word_count_is_zero_for_an_empty_body() {
        let c = "---\nstatus: draft\n---\n";
        let Extract::Fields { word_count, .. } = extract(c) else {
            panic!("expected Fields")
        };
        assert_eq!(word_count, 0);
    }

    #[test]
    fn nested_mapping_becomes_value_map() {
        let c = "---\nestimate:\n  low: 5\n  high: 10\n---\n";
        let Extract::Fields { fields: m, .. } = extract(c) else {
            panic!("expected Fields")
        };
        let mut expected = IndexMap::new();
        expected.insert("low".to_string(), Value::Int(5));
        expected.insert("high".to_string(), Value::Int(10));
        // Compare structurally regardless of key order (Pod::Hash is unordered).
        let Some(Value::Map(got)) = m.get("estimate") else {
            panic!("expected Map")
        };
        assert_eq!(got.get("low"), expected.get("low"));
        assert_eq!(got.get("high"), expected.get("high"));
    }
}
