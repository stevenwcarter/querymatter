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
/// when [`max_nesting_depth`]'s conservative estimate — computed over ONLY
/// the fenced frontmatter block ([`frontmatter_fence`]), never the Markdown
/// body that follows it — says parsing it could recurse past
/// [`MAX_NESTING_DEPTH`].
///
/// This has to run *before* `Matter::parse`, not after: past a certain
/// depth, gray_matter's YAML engine (yaml-rust2) overflows its own parser
/// stack while parsing — see [`MAX_NESTING_DEPTH`]'s doc — which aborts the
/// whole process before it would ever return an `Err` we could handle.
///
/// Scoping to just the fence matters (correctness fix, B9 review): a body
/// containing unrelated bracket-heavy or dash-heavy text (interval notation
/// `[0,1) [1,2) …`, a flattened outline `- - - - …`) is never YAML gray_matter
/// parses at all, so it must never be able to trip this cap and reject an
/// otherwise-valid file.
fn check_nesting_depth(content: &str) -> Result<(), String> {
    let fence = frontmatter_fence(content).unwrap_or_default();
    if max_nesting_depth(fence) > MAX_NESTING_DEPTH {
        return Err(format!(
            "frontmatter nesting exceeds {MAX_NESTING_DEPTH} levels — skipped"
        ));
    }
    Ok(())
}

/// Delimits the leading YAML frontmatter block exactly the way
/// `gray_matter`'s `Matter::parse` does (default delimiter `"---"`, used for
/// both the opening and — absent an explicit `close_delimiter`, which this
/// crate never sets — closing fence): the first line, trailing whitespace
/// aside, must read `---`; the block then runs up to the next line that,
/// trailing whitespace aside, also reads `---`. Returns the raw text
/// strictly between those two fence lines — the only text gray_matter's YAML
/// engine ever sees.
///
/// Returns `None` when there's no opening fence, or the opening fence is
/// never closed — in both cases gray_matter parses no frontmatter at all
/// (the whole input becomes body content, matching [`Extract::None`]), so
/// there is no YAML for a depth scan to reject.
fn frontmatter_fence(content: &str) -> Option<&str> {
    let (first_line, mut tail) = content.split_once('\n')?;
    if first_line.trim_end() != "---" {
        return None;
    }
    let body_start = content.len() - tail.len();
    loop {
        let line_start = content.len() - tail.len();
        match tail.split_once('\n') {
            Some((line, rest)) => {
                if line.trim_end() == "---" {
                    return Some(&content[body_start..line_start]);
                }
                tail = rest;
            }
            // `tail` itself is the final line (no trailing `\n` left),
            // matching what `str::lines()` would yield last.
            None if tail.trim_end() == "---" => {
                return Some(&content[body_start..line_start]);
            }
            None => return None,
        }
    }
}

/// A cheap, conservative pre-parse estimate of how deeply gray_matter's YAML
/// parser would need to recurse to parse `text`, computed without invoking
/// the parser at all.
///
/// `text` must already be just the fenced frontmatter block —
/// [`frontmatter_fence`]'s output, never a whole file or its Markdown body.
/// Scanning body text here would false-positive: bracket- or dash-heavy
/// prose the YAML engine never even sees (interval notation, a flattened
/// outline) would wrongly compute a rejection-worthy depth.
///
/// Three constructs can nest a `Value`, and the estimate is the running max
/// of all three — over-counting only rejects more input, never lets a
/// dangerous depth through, so none of these need to track YAML's grammar
/// exactly:
/// - the `[`/`{` bracket depth open at any point in the text (flow
///   collections can span multiple lines, so this isn't reset per line);
/// - the number of leading `- ` block-sequence markers on any single line
///   (a *compact* nested sequence, e.g. `- - - - v`); and
/// - indentation-nested block collections — the ordinary style, one `- `
///   entry or `key:` mapping per line, each level a separate, further-
///   indented line rather than packed onto one. A stack of the indentation
///   columns currently open tracks this: a line indented deeper than the
///   previous non-blank, non-comment line opens a level (push); a line
///   indented at or shallower than an open level's column closes it (pop
///   back to the matching column); the running maximum stack size is the
///   estimate. Blank and comment-only lines are skipped entirely rather than
///   treated as closing every open level at column 0 — YAML's own grammar
///   treats them the same way (neither affects block structure), and *not*
///   skipping them would let an attacker interleave them between real
///   nesting levels to reset the tracked indentation and slip back under the
///   cap while the real parser keeps recursing regardless.
///
/// A leading `-` only counts as a sequence marker when followed by
/// whitespace or end-of-line (`- - - - v`), not when it's the leading `-` of
/// a negative number (`x: -5`) or a `---`/`...` document marker.
fn max_nesting_depth(text: &str) -> usize {
    let mut bracket_depth = 0usize;
    let mut max_depth = 0usize;
    let mut indent_stack: Vec<usize> = Vec::new();
    let mut prev_indent: Option<usize> = None;
    for line in text.lines() {
        let content = line.trim_start();
        let mut rest = content;
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

        // Indentation-nested collections (B9 follow-up): skip lines that
        // never affect YAML's block structure — blank lines, and
        // comment-only lines — so an attacker can't interleave them between
        // real nesting levels to reset `prev_indent`/`indent_stack` back to
        // column 0 and slip under the cap while the real parser keeps
        // recursing at the true (unreset) depth.
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let indent = line.len() - content.len();
        while let Some(&top) = indent_stack.last() {
            if top > indent {
                indent_stack.pop();
            } else {
                break;
            }
        }
        if prev_indent.is_some_and(|prev| indent > prev) {
            indent_stack.push(indent);
        }
        prev_indent = Some(indent);
        max_depth = max_depth.max(indent_stack.len());
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

    // FIX 1 (B9 critical follow-up): an INDENTATION-nested block sequence —
    // one `-` per line, each further indented than the last — is exactly the
    // vector the compact-dash/bracket estimate above misses (it only ever
    // sees one `-` per line, scoring this depth 1 no matter how deep it
    // actually goes). Pins that the indentation-stack estimate now catches
    // it directly, independent of gray_matter or the CLI.
    #[test]
    fn max_nesting_depth_counts_indentation_nested_block_sequences() {
        let depth = MAX_NESTING_DEPTH + 20;
        let mut text = String::from("x:\n");
        for i in 0..depth {
            text.push_str(&" ".repeat(i + 1));
            text.push_str(if i + 1 == depth { "- v\n" } else { "-\n" });
        }
        assert!(
            max_nesting_depth(&text) > MAX_NESTING_DEPTH,
            "indentation-nested depth {depth} must be detected"
        );
    }

    // Companion to the above: legitimate frontmatter that's genuinely
    // shallow — a 3-level nested mapping — must NOT be scored anywhere near
    // the cap. Pins the estimator against over-rejecting ordinary documents.
    #[test]
    fn max_nesting_depth_stays_small_for_a_legitimate_shallow_nested_mapping() {
        let text = "estimate:\n  low:\n    min: 1\n    max: 2\n  high: 3\n";
        assert_eq!(max_nesting_depth(text), 2);
    }

    // Blank lines and full-line comments must never reset the tracked
    // indentation back to column 0 — see the doc comment on the skip inside
    // `max_nesting_depth`. Without the skip, an attacker could interleave
    // these between real nesting levels to keep every *segment* under the
    // cap while the real YAML parser keeps recursing at the true, unreset
    // depth (comments and blank lines never close a YAML block).
    #[test]
    fn max_nesting_depth_is_not_reset_by_interleaved_blank_or_comment_lines() {
        let depth = MAX_NESTING_DEPTH + 20;
        let mut text = String::from("x:\n");
        for i in 0..depth {
            if i % 10 == 0 {
                text.push('\n');
                text.push_str("# a comment\n");
            }
            text.push_str(&" ".repeat(i + 1));
            text.push_str(if i + 1 == depth { "- v\n" } else { "-\n" });
        }
        assert!(
            max_nesting_depth(&text) > MAX_NESTING_DEPTH,
            "interleaved blank/comment lines must not mask the true depth"
        );
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

    // B9 review fix: `frontmatter_fence` must delimit exactly the slice
    // gray_matter's YAML engine sees, never more (the body) or less.
    #[test]
    fn frontmatter_fence_extracts_only_the_fenced_slice() {
        let c = "---\ntitle: ok\nn: 1\n---\nbody text here\n";
        assert_eq!(frontmatter_fence(c), Some("title: ok\nn: 1\n"));
    }

    #[test]
    fn frontmatter_fence_is_none_without_an_opening_fence() {
        assert_eq!(frontmatter_fence("# just a heading\nbody\n"), None);
    }

    #[test]
    fn frontmatter_fence_is_none_when_never_closed() {
        assert_eq!(frontmatter_fence("---\ntitle: ok\nstill going\n"), None);
    }

    #[test]
    fn frontmatter_fence_handles_a_final_line_with_no_trailing_newline() {
        assert_eq!(
            frontmatter_fence("---\ntitle: ok\n---"),
            Some("title: ok\n")
        );
    }

    // FIX 3 (B9 review, minor): `frontmatter_fence` must recognize a fence in
    // every case gray_matter 0.3.2 would parse one — if gray_matter ever
    // accepted an opening fence `frontmatter_fence` rejects, the depth scan
    // would be skipped and deep YAML would reach the parser unguarded.
    // Checked against gray_matter's vendored source (`Matter::parse`,
    // `gray_matter-0.3.2/src/matter.rs`): its own opening-fence check is
    // `first_line.trim_end() == self.delimiter`, run on the literal first
    // line of the input — exactly as strict as `frontmatter_fence`'s check
    // here. A leading blank line, leading whitespace before the dashes, or a
    // leading BOM all make BOTH sides agree there is no fence at all (the
    // whole input becomes body content), so there's no bypass today. These
    // pin that agreement on the edge inputs directly, so a future gray_matter
    // bump that became more lenient couldn't silently reopen it without a
    // test noticing.
    #[test]
    fn frontmatter_fence_and_gray_matter_agree_a_leading_blank_line_is_no_fence() {
        let c = "\n---\ntitle: ok\n---\n";
        assert_eq!(frontmatter_fence(c), None);
        assert!(matches!(extract(c), Extract::None));
    }

    #[test]
    fn frontmatter_fence_and_gray_matter_agree_leading_whitespace_is_no_fence() {
        let c = "   ---\ntitle: ok\n---\n";
        assert_eq!(frontmatter_fence(c), None);
        assert!(matches!(extract(c), Extract::None));
    }

    #[test]
    fn frontmatter_fence_and_gray_matter_agree_a_leading_bom_is_no_fence() {
        let c = "\u{FEFF}---\ntitle: ok\n---\n";
        assert_eq!(frontmatter_fence(c), None);
        assert!(matches!(extract(c), Extract::None));
    }

    // B9 review fix (Critical false-positive): the depth scan must be scoped
    // to ONLY the fenced YAML gray_matter actually parses. Before this fix,
    // `check_nesting_depth` scanned the WHOLE FILE, so legitimate shallow
    // frontmatter followed by an unrelated bracket-heavy or dash-heavy body
    // (interval notation, a flattened outline) was wrongly rejected as
    // over-nested even though gray_matter never parses the body as YAML at
    // all. This reproduces the exact false positive: confirmed FAILING
    // (returned `Extract::Invalid`) before the fix that scopes the scan to
    // `frontmatter_fence`'s output, and PASSING after.
    #[test]
    fn body_with_many_unclosed_brackets_does_not_reject_shallow_frontmatter() {
        let intervals: String = (0..200).map(|i| format!("[{i},{}) ", i + 1)).collect();
        let c = format!("---\nstatus: draft\n---\n{intervals}\n");
        match extract(&c) {
            Extract::Fields { fields, .. } => {
                assert_eq!(fields.get("status"), Some(&Value::Str("draft".into())));
            }
            other => {
                panic!("expected Fields, body brackets must not reject frontmatter, got {other:?}")
            }
        }
        assert!(body(&c).is_some(), "body() must also accept this file");
    }

    #[test]
    fn body_with_a_flattened_outline_does_not_reject_shallow_frontmatter() {
        let outline = "- ".repeat(200) + "v";
        let c = format!("---\nstatus: draft\n---\n{outline}\n");
        match extract(&c) {
            Extract::Fields { fields, .. } => {
                assert_eq!(fields.get("status"), Some(&Value::Str("draft".into())));
            }
            other => {
                panic!("expected Fields, body dashes must not reject frontmatter, got {other:?}")
            }
        }
        assert!(body(&c).is_some(), "body() must also accept this file");
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
