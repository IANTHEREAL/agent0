//! Typed Intermediate Representation (IR) types.
//!
//! Every expression node carries its resolved `DataType`. Column references are
//! positional indices, function calls carry resolved return types, and all
//! syntax sugar is normalized to canonical forms.

use crate::sql::types::CastContext;
use crate::types::{DataType, Value};
use std::fmt;

// ── Core IR node ────────────────────────────────────────────

/// A typed expression: the core IR node.
///
/// Every `TypedExpr` carries the `DataType` of its result, determined at
/// analysis time. The evaluator never needs runtime type inference.
#[derive(Debug, Clone)]
pub struct TypedExpr {
    pub kind: TypedExprKind,
    pub data_type: DataType,
}

impl TypedExpr {
    pub fn new(kind: TypedExprKind, data_type: DataType) -> Self {
        Self { kind, data_type }
    }

    /// Convenience: create a NULL constant with the given contextual type.
    pub fn null(data_type: DataType) -> Self {
        Self {
            kind: TypedExprKind::Constant(Value::Null),
            data_type,
        }
    }

    /// Returns true if this expression is a NULL constant.
    pub fn is_null_constant(&self) -> bool {
        matches!(self.kind, TypedExprKind::Constant(Value::Null))
    }

    /// Returns true if this expression is a constant (non-NULL).
    pub fn is_constant(&self) -> bool {
        matches!(self.kind, TypedExprKind::Constant(_))
    }
}

// ── Expression kinds (22 variants) ──────────────────────────

/// The expression kind — each variant represents a distinct semantic concept
/// requiring different evaluation logic.
///
/// All syntax sugar (SUBSTRING, TRIM, POSITION, EXTRACT, AT TIME ZONE, OVERLAY,
/// CEIL, FLOOR) is normalized to `FunctionCall` by the Analyzer.
#[derive(Debug, Clone)]
pub enum TypedExprKind {
    // ── Leaf nodes ──────────────────────────────────────
    /// A constant value. Typed literals (`DATE '2024-01-01'`) are parsed to
    /// their `Value` representation at analysis time.
    Constant(Value),

    /// A resolved column reference.
    ///
    /// - `scope_depth`: 0 = current scope, 1+ = outer scope (correlated subquery)
    /// - `column_index`: absolute position in the flattened row
    /// - `column_name`: retained for EXPLAIN / error messages only
    ColumnRef {
        scope_depth: u32,
        column_index: usize,
        column_name: String,
    },

    // ── Operators ───────────────────────────────────────
    /// Binary operation with resolved operand types.
    BinaryOp {
        left: Box<TypedExpr>,
        op: BinaryOp,
        right: Box<TypedExpr>,
    },

    /// Unary operation (NOT, negation, etc.).
    UnaryOp {
        op: UnaryOp,
        operand: Box<TypedExpr>,
    },

    /// Explicit or implicit cast. `cast_context` determines permissiveness.
    Cast {
        expr: Box<TypedExpr>,
        target_type: DataType,
        cast_context: CastContext,
    },

    // ── Comparison & Logic ──────────────────────────────
    /// IS NULL / IS NOT NULL / IS TRUE / IS FALSE / IS UNKNOWN.
    IsTest {
        expr: Box<TypedExpr>,
        test: IsTestKind,
        negated: bool,
    },

    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        expr: Box<TypedExpr>,
        low: Box<TypedExpr>,
        high: Box<TypedExpr>,
        negated: bool,
    },

    /// `expr [NOT] IN (list)`.
    InList {
        expr: Box<TypedExpr>,
        list: Vec<TypedExpr>,
        negated: bool,
    },

    /// `expr [NOT] [I]LIKE pattern [ESCAPE escape]`.
    Like {
        expr: Box<TypedExpr>,
        pattern: Box<TypedExpr>,
        escape: Option<Box<TypedExpr>>,
        case_insensitive: bool,
        negated: bool,
    },

    /// `expr [NOT] SIMILAR TO pattern [ESCAPE escape]`.
    SimilarTo {
        expr: Box<TypedExpr>,
        pattern: Box<TypedExpr>,
        escape: Option<Box<TypedExpr>>,
        negated: bool,
    },

    // ── Conditional ─────────────────────────────────────
    /// CASE expression.
    Case {
        operand: Option<Box<TypedExpr>>,
        when_clauses: Vec<(TypedExpr, TypedExpr)>,
        else_result: Option<Box<TypedExpr>>,
    },

    /// COALESCE(a, b, ...) — returns first non-NULL.
    Coalesce(Vec<TypedExpr>),

    /// NULLIF(a, b) — returns NULL if a = b, else a.
    NullIf(Box<TypedExpr>, Box<TypedExpr>),

    /// GREATEST / LEAST.
    MinMax {
        args: Vec<TypedExpr>,
        is_greatest: bool,
    },

    // ── Functions ───────────────────────────────────────
    // Syntax sugar (SUBSTRING, TRIM, POSITION, EXTRACT, AT TIME ZONE,
    // OVERLAY, CEIL, FLOOR) is normalized to FunctionCall by the Analyzer.
    /// Scalar function call.
    FunctionCall {
        func: ResolvedFunction,
        args: Vec<TypedExpr>,
        order_by: Vec<TypedOrderByExpr>,
        filter: Option<Box<TypedExpr>>,
    },

    /// Aggregate function call (COUNT, SUM, AVG, etc.).
    AggregateCall {
        func: ResolvedFunction,
        args: Vec<TypedExpr>,
        distinct: bool,
        order_by: Vec<TypedOrderByExpr>,
        filter: Option<Box<TypedExpr>>,
    },

    /// Window function call (ROW_NUMBER, RANK, SUM OVER, etc.).
    WindowCall {
        func: ResolvedFunction,
        args: Vec<TypedExpr>,
        partition_by: Vec<TypedExpr>,
        order_by: Vec<TypedOrderByExpr>,
        window_frame: Option<WindowFrame>,
    },

    // ── Subqueries ──────────────────────────────────────
    /// Scalar subquery: `(SELECT expr FROM ...)`.
    ScalarSubquery(Box<AnalyzedQuery>),

    /// `[NOT] EXISTS (SELECT ...)`.
    Exists {
        subquery: Box<AnalyzedQuery>,
        negated: bool,
    },

    /// `expr [NOT] IN (SELECT ...)`.
    InSubquery {
        expr: Box<TypedExpr>,
        subquery: Box<AnalyzedQuery>,
        negated: bool,
    },

    /// `expr op ANY/ALL (SELECT ...)`.
    AnyAll {
        expr: Box<TypedExpr>,
        op: BinaryOp,
        subquery: Box<AnalyzedQuery>,
        is_all: bool,
    },

    // ── Array & JSON ────────────────────────────────────
    /// Array literal: `ARRAY[a, b, c]`.
    ArrayLiteral(Vec<TypedExpr>),

    /// Array subscript: `arr[idx]`.
    ArrayIndex {
        array: Box<TypedExpr>,
        index: Box<TypedExpr>,
    },

    /// JSON access: `expr -> path`, `expr ->> path`, etc.
    JsonAccess {
        expr: Box<TypedExpr>,
        path: Box<TypedExpr>,
        operator: JsonAccessOp,
    },

    // ── Composite ───────────────────────────────────────
    /// Row constructor: `ROW(a, b, c)` or `(a, b, c)`.
    Row(Vec<TypedExpr>),
}

// ── Binary operators ────────────────────────────────────────

/// Binary operators — tipg's own enum, independent of sqlparser.
///
/// Complete set covering arithmetic, comparison, logical, string, bitwise,
/// regex, array, JSON, and full-text search operators.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryOp {
    // Arithmetic
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Exp,

    // Comparison
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,

    // Logical
    And,
    Or,

    // String
    Concat,

    // Bitwise
    BitwiseAnd,
    BitwiseOr,
    BitwiseXor,
    ShiftLeft,
    ShiftRight,

    // Regex (PostgreSQL)
    RegexMatch,
    RegexIMatch,
    RegexNotMatch,
    RegexNotIMatch,

    // Array
    ArrayOverlap,
    ArrayContains,
    ArrayContainedBy,

    // JSON containment & existence
    JsonContains,
    JsonContainedBy,
    JsonExists,
    JsonExistsAny,
    JsonExistsAll,

    // Full-text search
    TsMatch,

    // Escape hatch for PG operator extensions
    Custom(String),
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add => write!(f, "+"),
            Self::Sub => write!(f, "-"),
            Self::Mul => write!(f, "*"),
            Self::Div => write!(f, "/"),
            Self::Mod => write!(f, "%"),
            Self::Exp => write!(f, "^"),
            Self::Eq => write!(f, "="),
            Self::NotEq => write!(f, "<>"),
            Self::Lt => write!(f, "<"),
            Self::LtEq => write!(f, "<="),
            Self::Gt => write!(f, ">"),
            Self::GtEq => write!(f, ">="),
            Self::And => write!(f, "AND"),
            Self::Or => write!(f, "OR"),
            Self::Concat => write!(f, "||"),
            Self::BitwiseAnd => write!(f, "&"),
            Self::BitwiseOr => write!(f, "|"),
            Self::BitwiseXor => write!(f, "#"),
            Self::ShiftLeft => write!(f, "<<"),
            Self::ShiftRight => write!(f, ">>"),
            Self::RegexMatch => write!(f, "~"),
            Self::RegexIMatch => write!(f, "~*"),
            Self::RegexNotMatch => write!(f, "!~"),
            Self::RegexNotIMatch => write!(f, "!~*"),
            Self::ArrayOverlap => write!(f, "&&"),
            Self::ArrayContains | Self::JsonContains => write!(f, "@>"),
            Self::ArrayContainedBy | Self::JsonContainedBy => write!(f, "<@"),
            Self::JsonExists => write!(f, "?"),
            Self::JsonExistsAny => write!(f, "?|"),
            Self::JsonExistsAll => write!(f, "?&"),
            Self::TsMatch => write!(f, "@@"),
            Self::Custom(s) => write!(f, "{}", s),
        }
    }
}

// ── Unary operators ─────────────────────────────────────────

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// Boolean NOT.
    Not,
    /// Numeric negation (-).
    Minus,
    /// Numeric identity (+).
    Plus,
    /// Bitwise NOT (~).
    BitwiseNot,
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Not => write!(f, "NOT"),
            Self::Minus => write!(f, "-"),
            Self::Plus => write!(f, "+"),
            Self::BitwiseNot => write!(f, "~"),
        }
    }
}

// ── IS test kinds ───────────────────────────────────────────

/// IS test kinds: IS NULL, IS TRUE, IS FALSE, IS UNKNOWN.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsTestKind {
    Null,
    True,
    False,
    Unknown,
}

// ── JSON access operators ───────────────────────────────────

/// JSON access operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonAccessOp {
    /// `->` returns JSON.
    Arrow,
    /// `->>` returns text.
    LongArrow,
    /// `#>` returns JSON.
    HashArrow,
    /// `#>>` returns text.
    HashLongArrow,
}

// ── Resolved function ───────────────────────────────────────

/// Resolved function identity.
///
/// The Analyzer resolves overloads at analysis time and stores the return type
/// directly — no runtime registry lookup needed.
#[derive(Debug, Clone)]
pub struct ResolvedFunction {
    /// Canonical name (for EXPLAIN / error messages only — NOT used for dispatch).
    pub name: String,
    /// Whether builtin or user-defined.
    pub kind: FunctionKind,
    /// The resolved return type for this specific call (after overload resolution).
    pub return_type: DataType,
}

/// Function origin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FunctionKind {
    /// Built-in function from the global registry.
    Builtin,
    /// User-defined function.
    UserDefined { oid: u32 },
}

// ── ORDER BY ────────────────────────────────────────────────

/// ORDER BY expression with resolved sort key.
#[derive(Debug, Clone)]
pub struct TypedOrderByExpr {
    pub expr: TypedExpr,
    pub asc: bool,
    pub nulls_first: bool,
}

// ── Window frame ────────────────────────────────────────────

/// Window frame specification.
#[derive(Debug, Clone)]
pub struct WindowFrame {
    pub units: WindowFrameUnits,
    pub start: WindowFrameBound,
    pub end: Option<WindowFrameBound>,
}

/// Window frame unit types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFrameUnits {
    Rows,
    Range,
    Groups,
}

/// Window frame bound specification.
#[derive(Debug, Clone)]
pub enum WindowFrameBound {
    CurrentRow,
    /// UNBOUNDED or N PRECEDING (`None` = UNBOUNDED).
    Preceding(Option<Box<TypedExpr>>),
    /// UNBOUNDED or N FOLLOWING (`None` = UNBOUNDED).
    Following(Option<Box<TypedExpr>>),
}

// ── Analyzed query ──────────────────────────────────────────

/// A fully analyzed SQL query.
///
/// Wraps the query body (SELECT, set operation) with query-level clauses
/// (CTEs, ORDER BY, LIMIT, OFFSET) and the resolved output schema.
///
/// This mirrors PostgreSQL's Query node: the body produces rows, and the
/// outer wrapper applies ordering, pagination, and CTE visibility.
#[derive(Debug, Clone)]
pub struct AnalyzedQuery {
    /// CTEs defined in this query's WITH clause.
    pub ctes: Vec<AnalyzedCte>,
    /// The query body (SELECT or set operation).
    pub body: AnalyzedQueryBody,
    /// ORDER BY expressions (query-level, applied after the body).
    pub order_by: Vec<TypedOrderByExpr>,
    /// LIMIT expression.
    pub limit: Option<TypedExpr>,
    /// OFFSET expression.
    pub offset: Option<TypedExpr>,
    /// Output schema: (column_name, data_type) for each output column.
    pub output_schema: Vec<(String, DataType)>,
}

/// The body of an analyzed query.
///
/// Each variant produces rows with a schema matching the parent
/// `AnalyzedQuery::output_schema`.
#[derive(Debug, Clone)]
pub enum AnalyzedQueryBody {
    /// A SELECT statement.
    Select(AnalyzedSelect),
    /// A set operation (UNION / INTERSECT / EXCEPT).
    SetOperation {
        op: SetOpKind,
        all: bool,
        left: Box<AnalyzedQuery>,
        right: Box<AnalyzedQuery>,
    },
}

/// A fully analyzed SELECT clause.
///
/// Contains all SELECT-specific clauses: projection, FROM, WHERE,
/// GROUP BY, HAVING, and DISTINCT mode.
#[derive(Debug, Clone)]
pub struct AnalyzedSelect {
    /// Output columns (SELECT list).
    pub projection: Vec<AnalyzedProjection>,
    /// FROM clause table references.
    pub from: Vec<AnalyzedTableRef>,
    /// WHERE predicate (type-checked to boolean).
    pub where_clause: Option<TypedExpr>,
    /// GROUP BY expressions.
    pub group_by: Vec<TypedExpr>,
    /// HAVING predicate (type-checked to boolean).
    pub having: Option<TypedExpr>,
    /// DISTINCT mode.
    pub distinct: AnalyzedDistinct,
}

/// DISTINCT mode for a SELECT.
#[derive(Debug, Clone)]
pub enum AnalyzedDistinct {
    /// No DISTINCT — return all rows.
    All,
    /// DISTINCT — deduplicate on all output columns.
    Distinct,
    /// DISTINCT ON (expressions) — deduplicate on specified expressions.
    DistinctOn(Vec<TypedExpr>),
}

/// A resolved SELECT-list item (output column).
#[derive(Debug, Clone)]
pub struct AnalyzedProjection {
    /// The analyzed expression.
    pub expr: TypedExpr,
    /// The output column name (user alias or auto-inferred).
    pub output_name: String,
}

// ── Table references ────────────────────────────────────────

/// An analyzed table reference in the FROM clause.
#[derive(Debug, Clone)]
pub struct AnalyzedTableRef {
    pub kind: AnalyzedTableRefKind,
    pub alias: Option<String>,
}

/// Kinds of table references.
#[derive(Debug, Clone)]
pub enum AnalyzedTableRefKind {
    /// A base table with resolved schema.
    Table {
        name: String,
        schema: TableRefSchema,
    },
    /// A subquery in FROM.
    Subquery(Box<AnalyzedQuery>),
    /// A JOIN.
    Join {
        left: Box<AnalyzedTableRef>,
        right: Box<AnalyzedTableRef>,
        join_type: JoinType,
        condition: JoinCondition,
    },
    /// A table-valued function (e.g. unnest, generate_series).
    Function {
        func: ResolvedFunction,
        args: Vec<TypedExpr>,
        output_columns: Vec<(String, DataType)>,
    },
}

/// Resolved schema information for a base table reference.
#[derive(Debug, Clone)]
pub struct TableRefSchema {
    pub table_id: u64,
    /// (column_name, data_type, nullable)
    pub columns: Vec<(String, DataType, bool)>,
}

// ── JOIN types ──────────────────────────────────────────────

/// JOIN types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

/// JOIN condition (resolved).
#[derive(Debug, Clone)]
pub enum JoinCondition {
    /// ON expression (type-checked to boolean).
    On(TypedExpr),
    /// USING with resolved column indices.
    Using(Vec<ResolvedUsingColumn>),
    /// No condition (CROSS JOIN).
    None,
}

/// A resolved USING column with positional indices into left/right tables.
#[derive(Debug, Clone)]
pub struct ResolvedUsingColumn {
    /// Column name (for EXPLAIN / error messages).
    pub name: String,
    /// Column index in left table's row.
    pub left_index: usize,
    /// Column index in right table's row.
    pub right_index: usize,
    /// Unified data type (after coercion if needed).
    pub data_type: DataType,
}

// ── CTE ─────────────────────────────────────────────────────

/// A resolved CTE (WITH clause entry).
#[derive(Debug, Clone)]
pub struct AnalyzedCte {
    pub name: String,
    pub query: AnalyzedQuery,
    pub columns: Vec<(String, DataType)>,
    /// Whether the CTE is materialized (`None` = unspecified / optimizer decides).
    pub materialized: Option<bool>,
}

// ── Set operations ──────────────────────────────────────────

/// Set operation kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}
