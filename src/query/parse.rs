//! Lowering from a SQL string to the querymatter [`Query`] AST.
//!
//! Parsing is a two-step pipeline. First, an optional **quoted** `FROM` glob
//! (`FROM 'plans/**'`) is extracted with a regex and stripped, because it is
//! not valid SQL for `sqlparser` to consume. The remainder is then handed to
//! `sqlparser` under the [`GenericDialect`], and the resulting parse tree is
//! lowered node-by-node into the [`Query`] AST. A **bare** `FROM plans` is left
//! for `sqlparser` and read back from the table name.
//!
//! Anything outside the supported subset — joins, subqueries, `HAVING`,
//! whole-query `DISTINCT`, set operations, multiple statements, or any node
//! kind the lowering does not translate — is rejected with a [`ParseError`]
//! rather than silently ignored.

use once_cell::sync::Lazy;
use regex::Regex;
use sqlparser::ast as sql;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::model::FileAttr;
use crate::query::ast::{
    Aggregate, CmpOp, ColRef, Literal, OrderKey, OrderTarget, Predicate, Query, SelectExpr,
    SelectItem,
};

/// An error produced while parsing or lowering a query.
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    /// The SQL failed `sqlparser`'s own grammar (syntax error, unexpected
    /// token, empty input, …).
    #[error("SQL syntax error: {0}")]
    Sql(String),
    /// The SQL parsed, but uses a construct outside querymatter's subset.
    #[error("unsupported query feature: {0}")]
    Unsupported(String),
    /// A column reference could not be interpreted (e.g. an unknown `file.*`
    /// attribute or an unsupported compound identifier).
    #[error("invalid column reference: {0}")]
    BadColumn(String),
}

/// Matches an optional quoted `FROM` glob. Group 1 is the whole quoted token;
/// groups 2 and 3 are the single- and double-quoted inner contents. Only the
/// quoted-glob form is matched here; a bare-identifier `FROM` is left for
/// `sqlparser`.
static FROM_GLOB: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?i)\bfrom\s+('([^']*)'|"([^"]*)")"#).expect("valid FROM regex"));

/// Parses `sql` into a [`Query`], rejecting anything outside the supported
/// subset with a descriptive [`ParseError`].
pub fn parse(sql: &str) -> Result<Query, ParseError> {
    let (rest, quoted_glob) = extract_quoted_from(sql);

    let statements =
        Parser::parse_sql(&GenericDialect, &rest).map_err(|e| ParseError::Sql(e.to_string()))?;

    let [statement] = statements.as_slice() else {
        return Err(ParseError::Unsupported(
            "expected exactly one SQL statement".to_string(),
        ));
    };
    let sql::Statement::Query(query) = statement else {
        return Err(ParseError::Unsupported(
            "only SELECT queries are supported".to_string(),
        ));
    };

    lower_query(query, quoted_glob)
}

/// Splits off an optional quoted `FROM '<glob>'` clause, returning the SQL with
/// that clause replaced by a space and the captured glob (if any).
fn extract_quoted_from(sql: &str) -> (String, Option<String>) {
    let Some(caps) = FROM_GLOB.captures(sql) else {
        return (sql.to_string(), None);
    };
    let glob = caps
        .get(2)
        .or_else(|| caps.get(3))
        .map(|m| m.as_str().to_string());
    let whole = caps.get(0).expect("group 0 always present");
    let mut rest = String::with_capacity(sql.len());
    rest.push_str(&sql[..whole.start()]);
    rest.push(' ');
    rest.push_str(&sql[whole.end()..]);
    (rest, glob)
}

/// Lowers a `sqlparser` query into the querymatter [`Query`] AST.
///
/// `quoted_glob` is the glob captured by [`extract_quoted_from`]; when present
/// it takes precedence over any table name in the parsed `FROM`.
fn lower_query(query: &sql::Query, quoted_glob: Option<String>) -> Result<Query, ParseError> {
    if query.with.is_some() {
        return Err(unsupported("WITH / common table expressions"));
    }
    if !query.locks.is_empty() {
        return Err(unsupported("FOR UPDATE / FOR SHARE locking clauses"));
    }

    let select = match query.body.as_ref() {
        sql::SetExpr::Select(select) => select.as_ref(),
        sql::SetExpr::SetOperation { .. } => {
            return Err(unsupported("set operations (UNION / INTERSECT / EXCEPT)"));
        }
        other => return Err(unsupported(format!("query body: {other:?}"))),
    };

    if select.distinct.is_some() {
        return Err(unsupported("DISTINCT on the whole SELECT"));
    }
    if select.having.is_some() {
        return Err(unsupported("HAVING clause"));
    }

    let from_glob = match quoted_glob {
        Some(glob) => Some(glob),
        None => lower_from(&select.from)?,
    };

    let select_items = select
        .projection
        .iter()
        .map(lower_select_item)
        .collect::<Result<Vec<_>, _>>()?;
    let aliases: Vec<&str> = select_items
        .iter()
        .filter_map(|item| item.alias.as_deref())
        .collect();

    let filter = select.selection.as_ref().map(lower_predicate).transpose()?;
    let group_by = lower_group_by(&select.group_by)?;
    let order_by = lower_order_by(query.order_by.as_ref(), &aliases)?;
    let (limit, offset) = lower_limit(query.limit_clause.as_ref())?;

    Ok(Query {
        select: select_items,
        from_glob,
        filter,
        group_by,
        order_by,
        limit,
        offset,
    })
}

/// Resolves a parsed `FROM` clause (bare identifier form only) to a glob.
fn lower_from(from: &[sql::TableWithJoins]) -> Result<Option<String>, ParseError> {
    match from {
        [] => Ok(None),
        [single] => {
            if !single.joins.is_empty() {
                return Err(unsupported("JOIN clauses"));
            }
            match &single.relation {
                sql::TableFactor::Table { name, .. } => Ok(Some(object_name_to_string(name))),
                other => Err(unsupported(format!(
                    "FROM must be a bare identifier or quoted glob, found {other:?}"
                ))),
            }
        }
        _ => Err(unsupported("multiple tables in FROM")),
    }
}

/// Lowers one projection item.
fn lower_select_item(item: &sql::SelectItem) -> Result<SelectItem, ParseError> {
    match item {
        sql::SelectItem::UnnamedExpr(expr) => Ok(SelectItem {
            expr: lower_select_expr(expr)?,
            alias: None,
        }),
        sql::SelectItem::ExprWithAlias { expr, alias } => Ok(SelectItem {
            expr: lower_select_expr(expr)?,
            alias: Some(alias.value.clone()),
        }),
        sql::SelectItem::Wildcard(_) => Ok(SelectItem {
            expr: SelectExpr::Star,
            alias: None,
        }),
        sql::SelectItem::QualifiedWildcard(..) => Err(unsupported("qualified wildcard (`t.*`)")),
        sql::SelectItem::ExprWithAliases { .. } => {
            Err(unsupported("multiple aliases on one projection item"))
        }
    }
}

/// Lowers a projection expression to a [`SelectExpr`] (a column or aggregate;
/// the `*` wildcard is handled one level up in [`lower_select_item`]).
fn lower_select_expr(expr: &sql::Expr) -> Result<SelectExpr, ParseError> {
    match expr {
        sql::Expr::Function(func) => Ok(SelectExpr::Agg(lower_aggregate(func)?)),
        other => Ok(SelectExpr::Col(lower_col_ref(other)?)),
    }
}

/// Lowers an aggregate function call.
fn lower_aggregate(func: &sql::Function) -> Result<Aggregate, ParseError> {
    if func.over.is_some() {
        return Err(unsupported("window functions (OVER clause)"));
    }
    if func.filter.is_some() {
        return Err(unsupported("aggregate FILTER clause"));
    }

    let name = object_name_to_string(&func.name).to_ascii_lowercase();
    match name.as_str() {
        "count" => lower_count(func),
        "min" => Ok(Aggregate::Min(single_col_arg(func, "min")?)),
        "max" => Ok(Aggregate::Max(single_col_arg(func, "max")?)),
        "sum" => Ok(Aggregate::Sum(single_col_arg(func, "sum")?)),
        "avg" => Ok(Aggregate::Avg(single_col_arg(func, "avg")?)),
        "group_concat" => Ok(Aggregate::GroupConcat(single_col_arg(
            func,
            "group_concat",
        )?)),
        other => Err(unsupported(format!("function `{other}`"))),
    }
}

/// Lowers a `count(...)` call, distinguishing `count(*)`, `count(col)`, and
/// `count(distinct col)`.
fn lower_count(func: &sql::Function) -> Result<Aggregate, ParseError> {
    let list = arg_list(func, "count")?;
    let distinct = matches!(
        list.duplicate_treatment,
        Some(sql::DuplicateTreatment::Distinct)
    );
    let [arg] = list.args.as_slice() else {
        return Err(unsupported("count() takes exactly one argument"));
    };
    match arg {
        sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Wildcard) => {
            if distinct {
                return Err(unsupported("count(distinct *)"));
            }
            Ok(Aggregate::CountStar)
        }
        sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(expr)) => {
            Ok(Aggregate::Count(lower_col_ref(expr)?, distinct))
        }
        other => Err(unsupported(format!("count argument: {other:?}"))),
    }
}

/// Extracts the single column argument of an aggregate such as `min`/`sum`.
fn single_col_arg(func: &sql::Function, name: &str) -> Result<ColRef, ParseError> {
    let list = arg_list(func, name)?;
    let [arg] = list.args.as_slice() else {
        return Err(unsupported(format!("{name}() takes exactly one argument")));
    };
    match arg {
        sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(expr)) => lower_col_ref(expr),
        other => Err(unsupported(format!("{name} argument: {other:?}"))),
    }
}

/// Returns the parenthesized argument list of a function call, rejecting the
/// paren-less and subquery-argument forms.
fn arg_list<'a>(
    func: &'a sql::Function,
    name: &str,
) -> Result<&'a sql::FunctionArgumentList, ParseError> {
    match &func.args {
        sql::FunctionArguments::List(list) => Ok(list),
        sql::FunctionArguments::None => {
            Err(unsupported(format!("{name} requires an argument list")))
        }
        sql::FunctionArguments::Subquery(_) => {
            Err(unsupported(format!("{name} with a subquery argument")))
        }
    }
}

/// Lowers a column reference: a bare identifier (a frontmatter field) or a
/// `file.<attr>` compound identifier (a pseudo-column).
fn lower_col_ref(expr: &sql::Expr) -> Result<ColRef, ParseError> {
    match expr {
        sql::Expr::Identifier(ident) => Ok(ColRef::Field(ident.value.clone())),
        sql::Expr::CompoundIdentifier(parts) => lower_compound(parts),
        other => Err(ParseError::BadColumn(format!(
            "expected a column reference, found {other}"
        ))),
    }
}

/// Lowers a compound identifier; only the `file.<attr>` form is supported.
fn lower_compound(parts: &[sql::Ident]) -> Result<ColRef, ParseError> {
    if let [prefix, attr] = parts
        && prefix.value.eq_ignore_ascii_case("file")
    {
        return Ok(ColRef::File(file_attr_from_str(&attr.value)?));
    }
    let joined = parts
        .iter()
        .map(|p| p.value.as_str())
        .collect::<Vec<_>>()
        .join(".");
    Err(ParseError::BadColumn(format!(
        "unsupported compound column `{joined}`"
    )))
}

/// Parses a `file.*` attribute name into a [`FileAttr`].
fn file_attr_from_str(name: &str) -> Result<FileAttr, ParseError> {
    match name.to_ascii_lowercase().as_str() {
        "name" => Ok(FileAttr::Name),
        "path" => Ok(FileAttr::Path),
        "folder" => Ok(FileAttr::Folder),
        "ext" => Ok(FileAttr::Ext),
        other => Err(ParseError::BadColumn(format!(
            "unknown file attribute `file.{other}`"
        ))),
    }
}

/// Lowers a `WHERE` expression into a [`Predicate`] tree.
fn lower_predicate(expr: &sql::Expr) -> Result<Predicate, ParseError> {
    match expr {
        sql::Expr::BinaryOp { left, op, right } => lower_binary(left, op, right),
        sql::Expr::Like {
            negated,
            expr,
            pattern,
            ..
        } => Ok(Predicate::Like(
            lower_col_ref(expr)?,
            string_literal(pattern)?,
            *negated,
        )),
        sql::Expr::InList {
            expr,
            list,
            negated,
        } => {
            let literals = list.iter().map(lower_literal).collect::<Result<_, _>>()?;
            Ok(Predicate::In(lower_col_ref(expr)?, literals, *negated))
        }
        sql::Expr::IsNull(inner) => Ok(Predicate::IsNull(lower_col_ref(inner)?, false)),
        sql::Expr::IsNotNull(inner) => Ok(Predicate::IsNull(lower_col_ref(inner)?, true)),
        sql::Expr::Nested(inner) => lower_predicate(inner),
        sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Not,
            expr,
        } => Ok(Predicate::Not(Box::new(lower_predicate(expr)?))),
        other => Err(unsupported(format!("WHERE expression: {other:?}"))),
    }
}

/// Lowers a binary operation: a boolean connective (`AND`/`OR`) or a comparison
/// of a column against a literal.
fn lower_binary(
    left: &sql::Expr,
    op: &sql::BinaryOperator,
    right: &sql::Expr,
) -> Result<Predicate, ParseError> {
    use sql::BinaryOperator as B;
    let cmp = match op {
        B::And => {
            return Ok(Predicate::And(
                Box::new(lower_predicate(left)?),
                Box::new(lower_predicate(right)?),
            ));
        }
        B::Or => {
            return Ok(Predicate::Or(
                Box::new(lower_predicate(left)?),
                Box::new(lower_predicate(right)?),
            ));
        }
        B::Eq => CmpOp::Eq,
        B::NotEq => CmpOp::Ne,
        B::Lt => CmpOp::Lt,
        B::LtEq => CmpOp::Le,
        B::Gt => CmpOp::Gt,
        B::GtEq => CmpOp::Ge,
        other => return Err(unsupported(format!("operator `{other}`"))),
    };
    Ok(Predicate::Compare(
        lower_col_ref(left)?,
        cmp,
        lower_literal(right)?,
    ))
}

/// Lowers a literal expression (a value, or a signed numeric literal).
fn lower_literal(expr: &sql::Expr) -> Result<Literal, ParseError> {
    match expr {
        sql::Expr::Value(value) => value_to_literal(&value.value),
        sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Minus,
            expr,
        } => negate_literal(expr),
        sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Plus,
            expr,
        } => lower_literal(expr),
        other => Err(unsupported(format!("expected a literal, found {other:?}"))),
    }
}

/// Negates a numeric literal (for `-<number>`).
fn negate_literal(expr: &sql::Expr) -> Result<Literal, ParseError> {
    match lower_literal(expr)? {
        Literal::Int(i) => Ok(Literal::Int(-i)),
        Literal::Float(f) => Ok(Literal::Float(-f)),
        other => Err(unsupported(format!("cannot negate {other:?}"))),
    }
}

/// Maps a `sqlparser` value literal to a querymatter [`Literal`].
fn value_to_literal(value: &sql::Value) -> Result<Literal, ParseError> {
    match value {
        sql::Value::Number(n, _) => parse_number(n),
        sql::Value::SingleQuotedString(s)
        | sql::Value::DoubleQuotedString(s)
        | sql::Value::TripleSingleQuotedString(s)
        | sql::Value::TripleDoubleQuotedString(s)
        | sql::Value::EscapedStringLiteral(s)
        | sql::Value::UnicodeStringLiteral(s)
        | sql::Value::NationalStringLiteral(s) => Ok(Literal::Str(s.clone())),
        sql::Value::Boolean(b) => Ok(Literal::Bool(*b)),
        sql::Value::Null => Ok(Literal::Null),
        other => Err(unsupported(format!("value literal: {other:?}"))),
    }
}

/// Parses a numeric literal, preferring an integer and falling back to a float.
fn parse_number(n: &str) -> Result<Literal, ParseError> {
    if let Ok(i) = n.parse::<i64>() {
        Ok(Literal::Int(i))
    } else if let Ok(f) = n.parse::<f64>() {
        Ok(Literal::Float(f))
    } else {
        Err(ParseError::Sql(format!("invalid numeric literal `{n}`")))
    }
}

/// Extracts a string literal, rejecting non-string values (used for `LIKE`
/// patterns).
fn string_literal(expr: &sql::Expr) -> Result<String, ParseError> {
    match lower_literal(expr)? {
        Literal::Str(s) => Ok(s),
        other => Err(unsupported(format!(
            "LIKE pattern must be a string, found {other:?}"
        ))),
    }
}

/// Lowers a `GROUP BY` clause into an ordered list of column references.
fn lower_group_by(group_by: &sql::GroupByExpr) -> Result<Vec<ColRef>, ParseError> {
    match group_by {
        sql::GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(unsupported("GROUP BY modifiers (ROLLUP / CUBE / …)"));
            }
            exprs.iter().map(lower_col_ref).collect()
        }
        sql::GroupByExpr::All(_) => Err(unsupported("GROUP BY ALL")),
    }
}

/// Lowers an `ORDER BY` clause, resolving identifiers that match a projection
/// alias to [`OrderTarget::Alias`] and everything else to a column.
fn lower_order_by(
    order_by: Option<&sql::OrderBy>,
    aliases: &[&str],
) -> Result<Vec<OrderKey>, ParseError> {
    let Some(order_by) = order_by else {
        return Ok(Vec::new());
    };
    let exprs = match &order_by.kind {
        sql::OrderByKind::Expressions(exprs) => exprs,
        sql::OrderByKind::All(_) => return Err(unsupported("ORDER BY ALL")),
    };
    exprs
        .iter()
        .map(|order| lower_order_expr(order, aliases))
        .collect()
}

/// Lowers a single `ORDER BY` term.
fn lower_order_expr(order: &sql::OrderByExpr, aliases: &[&str]) -> Result<OrderKey, ParseError> {
    let desc = order.options.asc == Some(false);
    let target = match &order.expr {
        sql::Expr::Identifier(ident) if aliases.contains(&ident.value.as_str()) => {
            OrderTarget::Alias(ident.value.clone())
        }
        other => OrderTarget::Col(lower_col_ref(other)?),
    };
    Ok(OrderKey { target, desc })
}

/// Lowers a `LIMIT` / `OFFSET` clause into `(limit, offset)`.
fn lower_limit(
    limit_clause: Option<&sql::LimitClause>,
) -> Result<(Option<usize>, Option<usize>), ParseError> {
    let Some(clause) = limit_clause else {
        return Ok((None, None));
    };
    match clause {
        sql::LimitClause::LimitOffset {
            limit,
            offset,
            limit_by,
        } => {
            if !limit_by.is_empty() {
                return Err(unsupported("LIMIT BY"));
            }
            let limit = limit.as_ref().map(expr_to_usize).transpose()?;
            let offset = offset
                .as_ref()
                .map(|o| expr_to_usize(&o.value))
                .transpose()?;
            Ok((limit, offset))
        }
        sql::LimitClause::OffsetCommaLimit { offset, limit } => {
            Ok((Some(expr_to_usize(limit)?), Some(expr_to_usize(offset)?)))
        }
    }
}

/// Interprets an expression as a non-negative row count (for `LIMIT`/`OFFSET`).
fn expr_to_usize(expr: &sql::Expr) -> Result<usize, ParseError> {
    match lower_literal(expr)? {
        Literal::Int(i) if i >= 0 => Ok(i as usize),
        other => Err(unsupported(format!(
            "expected a non-negative integer, found {other:?}"
        ))),
    }
}

/// Joins an [`sql::ObjectName`]'s identifier parts with `.`.
fn object_name_to_string(name: &sql::ObjectName) -> String {
    name.0
        .iter()
        .filter_map(|part| part.as_ident())
        .map(|ident| ident.value.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

/// Shorthand for building a [`ParseError::Unsupported`].
fn unsupported(what: impl Into<String>) -> ParseError {
    ParseError::Unsupported(what.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::FileAttr;
    use crate::query::ast::*;

    #[test]
    fn select_fields_with_alias_no_from() {
        let q = parse("SELECT status, count(*) AS Count GROUP BY status").unwrap();
        assert_eq!(
            q.select[0],
            SelectItem {
                expr: SelectExpr::Col(ColRef::Field("status".into())),
                alias: None
            }
        );
        assert_eq!(
            q.select[1],
            SelectItem {
                expr: SelectExpr::Agg(Aggregate::CountStar),
                alias: Some("Count".into())
            }
        );
        assert_eq!(q.group_by, vec![ColRef::Field("status".into())]);
        assert_eq!(q.from_glob, None);
    }
    #[test]
    fn file_pseudo_columns() {
        let q = parse("SELECT file.name, file.folder WHERE file.ext = 'md'").unwrap();
        assert_eq!(
            q.select[0].expr,
            SelectExpr::Col(ColRef::File(FileAttr::Name))
        );
        assert_eq!(
            q.select[1].expr,
            SelectExpr::Col(ColRef::File(FileAttr::Folder))
        );
        match q.filter.unwrap() {
            Predicate::Compare(ColRef::File(FileAttr::Ext), CmpOp::Eq, Literal::Str(s)) => {
                assert_eq!(s, "md")
            }
            p => panic!("unexpected {p:?}"),
        }
    }
    #[test]
    fn where_ops_and_boolean() {
        let q = parse("SELECT jira WHERE prd = '010' AND (status = 'draft' OR status = 'synced')")
            .unwrap();
        assert!(matches!(q.filter, Some(Predicate::And(_, _))));
    }
    #[test]
    fn in_like_isnull() {
        assert!(parse("SELECT jira WHERE status IN ('a','b')").is_ok());
        assert!(parse("SELECT jira WHERE slice LIKE 'mobile%'").is_ok());
        assert!(parse("SELECT jira WHERE epic IS NOT NULL").is_ok());
    }
    #[test]
    fn order_and_limit() {
        let q =
            parse("SELECT status, count(*) AS n GROUP BY status ORDER BY n DESC LIMIT 5 OFFSET 2")
                .unwrap();
        assert_eq!(
            q.order_by,
            vec![OrderKey {
                target: OrderTarget::Alias("n".into()),
                desc: true
            }]
        );
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.offset, Some(2));
    }
    #[test]
    fn from_quoted_glob_is_stripped() {
        let q = parse("SELECT jira FROM 'plans/**' WHERE status = 'draft'").unwrap();
        assert_eq!(q.from_glob.as_deref(), Some("plans/**"));
        assert!(matches!(q.filter, Some(Predicate::Compare(..))));
    }
    #[test]
    fn from_bare_ident() {
        let q = parse("SELECT jira FROM plans").unwrap();
        assert_eq!(q.from_glob.as_deref(), Some("plans"));
    }
    #[test]
    fn star_select() {
        let q = parse("SELECT *").unwrap();
        assert_eq!(q.select[0].expr, SelectExpr::Star);
    }
    #[test]
    fn aggregates_all_kinds() {
        assert!(
            parse(
                "SELECT min(prd), max(prd), sum(prd), avg(prd), group_concat(jira), count(distinct status) GROUP BY epic"
            )
            .is_ok()
        );
    }
    #[test]
    fn unsupported_join_errors() {
        assert!(matches!(
            parse("SELECT a FROM x JOIN y ON x.i=y.i"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn garbage_errors() {
        assert!(parse("SELCT nonsense").is_err());
    }
}
