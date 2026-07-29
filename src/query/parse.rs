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
//! Anything outside the supported subset — joins, subqueries, `DISTINCT ON`,
//! set operations, multiple statements, any clause that sqlparser parses but
//! querymatter does not translate, or any node kind the lowering does not
//! understand — is rejected with a [`ParseError`] rather than silently
//! ignored.

use sqlparser::ast as sql;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::model::FileAttr;
use crate::query::ast::{
    Aggregate, BinOp, CmpOp, ColRef, Expr, GroupedOrderKey, GroupedOrderTarget, Grouping, Having,
    HavingLeaf, LikePattern, Literal, OrderKey, OrderTarget, Predicate, Query, RegexPattern,
    RelDate, ScalarFn, SelectExpr, SelectItem,
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
    let keys = lower_group_by(&select.group_by, &select_items)?;
    let distinct = lower_distinct(select.distinct.as_ref())?;
    let grouping = lower_grouping(
        query.order_by.as_ref(),
        select.having.as_ref(),
        distinct,
        keys,
        &select_items,
        &aliases,
    )?;
    let (limit, offset) = lower_limit(query.limit_clause.as_ref())?;

    Ok(Query {
        select: select_items,
        from_glob,
        filter,
        grouping,
        limit,
        offset,
    })
}

/// Builds the [`Grouping`] variant this query belongs to, lowering `HAVING`
/// and `ORDER BY` into whichever side accepts them.
///
/// A query is grouped when it has explicit `GROUP BY` keys *or* any aggregate
/// `SELECT` item (the implicit single group) — the same test the executor
/// used to make for itself when it dispatched between its two pipelines, made
/// once here so the two can never disagree.
///
/// `DISTINCT` belongs to the ungrouped side only: a grouped query already
/// yields one row per distinct group key, so combining the two is
/// redundant/confusing rather than meaningful. Both rejections say so, in the
/// terms the query itself used — an explicit `GROUP BY`, or the aggregate
/// that grouped it implicitly.
fn lower_grouping(
    order_by: Option<&sql::OrderBy>,
    having: Option<&sql::Expr>,
    distinct: bool,
    keys: Vec<ColRef>,
    select_items: &[SelectItem],
    aliases: &[&str],
) -> Result<Grouping, ParseError> {
    let has_aggregate = select_items
        .iter()
        .any(|item| matches!(item.expr, SelectExpr::Agg(_)));
    if !keys.is_empty() || has_aggregate {
        if distinct {
            return Err(unsupported(if keys.is_empty() {
                "DISTINCT combined with aggregate functions"
            } else {
                "DISTINCT combined with GROUP BY"
            }));
        }
        return Ok(Grouping::Grouped {
            having: lower_having(having, &keys, select_items)?,
            order_by: lower_grouped_order_by(order_by, aliases, &keys)?,
            keys,
        });
    }
    if having.is_some() {
        return Err(unsupported("HAVING requires GROUP BY"));
    }
    Ok(Grouping::Ungrouped {
        distinct,
        order_by: lower_order_by(order_by, aliases)?,
    })
}

/// Lowers the `SELECT [DISTINCT | ALL]` marker to the flag
/// [`lower_grouping`] either stores on [`Grouping::Ungrouped`] or rejects.
///
/// Only the plain `DISTINCT` form sets the flag; a bare `SELECT` and the
/// explicit `ALL` form (which just spells out the default of keeping
/// duplicates) both lower to `false`. `DISTINCT ON (...)` is a Postgres
/// extension outside this subset, so it's rejected by name here rather than
/// falling through to a `{:?}`-dumped generic error.
fn lower_distinct(distinct: Option<&sql::Distinct>) -> Result<bool, ParseError> {
    match distinct {
        None | Some(sql::Distinct::All) => Ok(false),
        Some(sql::Distinct::Distinct) => Ok(true),
        Some(sql::Distinct::On(_)) => Err(unsupported("DISTINCT ON")),
    }
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

/// Lowers a projection expression to a [`SelectExpr`]: a bare aggregate call
/// stays [`SelectExpr::Agg`]; everything else — a column, literal, scalar
/// function, or arithmetic/concat expression — lowers via [`lower_expr`].
/// The `*` wildcard is handled one level up in [`lower_select_item`].
fn lower_select_expr(expr: &sql::Expr) -> Result<SelectExpr, ParseError> {
    if let sql::Expr::Function(func) = expr {
        let name = object_name_to_string(&func.name).to_ascii_lowercase();
        if is_aggregate_name(&name) {
            return Ok(SelectExpr::Agg(lower_aggregate(func)?));
        }
    }
    Ok(SelectExpr::Expr(lower_expr(expr)?))
}

/// True for the six aggregate function names, shared between the top-level
/// `SELECT` dispatch ([`lower_select_expr`]) and the "aggregate nested
/// inside an expression" rejection in [`lower_scalar_call`].
fn is_aggregate_name(name: &str) -> bool {
    matches!(
        name,
        "count" | "min" | "max" | "sum" | "avg" | "group_concat"
    )
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

/// Lowers a scalar expression: a column, literal, scalar-function call, or
/// arithmetic/concat operation. Used for `SELECT` items that aren't a bare
/// aggregate ([`lower_select_expr`]) and for their nested sub-expressions
/// (scalar-function arguments, binary operands).
fn lower_expr(expr: &sql::Expr) -> Result<Expr, ParseError> {
    match expr {
        sql::Expr::Identifier(_) | sql::Expr::CompoundIdentifier(_) => {
            Ok(Expr::Col(lower_col_ref(expr)?))
        }
        sql::Expr::Value(_)
        | sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Minus | sql::UnaryOperator::Plus,
            ..
        } => Ok(Expr::Lit(lower_literal(expr)?)),
        sql::Expr::Function(func) => lower_scalar_call(func),
        sql::Expr::Trim {
            trim_where,
            trim_what,
            trim_characters,
            expr,
        } => lower_trim(trim_where, trim_what, trim_characters, expr),
        sql::Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => lower_substr(expr, substring_from, substring_for),
        sql::Expr::BinaryOp { left, op, right } => lower_expr_binary(left, op, right),
        sql::Expr::Nested(inner) => lower_expr(inner),
        sql::Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => lower_case(operand, conditions, else_result),
        _ => Err(unsupported("this SELECT expression")),
    }
}

/// Lowers a `CASE [operand] WHEN cond THEN val ... [ELSE val] END`
/// expression.
///
/// In the searched form (`operand: None`), each `WHEN` condition is a full
/// `WHERE`-style predicate — lowered via [`lower_predicate`] and wrapped in
/// [`Expr::Predicate`] — so a searched `CASE` gets the same
/// comparison/`LIKE`/`IN`/`MEMBER OF`/`AND`/`OR`/`NOT` grammar `WHERE`
/// already supports, rather than a second, narrower boolean grammar. In the
/// simple form (`operand: Some`), each `WHEN` is a plain value the operand
/// is compared against for equality (`exec::eval_expr`), so it lowers via
/// [`lower_expr`] like any other value position. Every sub-expression lowers
/// through `lower_expr`/`lower_predicate`, so a relative-date string literal
/// inside a `CASE` arm is resolved to `RelDate` the same as anywhere else.
fn lower_case(
    operand: &Option<Box<sql::Expr>>,
    conditions: &[sql::CaseWhen],
    else_result: &Option<Box<sql::Expr>>,
) -> Result<Expr, ParseError> {
    let operand = operand
        .as_deref()
        .map(lower_expr)
        .transpose()?
        .map(Box::new);
    let whens = conditions
        .iter()
        .map(|when| {
            let cond = if operand.is_some() {
                lower_expr(&when.condition)?
            } else {
                Expr::Predicate(Box::new(lower_predicate(&when.condition)?))
            };
            Ok((cond, lower_expr(&when.result)?))
        })
        .collect::<Result<Vec<_>, ParseError>>()?;
    let else_expr = else_result
        .as_deref()
        .map(lower_expr)
        .transpose()?
        .map(Box::new);
    Ok(Expr::Case {
        operand,
        whens,
        else_expr,
    })
}

/// Lowers a scalar-function call (`lower(...)`, `ltrim(...)`, `replace(...)`,
/// …), or `coalesce(...)`. An aggregate name here means an aggregate nested
/// inside a larger expression (e.g. `count(*) + 1`), which this batch does
/// not support — mixing agg and scalar in one tree needs group-context
/// threading beyond this scope.
///
/// `trim`/`substr` never reach this function: `sqlparser` intercepts those
/// keywords as the dedicated [`sql::Expr::Trim`]/[`sql::Expr::Substring`]
/// nodes (see [`lower_trim`]/[`lower_substr`]) rather than a plain function
/// call, regardless of dialect.
fn lower_scalar_call(func: &sql::Function) -> Result<Expr, ParseError> {
    if func.over.is_some() {
        return Err(unsupported("window functions (OVER clause)"));
    }
    if func.filter.is_some() {
        return Err(unsupported("aggregate FILTER clause"));
    }

    let name = object_name_to_string(&func.name).to_ascii_lowercase();
    if name == "coalesce" {
        return lower_coalesce(func, &name);
    }

    let Some(scalar) = scalar_fn_from_name(&name) else {
        if is_aggregate_name(&name) {
            return Err(unsupported("an aggregate inside an expression"));
        }
        return Err(unsupported(format!("function `{name}`")));
    };

    let list = arg_list(func, &name)?;
    if list.duplicate_treatment.is_some() {
        return Err(unsupported(format!("DISTINCT inside {name}(...)")));
    }
    let args = list
        .args
        .iter()
        .map(scalar_arg)
        .collect::<Result<Vec<_>, _>>()?;
    check_scalar_arity(&scalar, &name, args.len())?;
    Ok(Expr::Scalar(scalar, args))
}

/// Lowers a `COALESCE(...)` call: every argument lowers via [`lower_expr`]
/// (through [`scalar_arg`]), so a nested aggregate (`COALESCE(count(*), 0)`)
/// is rejected by the same "aggregate inside an expression" path as any
/// other scalar-call argument (see [`lower_scalar_call`]). At least one
/// argument is required — `COALESCE()` is a parse-time arity error.
fn lower_coalesce(func: &sql::Function, name: &str) -> Result<Expr, ParseError> {
    let list = arg_list(func, name)?;
    if list.duplicate_treatment.is_some() {
        return Err(unsupported(format!("DISTINCT inside {name}(...)")));
    }
    let args = list
        .args
        .iter()
        .map(scalar_arg)
        .collect::<Result<Vec<_>, _>>()?;
    if args.is_empty() {
        return Err(unsupported("coalesce() requires at least one argument"));
    }
    Ok(Expr::Coalesce(args))
}

/// Maps a lowercased function name to the [`ScalarFn`] it calls, or `None`
/// for anything else (an aggregate, or an unknown function).
fn scalar_fn_from_name(name: &str) -> Option<ScalarFn> {
    match name {
        "lower" => Some(ScalarFn::Lower),
        "upper" => Some(ScalarFn::Upper),
        "length" => Some(ScalarFn::Length),
        "ltrim" => Some(ScalarFn::Ltrim),
        "rtrim" => Some(ScalarFn::Rtrim),
        "replace" => Some(ScalarFn::Replace),
        "date" => Some(ScalarFn::Date),
        _ => None,
    }
}

/// Validates a scalar function call's argument count, producing a
/// parse-time error that names both the function and its expected arity on
/// mismatch (e.g. `lower() takes 1 argument, got 2`).
fn check_scalar_arity(f: &ScalarFn, name: &str, argc: usize) -> Result<(), ParseError> {
    let (arity_ok, expected) = match f {
        ScalarFn::Lower
        | ScalarFn::Upper
        | ScalarFn::Length
        | ScalarFn::Trim
        | ScalarFn::Ltrim
        | ScalarFn::Rtrim => (argc == 1, "1 argument"),
        ScalarFn::Substr => ((2..=3).contains(&argc), "2 or 3 arguments"),
        ScalarFn::Replace => (argc == 3, "3 arguments"),
        ScalarFn::Date => ((1..=2).contains(&argc), "1 or 2 arguments"),
    };
    if arity_ok {
        Ok(())
    } else {
        Err(unsupported(format!(
            "{name}() takes {expected}, got {argc}"
        )))
    }
}

/// Extracts a scalar-function argument's expression, rejecting the wildcard
/// and named-argument forms (neither applies to these functions).
fn scalar_arg(arg: &sql::FunctionArg) -> Result<Expr, ParseError> {
    match arg {
        sql::FunctionArg::Unnamed(sql::FunctionArgExpr::Expr(expr)) => lower_expr(expr),
        _ => Err(unsupported("this function argument")),
    }
}

/// Lowers a `TRIM(...)` expression to `trim`/`ltrim`/`rtrim` based on
/// `trim_where` (no qualifier, or `BOTH`, maps to `trim`). Custom trim
/// characters (`TRIM('x' FROM s)`, the PostgreSQL comma form) are outside
/// this batch's whitespace-only scalar functions.
fn lower_trim(
    trim_where: &Option<sql::TrimWhereField>,
    trim_what: &Option<Box<sql::Expr>>,
    trim_characters: &Option<Vec<sql::Expr>>,
    expr: &sql::Expr,
) -> Result<Expr, ParseError> {
    if trim_what.is_some() || trim_characters.is_some() {
        return Err(unsupported("trimming custom characters"));
    }
    let scalar = match trim_where {
        None | Some(sql::TrimWhereField::Both) => ScalarFn::Trim,
        Some(sql::TrimWhereField::Leading) => ScalarFn::Ltrim,
        Some(sql::TrimWhereField::Trailing) => ScalarFn::Rtrim,
    };
    Ok(Expr::Scalar(scalar, vec![lower_expr(expr)?]))
}

/// Lowers a `SUBSTR`/`SUBSTRING` expression — either the `substr(s, start[,
/// len])` shorthand or the ANSI `SUBSTRING(s FROM start [FOR len])` form
/// (both parse to the same `sqlparser` node) — to `Expr::Scalar(Substr, ...)`.
fn lower_substr(
    expr: &sql::Expr,
    from: &Option<Box<sql::Expr>>,
    for_len: &Option<Box<sql::Expr>>,
) -> Result<Expr, ParseError> {
    let Some(from) = from else {
        return Err(unsupported("substr() without a start position"));
    };
    let mut args = vec![lower_expr(expr)?, lower_expr(from)?];
    if let Some(len) = for_len {
        args.push(lower_expr(len)?);
    }
    Ok(Expr::Scalar(ScalarFn::Substr, args))
}

/// Lowers a `sqlparser` binary operator node to `Expr::Binary`, for the
/// subset a `SELECT` expression supports: arithmetic and `||` concat.
fn lower_expr_binary(
    left: &sql::Expr,
    op: &sql::BinaryOperator,
    right: &sql::Expr,
) -> Result<Expr, ParseError> {
    use sql::BinaryOperator as B;
    let op = match op {
        B::Plus => BinOp::Add,
        B::Minus => BinOp::Sub,
        B::Multiply => BinOp::Mul,
        B::Divide => BinOp::Div,
        B::Modulo => BinOp::Mod,
        B::StringConcat => BinOp::Concat,
        other => return Err(unsupported(format!("operator `{other}`"))),
    };
    Ok(Expr::Binary(
        op,
        Box::new(lower_expr(left)?),
        Box::new(lower_expr(right)?),
    ))
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

/// Lowers a column reference: a bare identifier (a single-segment
/// frontmatter field) or a compound identifier — either a `file.<attr>`
/// pseudo-column or a dotted frontmatter field path (a multi-segment
/// [`ColRef::Field`] indexing into a nested `Value::Map`).
fn lower_col_ref(expr: &sql::Expr) -> Result<ColRef, ParseError> {
    match expr {
        sql::Expr::Identifier(ident) => Ok(ColRef::Field(vec![ident.value.clone()])),
        sql::Expr::CompoundIdentifier(parts) => lower_compound(parts),
        other => Err(ParseError::BadColumn(format!(
            "expected a column reference, found unsupported expression `{other}`"
        ))),
    }
}

/// Lowers a compound identifier. A `file`-prefixed one must be exactly
/// `file.<attr>` — `file.*` has no nesting, so any other length under that
/// prefix is rejected. Everything else lowers to a dotted
/// `ColRef::Field` path, letting later segments index into a `Value::Map`.
fn lower_compound(parts: &[sql::Ident]) -> Result<ColRef, ParseError> {
    if let [first, ..] = parts
        && first.value.eq_ignore_ascii_case("file")
    {
        let [_, attr] = parts else {
            let joined = parts
                .iter()
                .map(|p| p.value.as_str())
                .collect::<Vec<_>>()
                .join(".");
            return Err(ParseError::BadColumn(format!(
                "`file.*` has no nesting: `{joined}`"
            )));
        };
        let lowered = attr.value.to_ascii_lowercase();
        return FileAttr::from_attr_name(&lowered)
            .map(ColRef::File)
            .ok_or_else(|| {
                ParseError::BadColumn(format!("unknown file attribute `file.{lowered}`"))
            });
    }
    Ok(ColRef::Field(
        parts.iter().map(|p| p.value.clone()).collect(),
    ))
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
            lower_expr(expr)?,
            LikePattern::new(&string_literal(pattern, "LIKE")?),
            *negated,
        )),
        sql::Expr::RLike {
            negated,
            expr,
            pattern,
            ..
        } => lower_regexp(expr, pattern, *negated),
        sql::Expr::InList {
            expr,
            list,
            negated,
        } => {
            let literals = list.iter().map(lower_literal).collect::<Result<_, _>>()?;
            Ok(Predicate::In(lower_expr(expr)?, literals, *negated))
        }
        sql::Expr::IsNull(inner) => Ok(Predicate::IsNull(lower_expr(inner)?, false)),
        sql::Expr::IsNotNull(inner) => Ok(Predicate::IsNull(lower_expr(inner)?, true)),
        sql::Expr::MemberOf(member_of) => lower_member_of(member_of, false),
        sql::Expr::Nested(inner) => lower_predicate(inner),
        sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Not,
            expr,
        } => lower_not(expr),
        _ => Err(unsupported("this WHERE expression")),
    }
}

/// Lowers `<expr> [NOT] REGEXP '<pattern>'` (sqlparser's `RLike` node also
/// covers the `RLIKE` keyword alias — identical semantics, see
/// [`Predicate::Regexp`]). The pattern is compiled here, once: that both
/// rejects an invalid regex at parse time rather than on every row at
/// evaluation time, and hands the compiled [`RegexPattern`] to the AST, so
/// `exec::eval_predicate` only ever matches against it.
fn lower_regexp(
    expr: &sql::Expr,
    pattern: &sql::Expr,
    negated: bool,
) -> Result<Predicate, ParseError> {
    let operand = lower_expr(expr)?;
    let pat = string_literal(pattern, "REGEXP")?;
    let compiled =
        RegexPattern::new(&pat).map_err(|e| unsupported(format!("invalid regex `{pat}`: {e}")))?;
    Ok(Predicate::Regexp(operand, compiled, negated))
}

/// Lowers `NOT <expr>`. sqlparser 0.62 rejects the postfix `<value> NOT
/// MEMBER OF(...)` form outright (a syntax error, before lowering ever sees
/// it), but accepts the prefix `NOT <value> MEMBER OF(...)`, parsing it as a
/// plain `UnaryOp` wrapping `MemberOf`. That shape is special-cased here —
/// rather than falling through to a generic `Predicate::Not` wrap — so the
/// negation lands as a flat flag on `Predicate::MemberOf`, matching how
/// `In`/`Like` carry theirs.
fn lower_not(expr: &sql::Expr) -> Result<Predicate, ParseError> {
    if let sql::Expr::MemberOf(member_of) = expr {
        return lower_member_of(member_of, true);
    }
    Ok(Predicate::Not(Box::new(lower_predicate(expr)?)))
}

/// Lowers a `<value> MEMBER OF(<array>)` node: `value` lowers through the
/// full [`Expr`] grammar (a literal, a column, or a scalar call — matching
/// `Compare`/`Regexp`) and `array` must be a single column reference.
/// sqlparser's `MemberOf` node carries no `negated` field of its own, so
/// `negated` is threaded in by the caller (see [`lower_predicate`]).
fn lower_member_of(member_of: &sql::MemberOf, negated: bool) -> Result<Predicate, ParseError> {
    let value = lower_expr(&member_of.value)?;
    let col = lower_col_ref(&member_of.array)?;
    Ok(Predicate::MemberOf(value, col, negated))
}

/// Lowers a binary operation: a boolean connective (`AND`/`OR`) or a
/// comparison of two expressions (columns, literals, scalar calls, or
/// arithmetic), lowered via [`lower_expr`] on both sides.
fn lower_binary(
    left: &sql::Expr,
    op: &sql::BinaryOperator,
    right: &sql::Expr,
) -> Result<Predicate, ParseError> {
    use sql::BinaryOperator as B;
    match op {
        B::And => Ok(Predicate::And(
            Box::new(lower_predicate(left)?),
            Box::new(lower_predicate(right)?),
        )),
        B::Or => Ok(Predicate::Or(
            Box::new(lower_predicate(left)?),
            Box::new(lower_predicate(right)?),
        )),
        _ => {
            let cmp =
                cmp_op_from_binary(op).ok_or_else(|| unsupported(format!("operator `{op}`")))?;
            Ok(Predicate::Compare(
                lower_expr(left)?,
                cmp,
                lower_expr(right)?,
            ))
        }
    }
}

/// Maps a `sqlparser` comparison operator to a [`CmpOp`], or `None` for
/// anything else (a boolean connective, or an operator this subset doesn't
/// support). Shared between [`lower_binary`] (`WHERE`) and
/// [`lower_having_binary`] (`HAVING`).
fn cmp_op_from_binary(op: &sql::BinaryOperator) -> Option<CmpOp> {
    use sql::BinaryOperator as B;
    match op {
        B::Eq => Some(CmpOp::Eq),
        B::NotEq => Some(CmpOp::Ne),
        B::Lt => Some(CmpOp::Lt),
        B::LtEq => Some(CmpOp::Le),
        B::Gt => Some(CmpOp::Gt),
        B::GtEq => Some(CmpOp::Ge),
        _ => None,
    }
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
    if let Some(s) = raw_string(value) {
        return Ok(lower_string_literal(s));
    }
    match value {
        sql::Value::Number(n, _) => parse_number(n),
        sql::Value::Boolean(b) => Ok(Literal::Bool(*b)),
        sql::Value::Null => Ok(Literal::Null),
        _ => Err(unsupported("this literal value")),
    }
}

/// Lowers a quoted string literal's text, resolving it to
/// [`Literal::RelativeDate`] when it matches the strict relative-date
/// grammar (`today`/`now`/`[+-]<int>(d|w|mo|y)`, see [`RelDate::parse`]); any
/// other string — the overwhelming majority — stays a plain `Literal::Str`,
/// e.g. `'draft'` (spec §9: no pre-existing string literal is
/// reinterpreted).
fn lower_string_literal(s: &str) -> Literal {
    match RelDate::parse(s) {
        Some(rd) => Literal::RelativeDate(rd),
        None => Literal::Str(s.to_string()),
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

/// Extracts a string literal's raw text, rejecting non-string values (used
/// for `LIKE`/`REGEXP` patterns — `what` names the caller's construct for
/// the error message, e.g. `"LIKE"`). This bypasses [`lower_string_literal`]'s
/// relative-date check deliberately: a `LIKE`/`REGEXP` pattern is a plain
/// `String`, never a [`Literal`], so a pattern that happens to match the
/// relative-date grammar (e.g. `LIKE '-7d'`) must still match literally,
/// not be rejected as "not a string".
fn string_literal(expr: &sql::Expr, what: &str) -> Result<String, ParseError> {
    let sql::Expr::Value(value) = expr else {
        return Err(unsupported(format!("{what} pattern must be a string")));
    };
    raw_string(&value.value)
        .map(str::to_string)
        .ok_or_else(|| unsupported(format!("{what} pattern must be a string")))
}

/// Extracts a quoted string literal's raw text from a `sqlparser` value
/// node, or `None` for anything else (a number, boolean, `NULL`, …). Shared
/// between [`lower_string_literal`] (general literal lowering, which
/// additionally checks the relative-date grammar) and [`string_literal`]
/// (`LIKE` patterns, which must not).
fn raw_string(value: &sql::Value) -> Option<&str> {
    match value {
        sql::Value::SingleQuotedString(s)
        | sql::Value::DoubleQuotedString(s)
        | sql::Value::TripleSingleQuotedString(s)
        | sql::Value::TripleDoubleQuotedString(s)
        | sql::Value::EscapedStringLiteral(s)
        | sql::Value::UnicodeStringLiteral(s)
        | sql::Value::NationalStringLiteral(s) => Some(s),
        _ => None,
    }
}

/// Lowers a `GROUP BY` clause into an ordered list of column references,
/// resolving identifiers that match a projection alias via
/// [`lower_group_by_expr`].
fn lower_group_by(
    group_by: &sql::GroupByExpr,
    select_items: &[SelectItem],
) -> Result<Vec<ColRef>, ParseError> {
    match group_by {
        sql::GroupByExpr::Expressions(exprs, modifiers) => {
            if !modifiers.is_empty() {
                return Err(unsupported("GROUP BY modifiers (ROLLUP / CUBE / …)"));
            }
            exprs
                .iter()
                .map(|expr| lower_group_by_expr(expr, select_items))
                .collect()
        }
        sql::GroupByExpr::All(_) => Err(unsupported("GROUP BY ALL")),
    }
}

/// Lowers a single `GROUP BY` expression. A bare identifier that matches a
/// `SELECT` alias resolves to that item's underlying column — mirroring
/// [`lower_order_target`], the alias wins even when a real field shares the
/// same name. An alias on an aggregate or a computed expression is not a
/// valid grouping key, since there is no single column to group rows by.
/// Anything else (including a `file.*` compound identifier) falls back to
/// [`lower_col_ref`], so a real frontmatter field is still grouped on
/// directly.
fn lower_group_by_expr(
    expr: &sql::Expr,
    select_items: &[SelectItem],
) -> Result<ColRef, ParseError> {
    if let sql::Expr::Identifier(ident) = expr
        && let Some(item) = select_items
            .iter()
            .find(|item| item.alias.as_deref() == Some(ident.value.as_str()))
    {
        return match &item.expr {
            SelectExpr::Expr(Expr::Col(col)) => Ok(col.clone()),
            _ => Err(ParseError::BadColumn(format!(
                "GROUP BY alias `{}` refers to an aggregate or computed \
                 expression, not a plain column",
                ident.value
            ))),
        };
    }
    lower_col_ref(expr)
}

/// Lowers a `HAVING` clause into a [`Having`] tree, or `None` when the query
/// has no `HAVING`. Called only for a grouped query ([`lower_grouping`]
/// rejects `HAVING` outright on an ungrouped one), and rejects it again when
/// `group_by` is empty — the implicit single group of an aggregate-only
/// `SELECT` has no grouping keys to filter on, so `HAVING` there is as
/// meaningless as it is with no grouping at all.
fn lower_having(
    having: Option<&sql::Expr>,
    group_by: &[ColRef],
    select_items: &[SelectItem],
) -> Result<Option<Having>, ParseError> {
    let Some(expr) = having else {
        return Ok(None);
    };
    if group_by.is_empty() {
        return Err(unsupported("HAVING requires GROUP BY"));
    }
    Ok(Some(lower_having_expr(expr, group_by, select_items)?))
}

/// Lowers one `HAVING` expression node: a boolean connective/negation
/// recurses, and a comparison lowers via [`lower_having_binary`].
fn lower_having_expr(
    expr: &sql::Expr,
    group_by: &[ColRef],
    select_items: &[SelectItem],
) -> Result<Having, ParseError> {
    match expr {
        sql::Expr::BinaryOp { left, op, right } => {
            lower_having_binary(left, op, right, group_by, select_items)
        }
        sql::Expr::Nested(inner) => lower_having_expr(inner, group_by, select_items),
        sql::Expr::UnaryOp {
            op: sql::UnaryOperator::Not,
            expr,
        } => Ok(Having::Not(Box::new(lower_having_expr(
            expr,
            group_by,
            select_items,
        )?))),
        _ => Err(unsupported("this HAVING expression")),
    }
}

/// Lowers a `HAVING` binary operation: `AND`/`OR` recurse, anything else must
/// be a comparison between a [`HavingLeaf`] and a literal (see
/// [`lower_having_compare`]).
fn lower_having_binary(
    left: &sql::Expr,
    op: &sql::BinaryOperator,
    right: &sql::Expr,
    group_by: &[ColRef],
    select_items: &[SelectItem],
) -> Result<Having, ParseError> {
    use sql::BinaryOperator as B;
    match op {
        B::And => Ok(Having::And(
            Box::new(lower_having_expr(left, group_by, select_items)?),
            Box::new(lower_having_expr(right, group_by, select_items)?),
        )),
        B::Or => Ok(Having::Or(
            Box::new(lower_having_expr(left, group_by, select_items)?),
            Box::new(lower_having_expr(right, group_by, select_items)?),
        )),
        _ => {
            let cmp =
                cmp_op_from_binary(op).ok_or_else(|| unsupported(format!("operator `{op}`")))?;
            lower_having_compare(left, cmp, right, group_by, select_items)
        }
    }
}

/// Lowers a `HAVING` comparison. This subset only supports a [`HavingLeaf`]
/// (an aggregate call or a grouping-key column) compared against a plain
/// literal — never aggregate-vs-aggregate or an arbitrary expression. The
/// leaf may appear on either side (`count(*) > 1` or `1 < count(*)`); when
/// it's on the right, the operator is flipped so `Having::Compare` always
/// reads `leaf op literal`.
fn lower_having_compare(
    left: &sql::Expr,
    cmp: CmpOp,
    right: &sql::Expr,
    group_by: &[ColRef],
    select_items: &[SelectItem],
) -> Result<Having, ParseError> {
    if let Some(leaf) = try_having_leaf(left, group_by, select_items)? {
        return Ok(Having::Compare(leaf, cmp, lower_having_literal(right)?));
    }
    if let Some(leaf) = try_having_leaf(right, group_by, select_items)? {
        return Ok(Having::Compare(
            leaf,
            flip_cmp_op(cmp),
            lower_having_literal(left)?,
        ));
    }
    Err(unsupported("this HAVING comparison"))
}

/// Lowers the literal side of a `HAVING` comparison, once the other side has
/// already been confirmed to be a [`HavingLeaf`]. Replaces
/// [`lower_literal`]'s generic "this literal value" message with a
/// `HAVING`-specific one when this side isn't a literal at all — most often
/// aggregate-vs-aggregate (`HAVING count(*) < count(*)`), which this subset
/// doesn't support: a `HAVING` leaf may only be compared against a plain
/// literal, never against another leaf.
fn lower_having_literal(expr: &sql::Expr) -> Result<Literal, ParseError> {
    lower_literal(expr).map_err(|_| {
        unsupported("HAVING compares an aggregate or grouping-key column to a literal")
    })
}

/// Attempts to lower `expr` as a [`HavingLeaf`]: `Ok(None)` when `expr` isn't
/// shaped like a leaf at all (so the caller should try the other side of the
/// comparison), `Err` when it IS leaf-shaped but fails to lower — an unknown
/// function, or a column that isn't one of `group_by`'s keys — which is a
/// hard error rather than a fallback.
fn try_having_leaf(
    expr: &sql::Expr,
    group_by: &[ColRef],
    select_items: &[SelectItem],
) -> Result<Option<HavingLeaf>, ParseError> {
    match expr {
        sql::Expr::Function(func) => Ok(Some(HavingLeaf::Agg(lower_aggregate(func)?))),
        sql::Expr::Identifier(ident) => {
            // A bare identifier may name a `SELECT` alias — resolve it to what
            // it projects (an aggregate, or a GROUP BY-key column) before
            // falling back to treating it as a literal column. Alias resolution
            // wins, mirroring how `ORDER BY` resolves a projection alias.
            if let Some(leaf) = resolve_having_alias(&ident.value, group_by, select_items)? {
                return Ok(Some(leaf));
            }
            having_column_leaf(expr, group_by)
        }
        sql::Expr::CompoundIdentifier(_) => having_column_leaf(expr, group_by),
        _ => Ok(None),
    }
}

/// Lowers a bare column identifier as a [`HavingLeaf::Group`], erroring when it
/// isn't one of the query's `GROUP BY` keys — the fallback once alias
/// resolution ([`resolve_having_alias`]) has found no matching projection.
fn having_column_leaf(
    expr: &sql::Expr,
    group_by: &[ColRef],
) -> Result<Option<HavingLeaf>, ParseError> {
    let col = lower_col_ref(expr)?;
    if group_by.contains(&col) {
        Ok(Some(HavingLeaf::Group(col)))
    } else {
        Err(unsupported(format!(
            "HAVING column `{}` must be a GROUP BY key or a SELECT alias",
            col.label()
        )))
    }
}

/// Resolves a bare `HAVING` identifier that matches a `SELECT` alias to the
/// leaf it projects: an aggregate projection (`count(*) AS n` … `HAVING n`) →
/// [`HavingLeaf::Agg`]; a projection that is exactly a `GROUP BY`-key column
/// (`status AS s` … `HAVING s`) → [`HavingLeaf::Group`]. `Ok(None)` when no
/// projection carries that alias (the caller then tries it as a plain column).
/// An alias that projects anything else — a scalar expression, arithmetic, a
/// non-grouped column — is an `Err`: `HAVING`'s leaf grammar is
/// aggregate-or-group-key only, so such an alias has no valid `HAVING` meaning.
fn resolve_having_alias(
    name: &str,
    group_by: &[ColRef],
    select_items: &[SelectItem],
) -> Result<Option<HavingLeaf>, ParseError> {
    let Some(item) = select_items
        .iter()
        .find(|item| item.alias.as_deref() == Some(name))
    else {
        return Ok(None);
    };
    match &item.expr {
        SelectExpr::Agg(agg) => Ok(Some(HavingLeaf::Agg(agg.clone()))),
        SelectExpr::Expr(Expr::Col(col)) if group_by.contains(col) => {
            Ok(Some(HavingLeaf::Group(col.clone())))
        }
        SelectExpr::Expr(_) | SelectExpr::Star => Err(unsupported(format!(
            "HAVING alias `{name}` refers to a non-aggregate, non-GROUP BY expression"
        ))),
    }
}

/// Reverses a comparison operator (for a `HAVING` leaf found on the
/// right-hand side, e.g. `1 < count(*)` becomes `count(*) > 1`).
fn flip_cmp_op(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Eq => CmpOp::Eq,
        CmpOp::Ne => CmpOp::Ne,
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
    }
}

/// Lowers an ungrouped query's `ORDER BY` clause. A bare aggregate call has
/// no sort target to lower into here — [`OrderTarget`] has no aggregate
/// variant — so it is rejected outright, mirroring how [`lower_having`]
/// rejects `HAVING` without a `GROUP BY`.
fn lower_order_by(
    order_by: Option<&sql::OrderBy>,
    aliases: &[&str],
) -> Result<Vec<OrderKey>, ParseError> {
    order_by_terms(order_by)?
        .iter()
        .map(|order| match try_order_aggregate(&order.expr)? {
            Some(_) => Err(unsupported("ORDER BY an aggregate requires GROUP BY")),
            None => Ok(OrderKey {
                target: lower_order_target(&order.expr, aliases)?,
                desc: is_desc(order),
            }),
        })
        .collect()
}

/// Lowers a grouped query's `ORDER BY` clause, which additionally accepts a
/// bare aggregate call (e.g. `ORDER BY count(*) DESC`, no `AS` alias needed).
///
/// That aggregate form still requires *explicit* `keys`: the implicit single
/// group of an aggregate-only `SELECT` doesn't qualify, matching how
/// [`lower_having`] rejects `HAVING` on the same query shape.
fn lower_grouped_order_by(
    order_by: Option<&sql::OrderBy>,
    aliases: &[&str],
    keys: &[ColRef],
) -> Result<Vec<GroupedOrderKey>, ParseError> {
    order_by_terms(order_by)?
        .iter()
        .map(|order| {
            let target = match try_order_aggregate(&order.expr)? {
                Some(_) if keys.is_empty() => {
                    return Err(unsupported("ORDER BY an aggregate requires GROUP BY"));
                }
                Some(agg) => GroupedOrderTarget::Agg(agg),
                None => GroupedOrderTarget::Scalar(lower_order_target(&order.expr, aliases)?),
            };
            Ok(GroupedOrderKey {
                target,
                desc: is_desc(order),
            })
        })
        .collect()
}

/// The terms of an `ORDER BY` clause, or an empty slice when the query has no
/// `ORDER BY` at all. `ORDER BY ALL` is outside this subset.
fn order_by_terms(order_by: Option<&sql::OrderBy>) -> Result<&[sql::OrderByExpr], ParseError> {
    let Some(order_by) = order_by else {
        return Ok(&[]);
    };
    match &order_by.kind {
        sql::OrderByKind::Expressions(exprs) => Ok(exprs),
        sql::OrderByKind::All(_) => Err(unsupported("ORDER BY ALL")),
    }
}

/// Lowers an `ORDER BY` term's aggregate form, if it has one: a function call
/// is always a bare aggregate here, and anything else is `None` so the caller
/// falls through to [`lower_order_target`].
///
/// [`lower_aggregate`] runs FIRST, before either caller's "requires GROUP BY"
/// check: it already rejects a non-aggregate function name (e.g. `ORDER BY
/// upper(status)`) with a clean "function `upper`" message, so that case must
/// never be misreported as "requires GROUP BY" — a message that only makes
/// sense once the function is confirmed to actually be an aggregate.
fn try_order_aggregate(expr: &sql::Expr) -> Result<Option<Aggregate>, ParseError> {
    match expr {
        sql::Expr::Function(func) => Ok(Some(lower_aggregate(func)?)),
        _ => Ok(None),
    }
}

/// Lowers an `ORDER BY` term's non-aggregate target: an identifier matching a
/// projection alias becomes [`OrderTarget::Alias`], a bare or compound
/// identifier a plain column via [`lower_col_ref`], and everything else
/// (arithmetic, `CASE`, a scalar-fn call, …) an [`OrderTarget::Expr`] via
/// [`lower_expr`] — so `ORDER BY` accepts the same expression grammar a
/// `SELECT` item does.
fn lower_order_target(expr: &sql::Expr, aliases: &[&str]) -> Result<OrderTarget, ParseError> {
    match expr {
        sql::Expr::Identifier(ident) if aliases.contains(&ident.value.as_str()) => {
            Ok(OrderTarget::Alias(ident.value.clone()))
        }
        sql::Expr::Identifier(_) | sql::Expr::CompoundIdentifier(_) => {
            Ok(OrderTarget::Col(lower_col_ref(expr)?))
        }
        other => Ok(OrderTarget::Expr(lower_expr(other)?)),
    }
}

/// Whether an `ORDER BY` term asked for `DESC`; `ASC` is the default.
fn is_desc(order: &sql::OrderByExpr) -> bool {
    order.options.asc == Some(false)
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

    /// The `GROUP BY` keys, `HAVING`, and `ORDER BY` of a query the test
    /// expects to have lowered to [`Grouping::Grouped`] — panicking when it
    /// didn't, since an assertion about those clauses is meaningless on a
    /// query the parser decided was ungrouped.
    fn grouped(q: &Query) -> (&[ColRef], Option<&Having>, &[GroupedOrderKey]) {
        match &q.grouping {
            Grouping::Grouped {
                keys,
                having,
                order_by,
            } => (keys, having.as_ref(), order_by),
            Grouping::Ungrouped { .. } => panic!("expected a grouped query: {:?}", q.grouping),
        }
    }

    /// The `DISTINCT` flag and `ORDER BY` of a query the test expects to have
    /// lowered to [`Grouping::Ungrouped`]; the counterpart to [`grouped`].
    fn ungrouped(q: &Query) -> (bool, &[OrderKey]) {
        match &q.grouping {
            Grouping::Ungrouped { distinct, order_by } => (*distinct, order_by),
            Grouping::Grouped { .. } => panic!("expected an ungrouped query: {:?}", q.grouping),
        }
    }

    #[test]
    fn select_fields_with_alias_no_from() {
        let q = parse("SELECT status, count(*) AS Count GROUP BY status").unwrap();
        assert_eq!(
            q.select[0],
            SelectItem {
                expr: SelectExpr::Expr(Expr::Col(ColRef::Field(vec!["status".into()]))),
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
        let (keys, _, _) = grouped(&q);
        assert_eq!(keys, [ColRef::Field(vec!["status".into()])]);
        assert_eq!(q.from_glob, None);
    }
    #[test]
    fn file_pseudo_columns() {
        let q = parse("SELECT file.name, file.folder WHERE file.ext = 'md'").unwrap();
        assert_eq!(
            q.select[0].expr,
            SelectExpr::Expr(Expr::Col(ColRef::File(FileAttr::Name)))
        );
        assert_eq!(
            q.select[1].expr,
            SelectExpr::Expr(Expr::Col(ColRef::File(FileAttr::Folder)))
        );
        match q.filter.unwrap() {
            Predicate::Compare(
                Expr::Col(ColRef::File(FileAttr::Ext)),
                CmpOp::Eq,
                Expr::Lit(Literal::Str(s)),
            ) => {
                assert_eq!(s, "md")
            }
            p => panic!("unexpected {p:?}"),
        }
    }
    // Task 6 (W56): `file.word_count` recognized the same way as the other
    // `file.*` pseudo-columns.
    #[test]
    fn file_word_count_pseudo_column() {
        let q = parse("SELECT file.word_count").unwrap();
        assert_eq!(
            q.select[0].expr,
            SelectExpr::Expr(Expr::Col(ColRef::File(FileAttr::WordCount)))
        );
    }
    // Task 7 (W56 part 2): `file.body` recognized the same way as the other
    // `file.*` pseudo-columns.
    #[test]
    fn file_body_pseudo_column() {
        let q = parse("SELECT file.body WHERE file.body LIKE '%TODO%'").unwrap();
        assert_eq!(
            q.select[0].expr,
            SelectExpr::Expr(Expr::Col(ColRef::File(FileAttr::Body)))
        );
        match q.filter.unwrap() {
            Predicate::Like(Expr::Col(ColRef::File(FileAttr::Body)), pattern, false) => {
                assert_eq!(pattern, LikePattern::new("%TODO%"))
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
        // IN and NOT IN carry the tested expression, the literal list, and
        // the negation flag.
        assert_eq!(
            parse("SELECT jira WHERE status IN ('a','b')")
                .unwrap()
                .filter,
            Some(Predicate::In(
                Expr::Col(ColRef::Field(vec!["status".into()])),
                vec![Literal::Str("a".into()), Literal::Str("b".into())],
                false
            ))
        );
        assert_eq!(
            parse("SELECT jira WHERE status NOT IN ('a','b')")
                .unwrap()
                .filter,
            Some(Predicate::In(
                Expr::Col(ColRef::Field(vec!["status".into()])),
                vec![Literal::Str("a".into()), Literal::Str("b".into())],
                true
            ))
        );

        // LIKE and NOT LIKE carry the tested expression, the pattern, and
        // the negation flag.
        assert_eq!(
            parse("SELECT jira WHERE slice LIKE 'mobile%'")
                .unwrap()
                .filter,
            Some(Predicate::Like(
                Expr::Col(ColRef::Field(vec!["slice".into()])),
                LikePattern::new("mobile%"),
                false
            ))
        );
        assert_eq!(
            parse("SELECT jira WHERE slice NOT LIKE 'mobile%'")
                .unwrap()
                .filter,
            Some(Predicate::Like(
                Expr::Col(ColRef::Field(vec!["slice".into()])),
                LikePattern::new("mobile%"),
                true
            ))
        );

        // IS NULL / IS NOT NULL carry the tested expression and the
        // negation flag.
        assert_eq!(
            parse("SELECT jira WHERE epic IS NULL").unwrap().filter,
            Some(Predicate::IsNull(
                Expr::Col(ColRef::Field(vec!["epic".into()])),
                false
            ))
        );
        assert_eq!(
            parse("SELECT jira WHERE epic IS NOT NULL").unwrap().filter,
            Some(Predicate::IsNull(
                Expr::Col(ColRef::Field(vec!["epic".into()])),
                true
            ))
        );
    }
    #[test]
    fn where_scalar_expr_widens_like_in_isnull_member_of() {
        // W43: the tested side of LIKE/IN/IS NULL/MEMBER OF now lowers
        // through the full `Expr` grammar, not just a bare column.
        assert_eq!(
            parse("SELECT x WHERE lower(status) LIKE '%d%'")
                .unwrap()
                .filter,
            Some(Predicate::Like(
                Expr::Scalar(
                    ScalarFn::Lower,
                    vec![Expr::Col(ColRef::Field(vec!["status".into()]))]
                ),
                LikePattern::new("%d%"),
                false
            ))
        );
        assert_eq!(
            parse("SELECT x WHERE lead MEMBER OF(tags)").unwrap().filter,
            Some(Predicate::MemberOf(
                Expr::Col(ColRef::Field(vec!["lead".into()])),
                ColRef::Field(vec!["tags".into()]),
                false
            ))
        );
    }
    #[test]
    fn regexp_shape() {
        // REGEXP and NOT REGEXP carry the operand (a general `Expr`, not
        // just a `ColRef`), the pattern, and the negation flag.
        assert_eq!(
            parse("SELECT jira WHERE jira REGEXP '^DCP-[0-9]+$'")
                .unwrap()
                .filter,
            Some(Predicate::Regexp(
                Expr::Col(ColRef::Field(vec!["jira".into()])),
                RegexPattern::new("^DCP-[0-9]+$").unwrap(),
                false
            ))
        );
        assert_eq!(
            parse("SELECT jira WHERE jira NOT REGEXP '^DCP-'")
                .unwrap()
                .filter,
            Some(Predicate::Regexp(
                Expr::Col(ColRef::Field(vec!["jira".into()])),
                RegexPattern::new("^DCP-").unwrap(),
                true
            ))
        );
        // The left side lowers through the full `Expr` grammar, so a
        // scalar-fn call works as the operand, unlike `LIKE`'s column-only
        // left side.
        assert_eq!(
            parse("SELECT jira WHERE lower(status) REGEXP 'draft'")
                .unwrap()
                .filter,
            Some(Predicate::Regexp(
                Expr::Scalar(
                    ScalarFn::Lower,
                    vec![Expr::Col(ColRef::Field(vec!["status".into()]))]
                ),
                RegexPattern::new("draft").unwrap(),
                false
            ))
        );
        // `RLIKE` is sqlparser's alias for the same node (identical
        // semantics), so it lowers the same way.
        assert_eq!(
            parse("SELECT jira WHERE jira RLIKE '^DCP-'")
                .unwrap()
                .filter,
            Some(Predicate::Regexp(
                Expr::Col(ColRef::Field(vec!["jira".into()])),
                RegexPattern::new("^DCP-").unwrap(),
                false
            ))
        );
    }
    #[test]
    fn regexp_bad_pattern_rejected_at_parse_time() {
        let err = parse("SELECT jira WHERE jira REGEXP '('").unwrap_err();
        assert!(matches!(err, ParseError::Unsupported(_)), "{err:?}");
    }
    #[test]
    fn member_of_shape() {
        // `MEMBER OF` carries the tested value (a general `Expr`, lowered
        // through `lower_expr`), the column, and the negation flag. Only the
        // prefix `NOT <value> MEMBER OF(...)` form is valid SQL under
        // sqlparser 0.62 (the postfix `<value> NOT MEMBER OF(...)` is a
        // syntax error at the sqlparser level — see `lower_not`); the prefix
        // form still lands as a flat negated flag, not a wrapping
        // `Predicate::Not`.
        assert_eq!(
            parse("SELECT jira WHERE 'mobile' MEMBER OF(tags)")
                .unwrap()
                .filter,
            Some(Predicate::MemberOf(
                Expr::Lit(Literal::Str("mobile".into())),
                ColRef::Field(vec!["tags".into()]),
                false
            ))
        );
        assert_eq!(
            parse("SELECT jira WHERE NOT 'mobile' MEMBER OF(tags)")
                .unwrap()
                .filter,
            Some(Predicate::MemberOf(
                Expr::Lit(Literal::Str("mobile".into())),
                ColRef::Field(vec!["tags".into()]),
                true
            ))
        );
    }

    #[test]
    fn member_of_now_accepts_a_bare_column_value() {
        // W43: the value side now lowers through the full `Expr` grammar, so
        // what used to be rejected — a bare column on the value side — is
        // now the common, intended form (`lead MEMBER OF(tags)`).
        assert_eq!(
            parse("SELECT jira WHERE status MEMBER OF(tags)")
                .unwrap()
                .filter,
            Some(Predicate::MemberOf(
                Expr::Col(ColRef::Field(vec!["status".into()])),
                ColRef::Field(vec!["tags".into()]),
                false
            ))
        );
    }

    #[test]
    fn member_of_rejects_non_column_array() {
        // The array side must still lower to a single column reference —
        // widening only loosened the value side (see the test above). The
        // error now comes straight from `lower_col_ref` rather than the old
        // generic "this MEMBER OF form".
        assert!(matches!(
            parse("SELECT jira WHERE 'mobile' MEMBER OF(1)"),
            Err(ParseError::BadColumn(_))
        ));
    }
    #[test]
    fn order_and_limit() {
        let q =
            parse("SELECT status, count(*) AS n GROUP BY status ORDER BY n DESC LIMIT 5 OFFSET 2")
                .unwrap();
        let (_, _, order_by) = grouped(&q);
        assert_eq!(
            order_by,
            [GroupedOrderKey {
                target: GroupedOrderTarget::Scalar(OrderTarget::Alias("n".into())),
                desc: true
            }]
        );
        assert_eq!(q.limit, Some(5));
        assert_eq!(q.offset, Some(2));
    }
    #[test]
    fn order_by_bare_aggregate() {
        // No `AS` alias needed: `ORDER BY count(*)` lowers straight to an
        // aggregate order target.
        let q = parse("SELECT status, count(*) GROUP BY status ORDER BY count(*) DESC").unwrap();
        let (_, _, order_by) = grouped(&q);
        assert_eq!(
            order_by,
            [GroupedOrderKey {
                target: GroupedOrderTarget::Agg(Aggregate::CountStar),
                desc: true
            }]
        );
    }
    #[test]
    fn order_by_arbitrary_expr_lowers_to_expr_target() {
        // Anything beyond a bare alias/column/aggregate — arithmetic, a
        // scalar-fn call, `CASE`, … — lowers through `lower_expr` into
        // `OrderTarget::Expr`, generalizing `ORDER BY` to the same
        // expression grammar a `SELECT` item accepts.
        let q =
            parse("SELECT status ORDER BY CASE WHEN status = 'draft' THEN 0 ELSE 1 END").unwrap();
        let (_, order_by) = ungrouped(&q);
        assert_eq!(
            order_by,
            [OrderKey {
                target: OrderTarget::Expr(Expr::Case {
                    operand: None,
                    whens: vec![(
                        Expr::Predicate(Box::new(Predicate::Compare(
                            Expr::Col(ColRef::Field(vec!["status".into()])),
                            CmpOp::Eq,
                            Expr::Lit(Literal::Str("draft".into()))
                        ))),
                        Expr::Lit(Literal::Int(0))
                    )],
                    else_expr: Some(Box::new(Expr::Lit(Literal::Int(1)))),
                }),
                desc: false
            }]
        );
    }
    #[test]
    fn order_by_aggregate_ungrouped_errors() {
        assert!(crate::query::parse("SELECT status ORDER BY count(*)").is_err());
    }
    #[test]
    fn order_by_aggregate_implicit_group_errors() {
        // This query IS grouped (the bare aggregate SELECT item makes one
        // implicit group), but its grouping `keys` are empty — and an
        // aggregate ORDER BY needs a real GROUP BY, matching how `HAVING`
        // rejects that same case.
        assert!(crate::query::parse("SELECT count(*) ORDER BY count(*)").is_err());
    }
    #[test]
    fn order_by_scalar_fn_does_not_claim_requires_group_by() {
        // `upper` isn't an aggregate at all, so the absent GROUP BY must
        // never be blamed — the error should name the unsupported function
        // instead of the (irrelevant) GROUP BY requirement.
        let err = crate::query::parse("SELECT status ORDER BY upper(status)").unwrap_err();
        let message = err.to_string();
        assert!(!message.contains("requires GROUP BY"), "got: {message}");
        assert!(message.contains("upper"), "got: {message}");
    }
    #[test]
    fn group_by_resolves_select_alias() {
        // GROUP BY s resolves through the `status AS s` alias, exactly as
        // `GROUP BY status` would.
        let q = parse("SELECT status AS s, count(*) AS n GROUP BY s ORDER BY s").unwrap();
        let (keys, _, _) = grouped(&q);
        assert_eq!(keys, [ColRef::Field(vec!["status".into()])]);
    }
    #[test]
    fn group_by_alias_precedence_over_same_named_field() {
        // A name that is both a real field and a SELECT alias resolves to
        // the alias, matching how ORDER BY resolves the same ambiguity.
        let q = parse("SELECT jira AS status, count(*) AS n GROUP BY status").unwrap();
        let (keys, _, _) = grouped(&q);
        assert_eq!(keys, [ColRef::Field(vec!["jira".into()])]);
    }
    #[test]
    fn group_by_alias_on_aggregate_or_expr_is_rejected() {
        assert!(parse("SELECT count(*) AS n GROUP BY n").is_err());
    }
    #[test]
    fn group_by_alias_on_computed_expr_is_rejected() {
        assert!(parse("SELECT lower(status) AS lx GROUP BY lx").is_err());
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
        let prd = ColRef::Field(vec!["prd".into()]);
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
            SelectExpr::Agg(Aggregate::GroupConcat(ColRef::Field(vec!["jira".into()])))
        );
        assert_eq!(
            q.select[5].expr,
            SelectExpr::Agg(Aggregate::Count(ColRef::Field(vec!["status".into()]), true))
        );
        let (keys, _, _) = grouped(&q);
        assert_eq!(keys, [ColRef::Field(vec!["epic".into()])]);
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
    fn header_derivation_for_computed_expr() {
        // A computed expression with no alias falls back to a rendered form;
        // a bare column (even through `lower_expr`) still keeps its plain
        // field label (pinned separately by `header_derivation` above).
        let header = |sql: &str| parse(sql).unwrap().select[0].header();
        assert_eq!(header("SELECT lower(status)"), "lower(status)");
        assert_eq!(header("SELECT a + b"), "a + b");
        assert_eq!(header("SELECT a || '-' || status"), "a || '-' || status");
    }

    #[test]
    fn header_derivation_for_relative_date_literal() {
        // A bare relative-date literal in the SELECT list renders back to
        // its source token, not a resolved date (rendering has no clock —
        // resolution only happens in `exec`).
        let header = |sql: &str| parse(sql).unwrap().select[0].header();
        assert_eq!(header("SELECT '-7d'"), "'-7d'");
        assert_eq!(header("SELECT 'today'"), "'today'");
        assert_eq!(header("SELECT '+3w'"), "'+3w'");
    }

    #[test]
    fn lower_expr_scalar_and_arithmetic_shapes() {
        let q = parse("SELECT lower(status), a + b, a || '-' || status").unwrap();
        assert_eq!(
            q.select[0].expr,
            SelectExpr::Expr(Expr::Scalar(
                ScalarFn::Lower,
                vec![Expr::Col(ColRef::Field(vec!["status".into()]))]
            ))
        );
        assert_eq!(
            q.select[1].expr,
            SelectExpr::Expr(Expr::Binary(
                BinOp::Add,
                Box::new(Expr::Col(ColRef::Field(vec!["a".into()]))),
                Box::new(Expr::Col(ColRef::Field(vec!["b".into()])))
            ))
        );
        assert_eq!(
            q.select[2].expr,
            SelectExpr::Expr(Expr::Binary(
                BinOp::Concat,
                Box::new(Expr::Binary(
                    BinOp::Concat,
                    Box::new(Expr::Col(ColRef::Field(vec!["a".into()]))),
                    Box::new(Expr::Lit(Literal::Str("-".into())))
                )),
                Box::new(Expr::Col(ColRef::Field(vec!["status".into()])))
            ))
        );
    }

    #[test]
    fn lower_expr_trim_and_substr_forms() {
        // `trim`/`ltrim`/`rtrim`/`substr` each parse through a different
        // sqlparser AST shape (a plain `Function`, or the dedicated
        // `Trim`/`Substring` nodes) — pin that all four land on the right
        // `ScalarFn`.
        let col = || Expr::Col(ColRef::Field(vec!["status".into()]));
        assert_eq!(
            parse("SELECT trim(status)").unwrap().select[0].expr,
            SelectExpr::Expr(Expr::Scalar(ScalarFn::Trim, vec![col()]))
        );
        assert_eq!(
            parse("SELECT ltrim(status)").unwrap().select[0].expr,
            SelectExpr::Expr(Expr::Scalar(ScalarFn::Ltrim, vec![col()]))
        );
        assert_eq!(
            parse("SELECT rtrim(status)").unwrap().select[0].expr,
            SelectExpr::Expr(Expr::Scalar(ScalarFn::Rtrim, vec![col()]))
        );
        assert_eq!(
            parse("SELECT substr(status, 2, 3)").unwrap().select[0].expr,
            SelectExpr::Expr(Expr::Scalar(
                ScalarFn::Substr,
                vec![
                    col(),
                    Expr::Lit(Literal::Int(2)),
                    Expr::Lit(Literal::Int(3))
                ]
            ))
        );
        assert_eq!(
            parse("SELECT substr(status, 2)").unwrap().select[0].expr,
            SelectExpr::Expr(Expr::Scalar(
                ScalarFn::Substr,
                vec![col(), Expr::Lit(Literal::Int(2))]
            ))
        );
    }

    #[test]
    fn trim_custom_characters_is_unsupported() {
        // `TRIM('x' FROM s)` is outside the whitespace-only scalar set.
        assert!(matches!(
            parse("SELECT trim('x' from status)"),
            Err(ParseError::Unsupported(_))
        ));
    }

    #[test]
    fn aggregate_inside_expression_is_unsupported() {
        // `count(*)` alone stays a bare aggregate; wrapped in arithmetic it
        // needs group-context threading this batch doesn't do.
        assert!(matches!(
            parse("SELECT count(*)").unwrap().select[0].expr,
            SelectExpr::Agg(Aggregate::CountStar)
        ));
        assert!(matches!(
            parse("SELECT count(*) + 1"),
            Err(ParseError::Unsupported(_))
        ));
        assert!(matches!(
            parse("SELECT lower(status) + min(status)"),
            Err(ParseError::Unsupported(_))
        ));
    }

    #[test]
    fn coalesce_parses_and_references_all_columns() {
        let q = parse("SELECT COALESCE(epic, 'none') AS e").unwrap();
        assert!(q.referenced_fields().contains("epic"));
        assert_eq!(q.select[0].header(), "e");
        // zero-arg is an error
        assert!(parse("SELECT COALESCE() AS e").is_err());
        // aggregate inside coalesce rejected
        assert!(parse("SELECT COALESCE(count(*), 0)").is_err());
    }

    #[test]
    fn coalesce_default_header() {
        let q = parse("SELECT COALESCE(epic, 'none')").unwrap();
        assert_eq!(q.select[0].header(), "coalesce(epic, 'none')");
    }

    #[test]
    fn unknown_scalar_function_is_unsupported() {
        assert!(matches!(
            parse("SELECT frobnicate(status)"),
            Err(ParseError::Unsupported(_))
        ));
    }

    #[test]
    fn scalar_function_wrong_arity_is_unsupported() {
        assert!(matches!(
            parse("SELECT lower(status, status)"),
            Err(ParseError::Unsupported(_))
        ));
        assert!(matches!(
            parse("SELECT replace(status, status)"),
            Err(ParseError::Unsupported(_))
        ));
    }

    #[test]
    fn scalar_arity_error_names_function_and_expected_arity() {
        // The message must name both the function and its expected arity,
        // not just the count the caller passed — pinned end-to-end through
        // a real query for a fixed arity (`lower` takes exactly 1 arg).
        let err = parse("SELECT lower(status, status)").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported query feature: lower() takes 1 argument, got 2"
        );

        // `substr`'s range arity ("2 or 3 arguments") is exercised directly:
        // sqlparser's dedicated `SUBSTRING` AST node means a too-few-args
        // `substr(...)` call is rejected by `lower_substr` before this
        // check ever runs (see `substr() without a start position` above),
        // so this message is reachable only via `check_scalar_arity`
        // itself — still worth pinning since it's the same fix.
        let err = check_scalar_arity(&ScalarFn::Substr, "substr", 1).unwrap_err();
        assert_eq!(
            err.to_string(),
            "unsupported query feature: substr() takes 2 or 3 arguments, got 1"
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
                Expr::Col(ColRef::Field(vec!["title".into()])),
                CmpOp::Eq,
                Expr::Lit(Literal::Str("imported from \"archive\"".into()))
            ))
        );
    }

    #[test]
    fn having_lowers_to_compare_of_agg_and_literal() {
        let q = parse("SELECT status, count(*) AS n GROUP BY status HAVING count(*) > 1").unwrap();
        let (_, having, _) = grouped(&q);
        assert_eq!(
            having,
            Some(&Having::Compare(
                HavingLeaf::Agg(Aggregate::CountStar),
                CmpOp::Gt,
                Literal::Int(1)
            ))
        );
    }
    #[test]
    fn having_leaf_may_be_a_grouping_key_column() {
        let q = parse("SELECT status GROUP BY status HAVING status = 'draft'").unwrap();
        let (_, having, _) = grouped(&q);
        assert_eq!(
            having,
            Some(&Having::Compare(
                HavingLeaf::Group(ColRef::Field(vec!["status".into()])),
                CmpOp::Eq,
                Literal::Str("draft".into())
            ))
        );
    }
    #[test]
    fn having_leaf_on_right_flips_operator() {
        let q = parse("SELECT status GROUP BY status HAVING 1 < count(*)").unwrap();
        let (_, having, _) = grouped(&q);
        assert_eq!(
            having,
            Some(&Having::Compare(
                HavingLeaf::Agg(Aggregate::CountStar),
                CmpOp::Gt,
                Literal::Int(1)
            ))
        );
    }
    #[test]
    fn having_supports_and_or_not() {
        let q = parse(
            "SELECT status GROUP BY status HAVING count(*) > 1 AND NOT (status = 'draft' OR status = 'x')",
        )
        .unwrap();
        let (_, having, _) = grouped(&q);
        assert!(matches!(having, Some(Having::And(_, _))));
    }
    #[test]
    fn having_non_group_key_column_is_unsupported() {
        // `prd` isn't a GROUP BY key, so referencing it in HAVING is as
        // invalid as selecting it would be under GROUP BY.
        assert!(matches!(
            parse("SELECT status GROUP BY status HAVING prd = '010'"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn having_without_group_by_is_unsupported() {
        assert!(matches!(
            parse("SELECT status HAVING count(*) > 1"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn having_resolves_an_aggregate_select_alias() {
        // `count` is the alias for count(*); HAVING resolves it to the
        // aggregate, exactly as if the user had written `HAVING count(*) > 1`.
        let q = parse("SELECT type, count(*) AS count GROUP BY type HAVING count > 1").unwrap();
        let (_, having, _) = grouped(&q);
        assert_eq!(
            having,
            Some(&Having::Compare(
                HavingLeaf::Agg(Aggregate::CountStar),
                CmpOp::Gt,
                Literal::Int(1)
            ))
        );
        // The alias form and the aggregate-call form must lower identically.
        let agg_form =
            parse("SELECT type, count(*) AS count GROUP BY type HAVING count(*) > 1").unwrap();
        assert_eq!(having, grouped(&agg_form).1);
    }
    #[test]
    fn having_resolves_an_aggregate_alias_on_either_side() {
        // Alias resolution also works when the leaf is on the right of the
        // comparison (operator flips, same as a bare aggregate call).
        let q = parse("SELECT type, count(*) AS n GROUP BY type HAVING 1 < n").unwrap();
        let (_, having, _) = grouped(&q);
        assert_eq!(
            having,
            Some(&Having::Compare(
                HavingLeaf::Agg(Aggregate::CountStar),
                CmpOp::Gt,
                Literal::Int(1)
            ))
        );
    }
    #[test]
    fn having_resolves_a_group_key_select_alias() {
        // Aliasing a GROUP BY key and referencing the alias in HAVING resolves
        // to that group key (previously rejected as "not a GROUP BY key").
        let q = parse("SELECT status AS s GROUP BY status HAVING s = 'draft'").unwrap();
        let (_, having, _) = grouped(&q);
        assert_eq!(
            having,
            Some(&Having::Compare(
                HavingLeaf::Group(ColRef::Field(vec!["status".into()])),
                CmpOp::Eq,
                Literal::Str("draft".into())
            ))
        );
    }
    #[test]
    fn having_alias_to_a_non_aggregate_non_group_expr_is_unsupported() {
        // `u` aliases a scalar expression over a group key — constant per
        // group, but HAVING's leaf grammar is aggregate-or-group-key only, so
        // referencing it is rejected rather than silently mis-evaluated.
        assert!(matches!(
            parse("SELECT status, lower(status) AS u GROUP BY status HAVING u = 'draft'"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn having_unknown_identifier_still_rejected_with_no_matching_alias() {
        // A bare identifier that is neither a GROUP BY key nor a SELECT alias
        // is still an error.
        assert!(matches!(
            parse("SELECT status, count(*) AS n GROUP BY status HAVING bogus > 1"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn having_aggregate_vs_aggregate_is_unsupported() {
        let err = parse("SELECT status GROUP BY status HAVING count(*) > sum(prd)").unwrap_err();
        assert!(matches!(err, ParseError::Unsupported(_)));
        assert!(
            err.to_string().contains("aggregate or grouping-key column"),
            "got: {err}"
        );
    }
    #[test]
    fn distinct_sets_the_flag() {
        assert!(!ungrouped(&parse("SELECT jira").unwrap()).0);
        assert!(!ungrouped(&parse("SELECT ALL jira").unwrap()).0);
        assert!(ungrouped(&parse("SELECT DISTINCT jira").unwrap()).0);
    }
    #[test]
    fn rejects_distinct_on() {
        assert!(matches!(
            parse("SELECT DISTINCT ON (jira) jira"),
            Err(ParseError::Unsupported(_))
        ));
    }
    #[test]
    fn distinct_with_group_by_is_rejected() {
        assert!(parse("SELECT DISTINCT status, count(*) GROUP BY status").is_err());
    }
    #[test]
    fn distinct_with_implicit_aggregate_grouping_is_rejected() {
        // An aggregate SELECT item groups every row into one implicit group,
        // so `DISTINCT` is as redundant/confusing here as it is alongside an
        // explicit `GROUP BY` — and it used to be silently dropped, since the
        // executor only ever read `distinct` on the ungrouped path.
        let err = parse("SELECT DISTINCT count(*) FROM x").unwrap_err();
        assert!(matches!(err, ParseError::Unsupported(_)));
        assert_eq!(
            err.to_string(),
            "unsupported query feature: DISTINCT combined with aggregate functions"
        );
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
        // A CAST in a comparison, an IN subquery, and a BETWEEN all route
        // through catch-all arms that used to `{:?}` the node. (Arithmetic on
        // a comparison's RHS, e.g. `status = status + 1`, is no longer one of
        // these — Task 3 made both comparison sides full expressions.)
        for sql in [
            "SELECT status WHERE CAST(prd AS INT) = 1",
            "SELECT status WHERE status IN (SELECT status)",
            "SELECT status WHERE status BETWEEN 'a' AND 'z'",
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

    #[test]
    fn dotted_identifier_lowers_to_path() {
        let q = parse("SELECT estimate.low WHERE estimate.high > 10").unwrap();
        assert!(q.referenced_fields().contains("estimate"));
        // referenced_fields returns the TOP-LEVEL segment only
        assert!(!q.referenced_fields().contains("high"));
        assert!(!q.referenced_fields().contains("low"));
    }

    #[test]
    fn file_attr_still_special_and_no_nesting() {
        assert!(parse("SELECT file.name").is_ok());
        assert!(parse("SELECT file.name.x").is_err()); // file.* has no nesting
    }

    #[test]
    fn relative_date_string_lowers_to_reldate_literal() {
        use crate::query::ast::{DateUnit, RelDate};
        let q = parse("SELECT file.name WHERE created >= '-7d'").unwrap();
        assert_eq!(
            q.filter,
            Some(Predicate::Compare(
                Expr::Col(ColRef::Field(vec!["created".into()])),
                CmpOp::Ge,
                Expr::Lit(Literal::RelativeDate(RelDate::Offset {
                    n: -7,
                    unit: DateUnit::Day
                }))
            ))
        );

        // A non-matching string stays a plain `Str` literal — no
        // pre-existing string literal is reinterpreted (spec §9).
        let q2 = parse("SELECT file.name WHERE status = 'draft'").unwrap();
        assert_eq!(
            q2.filter,
            Some(Predicate::Compare(
                Expr::Col(ColRef::Field(vec!["status".into()])),
                CmpOp::Eq,
                Expr::Lit(Literal::Str("draft".into()))
            ))
        );
    }

    #[test]
    fn like_pattern_resembling_reldate_stays_literal_text() {
        // `Predicate::Like`'s pattern is a `LikePattern` over the literal
        // pattern text, never a `Literal` — it must never be reinterpreted as
        // a relative date even when the pattern text happens to match the
        // grammar.
        let q = parse("SELECT file.name WHERE jira LIKE '-7d'").unwrap();
        assert_eq!(
            q.filter,
            Some(Predicate::Like(
                Expr::Col(ColRef::Field(vec!["jira".into()])),
                LikePattern::new("-7d"),
                false
            ))
        );
    }
}
