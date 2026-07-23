//! The query executor: filter / project / order / limit.
//!
//! [`execute`] evaluates a parsed [`Query`] against a set of [`Record`]s and
//! produces a [`ResultTable`]. This module implements the **non-grouped**
//! path only — queries with no `GROUP BY` and no aggregate `SELECT` items.
//! Queries that need grouping or aggregation are rejected with
//! [`ExecError::NotYetSupported`] until Task 8 fills in that branch; see
//! [`is_grouped_or_aggregate`] for the dispatch check.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use globset::Glob;
use regex::Regex;

use crate::model::{FileAttr, Record, Value, compare_values};
use crate::query::ResultTable;
use crate::query::ast::{
    CmpOp, ColRef, Literal, OrderKey, OrderTarget, Predicate, Query, SelectExpr,
};

/// An error that can occur while executing a parsed [`Query`].
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The query has a `GROUP BY` clause or an aggregate `SELECT` item;
    /// executing those is Task 8's job.
    #[error("GROUP BY and aggregate queries are not supported yet")]
    NotYetSupported,
    /// The `FROM '<glob>'` pattern failed to compile.
    #[error("invalid FROM glob `{glob}`: {source}")]
    InvalidGlob {
        glob: String,
        #[source]
        source: globset::Error,
    },
    /// An `ORDER BY` alias didn't match any `SELECT` alias.
    #[error("unknown ORDER BY alias `{0}`")]
    UnknownAlias(String),
}

/// Executes `q` against `records`, returning the projected, filtered,
/// ordered, and limited result.
///
/// This dispatches on whether `q` is grouped/aggregate (Task 8) or not (this
/// task); see [`is_grouped_or_aggregate`].
pub fn execute<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
) -> Result<ResultTable, ExecError> {
    if is_grouped_or_aggregate(q) {
        return Err(ExecError::NotYetSupported);
    }
    execute_ungrouped(q, records)
}

/// True when `q` needs the grouped/aggregate execution path (Task 8): a
/// `GROUP BY` clause, or any `SELECT` item that is an aggregate.
fn is_grouped_or_aggregate(q: &Query) -> bool {
    !q.group_by.is_empty()
        || q.select
            .iter()
            .any(|item| matches!(item.expr, SelectExpr::Agg(_)))
}

/// The filter / project / order / limit pipeline for a non-grouped query.
fn execute_ungrouped<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
) -> Result<ResultTable, ExecError> {
    let candidates = filter_by_glob(records.collect(), q.from_glob.as_deref())?;
    let filtered: Vec<&Record> = candidates
        .into_iter()
        .filter(|record| match &q.filter {
            Some(pred) => eval_predicate(record, pred),
            None => true,
        })
        .collect();

    let columns = expand_select(q, &filtered)?;
    let headers: Vec<String> = columns.iter().map(|(header, _)| header.clone()).collect();
    let mut rows: Vec<(&Record, Vec<Value>)> = filtered
        .into_iter()
        .map(|record| {
            let row = columns
                .iter()
                .map(|(_, col)| resolve_col(record, col))
                .collect();
            (record, row)
        })
        .collect();

    let order = resolve_order_targets(&q.order_by, &headers)?;
    rows.sort_by(|(ra, rowa), (rb, rowb)| {
        order
            .iter()
            .map(|(target, desc)| {
                let va = order_key_value(target, ra, rowa);
                let vb = order_key_value(target, rb, rowb);
                order_cmp(&va, &vb, *desc)
            })
            .find(|ord| *ord != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    });

    let offset = q.offset.unwrap_or(0);
    let rows: Vec<Vec<Value>> = rows
        .into_iter()
        .map(|(_, row)| row)
        .skip(offset)
        .take(q.limit.unwrap_or(usize::MAX))
        .collect();

    Ok(ResultTable { headers, rows })
}

/// Keeps only the records whose `file.path` matches `glob`, if given.
fn filter_by_glob<'a>(
    records: Vec<&'a Record>,
    glob: Option<&str>,
) -> Result<Vec<&'a Record>, ExecError> {
    let Some(pattern) = glob else {
        return Ok(records);
    };
    let matcher = Glob::new(pattern)
        .map_err(|source| ExecError::InvalidGlob {
            glob: pattern.to_string(),
            source,
        })?
        .compile_matcher();
    Ok(records
        .into_iter()
        .filter(|record| matcher.is_match(record.file_attr(FileAttr::Path).to_cmp_string()))
        .collect())
}

/// Expands `q.select` into a flat list of `(header, column)` pairs, resolving
/// `SelectExpr::Star` to the sorted union of `filtered`'s field names.
///
/// Aggregate select items cannot appear here: [`execute`] routes queries
/// containing one to the grouped path before this function runs.
fn expand_select(q: &Query, filtered: &[&Record]) -> Result<Vec<(String, ColRef)>, ExecError> {
    let mut columns = Vec::with_capacity(q.select.len());
    for item in &q.select {
        match &item.expr {
            SelectExpr::Star => {
                for name in sorted_field_union(filtered) {
                    columns.push((name.clone(), ColRef::Field(name)));
                }
            }
            SelectExpr::Col(col) => columns.push((item.header(), col.clone())),
            SelectExpr::Agg(_) => return Err(ExecError::NotYetSupported),
        }
    }
    Ok(columns)
}

/// The sorted union of every field name across `records`.
fn sorted_field_union(records: &[&Record]) -> Vec<String> {
    let names: BTreeSet<&str> = records.iter().flat_map(|r| r.field_names()).collect();
    names.into_iter().map(String::from).collect()
}

/// Looks up a column reference's value on `record`.
fn resolve_col(record: &Record, col: &ColRef) -> Value {
    match col {
        ColRef::Field(name) => record.field(name),
        ColRef::File(attr) => record.file_attr(*attr),
    }
}

/// Evaluates a `WHERE` predicate tree against a single record.
fn eval_predicate(record: &Record, pred: &Predicate) -> bool {
    match pred {
        Predicate::Compare(col, op, lit) => eval_compare(&resolve_col(record, col), op, lit),
        Predicate::Like(col, pattern, negated) => {
            let value = resolve_col(record, col);
            let matched = !value.is_null() && like_matches(&value.to_cmp_string(), pattern);
            matched != *negated
        }
        Predicate::In(col, literals, negated) => {
            let value = resolve_col(record, col);
            let matched = literals
                .iter()
                .any(|lit| eval_compare(&value, &CmpOp::Eq, lit));
            matched != *negated
        }
        Predicate::IsNull(col, negated) => resolve_col(record, col).is_null() != *negated,
        Predicate::And(a, b) => eval_predicate(record, a) && eval_predicate(record, b),
        Predicate::Or(a, b) => eval_predicate(record, a) || eval_predicate(record, b),
        Predicate::Not(inner) => !eval_predicate(record, inner),
    }
}

/// Compares `value` against a literal per the coercion rule: a string literal
/// compares `to_cmp_string()`; a numeric literal requires `value` to also be
/// numeric; a `Null` value (or `NULL` literal) never compares equal/ordered.
fn eval_compare(value: &Value, op: &CmpOp, lit: &Literal) -> bool {
    if value.is_null() {
        return false;
    }
    let ordering = match lit {
        Literal::Str(s) => Some(value.to_cmp_string().cmp(s)),
        Literal::Int(_) | Literal::Float(_) => match (value.as_number(), literal_as_number(lit)) {
            (Some(v), Some(l)) => v.partial_cmp(&l),
            _ => None,
        },
        Literal::Bool(b) => compare_values(value, &Value::Bool(*b)),
        Literal::Null => None,
    };
    ordering.is_some_and(|ord| apply_cmp(op, ord))
}

/// The `f64` value of an `Int`/`Float` literal, or `None` for other kinds.
fn literal_as_number(lit: &Literal) -> Option<f64> {
    match lit {
        Literal::Int(i) => Some(*i as f64),
        Literal::Float(f) => Some(*f),
        Literal::Str(_) | Literal::Bool(_) | Literal::Null => None,
    }
}

/// Interprets an `Ordering` per comparison operator.
fn apply_cmp(op: &CmpOp, ordering: Ordering) -> bool {
    match op {
        CmpOp::Eq => ordering == Ordering::Equal,
        CmpOp::Ne => ordering != Ordering::Equal,
        CmpOp::Lt => ordering == Ordering::Less,
        CmpOp::Le => ordering != Ordering::Greater,
        CmpOp::Gt => ordering == Ordering::Greater,
        CmpOp::Ge => ordering != Ordering::Less,
    }
}

/// Matches `value` against a SQL `LIKE` pattern: `%` becomes `.*`, `_`
/// becomes `.`, and everything else is matched literally (case-sensitive).
fn like_matches(value: &str, pattern: &str) -> bool {
    let escaped = regex::escape(pattern);
    let translated = escaped.replace('%', ".*").replace('_', ".");
    // `translated` is `regex::escape`'s output with only `.*`/`.` substituted
    // in, so wrapping it in `^…$` is always a valid regex.
    let re = Regex::new(&format!("^{translated}$"))
        .expect("LIKE pattern translates to a well-formed regex");
    re.is_match(value)
}

/// An `ORDER BY` target resolved against the projection, so sorting doesn't
/// need to re-resolve an alias (or fail) on every comparison.
enum ResolvedOrderTarget {
    /// An index into the projected row (a `SELECT ... AS alias` match).
    AliasIndex(usize),
    /// A fresh column lookup on the source record.
    Col(ColRef),
}

/// Resolves each `ORDER BY` key's target once, up front, returning the
/// resolved target paired with its `DESC` flag.
fn resolve_order_targets(
    order_by: &[OrderKey],
    headers: &[String],
) -> Result<Vec<(ResolvedOrderTarget, bool)>, ExecError> {
    order_by
        .iter()
        .map(|key| {
            let target = match &key.target {
                OrderTarget::Alias(name) => {
                    let idx = headers
                        .iter()
                        .position(|h| h == name)
                        .ok_or_else(|| ExecError::UnknownAlias(name.clone()))?;
                    ResolvedOrderTarget::AliasIndex(idx)
                }
                OrderTarget::Col(col) => ResolvedOrderTarget::Col(col.clone()),
            };
            Ok((target, key.desc))
        })
        .collect()
}

/// Reads the sort key's value for one row, given its source record and
/// already-projected row.
fn order_key_value(target: &ResolvedOrderTarget, record: &Record, row: &[Value]) -> Value {
    match target {
        ResolvedOrderTarget::AliasIndex(idx) => row[*idx].clone(),
        ResolvedOrderTarget::Col(col) => resolve_col(record, col),
    }
}

/// Orders two sort-key values, always placing `NULL` last regardless of
/// `desc` (only the non-null comparison is reversed for `DESC`).
fn order_cmp(a: &Value, b: &Value, desc: bool) -> Ordering {
    match (a.is_null(), b.is_null()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let ord = compare_values(a, b).unwrap_or(Ordering::Equal);
            if desc { ord.reverse() } else { ord }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Record, Value};
    use crate::query::parse::parse;
    use indexmap::IndexMap;
    use std::path::Path;

    fn rec(root: &str, path: &str, kv: &[(&str, Value)]) -> Record {
        let mut m = IndexMap::new();
        for (k, v) in kv {
            m.insert((*k).to_string(), v.clone());
        }
        Record::new(Path::new(root), Path::new(path), m)
    }
    fn recs() -> Vec<Record> {
        vec![
            rec(
                "s",
                "s/plans/a.md",
                &[
                    ("status", Value::Str("draft".into())),
                    ("prd", Value::Str("010".into())),
                ],
            ),
            rec(
                "s",
                "s/plans/b.md",
                &[
                    ("status", Value::Str("synced".into())),
                    ("prd", Value::Str("010".into())),
                ],
            ),
            rec(
                "s",
                "s/product/c.md",
                &[
                    ("status", Value::Str("synced".into())),
                    ("prd", Value::Str("011".into())),
                ],
            ),
        ]
    }

    #[test]
    fn filter_and_project_with_alias() {
        let q = parse("SELECT status AS S, file.name WHERE prd = '010'").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.headers, vec!["S", "file.name"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(
            t.rows[0],
            vec![Value::Str("draft".into()), Value::Str("a.md".into())]
        );
    }
    #[test]
    fn order_desc_and_limit() {
        let q = parse("SELECT status WHERE prd = '010' ORDER BY status DESC LIMIT 1").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("synced".into())]]);
    }
    #[test]
    fn star_expands_sorted_union() {
        let q = parse("SELECT * WHERE status = 'draft'").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.headers, vec!["prd", "status"]);
    }
    #[test]
    fn from_glob_filters_by_path() {
        let q = parse("SELECT file.name FROM 'plans/**'").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows.len(), 2);
    }
    #[test]
    fn like_and_in() {
        let q = parse("SELECT status WHERE status LIKE 'syn%'").unwrap();
        assert_eq!(execute(&q, recs().iter()).unwrap().rows.len(), 2);
        let q2 = parse("SELECT status WHERE prd IN ('011')").unwrap();
        assert_eq!(execute(&q2, recs().iter()).unwrap().rows.len(), 1);
    }
}
