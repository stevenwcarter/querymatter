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
    /// A fence was found and parsed into field name/value pairs.
    ///
    /// Order is whatever gray_matter's YAML engine hands back: its
    /// `Pod::Hash` is a plain `HashMap`, so this is *not* guaranteed to
    /// match the order fields appeared in the source document.
    Fields(IndexMap<String, Value>),
}

/// Pulls the leading `---`-fenced YAML block out of `content` and converts
/// it into a field map.
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
    Extract::Fields(fields)
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
        Pod::String(s) => Value::Str(s),
        Pod::Integer(i) => Value::Int(i),
        Pod::Float(f) => Value::Float(f),
        Pod::Boolean(b) => Value::Bool(b),
        Pod::Array(items) => Value::List(items.into_iter().map(pod_to_value).collect()),
        Pod::Hash(map) => Value::Map(map.into_iter().map(|(k, v)| (k, pod_to_value(v))).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;

    #[test]
    fn scalars_parse_to_fields() {
        let c = "---\njira: DCP-459\nstatus: draft\n---\n# body\n";
        match extract(c) {
            Extract::Fields(m) => {
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
            Extract::Fields(m) => assert_eq!(
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
        let Extract::Fields(m) = extract(c) else {
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
        let Extract::Fields(m) = extract(c) else {
            panic!("expected Fields")
        };
        assert_eq!(m.get("prd"), Some(&Value::Str("010".into())));
    }
    #[test]
    fn nested_mapping_becomes_value_map() {
        let c = "---\nestimate:\n  low: 5\n  high: 10\n---\n";
        let Extract::Fields(m) = extract(c) else {
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
