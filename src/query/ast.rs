//! The query AST for the SQL subset understood by querymatter.
//!
//! These types are the contract between the parser (`query::parse`) and the
//! executor (`query::exec`): the parser lowers a `sqlparser` parse tree into a
//! [`Query`], and the executor evaluates a [`Query`] against the record store.
//! The shapes here deliberately model only the supported subset, so an
//! ill-formed or unsupported query is rejected at parse time rather than being
//! representable in the AST.

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
    /// The glob or directory the query scans, if a `FROM` clause was given.
    pub from_glob: Option<String>,
    /// The `WHERE` predicate, if any.
    pub filter: Option<Predicate>,
    /// `GROUP BY` keys, in order; empty when the query is not grouped.
    pub group_by: Vec<ColRef>,
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

/// A reference to a queryable column: either a frontmatter field or a `file.*`
/// pseudo-column.
#[derive(Debug, Clone, PartialEq)]
pub enum ColRef {
    /// A YAML frontmatter field, named verbatim.
    Field(String),
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
    /// `lower(s)` — ASCII-aware lowercase.
    Lower,
    /// `upper(s)` — ASCII-aware uppercase.
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

/// The sort key of an `ORDER BY` clause: either a projection alias or a column.
#[derive(Debug, Clone, PartialEq)]
pub enum OrderTarget {
    /// An identifier that matched a `SELECT` alias.
    Alias(String),
    /// A direct column reference.
    Col(ColRef),
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
    /// The textual label for this column (`status`, `file.name`, …), used in
    /// default headers and aggregate rendering.
    fn label(&self) -> String {
        match self {
            ColRef::Field(name) => name.clone(),
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
