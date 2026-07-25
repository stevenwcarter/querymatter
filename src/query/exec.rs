//! The query executor: filter / project / order / limit, plus `GROUP BY`,
//! aggregate functions, and `HAVING` group filtering.
//!
//! [`execute`] evaluates a parsed [`Query`] against a set of [`Record`]s and
//! produces a [`ResultTable`]. It dispatches between two pipelines: the
//! **non-grouped** path (no `GROUP BY`, no aggregate `SELECT` items) and the
//! **grouped/aggregate** path (a `GROUP BY` clause and/or an aggregate
//! `SELECT` item); see [`is_grouped_or_aggregate`] for the dispatch check.

use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use chrono::{DateTime, Duration, Months, NaiveDate, SecondsFormat, Utc};
use globset::Glob;
use indexmap::IndexMap;
use regex::Regex;

use crate::frontmatter;
use crate::model::{FileAttr, Record, Value, compare_values};
use crate::query::ResultTable;
use crate::query::ast::{
    Aggregate, BinOp, CmpOp, ColRef, DateUnit, Expr, Having, HavingLeaf, Literal, OrderKey,
    OrderTarget, Predicate, Query, RelDate, ScalarFn, SelectExpr, SelectItem,
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
    /// An `ORDER BY` aggregate target reached the ungrouped execution path.
    /// `parse::lower_order_expr` rejects this at parse time (an aggregate
    /// `ORDER BY` requires a non-empty `GROUP BY`, which always routes to
    /// [`execute_grouped`]), so this is unreachable for any [`Query`] built
    /// by [`crate::query::parse`]; it exists only so [`resolve_order_targets`]
    /// stays total for a hand-built `Query` that bypasses that guarantee.
    #[error("ORDER BY an aggregate requires GROUP BY")]
    AggregateOrderWithoutGroupBy,
    /// A `SELECT`/`WHERE`/`GROUP BY`/`ORDER BY`/`HAVING`/`MEMBER OF` column
    /// that isn't in the record set's schema — almost always a typo. Skipped
    /// entirely under `--lenient` (see [`execute`]), where an unknown column
    /// resolves to `Value::Null` instead, matching pre-validation behavior.
    #[error("unknown column `{name}`{}", suggestion_suffix(suggestion))]
    UnknownColumn {
        name: String,
        suggestion: Option<String>,
    },
    /// `file.body` was referenced but disk reads are disallowed for this
    /// query (`--force-cache`, design W56) — raised once, up front, rather
    /// than silently resolving every row's `file.body` to `Value::Null` (see
    /// [`references_body`]). Only raised in strict (non-`--lenient`) mode;
    /// `--lenient` instead resolves the same situation to per-row `Null` (see
    /// [`read_body`]), matching how an unknown column degrades under
    /// `--lenient` too.
    #[error(
        "file.body requires disk access, but --force-cache disables it for this query \
         (retry without --force-cache, or drop file.body from the query)"
    )]
    BodyUnavailable,
}

/// Executes `q` against `records`, returning the projected, filtered,
/// ordered, and limited result.
///
/// Validates against the sorted union of `records`' own field names — see
/// [`execute_with_schema`], which this delegates to. Suitable whenever every
/// record passed in still carries every field (direct unit tests below, and
/// any caller with an unpruned record set); a caller whose records may have
/// had field VALUES pruned by projection push-down (design W17 — see
/// [`crate::store`]) must call [`execute_with_schema`] instead, passing the
/// record store's own [`crate::store::RecordStore::schema`] (always the FULL
/// field-name union, regardless of pruning) rather than let this function
/// derive a schema from the (possibly narrowed) `records` it's given.
///
/// Always evaluates with disk reads allowed — every caller of this entry
/// point (unit tests, and callers with an unpruned record set) has no notion
/// of `--force-cache`; a caller that does must call [`execute_with_schema`]
/// instead, passing its own `disk_reads_allowed`.
pub fn execute<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
    lenient: bool,
) -> Result<ResultTable, ExecError> {
    let records: Vec<&Record> = records.collect();
    let schema = sorted_field_union(&records);
    execute_with_schema_at(
        q,
        records.into_iter(),
        &schema,
        lenient,
        true,
        SystemTime::now(),
    )
}

/// Like [`execute`], but validates against an explicit `schema` instead of
/// one derived from `records`' own field names.
///
/// Unless `lenient` is set, every column `q` references (see
/// [`Query::referenced_fields`]) is checked against `schema` before the
/// filter/project pipeline runs, so a typo'd column fails fast with a
/// suggestion rather than silently reading as `Null` throughout. An empty
/// `schema` skips this check: a fresh or empty vault (or one whose only
/// records have explicit-but-empty frontmatter, e.g. `---\n{}\n---`) has no
/// fields to check against, and must not fail every query on that account
/// alone.
///
/// `disk_reads_allowed` gates `file.body` (design W56): `false` under
/// `Freshness::ForceCache`, where the whole query fails fast with
/// [`ExecError::BodyUnavailable`] in strict mode when it references
/// `file.body` (see [`references_body`]), or resolves it to `Value::Null`
/// per row under `--lenient` (see [`read_body`]).
///
/// Dispatches on whether `q` is grouped/aggregate; see
/// [`is_grouped_or_aggregate`].
pub fn execute_with_schema<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
    schema: &[String],
    lenient: bool,
    disk_reads_allowed: bool,
) -> Result<ResultTable, ExecError> {
    execute_with_schema_at(
        q,
        records,
        schema,
        lenient,
        disk_reads_allowed,
        SystemTime::now(),
    )
}

/// Like [`execute_with_schema`], but resolves relative-date literals
/// (`'today'`, `'-7d'`, …) against an explicit `now` instead of the wall
/// clock.
///
/// This is the seam both public entry points ([`execute`] and
/// [`execute_with_schema`]) delegate through — the earliest point they
/// share — so a relative-date literal is resolved to a concrete
/// `Literal::Str` date (see [`rewrite_relative_dates`]) before column
/// validation and the filter/group pipeline ever run; `eval_expr` and
/// `eval_having` never see a `Literal::RelativeDate`. Tests call this
/// directly to pin resolution against a fixed instant.
fn execute_with_schema_at<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
    schema: &[String],
    lenient: bool,
    disk_reads_allowed: bool,
    now: SystemTime,
) -> Result<ResultTable, ExecError> {
    let mut resolved = q.clone();
    rewrite_relative_dates(&mut resolved, now);
    let q = &resolved;

    let records: Vec<&Record> = records.collect();
    if !lenient && !schema.is_empty() {
        validate_columns(q, schema)?;
    }
    // Design W56: a `file.body` reference under `--force-cache` can never
    // produce a real answer, so strict mode fails the whole query fast
    // here — one clear diagnostic — rather than letting every row resolve
    // it to a silent `Null` (which is still what happens under `--lenient`,
    // mirroring the unknown-column check just above).
    if !lenient && !disk_reads_allowed && references_body(q) {
        return Err(ExecError::BodyUnavailable);
    }
    if is_grouped_or_aggregate(q) {
        return execute_grouped(q, records.into_iter(), disk_reads_allowed);
    }
    execute_ungrouped(q, records.into_iter(), disk_reads_allowed)
}

/// Replaces every `Literal::RelativeDate` in `q` with the `Literal::Str`
/// ISO-8601 date/datetime it resolves to against `now` (see
/// [`resolve_reldate`]). Walks every literal position in the query tree —
/// `SELECT` expressions, the `WHERE` predicate (comparison operands, `IN`
/// lists, `MEMBER OF`'s literal), `ORDER BY`'s computed-expression target,
/// and `HAVING` — so no relative-date literal can reach evaluation
/// unresolved, regardless of where in the query it appears.
fn rewrite_relative_dates(q: &mut Query, now: SystemTime) {
    for item in &mut q.select {
        if let SelectExpr::Expr(expr) = &mut item.expr {
            rewrite_expr_literals(expr, now);
        }
    }
    if let Some(pred) = &mut q.filter {
        rewrite_predicate_literals(pred, now);
    }
    for key in &mut q.order_by {
        if let OrderTarget::Expr(expr) = &mut key.target {
            rewrite_expr_literals(expr, now);
        }
    }
    if let Some(having) = &mut q.having {
        rewrite_having_literals(having, now);
    }
}

/// Walks `expr`'s literal positions for [`rewrite_relative_dates`]: a bare
/// literal resolves directly, and a scalar/binary expression recurses into
/// its arguments/operands.
fn rewrite_expr_literals(expr: &mut Expr, now: SystemTime) {
    match expr {
        Expr::Lit(lit) => rewrite_literal(lit, now),
        Expr::Col(_) => {}
        Expr::Scalar(_, args) => {
            for arg in args {
                rewrite_expr_literals(arg, now);
            }
        }
        Expr::Binary(_, l, r) => {
            rewrite_expr_literals(l, now);
            rewrite_expr_literals(r, now);
        }
        Expr::Coalesce(args) => {
            for arg in args {
                rewrite_expr_literals(arg, now);
            }
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(op) = operand {
                rewrite_expr_literals(op, now);
            }
            for (cond, then) in whens {
                rewrite_expr_literals(cond, now);
                rewrite_expr_literals(then, now);
            }
            if let Some(e) = else_expr {
                rewrite_expr_literals(e, now);
            }
        }
        Expr::Predicate(pred) => rewrite_predicate_literals(pred, now),
    }
}

/// Walks a `WHERE` predicate tree's literal positions for
/// [`rewrite_relative_dates`]: both `Compare` operands, `Like`/`In`/`IsNull`'s
/// tested `Expr` (and every `IN` list element), `MemberOf`'s value `Expr`,
/// and `Regexp`'s `Expr` operand — all now equally general, so the widened
/// operands can carry a relative-date literal the same as `Compare` always
/// could. `Like`/`Regexp`'s pattern is a plain `String` (never a `Literal`),
/// so neither needs a rewrite of its own beyond the tested expression.
fn rewrite_predicate_literals(pred: &mut Predicate, now: SystemTime) {
    match pred {
        Predicate::Compare(l, _, r) => {
            rewrite_expr_literals(l, now);
            rewrite_expr_literals(r, now);
        }
        Predicate::Like(expr, _, _) | Predicate::IsNull(expr, _) => {
            rewrite_expr_literals(expr, now)
        }
        Predicate::In(expr, literals, _) => {
            rewrite_expr_literals(expr, now);
            for lit in literals {
                rewrite_literal(lit, now);
            }
        }
        Predicate::MemberOf(value, _, _) => rewrite_expr_literals(value, now),
        Predicate::Regexp(expr, _, _) => rewrite_expr_literals(expr, now),
        Predicate::And(a, b) | Predicate::Or(a, b) => {
            rewrite_predicate_literals(a, now);
            rewrite_predicate_literals(b, now);
        }
        Predicate::Not(inner) => rewrite_predicate_literals(inner, now),
    }
}

/// Walks a `HAVING` predicate tree's literal positions for
/// [`rewrite_relative_dates`].
fn rewrite_having_literals(having: &mut Having, now: SystemTime) {
    match having {
        Having::Compare(_, _, lit) => rewrite_literal(lit, now),
        Having::And(a, b) | Having::Or(a, b) => {
            rewrite_having_literals(a, now);
            rewrite_having_literals(b, now);
        }
        Having::Not(inner) => rewrite_having_literals(inner, now),
    }
}

/// Resolves `lit` in place when it's a `Literal::RelativeDate`; any other
/// literal is left untouched.
fn rewrite_literal(lit: &mut Literal, now: SystemTime) {
    if let Literal::RelativeDate(rd) = lit {
        *lit = Literal::Str(resolve_reldate(*rd, now));
    }
}

/// Resolves a [`RelDate`] to its concrete ISO-8601 rendering, anchored at
/// `now`. `today` and an offset resolve to a plain `%Y-%m-%d` date, so they
/// compare lexicographically against a frontmatter date field of the same
/// shape; `now` resolves to a full RFC3339 instant. `mo`/`y` offsets use
/// calendar-aware arithmetic ([`Months`]), since a fixed 30/365-day
/// [`Duration`] would drift across month/year boundaries; `d`/`w` use
/// [`Duration`] directly, since those units are already fixed-length.
fn resolve_reldate(rd: RelDate, now: SystemTime) -> String {
    let now_utc = DateTime::<Utc>::from(now);
    match rd {
        RelDate::Now => now_utc.to_rfc3339_opts(SecondsFormat::Secs, true),
        RelDate::Today => now_utc.date_naive().format("%Y-%m-%d").to_string(),
        RelDate::Offset { n, unit } => {
            let d = now_utc.date_naive();
            let shifted = match unit {
                DateUnit::Day => d + Duration::days(n),
                DateUnit::Week => d + Duration::weeks(n),
                DateUnit::Month if n >= 0 => d + Months::new(n as u32),
                DateUnit::Month => d - Months::new((-n) as u32),
                DateUnit::Year if n >= 0 => d + Months::new(12 * n as u32),
                DateUnit::Year => d - Months::new(12 * (-n) as u32),
            };
            shifted.format("%Y-%m-%d").to_string()
        }
    }
}

/// Checks every column `q.referenced_fields()` touches against `schema`,
/// failing on the first (sorted) name that isn't in it.
fn validate_columns(q: &Query, schema: &[String]) -> Result<(), ExecError> {
    for name in q.referenced_fields() {
        if !schema.contains(&name) {
            let suggestion = nearest(&name, schema);
            return Err(ExecError::UnknownColumn { name, suggestion });
        }
    }
    Ok(())
}

/// The schema field nearest to `name` by Levenshtein distance, when one is
/// close enough to plausibly be what a typo meant: within 2 edits, or within
/// a third of `name`'s length, whichever allows more slack. `None` when no
/// field clears that bar (or `schema` is empty).
fn nearest(name: &str, schema: &[String]) -> Option<String> {
    let len = name.chars().count();
    let threshold = len.div_ceil(3).max(2);
    schema
        .iter()
        .map(|field| (field, levenshtein(name, field)))
        .filter(|(_, dist)| *dist <= threshold)
        .min_by_key(|(_, dist)| *dist)
        .map(|(field, _)| field.clone())
}

/// The Levenshtein (edit) distance between `a` and `b`, char-wise. Used only
/// by [`nearest`] to suggest a schema field for a likely-typo'd column name,
/// so this is a plain textbook DP rather than a dependency — the crate has no
/// other use for a general string-distance metric.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// The `, did you mean '<x>'?` suffix for [`ExecError::UnknownColumn`]'s
/// `Display`, or an empty string when no schema field was close enough to
/// suggest.
fn suggestion_suffix(suggestion: &Option<String>) -> String {
    match suggestion {
        Some(name) => format!(", did you mean '{name}'?"),
        None => String::new(),
    }
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
    disk_reads_allowed: bool,
) -> Result<Vec<&'a Record>, ExecError> {
    let candidates = filter_by_glob(records.collect(), q.from_glob.as_deref())?;
    Ok(candidates
        .into_iter()
        .filter(|record| match &q.filter {
            // SQL 3VL: a row is kept only when the predicate is definitely
            // true; both `Some(false)` and `None` (unknown, i.e. a NULL was
            // involved) exclude it.
            Some(pred) => eval_predicate(record, pred, disk_reads_allowed) == Some(true),
            None => true,
        })
        .collect())
}

/// The filter / project / order / limit pipeline for a non-grouped query.
fn execute_ungrouped<'a>(
    q: &Query,
    records: impl Iterator<Item = &'a Record>,
    disk_reads_allowed: bool,
) -> Result<ResultTable, ExecError> {
    let filtered = filter_records(q, records, disk_reads_allowed)?;
    let columns = expand_select(q, &filtered);
    let headers: Vec<String> = columns.iter().map(|(header, _)| header.clone()).collect();
    let mut rows: Vec<(&Record, Vec<Value>)> = filtered
        .into_iter()
        .map(|record| {
            let row = columns
                .iter()
                .map(|(_, expr)| eval_expr(record, expr, disk_reads_allowed))
                .collect();
            (record, row)
        })
        .collect();

    if q.distinct {
        dedup_rows(&mut rows);
    }

    let order = resolve_order_targets(&q.order_by, &headers)?;
    let cmp = |(ra, rowa): &(&Record, Vec<Value>), (rb, rowb): &(&Record, Vec<Value>)| {
        order
            .iter()
            .map(|(target, desc)| {
                let va = order_key_value(target, ra, rowa, disk_reads_allowed);
                let vb = order_key_value(target, rb, rowb, disk_reads_allowed);
                order_cmp(&va, &vb, *desc)
            })
            .find(|ord| *ord != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    };

    let offset = q.offset.unwrap_or(0);
    let rows = match q.limit {
        // Only the `offset + limit` window can ever be observed, so select
        // just that many rows instead of fully sorting the rest.
        Some(limit) => {
            let n = offset.saturating_add(limit).min(rows.len());
            bounded_top_k(rows, n, cmp)
        }
        None => {
            rows.sort_by(cmp);
            rows
        }
    };

    let rows: Vec<Vec<Value>> = rows
        .into_iter()
        .map(|(_, row)| row)
        .skip(offset)
        .take(q.limit.unwrap_or(usize::MAX))
        .collect();

    Ok(ResultTable { headers, rows })
}

/// The smallest `n` of `items` under `cmp`, sorted, with ties broken by each
/// item's original position — byte-identical to a full stable sort by `cmp`
/// followed by `.take(n)`. Used by [`execute_ungrouped`] and
/// [`execute_grouped`] to bound `ORDER BY` + `LIMIT` to the requested window.
///
/// `n` is clamped to `items.len()`. Selection runs in expected `O(len)` via
/// [`slice::select_nth_unstable_by`] rather than a full `O(len log len)`
/// sort, so cost tracks the requested window (`offset + limit`) rather than
/// the whole result.
fn bounded_top_k<T>(items: Vec<T>, n: usize, cmp: impl Fn(&T, &T) -> Ordering) -> Vec<T> {
    let mut indexed: Vec<(usize, T)> = items.into_iter().enumerate().collect();
    let len = indexed.len();
    let n = n.min(len);

    // Folding the original index in as a final ascending tiebreaker turns
    // `cmp` (which may rank equal-key items as ties) into a strict total
    // order, so the partial selection below and the final sort agree with
    // what a *stable* sort by `cmp` alone would produce: equal-key items
    // keep their input order, exactly as `Vec::sort_by`'s stability
    // guarantees for a full sort.
    let total_order = |a: &(usize, T), b: &(usize, T)| cmp(&a.1, &b.1).then(a.0.cmp(&b.0));

    if n == 0 {
        return Vec::new();
    }
    if n < len {
        indexed.select_nth_unstable_by(n - 1, &total_order);
        indexed.truncate(n);
    }
    indexed.sort_by(&total_order);
    indexed.into_iter().map(|(_, item)| item).collect()
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
    disk_reads_allowed: bool,
) -> Result<ResultTable, ExecError> {
    let filtered = filter_records(q, records, disk_reads_allowed)?;

    let items = validate_grouped_select(q)?;
    let headers: Vec<String> = q.select.iter().map(|item| item.header()).collect();

    let mut groups = group_rows(&filtered, &q.group_by, disk_reads_allowed);
    groups.sort_by(|a, b| compare_key_tuple(&a.key, &b.key));

    let order = resolve_group_order_targets(&q.order_by, &headers, &q.group_by)?;

    // Each order key's value is resolved here, while `group.rows` is still
    // around — an aggregate order target (Task 8) is computed fresh from
    // them and need not be a projected `SELECT` cell, unlike `Row`/`GroupKey`
    // targets, which are just read back from `row`/`group.key`.
    let mut rows: Vec<(Vec<Value>, Vec<Value>)> = groups
        .into_iter()
        .map(|group| {
            let row = project_group(&group, &items, disk_reads_allowed);
            let order_keys: Vec<Value> = order
                .iter()
                .map(|(target, _)| group_order_key_value(target, &group, &row, disk_reads_allowed))
                .collect();
            (group, row, order_keys)
        })
        .filter(|(group, _, _)| match &q.having {
            // SQL 3VL, same rule as WHERE: a group is kept only when HAVING
            // is definitely true; unknown/false both drop it.
            Some(having) => {
                eval_having(having, group, &q.group_by, disk_reads_allowed) == Some(true)
            }
            None => true,
        })
        .map(|(_, row, order_keys)| (row, order_keys))
        .collect();

    let cmp = |(_, oa): &(Vec<Value>, Vec<Value>), (_, ob): &(Vec<Value>, Vec<Value>)| {
        order
            .iter()
            .zip(oa)
            .zip(ob)
            .map(|(((_, desc), va), vb)| order_cmp(va, vb, *desc))
            .find(|ord| *ord != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    };

    let offset = q.offset.unwrap_or(0);
    let rows = match q.limit {
        // Only the `offset + limit` window of groups can ever be observed,
        // so select just that many instead of fully sorting the rest.
        Some(limit) => {
            let n = offset.saturating_add(limit).min(rows.len());
            bounded_top_k(rows, n, cmp)
        }
        None => {
            rows.sort_by(cmp);
            rows
        }
    };

    let rows: Vec<Vec<Value>> = rows
        .into_iter()
        .map(|(row, _)| row)
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
        Expr::Coalesce(args) => args.iter().flat_map(expr_columns).collect(),
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            let mut cols: Vec<&ColRef> = operand
                .as_deref()
                .into_iter()
                .flat_map(expr_columns)
                .collect();
            for (cond, then) in whens {
                cols.extend(expr_columns(cond));
                cols.extend(expr_columns(then));
            }
            cols.extend(else_expr.as_deref().into_iter().flat_map(expr_columns));
            cols
        }
        Expr::Predicate(pred) => predicate_columns(pred),
    }
}

/// Every column a `WHERE`-style predicate tree references, mirroring
/// [`expr_columns`] but for [`Predicate`] — used when a `CASE WHEN`
/// condition ([`Expr::Predicate`]) reaches [`expr_columns`].
fn predicate_columns(pred: &Predicate) -> Vec<&ColRef> {
    match pred {
        Predicate::Compare(l, _, r) => expr_columns(l).into_iter().chain(expr_columns(r)).collect(),
        Predicate::Like(expr, _, _) | Predicate::In(expr, _, _) | Predicate::IsNull(expr, _) => {
            expr_columns(expr)
        }
        Predicate::Regexp(expr, _, _) => expr_columns(expr),
        Predicate::MemberOf(value, col, _) => {
            let mut cols = expr_columns(value);
            cols.push(col);
            cols
        }
        Predicate::And(a, b) | Predicate::Or(a, b) => predicate_columns(a)
            .into_iter()
            .chain(predicate_columns(b))
            .collect(),
        Predicate::Not(inner) => predicate_columns(inner),
    }
}

/// Whether `q` references `file.body` in any position it could be
/// evaluated: `SELECT`, `WHERE`, `GROUP BY`, `ORDER BY`, or `HAVING`. Checked
/// once, up front, by [`execute_with_schema_at`] (mirroring the
/// unknown-column validation just above it), so a `--force-cache` query that
/// can never produce a real body fails fast with one clear diagnostic
/// instead of silently NULLing every row it touches (design W56).
fn references_body(q: &Query) -> bool {
    let select_hit = q.select.iter().any(|item| match &item.expr {
        SelectExpr::Star => false,
        SelectExpr::Expr(expr) => expr_columns(expr).into_iter().any(is_body_col),
        SelectExpr::Agg(agg) => aggregate_col(agg).is_some_and(is_body_col),
    });
    let where_hit = q
        .filter
        .as_ref()
        .is_some_and(|pred| predicate_columns(pred).into_iter().any(is_body_col));
    let group_hit = q.group_by.iter().any(is_body_col);
    let order_hit = q.order_by.iter().any(|key| match &key.target {
        OrderTarget::Alias(_) => false,
        OrderTarget::Col(col) => is_body_col(col),
        OrderTarget::Agg(agg) => aggregate_col(agg).is_some_and(is_body_col),
        OrderTarget::Expr(expr) => expr_columns(expr).into_iter().any(is_body_col),
    });
    let having_hit = q.having.as_ref().is_some_and(having_references_body);

    select_hit || where_hit || group_hit || order_hit || having_hit
}

/// True for `file.body`'s `ColRef`; every other column (a frontmatter field
/// or any other `file.*` attribute) is fine to evaluate regardless of the
/// disk-read gate.
fn is_body_col(col: &ColRef) -> bool {
    matches!(col, ColRef::File(FileAttr::Body))
}

/// The single column an aggregate's argument names, if any (`CountStar`
/// takes none).
fn aggregate_col(agg: &Aggregate) -> Option<&ColRef> {
    match agg {
        Aggregate::CountStar => None,
        Aggregate::Count(col, _)
        | Aggregate::Min(col)
        | Aggregate::Max(col)
        | Aggregate::Sum(col)
        | Aggregate::Avg(col)
        | Aggregate::GroupConcat(col) => Some(col),
    }
}

/// Walks a `HAVING` tree for a `file.body` reference, mirroring
/// [`references_body`]'s other clauses.
fn having_references_body(having: &Having) -> bool {
    match having {
        Having::Compare(leaf, _, _) => match leaf {
            HavingLeaf::Group(col) => is_body_col(col),
            HavingLeaf::Agg(agg) => aggregate_col(agg).is_some_and(is_body_col),
        },
        Having::And(a, b) | Having::Or(a, b) => {
            having_references_body(a) || having_references_body(b)
        }
        Having::Not(inner) => having_references_body(inner),
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
/// Buckets are found via a `HashMap` keyed on each row's group-by cells,
/// mapping to the bucket's index in `groups`. Each cell is turned into a
/// `String` by [`hashable_cell_key`] and the per-cell keys are collected into
/// a `Vec<String>` rather than joined into one string — this preserves two
/// boundaries a naive key would lose. Cross-column: e.g. `("ab", "c")` and
/// `("a", "bc")` never collide, since each cell is hashed separately (same
/// rationale as [`dedup_rows`]). Cross-variant: e.g. `Value::Int(1)` and
/// `Value::Str("1")`, or `Value::Null` and `Value::Str("")`, stay in separate
/// groups even though [`Value::to_cmp_string`] displays them identically,
/// because [`hashable_cell_key`] also folds in the cell's `Value` variant —
/// matching the type-distinctness a structural `Vec<Value>` `PartialEq`
/// comparison would give. [`hashable_cell_key`] also matches that structural
/// comparison's two other quirks: a `Value::List` cell keys on its elements
/// recursively rather than `to_cmp_string()`'s lossy `", "`-joined form (so
/// `[a, b]` and `["a, b"]` land in different groups, like `main`'s `Vec<Value>`
/// `==` would), and a `Value::Float` cell normalizes `-0.0` to `0.0` before
/// keying (so `0.0` and `-0.0` land in the SAME group, like `f64`'s `==`).
/// This keeps bucketing near-linear in `records.len()` even for
/// high-cardinality group-by columns; a per-row linear scan of existing
/// groups would be quadratic.
///
/// An empty `group_by` means "aggregate over everything": every record —
/// including none at all — falls into the single group keyed by `[]`, so a
/// bare `count(*)` still returns one row for an empty input, matching SQL.
fn group_rows<'a>(
    records: &[&'a Record],
    group_by: &[ColRef],
    disk_reads_allowed: bool,
) -> Vec<Group<'a>> {
    if group_by.is_empty() {
        return vec![Group {
            key: Vec::new(),
            rows: records.to_vec(),
        }];
    }
    let mut groups: Vec<Group<'a>> = Vec::new();
    let mut index: HashMap<Vec<String>, usize> = HashMap::new();
    for &record in records {
        let key: Vec<Value> = group_by
            .iter()
            .map(|col| resolve_col(record, col, disk_reads_allowed))
            .collect();
        let hash_key: Vec<String> = key.iter().map(hashable_cell_key).collect();
        match index.entry(hash_key) {
            Entry::Occupied(entry) => groups[*entry.get()].rows.push(record),
            Entry::Vacant(entry) => {
                entry.insert(groups.len());
                groups.push(Group {
                    key,
                    rows: vec![record],
                });
            }
        }
    }
    groups
}

/// The `HashMap` key for one `GROUP BY` cell, built to agree exactly with
/// `main`'s structural `Vec<Value>` `PartialEq` over a group-by tuple —
/// including its two non-obvious cases, `List` and `Float`.
///
/// A scalar (`Null`/`Bool`/`Int`/`Str`) keys on its [`Value::variant_name`],
/// a `\u{1}` separator (which can't appear in a variant name), then its
/// [`Value::to_cmp_string`] form. The variant prefix is what makes the key
/// type-distinct: `to_cmp_string()` alone is [`Value::display`], which is
/// lossy across variants (`Int(1)` and `Str("1")` both display `"1"`; `Null`
/// and `Str("")` both display `""`), so hashing on it directly would
/// silently merge groups that structural equality keeps apart.
///
/// `Float(f)` normalizes `-0.0` to `0.0` before keying, since `f64`'s
/// `PartialEq` (and so `Value`'s derived one) treats them as equal —
/// `to_cmp_string()` alone would key them `"-0"` and `"0"`, splitting one
/// structural group into two.
///
/// `List(items)` recurses into each element's own `hashable_cell_key`
/// rather than using [`Value::to_cmp_string`]'s `", "`-joined `display()`
/// form: that join is lossy the same way string concatenation always is —
/// `[Str("a"), Str("b")]` and `[Str("a, b")]` both display `"a, b"`, so
/// hashing the joined string would merge two structurally-distinct lists
/// into one group. Each element's key is instead length-prefixed
/// (`"<byte-len>\u{1}<key>"`) before being appended, which is what makes the
/// concatenation collision-free regardless of what characters an element's
/// own key contains: decoding never needs to guess where one element's key
/// ends and the next begins.
///
/// `Map(entries)` recurses the same way, for the same reason — its compact
/// `{k: v}` `to_cmp_string()` form is just as lossy as `List`'s `", "` join
/// (`{a: "x", b: "y"}` and a single-key `{a: "x, b: y"}` both render `{a: x,
/// b: y}`). Both the field name and its recursively-keyed value are
/// length-prefixed before being appended, so concatenation stays unambiguous
/// regardless of what characters a name or nested key contains. Unlike
/// `List`, the entries are sorted by key first: `IndexMap`'s `PartialEq` (and
/// so `Value`'s derived one) is order-insensitive — `{a: 1, b: 2}` and `{b:
/// 2, a: 1}` are structurally equal — so without sorting, two insertion
/// orders of the same map would hash to different keys and wrongly split one
/// group into two. `List`/`Vec` equality is order-sensitive, which is why
/// its arm above does not sort.
fn hashable_cell_key(value: &Value) -> String {
    match value {
        Value::List(items) => {
            let mut key = String::from("List");
            for item in items {
                let element = hashable_cell_key(item);
                key.push('\u{1}');
                key.push_str(&element.len().to_string());
                key.push('\u{1}');
                key.push_str(&element);
            }
            key
        }
        Value::Float(f) => {
            let normalized = if *f == 0.0 { 0.0 } else { *f };
            format!("Float\u{1}{normalized}")
        }
        Value::Map(entries) => {
            let mut pairs: Vec<(&String, &Value)> = entries.iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0)); // order-insensitive map equality -> sort
            let mut key = String::from("Map");
            for (name, v) in pairs {
                let element = hashable_cell_key(v);
                for part in [name.as_str(), element.as_str()] {
                    key.push('\u{1}');
                    key.push_str(&part.len().to_string());
                    key.push('\u{1}');
                    key.push_str(part);
                }
            }
            key
        }
        _ => format!("{}\u{1}{}", value.variant_name(), value.to_cmp_string()),
    }
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

/// One `SELECT` item's projection state while [`project_group`] folds a
/// group's rows: a non-aggregate expression just carries the expression to
/// evaluate afterward (it only ever looks at the group's representative
/// row, never the whole group — see [`eval_group_expr`]), while an aggregate
/// carries a running [`AggState`] to be updated once per row.
enum ProjectedItem<'a> {
    Expr(&'a Expr),
    Agg(AggState<'a>),
}

/// Projects one group's row: a non-aggregate expression is evaluated over
/// the group's representative row; every aggregate `SELECT` item instead
/// gets its own [`AggState`], and ALL of them are folded together over a
/// SINGLE pass through `group.rows` — rather than each aggregate rescanning
/// the group on its own (which is what calling [`compute_aggregate`] once
/// per item would do). The more aggregates a query projects, the more this
/// saves.
fn project_group(
    group: &Group<'_>,
    items: &[GroupedSelectItem],
    disk_reads_allowed: bool,
) -> Vec<Value> {
    let mut projected: Vec<ProjectedItem<'_>> = items
        .iter()
        .map(|item| match item {
            GroupedSelectItem::Expr(expr) => ProjectedItem::Expr(expr),
            GroupedSelectItem::Agg(agg) => ProjectedItem::Agg(AggState::new(agg)),
        })
        .collect();

    for record in &group.rows {
        for item in &mut projected {
            if let ProjectedItem::Agg(state) = item {
                state.update(record, disk_reads_allowed);
            }
        }
    }

    projected
        .into_iter()
        .map(|item| match item {
            ProjectedItem::Expr(expr) => eval_group_expr(&group.rows, expr, disk_reads_allowed),
            ProjectedItem::Agg(state) => state.finish(),
        })
        .collect()
}

/// Evaluates a validated grouped-`SELECT` expression against the group's
/// representative row (its first record). `rows` is empty only for the
/// zero-row "aggregate over nothing" bucket (empty `GROUP BY`, no matching
/// records — see [`group_rows`]); [`validate_grouped_select`] guarantees a
/// `SELECT` expression surviving that bucket references no columns, so
/// evaluating it against a fieldless stand-in record is safe.
fn eval_group_expr(rows: &[&Record], expr: &Expr, disk_reads_allowed: bool) -> Value {
    match rows.first() {
        Some(record) => eval_expr(record, expr, disk_reads_allowed),
        None => eval_expr(&empty_record(), expr, disk_reads_allowed),
    }
}

/// A fieldless record with no `file.*` identity, used only as the
/// evaluation context in [`eval_group_expr`]'s zero-row fallback.
fn empty_record() -> Record {
    Record::new(
        Path::new(""),
        Path::new(""),
        IndexMap::new(),
        SystemTime::UNIX_EPOCH,
        0,
        0,
    )
}

/// One aggregate call's running state, updated one row at a time. This is
/// the single source of truth for every aggregate's NULL-handling and
/// finalization rule, shared by two callers with different scanning needs:
/// [`project_group`] builds one `AggState` per `SELECT` aggregate and folds
/// a group's rows into all of them in one pass, while [`compute_aggregate`]
/// below folds a group's rows into just one — for `HAVING`
/// ([`eval_having_leaf`]) and a bare `ORDER BY` aggregate
/// ([`resolve_group_order_targets`]), which each only ever need a single
/// aggregate's value.
enum AggState<'a> {
    /// `count(*)` — every row counts, `NULL` or not.
    CountStar(i64),
    /// `count(col)` — counts only `col`'s non-null values.
    Count { col: &'a ColRef, count: i64 },
    /// `count(distinct col)` — the distinct non-null values seen, keyed by
    /// [`Value::to_cmp_string`] (matching [`Value`] equality for the
    /// purposes of this count).
    CountDistinct {
        col: &'a ColRef,
        seen: BTreeSet<String>,
    },
    /// `sum(col)` — the running total of `col`'s numeric-coercible values;
    /// `NULL` and non-numeric values are both skipped (mirroring
    /// [`Value::as_number`], which returns `None` for both), so an
    /// all-skipped group sums to the identity `0.0`.
    Sum { col: &'a ColRef, sum: f64 },
    /// `avg(col)` — the running sum and count of `col`'s numeric-coercible
    /// values; `finish` divides by `count`, or yields `NULL` if `count` is
    /// still zero (no numeric values seen).
    Avg {
        col: &'a ColRef,
        sum: f64,
        count: usize,
    },
    /// `min(col)`/`max(col)` — the running extreme of `col`'s non-null
    /// values via [`compare_values`], starting at `NULL` (no extreme picked
    /// yet). `want` is the [`Ordering`] that means "this value replaces the
    /// running extreme" (`Less` for `MIN`, `Greater` for `MAX`).
    Extreme {
        col: &'a ColRef,
        want: Ordering,
        best: Value,
    },
    /// `group_concat(col)` — `col`'s non-null values, `display`-rendered and
    /// collected in row order; `finish` joins them with `", "`.
    GroupConcat { col: &'a ColRef, parts: Vec<String> },
}

impl<'a> AggState<'a> {
    /// A zeroed/empty accumulator for `agg`, ready to fold in rows one at a
    /// time via [`AggState::update`].
    fn new(agg: &'a Aggregate) -> Self {
        match agg {
            Aggregate::CountStar => AggState::CountStar(0),
            Aggregate::Count(col, false) => AggState::Count { col, count: 0 },
            Aggregate::Count(col, true) => AggState::CountDistinct {
                col,
                seen: BTreeSet::new(),
            },
            Aggregate::Sum(col) => AggState::Sum { col, sum: 0.0 },
            Aggregate::Avg(col) => AggState::Avg {
                col,
                sum: 0.0,
                count: 0,
            },
            Aggregate::Min(col) => AggState::Extreme {
                col,
                want: Ordering::Less,
                best: Value::Null,
            },
            Aggregate::Max(col) => AggState::Extreme {
                col,
                want: Ordering::Greater,
                best: Value::Null,
            },
            Aggregate::GroupConcat(col) => AggState::GroupConcat {
                col,
                parts: Vec::new(),
            },
        }
    }

    /// Folds one more row into the running state. Call once per row in the
    /// group, in row order — order matters for `GROUP_CONCAT`.
    fn update(&mut self, record: &Record, disk_reads_allowed: bool) {
        match self {
            AggState::CountStar(count) => *count += 1,
            AggState::Count { col, count } => {
                if !resolve_col(record, col, disk_reads_allowed).is_null() {
                    *count += 1;
                }
            }
            AggState::CountDistinct { col, seen } => {
                let value = resolve_col(record, col, disk_reads_allowed);
                if !value.is_null() {
                    seen.insert(value.to_cmp_string());
                }
            }
            AggState::Sum { col, sum } => {
                if let Some(n) = resolve_col(record, col, disk_reads_allowed).as_number() {
                    *sum += n;
                }
            }
            AggState::Avg { col, sum, count } => {
                if let Some(n) = resolve_col(record, col, disk_reads_allowed).as_number() {
                    *sum += n;
                    *count += 1;
                }
            }
            AggState::Extreme { col, want, best } => {
                let value = resolve_col(record, col, disk_reads_allowed);
                if !value.is_null() {
                    match compare_values(&value, best) {
                        Some(ord) if ord == *want => *best = value,
                        Some(_) => {}
                        // `best` is still `Null` (no extreme picked yet): take `value`.
                        None => *best = value,
                    }
                }
            }
            AggState::GroupConcat { col, parts } => {
                let value = resolve_col(record, col, disk_reads_allowed);
                if !value.is_null() {
                    parts.push(value.display());
                }
            }
        }
    }

    /// The aggregate's final value, once every row in the group has been
    /// folded in via [`AggState::update`].
    fn finish(self) -> Value {
        match self {
            AggState::CountStar(count) => Value::Int(count),
            AggState::Count { count, .. } => Value::Int(count),
            AggState::CountDistinct { seen, .. } => Value::Int(seen.len() as i64),
            AggState::Sum { sum, .. } => Value::Float(sum),
            AggState::Avg { sum, count, .. } => {
                if count == 0 {
                    Value::Null
                } else {
                    Value::Float(sum / count as f64)
                }
            }
            AggState::Extreme { best, .. } => best,
            AggState::GroupConcat { parts, .. } => Value::Str(parts.join(", ")),
        }
    }
}

/// Computes one aggregate function's value over a group's rows: builds a
/// single [`AggState`] and folds every row into it. See [`AggState`]'s doc
/// for why this and [`project_group`] share the same accumulator instead of
/// each re-implementing per-aggregate NULL handling.
fn compute_aggregate(agg: &Aggregate, rows: &[&Record], disk_reads_allowed: bool) -> Value {
    let mut state = AggState::new(agg);
    for record in rows {
        state.update(record, disk_reads_allowed);
    }
    state.finish()
}

/// Evaluates a `HAVING` predicate tree against one group under SQL
/// three-valued logic (3VL), mirroring [`eval_predicate`]'s handling for
/// `WHERE`. `group_by` is `q.group_by`, needed to resolve a
/// [`HavingLeaf::Group`] leaf's value from the group's key tuple by
/// position — see [`eval_having_leaf`].
fn eval_having(
    having: &Having,
    group: &Group<'_>,
    group_by: &[ColRef],
    disk_reads_allowed: bool,
) -> Option<bool> {
    match having {
        Having::Compare(leaf, op, lit) => {
            let value = eval_having_leaf(leaf, group, group_by, disk_reads_allowed);
            eval_compare(&value, op, &literal_value(lit))
        }
        Having::And(a, b) => three_valued_and(
            eval_having(a, group, group_by, disk_reads_allowed),
            eval_having(b, group, group_by, disk_reads_allowed),
        ),
        Having::Or(a, b) => three_valued_or(
            eval_having(a, group, group_by, disk_reads_allowed),
            eval_having(b, group, group_by, disk_reads_allowed),
        ),
        Having::Not(inner) => {
            three_valued_not(eval_having(inner, group, group_by, disk_reads_allowed))
        }
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
fn eval_having_leaf(
    leaf: &HavingLeaf,
    group: &Group<'_>,
    group_by: &[ColRef],
    disk_reads_allowed: bool,
) -> Value {
    match leaf {
        HavingLeaf::Agg(agg) => compute_aggregate(agg, &group.rows, disk_reads_allowed),
        HavingLeaf::Group(col) => group_by
            .iter()
            .position(|g| g == col)
            .map(|idx| group.key[idx].clone())
            .unwrap_or(Value::Null),
    }
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
    /// A bare aggregate call (Task 8), computed fresh from the group's rows
    /// — it need not appear in `SELECT`, mirroring how `HAVING` may
    /// reference an unselected aggregate (see [`HavingLeaf::Agg`]).
    Agg(Aggregate),
    /// A computed expression (`CASE`, arithmetic, a scalar-fn call, …) built
    /// entirely from `group_by` columns — the same restriction
    /// [`validate_grouped_select`] applies to a non-aggregate `SELECT` item,
    /// since it likewise must reduce to one value per group.
    Expr(Expr),
}

/// Resolves each `ORDER BY` key's target for the grouped path: an explicit
/// alias resolves against `headers`, exactly like the non-grouped path; a
/// bare column must be one of `group_by`'s keys — referencing anything else
/// is as invalid as selecting it, so it's rejected the same way; a bare
/// aggregate always resolves, since it's computed fresh per group rather
/// than looked up in the projection or the grouping keys; a computed
/// expression is checked the same way a grouped `SELECT` expression is (see
/// [`validate_grouped_select`]) — every column it references must be one of
/// `group_by`'s keys.
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
                OrderTarget::Agg(agg) => ResolvedGroupOrderTarget::Agg(agg.clone()),
                OrderTarget::Expr(expr) => {
                    if expr_columns(expr)
                        .into_iter()
                        .all(|col| group_by.contains(col))
                    {
                        ResolvedGroupOrderTarget::Expr(expr.clone())
                    } else {
                        return Err(ExecError::NonGroupedColumn(expr_header(expr)));
                    }
                }
            };
            Ok((target, key.desc))
        })
        .collect()
}

/// Renders an arbitrary `Expr` the way it would appear as a default `SELECT`
/// header, for a `NonGroupedColumn` message about an `ORDER BY` expression
/// (or, via [`col_header`], a bare `ORDER BY` column).
fn expr_header(expr: &Expr) -> String {
    SelectItem {
        expr: SelectExpr::Expr(expr.clone()),
        alias: None,
    }
    .header()
}

/// Renders a bare `ColRef` the way it would appear as a default `SELECT`
/// header, for a `NonGroupedColumn` message about an `ORDER BY` column.
fn col_header(col: &ColRef) -> String {
    expr_header(&Expr::Col(col.clone()))
}

/// Reads the sort key's value for one group, given its already-projected
/// row. An aggregate target is computed fresh from `group.rows` — it need
/// not be one of `group`'s already-projected `SELECT` cells. An expression
/// target is likewise evaluated fresh, over the group's representative row
/// (see [`eval_group_expr`]) — [`resolve_group_order_targets`] guarantees it
/// references only `group_by` columns, so any row in the group yields the
/// same value.
fn group_order_key_value(
    target: &ResolvedGroupOrderTarget,
    group: &Group<'_>,
    row: &[Value],
    disk_reads_allowed: bool,
) -> Value {
    match target {
        ResolvedGroupOrderTarget::Row(idx) => row[*idx].clone(),
        ResolvedGroupOrderTarget::GroupKey(idx) => group.key[*idx].clone(),
        ResolvedGroupOrderTarget::Agg(agg) => {
            compute_aggregate(agg, &group.rows, disk_reads_allowed)
        }
        ResolvedGroupOrderTarget::Expr(expr) => {
            eval_group_expr(&group.rows, expr, disk_reads_allowed)
        }
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
                    columns.push((name.clone(), Expr::Col(ColRef::Field(vec![name]))));
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
///
/// `ColRef::File(FileAttr::Body)` is special-cased here rather than falling
/// through to [`Record::file_attr`] (which is pure and can only return its
/// `Null` sentinel for `Body` — see its doc comment): it's the one column
/// that needs a real disk read, gated by `disk_reads_allowed` (design W56;
/// `false` under `Freshness::ForceCache`) — see [`read_body`].
fn resolve_col(record: &Record, col: &ColRef, disk_reads_allowed: bool) -> Value {
    match col {
        ColRef::Field(path) => record.field(path),
        ColRef::File(FileAttr::Body) => read_body(record, disk_reads_allowed),
        ColRef::File(attr) => record.file_attr(*attr),
    }
}

/// Reads `record`'s Markdown body fresh from disk — the on-disk cache never
/// stores body text (design W56), only its word count, so this always hits
/// the filesystem when `disk_reads_allowed` permits it.
///
/// Returns `Value::Null` whenever a real body can't be produced: disk reads
/// disallowed (`--force-cache`; under `--lenient` this is the ONLY signal the
/// caller gets — strict mode instead fails the whole query up front via
/// [`references_body`]/[`ExecError::BodyUnavailable`] before this is ever
/// called), the file is unreadable (moved/deleted/permission-denied since it
/// was cached), or its frontmatter fence is no longer valid YAML (see
/// [`crate::frontmatter::body`]). Every one of these is indistinguishable
/// from "no value" elsewhere in the query engine (a missing/invalid
/// frontmatter field is `Null` too), so `Null` — never a panic — is the
/// right total answer for a per-row condition that can't be predicted ahead
/// of time.
fn read_body(record: &Record, disk_reads_allowed: bool) -> Value {
    if !disk_reads_allowed {
        return Value::Null;
    }
    match fs::read_to_string(record.abs_path()) {
        Ok(content) => match frontmatter::body(&content) {
            Some(body) => Value::Str(body),
            None => Value::Null,
        },
        Err(_) => Value::Null,
    }
}

/// Evaluates a scalar expression against `record`: a column/`file.*`
/// pseudo-column resolves via [`resolve_col`], a literal evaluates to its
/// `Value`, a scalar-function call evaluates its arguments first then
/// applies [`apply_scalar`], a binary op evaluates both sides then applies
/// [`apply_binary`], `COALESCE` evaluates its arguments left to right,
/// short-circuiting on the first non-null, `CASE` picks its first matching
/// arm (see below), and `Expr::Predicate` evaluates a `WHERE`-style
/// predicate to a `Value::Bool`/`Value::Null`. Used by both the ungrouped
/// projection (per row, [`expand_select`]) and the grouped projection (over
/// a group's representative row, [`eval_group_expr`]).
///
/// `CASE` evaluation: the searched form (`operand: None`) returns the first
/// `WHEN` arm whose condition is truthy (via [`is_truthy`]); the simple form
/// (`operand: Some`) returns the first arm whose value equals the operand
/// (via [`eval_compare`], the same equality `IN`/`MEMBER OF` use). Neither
/// matching falls through to `else_expr`, or `Value::Null` with no `ELSE`.
pub(crate) fn eval_expr(record: &Record, expr: &Expr, disk_reads_allowed: bool) -> Value {
    match expr {
        Expr::Col(col) => resolve_col(record, col, disk_reads_allowed),
        Expr::Lit(lit) => literal_value(lit),
        Expr::Scalar(f, args) => {
            let values: Vec<Value> = args
                .iter()
                .map(|arg| eval_expr(record, arg, disk_reads_allowed))
                .collect();
            apply_scalar(f.clone(), &values)
        }
        Expr::Binary(op, left, right) => {
            let l = eval_expr(record, left, disk_reads_allowed);
            let r = eval_expr(record, right, disk_reads_allowed);
            apply_binary(op.clone(), &l, &r)
        }
        Expr::Coalesce(args) => args
            .iter()
            .map(|arg| eval_expr(record, arg, disk_reads_allowed))
            .find(|v| !v.is_null())
            .unwrap_or(Value::Null),
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            match operand {
                None => {
                    for (cond, then) in whens {
                        if is_truthy(&eval_expr(record, cond, disk_reads_allowed)) {
                            return eval_expr(record, then, disk_reads_allowed);
                        }
                    }
                }
                Some(op) => {
                    let target = eval_expr(record, op, disk_reads_allowed);
                    for (val, then) in whens {
                        let candidate = eval_expr(record, val, disk_reads_allowed);
                        if eval_compare(&target, &CmpOp::Eq, &candidate) == Some(true) {
                            return eval_expr(record, then, disk_reads_allowed);
                        }
                    }
                }
            }
            else_expr
                .as_deref()
                .map_or(Value::Null, |e| eval_expr(record, e, disk_reads_allowed))
        }
        Expr::Predicate(pred) => {
            eval_predicate(record, pred, disk_reads_allowed).map_or(Value::Null, Value::Bool)
        }
    }
}

/// Whether `v` counts as a `CASE WHEN` condition being satisfied: only
/// `Value::Bool(true)` is truthy — `Value::Null` (the 3VL-unknown result an
/// `Expr::Predicate` condition can produce) and any other value are not,
/// mirroring [`filter_records`]'s "only `Some(true)` keeps a row" rule.
fn is_truthy(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

/// Converts a literal constant to the `Value` it evaluates to.
fn literal_value(lit: &Literal) -> Value {
    match lit {
        Literal::Str(s) => Value::Str(s.clone()),
        Literal::Int(i) => Value::Int(*i),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
        Literal::Null => Value::Null,
        // `execute_with_schema_at` runs `rewrite_relative_dates` before the
        // filter/group pipeline (and before `HAVING` evaluation, which also
        // calls this), resolving every `RelativeDate` to a `Literal::Str`.
        // This function has no clock to resolve one itself, so reaching
        // here would mean a `Literal::RelativeDate` survived the rewrite —
        // a bug in the rewrite's traversal, not a state `eval_expr`/
        // `eval_having` should ever see in practice.
        Literal::RelativeDate(_) => {
            unreachable!("relative-date literals are resolved before evaluation")
        }
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
///
/// `ScalarFn::Date` is the one exception to the stringify-first rule (see
/// [`cast_date`]): a non-string, non-`Date`/`DateTime` first argument yields
/// `Null` instead of stringifying, since there's no sensible date to parse
/// out of e.g. an `Int`.
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
        (ScalarFn::Date, [s]) => cast_date(s, None),
        (ScalarFn::Date, [s, fmt]) => cast_date(s, Some(fmt)),
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

/// `date(s[, fmt])`: casts `s` to a [`Value::Date`]/[`Value::DateTime`]. A
/// `s` that's already a `Date`/`DateTime` passes through unchanged. A `fmt`
/// argument parses `s` as a [`NaiveDate`] with that chrono format string;
/// without one, `s` tries strict `%Y-%m-%d` then RFC3339 — the same
/// detection frontmatter ingest applies to auto-classify a string field.
/// Anything else — a non-string `s`, or a string chrono can't parse —
/// yields `Value::Null`.
fn cast_date(s: &Value, fmt: Option<&Value>) -> Value {
    if matches!(s, Value::Date(_) | Value::DateTime(_)) {
        return s.clone();
    }
    let Value::Str(s) = s else {
        return Value::Null;
    };
    if let Some(fmt) = fmt {
        return match NaiveDate::parse_from_str(s, &fmt.display()) {
            Ok(d) => Value::Date(d),
            Err(_) => Value::Null,
        };
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Value::Date(d);
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Value::DateTime(dt.with_timezone(&Utc));
    }
    Value::Null
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
/// `status NOT LIKE ...`, `NOT 'x' MEMBER OF(tags)`, and `NOT (status = ...)`
/// all EXCLUDE a NULL-`status` row — matching the plain-`Compare` path and
/// the spec's "any comparison where a side is Null yields 'not true'" rule
/// (§4). Only `IS NULL` / `IS NOT NULL` are ever determinate for a NULL
/// field.
fn eval_predicate(record: &Record, pred: &Predicate, disk_reads_allowed: bool) -> Option<bool> {
    match pred {
        Predicate::Compare(left, op, right) => eval_compare(
            &eval_expr(record, left, disk_reads_allowed),
            op,
            &eval_expr(record, right, disk_reads_allowed),
        ),
        Predicate::Like(expr, pattern, negated) => {
            let value = eval_expr(record, expr, disk_reads_allowed);
            if value.is_null() {
                return None;
            }
            let base = Some(like_matches(&value.to_cmp_string(), pattern));
            maybe_negate(base, *negated)
        }
        Predicate::Regexp(expr, pattern, negated) => {
            let value = eval_expr(record, expr, disk_reads_allowed);
            if value.is_null() {
                return None;
            }
            let base = Some(regexp_matches(&value.display(), pattern));
            maybe_negate(base, *negated)
        }
        Predicate::In(expr, literals, negated) => {
            let value = eval_expr(record, expr, disk_reads_allowed);
            if value.is_null() {
                return None;
            }
            let base = Some(literals.iter().any(|lit| element_equals(&value, lit)));
            maybe_negate(base, *negated)
        }
        Predicate::MemberOf(value_expr, col, negated) => {
            let needle = eval_expr(record, value_expr, disk_reads_allowed);
            let value = resolve_col(record, col, disk_reads_allowed);
            let Value::List(items) = &value else {
                // Unknown for both a `Null` field and a non-list value —
                // never a hard `false` — mirroring `In`'s null-column rule.
                return None;
            };
            let base = Some(
                items
                    .iter()
                    .any(|el| eval_compare(el, &CmpOp::Eq, &needle) == Some(true)),
            );
            maybe_negate(base, *negated)
        }
        // The only predicate that is determinate — and true — for a NULL field.
        Predicate::IsNull(expr, negated) => {
            Some(eval_expr(record, expr, disk_reads_allowed).is_null() != *negated)
        }
        Predicate::And(a, b) => three_valued_and(
            eval_predicate(record, a, disk_reads_allowed),
            eval_predicate(record, b, disk_reads_allowed),
        ),
        Predicate::Or(a, b) => three_valued_or(
            eval_predicate(record, a, disk_reads_allowed),
            eval_predicate(record, b, disk_reads_allowed),
        ),
        Predicate::Not(inner) => {
            three_valued_not(eval_predicate(record, inner, disk_reads_allowed))
        }
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

/// Matches `value` against a `REGEXP` pattern. `parse::lower_regexp` already
/// confirmed `pattern` compiles at parse time, so recompiling it here can't
/// fail — mirroring [`like_matches`], it's recompiled fresh per call rather
/// than cached.
fn regexp_matches(value: &str, pattern: &str) -> bool {
    let re = Regex::new(pattern).expect("REGEXP pattern validated to compile at parse time");
    re.is_match(value)
}

/// An `ORDER BY` target resolved against the projection, so sorting doesn't
/// need to re-resolve an alias (or fail) on every comparison.
enum ResolvedOrderTarget {
    /// An index into the projected row (a `SELECT ... AS alias` match).
    AliasIndex(usize),
    /// A fresh column lookup on the source record.
    Col(ColRef),
    /// A computed expression (arithmetic, `CASE`, a scalar-fn call, …),
    /// evaluated fresh per row via [`eval_expr`].
    Expr(Expr),
}

/// Resolves each `ORDER BY` key's target once, up front, returning the
/// resolved target paired with its `DESC` flag.
///
/// `OrderTarget::Agg` has no resolution here — see
/// [`ExecError::AggregateOrderWithoutGroupBy`] for why it's unreachable for
/// any [`Query`] the parser produces.
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
                OrderTarget::Agg(_) => return Err(ExecError::AggregateOrderWithoutGroupBy),
                OrderTarget::Expr(expr) => ResolvedOrderTarget::Expr(expr.clone()),
            };
            Ok((target, key.desc))
        })
        .collect()
}

/// Reads the sort key's value for one row, given its source record and
/// already-projected row.
fn order_key_value(
    target: &ResolvedOrderTarget,
    record: &Record,
    row: &[Value],
    disk_reads_allowed: bool,
) -> Value {
    match target {
        ResolvedOrderTarget::AliasIndex(idx) => row[*idx].clone(),
        ResolvedOrderTarget::Col(col) => resolve_col(record, col, disk_reads_allowed),
        ResolvedOrderTarget::Expr(expr) => eval_expr(record, expr, disk_reads_allowed),
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
        Record::new(
            Path::new(root),
            Path::new(path),
            m,
            SystemTime::UNIX_EPOCH,
            0,
            0,
        )
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
    fn date_cast_passes_through_date_and_datetime_and_defaults_to_rfc3339() {
        use chrono::TimeZone;
        use std::slice;

        let s = |t: &str| Value::Str(t.into());
        let d = Value::Date(NaiveDate::from_ymd_opt(2026, 7, 24).unwrap());
        let dt = Value::DateTime(Utc.with_ymd_and_hms(2026, 7, 24, 9, 30, 0).unwrap());

        // Already a Date/DateTime: passes through unchanged, no re-parse.
        assert_eq!(apply_scalar(ScalarFn::Date, slice::from_ref(&d)), d);
        assert_eq!(apply_scalar(ScalarFn::Date, slice::from_ref(&dt)), dt);

        // No fmt arg: tries `%Y-%m-%d`, then falls back to RFC3339.
        assert_eq!(
            apply_scalar(ScalarFn::Date, &[s("2026-07-24T09:30:00Z")]),
            dt
        );

        // A value that isn't a string, `Date`, or `DateTime` has no sensible
        // date to parse out of it — `Null`, not a stringify-then-parse
        // attempt (see the `apply_scalar` doc comment).
        assert_eq!(
            apply_scalar(ScalarFn::Date, &[Value::Int(20260724)]),
            Value::Null
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
            execute(&neg_start, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("hello".into())]]
        );

        let huge_len = parse("SELECT substr(name, 2, 99999999999999999999)").unwrap();
        assert_eq!(
            execute(&huge_len, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("ello".into())]]
        );

        let huge_start = parse("SELECT substr(name, 99999999999999999999)").unwrap();
        assert_eq!(
            execute(&huge_start, rows.iter(), false).unwrap().rows,
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
            execute(&lower, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("draft".into())]]
        );

        let div = parse("SELECT (a / b) AS r").unwrap();
        assert_eq!(
            execute(&div, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Float(1.5)]]
        );

        let concat = parse("SELECT a || '-' || status").unwrap();
        assert_eq!(
            execute(&concat, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("3-Draft".into())]]
        );
    }

    #[test]
    fn date_cast_parses_iso_and_custom_format() {
        let rows = [rec("s", "s/a.md", &[])];
        let expected = NaiveDate::from_ymd_opt(2026, 7, 24).unwrap();

        let iso = parse("SELECT date('2026-07-24')").unwrap();
        assert_eq!(
            execute(&iso, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Date(expected)]]
        );

        let custom = parse("SELECT date('07/24/2026', '%m/%d/%Y')").unwrap();
        assert_eq!(
            execute(&custom, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Date(expected)]]
        );

        let unparseable = parse("SELECT date('nonsense')").unwrap();
        assert_eq!(
            execute(&unparseable, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Null]]
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
            execute(&q, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Float(4.5)]]
        );
    }

    #[test]
    fn coalesce_returns_first_non_null() {
        let rows = [
            rec(
                "s",
                "s/a.md",
                &[("jira", Value::Str("DCP-1".into())), ("epic", Value::Null)],
            ),
            rec(
                "s",
                "s/b.md",
                &[("jira", Value::Null), ("epic", Value::Null)],
            ),
        ];

        let q = parse("SELECT COALESCE(epic, jira, 'none')").unwrap();
        assert_eq!(
            execute(&q, rows.iter(), false).unwrap().rows,
            vec![
                vec![Value::Str("DCP-1".into())],
                vec![Value::Str("none".into())],
            ]
        );

        // No fallback literal: every argument null yields Null, not 'none'.
        let all_null = parse("SELECT COALESCE(epic, jira)").unwrap();
        assert_eq!(
            execute(&all_null, rows[1..2].iter(), false).unwrap().rows,
            vec![vec![Value::Null]]
        );
    }

    #[test]
    fn searched_case_selects_first_true_branch() {
        let rows = [
            rec("s", "s/a.md", &[("status", Value::Str("draft".into()))]),
            rec("s", "s/b.md", &[("status", Value::Str("done".into()))]),
        ];
        let q = parse("SELECT CASE WHEN status = 'draft' THEN 'D' ELSE 'X' END").unwrap();
        assert_eq!(
            execute(&q, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("D".into())], vec![Value::Str("X".into())]]
        );
    }

    #[test]
    fn simple_case_matches_operand() {
        let rows = [
            rec("s", "s/a.md", &[("status", Value::Str("done".into()))]),
            rec("s", "s/b.md", &[("status", Value::Str("other".into()))]),
        ];
        let q = parse("SELECT CASE status WHEN 'draft' THEN 'D' WHEN 'done' THEN 'Z' END").unwrap();
        assert_eq!(
            execute(&q, rows.iter(), false).unwrap().rows,
            // "done" matches the second WHEN; "other" matches no WHEN and
            // there's no ELSE, so it falls back to `Value::Null`.
            vec![vec![Value::Str("Z".into())], vec![Value::Null]]
        );
    }

    #[test]
    fn case_usable_in_where_and_order_by() {
        let rows = [
            rec("s", "s/b.md", &[("status", Value::Str("synced".into()))]),
            rec("s", "s/a.md", &[("status", Value::Str("draft".into()))]),
            rec("s", "s/c.md", &[("status", Value::Str("synced".into()))]),
        ];

        // `WHERE (CASE WHEN status = 'draft' THEN 1 ELSE 0 END) = 1` keeps
        // only the draft row — proves a searched CASE evaluates as a
        // comparison operand inside a WHERE predicate.
        let where_q =
            parse("SELECT file.name WHERE (CASE WHEN status = 'draft' THEN 1 ELSE 0 END) = 1")
                .unwrap();
        let t = execute(&where_q, rows.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("a.md".into())]]);

        // `ORDER BY CASE WHEN status = 'draft' THEN 0 ELSE 1 END` puts
        // drafts first even though "a.md" is neither first in scan order
        // nor first alphabetically — proves the `ORDER BY <expr>` path.
        // `file.name` is a tiebreaker, pinning multi-key ORDER BY semantics.
        let order_q = parse(
            "SELECT file.name ORDER BY CASE WHEN status = 'draft' THEN 0 ELSE 1 END, file.name",
        )
        .unwrap();
        let t = execute(&order_q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("a.md".into())],
                vec![Value::Str("b.md".into())],
                vec![Value::Str("c.md".into())],
            ]
        );
    }

    #[test]
    fn filter_and_project_with_alias() {
        let q = parse("SELECT status AS S, file.name WHERE prd = '010'").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
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
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("synced".into())]]);
    }
    #[test]
    fn order_by_limit_preserves_tie_input_order_like_full_sort() {
        // Five rows share the same `status` key, so a full stable sort
        // leaves ORDER BY status LIMIT 3 with the first 3 rows in *input*
        // (scan) order. A non-stable top-k (e.g. an unstable binary-heap
        // selection with no tiebreaker) would be free to return any 3 of
        // the 5 equal-key rows in any order — this is the load-bearing pin
        // that the bounded top-k must match a stable full sort exactly.
        let rows = [
            rec("s", "s/a.md", &[("status", Value::Str("x".into()))]),
            rec("s", "s/b.md", &[("status", Value::Str("x".into()))]),
            rec("s", "s/c.md", &[("status", Value::Str("x".into()))]),
            rec("s", "s/d.md", &[("status", Value::Str("x".into()))]),
            rec("s", "s/e.md", &[("status", Value::Str("x".into()))]),
        ];
        let q = parse("SELECT file.name ORDER BY status LIMIT 3").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("a.md".into())],
                vec![Value::Str("b.md".into())],
                vec![Value::Str("c.md".into())],
            ]
        );
    }
    #[test]
    fn order_by_limit_offset_window_matches_full_sort() {
        // n: a=5, b=3, c=4, d=1, e=2. A full stable sort by n DESC is
        // a(5), c(4), b(3), e(2), d(1); .skip(2).take(3) is b, e, d — the
        // bounded top-k (which only ever materializes offset + limit rows)
        // must land on the exact same window.
        let rows = [
            rec("s", "s/a.md", &[("n", Value::Int(5))]),
            rec("s", "s/b.md", &[("n", Value::Int(3))]),
            rec("s", "s/c.md", &[("n", Value::Int(4))]),
            rec("s", "s/d.md", &[("n", Value::Int(1))]),
            rec("s", "s/e.md", &[("n", Value::Int(2))]),
        ];
        let q = parse("SELECT file.name ORDER BY n DESC LIMIT 3 OFFSET 2").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("b.md".into())],
                vec![Value::Str("e.md".into())],
                vec![Value::Str("d.md".into())],
            ]
        );
    }
    #[test]
    fn order_by_limit_zero_returns_no_rows() {
        let q = parse("SELECT status ORDER BY status LIMIT 0").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert!(t.rows.is_empty());
    }
    #[test]
    fn order_by_limit_exceeds_row_count_returns_all_rows_in_order() {
        // `recs()` has only 3 rows; LIMIT 100 must behave exactly like no
        // LIMIT at all rather than panicking or truncating oddly.
        let q = parse("SELECT status ORDER BY status LIMIT 100").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("draft".into())],
                vec![Value::Str("synced".into())],
                vec![Value::Str("synced".into())],
            ]
        );
    }
    #[test]
    fn distinct_dedups_projection() {
        let rows = [
            rec("s", "s/a/1.md", &[]),
            rec("s", "s/a/2.md", &[]),
            rec("s", "s/b/3.md", &[]),
        ];
        let q = parse("SELECT DISTINCT file.folder").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
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
        let t = execute(&q, recs().iter(), false).unwrap();
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
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![vec![Value::Str("B".into())], vec![Value::Str("A".into())]]
        );
    }
    #[test]
    fn star_expands_sorted_union() {
        let q = parse("SELECT * WHERE status = 'draft'").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(t.headers, vec!["prd", "status"]);
    }
    #[test]
    fn from_glob_filters_by_path() {
        let q = parse("SELECT file.name FROM 'plans/**'").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(t.rows.len(), 2);
    }
    #[test]
    fn like_and_in() {
        let q = parse("SELECT status WHERE status LIKE 'syn%'").unwrap();
        assert_eq!(execute(&q, recs().iter(), false).unwrap().rows.len(), 2);
        let q2 = parse("SELECT status WHERE prd IN ('011')").unwrap();
        assert_eq!(execute(&q2, recs().iter(), false).unwrap().rows.len(), 1);
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
            execute(&present, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("a.md".into())]]
        );

        // A non-member yields no match on the list row either.
        let absent = parse("SELECT file.name WHERE 'ios' MEMBER OF(tags)").unwrap();
        assert!(
            execute(&absent, rows.iter(), false)
                .unwrap()
                .rows
                .is_empty()
        );

        // `NOT <lit> MEMBER OF(col)` (sqlparser 0.62 only accepts the prefix
        // NOT form, not `col NOT MEMBER OF(...)`) flips the list row to a
        // match, but negating unknown stays unknown — the Null/non-list rows
        // must NOT be resurrected.
        let negated = parse("SELECT file.name WHERE NOT 'ios' MEMBER OF(tags)").unwrap();
        assert_eq!(
            execute(&negated, rows.iter(), false).unwrap().rows,
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
        let t = execute(&q, all.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b.md".into())]]);
    }
    #[test]
    fn not_like_excludes_null_field_row() {
        let all = recs_with_null_status();
        let q = parse("SELECT file.name WHERE status NOT LIKE 'dr%'").unwrap();
        let t = execute(&q, all.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b.md".into())]]);
    }
    #[test]
    fn regexp_matches_and_negates_and_over_scalar_fn() {
        let rows = [
            rec(
                "s",
                "s/a.md",
                &[
                    ("jira", Value::Str("DCP-459".into())),
                    ("status", Value::Str("Draft".into())),
                ],
            ),
            rec(
                "s",
                "s/b.md",
                &[
                    ("jira", Value::Str("NOPE-1".into())),
                    ("status", Value::Str("synced".into())),
                ],
            ),
        ];

        let matches = parse("SELECT jira WHERE jira REGEXP '^DCP-[0-9]+$'").unwrap();
        assert_eq!(
            execute(&matches, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("DCP-459".into())]]
        );

        let excludes = parse("SELECT jira WHERE jira NOT REGEXP '^DCP-'").unwrap();
        assert_eq!(
            execute(&excludes, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("NOPE-1".into())]]
        );

        // The operand is a general `Expr`: a scalar-fn call works as the
        // left side, unlike `LIKE`'s column-only left side. `status` is
        // mixed-case (`Draft`); `REGEXP` itself stays case-sensitive, so
        // matching `'draft'` only works once `lower()` normalizes it first.
        let via_scalar = parse("SELECT jira WHERE lower(status) REGEXP 'draft'").unwrap();
        assert_eq!(
            execute(&via_scalar, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("DCP-459".into())]]
        );
    }
    #[test]
    fn not_regexp_excludes_null_field_row() {
        // Mirrors `not_like_excludes_null_field_row`: a NULL operand is
        // "unknown" under 3VL, so `NOT REGEXP` must not resurrect it either.
        let all = recs_with_null_status();
        let q = parse("SELECT file.name WHERE status NOT REGEXP 'dr.*'").unwrap();
        let t = execute(&q, all.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b.md".into())]]);
    }
    #[test]
    fn not_paren_compare_excludes_null_field_row() {
        let all = recs_with_null_status();
        let q = parse("SELECT file.name WHERE NOT (status = 'draft')").unwrap();
        let t = execute(&q, all.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b.md".into())]]);
    }
    #[test]
    fn is_null_and_is_not_null_partition_on_null_field() {
        let all = recs_with_null_status();
        let is_null = parse("SELECT file.name WHERE status IS NULL").unwrap();
        assert_eq!(
            execute(&is_null, all.iter(), false).unwrap().rows,
            vec![vec![Value::Str("c.md".into())]],
            "IS NULL selects only the status-less row"
        );
        let not_null = parse("SELECT file.name WHERE status IS NOT NULL").unwrap();
        assert_eq!(
            execute(&not_null, all.iter(), false).unwrap().rows,
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
            false,
        )
        .unwrap();
        let not_paren = execute(
            &parse("SELECT file.name WHERE NOT (status = 'draft')").unwrap(),
            all.iter(),
            false,
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
        let t = execute(&q, rows.iter(), false).unwrap();
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
            execute(&lt, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("9".into())]],
            "n < 10"
        );

        let gt_flipped = parse("SELECT n WHERE 10 > n").unwrap();
        assert_eq!(
            execute(&gt_flipped, rows.iter(), false).unwrap().rows,
            vec![vec![Value::Str("9".into())]],
            "10 > n must agree with n < 10"
        );

        let gt = parse("SELECT n WHERE n > 10").unwrap();
        assert!(
            execute(&gt, rows.iter(), false).unwrap().rows.is_empty(),
            "n > 10 is false"
        );

        let lt_flipped = parse("SELECT n WHERE 10 < n").unwrap();
        assert!(
            execute(&lt_flipped, rows.iter(), false)
                .unwrap()
                .rows
                .is_empty(),
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
            execute(&eq, numeric.iter(), false).unwrap().rows,
            vec![vec![Value::Str("5.0".into())]]
        );
        let eq_flipped = parse("SELECT status WHERE 5 = status").unwrap();
        assert_eq!(
            execute(&eq_flipped, numeric.iter(), false).unwrap().rows,
            vec![vec![Value::Str("5.0".into())]],
            "5 = status must agree with status = 5"
        );

        let non_numeric = [rec(
            "s",
            "s/b.md",
            &[("status", Value::Str("draft".into()))],
        )];
        let eq2 = parse("SELECT status WHERE status = 5").unwrap();
        assert!(
            execute(&eq2, non_numeric.iter(), false)
                .unwrap()
                .rows
                .is_empty()
        );
        let eq2_flipped = parse("SELECT status WHERE 5 = status").unwrap();
        assert!(
            execute(&eq2_flipped, non_numeric.iter(), false)
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
            execute(&cmp, bounds.iter(), false).unwrap().rows,
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
            execute(&scalar, draft.iter(), false).unwrap().rows,
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
            execute(&arith, arith_rows.iter(), false).unwrap().rows,
            vec![vec![Value::Int(4)]]
        );
    }

    #[test]
    fn where_null_operand_is_unknown_not_match() {
        // `missing` is absent from every record below, so it resolves to
        // `Value::Null`; comparing it against `status` (present on both
        // rows) must be unknown — not a match, and not an error — the same
        // 3VL rule a NULL field already follows on the literal-comparison
        // side. `missing` also isn't in the schema, so this pins the 3VL
        // behavior under `lenient = true`; the strict, non-lenient rejection
        // of a genuinely unknown column is exercised separately, by the
        // `unknown_column_*` tests below.
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
        assert!(execute(&q, rows.iter(), true).unwrap().rows.is_empty());

        let negated = parse("SELECT status WHERE NOT (missing = status)").unwrap();
        assert!(
            execute(&negated, rows.iter(), true)
                .unwrap()
                .rows
                .is_empty(),
            "NOT (unknown) is still unknown, not true — must not resurrect the rows"
        );

        // Same check with the Null on the right-hand operand instead.
        let right_null = parse("SELECT status WHERE status = missing").unwrap();
        assert!(
            execute(&right_null, rows.iter(), true)
                .unwrap()
                .rows
                .is_empty(),
            "a right-side Null operand must also be unknown, not a hard match/no-match"
        );

        let right_null_negated = parse("SELECT status WHERE NOT (status = missing)").unwrap();
        assert!(
            execute(&right_null_negated, rows.iter(), true)
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
        let t_asc = execute(&asc, all.iter(), false).unwrap();
        assert_eq!(t_asc.rows.last().unwrap()[0], Value::Null);

        let desc = parse("SELECT status, file.name ORDER BY status DESC").unwrap();
        let t_desc = execute(&desc, all.iter(), false).unwrap();
        assert_eq!(t_desc.rows.last().unwrap()[0], Value::Null);
    }

    #[test]
    fn unknown_column_errors_with_suggestion() {
        // "staus" is a one-deletion typo of the real field "status".
        let q = parse("SELECT staus").unwrap();
        match execute(&q, recs().iter(), false) {
            Err(ExecError::UnknownColumn { name, suggestion }) => {
                assert_eq!(name, "staus");
                assert_eq!(suggestion.as_deref(), Some("status"));
            }
            other => panic!("expected UnknownColumn, got {other:?}"),
        }
    }

    #[test]
    fn lenient_restores_null_for_unknown_column() {
        // Under `lenient = true`, validation is skipped entirely and an
        // unknown column resolves to `Value::Null` at every row, matching
        // pre-validation behavior.
        let all = recs();
        let q = parse("SELECT staus").unwrap();
        let t = execute(&q, all.iter(), true).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Null]; all.len()]);
    }

    #[test]
    fn empty_store_skips_validation() {
        // An empty record set has no schema to check against — it must not
        // fail the query just because it declares no fields at all.
        let none: Vec<Record> = Vec::new();
        let q = parse("SELECT staus").unwrap();
        let t = execute(&q, none.iter(), false).unwrap();
        assert!(t.rows.is_empty());
    }

    #[test]
    fn record_with_empty_frontmatter_skips_validation() {
        // A lone file with an explicit-but-empty frontmatter mapping
        // (`---\n{}\n---`) yields one record with zero fields: `records` is
        // non-empty, but the schema (the field union) is — validation must
        // key off the schema, not the record count.
        let empty = [rec("s", "s/a.md", &[])];
        let q = parse("SELECT somefield").unwrap();
        let t = execute(&q, empty.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Null]]);
    }

    #[test]
    fn typo_inside_scalar_and_having_is_caught() {
        // Validation must walk into a scalar-function argument, not just a
        // bare `SELECT` column.
        let scalar = parse("SELECT lower(staus)").unwrap();
        assert!(matches!(
            execute(&scalar, recs().iter(), false),
            Err(ExecError::UnknownColumn { .. })
        ));

        // ...and into a HAVING aggregate's argument too.
        let having = parse("SELECT status GROUP BY status HAVING count(staus) > 0").unwrap();
        assert!(matches!(
            execute(&having, recs().iter(), false),
            Err(ExecError::UnknownColumn { .. })
        ));
    }

    #[test]
    fn unknown_column_inside_case_arm_is_caught_strict_and_nulled_lenient() {
        // Validation must walk into a CASE arm's condition too, proving
        // `Query::referenced_fields`/`expr_columns` descend into
        // `Expr::Case` the same as they do a scalar-fn argument (see
        // `typo_inside_scalar_and_having_is_caught` above).
        let q = parse("SELECT CASE WHEN bogus_col = 'x' THEN 1 END").unwrap();
        assert!(matches!(
            execute(&q, recs().iter(), false),
            Err(ExecError::UnknownColumn { .. })
        ));

        // Under `--lenient`, validation is skipped entirely: `bogus_col`
        // resolves to `Value::Null` at every row, no WHEN arm's condition is
        // ever truthy, and (with no ELSE) the whole CASE evaluates to
        // `Value::Null`.
        let all = recs();
        let t = execute(&q, all.iter(), true).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Null]; all.len()]);
    }

    #[test]
    fn dotted_path_select_and_filter_reads_nested_map() {
        // End-to-end wiring check: parse's `ColRef::Field(path)` lowering,
        // schema validation's top-level-only check, `resolve_col`, and
        // `Record::field`'s map walk each have their own unit test, but only
        // this pins that a real query actually threads a dotted path through
        // all of them together.
        let mut estimate = IndexMap::new();
        estimate.insert("low".to_string(), Value::Int(5));
        estimate.insert("high".to_string(), Value::Int(20));
        let rows = [rec("s", "s/a.md", &[("estimate", Value::Map(estimate))])];
        let q = parse("SELECT estimate.low WHERE estimate.high > 10").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(t.headers, vec!["estimate.low"]);
        assert_eq!(t.rows, vec![vec![Value::Int(5)]]);
    }

    #[test]
    fn relative_date_resolves_against_injected_now() {
        use std::time::{Duration, UNIX_EPOCH};

        // now = 2026-07-24T00:00:00Z ; '-7d' must resolve to "2026-07-17".
        //
        // Note: 1_784_851_200 is the correct epoch second for
        // 2026-07-24T00:00:00Z (verified independently via `python3
        // -c 'datetime.datetime(2026,7,24,tzinfo=timezone.utc).timestamp()'`).
        // The task brief's suggested constant, 1_784_246_400, is actually
        // 2026-07-17T00:00:00Z — a week earlier than its own comment claims
        // — which would make BOTH assertions below fail (see task-5-report.md).
        let now = UNIX_EPOCH + Duration::from_secs(1_784_851_200);

        let in_range = rec(
            "s",
            "s/a.md",
            &[("created", Value::Str("2026-07-20".into()))],
        );
        let q = parse("SELECT file.name WHERE created >= '-7d'").unwrap();
        let t = execute_with_schema_at(
            &q,
            std::iter::once(&in_range),
            &["created".to_string()],
            false,
            true,
            now,
        )
        .unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("a.md".into())]]);

        let out_of_range = rec(
            "s",
            "s/b.md",
            &[("created", Value::Str("2026-07-10".into()))],
        );
        let t = execute_with_schema_at(
            &q,
            std::iter::once(&out_of_range),
            &["created".to_string()],
            false,
            true,
            now,
        )
        .unwrap();
        assert!(t.rows.is_empty());
    }

    #[test]
    fn reldate_resolution_uses_calendar_arithmetic_and_now_form() {
        // `today`/offset resolve to a plain `%Y-%m-%d`; `now` resolves to a
        // full RFC3339 instant; `mo`/`y` use calendar (not fixed-length)
        // arithmetic — pinned directly against the fixed instant.
        use std::time::{Duration, UNIX_EPOCH};
        let now = UNIX_EPOCH + Duration::from_secs(1_784_851_200); // 2026-07-24T00:00:00Z

        assert_eq!(resolve_reldate(RelDate::Today, now), "2026-07-24");
        assert_eq!(resolve_reldate(RelDate::Now, now), "2026-07-24T00:00:00Z");
        assert_eq!(
            resolve_reldate(
                RelDate::Offset {
                    n: -2,
                    unit: DateUnit::Month
                },
                now
            ),
            "2026-05-24"
        );
        assert_eq!(
            resolve_reldate(
                RelDate::Offset {
                    n: -1,
                    unit: DateUnit::Year
                },
                now
            ),
            "2025-07-24"
        );
    }

    /// The fixed instant shared by every relative-date behavioral test
    /// below: 2026-07-24T00:00:00Z, so `'-7d'` always resolves to
    /// `"2026-07-17"` (matching `relative_date_resolves_against_injected_now`
    /// above).
    fn fixed_now() -> SystemTime {
        use std::time::{Duration, UNIX_EPOCH};
        UNIX_EPOCH + Duration::from_secs(1_784_851_200)
    }

    #[test]
    fn relative_date_resolves_in_in_list() {
        // `IN (...)` carries a `Vec<Literal>`, a distinct AST position from
        // a `Compare`'s single literal — this must be walked too.
        let now = fixed_now();
        let q = parse("SELECT file.name WHERE created IN ('-7d', 'draft')").unwrap();

        let matching = rec(
            "s",
            "s/a.md",
            &[("created", Value::Str("2026-07-17".into()))],
        );
        let t = execute_with_schema_at(
            &q,
            std::iter::once(&matching),
            &["created".to_string()],
            false,
            true,
            now,
        )
        .unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("a.md".into())]]);

        let non_matching = rec(
            "s",
            "s/b.md",
            &[("created", Value::Str("2026-07-16".into()))],
        );
        let t = execute_with_schema_at(
            &q,
            std::iter::once(&non_matching),
            &["created".to_string()],
            false,
            true,
            now,
        )
        .unwrap();
        assert!(t.rows.is_empty());
    }

    #[test]
    fn relative_date_resolves_in_member_of() {
        // `MEMBER OF`'s left-hand literal is its own AST position, separate
        // from both `Compare` and `In`.
        let now = fixed_now();
        let q = parse("SELECT file.name WHERE '-7d' MEMBER OF(dates)").unwrap();

        let has_date = rec(
            "s",
            "s/a.md",
            &[("dates", Value::List(vec![Value::Str("2026-07-17".into())]))],
        );
        let t = execute_with_schema_at(
            &q,
            std::iter::once(&has_date),
            &["dates".to_string()],
            false,
            true,
            now,
        )
        .unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("a.md".into())]]);

        let missing_date = rec(
            "s",
            "s/b.md",
            &[("dates", Value::List(vec![Value::Str("2026-07-16".into())]))],
        );
        let t = execute_with_schema_at(
            &q,
            std::iter::once(&missing_date),
            &["dates".to_string()],
            false,
            true,
            now,
        )
        .unwrap();
        assert!(t.rows.is_empty());
    }

    #[test]
    fn relative_date_resolves_in_having() {
        // `HAVING`'s literal is resolved before `eval_having` runs, exactly
        // like `WHERE`'s — a group whose aggregate clears the resolved
        // date survives, one that doesn't gets dropped.
        let now = fixed_now();
        let rows = [
            rec(
                "s",
                "s/a1.md",
                &[
                    ("status", Value::Str("a".into())),
                    ("created", Value::Str("2026-07-20".into())),
                ],
            ),
            rec(
                "s",
                "s/b1.md",
                &[
                    ("status", Value::Str("b".into())),
                    ("created", Value::Str("2026-07-10".into())),
                ],
            ),
        ];
        let q = parse("SELECT status GROUP BY status HAVING min(created) >= '-7d'").unwrap();
        let t = execute_with_schema_at(
            &q,
            rows.iter(),
            &["status".to_string(), "created".to_string()],
            false,
            true,
            now,
        )
        .unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("a".into())]]);
    }

    #[test]
    fn relative_date_resolves_in_executed_select_projection() {
        // Executed end-to-end (not just parsed): the projected cell must be
        // the resolved ISO date, not the literal source text `-7d`.
        let now = fixed_now();
        let row = rec("s", "s/a.md", &[]);
        let q = parse("SELECT '-7d' AS d").unwrap();
        let t = execute_with_schema_at(&q, std::iter::once(&row), &[], false, true, now).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("2026-07-17".into())]]);
    }

    #[test]
    fn relative_date_resolves_inside_coalesce_argument() {
        // Regression: `rewrite_expr_literals` must recurse into
        // `Expr::Coalesce`'s arguments, not just `Scalar`/`Binary` — a
        // relative-date literal nested inside `COALESCE(...)` must reach
        // evaluation already resolved to its ISO date, never the literal
        // source text `-7d`. `epic` is absent on the row, so `COALESCE`
        // falls through to the `'-7d'` argument, which must be the
        // *resolved* value here (if the `Coalesce` arm were dropped from
        // `rewrite_expr_literals`, this literal would survive the rewrite
        // as a `Literal::RelativeDate` and panic in `literal_value`, not
        // silently return the wrong string).
        let now = fixed_now();
        let row = rec("s", "s/a.md", &[]);
        let q = parse("SELECT COALESCE(epic, '-7d') AS d").unwrap();
        let t = execute_with_schema_at(&q, std::iter::once(&row), &[], false, true, now).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("2026-07-17".into())]]);
    }

    #[test]
    fn relative_date_rewrite_recurses_into_case_arm() {
        // Regression, mirroring `relative_date_resolves_inside_coalesce_argument`
        // above: `rewrite_expr_literals` must recurse into `Expr::Case`'s
        // WHEN/THEN arms (and its ELSE), not just `Coalesce`/`Scalar`/
        // `Binary`. Pinned directly against the rewritten AST rather than
        // through execution — the arm's literal must become a resolved
        // `Literal::Str`, never survive the rewrite as a
        // `Literal::RelativeDate` (which would panic in `literal_value` once
        // evaluation reached it).
        let mut q = parse("SELECT CASE WHEN status = 'draft' THEN '-7d' ELSE 'none' END").unwrap();
        rewrite_relative_dates(&mut q, fixed_now());

        let SelectExpr::Expr(Expr::Case { whens, .. }) = &q.select[0].expr else {
            panic!("expected the SELECT item to lower to a CASE expression");
        };
        let [(_, then)] = whens.as_slice() else {
            panic!("expected exactly one WHEN arm");
        };
        assert_eq!(then, &Expr::Lit(Literal::Str("2026-07-17".into())));
    }

    #[test]
    fn relative_date_resolves_inside_order_by_case_arm() {
        // Regression: `rewrite_relative_dates` must also walk
        // `OrderTarget::Expr` — without that, a relative-date literal
        // nested inside an `ORDER BY CASE ...` condition would never be
        // visited by the rewrite at all (only `SELECT`/`WHERE`/`HAVING`
        // were walked before this task) and would reach `literal_value` as
        // an unresolved `Literal::RelativeDate`, panicking via its
        // `unreachable!()`.
        let now = fixed_now();
        let recent = rec(
            "s",
            "s/a.md",
            &[("created", Value::Str("2026-07-20".into()))],
        );
        let old = rec(
            "s",
            "s/b.md",
            &[("created", Value::Str("2026-06-01".into()))],
        );
        let q = parse(
            "SELECT file.name ORDER BY CASE WHEN created >= '-7d' THEN 0 ELSE 1 END, file.name",
        )
        .unwrap();
        // Must not panic; the recent row must sort first.
        let t = execute_with_schema_at(
            &q,
            [&old, &recent].into_iter(),
            &["created".to_string()],
            false,
            true,
            now,
        )
        .unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("a.md".into())],
                vec![Value::Str("b.md".into())],
            ]
        );
    }

    #[test]
    fn extreme_offset_magnitude_falls_back_to_str_and_does_not_panic() {
        // Each of these is malformed/out-of-bound enough that
        // `RelDate::parse` must reject it, so it stays a plain
        // `Literal::Str` rather than reaching `resolve_reldate` and
        // overflowing/panicking `chrono::Duration::days`/`weeks`:
        //
        // - a 15-digit offset: within `i64`, but far beyond
        //   `RelDate::parse`'s max offset magnitude;
        // - a doubled or misplaced sign (`--…`, `-+…`, `+-…`): without the
        //   all-digits check on `digits`, the embedded second sign would
        //   let a huge NEGATIVE `n` sail past the `> MAX_OFFSET_MAGNITUDE`
        //   check (which only catches large *positive* values), then get
        //   flipped back to a huge *positive* value by `sign * n` —
        //   bypassing the bound entirely. Reproduced end-to-end
        //   (`TimeDelta::days out of bounds`) before the all-digits guard
        //   was added; see task-5-report.md.
        for token in ["-999999999999999d", "--999999999999999d", "-+7d", "+-7d"] {
            let sql = format!("SELECT file.name WHERE created = '{token}'");
            let q = parse(&sql).unwrap();
            assert_eq!(
                q.filter,
                Some(Predicate::Compare(
                    Expr::Col(ColRef::Field(vec!["created".into()])),
                    CmpOp::Eq,
                    Expr::Lit(Literal::Str(token.into()))
                )),
                "{token} should stay a plain string literal"
            );

            let row = rec(
                "s",
                "s/a.md",
                &[("created", Value::Str("2026-07-17".into()))],
            );
            // Must not panic; the literal text never matches a real date.
            let t = execute(&q, std::iter::once(&row), false).unwrap();
            assert!(t.rows.is_empty(), "{token} should match no rows");
        }
    }

    #[test]
    fn order_by_date_field_is_chronological_with_nulls_last() {
        // I3 (W57 characterization): now that a strict-ISO `created` field
        // auto-detects to `Value::Date` at ingest, `ORDER BY created` must
        // still sort chronologically (== lexically for ISO dates) and place
        // a record with no `created` field (`Value::Null`) last — for both
        // directions — exactly as it did back when the field was a plain
        // `Value::Str` (see `null_field_sorts_last_asc_and_desc` above).
        let rows = [
            rec(
                "s",
                "s/a.md",
                &[(
                    "created",
                    Value::Date(NaiveDate::from_ymd_opt(2026, 7, 24).unwrap()),
                )],
            ),
            rec(
                "s",
                "s/b.md",
                &[(
                    "created",
                    Value::Date(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
                )],
            ),
            rec(
                "s",
                "s/c.md",
                &[(
                    "created",
                    Value::Date(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()),
                )],
            ),
            rec("s", "s/d.md", &[]), // no `created` field -> Value::Null
        ];

        let asc = parse("SELECT file.name ORDER BY created ASC").unwrap();
        let t_asc = execute(&asc, rows.iter(), false).unwrap();
        assert_eq!(
            t_asc.rows,
            vec![
                vec![Value::Str("b.md".into())],
                vec![Value::Str("a.md".into())],
                vec![Value::Str("c.md".into())],
                vec![Value::Str("d.md".into())],
            ]
        );

        // NULL stays last under DESC too — only the non-null comparison
        // reverses (see `order_cmp`).
        let desc = parse("SELECT file.name ORDER BY created DESC").unwrap();
        let t_desc = execute(&desc, rows.iter(), false).unwrap();
        assert_eq!(
            t_desc.rows,
            vec![
                vec![Value::Str("c.md".into())],
                vec![Value::Str("a.md".into())],
                vec![Value::Str("b.md".into())],
                vec![Value::Str("d.md".into())],
            ]
        );
    }

    #[test]
    fn order_by_mixed_date_and_string_column_is_panic_free_with_defined_order() {
        // I4 (W57 characterization): a field that auto-detected to
        // `Value::Date` on some records but stayed `Value::Str` on another
        // (e.g. a non-ISO value like "someday") must still sort without
        // panicking, via `model::compare_dates`'s ISO-text fallback for any
        // pairing that mixes `Date` with `Str`.
        let rows = [
            rec(
                "s",
                "s/a.md",
                &[(
                    "created",
                    Value::Date(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
                )],
            ),
            rec("s", "s/b.md", &[("created", Value::Str("someday".into()))]),
            rec(
                "s",
                "s/c.md",
                &[(
                    "created",
                    Value::Date(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap()),
                )],
            ),
        ];
        let q = parse("SELECT file.name ORDER BY created ASC").unwrap();
        // Must not panic (a non-transitive comparator would corrupt the sort
        // or panic outright); "2026-01-01" < "2026-06-01" < "someday"
        // lexically, so the two dated rows sort first, in date order, and
        // the non-date string sorts last — a defined total order.
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("a.md".into())],
                vec![Value::Str("c.md".into())],
                vec![Value::Str("b.md".into())],
            ]
        );
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
        Record::new(
            Path::new("s"),
            Path::new(path),
            m,
            SystemTime::UNIX_EPOCH,
            0,
            0,
        )
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
        Record::new(
            Path::new("s"),
            Path::new(path),
            m,
            SystemTime::UNIX_EPOCH,
            0,
            0,
        )
    }

    #[test]
    fn count_per_status_renamed_ordered() {
        let q =
            parse("SELECT status, count(*) AS Count GROUP BY status ORDER BY Count DESC").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
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
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(3)]]);
    }
    #[test]
    fn count_distinct() {
        let q = parse("SELECT count(distinct status) AS d").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(2)]]);
    }
    #[test]
    fn group_concat() {
        let q = parse("SELECT prd, group_concat(status) AS ss GROUP BY prd ORDER BY prd").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(
            t.rows[0],
            vec![Value::Str("010".into()), Value::Str("draft, synced".into())]
        );
    }
    #[test]
    fn non_grouped_column_errors() {
        let q = parse("SELECT status, prd, count(*) GROUP BY status").unwrap();
        assert!(matches!(
            execute(&q, recs().iter(), false),
            Err(ExecError::NonGroupedColumn(_))
        ));
    }
    #[test]
    fn grouped_select_expr_over_grouping_key() {
        // `lower(status)` is valid because `status` is a GROUP BY key;
        // it's evaluated over each group's representative row.
        let q =
            parse("SELECT lower(status), count(*) AS n GROUP BY status ORDER BY status").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
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
            execute(&q, recs().iter(), false),
            Err(ExecError::NonGroupedColumn(_))
        ));
    }
    #[test]
    fn grouped_coalesce_over_grouping_key() {
        // `COALESCE(status, 'x')` is valid because `status` is a GROUP BY
        // key; `expr_columns` recurses into COALESCE's arguments the same
        // as any other nested expression.
        let q =
            parse("SELECT COALESCE(status, 'x'), count(*) AS n GROUP BY status ORDER BY status")
                .unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("draft".into()), Value::Int(1)],
                vec![Value::Str("synced".into()), Value::Int(2)],
            ]
        );
    }
    #[test]
    fn grouped_coalesce_referencing_non_group_key_errors() {
        // `prd` isn't a GROUP BY key; wrapping it in `COALESCE(...)` must
        // still be rejected, mirroring `grouped_select_expr_referencing_non_group_key_errors`.
        let q = parse("SELECT COALESCE(prd, 'x'), count(*) GROUP BY status").unwrap();
        assert!(matches!(
            execute(&q, recs().iter(), false),
            Err(ExecError::NonGroupedColumn(_))
        ));
    }
    #[test]
    fn grouped_select_case_over_grouping_key() {
        // `CASE WHEN status = ... END` is valid because it references only
        // `status` (a GROUP BY key); evaluated over each group's
        // representative row, mirroring `grouped_select_expr_over_grouping_key`
        // for `lower(...)` and `grouped_coalesce_over_grouping_key` for
        // `COALESCE(...)` — the grouped-CASE interaction Task 9's reviewer
        // flagged as wired but untested.
        let q = parse(
            "SELECT CASE WHEN status = 'draft' THEN 'D' ELSE 'S' END AS c, count(*) AS n \
             GROUP BY status ORDER BY status",
        )
        .unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("D".into()), Value::Int(1)],
                vec![Value::Str("S".into()), Value::Int(2)],
            ]
        );
    }
    #[test]
    fn grouped_select_case_referencing_non_group_key_errors() {
        // `prd` isn't a GROUP BY key; referencing it inside a CASE arm must
        // still be rejected — validation walks into CASE conditions/results
        // the same as any other nested expression (`expr_columns`'s `Case`
        // arm), mirroring `grouped_coalesce_referencing_non_group_key_errors`.
        let q = parse("SELECT CASE WHEN prd = '010' THEN 1 ELSE 0 END, count(*) GROUP BY status")
            .unwrap();
        assert!(matches!(
            execute(&q, recs().iter(), false),
            Err(ExecError::NonGroupedColumn(_))
        ));
    }
    #[test]
    fn grouped_order_by_case_puts_target_group_first() {
        // Alphabetically "approved" sorts before "draft", so the
        // deterministic pre-`ORDER BY` group sort (`compare_key_tuple`)
        // would put "approved" first; `ORDER BY CASE WHEN status = 'draft'
        // THEN 0 ELSE 1 END` must override that and put "draft" first
        // instead — proving the grouped path resolves `OrderTarget::Expr`
        // (via `eval_group_expr`) rather than silently falling back to the
        // pre-sort's alphabetical order.
        let rows = [
            rec("s/a.md", "approved", "010"),
            rec("s/b.md", "draft", "010"),
            rec("s/c.md", "draft", "011"),
        ];
        let q = parse(
            "SELECT status, count(*) AS n GROUP BY status \
             ORDER BY CASE WHEN status = 'draft' THEN 0 ELSE 1 END",
        )
        .unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("draft".into()), Value::Int(2)],
                vec![Value::Str("approved".into()), Value::Int(1)],
            ]
        );
    }
    #[test]
    fn grouped_literal_expr_survives_zero_row_aggregate_bucket() {
        // A columnless computed expression must still evaluate correctly
        // even when the implicit single group (empty GROUP BY) has zero
        // rows to use as a representative.
        let q = parse("SELECT 1 + 1 AS two, count(*) AS n WHERE status = 'nope'").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(2), Value::Int(0)]]);
    }
    #[test]
    fn sum_and_avg_over_numeric_column() {
        let rows = [
            rec_n("s/a.md", "draft", Value::Int(2)),
            rec_n("s/b.md", "draft", Value::Int(4)),
        ];
        let q = parse("SELECT sum(n) AS total, avg(n) AS mean").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
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
        let t = execute(&q, rows.iter(), false).unwrap();
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
        let t = execute(&q, rows.iter(), false).unwrap();
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
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(1), Value::Int(5)]]);
    }
    #[test]
    fn min_and_max_of_all_null_column_is_null() {
        let rows = [
            rec_n("s/a.md", "draft", Value::Null),
            rec_n("s/b.md", "draft", Value::Null),
        ];
        let q = parse("SELECT min(n) AS lo, max(n) AS hi").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Null, Value::Null]]);
    }
    #[test]
    fn min_and_max_over_date_column() {
        // I3 (W57 characterization): MIN/MAX over a `Value::Date` column
        // must resolve chronologically (== lexically for ISO dates), just
        // as `min_and_max_over_column` above pins for an `Int` column.
        let rows = [
            rec_n(
                "s/a.md",
                "draft",
                Value::Date(NaiveDate::from_ymd_opt(2026, 7, 24).unwrap()),
            ),
            rec_n(
                "s/b.md",
                "draft",
                Value::Date(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
            ),
            rec_n(
                "s/c.md",
                "draft",
                Value::Date(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()),
            ),
        ];
        let q = parse("SELECT min(n) AS lo, max(n) AS hi").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![vec![
                Value::Date(NaiveDate::from_ymd_opt(2026, 1, 1).unwrap()),
                Value::Date(NaiveDate::from_ymd_opt(2026, 12, 31).unwrap()),
            ]]
        );
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
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Int(3), Value::Int(2)]]);
    }
    #[test]
    fn multiple_aggregates_per_group_match_single_pass() {
        // Characterization test for the single-pass `project_group`
        // refactor (Task 11/W40): projects five aggregates over the SAME
        // column, across two groups, with a `NULL` in each group's `n`
        // column — so COUNT(*)'s row count, SUM/AVG's numeric-only count,
        // and MIN/MAX's non-null skipping are all exercised at once. Pins
        // the exact per-group values; must stay green whether each
        // aggregate rescans the group (today) or all five are folded from
        // one pass over the group's rows (after the refactor).
        let rows = [
            rec_n("s/a.md", "draft", Value::Int(2)),
            rec_n("s/b.md", "draft", Value::Int(4)),
            rec_n("s/c.md", "draft", Value::Null),
            rec_n("s/d.md", "synced", Value::Int(10)),
            rec_n("s/e.md", "synced", Value::Null),
        ];
        let q = parse(
            "SELECT status, count(*) AS c, sum(n) AS s, avg(n) AS a, min(n) AS mn, max(n) AS mx \
             GROUP BY status ORDER BY status",
        )
        .unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![
                    Value::Str("draft".into()),
                    Value::Int(3),
                    Value::Float(6.0),
                    Value::Float(3.0),
                    Value::Int(2),
                    Value::Int(4),
                ],
                vec![
                    Value::Str("synced".into()),
                    Value::Int(2),
                    Value::Float(10.0),
                    Value::Float(10.0),
                    Value::Int(10),
                    Value::Int(10),
                ],
            ]
        );
    }
    #[test]
    fn order_by_ungrouped_column_errors() {
        // `prd` isn't a GROUP BY key here, so ordering by it is exactly as
        // invalid as selecting it would be.
        let q = parse("SELECT status, count(*) GROUP BY status ORDER BY prd").unwrap();
        assert!(matches!(
            execute(&q, recs().iter(), false),
            Err(ExecError::NonGroupedColumn(_))
        ));
    }
    #[test]
    fn order_by_bare_aggregate() {
        // status: draft x1, synced x2 (see `recs()`) — no `AS` alias on
        // `count(*)`, yet `ORDER BY count(*) DESC` still sorts groups by it.
        let q = parse("SELECT status, count(*) GROUP BY status ORDER BY count(*) DESC").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("synced".into()), Value::Int(2)],
                vec![Value::Str("draft".into()), Value::Int(1)],
            ]
        );
    }
    #[test]
    fn order_by_bare_aggregate_ascending() {
        // Same groups as `order_by_bare_aggregate`, but `ASC` — pins the
        // sort direction itself so a flipped-comparator bug can't hide
        // behind a DESC-only test.
        let q = parse("SELECT status, count(*) GROUP BY status ORDER BY count(*) ASC").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("draft".into()), Value::Int(1)],
                vec![Value::Str("synced".into()), Value::Int(2)],
            ]
        );
    }
    #[test]
    fn order_by_bare_aggregate_need_not_be_selected() {
        // `count(*)` drives the sort even though it's absent from SELECT —
        // it's computed fresh from each group's rows, same as `HAVING`.
        let q = parse("SELECT status GROUP BY status ORDER BY count(*) DESC").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("synced".into())],
                vec![Value::Str("draft".into())],
            ]
        );
    }
    #[test]
    fn aggregate_with_no_group_by_over_zero_rows_is_one_row_of_zero() {
        let q = parse("SELECT count(*) AS n WHERE status = 'nope'").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
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
        let t = execute(&q, recs().iter(), false).unwrap();
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
        let t = execute(&q, recs().iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("synced".into())]]);
    }
    #[test]
    fn having_can_reference_a_grouping_key() {
        let q =
            parse("SELECT status, count(*) AS n GROUP BY status HAVING status = 'draft'").unwrap();
        let t = execute(&q, recs().iter(), false).unwrap();
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
        let t = execute(&q, recs().iter(), false).unwrap();
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
        let t = execute(&q, rows.iter(), false).unwrap();
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
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("x".into()), Value::Int(2)]]);
    }
    #[test]
    fn grouped_order_by_limit_preserves_tie_input_order_like_full_sort() {
        // Five single-row groups with status a..e (scanned out of order).
        // GROUP BY pre-sorts groups by key tuple (alphabetically) before
        // ORDER BY runs, and every group ties on count(*) == 1, so a full
        // stable sort by count(*) DESC leaves that alphabetical order
        // untouched — LIMIT 3 must return a, b, c, matching the ungrouped
        // tie-stability pin above.
        let rows = [
            rec("s/c.md", "c", "010"),
            rec("s/a.md", "a", "010"),
            rec("s/e.md", "e", "010"),
            rec("s/b.md", "b", "010"),
            rec("s/d.md", "d", "010"),
        ];
        let q =
            parse("SELECT status, count(*) AS n GROUP BY status ORDER BY count(*) DESC LIMIT 3")
                .unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("a".into()), Value::Int(1)],
                vec![Value::Str("b".into()), Value::Int(1)],
                vec![Value::Str("c".into()), Value::Int(1)],
            ]
        );
    }
    #[test]
    fn grouped_order_by_limit_offset_window_matches_full_sort() {
        // Same five tied groups as above; LIMIT 2 OFFSET 1 must equal
        // full-sort(a, b, c, d, e).skip(1).take(2) = b, c.
        let rows = [
            rec("s/c.md", "c", "010"),
            rec("s/a.md", "a", "010"),
            rec("s/e.md", "e", "010"),
            rec("s/b.md", "b", "010"),
            rec("s/d.md", "d", "010"),
        ];
        let q = parse(
            "SELECT status, count(*) AS n GROUP BY status ORDER BY count(*) DESC LIMIT 2 OFFSET 1",
        )
        .unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![Value::Str("b".into()), Value::Int(1)],
                vec![Value::Str("c".into()), Value::Int(1)],
            ]
        );
    }
    #[test]
    fn group_by_key_does_not_collide_ambiguous_concatenations() {
        // Two records whose grouping-column values are ("a","b") and
        // ("ab","") must form TWO distinct groups, not one — a naive
        // join-on-empty-sep key would collide them. Pins that the hashed
        // bucketing keys on the per-cell string vector, not a joined string.
        let rows = [rec("s/a.md", "a", "b"), rec("s/b.md", "ab", "")];
        let q = parse("SELECT status, prd, count(*) AS n GROUP BY status, prd").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows,
            vec![
                vec![
                    Value::Str("a".into()),
                    Value::Str("b".into()),
                    Value::Int(1)
                ],
                vec![
                    Value::Str("ab".into()),
                    Value::Str("".into()),
                    Value::Int(1)
                ],
            ]
        );
    }
    #[test]
    fn group_by_key_preserves_value_variant_identity() {
        // Four rows share a single grouping column `n` whose values are
        // type-distinct but display identically: `Int(1)`/`Str("1")` both
        // "1", and `Null`/`Str("")` both "". A hash key built from
        // `Value::to_cmp_string()` alone (display, lossy across variants)
        // would collapse these into 2 groups; the previous structural
        // `Vec<Value>` `PartialEq` key — and the fixed hash key — keep all 4
        // apart.
        let rows = [
            rec_n("s/a.md", "x", Value::Int(1)),
            rec_n("s/b.md", "x", Value::Str("1".into())),
            rec_n("s/c.md", "x", Value::Null),
            rec_n("s/d.md", "x", Value::Str("".into())),
        ];
        let q = parse("SELECT n, count(*) AS n_count GROUP BY n").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(t.rows.len(), 4);
        assert!(t.rows.iter().all(|row| row[1] == Value::Int(1)));
    }

    /// MUST-FIX #3 (Finding A) characterization: `main`'s GROUP BY key is a
    /// structural `Vec<Value>` `PartialEq`, under which `[Str("a"),
    /// Str("b")]` and `[Str("a, b")]` are different — even though
    /// `Value::display()`'s `", "`-join renders both `"a, b"`, which would
    /// merge them into one group if `hashable_cell_key` still hashed a
    /// list's `to_cmp_string()` directly instead of recursing into its
    /// elements.
    #[test]
    fn group_by_list_column_distinguishes_structurally_different_lists() {
        let rows = [
            rec_n(
                "s/a.md",
                "x",
                Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
            ),
            rec_n("s/b.md", "x", Value::List(vec![Value::Str("a, b".into())])),
        ];

        let q = parse("SELECT n, count(*) AS n_count GROUP BY n").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows.len(),
            2,
            "[a, b] and [\"a, b\"] must land in different groups, matching \
             main's structural Vec<Value> equality; got: {:?}",
            t.rows
        );
        assert!(t.rows.iter().all(|row| row[1] == Value::Int(1)));
    }

    /// MUST-FIX #3 (Finding A) characterization: `f64`'s `PartialEq` (and so
    /// `Value`'s derived one) treats `0.0 == -0.0`, so `main`'s structural
    /// GROUP BY key puts them in the SAME group — which `hashable_cell_key`
    /// would miss without normalizing `-0.0` away, since `"-0"` and `"0"`
    /// are different strings.
    #[test]
    fn group_by_float_column_treats_positive_and_negative_zero_as_one_group() {
        let rows = [
            rec_n("s/a.md", "x", Value::Float(0.0)),
            rec_n("s/b.md", "x", Value::Float(-0.0)),
        ];

        let q = parse("SELECT n, count(*) AS n_count GROUP BY n").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows.len(),
            1,
            "0.0 and -0.0 must land in the SAME group, matching main's \
             structural Value PartialEq; got: {:?}",
            t.rows
        );
        assert_eq!(t.rows[0][1], Value::Int(2));
    }

    /// M1 characterization: a `Value::Map` cell used to fall through the
    /// wildcard arm and key on its compact `{k: v}` `to_cmp_string()` form,
    /// which is lossy the same way `List`'s `", "` join was — `{a: "x", b:
    /// "y"}` and the single-key `{a: "x, b: y"}` both compact-render `{a: x,
    /// b: y}`. The structural `Map` arm recurses per-entry instead, so these
    /// two structurally-different maps must land in different groups.
    #[test]
    fn group_by_map_column_distinguishes_structurally_different_maps() {
        let mut two_entries = IndexMap::new();
        two_entries.insert("a".to_string(), Value::Str("x".into()));
        two_entries.insert("b".to_string(), Value::Str("y".into()));

        let mut one_entry = IndexMap::new();
        one_entry.insert("a".to_string(), Value::Str("x, b: y".into()));

        let rows = [
            rec_n("s/a.md", "x", Value::Map(two_entries)),
            rec_n("s/b.md", "x", Value::Map(one_entry)),
        ];

        let q = parse("SELECT n, count(*) AS n_count GROUP BY n").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows.len(),
            2,
            "{{a: x, b: y}} and {{a: \"x, b: y\"}} must land in different \
             groups, matching main's structural Value PartialEq; got: {:?}",
            t.rows
        );
        assert!(t.rows.iter().all(|row| row[1] == Value::Int(1)));
    }

    /// M1 characterization: `IndexMap`'s `PartialEq` (and so `Value`'s
    /// derived one) is order-insensitive, so two maps with the same entries
    /// in different insertion order are structurally equal and must land in
    /// the SAME group — which `hashable_cell_key`'s `Map` arm achieves by
    /// sorting entries by key before building the key string.
    #[test]
    fn group_by_map_column_is_order_insensitive() {
        let mut ab = IndexMap::new();
        ab.insert("a".to_string(), Value::Int(1));
        ab.insert("b".to_string(), Value::Int(2));

        let mut ba = IndexMap::new();
        ba.insert("b".to_string(), Value::Int(2));
        ba.insert("a".to_string(), Value::Int(1));

        let rows = [
            rec_n("s/a.md", "x", Value::Map(ab)),
            rec_n("s/b.md", "x", Value::Map(ba)),
        ];

        let q = parse("SELECT n, count(*) AS n_count GROUP BY n").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();
        assert_eq!(
            t.rows.len(),
            1,
            "{{a: 1, b: 2}} and {{b: 2, a: 1}} must land in the SAME group, \
             matching main's structural Value PartialEq; got: {:?}",
            t.rows
        );
        assert_eq!(t.rows[0][1], Value::Int(2));
    }

    #[test]
    fn group_by_many_distinct_groups_partitions_all_rows() {
        // High-cardinality guard for the hash-keyed bucketing: 500 records
        // each with a unique `status` plus 500 more sharing one `status`
        // must land in 501 groups with none dropped or merged wrongly, as
        // cardinality grows past what a linear scan would handle cheaply.
        let mut rows: Vec<Record> = (0..500)
            .map(|i| rec(&format!("s/u{i}.md"), &format!("status-{i}"), "010"))
            .collect();
        rows.extend((0..500).map(|i| rec(&format!("s/d{i}.md"), "dup", "010")));

        let q = parse("SELECT status, count(*) AS n GROUP BY status").unwrap();
        let t = execute(&q, rows.iter(), false).unwrap();

        assert_eq!(t.rows.len(), 501);
        let total: i64 = t
            .rows
            .iter()
            .filter_map(|row| match &row[1] {
                Value::Int(n) => Some(*n),
                _ => None,
            })
            .sum();
        assert_eq!(total, 1000);
        assert!(
            t.rows
                .contains(&vec![Value::Str("dup".into()), Value::Int(500)])
        );
    }

    /// Task 7 (W56 part 2): `file.body` lazy eval-time disk read.
    mod file_body {
        use super::*;
        use tempfile::TempDir;

        /// Writes `content` to `dir`/`rel` (creating parent directories as
        /// needed) and returns a [`Record`] whose `abs_path` points at that
        /// real file — unlike [`super::rec`]'s fake root/path, `file.body`
        /// needs a file that's actually readable from disk.
        fn rec_on_disk(dir: &Path, rel: &str, content: &str, kv: &[(&str, Value)]) -> Record {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, content).unwrap();
            let mut fields = IndexMap::new();
            for (k, v) in kv {
                fields.insert((*k).to_string(), v.clone());
            }
            Record::new(dir, &path, fields, SystemTime::UNIX_EPOCH, 0, 0)
        }

        /// Test 1 (brief): `WHERE file.body LIKE '%TODO%'` matches a fixture
        /// whose body contains it, end to end through `execute` (disk reads
        /// allowed by default there).
        #[test]
        fn like_matches_a_body_containing_the_pattern() {
            let td = TempDir::new().unwrap();
            let has_todo = rec_on_disk(
                td.path(),
                "a.md",
                "---\nstatus: draft\n---\nTODO fix this\n",
                &[("status", Value::Str("draft".into()))],
            );
            let no_todo = rec_on_disk(
                td.path(),
                "b.md",
                "---\nstatus: draft\n---\nall done\n",
                &[("status", Value::Str("draft".into()))],
            );

            let q = parse("SELECT file.name WHERE file.body LIKE '%TODO%'").unwrap();
            let t = execute(&q, [&has_todo, &no_todo].into_iter(), false).unwrap();
            assert_eq!(t.rows, vec![vec![Value::Str("a.md".into())]]);
        }

        /// `SELECT file.body` returns the text after the frontmatter fence —
        /// not the raw file (fence included) — matching what `word_count`
        /// already counts (design W56).
        #[test]
        fn select_file_body_returns_text_after_the_fence() {
            let td = TempDir::new().unwrap();
            let r = rec_on_disk(
                td.path(),
                "a.md",
                "---\nstatus: draft\n---\nhello world\n",
                &[],
            );

            let q = parse("SELECT file.body").unwrap();
            let t = execute(&q, std::iter::once(&r), false).unwrap();
            assert_eq!(t.rows, vec![vec![Value::Str("hello world".into())]]);
        }

        /// Test 2 (brief), lenient half: under `--force-cache`
        /// (`disk_reads_allowed = false`), `file.body` resolves to `Null` per
        /// row rather than a wrong/stale answer — `--lenient` set.
        #[test]
        fn force_cache_lenient_yields_null() {
            let td = TempDir::new().unwrap();
            let r = rec_on_disk(td.path(), "a.md", "---\nstatus: draft\n---\nTODO\n", &[]);

            let q = parse("SELECT file.body").unwrap();
            let t = execute_with_schema_at(
                &q,
                std::iter::once(&r),
                &[],
                true,  // lenient
                false, // disk_reads_allowed
                SystemTime::now(),
            )
            .unwrap();
            assert_eq!(t.rows, vec![vec![Value::Null]]);
        }

        /// Test 2 (brief), strict half: under `--force-cache` WITHOUT
        /// `--lenient`, a `file.body` reference fails the whole query with a
        /// clear diagnostic rather than silently returning `Null` rows — the
        /// "never a silent wrong answer" invariant (design W56).
        #[test]
        fn force_cache_strict_is_a_clear_diagnostic_not_null() {
            let td = TempDir::new().unwrap();
            let r = rec_on_disk(td.path(), "a.md", "---\nstatus: draft\n---\nTODO\n", &[]);

            let q = parse("SELECT file.body").unwrap();
            let err = execute_with_schema_at(
                &q,
                std::iter::once(&r),
                &[],
                false, // strict
                false, // disk_reads_allowed
                SystemTime::now(),
            )
            .unwrap_err();
            assert!(matches!(err, ExecError::BodyUnavailable));
            assert!(err.to_string().contains("force-cache"));
        }

        /// `file.body` referenced only inside `WHERE` (not `SELECT`) must
        /// still trip the strict `--force-cache` diagnostic — `references_body`
        /// walks every clause, not just the projection.
        #[test]
        fn force_cache_strict_catches_a_where_only_reference() {
            let td = TempDir::new().unwrap();
            let r = rec_on_disk(td.path(), "a.md", "---\nstatus: draft\n---\nTODO\n", &[]);

            let q = parse("SELECT file.name WHERE file.body LIKE '%TODO%'").unwrap();
            let err = execute_with_schema_at(
                &q,
                std::iter::once(&r),
                &[],
                false,
                false,
                SystemTime::now(),
            )
            .unwrap_err();
            assert!(matches!(err, ExecError::BodyUnavailable));
        }

        /// Test 3 (brief): the I/O-regression guard. Same on-disk record used
        /// two ways after its file is deleted:
        /// - a query that does NOT reference `file.body` (`SELECT status`)
        ///   must still succeed with the correct in-memory field value —
        ///   proving evaluation never touched the now-missing file for this
        ///   query, since `resolve_col` only special-cases `FileAttr::Body`
        ///   and every other column resolves straight off the `Record`'s
        ///   already-parsed fields.
        /// - the SAME record's `file.body`, evaluated separately, resolves to
        ///   `Null` — proving the deleted file really is unreadable now, so
        ///   the first query's success isn't a coincidence of the delete
        ///   being a no-op.
        #[test]
        fn frontmatter_only_query_succeeds_after_the_file_is_deleted() {
            let td = TempDir::new().unwrap();
            let r = rec_on_disk(
                td.path(),
                "a.md",
                "---\nstatus: draft\n---\nTODO fix this\n",
                &[("status", Value::Str("draft".into()))],
            );
            fs::remove_file(r.abs_path()).unwrap();

            // No `file.body` reference anywhere in this query: must succeed,
            // reading `status` straight from the in-memory `Record`.
            let no_body_ref = parse("SELECT status WHERE status = 'draft'").unwrap();
            let t = execute(&no_body_ref, std::iter::once(&r), false).unwrap();
            assert_eq!(t.rows, vec![vec![Value::Str("draft".into())]]);

            // Contrast: the same (now-deleted) file's body really is
            // unreadable — confirming the delete above was real, not a no-op.
            let body_ref = parse("SELECT file.body").unwrap();
            let t = execute(&body_ref, std::iter::once(&r), false).unwrap();
            assert_eq!(t.rows, vec![vec![Value::Null]]);
        }
    }
}
