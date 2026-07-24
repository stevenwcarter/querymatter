//! The query executor: filter / project / order / limit, plus `GROUP BY`,
//! aggregate functions, and `HAVING` group filtering.
//!
//! [`execute`] evaluates a parsed [`Query`] against a set of [`Record`]s and
//! produces a [`ResultTable`]. It dispatches between two pipelines: the
//! **non-grouped** path (no `GROUP BY`, no aggregate `SELECT` items) and the
//! **grouped/aggregate** path (a `GROUP BY` clause and/or an aggregate
//! `SELECT` item); see [`is_grouped_or_aggregate`] for the dispatch check.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use globset::Glob;
use indexmap::IndexMap;
use regex::Regex;

use crate::model::{FileAttr, Record, Value, compare_values};
use crate::query::ResultTable;
use crate::query::ast::{
    Aggregate, BinOp, CmpOp, ColRef, Expr, Having, HavingLeaf, Literal, OrderKey, OrderTarget,
    Predicate, Query, ScalarFn, SelectExpr, SelectItem,
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
                .map(|(_, expr)| eval_expr(record, expr))
                .collect();
            (record, row)
        })
        .collect();

    if q.distinct {
        dedup_rows(&mut rows);
    }

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

/// Drops duplicate projected rows in place for `SELECT DISTINCT`, keeping
/// each row's first occurrence.
///
/// A row is keyed on its cells' [`Value::to_cmp_string`] — the same
/// non-`Value` conversion `count(distinct col)` keys on, since `Value` has
/// no `Eq`/`Hash` — collected per-row rather than joined into one string, so
/// that e.g. cells `("ab", "c")` and `("a", "bc")` never collide.
fn dedup_rows(rows: &mut Vec<(&Record, Vec<Value>)>) {
    let mut seen: HashSet<Vec<String>> = HashSet::new();
    rows.retain(|(_, row)| {
        let key: Vec<String> = row.iter().map(Value::to_cmp_string).collect();
        seen.insert(key)
    });
}

/// The filter / group / aggregate / `HAVING` / order / limit pipeline for a
/// query with a `GROUP BY` clause and/or aggregate `SELECT` items.
///
/// With aggregates but no `GROUP BY`, every filtered row is treated as one
/// group (see [`group_rows`]). Group order is made deterministic by sorting
/// on the key tuple before `ORDER BY` is applied, so results are stable even
/// when the query has no explicit ordering. `HAVING`, when present, drops
/// groups after their `SELECT` row is projected but before `ORDER BY` /
/// `LIMIT` / `OFFSET` run — see [`eval_having`].
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
            (group, row)
        })
        .filter(|(group, _)| match &q.having {
            // SQL 3VL, same rule as WHERE: a group is kept only when HAVING
            // is definitely true; unknown/false both drop it.
            Some(having) => eval_having(having, group, &q.group_by) == Some(true),
            None => true,
        })
        .map(|(group, row)| (group.key, row))
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

/// A validated grouped `SELECT` item: SQL only allows projecting a
/// non-aggregate expression built entirely from grouping-key columns (and
/// literals), or an aggregate, once `GROUP BY` (or a bare aggregate) is in
/// play.
enum GroupedSelectItem {
    /// A non-aggregate expression, evaluated over the group's representative
    /// row (see [`project_group`]); every column it references is one of
    /// `group_by`'s keys, so any row in the group yields the same value.
    Expr(Expr),
    /// An aggregate to compute over the group's rows.
    Agg(Aggregate),
}

/// Validates `q.select` for the grouped path: every non-aggregate item's
/// expression must reference only columns that also appear in `q.group_by`
/// (a bare grouping-key column trivially satisfies this, and so does a
/// column-free literal/computed expression); anything else — a column
/// outside the grouping keys, or `*` — is rejected, since neither reduces to
/// a single value per group.
fn validate_grouped_select(q: &Query) -> Result<Vec<GroupedSelectItem>, ExecError> {
    q.select
        .iter()
        .map(|item| match &item.expr {
            SelectExpr::Agg(agg) => Ok(GroupedSelectItem::Agg(agg.clone())),
            SelectExpr::Expr(expr) => {
                if expr_columns(expr)
                    .into_iter()
                    .all(|col| q.group_by.contains(col))
                {
                    Ok(GroupedSelectItem::Expr(expr.clone()))
                } else {
                    Err(ExecError::NonGroupedColumn(item.header()))
                }
            }
            SelectExpr::Star => Err(ExecError::NonGroupedColumn(item.header())),
        })
        .collect()
}

/// Every column (frontmatter field or `file.*`) that `expr` references,
/// walking scalar-function arguments and binary operands. Used by
/// [`validate_grouped_select`] to check that a non-aggregate `SELECT`
/// expression is composed entirely of grouping-key columns.
fn expr_columns(expr: &Expr) -> Vec<&ColRef> {
    match expr {
        Expr::Col(col) => vec![col],
        Expr::Lit(_) => Vec::new(),
        Expr::Scalar(_, args) => args.iter().flat_map(expr_columns).collect(),
        Expr::Binary(_, l, r) => expr_columns(l).into_iter().chain(expr_columns(r)).collect(),
    }
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

/// Projects one group's row: a non-aggregate expression is evaluated over
/// the group's representative row, an aggregate is computed over the
/// group's records.
fn project_group(group: &Group<'_>, items: &[GroupedSelectItem]) -> Vec<Value> {
    items
        .iter()
        .map(|item| match item {
            GroupedSelectItem::Expr(expr) => eval_group_expr(&group.rows, expr),
            GroupedSelectItem::Agg(agg) => compute_aggregate(agg, &group.rows),
        })
        .collect()
}

/// Evaluates a validated grouped-`SELECT` expression against the group's
/// representative row (its first record). `rows` is empty only for the
/// zero-row "aggregate over nothing" bucket (empty `GROUP BY`, no matching
/// records — see [`group_rows`]); [`validate_grouped_select`] guarantees a
/// `SELECT` expression surviving that bucket references no columns, so
/// evaluating it against a fieldless stand-in record is safe.
fn eval_group_expr(rows: &[&Record], expr: &Expr) -> Value {
    match rows.first() {
        Some(record) => eval_expr(record, expr),
        None => eval_expr(&empty_record(), expr),
    }
}

/// A fieldless record with no `file.*` identity, used only as the
/// evaluation context in [`eval_group_expr`]'s zero-row fallback.
fn empty_record() -> Record {
    Record::new(Path::new(""), Path::new(""), IndexMap::new())
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

/// Evaluates a `HAVING` predicate tree against one group under SQL
/// three-valued logic (3VL), mirroring [`eval_predicate`]'s handling for
/// `WHERE`. `group_by` is `q.group_by`, needed to resolve a
/// [`HavingLeaf::Group`] leaf's value from the group's key tuple by
/// position — see [`eval_having_leaf`].
fn eval_having(having: &Having, group: &Group<'_>, group_by: &[ColRef]) -> Option<bool> {
    match having {
        Having::Compare(leaf, op, lit) => {
            let value = eval_having_leaf(leaf, group, group_by);
            eval_compare(&value, op, &literal_value(lit))
        }
        Having::And(a, b) => three_valued_and(
            eval_having(a, group, group_by),
            eval_having(b, group, group_by),
        ),
        Having::Or(a, b) => three_valued_or(
            eval_having(a, group, group_by),
            eval_having(b, group, group_by),
        ),
        Having::Not(inner) => three_valued_not(eval_having(inner, group, group_by)),
    }
}

/// Resolves a `HAVING` comparison leaf's value: an aggregate is computed
/// fresh from the group's rows (it need not appear in `SELECT` — standard
/// SQL allows `HAVING` to reference an unselected aggregate); a grouping-key
/// leaf is read from the group's key tuple, at the position `col` occupies in
/// `group_by`. `parse::lower_having` guarantees every [`HavingLeaf::Group`]
/// is one of `group_by`'s keys, so the position lookup always succeeds for a
/// query built by the parser; `Value::Null` is a defensive fallback only
/// reachable from a hand-built AST that bypasses that guarantee.
fn eval_having_leaf(leaf: &HavingLeaf, group: &Group<'_>, group_by: &[ColRef]) -> Value {
    match leaf {
        HavingLeaf::Agg(agg) => compute_aggregate(agg, &group.rows),
        HavingLeaf::Group(col) => group_by
            .iter()
            .position(|g| g == col)
            .map(|idx| group.key[idx].clone())
            .unwrap_or(Value::Null),
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
        expr: SelectExpr::Expr(Expr::Col(col.clone())),
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

/// Expands `q.select` into a flat list of `(header, expr)` pairs, resolving
/// `SelectExpr::Star` to the sorted union of `filtered`'s field names (each
/// becoming a bare `Expr::Col`).
///
/// Aggregate select items cannot appear here: [`execute`] routes queries
/// containing one to the grouped path before this function runs.
fn expand_select(q: &Query, filtered: &[&Record]) -> Vec<(String, Expr)> {
    let mut columns = Vec::with_capacity(q.select.len());
    for item in &q.select {
        match &item.expr {
            SelectExpr::Star => {
                for name in sorted_field_union(filtered) {
                    columns.push((name.clone(), Expr::Col(ColRef::Field(name))));
                }
            }
            SelectExpr::Expr(expr) => columns.push((item.header(), expr.clone())),
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

/// Evaluates a scalar expression against `record`: a column/`file.*`
/// pseudo-column resolves via [`resolve_col`], a literal evaluates to its
/// `Value`, a scalar-function call evaluates its arguments first then
/// applies [`apply_scalar`], and a binary op evaluates both sides then
/// applies [`apply_binary`]. Used by both the ungrouped projection (per row,
/// [`expand_select`]) and the grouped projection (over a group's
/// representative row, [`eval_group_expr`]).
pub(crate) fn eval_expr(record: &Record, expr: &Expr) -> Value {
    match expr {
        Expr::Col(col) => resolve_col(record, col),
        Expr::Lit(lit) => literal_value(lit),
        Expr::Scalar(f, args) => {
            let values: Vec<Value> = args.iter().map(|arg| eval_expr(record, arg)).collect();
            apply_scalar(f.clone(), &values)
        }
        Expr::Binary(op, left, right) => {
            let l = eval_expr(record, left);
            let r = eval_expr(record, right);
            apply_binary(op.clone(), &l, &r)
        }
    }
}

/// Converts a literal constant to the `Value` it evaluates to.
fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Str(s) => Value::Str(s.clone()),
        Literal::Int(i) => Value::Int(*i),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
    }
}

/// Applies a scalar string function to its already-evaluated arguments.
///
/// A non-string argument stringifies via [`Value::display`] first — the same
/// conversion the renderer uses — so a field renders identically whether
/// selected bare or wrapped in e.g. `lower(...)`. Any `Null` argument
/// short-circuits the whole call to `Null` (SQL-standard scalar null
/// propagation). Argument count is validated at parse time
/// (`parse::check_scalar_arity`), so a mismatch reaching here can only come
/// from an in-crate caller building a bad-arity `Expr::Scalar` directly —
/// that falls back to `Null` rather than panicking.
pub(crate) fn apply_scalar(f: ScalarFn, args: &[Value]) -> Value {
    if args.iter().any(Value::is_null) {
        return Value::Null;
    }
    match (f, args) {
        (ScalarFn::Lower, [s]) => Value::Str(s.display().to_lowercase()),
        (ScalarFn::Upper, [s]) => Value::Str(s.display().to_uppercase()),
        (ScalarFn::Length, [s]) => Value::Int(s.display().chars().count() as i64),
        (ScalarFn::Trim, [s]) => Value::Str(s.display().trim().to_string()),
        (ScalarFn::Ltrim, [s]) => Value::Str(s.display().trim_start().to_string()),
        (ScalarFn::Rtrim, [s]) => Value::Str(s.display().trim_end().to_string()),
        (ScalarFn::Substr, [s, start]) => substr(&s.display(), start, None),
        (ScalarFn::Substr, [s, start, len]) => substr(&s.display(), start, Some(len)),
        (ScalarFn::Replace, [s, from, to]) => {
            Value::Str(s.display().replace(&from.display(), &to.display()))
        }
        // Wrong arity can't come from the parser (see doc comment above);
        // return `Null` rather than panicking on a hand-built bad call.
        _ => Value::Null,
    }
}

/// `substr(s, start[, len])`: 1-based, char-indexed, clamped to `s`'s
/// bounds; a non-numeric `start` or `len` yields `""`. A `start` at or
/// before the first character clamps to it (e.g. `substr("ab", -2)` is
/// `"ab"`, not `""`); only a `start` past the last character yields `""`.
///
/// All index arithmetic is saturating rather than raw `-`/`+`, so a
/// wildly out-of-`i64`/`usize`-range `start` or `len` (e.g. a numeric
/// literal like `-99999999999999999999`) clamps to the nearest in-bounds
/// index instead of overflowing.
fn substr(s: &str, start: &Value, len: Option<&Value>) -> Value {
    let Some(start) = start.as_number() else {
        return Value::Str(String::new());
    };
    let chars: Vec<char> = s.chars().collect();
    // `start as i64` already saturates for an out-of-range `f64` (including
    // NaN, which casts to 0); `saturating_sub` then keeps the following
    // "make 1-based 0-based" step from overflowing at `i64::MIN`.
    let from = (start as i64).saturating_sub(1).max(0) as usize;
    if from >= chars.len() {
        return Value::Str(String::new());
    }
    let to = match len {
        None => chars.len(),
        Some(len) => match len.as_number() {
            Some(n) => from.saturating_add(n.max(0.0) as usize).min(chars.len()),
            None => from,
        },
    };
    Value::Str(chars[from..to.max(from)].iter().collect())
}

/// Applies an arithmetic or concatenation operator to two already-evaluated
/// operands, under SQL 3-valued null propagation.
///
/// **Concat** (`||`) stringifies each operand via [`Value::display`] — the
/// same conversion the renderer uses — and joins them; either operand
/// `Null` yields `Null`.
///
/// **Arithmetic** (`+ - * / %`) coerces both operands to numbers the same
/// way [`eval_compare`]'s numeric-literal comparison does; a non-numeric
/// operand yields `Null` rather than a type error, and so does either
/// operand being `Null`. `/` always promotes to `Float`. For `+ - * %`, the
/// result is `Float` when either operand is a `Value::Float(_)` *or* either
/// operand's coerced number is non-integral (e.g. a quoted numeric field
/// like `"3.5"`) — otherwise it stays `Int`. Divide/modulo by zero yields
/// `Null` rather than panicking.
pub(crate) fn apply_binary(op: BinOp, l: &Value, r: &Value) -> Value {
    if op == BinOp::Concat {
        return match (l.is_null(), r.is_null()) {
            (false, false) => Value::Str(format!("{}{}", l.display(), r.display())),
            _ => Value::Null,
        };
    }
    if l.is_null() || r.is_null() {
        return Value::Null;
    }
    let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) else {
        return Value::Null;
    };
    // The result type is decided from the coerced numbers, not the
    // pre-coercion `Value` tag alone: a quoted-numeric string operand
    // (`Value::Str`) with a fractional value must still promote the
    // result to `Float`, or a fractional result silently truncates.
    let is_float = matches!(l, Value::Float(_))
        || matches!(r, Value::Float(_))
        || lv.fract() != 0.0
        || rv.fract() != 0.0;
    match op {
        BinOp::Add => numeric_result(lv + rv, is_float),
        BinOp::Sub => numeric_result(lv - rv, is_float),
        BinOp::Mul => numeric_result(lv * rv, is_float),
        BinOp::Div if rv == 0.0 => Value::Null,
        BinOp::Div => Value::Float(lv / rv),
        BinOp::Mod if rv == 0.0 => Value::Null,
        BinOp::Mod => numeric_result(lv % rv, is_float),
        BinOp::Concat => unreachable!("apply_binary routes Concat separately, above"),
    }
}

/// Wraps an arithmetic result as `Float` when the caller determined either
/// operand was a `Float` or non-integral (see [`apply_binary`]), else `Int`.
fn numeric_result(v: f64, is_float: bool) -> Value {
    if is_float {
        Value::Float(v)
    } else {
        Value::Int(v as i64)
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
        Predicate::Compare(left, op, right) => {
            eval_compare(&eval_expr(record, left), op, &eval_expr(record, right))
        }
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
            let base = Some(literals.iter().any(|lit| element_equals(&value, lit)));
            maybe_negate(base, *negated)
        }
        Predicate::MemberOf(lit, col, negated) => {
            let value = resolve_col(record, col);
            let Value::List(items) = &value else {
                // Unknown for both a `Null` field and a non-list value —
                // never a hard `false` — mirroring `In`'s null-column rule.
                return None;
            };
            let base = Some(items.iter().any(|el| element_equals(el, lit)));
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

/// The element-equality shared by `IN (...)` (comparing the column's value
/// against each candidate literal) and `MEMBER OF(...)` (comparing each list
/// element against the target literal): `true` when `value` equals `lit`
/// under [`eval_compare`]'s `Eq` rule.
fn element_equals(value: &Value, lit: &Literal) -> bool {
    eval_compare(value, &CmpOp::Eq, &literal_value(lit)) == Some(true)
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

/// Compares `left` against `right` under 3VL, both already evaluated via
/// [`eval_expr`], using the same symmetric numeric-else-lexicographic rule
/// as [`compare_values`] — so a query and its operand-swapped equivalent
/// (`a op b` vs `b op' a`) always agree, regardless of which side holds a
/// numeric-looking string (e.g. frontmatter `n: "9"`). Now that both sides
/// of a comparison can be arbitrary expressions, dispatching the coercion
/// off of only one side's `Value` variant — as if it were still always the
/// literal — would make the result depend on operand order.
///
/// `Null` on either side yields `None` (unknown) — the only source of
/// unknown here. Otherwise the result is always `Some(_)`: when both sides
/// coerce to a number they compare numerically; otherwise they compare
/// lexicographically on `to_cmp_string()`, so a non-numeric operand against
/// a numeric one fails the predicate as `Some(false)` rather than being
/// unknown, per the spec's "the row fails the predicate".
fn eval_compare(left: &Value, op: &CmpOp, right: &Value) -> Option<bool> {
    if left.is_null() || right.is_null() {
        return None;
    }
    Some(compare_values(left, right).is_some_and(|ord| apply_cmp(op, ord)))
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
    use crate::query::ast::{BinOp, ScalarFn};
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
    fn scalar_string_functions() {
        let s = |t: &str| Value::Str(t.into());
        assert_eq!(apply_scalar(ScalarFn::Lower, &[s("DrAfT")]), s("draft"));
        assert_eq!(apply_scalar(ScalarFn::Upper, &[s("draft")]), s("DRAFT"));
        assert_eq!(apply_scalar(ScalarFn::Length, &[s("héllo")]), Value::Int(5));
        assert_eq!(apply_scalar(ScalarFn::Trim, &[s("  x  ")]), s("x"));
        assert_eq!(apply_scalar(ScalarFn::Ltrim, &[s("  x  ")]), s("x  "));
        assert_eq!(apply_scalar(ScalarFn::Rtrim, &[s("  x  ")]), s("  x"));
        assert_eq!(
            apply_scalar(
                ScalarFn::Substr,
                &[s("abcdef"), Value::Int(2), Value::Int(3)]
            ),
            s("bcd")
        );
        assert_eq!(
            apply_scalar(ScalarFn::Substr, &[s("abcdef"), Value::Int(4)]),
            s("def")
        );
        assert_eq!(
            apply_scalar(ScalarFn::Replace, &[s("a-b-c"), s("-"), s("_")]),
            s("a_b_c")
        );
    }

    #[test]
    fn scalar_null_propagates_and_stringifies_numbers() {
        assert_eq!(apply_scalar(ScalarFn::Lower, &[Value::Null]), Value::Null);
        // a non-string arg stringifies first (same conversion the renderer uses)
        assert_eq!(
            apply_scalar(ScalarFn::Length, &[Value::Int(100)]),
            Value::Int(3)
        );
    }

    #[test]
    fn substr_clamps_out_of_range() {
        let s = |t: &str| Value::Str(t.into());
        assert_eq!(
            apply_scalar(ScalarFn::Substr, &[s("abc"), Value::Int(10)]),
            s("")
        );
        assert_eq!(
            apply_scalar(ScalarFn::Substr, &[s("abc"), Value::Int(1), Value::Int(99)]),
            s("abc")
        );
    }

    #[test]
    fn substr_negative_or_zero_start_clamps_to_full_string() {
        // A start at or before the first character clamps to it — it does
        // NOT yield "" (only a start past the last character does; see
        // `substr_clamps_out_of_range` above).
        let s = |t: &str| Value::Str(t.into());
        assert_eq!(
            apply_scalar(ScalarFn::Substr, &[s("ab"), Value::Int(-2)]),
            s("ab")
        );
        assert_eq!(
            apply_scalar(ScalarFn::Substr, &[s("ab"), Value::Int(0)]),
            s("ab")
        );
    }

    #[test]
    fn substr_overflow_safe_for_extreme_start_and_len() {
        // Regression: an over-long numeric literal used to panic via a raw
        // `-`/`+` on the saturated-but-extreme `i64`/`usize` index. All
        // three cases must clamp to an in-bounds substring, never panic.
        let s = |t: &str| Value::Str(t.into());

        // Huge negative start: clamps to the first character.
        assert_eq!(
            apply_scalar(
                ScalarFn::Substr,
                &[s("hello"), Value::Float(-99999999999999999999.0)]
            ),
            s("hello")
        );
        // Huge positive len: clamps to the end of the string.
        assert_eq!(
            apply_scalar(
                ScalarFn::Substr,
                &[
                    s("hello"),
                    Value::Int(2),
                    Value::Float(99999999999999999999.0)
                ]
            ),
            s("ello")
        );
        // Huge positive start: past the end, yields "".
        assert_eq!(
            apply_scalar(
                ScalarFn::Substr,
                &[s("hello"), Value::Float(99999999999999999999.0)]
            ),
            s("")
        );
    }

    #[test]
    fn substr_select_survives_extreme_literals_without_panicking() {
        // Same regression as `substr_overflow_safe_for_extreme_start_and_len`,
        // pinned at the actual seam the bug was reported against: a real SQL
        // literal parsed and executed end-to-end, not just the raw `Value`.
        let rows = [rec("s", "s/a.md", &[("name", Value::Str("hello".into()))])];

        let neg_start = parse("SELECT substr(name, -99999999999999999999)").unwrap();
        assert_eq!(
            execute(&neg_start, rows.iter()).unwrap().rows,
            vec![vec![Value::Str("hello".into())]]
        );

        let huge_len = parse("SELECT substr(name, 2, 99999999999999999999)").unwrap();
        assert_eq!(
            execute(&huge_len, rows.iter()).unwrap().rows,
            vec![vec![Value::Str("ello".into())]]
        );

        let huge_start = parse("SELECT substr(name, 99999999999999999999)").unwrap();
        assert_eq!(
            execute(&huge_start, rows.iter()).unwrap().rows,
            vec![vec![Value::Str("".into())]]
        );
    }

    #[test]
    fn apply_scalar_wrong_arity_returns_null_not_panic() {
        // apply_scalar is pub(crate); an in-crate caller hand-building a
        // bad-arity Expr::Scalar must get Null, not a panic (the parser
        // still rejects wrong arity up front for real queries).
        assert_eq!(apply_scalar(ScalarFn::Lower, &[]), Value::Null);
    }

    #[test]
    fn arithmetic_types_and_null_safety() {
        assert_eq!(
            apply_binary(BinOp::Add, &Value::Int(2), &Value::Int(3)),
            Value::Int(5)
        );
        assert_eq!(
            apply_binary(BinOp::Div, &Value::Int(3), &Value::Int(2)),
            Value::Float(1.5)
        );
        assert_eq!(
            apply_binary(BinOp::Mul, &Value::Int(2), &Value::Float(1.5)),
            Value::Float(3.0)
        );
        assert_eq!(
            apply_binary(BinOp::Div, &Value::Int(1), &Value::Int(0)),
            Value::Null
        );
        assert_eq!(
            apply_binary(BinOp::Mod, &Value::Int(1), &Value::Int(0)),
            Value::Null
        );
        assert_eq!(
            apply_binary(BinOp::Add, &Value::Null, &Value::Int(1)),
            Value::Null
        );
        assert_eq!(
            apply_binary(BinOp::Add, &Value::Str("x".into()), &Value::Int(1)),
            Value::Null
        );
    }

    #[test]
    fn arithmetic_promotes_float_from_numeric_string_operand() {
        // Regression: promotion used to be keyed on the operand's `Value`
        // variant, so a quoted-numeric frontmatter field (`Value::Str`)
        // never tripped the Float promotion and a fractional result
        // truncated. It must now be keyed on the coerced number itself.
        assert_eq!(
            apply_binary(BinOp::Add, &Value::Str("3.5".into()), &Value::Int(1)),
            Value::Float(4.5)
        );
        // A numeric string with an integral value still stays `Int`.
        assert_eq!(
            apply_binary(BinOp::Add, &Value::Str("4".into()), &Value::Int(1)),
            Value::Int(5)
        );
    }

    #[test]
    fn concat_joins_and_propagates_null() {
        assert_eq!(
            apply_binary(BinOp::Concat, &Value::Str("a".into()), &Value::Int(1)),
            Value::Str("a1".into())
        );
        assert_eq!(
            apply_binary(BinOp::Concat, &Value::Str("a".into()), &Value::Null),
            Value::Null
        );
    }

    #[test]
    fn select_scalar_and_arithmetic_round_trip() {
        let rows = [rec(
            "s",
            "s/a.md",
            &[
                ("status", Value::Str("Draft".into())),
                ("a", Value::Int(3)),
                ("b", Value::Int(2)),
            ],
        )];

        let lower = parse("SELECT lower(status)").unwrap();
        assert_eq!(
            execute(&lower, rows.iter()).unwrap().rows,
            vec![vec![Value::Str("draft".into())]]
        );

        let div = parse("SELECT (a / b) AS r").unwrap();
        assert_eq!(
            execute(&div, rows.iter()).unwrap().rows,
            vec![vec![Value::Float(1.5)]]
        );

        let concat = parse("SELECT a || '-' || status").unwrap();
        assert_eq!(
            execute(&concat, rows.iter()).unwrap().rows,
            vec![vec![Value::Str("3-Draft".into())]]
        );
    }

    #[test]
    fn select_arithmetic_promotes_float_from_numeric_string_field() {
        // Same regression as `arithmetic_promotes_float_from_numeric_string_operand`,
        // pinned end-to-end: a quoted-numeric frontmatter field (`Value::Str`,
        // as `frontmatter.rs::pod_to_value` produces for `x: "3.5"`) must
        // still promote `x + 1` to a `Float`, not truncate it.
        let rows = [rec("s", "s/a.md", &[("x", Value::Str("3.5".into()))])];
        let q = parse("SELECT x + 1").unwrap();
        assert_eq!(
            execute(&q, rows.iter()).unwrap().rows,
            vec![vec![Value::Float(4.5)]]
        );
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
    fn distinct_dedups_projection() {
        let rows = [
            rec("s", "s/a/1.md", &[]),
            rec("s", "s/a/2.md", &[]),
            rec("s", "s/b/3.md", &[]),
        ];
        let q = parse("SELECT DISTINCT file.folder").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        assert_eq!(
            t.rows,
            vec![vec![Value::Str("a".into())], vec![Value::Str("b".into())]]
        );
    }
    #[test]
    fn distinct_collapses_rows_that_differ_only_outside_the_projection() {
        // b and c both have status "synced" but differ in `prd`/folder —
        // DISTINCT keys on the final *projected* cells only, so they still
        // collapse to a single row.
        let q = parse("SELECT DISTINCT status").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("draft".into())],
                vec![Value::Str("synced".into())],
            ]
        );
    }
    #[test]
    fn distinct_dedup_uses_scan_order_before_order_by_resolves_ties() {
        // Regression pin for pipeline order: dedup must run before ORDER BY,
        // keeping each key's *first-scanned* row, even when ORDER BY sorts
        // by a column outside the SELECT list. Sorting first would instead
        // let whichever duplicate sorts smallest win the tie, changing which
        // row's non-projected columns decide the final order.
        let rows = [
            rec(
                "s",
                "s/a.md",
                &[("status", Value::Str("A".into())), ("prd", Value::Int(9))],
            ),
            rec(
                "s",
                "s/a2.md",
                &[("status", Value::Str("A".into())), ("prd", Value::Int(1))],
            ),
            rec(
                "s",
                "s/b.md",
                &[("status", Value::Str("B".into())), ("prd", Value::Int(5))],
            ),
        ];
        let q = parse("SELECT DISTINCT status ORDER BY prd").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        assert_eq!(
            t.rows,
            vec![vec![Value::Str("B".into())], vec![Value::Str("A".into())]]
        );
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
    #[test]
    fn member_of_list_field() {
        // `a.md` carries a list `tags`; `b.md` has no `tags` at all (resolves
        // to `Value::Null`); `c.md`'s `tags` is a bare string, not a list.
        let rows = [
            rec(
                "s",
                "s/a.md",
                &[(
                    "tags",
                    Value::List(vec![
                        Value::Str("mobile".into()),
                        Value::Str("backend".into()),
                    ]),
                )],
            ),
            rec("s", "s/b.md", &[]),
            rec("s", "s/c.md", &[("tags", Value::Str("x".into()))]),
        ];

        // A member of the list matches; the Null and non-list rows are
        // unknown, not false, so they're excluded either way.
        let present = parse("SELECT file.name WHERE 'mobile' MEMBER OF(tags)").unwrap();
        assert_eq!(
            execute(&present, rows.iter()).unwrap().rows,
            vec![vec![Value::Str("a.md".into())]]
        );

        // A non-member yields no match on the list row either.
        let absent = parse("SELECT file.name WHERE 'ios' MEMBER OF(tags)").unwrap();
        assert!(execute(&absent, rows.iter()).unwrap().rows.is_empty());

        // `NOT <lit> MEMBER OF(col)` (sqlparser 0.62 only accepts the prefix
        // NOT form, not `col NOT MEMBER OF(...)`) flips the list row to a
        // match, but negating unknown stays unknown — the Null/non-list rows
        // must NOT be resurrected.
        let negated = parse("SELECT file.name WHERE NOT 'ios' MEMBER OF(tags)").unwrap();
        assert_eq!(
            execute(&negated, rows.iter()).unwrap().rows,
            vec![vec![Value::Str("a.md".into())]]
        );
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
    fn where_comparison_is_commutative_for_numeric_string_field() {
        // Critical fix: `eval_compare` used to pick its coercion rule off the
        // RIGHT operand's `Value` variant alone, so a numeric-looking string
        // field (`n: "9"` — a real pattern in this codebase, e.g. `prd:
        // '010'`) compared differently depending on which side it was
        // written on. All four phrasings below describe the same fact
        // (9 < 10) and must now agree.
        let rows = [rec("s", "s/a.md", &[("n", Value::Str("9".into()))])];

        let lt = parse("SELECT n WHERE n < 10").unwrap();
        assert_eq!(
            execute(&lt, rows.iter()).unwrap().rows,
            vec![vec![Value::Str("9".into())]],
            "n < 10"
        );

        let gt_flipped = parse("SELECT n WHERE 10 > n").unwrap();
        assert_eq!(
            execute(&gt_flipped, rows.iter()).unwrap().rows,
            vec![vec![Value::Str("9".into())]],
            "10 > n must agree with n < 10"
        );

        let gt = parse("SELECT n WHERE n > 10").unwrap();
        assert!(
            execute(&gt, rows.iter()).unwrap().rows.is_empty(),
            "n > 10 is false"
        );

        let lt_flipped = parse("SELECT n WHERE 10 < n").unwrap();
        assert!(
            execute(&lt_flipped, rows.iter()).unwrap().rows.is_empty(),
            "10 < n must agree with n > 10"
        );
    }

    #[test]
    fn where_equality_is_commutative_for_numeric_string_and_non_numeric_field() {
        // Same bug, `=` form: a numeric-looking string ("5.0") must compare
        // equal to Int(5) from either side, while a genuinely non-numeric
        // string ("draft") must still fail the predicate from either side —
        // not flip to a match just because the literal moved to the left.
        let numeric = [rec("s", "s/a.md", &[("status", Value::Str("5.0".into()))])];
        let eq = parse("SELECT status WHERE status = 5").unwrap();
        assert_eq!(
            execute(&eq, numeric.iter()).unwrap().rows,
            vec![vec![Value::Str("5.0".into())]]
        );
        let eq_flipped = parse("SELECT status WHERE 5 = status").unwrap();
        assert_eq!(
            execute(&eq_flipped, numeric.iter()).unwrap().rows,
            vec![vec![Value::Str("5.0".into())]],
            "5 = status must agree with status = 5"
        );

        let non_numeric = [rec(
            "s",
            "s/b.md",
            &[("status", Value::Str("draft".into()))],
        )];
        let eq2 = parse("SELECT status WHERE status = 5").unwrap();
        assert!(execute(&eq2, non_numeric.iter()).unwrap().rows.is_empty());
        let eq2_flipped = parse("SELECT status WHERE 5 = status").unwrap();
        assert!(
            execute(&eq2_flipped, non_numeric.iter())
                .unwrap()
                .rows
                .is_empty(),
            "5 = status must still fail (not unknown) against a non-numeric field"
        );
    }

    #[test]
    fn where_column_to_column_and_scalar() {
        // Column-to-column: `start < end` matches only the row where it holds.
        let bounds = [
            rec(
                "s",
                "s/a.md",
                &[("start", Value::Int(1)), ("end", Value::Int(5))],
            ),
            rec(
                "s",
                "s/b.md",
                &[("start", Value::Int(5)), ("end", Value::Int(5))],
            ),
        ];
        let cmp = parse("SELECT start WHERE start < end").unwrap();
        assert_eq!(
            execute(&cmp, bounds.iter()).unwrap().rows,
            vec![vec![Value::Int(1)]]
        );

        // A scalar expression on the left, a literal on the right.
        let draft = [rec(
            "s",
            "s/c.md",
            &[("status", Value::Str("Draft".into()))],
        )];
        let scalar = parse("SELECT status WHERE lower(status) = 'draft'").unwrap();
        assert_eq!(
            execute(&scalar, draft.iter()).unwrap().rows,
            vec![vec![Value::Str("Draft".into())]]
        );

        // Arithmetic on the left, a column on the right.
        let arith_rows = [rec(
            "s",
            "s/d.md",
            &[("start", Value::Int(4)), ("end", Value::Int(5))],
        )];
        let arith = parse("SELECT start WHERE start + 1 = end").unwrap();
        assert_eq!(
            execute(&arith, arith_rows.iter()).unwrap().rows,
            vec![vec![Value::Int(4)]]
        );
    }

    #[test]
    fn where_null_operand_is_unknown_not_match() {
        // `missing` is absent from every record below, so it resolves to
        // `Value::Null`; comparing it against `status` (present on both
        // rows) must be unknown — not a match, and not an error — the same
        // 3VL rule a NULL field already follows on the literal-comparison
        // side (column validation for a genuinely unknown field is T9).
        //
        // A plain `WHERE` can't tell unknown (`None`) apart from `Some(false)`
        // — both yield zero rows — so this also checks the negated form:
        // `missing = status` is unknown, `NOT unknown` stays unknown under
        // 3VL, and an unknown `WHERE` still excludes every row. If
        // `eval_compare` wrongly returned `Some(false)` for a null operand,
        // `NOT false` = `true` would wrongly INCLUDE both rows here — so this
        // distinguishes the two where the un-negated form alone cannot.
        let rows = [
            rec("s", "s/a.md", &[("status", Value::Str("draft".into()))]),
            rec("s", "s/b.md", &[("status", Value::Str("synced".into()))]),
        ];
        let q = parse("SELECT status WHERE missing = status").unwrap();
        assert!(execute(&q, rows.iter()).unwrap().rows.is_empty());

        let negated = parse("SELECT status WHERE NOT (missing = status)").unwrap();
        assert!(
            execute(&negated, rows.iter()).unwrap().rows.is_empty(),
            "NOT (unknown) is still unknown, not true — must not resurrect the rows"
        );

        // Same check with the Null on the right-hand operand instead.
        let right_null = parse("SELECT status WHERE status = missing").unwrap();
        assert!(
            execute(&right_null, rows.iter()).unwrap().rows.is_empty(),
            "a right-side Null operand must also be unknown, not a hard match/no-match"
        );

        let right_null_negated = parse("SELECT status WHERE NOT (status = missing)").unwrap();
        assert!(
            execute(&right_null_negated, rows.iter())
                .unwrap()
                .rows
                .is_empty(),
            "NOT (unknown) on a right-side Null must also stay unknown"
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
    fn grouped_select_expr_over_grouping_key() {
        // `lower(status)` is valid because `status` is a GROUP BY key;
        // it's evaluated over each group's representative row.
        let q =
            parse("SELECT lower(status), count(*) AS n GROUP BY status ORDER BY status").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("draft".into()), Value::Int(1)],
                vec![Value::Str("synced".into()), Value::Int(2)],
            ]
        );
    }
    #[test]
    fn grouped_select_expr_referencing_non_group_key_errors() {
        // `prd` isn't a GROUP BY key; wrapping it in `lower(...)` must still
        // be rejected — validation walks into scalar/arithmetic
        // sub-expressions, not just bare columns.
        let q = parse("SELECT lower(prd), count(*) GROUP BY status").unwrap();
        assert!(matches!(
            execute(&q, recs().iter()),
            Err(ExecError::NonGroupedColumn(_))
        ));
    }
    #[test]
    fn grouped_literal_expr_survives_zero_row_aggregate_bucket() {
        // A columnless computed expression must still evaluate correctly
        // even when the implicit single group (empty GROUP BY) has zero
        // rows to use as a representative.
        let q = parse("SELECT 1 + 1 AS two, count(*) AS n WHERE status = 'nope'").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(2), Value::Int(0)]]);
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

    #[test]
    fn having_filters_groups() {
        // status: draft x1, synced x2 (see `recs()`); only the synced group
        // clears `count(*) > 1`.
        let q = parse(
            "SELECT status, count(*) AS n GROUP BY status HAVING count(*) > 1 ORDER BY status",
        )
        .unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(
            t.rows,
            vec![vec![Value::Str("synced".into()), Value::Int(2)]]
        );
    }
    #[test]
    fn having_can_reference_aggregate_not_selected() {
        // `count(*)` never appears in SELECT, only in HAVING — standard SQL
        // still allows filtering on an aggregate that isn't projected.
        let q = parse("SELECT status GROUP BY status HAVING count(*) > 1").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("synced".into())]]);
    }
    #[test]
    fn having_can_reference_a_grouping_key() {
        let q =
            parse("SELECT status, count(*) AS n GROUP BY status HAVING status = 'draft'").unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(
            t.rows,
            vec![vec![Value::Str("draft".into()), Value::Int(1)]]
        );
    }
    #[test]
    fn having_and_or_not_combination() {
        // Both groups pass `count(*) >= 1`; only `draft` also passes
        // `NOT (count(*) > 1)` — pins that the boolean connectives evaluate
        // per-group exactly like WHERE's.
        let q = parse(
            "SELECT status, count(*) AS n GROUP BY status HAVING count(*) >= 1 AND NOT (count(*) > 1) ORDER BY status",
        )
        .unwrap();
        let t = execute(&q, recs().iter()).unwrap();
        assert_eq!(
            t.rows,
            vec![vec![Value::Str("draft".into()), Value::Int(1)]]
        );
    }
    #[test]
    fn having_unknown_drops_the_group() {
        // `avg(n)` over a group with no numeric `n` values is `Value::Null`;
        // comparing NULL against a literal is unknown under 3VL, which drops
        // the group exactly like an unknown WHERE would drop a row — not a
        // hard non-match.
        let rows = [rec_n("s/a.md", "draft", Value::Str("n/a".into()))];
        let q = parse("SELECT status GROUP BY status HAVING avg(n) > 1").unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        assert!(t.rows.is_empty());
    }
    #[test]
    fn having_applies_before_order_and_limit() {
        // Three status groups; HAVING keeps only the two with more than one
        // row, and ORDER BY / LIMIT then apply to that already-filtered set
        // (a LIMIT 1 that counted the dropped group would return "y", not "x").
        let rows = [
            rec_n("s/a.md", "x", Value::Int(1)),
            rec_n("s/b.md", "x", Value::Int(1)),
            rec_n("s/c.md", "y", Value::Int(1)),
            rec_n("s/d.md", "y", Value::Int(1)),
            rec_n("s/e.md", "z", Value::Int(1)),
        ];
        let q = parse(
            "SELECT status, count(*) AS n GROUP BY status HAVING count(*) > 1 ORDER BY status LIMIT 1",
        )
        .unwrap();
        let t = execute(&q, rows.iter()).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("x".into()), Value::Int(2)]]);
    }
}
