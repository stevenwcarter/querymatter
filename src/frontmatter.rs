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
/// when a fence exists but its contents aren't a valid YAML mapping (or nest
/// beyond [`MAX_NESTING_DEPTH`] — see its doc), and [`Extract::Fields`]
/// otherwise.
pub fn extract(content: &str) -> Extract {
    if let Err(reason) = check_nesting_depth(content) {
        return Extract::Invalid(reason);
    }
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

    // Belt and suspenders alongside `check_nesting_depth` above (B9): that
    // pre-parse check already keeps gray_matter from ever handing back a
    // `Pod` deeper than `MAX_NESTING_DEPTH`, so this `None` arm should be
    // unreachable in practice, but `pod_to_value` enforces the same cap on
    // its own terms rather than trusting that invariant to hold forever.
    let fields: Option<IndexMap<String, Value>> = hash
        .into_iter()
        .map(|(key, value)| pod_to_value(value).map(|v| (key, v)))
        .collect();
    let Some(fields) = fields else {
        return Extract::Invalid(format!(
            "frontmatter nesting exceeds {MAX_NESTING_DEPTH} levels — skipped"
        ));
    };
    let word_count = parsed.content.split_whitespace().count();
    Extract::Fields { fields, word_count }
}

/// The Markdown body after the frontmatter fence — the exact text [`extract`]
/// counts words from, exposed standalone for `file.body`'s eval-time disk
/// read ([`crate::query::exec::read_body`]), which re-parses a freshly-read
/// file fresh rather than reusing anything cached (design W56: only the word
/// count is ever persisted, never the body text itself).
///
/// `None` when `content`'s frontmatter fence, if present, isn't valid YAML or
/// nests beyond [`MAX_NESTING_DEPTH`] — the same conditions [`extract`]
/// reports as [`Extract::Invalid`]; a file with no fence at all (or an empty
/// one) still yields `Some` (gray_matter treats the whole input as body
/// content in that case, matching `extract`'s [`Extract::None`] behavior for
/// word-counting purposes).
///
/// The depth check (B9) matters here too: unlike `extract`, `body` runs
/// against a fresh disk read of a file that's already in the record store
/// (`crate::query::exec::read_body`) — its frontmatter could have been
/// edited to something hostile since the file was last scanned, so this
/// can't assume `extract` already screened the content.
pub fn body(content: &str) -> Option<String> {
    check_nesting_depth(content).ok()?;
    Matter::<YAML>::new()
        .parse::<Pod>(content)
        .ok()
        .map(|parsed| parsed.content)
}

/// Hard cap on frontmatter nesting depth (security/correctness fix B9).
///
/// Chosen far below two independently-observed overflow points: yaml-rust2
/// 0.10's own `u8` counter on `[`/`{` flow-collection nesting, which caps out
/// and cleanly errors at 255; and, empirically, its recursive-descent parser
/// itself overflowing its call stack — a raw process-aborting crash, not a
/// catchable error — somewhere between depth 900 and 950 when parsing a
/// compact block sequence (`- - - - … v`, all on one line) on this crate's
/// ~2 MiB file-scanning worker-thread stacks (`parallel::map_paths`). 128 is
/// also nowhere near any legitimate frontmatter (see [`check_nesting_depth`]
/// and [`max_nesting_depth`] for where this is enforced).
const MAX_NESTING_DEPTH: usize = 128;

/// Rejects `content` before it's ever handed to gray_matter's YAML parser,
/// when [`max_nesting_depth`]'s conservative estimate says parsing it could
/// recurse past [`MAX_NESTING_DEPTH`].
///
/// This has to run *before* `Matter::parse`, not after: past a certain
/// depth, gray_matter's YAML engine (yaml-rust2) overflows its own parser
/// stack while parsing — see [`MAX_NESTING_DEPTH`]'s doc — which aborts the
/// whole process before it would ever return an `Err` we could handle.
fn check_nesting_depth(content: &str) -> Result<(), String> {
    if max_nesting_depth(content) > MAX_NESTING_DEPTH {
        return Err(format!(
            "frontmatter nesting exceeds {MAX_NESTING_DEPTH} levels — skipped"
        ));
    }
    Ok(())
}

/// A cheap, conservative pre-parse estimate of how deeply gray_matter's YAML
/// parser would need to recurse to parse `text`, computed without invoking
/// the parser at all.
///
/// Only two constructs can nest a `Value` at all: `[`/`{` flow collections,
/// and block sequences (`- ` entries). Over-counting only rejects more
/// input, never lets a dangerous depth through, so this doesn't need to
/// track YAML's grammar exactly — it takes the running max of:
/// - the `[`/`{` bracket depth open at any point in the text (flow
///   collections can span multiple lines, so this isn't reset per line), and
/// - the number of leading `- ` block-sequence markers on any single line
///   (a *compact* nested sequence, e.g. `- - - - v`). Indentation-nested
///   block sequences (one `-` per line, each further indented) reach a
///   comparable depth only by making the file quadratically larger, which
///   `max_file_bytes` (fix B8) already bounds well before this depth would
///   matter — so this deliberately doesn't try to track indentation levels
///   across lines.
///
/// A leading `-` only counts as a sequence marker when followed by
/// whitespace or end-of-line (`- - - - v`), not when it's the leading `-` of
/// a negative number (`x: -5`) or a `---`/`...` document marker.
fn max_nesting_depth(text: &str) -> usize {
    let mut bracket_depth = 0usize;
    let mut max_depth = 0usize;
    for line in text.lines() {
        let mut rest = line.trim_start();
        let mut dash_run = 0usize;
        while let Some(after_dash) = rest.strip_prefix('-') {
            if after_dash.is_empty() || after_dash.starts_with(char::is_whitespace) {
                dash_run += 1;
                rest = after_dash.trim_start();
            } else {
                break;
            }
        }
        max_depth = max_depth.max(bracket_depth + dash_run);
        for c in line.chars() {
            match c {
                '[' | '{' => {
                    bracket_depth += 1;
                    max_depth = max_depth.max(bracket_depth);
                }
                ']' | '}' => bracket_depth = bracket_depth.saturating_sub(1),
                _ => {}
            }
        }
    }
    max_depth
}

/// Converts gray_matter's dynamic `Pod` into our `Value`.
///
/// Scalars map directly; arrays recurse into `Value::List`; a nested mapping
/// recurses into `Value::Map`, preserving its structure rather than
/// collapsing it to a string (its compact string form is available on demand
/// via [`Value::display`]).
///
/// Returns `None` once nesting passes [`MAX_NESTING_DEPTH`] (B9) rather than
/// recursing without limit — in practice unreachable, since
/// [`check_nesting_depth`] already rejects anything this deep before
/// gray_matter parses it into a `Pod` at all, but this is cheap enough to
/// enforce independently rather than resting entirely on that invariant.
fn pod_to_value(pod: Pod) -> Option<Value> {
    pod_to_value_at(pod, 0)
}

fn pod_to_value_at(pod: Pod, depth: usize) -> Option<Value> {
    if depth > MAX_NESTING_DEPTH {
        return None;
    }
    let value = match pod {
        Pod::Null => Value::Null,
        Pod::String(s) => detect_scalar(&s),
        Pod::Integer(i) => Value::Int(i),
        Pod::Float(f) => Value::Float(f),
        Pod::Boolean(b) => Value::Bool(b),
        Pod::Array(items) => Value::List(
            items
                .into_iter()
                .map(|item| pod_to_value_at(item, depth + 1))
                .collect::<Option<Vec<_>>>()?,
        ),
        Pod::Hash(map) => Value::Map(
            map.into_iter()
                .map(|(k, v)| pod_to_value_at(v, depth + 1).map(|v| (k, v)))
                .collect::<Option<IndexMap<_, _>>>()?,
        ),
    };
    Some(value)
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

    // Task 7 (W56 part 2): `body` exposes the same post-fence text
    // `word_count` was already counting, standalone — for `file.body`'s
    // eval-time re-read.
    #[test]
    fn body_returns_the_text_after_the_fence() {
        let c = "---\nstatus: draft\n---\nTODO fix this\n";
        assert_eq!(body(c).as_deref(), Some("TODO fix this"));
    }

    #[test]
    fn body_is_none_for_invalid_frontmatter_yaml() {
        let c = "---\nkey: : : broken\n  bad indent\n---\n";
        assert_eq!(body(c), None);
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

    // Task 9 (B9, correctness): pins `max_nesting_depth`'s heuristic directly,
    // independent of gray_matter, so a future edit to it can't silently widen
    // or narrow what counts as "nested" without a test noticing.
    #[test]
    fn max_nesting_depth_counts_compact_block_sequences() {
        assert_eq!(max_nesting_depth("x:\n  - - - - v\n"), 4);
    }

    #[test]
    fn max_nesting_depth_counts_flow_brackets() {
        assert_eq!(max_nesting_depth("x: [[[[v]]]]\n"), 4);
    }

    #[test]
    fn max_nesting_depth_ignores_a_negative_number() {
        assert_eq!(max_nesting_depth("x: -5\n"), 0);
    }

    #[test]
    fn max_nesting_depth_ignores_the_fence_itself() {
        assert_eq!(max_nesting_depth("---\ntitle: ok\n---\n"), 0);
    }

    #[test]
    fn max_nesting_depth_ignores_an_ordinary_list() {
        assert_eq!(max_nesting_depth("tags:\n  - a\n  - b\n"), 1);
    }

    #[test]
    fn extract_rejects_frontmatter_nested_past_the_cap() {
        let depth = MAX_NESTING_DEPTH + 1;
        let c = format!("---\nx:\n  {}v\n---\n", "- ".repeat(depth));
        assert!(
            matches!(extract(&c), Extract::Invalid(reason) if reason.contains("nesting")),
            "expected a nesting-depth Invalid"
        );
    }

    #[test]
    fn extract_accepts_frontmatter_nested_within_the_cap() {
        let depth = MAX_NESTING_DEPTH - 1;
        let c = format!("---\nx:\n  {}v\n---\n", "- ".repeat(depth));
        assert!(
            matches!(extract(&c), Extract::Fields { .. }),
            "depth {depth} is within the cap and must still load"
        );
    }

    #[test]
    fn body_is_none_for_frontmatter_nested_past_the_cap() {
        let depth = MAX_NESTING_DEPTH + 1;
        let c = format!("---\nx:\n  {}v\n---\nsome body text\n", "- ".repeat(depth));
        assert_eq!(body(&c), None);
    }

    // Direct unit test of `pod_to_value`'s own cap, built from a `Pod` tree
    // constructed by hand rather than parsed from YAML — this is the "belt
    // and suspenders" layer, so it must refuse deep input on its own terms
    // even if it were ever reached some way other than through `extract`.
    #[test]
    fn pod_to_value_rejects_a_pod_nested_past_the_cap() {
        let mut pod = Pod::Integer(1);
        for _ in 0..=MAX_NESTING_DEPTH {
            pod = Pod::Array(vec![pod]);
        }
        assert_eq!(pod_to_value(pod), None);
    }

    #[test]
    fn pod_to_value_accepts_a_pod_nested_within_the_cap() {
        let mut pod = Pod::Integer(1);
        for _ in 0..MAX_NESTING_DEPTH - 1 {
            pod = Pod::Array(vec![pod]);
        }
        assert!(pod_to_value(pod).is_some());
    }
}
