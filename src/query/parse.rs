//! Lowering from a SQL string to the querymatter [`Query`] AST.
//!
//! The SQL is handed to `sqlparser` (under the [`GenericDialect`]) and the
//! resulting parse tree is lowered node-by-node into the [`Query`] AST. `FROM`
//! is optional: sqlparser accepts both a bare identifier (`FROM plans`) and a
//! quoted glob (`FROM 'plans/**'`, parsed as a quoted-identifier table), and
//! either form is read straight back from the table name by [`lower_from`].
//!
//! The raw SQL is never pre-processed, so a string literal that merely
//! *contains* the word `from` followed by a quote (e.g.
//! `WHERE title = 'imported from "x"'`) is passed through the real parser
//! untouched and never corrupted.
//!
//! Anything outside the supported subset — joins, subqueries, `HAVING`,
//! whole-query `DISTINCT`, set operations, multiple statements, any clause that
//! sqlparser parses but querymatter does not translate, or any node kind the
//! lowering does not understand — is rejected with a [`ParseError`] rather than
//! silently ignored.

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

/// Parses `sql` into a [`Query`], rejecting anything outside the supported
/// subset with a descriptive [`ParseError`].
pub fn parse(sql: &str) -> Result<Query, ParseError> {
    let statements =
        Parser::parse_sql(&GenericDialect, sql).map_err(|err| ParseError::Sql(err.to_string()))?;
    lower_parsed(&statements)
}

/// Validates that `statements` is exactly one `SELECT` query and lowers it.
fn lower_parsed(statements: &[sql::Statement]) -> Result<Query, ParseError> {
    let [statement] = statements else {
        return Err(unsupported("expected exactly one SQL statement"));
    };
    let sql::Statement::Query(query) = statement else {
        return Err(unsupported("only SELECT queries are supported"));
    };
    lower_query(query)
}

/// Lowers a `sqlparser` query into the querymatter [`Query`] AST.
fn lower_query(query: &sql::Query) -> Result<Query, ParseError> {
    reject_unsupported_query_clauses(query)?;

    let select = match query.body.as_ref() {
        sql::SetExpr::Select(select) => select.as_ref(),
        sql::SetExpr::SetOperation { .. } => {
            return Err(unsupported("set operations (UNION / INTERSECT / EXCEPT)"));
        }
        _ => return Err(unsupported("this query form")),
    };

    reject_unsupported_select_clauses(select)?;

    let from_glob = lower_from(&select.from)?;

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

/// Rejects `Query`-level clauses that sqlparser parses but querymatter does not
/// translate, so none can be silently discarded.
fn reject_unsupported_query_clauses(query: &sql::Query) -> Result<(), ParseError> {
    let clauses = [
        ("WITH / common table expressions", query.with.is_some()),
        ("FETCH clause", query.fetch.is_some()),
        (
            "FOR UPDATE / FOR SHARE locking clause",
            !query.locks.is_empty(),
        ),
        ("FOR XML / FOR JSON clause", query.for_clause.is_some()),
        ("SETTINGS clause", query.settings.is_some()),
        ("FORMAT clause", query.format_clause.is_some()),
        ("pipe operators (|>)", !query.pipe_operators.is_empty()),
    ];
    reject_first(&clauses)
}

/// Rejects `Select`-level clauses that sqlparser parses but querymatter does
/// not translate. sqlparser 0.62 populates these fields unconditionally, so
/// each must be inspected explicitly or it would be parsed and dropped.
fn reject_unsupported_select_clauses(select: &sql::Select) -> Result<(), ParseError> {
    let clauses = [
        ("DISTINCT on the whole SELECT", select.distinct.is_some()),
        ("HAVING clause", select.having.is_some()),
        ("SELECT INTO", select.into.is_some()),
        ("PREWHERE clause", select.prewhere.is_some()),
        ("QUALIFY clause", select.qualify.is_some()),
        ("TOP clause", select.top.is_some()),
        (
            "named window (WINDOW) clause",
            !select.named_window.is_empty(),
        ),
        ("CLUSTER BY clause", !select.cluster_by.is_empty()),
        ("DISTRIBUTE BY clause", !select.distribute_by.is_empty()),
        ("SORT BY clause", !select.sort_by.is_empty()),
        (
            "CONNECT BY / START WITH clause",
            !select.connect_by.is_empty(),
        ),
        ("LATERAL VIEW clause", !select.lateral_views.is_empty()),
    ];
    reject_first(&clauses)
}

/// Returns [`ParseError::Unsupported`] for the first `(name, present)` pair whose
/// `present` flag is set.
fn reject_first(clauses: &[(&str, bool)]) -> Result<(), ParseError> {
    match clauses.iter().find(|&&(_, present)| present) {
        Some(&(name, _)) => Err(unsupported(name)),
        None => Ok(()),
    }
}

/// Resolves a parsed `FROM` clause (bare identifier or quoted-identifier glob)
/// to a glob string.
fn lower_from(from: &[sql::TableWithJoins]) -> Result<Option<String>, ParseError> {
    match from {
        [] => Ok(None),
        [single] => {
            if !single.joins.is_empty() {
                return Err(unsupported("JOIN clauses"));
            }
            match &single.relation {
                sql::TableFactor::Table { name, .. } => Ok(Some(object_name_to_string(name))),
                _ => Err(unsupported("FROM must be a bare identifier or quoted glob")),
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
        _ => Err(unsupported("this count(...) argument")),
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
        _ => Err(unsupported(format!("this {name}(...) argument"))),
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
            "expected a column reference, found unsupported expression `{other}`"
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
        _ => Err(unsupported("this WHERE expression")),
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
        _ => Err(unsupported("this literal value")),
    }
}

/// Negates a numeric literal (for `-<number>`).
fn negate_literal(expr: &sql::Expr) -> Result<Literal, ParseError> {
    match lower_literal(expr)? {
        Literal::Int(i) => Ok(Literal::Int(-i)),
        Literal::Float(f) => Ok(Literal::Float(-f)),
        _ => Err(unsupported("negating a non-numeric literal")),
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
        _ => Err(unsupported("this literal value")),
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
        _ => Err(unsupported("LIKE pattern must be a string")),
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
        _ => Err(unsupported("expected a non-negative integer")),
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
        // IN and NOT IN carry the column, the literal list, and the negation flag.
        assert_eq!(
            parse("SELECT jira WHERE status IN ('a','b')")
                .unwrap()
                .filter,
            Some(Predicate::In(
                ColRef::Field("status".into()),
                vec![Literal::Str("a".into()), Literal::Str("b".into())],
                false
            ))
        );
        assert_eq!(
            parse("SELECT jira WHERE status NOT IN ('a','b')")
                .unwrap()
                .filter,
            Some(Predicate::In(
                ColRef::Field("status".into()),
                vec![Literal::Str("a".into()), Literal::Str("b".into())],
                true
            ))
        );

        // LIKE and NOT LIKE carry the pattern and the negation flag.
        assert_eq!(
            parse("SELECT jira WHERE slice LIKE 'mobile%'")
                .unwrap()
                .filter,
            Some(Predicate::Like(
                ColRef::Field("slice".into()),
                "mobile%".into(),
                false
            ))
        );
        assert_eq!(
            parse("SELECT jira WHERE slice NOT LIKE 'mobile%'")
                .unwrap()
                .filter,
            Some(Predicate::Like(
                ColRef::Field("slice".into()),
                "mobile%".into(),
                true
            ))
        );

        // IS NULL / IS NOT NULL carry the negation flag.
        assert_eq!(
            parse("SELECT jira WHERE epic IS NULL").unwrap().filter,
            Some(Predicate::IsNull(ColRef::Field("epic".into()), false))
        );
        assert_eq!(
            parse("SELECT jira WHERE epic IS NOT NULL").unwrap().filter,
            Some(Predicate::IsNull(ColRef::Field("epic".into()), true))
        );
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
        let q = parse(
            "SELECT min(prd), max(prd), sum(prd), avg(prd), group_concat(jira), count(distinct status) GROUP BY epic",
        )
        .unwrap();
        let prd = ColRef::Field("prd".into());
        assert_eq!(
            q.select[0].expr,
            SelectExpr::Agg(Aggregate::Min(prd.clone()))
        );
        assert_eq!(
            q.select[1].expr,
            SelectExpr::Agg(Aggregate::Max(prd.clone()))
        );
        assert_eq!(
            q.select[2].expr,
            SelectExpr::Agg(Aggregate::Sum(prd.clone()))
        );
        assert_eq!(q.select[3].expr, SelectExpr::Agg(Aggregate::Avg(prd)));
        assert_eq!(
            q.select[4].expr,
            SelectExpr::Agg(Aggregate::GroupConcat(ColRef::Field("jira".into())))
        );
        assert_eq!(
            q.select[5].expr,
            SelectExpr::Agg(Aggregate::Count(ColRef::Field("status".into()), true))
        );
        assert_eq!(q.group_by, vec![ColRef::Field("epic".into())]);
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

    #[test]
    fn header_derivation() {
        let header = |sql: &str| parse(sql).unwrap().select[0].header();
        assert_eq!(header("SELECT status AS S"), "S"); // alias wins
        assert_eq!(header("SELECT status"), "status"); // field name
        assert_eq!(header("SELECT file.folder"), "file.folder"); // file pseudo-column
        assert_eq!(header("SELECT *"), "*"); // star
        assert_eq!(header("SELECT count(*)"), "count(*)"); // aggregate
        assert_eq!(
            header("SELECT count(distinct status)"),
            "count(distinct status)"
        );
    }

    #[test]
    fn where_literal_with_from_lookalike_is_not_corrupted() {
        // The real parser must not corrupt a string literal that merely
        // contains `from "..."`; there is no raw-SQL pre-strip to fool.
        let q = parse("SELECT jira WHERE title = 'imported from \"archive\"'").unwrap();
        assert_eq!(q.from_glob, None);
        assert_eq!(
            q.filter,
            Some(Predicate::Compare(
                ColRef::Field("title".into()),
                CmpOp::Eq,
                Literal::Str("imported from \"archive\"".into())
            ))
        );
    }

    #[test]
    fn rejects_having() {
        assert!(matches!(
            parse("SELECT status GROUP BY status HAVING count(*) > 1"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn rejects_whole_query_distinct() {
        assert!(matches!(
            parse("SELECT DISTINCT jira"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn rejects_set_operation() {
        assert!(matches!(
            parse("SELECT a UNION SELECT b"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn rejects_derived_table_from() {
        assert!(matches!(
            parse("SELECT x FROM (SELECT 1)"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn rejects_multiple_statements() {
        assert!(matches!(
            parse("SELECT a; SELECT b"),
            Err(ParseError::Unsupported(_))
        ));
    }

    // Clauses that GenericDialect *parses* but querymatter does not translate
    // must surface as Unsupported rather than being silently dropped.
    #[test]
    fn rejects_sort_by() {
        assert!(matches!(
            parse("SELECT jira SORT BY prd"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn rejects_cluster_by() {
        assert!(matches!(
            parse("SELECT jira CLUSTER BY prd"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn rejects_distribute_by() {
        assert!(matches!(
            parse("SELECT jira DISTRIBUTE BY prd"),
            Err(ParseError::Unsupported(_))
        ));
    }

    // PREWHERE and QUALIFY are not accepted by GenericDialect at all, so they
    // are rejected earlier as a syntax error; the defensive guards for those
    // fields remain for a future dialect that would populate them.
    #[test]
    fn prewhere_and_qualify_are_rejected() {
        assert!(parse("SELECT jira PREWHERE status = 'x'").is_err());
        assert!(parse("SELECT jira QUALIFY prd = 1").is_err());
    }

    /// Unsupported constructs must produce a human phrase, never a raw
    /// sqlparser AST Debug dump (which contains struct-literal braces).
    #[test]
    fn unsupported_messages_have_no_ast_debug_dump() {
        // A CAST in WHERE, a subquery, and a non-literal IN value all route
        // through catch-all arms that used to `{:?}` the node.
        for sql in [
            "SELECT status WHERE CAST(prd AS INT) = 1",
            "SELECT status WHERE status IN (SELECT status)",
            "SELECT status WHERE status = status + 1",
        ] {
            let err = crate::query::parse(sql).unwrap_err();
            let msg = err.to_string();
            assert!(
                !msg.contains('{') && !msg.contains('}'),
                "message leaked an AST dump for {sql:?}: {msg}"
            );
            assert!(
                msg.to_lowercase().contains("support"),
                "message should say what isn't supported for {sql:?}: {msg}"
            );
        }
    }
}
