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
/// Scalars map directly; arrays recurse into `Value::List`. A nested
/// mapping has no `Value` variant of its own, so it collapses to its
/// compact string form via [`compact_pod`].
fn pod_to_value(pod: Pod) -> Value {
    match pod {
        Pod::Null => Value::Null,
        Pod::String(s) => Value::Str(s),
        Pod::Integer(i) => Value::Int(i),
        Pod::Float(f) => Value::Float(f),
        Pod::Boolean(b) => Value::Bool(b),
        Pod::Array(items) => Value::List(items.into_iter().map(pod_to_value).collect()),
        Pod::Hash(_) => Value::Str(compact_pod(&pod)),
    }
}

/// Renders a `Pod` as a compact, deterministic string — used for values
/// that are themselves nested YAML mappings, which `Value` can't represent.
/// Hash keys are sorted before rendering, since gray_matter stores them in
/// a plain `HashMap` whose natural iteration order isn't stable.
fn compact_pod(pod: &Pod) -> String {
    match pod {
        Pod::Null => "null".to_string(),
        Pod::String(s) => s.clone(),
        Pod::Integer(i) => i.to_string(),
        Pod::Float(f) => f.to_string(),
        Pod::Boolean(b) => b.to_string(),
        Pod::Array(items) => {
            let rendered: Vec<_> = items.iter().map(compact_pod).collect();
            format!("[{}]", rendered.join(", "))
        }
        Pod::Hash(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            let rendered: Vec<_> = entries
                .iter()
                .map(|(key, value)| format!("{key}: {}", compact_pod(value)))
                .collect();
            format!("{{{}}}", rendered.join(", "))
        }
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
}
