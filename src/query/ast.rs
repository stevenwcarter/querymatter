//! The query AST for the SQL subset understood by querymatter.
//!
//! These types are the contract between the parser (`query::parse`) and the
//! executor (`query::exec`): the parser lowers a `sqlparser` parse tree into a
//! [`Query`], and the executor evaluates a [`Query`] against the record store.
//! The shapes here deliberately model only the supported subset, so an
//! ill-formed or unsupported query is rejected at parse time rather than being
//! representable in the AST.

use std::collections::BTreeSet;

use crate::model::FileAttr;

/// A fully-parsed query: the projection plus its optional clauses.
///
/// Absent clauses are represented by empty collections / `None` rather than a
/// sentinel, so the executor can treat "no `GROUP BY`" and "grouped by nothing"
/// uniformly.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// Projected columns / expressions, in output order.
    pub select: Vec<SelectItem>,
    /// Whether `SELECT DISTINCT` was specified: duplicate result rows (keyed
    /// on the final projected cells) are dropped, keeping the first
    /// occurrence. The parser rejects this combined with `GROUP BY` rather
    /// than leaving that combination representable.
    pub distinct: bool,
    /// The glob or directory the query scans, if a `FROM` clause was given.
    pub from_glob: Option<String>,
    /// The `WHERE` predicate, if any.
    pub filter: Option<Predicate>,
    /// `GROUP BY` keys, in order; empty when the query is not grouped.
    pub group_by: Vec<ColRef>,
    /// The `HAVING` predicate over group aggregates, if any. Only meaningful
    /// alongside a non-empty `group_by` — the parser rejects `HAVING` on an
    /// ungrouped query rather than leaving this representable.
    pub having: Option<Having>,
    /// `ORDER BY` keys, in order; empty when the query is unordered.
    pub order_by: Vec<OrderKey>,
    /// `LIMIT` row cap, if given.
    pub limit: Option<usize>,
    /// `OFFSET` row skip, if given.
    pub offset: Option<usize>,
}

/// One item in the `SELECT` projection: an expression with an optional alias.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectItem {
    /// The projected expression.
    pub expr: SelectExpr,
    /// An explicit `AS <alias>`, if present.
    pub alias: Option<String>,
}

/// A projectable expression: `*`, a scalar expression, or an aggregate.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectExpr {
    /// The `*` wildcard, expanding to every frontmatter field.
    Star,
    /// A scalar expression: a column, literal, scalar-fn call, or
    /// arithmetic/concat. A bare column is `Expr(Expr::Col(_))`.
    Expr(Expr),
    /// An aggregate over the current group.
    Agg(Aggregate),
}

/// A reference to a queryable column: either a frontmatter field (possibly
/// dotted into a nested `Value::Map`) or a `file.*` pseudo-column.
#[derive(Debug, Clone, PartialEq)]
pub enum ColRef {
    /// A YAML frontmatter field path: a bare field is one segment
    /// (`status`); each further segment indexes into a nested `Value::Map`
    /// (`estimate.low` lowers to `["estimate", "low"]`). Never empty for any
    /// `ColRef` the parser builds.
    Field(Vec<String>),
    /// A `file.*` pseudo-column (`file.name`, `file.path`, …).
    File(FileAttr),
}

/// A scalar expression: evaluates to one [`crate::model::Value`] per row
/// (ungrouped) or per group cell (grouped, over the group's representative
/// row — see `exec::eval_expr`). This is the operand type for `SELECT`
/// projections (`SelectExpr::Expr`); an aggregate *containing* one of these
/// (e.g. `count(*) + 1`) is rejected at parse time rather than represented
/// here.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A column or `file.*` pseudo-column.
    Col(ColRef),
    /// A literal constant.
    Lit(Literal),
    /// A scalar function applied to argument expressions.
    Scalar(ScalarFn, Vec<Expr>),
    /// A binary arithmetic or string-concat operation.
    Binary(BinOp, Box<Expr>, Box<Expr>),
}

/// A scalar string function, as it may appear in a `SELECT` expression.
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarFn {
    /// `lower(s)` — Unicode-aware lowercase.
    Lower,
    /// `upper(s)` — Unicode-aware uppercase.
    Upper,
    /// `length(s)` — character count.
    Length,
    /// `trim(s)` — whitespace trimmed from both ends.
    Trim,
    /// `ltrim(s)` — whitespace trimmed from the start.
    Ltrim,
    /// `rtrim(s)` — whitespace trimmed from the end.
    Rtrim,
    /// `substr(s, start[, len])` — 1-based, char-indexed, clamped.
    Substr,
    /// `replace(s, from, to)` — all non-overlapping occurrences.
    Replace,
}

/// A binary operator in a `SELECT` expression: arithmetic or string concat.
#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `||` — string concatenation.
    Concat,
}

/// An aggregate function applied to the rows of a group.
#[derive(Debug, Clone, PartialEq)]
pub enum Aggregate {
    /// `count(*)` — the number of rows in the group.
    CountStar,
    /// `count(col)` / `count(distinct col)` — non-null (optionally distinct)
    /// values of `col`.
    Count(ColRef, /* distinct */ bool),
    /// `min(col)`.
    Min(ColRef),
    /// `max(col)`.
    Max(ColRef),
    /// `sum(col)`.
    Sum(ColRef),
    /// `avg(col)`.
    Avg(ColRef),
    /// `group_concat(col)` — the group's values joined into one string.
    GroupConcat(ColRef),
}

/// A `WHERE` predicate tree.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    /// A comparison of two expressions, e.g. `status = 'draft'`,
    /// `start < end`, or `lower(status) = 'draft'`.
    Compare(Expr, CmpOp, Expr),
    /// `col [NOT] LIKE '<pattern>'`; the `bool` is `true` when negated.
    Like(ColRef, String, /* negated */ bool),
    /// `col [NOT] IN (<literals>)`; the `bool` is `true` when negated.
    In(ColRef, Vec<Literal>, /* negated */ bool),
    /// `<lit> MEMBER OF(col)` / `NOT <lit> MEMBER OF(col)`; the `bool` is
    /// `true` when negated. `col` must resolve to a `Value::List` — a `Null`
    /// or non-list value makes the predicate unknown, mirroring `In`'s
    /// null-column handling (see `exec::eval_predicate`).
    MemberOf(Literal, ColRef, /* negated */ bool),
    /// `col IS [NOT] NULL`; the `bool` is `true` for `IS NOT NULL`.
    IsNull(ColRef, /* negated */ bool),
    /// Logical conjunction.
    And(Box<Predicate>, Box<Predicate>),
    /// Logical disjunction.
    Or(Box<Predicate>, Box<Predicate>),
    /// Logical negation.
    Not(Box<Predicate>),
}

/// A `HAVING` predicate tree: group-level filtering, evaluated once per group
/// after its aggregates (and any grouping-key projection) are computed.
///
/// Structurally this mirrors [`Predicate`]'s boolean connectives, but its
/// only leaf is a comparison between a [`HavingLeaf`] and a plain [`Literal`]
/// (e.g. `count(*) > 1`) — `HAVING` in this subset never compares two
/// aggregates, nor an arbitrary expression; see [`HavingLeaf`].
#[derive(Debug, Clone, PartialEq)]
pub enum Having {
    /// A comparison of a group aggregate or grouping key against a literal.
    Compare(HavingLeaf, CmpOp, Literal),
    /// Logical conjunction.
    And(Box<Having>, Box<Having>),
    /// Logical disjunction.
    Or(Box<Having>, Box<Having>),
    /// Logical negation.
    Not(Box<Having>),
}

/// The left-hand side of a [`Having`] comparison: either one of the query's
/// `GROUP BY` keys (resolved from the group's key tuple) or an aggregate
/// function computed over the group's rows. The aggregate need not appear in
/// the `SELECT` list — standard SQL allows `HAVING` to reference an aggregate
/// that isn't projected.
#[derive(Debug, Clone, PartialEq)]
pub enum HavingLeaf {
    /// A `GROUP BY` key column.
    Group(ColRef),
    /// An aggregate over the group's rows.
    Agg(Aggregate),
}

/// A comparison operator.
#[derive(Debug, Clone, PartialEq)]
pub enum CmpOp {
    /// `=`
    Eq,
    /// `<>` / `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A literal value appearing on the right-hand side of a predicate.
#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    /// A string literal.
    Str(String),
    /// An integer literal.
    Int(i64),
    /// A floating-point literal.
    Float(f64),
    /// A boolean literal.
    Bool(bool),
    /// The `NULL` literal.
    Null,
}

/// One `ORDER BY` key: what to sort on and in which direction.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderKey {
    /// The value to sort by.
    pub target: OrderTarget,
    /// `true` for `DESC`, `false` for `ASC` (the default).
    pub desc: bool,
}

/// The sort key of an `ORDER BY` clause: a projection alias, a column, or a
/// bare aggregate call.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderTarget {
    /// An identifier that matched a `SELECT` alias.
    Alias(String),
    /// A direct column reference.
    Col(ColRef),
    /// A bare aggregate function call with no `AS` alias, e.g. `ORDER BY
    /// count(*) DESC`. Only valid alongside a non-empty `GROUP BY` — the
    /// parser rejects it on an ungrouped query (including the implicit
    /// single-group case, where `group_by` is empty too) rather than
    /// leaving that combination representable.
    Agg(Aggregate),
}

impl Query {
    /// Every `ColRef::Field`'s top-level path segment this query references
    /// (e.g. `estimate` for `estimate.low` — a sub-key is dynamic and is
    /// never checked against the schema, see spec §3.4), across every
    /// column position: `SELECT` (including a column nested inside a
    /// `Expr::Scalar`/`Expr::Binary` argument or an aggregate's argument),
    /// `WHERE`, `GROUP BY`, `ORDER BY` (a bare column or an aggregate
    /// target — an alias is not a field reference, since it names a
    /// `SELECT` item rather than a column), `HAVING`, and `MEMBER OF`'s
    /// column. A `file.*` pseudo-column is never included (its validity is
    /// checked at parse time, not against the schema), and neither is the
    /// `*` wildcard, which names no specific field.
    ///
    /// Used by [`crate::query::exec::execute`] to validate every referenced
    /// column exists in the schema before running the query. Also consumed
    /// by a later projection-push-down optimization, which needs the exact
    /// same "every column this query could possibly touch" set — keep this
    /// complete for every clause, not just what column validation exercises.
    pub fn referenced_fields(&self) -> BTreeSet<String> {
        let mut fields = BTreeSet::new();
        for item in &self.select {
            match &item.expr {
                SelectExpr::Star => {}
                SelectExpr::Expr(expr) => collect_expr_fields(expr, &mut fields),
                SelectExpr::Agg(agg) => collect_aggregate_fields(agg, &mut fields),
            }
        }
        if let Some(pred) = &self.filter {
            collect_predicate_fields(pred, &mut fields);
        }
        for col in &self.group_by {
            collect_col_field(col, &mut fields);
        }
        for key in &self.order_by {
            match &key.target {
                OrderTarget::Alias(_) => {}
                OrderTarget::Col(col) => collect_col_field(col, &mut fields),
                OrderTarget::Agg(agg) => collect_aggregate_fields(agg, &mut fields),
            }
        }
        if let Some(having) = &self.having {
            collect_having_fields(having, &mut fields);
        }
        fields
    }
}

/// Adds `col`'s top-level field-path segment to `fields` when it's a
/// frontmatter field; a `file.*` pseudo-column contributes nothing (see
/// [`Query::referenced_fields`]). Only the top-level segment is added — a
/// dotted path's sub-keys are dynamic and aren't checked against the schema
/// (spec §3.4). `path` is always non-empty for any `ColRef::Field` the
/// parser builds; the `first()` guard is a defensive no-op for a hand-built
/// empty path.
fn collect_col_field(col: &ColRef, fields: &mut BTreeSet<String>) {
    if let ColRef::Field(path) = col
        && let Some(top) = path.first()
    {
        fields.insert(top.clone());
    }
}

/// Walks `expr`'s column positions: a bare column, or every argument of a
/// nested scalar-function/arithmetic expression.
fn collect_expr_fields(expr: &Expr, fields: &mut BTreeSet<String>) {
    match expr {
        Expr::Col(col) => collect_col_field(col, fields),
        Expr::Lit(_) => {}
        Expr::Scalar(_, args) => {
            for arg in args {
                collect_expr_fields(arg, fields);
            }
        }
        Expr::Binary(_, l, r) => {
            collect_expr_fields(l, fields);
            collect_expr_fields(r, fields);
        }
    }
}

/// Adds the column an aggregate's argument references; `CountStar` takes no
/// column at all.
fn collect_aggregate_fields(agg: &Aggregate, fields: &mut BTreeSet<String>) {
    match agg {
        Aggregate::CountStar => {}
        Aggregate::Count(col, _)
        | Aggregate::Min(col)
        | Aggregate::Max(col)
        | Aggregate::Sum(col)
        | Aggregate::Avg(col)
        | Aggregate::GroupConcat(col) => collect_col_field(col, fields),
    }
}

/// Walks a `WHERE` predicate tree's leaves for column references.
fn collect_predicate_fields(pred: &Predicate, fields: &mut BTreeSet<String>) {
    match pred {
        Predicate::Compare(l, _, r) => {
            collect_expr_fields(l, fields);
            collect_expr_fields(r, fields);
        }
        Predicate::Like(col, _, _) | Predicate::In(col, _, _) | Predicate::IsNull(col, _) => {
            collect_col_field(col, fields);
        }
        Predicate::MemberOf(_, col, _) => collect_col_field(col, fields),
        Predicate::And(a, b) | Predicate::Or(a, b) => {
            collect_predicate_fields(a, fields);
            collect_predicate_fields(b, fields);
        }
        Predicate::Not(inner) => collect_predicate_fields(inner, fields),
    }
}

/// Walks a `HAVING` predicate tree's leaves for column/aggregate references.
fn collect_having_fields(having: &Having, fields: &mut BTreeSet<String>) {
    match having {
        Having::Compare(leaf, _, _) => match leaf {
            HavingLeaf::Group(col) => collect_col_field(col, fields),
            HavingLeaf::Agg(agg) => collect_aggregate_fields(agg, fields),
        },
        Having::And(a, b) | Having::Or(a, b) => {
            collect_having_fields(a, fields);
            collect_having_fields(b, fields);
        }
        Having::Not(inner) => collect_having_fields(inner, fields),
    }
}

impl SelectItem {
    /// The column header this item produces in the result table.
    ///
    /// The explicit alias wins when present; otherwise a default header is
    /// derived from the expression (the field name, the `file.*` label, `*`,
    /// or SQL-ish aggregate text such as `count(*)` / `min(prd)`).
    pub fn header(&self) -> String {
        match &self.alias {
            Some(alias) => alias.clone(),
            None => self.expr.default_header(),
        }
    }
}

impl SelectExpr {
    /// The header used when a projection item carries no explicit alias.
    ///
    /// A bare column keeps its plain field/`file.*` label; any other
    /// expression falls back to a rendered form (see [`expr_label`]).
    fn default_header(&self) -> String {
        match self {
            SelectExpr::Star => "*".to_string(),
            SelectExpr::Expr(Expr::Col(col)) => col.label(),
            SelectExpr::Expr(expr) => expr_label(expr),
            SelectExpr::Agg(agg) => agg.default_header(),
        }
    }
}

impl ColRef {
    /// The textual label for this column (`status`, `estimate.low`,
    /// `file.name`, …), used in default headers, aggregate rendering, and
    /// `HAVING` error messages.
    pub(crate) fn label(&self) -> String {
        match self {
            ColRef::Field(path) => path.join("."),
            ColRef::File(attr) => file_attr_label(*attr).to_string(),
        }
    }
}

impl Aggregate {
    /// SQL-ish default header text for this aggregate, e.g. `count(*)`,
    /// `count(distinct status)`, `min(prd)`, `group_concat(jira)`.
    fn default_header(&self) -> String {
        match self {
            Aggregate::CountStar => "count(*)".to_string(),
            Aggregate::Count(col, false) => format!("count({})", col.label()),
            Aggregate::Count(col, true) => format!("count(distinct {})", col.label()),
            Aggregate::Min(col) => format!("min({})", col.label()),
            Aggregate::Max(col) => format!("max({})", col.label()),
            Aggregate::Sum(col) => format!("sum({})", col.label()),
            Aggregate::Avg(col) => format!("avg({})", col.label()),
            Aggregate::GroupConcat(col) => format!("group_concat({})", col.label()),
        }
    }
}

/// A SQL-ish rendering of a computed [`Expr`], used as a projection's
/// default header when it has no alias (mirrors [`Aggregate::default_header`]).
/// A bare `Expr::Col` never reaches here — [`SelectExpr::default_header`]
/// handles it directly so it keeps its plain field/`file.*` label.
fn expr_label(expr: &Expr) -> String {
    match expr {
        Expr::Col(col) => col.label(),
        Expr::Lit(lit) => literal_label(lit),
        Expr::Scalar(f, args) => {
            let rendered: Vec<String> = args.iter().map(expr_label).collect();
            format!("{}({})", scalar_fn_name(f), rendered.join(", "))
        }
        Expr::Binary(op, l, r) => {
            format!("{} {} {}", expr_label(l), bin_op_symbol(op), expr_label(r))
        }
    }
}

/// A SQL-ish rendering of a literal constant, for [`expr_label`].
fn literal_label(lit: &Literal) -> String {
    match lit {
        Literal::Str(s) => format!("'{s}'"),
        Literal::Int(i) => i.to_string(),
        Literal::Float(f) => f.to_string(),
        Literal::Bool(b) => b.to_string(),
        Literal::Null => "NULL".to_string(),
    }
}

/// The SQL function name for a [`ScalarFn`], for [`expr_label`].
fn scalar_fn_name(f: &ScalarFn) -> &'static str {
    match f {
        ScalarFn::Lower => "lower",
        ScalarFn::Upper => "upper",
        ScalarFn::Length => "length",
        ScalarFn::Trim => "trim",
        ScalarFn::Ltrim => "ltrim",
        ScalarFn::Rtrim => "rtrim",
        ScalarFn::Substr => "substr",
        ScalarFn::Replace => "replace",
    }
}

/// The infix symbol for a [`BinOp`], for [`expr_label`].
fn bin_op_symbol(op: &BinOp) -> &'static str {
    match op {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
        BinOp::Concat => "||",
    }
}

/// The `file.<attr>` label for a [`FileAttr`].
fn file_attr_label(attr: FileAttr) -> &'static str {
    match attr {
        FileAttr::Name => "file.name",
        FileAttr::Path => "file.path",
        FileAttr::Folder => "file.folder",
        FileAttr::Ext => "file.ext",
    }
}

#[cfg(test)]
mod tests {
    use crate::query::parse::parse;

    #[test]
    fn referenced_fields_covers_all_positions() {
        let q = parse(
            "SELECT lower(a), count(b) WHERE c = 'x' GROUP BY a HAVING count(b) > 0 ORDER BY a",
        )
        .unwrap();
        let f = q.referenced_fields();
        for name in ["a", "b", "c"] {
            assert!(f.contains(name), "missing {name}");
        }
    }

    #[test]
    fn referenced_fields_excludes_star_and_file_attrs() {
        let q = parse("SELECT *, file.name, file.folder WHERE file.ext = 'md'").unwrap();
        assert!(
            q.referenced_fields().is_empty(),
            "`*` and `file.*` must never appear in referenced_fields"
        );
    }

    #[test]
    fn referenced_fields_includes_member_of_column() {
        let q = parse("SELECT file.name WHERE 'x' MEMBER OF(tags)").unwrap();
        assert_eq!(
            q.referenced_fields(),
            std::collections::BTreeSet::from(["tags".to_string()])
        );
    }

    #[test]
    fn referenced_fields_includes_order_by_bare_aggregate_column() {
        let q = parse("SELECT status GROUP BY status ORDER BY sum(n) DESC").unwrap();
        assert!(q.referenced_fields().contains("n"));
    }
}
