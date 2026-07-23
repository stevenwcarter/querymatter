//! The query executor: filter / project / order / limit, plus `GROUP BY` and
//! aggregate functions.
//!
//! [`execute`] evaluates a parsed [`Query`] against a set of [`Record`]s and
//! produces a [`ResultTable`]. It dispatches between two pipelines: the
//! **non-grouped** path (no `GROUP BY`, no aggregate `SELECT` items) and the
//! **grouped/aggregate** path (a `GROUP BY` clause and/or an aggregate
//! `SELECT` item); see [`is_grouped_or_aggregate`] for the dispatch check.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use globset::Glob;
use regex::Regex;

use crate::model::{FileAttr, Record, Value, compare_values};
use crate::query::ResultTable;
use crate::query::ast::{
    Aggregate, CmpOp, ColRef, Literal, OrderKey, OrderTarget, Predicate, Query, SelectExpr,
    SelectItem,
};

/// An error that can occur while executing a parsed [`Query`].
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// A non-aggregate `SELECT` item referenced a column that isn't in
    /// `GROUP BY`, e.g. `SELECT status, prd, count(*) GROUP BY status`.
    #[error("column `{0}` must appear in GROUP BY or be an aggregate")]
    NonGroupedColumn(String),
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
/// Dispatches on whether `q` is grouped/aggregate; see
/// [`is_grouped_or_aggregate`].
pub fn execute<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
) -> Result<ResultTable, ExecError> {
    if is_grouped_or_aggregate(q) {
        return execute_grouped(q, records);
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

/// Applies the `FROM` glob and `WHERE` predicate — the filtering step shared
/// by both the non-grouped and grouped/aggregate pipelines.
fn filter_records<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
) -> Result<Vec<&'a Record>, ExecError> {
    let candidates = filter_by_glob(records.collect(), q.from_glob.as_deref())?;
    Ok(candidates
        .into_iter()
        .filter(|record| match &q.filter {
            // SQL 3VL: a row is kept only when the predicate is definitely
            // true; both `Some(false)` and `None` (unknown, i.e. a NULL was
            // involved) exclude it.
            Some(pred) => eval_predicate(record, pred) == Some(true),
            None => true,
        })
        .collect())
}

/// The filter / project / order / limit pipeline for a non-grouped query.
fn execute_ungrouped<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
) -> Result<ResultTable, ExecError> {
    let filtered = filter_records(q, records)?;
    let columns = expand_select(q, &filtered);
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

/// The filter / group / aggregate / order / limit pipeline for a query with
/// a `GROUP BY` clause and/or aggregate `SELECT` items.
///
/// With aggregates but no `GROUP BY`, every filtered row is treated as one
/// group (see [`group_rows`]). Group order is made deterministic by sorting
/// on the key tuple before `ORDER BY` is applied, so results are stable even
/// when the query has no explicit ordering.
fn execute_grouped<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
) -> Result<ResultTable, ExecError> {
    let filtered = filter_records(q, records)?;

    let items = validate_grouped_select(q)?;
    let headers: Vec<String> = q.select.iter().map(|item| item.header()).collect();

    let mut groups = group_rows(&filtered, &q.group_by);
    groups.sort_by(|a, b| compare_key_tuple(&a.key, &b.key));

    let mut rows: Vec<(Vec<Value>, Vec<Value>)> = groups
        .into_iter()
        .map(|group| {
            let row = project_group(&group, &items);
            (group.key, row)
        })
        .collect();

    let order = resolve_group_order_targets(&q.order_by, &headers, &q.group_by)?;
    rows.sort_by(|(ka, rowa), (kb, rowb)| {
        order
            .iter()
            .map(|(target, desc)| {
                let va = group_order_key_value(target, ka, rowa);
                let vb = group_order_key_value(target, kb, rowb);
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

/// A validated grouped `SELECT` item: SQL only allows projecting a grouping
/// key or an aggregate once `GROUP BY` (or a bare aggregate) is in play.
enum GroupedSelectItem {
    /// Index into `group_by` (and thus into a group's key tuple).
    Key(usize),
    /// An aggregate to compute over the group's rows.
    Agg(Aggregate),
}

/// Validates `q.select` for the grouped path: every non-aggregate item must
/// be a `ColRef` that also appears in `q.group_by`; anything else — a column
/// outside the grouping keys, or `*` — is rejected, since neither reduces to
/// a single value per group.
fn validate_grouped_select(q: &Query) -> Result<Vec<GroupedSelectItem>, ExecError> {
    q.select
        .iter()
        .map(|item| match &item.expr {
            SelectExpr::Agg(agg) => Ok(GroupedSelectItem::Agg(agg.clone())),
            SelectExpr::Col(col) => q
                .group_by
                .iter()
                .position(|key| key == col)
                .map(GroupedSelectItem::Key)
                .ok_or_else(|| ExecError::NonGroupedColumn(item.header())),
            SelectExpr::Star => Err(ExecError::NonGroupedColumn(item.header())),
        })
        .collect()
}

/// One `GROUP BY` bucket: its key tuple (the `group_by` columns' shared
/// values) plus every record that produced that key.
struct Group<'a> {
    key: Vec<Value>,
    rows: Vec<&'a Record>,
}

/// Buckets `records` by the tuple of `group_by` column values, in
/// first-appearance order (the caller sorts for determinism afterward).
///
/// An empty `group_by` means "aggregate over everything": every record —
/// including none at all — falls into the single group keyed by `[]`, so a
/// bare `count(*)` still returns one row for an empty input, matching SQL.
fn group_rows<'a>(records: &[&'a Record], group_by: &[ColRef]) -> Vec<Group<'a>> {
    if group_by.is_empty() {
        return vec![Group {
            key: Vec::new(),
            rows: records.to_vec(),
        }];
    }
    let mut groups: Vec<Group<'a>> = Vec::new();
    for &record in records {
        let key: Vec<Value> = group_by
            .iter()
            .map(|col| resolve_col(record, col))
            .collect();
        match groups.iter_mut().find(|group| group.key == key) {
            Some(group) => group.rows.push(record),
            None => groups.push(Group {
                key,
                rows: vec![record],
            }),
        }
    }
    groups
}

/// Orders two group-key tuples element-wise via [`order_cmp`] (always
/// ascending, `NULL` last), used only to make group order deterministic
/// before an explicit `ORDER BY` is applied.
fn compare_key_tuple(a: &[Value], b: &[Value]) -> Ordering {
    a.iter()
        .zip(b)
        .map(|(x, y)| order_cmp(x, y, false))
        .find(|ord| *ord != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

/// Projects one group's row: a grouping key becomes its key-tuple value, an
/// aggregate is computed over the group's records.
fn project_group(group: &Group<'_>, items: &[GroupedSelectItem]) -> Vec<Value> {
    items
        .iter()
        .map(|item| match item {
            GroupedSelectItem::Key(idx) => group.key[*idx].clone(),
            GroupedSelectItem::Agg(agg) => compute_aggregate(agg, &group.rows),
        })
        .collect()
}

/// Computes one aggregate function's value over a group's rows.
fn compute_aggregate(agg: &Aggregate, rows: &[&Record]) -> Value {
    match agg {
        Aggregate::CountStar => Value::Int(rows.len() as i64),
        Aggregate::Count(col, false) => Value::Int(non_null_values(rows, col).count() as i64),
        Aggregate::Count(col, true) => {
            let distinct: BTreeSet<String> = non_null_values(rows, col)
                .map(|v| v.to_cmp_string())
                .collect();
            Value::Int(distinct.len() as i64)
        }
        Aggregate::Sum(col) => Value::Float(numeric_values(rows, col).sum()),
        Aggregate::Avg(col) => {
            let nums: Vec<f64> = numeric_values(rows, col).collect();
            if nums.is_empty() {
                Value::Null
            } else {
                Value::Float(nums.iter().sum::<f64>() / nums.len() as f64)
            }
        }
        Aggregate::Min(col) => extreme_value(rows, col, Ordering::Less),
        Aggregate::Max(col) => extreme_value(rows, col, Ordering::Greater),
        Aggregate::GroupConcat(col) => Value::Str(
            non_null_values(rows, col)
                .map(|v| v.display())
                .collect::<Vec<_>>()
                .join(", "),
        ),
    }
}

/// The non-null values of `col` across `rows`, in row order.
fn non_null_values<'a>(rows: &'a [&Record], col: &'a ColRef) -> impl Iterator<Item = Value> + 'a {
    rows.iter()
        .map(move |record| resolve_col(record, col))
        .filter(|v| !v.is_null())
}

/// The numeric-coercible values of `col` across `rows`; `NULL` and
/// non-numeric values are both skipped (mirroring `Value::as_number`, which
/// already returns `None` for both).
fn numeric_values<'a>(rows: &'a [&Record], col: &'a ColRef) -> impl Iterator<Item = f64> + 'a {
    rows.iter()
        .filter_map(move |record| resolve_col(record, col).as_number())
}

/// `MIN`/`MAX` over `col`'s non-null values via [`compare_values`], `Null`
/// when there are none. `want` is the ordering that means "this value
/// replaces the running extreme" (`Less` for `MIN`, `Greater` for `MAX`).
fn extreme_value(rows: &[&Record], col: &ColRef, want: Ordering) -> Value {
    non_null_values(rows, col).fold(Value::Null, |acc, v| match compare_values(&v, &acc) {
        Some(ord) if ord == want => v,
        Some(_) => acc,
        // `acc` is still `Null` (no extreme picked yet): take `v`.
        None => v,
    })
}

/// An `ORDER BY` target for the grouped path, resolved once against the
/// projection and the `GROUP BY` keys.
enum ResolvedGroupOrderTarget {
    /// An index into the projected row (a `SELECT ... AS alias` match, same
    /// alias rule as the non-grouped path).
    Row(usize),
    /// An index into the group key tuple (a bare `ORDER BY` column, which is
    /// only meaningful when it's one of the `GROUP BY` keys).
    GroupKey(usize),
}

/// Resolves each `ORDER BY` key's target for the grouped path: an explicit
/// alias resolves against `headers`, exactly like the non-grouped path; a
/// bare column must be one of `group_by`'s keys — referencing anything else
/// is as invalid as selecting it, so it's rejected the same way.
fn resolve_group_order_targets(
    order_by: &[OrderKey],
    headers: &[String],
    group_by: &[ColRef],
) -> Result<Vec<(ResolvedGroupOrderTarget, bool)>, ExecError> {
    order_by
        .iter()
        .map(|key| {
            let target = match &key.target {
                OrderTarget::Alias(name) => headers
                    .iter()
                    .position(|h| h == name)
                    .map(ResolvedGroupOrderTarget::Row)
                    .ok_or_else(|| ExecError::UnknownAlias(name.clone()))?,
                OrderTarget::Col(col) => group_by
                    .iter()
                    .position(|g| g == col)
                    .map(ResolvedGroupOrderTarget::GroupKey)
                    .ok_or_else(|| ExecError::NonGroupedColumn(col_header(col)))?,
            };
            Ok((target, key.desc))
        })
        .collect()
}

/// Renders a bare `ColRef` the way it would appear as a default `SELECT`
/// header, for a `NonGroupedColumn` message about an `ORDER BY` column.
fn col_header(col: &ColRef) -> String {
    SelectItem {
        expr: SelectExpr::Col(col.clone()),
        alias: None,
    }
    .header()
}

/// Reads the sort key's value for one grouped row, given its key tuple and
/// already-projected row.
fn group_order_key_value(target: &ResolvedGroupOrderTarget, key: &[Value], row: &[Value]) -> Value {
    match target {
        ResolvedGroupOrderTarget::Row(idx) => row[*idx].clone(),
        ResolvedGroupOrderTarget::GroupKey(idx) => key[*idx].clone(),
    }
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
fn expand_select(q: &Query, filtered: &[&Record]) -> Vec<(String, ColRef)> {
    let mut columns = Vec::with_capacity(q.select.len());
    for item in &q.select {
        match &item.expr {
            SelectExpr::Star => {
                for name in sorted_field_union(filtered) {
                    columns.push((name.clone(), ColRef::Field(name)));
                }
            }
            SelectExpr::Col(col) => columns.push((item.header(), col.clone())),
            SelectExpr::Agg(_) => {
                unreachable!("execute() routes aggregate SELECT items to execute_grouped")
            }
        }
    }
    columns
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

/// Evaluates a `WHERE` predicate tree against a single record under SQL
/// three-valued logic (3VL): `Some(true)` / `Some(false)` / `None`, where
/// `None` means "unknown" — a NULL field value was involved.
///
/// A `WHERE` keeps a row only when this is `Some(true)` (see
/// [`filter_records`]); both `Some(false)` and `None` exclude it. Threading
/// the unknown through negation is what makes `status NOT IN (...)`,
/// `status NOT LIKE ...`, and `NOT (status = ...)` all EXCLUDE a NULL-`status`
/// row — matching the plain-`Compare` path and the spec's "any comparison
/// where a side is Null yields 'not true'" rule (§4). Only `IS NULL` /
/// `IS NOT NULL` are ever determinate for a NULL field.
fn eval_predicate(record: &Record, pred: &Predicate) -> Option<bool> {
    match pred {
        Predicate::Compare(col, op, lit) => eval_compare(&resolve_col(record, col), op, lit),
        Predicate::Like(col, pattern, negated) => {
            let value = resolve_col(record, col);
            if value.is_null() {
                return None;
            }
            let base = Some(like_matches(&value.to_cmp_string(), pattern));
            maybe_negate(base, *negated)
        }
        Predicate::In(col, literals, negated) => {
            let value = resolve_col(record, col);
            if value.is_null() {
                return None;
            }
            let base = Some(
                literals
                    .iter()
                    .any(|lit| eval_compare(&value, &CmpOp::Eq, lit) == Some(true)),
            );
            maybe_negate(base, *negated)
        }
        // The only predicate that is determinate — and true — for a NULL field.
        Predicate::IsNull(col, negated) => Some(resolve_col(record, col).is_null() != *negated),
        Predicate::And(a, b) => {
            three_valued_and(eval_predicate(record, a), eval_predicate(record, b))
        }
        Predicate::Or(a, b) => {
            three_valued_or(eval_predicate(record, a), eval_predicate(record, b))
        }
        Predicate::Not(inner) => three_valued_not(eval_predicate(record, inner)),
    }
}

/// Applies the 3VL `NOT` to `v` when `negated`, else returns it unchanged.
fn maybe_negate(v: Option<bool>, negated: bool) -> Option<bool> {
    if negated { three_valued_not(v) } else { v }
}

/// SQL 3VL `NOT`: `NOT unknown` stays unknown.
fn three_valued_not(v: Option<bool>) -> Option<bool> {
    v.map(|b| !b)
}

/// SQL 3VL `AND`: `false` dominates, then unknown, then `true`.
fn three_valued_and(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(false), _) | (_, Some(false)) => Some(false),
        (Some(true), Some(true)) => Some(true),
        _ => None,
    }
}

/// SQL 3VL `OR`: `true` dominates, then unknown, then `false`.
fn three_valued_or(a: Option<bool>, b: Option<bool>) -> Option<bool> {
    match (a, b) {
        (Some(true), _) | (_, Some(true)) => Some(true),
        (Some(false), Some(false)) => Some(false),
        _ => None,
    }
}

/// Compares `value` against a literal per the coercion rule, under 3VL.
///
/// A NULL `value` yields `None` (unknown) — the only source of unknown here.
/// Otherwise the result is always `Some(_)`: a string literal compares
/// `to_cmp_string()`; a numeric literal requires `value` to also be numeric;
/// a non-null value that can't be coerced/ordered against the literal (a
/// numeric literal vs a non-numeric value, or a `NULL` literal) fails the
/// predicate as `Some(false)`, per the spec's "the row fails the predicate".
fn eval_compare(value: &Value, op: &CmpOp, lit: &Literal) -> Option<bool> {
    if value.is_null() {
        return None;
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
    Some(ordering.is_some_and(|ord| apply_cmp(op, ord)))
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
    /// Records for the 3VL / NULL-under-negation tests (Fix 1): two carry a
    /// `status`, one has none — so `status` resolves to `Value::Null` on it.
    fn recs_with_null_status() -> Vec<Record> {
        vec![
            rec("s", "s/a.md", &[("status", Value::Str("draft".into()))]),
            rec("s", "s/b.md", &[("status", Value::Str("synced".into()))]),
            rec("s", "s/c.md", &[("prd", Value::Str("011".into()))]),
        ]
    }

    #[test]
    fn not_in_excludes_null_field_row() {
        // 'synced' passes; 'draft' fails; the status-less row is UNKNOWN under
        // 3VL, so `NOT IN` must NOT resurrect it (the pre-3VL bug did).
        let all = recs_with_null_status();
        let q = parse("SELECT file.name WHERE status NOT IN ('draft')").unwrap();
        let t = execute(&q, all.iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b.md".into())]]);
    }
    #[test]
    fn not_like_excludes_null_field_row() {
        let all = recs_with_null_status();
        let q = parse("SELECT file.name WHERE status NOT LIKE 'dr%'").unwrap();
        let t = execute(&q, all.iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b.md".into())]]);
    }
    #[test]
    fn not_paren_compare_excludes_null_field_row() {
        let all = recs_with_null_status();
        let q = parse("SELECT file.name WHERE NOT (status = 'draft')").unwrap();
        let t = execute(&q, all.iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b.md".into())]]);
    }
    #[test]
    fn is_null_and_is_not_null_partition_on_null_field() {
        let all = recs_with_null_status();
        let is_null = parse("SELECT file.name WHERE status IS NULL").unwrap();
        assert_eq!(
            execute(&is_null, all.iter()).unwrap().rows,
            vec![vec![Value::Str("c.md".into())]],
            "IS NULL selects only the status-less row"
        );
        let not_null = parse("SELECT file.name WHERE status IS NOT NULL").unwrap();
        assert_eq!(
            execute(&not_null, all.iter()).unwrap().rows,
            vec![
                vec![Value::Str("a.md".into())],
                vec![Value::Str("b.md".into())],
            ],
            "IS NOT NULL excludes the status-less row"
        );
    }
    #[test]
    fn ne_and_not_paren_agree_on_null_field() {
        // `status != 'draft'` and `NOT (status = 'draft')` must agree under 3VL:
        // both exclude the status-less (NULL) row and both keep 'synced'.
        let all = recs_with_null_status();
        let ne = execute(
            &parse("SELECT file.name WHERE status != 'draft'").unwrap(),
            all.iter(),
        )
        .unwrap();
        let not_paren = execute(
            &parse("SELECT file.name WHERE NOT (status = 'draft')").unwrap(),
            all.iter(),
        )
        .unwrap();
        assert_eq!(ne.rows, not_paren.rows);
        assert_eq!(ne.rows, vec![vec![Value::Str("b.md".into())]]);
    }
    #[test]
    fn where_numeric_compare_coerces_and_is_numeric() {
        // Fix 2 / spec §10: pin the WHERE numeric-coercion funnel. `n > 2`
        // compares numerically, and a numeric string ("5") coerces, so Int(3)
        // and Str("5") pass while Int(1) fails.
        let rows = [
            rec("s", "s/a.md", &[("n", Value::Int(1))]),
            rec("s", "s/b.md", &[("n", Value::Int(3))]),
            rec("s", "s/c.md", &[("n", Value::Str("5".into()))]),
        ];
        let q = parse("SELECT n WHERE n > 2").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        assert_eq!(
            t.rows,
            vec![vec![Value::Int(3)], vec![Value::Str("5".into())]]
        );
    }

    #[test]
    fn null_field_sorts_last_asc_and_desc() {
        // One record is missing `status` entirely, so ORDER BY status must
        // resolve it to Value::Null and place it last regardless of
        // direction (pins order_cmp's null-handling for both the
        // non-grouped and grouped paths).
        let with_null = rec("s", "s/plans/d.md", &[("prd", Value::Str("012".into()))]);
        let mut all = recs();
        all.push(with_null);

        let asc = parse("SELECT status, file.name ORDER BY status ASC").unwrap();
        let t_asc = execute(&asc, all.iter()).unwrap();
        assert_eq!(t_asc.rows.last().unwrap()[0], Value::Null);

        let desc = parse("SELECT status, file.name ORDER BY status DESC").unwrap();
        let t_desc = execute(&desc, all.iter()).unwrap();
        assert_eq!(t_desc.rows.last().unwrap()[0], Value::Null);
    }
}

#[cfg(test)]
mod agg_tests {
    use super::*;
    use crate::model::{Record, Value};
    use crate::query::parse::parse;
    use indexmap::IndexMap;
    use std::path::Path;

    fn rec(path: &str, status: &str, prd: &str) -> Record {
        let mut m = IndexMap::new();
        m.insert("status".into(), Value::Str(status.into()));
        m.insert("prd".into(), Value::Str(prd.into()));
        Record::new(Path::new("s"), Path::new(path), m)
    }
    fn recs() -> Vec<Record> {
        vec![
            rec("s/a.md", "draft", "010"),
            rec("s/b.md", "synced", "010"),
            rec("s/c.md", "synced", "011"),
        ]
    }
    /// Builds a record with a `status` field and one extra numeric-ish field
    /// `n`, for the `sum`/`avg`/`min`/`max`/plain-`count` tests below.
    fn rec_n(path: &str, status: &str, n: Value) -> Record {
        let mut m = IndexMap::new();
        m.insert("status".into(), Value::Str(status.into()));
        m.insert("n".into(), n);
        Record::new(Path::new("s"), Path::new(path), m)
    }

    #[test]
    fn count_per_status_renamed_ordered() {
        let q =
            parse("SELECT status, count(*) AS Count GROUP BY status ORDER BY Count DESC").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.headers, vec!["status", "Count"]);
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("synced".into()), Value::Int(2)],
                vec![Value::Str("draft".into()), Value::Int(1)],
            ]
        );
    }
    #[test]
    fn bare_count_star_single_group() {
        let q = parse("SELECT count(*) AS n").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(3)]]);
    }
    #[test]
    fn count_distinct() {
        let q = parse("SELECT count(distinct status) AS d").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(2)]]);
    }
    #[test]
    fn group_concat() {
        let q = parse("SELECT prd, group_concat(status) AS ss GROUP BY prd ORDER BY prd").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(
            t.rows[0],
            vec![Value::Str("010".into()), Value::Str("draft, synced".into())]
        );
    }
    #[test]
    fn non_grouped_column_errors() {
        let q = parse("SELECT status, prd, count(*) GROUP BY status").unwrap();
        assert!(matches!(
            execute(&q, recs().iter()),
            Err(ExecError::NonGroupedColumn(_))
        ));
    }
    #[test]
    fn sum_and_avg_over_numeric_column() {
        let rows = [
            rec_n("s/a.md", "draft", Value::Int(2)),
            rec_n("s/b.md", "draft", Value::Int(4)),
        ];
        let q = parse("SELECT sum(n) AS total, avg(n) AS mean").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Float(6.0), Value::Float(3.0)]]);
    }
    #[test]
    fn sum_and_avg_skip_non_numeric_values() {
        let rows = [
            rec_n("s/a.md", "draft", Value::Int(2)),
            rec_n("s/b.md", "draft", Value::Str("n/a".into())),
            rec_n("s/c.md", "draft", Value::Int(4)),
        ];
        let q = parse("SELECT sum(n) AS total, avg(n) AS mean").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        // The non-numeric "n/a" is skipped, so only 2 and 4 count.
        assert_eq!(t.rows, vec![vec![Value::Float(6.0), Value::Float(3.0)]]);
    }
    #[test]
    fn avg_of_no_numeric_values_is_null_and_sum_is_zero() {
        let rows = [
            rec_n("s/a.md", "draft", Value::Str("n/a".into())),
            rec_n("s/b.md", "draft", Value::Null),
        ];
        let q = parse("SELECT sum(n) AS total, avg(n) AS mean").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        // `avg` over zero numeric values is Null; `sum` is the identity 0.0.
        assert_eq!(t.rows, vec![vec![Value::Float(0.0), Value::Null]]);
    }
    #[test]
    fn min_and_max_over_column() {
        let rows = [
            rec_n("s/a.md", "draft", Value::Int(5)),
            rec_n("s/b.md", "draft", Value::Int(1)),
            rec_n("s/c.md", "draft", Value::Int(3)),
        ];
        let q = parse("SELECT min(n) AS lo, max(n) AS hi").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(1), Value::Int(5)]]);
    }
    #[test]
    fn min_and_max_of_all_null_column_is_null() {
        let rows = [
            rec_n("s/a.md", "draft", Value::Null),
            rec_n("s/b.md", "draft", Value::Null),
        ];
        let q = parse("SELECT min(n) AS lo, max(n) AS hi").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Null, Value::Null]]);
    }
    #[test]
    fn count_col_counts_non_null_only() {
        // Contrast with `count(*)` (row count) and `count(distinct ...)`
        // (already pinned by `count_distinct` above): plain `count(col)`
        // counts only the non-null values of that column.
        let rows = [
            rec_n("s/a.md", "draft", Value::Int(1)),
            rec_n("s/b.md", "draft", Value::Null),
            rec_n("s/c.md", "draft", Value::Int(3)),
        ];
        let q = parse("SELECT count(*) AS rows, count(n) AS non_null").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(3), Value::Int(2)]]);
    }
    #[test]
    fn order_by_ungrouped_column_errors() {
        // `prd` isn't a GROUP BY key here, so ordering by it is exactly as
        // invalid as selecting it would be.
        let q = parse("SELECT status, count(*) GROUP BY status ORDER BY prd").unwrap();
        assert!(matches!(
            execute(&q, recs().iter()),
            Err(ExecError::NonGroupedColumn(_))
        ));
    }
    #[test]
    fn aggregate_with_no_group_by_over_zero_rows_is_one_row_of_zero() {
        let q = parse("SELECT count(*) AS n WHERE status = 'nope'").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(0)]]);
    }
}
