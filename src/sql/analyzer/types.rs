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
}

impl fmt::Display for TypedExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            TypedExprKind::Constant(v) => write!(f, "{}", v),
            TypedExprKind::ColumnRef { column_name, .. } => write!(f, "{}", column_name),
            TypedExprKind::BinaryOp { left, op, right } => {
                write!(f, "({} {} {})", left, op, right)
            }
            TypedExprKind::UnaryOp { op, operand } => write!(f, "({}{})", op, operand),
            TypedExprKind::Cast {
                expr, target_type, ..
            } => {
                write!(f, "CAST({} AS {:?})", expr, target_type)
            }
            TypedExprKind::IsTest {
                expr,
                test,
                negated,
            } => {
                write!(
                    f,
                    "({} IS {}{})",
                    expr,
                    if *negated { "NOT " } else { "" },
                    test
                )
            }
            TypedExprKind::Between {
                expr,
                low,
                high,
                negated,
            } => {
                write!(
                    f,
                    "({} {}BETWEEN {} AND {})",
                    expr,
                    if *negated { "NOT " } else { "" },
                    low,
                    high
                )
            }
            TypedExprKind::InList {
                expr,
                list,
                negated,
            } => {
                let items: Vec<String> = list.iter().map(|e| format!("{}", e)).collect();
                write!(
                    f,
                    "({} {}IN ({}))",
                    expr,
                    if *negated { "NOT " } else { "" },
                    items.join(", ")
                )
            }
            TypedExprKind::Like {
                expr,
                pattern,
                negated,
                case_insensitive,
                ..
            } => {
                let kw = if *case_insensitive { "ILIKE" } else { "LIKE" };
                write!(
                    f,
                    "({} {}{}  {})",
                    expr,
                    if *negated { "NOT " } else { "" },
                    kw,
                    pattern
                )
            }
            TypedExprKind::SimilarTo {
                expr,
                pattern,
                negated,
                ..
            } => {
                write!(
                    f,
                    "({} {}SIMILAR TO {})",
                    expr,
                    if *negated { "NOT " } else { "" },
                    pattern
                )
            }
            TypedExprKind::Case {
                operand,
                when_clauses,
                else_result,
            } => {
                write!(f, "CASE")?;
                if let Some(op) = operand {
                    write!(f, " {}", op)?;
                }
                for (when, then) in when_clauses {
                    write!(f, " WHEN {} THEN {}", when, then)?;
                }
                if let Some(el) = else_result {
                    write!(f, " ELSE {}", el)?;
                }
                write!(f, " END")
            }
            TypedExprKind::Coalesce(args) => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                write!(f, "COALESCE({})", items.join(", "))
            }
            TypedExprKind::NullIf(a, b) => write!(f, "NULLIF({}, {})", a, b),
            TypedExprKind::MinMax { args, is_greatest } => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                let name = if *is_greatest { "GREATEST" } else { "LEAST" };
                write!(f, "{}({})", name, items.join(", "))
            }
            TypedExprKind::FunctionCall { func, args, .. } => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                write!(f, "{}({})", func.name, items.join(", "))
            }
            TypedExprKind::AggregateCall {
                func,
                args,
                distinct,
                ..
            } => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                write!(
                    f,
                    "{}({}{})",
                    func.name,
                    if *distinct { "DISTINCT " } else { "" },
                    if items.is_empty() {
                        "*".to_string()
                    } else {
                        items.join(", ")
                    }
                )
            }
            TypedExprKind::WindowCall { func, args, .. } => {
                let items: Vec<String> = args.iter().map(|e| format!("{}", e)).collect();
                write!(
                    f,
                    "{}({}) OVER (..)",
                    func.name,
                    if items.is_empty() {
                        "*".to_string()
                    } else {
                        items.join(", ")
                    }
                )
            }
            TypedExprKind::ScalarSubquery(_) => write!(f, "(subquery)"),
            TypedExprKind::ArraySubquery(_) => write!(f, "ARRAY(subquery)"),
            TypedExprKind::Exists { negated, .. } => {
                write!(f, "{}EXISTS (subquery)", if *negated { "NOT " } else { "" })
            }
            TypedExprKind::InSubquery { expr, negated, .. } => {
                write!(
                    f,
                    "({} {}IN (subquery))",
                    expr,
                    if *negated { "NOT " } else { "" }
                )
            }
            TypedExprKind::AnyAll {
                expr, op, is_all, ..
            } => {
                let kw = if *is_all { "ALL" } else { "ANY" };
                write!(f, "({} {} {} (subquery))", expr, op, kw)
            }
            TypedExprKind::ArrayLiteral(elems) => {
                let items: Vec<String> = elems.iter().map(|e| format!("{}", e)).collect();
                write!(f, "ARRAY[{}]", items.join(", "))
            }
            TypedExprKind::ArrayIndex { array, index } => {
                write!(f, "{}[{}]", array, index)
            }
            TypedExprKind::JsonAccess {
                expr,
                path,
                operator,
            } => {
                let op_str = match operator {
                    JsonAccessOp::Arrow => "->",
                    JsonAccessOp::LongArrow => "->>",
                    JsonAccessOp::HashArrow => "#>",
                    JsonAccessOp::HashLongArrow => "#>>",
                    JsonAccessOp::HashMinus => "#-",
                };
                write!(f, "({} {} {})", expr, op_str, path)
            }
            TypedExprKind::Row(elems) => {
                let items: Vec<String> = elems.iter().map(|e| format!("{}", e)).collect();
                write!(f, "ROW({})", items.join(", "))
            }
            TypedExprKind::Default => write!(f, "DEFAULT"),
        }
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
    /// `ARRAY(SELECT ...)` — array constructor from subquery.
    /// Each result row's first column becomes an array element.
    ArraySubquery(Box<AnalyzedQuery>),

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
    #[allow(dead_code)]
    Row(Vec<TypedExpr>),

    // ── DML placeholder ────────────────────────────────
    /// DEFAULT keyword in INSERT VALUES — placeholder for executor to fill
    /// with the column's default value or serial sequence.
    Default,
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
    #[allow(dead_code)]
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

impl fmt::Display for IsTestKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => write!(f, "NULL"),
            Self::True => write!(f, "TRUE"),
            Self::False => write!(f, "FALSE"),
            Self::Unknown => write!(f, "UNKNOWN"),
        }
    }
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
    /// `#-` deletes path and returns JSON.
    HashMinus,
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
    #[allow(dead_code)]
    pub kind: FunctionKind,
    /// The resolved return type for this specific call (after overload resolution).
    #[allow(dead_code)]
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

/// A typed argument to a table-valued function (FROM ... func(...)).
#[derive(Debug, Clone)]
pub enum TypedFunctionArg {
    /// Positional argument: `func(expr)`.
    Positional(TypedExpr),
    /// Named argument: `func(param => expr)`.
    Named { name: String, expr: TypedExpr },
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
/// Each variant produces rows with a schema matching
/// `AnalyzedQuery::output_schema`.
#[derive(Debug, Clone)]
pub enum AnalyzedQueryBody {
    /// A SELECT statement.
    Select(AnalyzedSelect),
    /// A VALUES clause (`VALUES (..), (..)`).
    ///
    /// Each inner Vec is one row of typed expressions.
    Values(Vec<Vec<TypedExpr>>),
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
        #[allow(dead_code)]
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
        /// Global column offset where this join's left child starts.
        /// Used by the executor to reindex ON conditions from global to local
        /// indices when the join is nested inside a larger join tree.
        left_col_start: usize,
    },
    /// A table-valued function (e.g. unnest, generate_series).
    Function {
        func: ResolvedFunction,
        args: Vec<TypedFunctionArg>,
        output_columns: Vec<(String, DataType)>,
    },
}

/// Resolved schema information for a base table reference.
#[derive(Debug, Clone)]
pub struct TableRefSchema {
    #[allow(dead_code)]
    pub table_id: u64,
    /// (column_name, data_type, nullable)
    #[allow(dead_code)]
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
    /// Original left column type (before coercion).
    pub left_type: DataType,
    /// Original right column type (before coercion).
    pub right_type: DataType,
}

// ── JOIN condition reindexing ────────────────────────────────
//
// Shared by executor (`executor/select/analyzed/joins.rs`) and optimizer
// (`optimizer/logical_planner.rs`). Placed here because `JoinCondition` is
// defined in this module — no dependency inversion.

/// Reindex a JoinCondition from global column indices to local indices.
///
/// When the analyzer builds `A JOIN (B JOIN C ON ...)`, the ON condition for
/// the inner join has column indices that are global (relative to the full
/// FROM clause). Both executor and optimizer call this to normalize to local
/// indices (starting from 0 for the join's left child).
pub fn reindex_join_condition(condition: &JoinCondition, offset: usize) -> JoinCondition {
    match condition {
        JoinCondition::On(expr) => JoinCondition::On(reindex_typed_expr(expr, offset)),
        // USING already uses local indices (computed at analysis time).
        other => other.clone(),
    }
}

/// Recursively clone a TypedExpr, subtracting `offset` from all
/// `ColumnRef.column_index` where `scope_depth == 0` (current-scope refs only).
pub fn reindex_typed_expr(expr: &TypedExpr, offset: usize) -> TypedExpr {
    let kind = match &expr.kind {
        TypedExprKind::ColumnRef {
            scope_depth,
            column_index,
            column_name,
        } => {
            if *scope_depth == 0 {
                TypedExprKind::ColumnRef {
                    scope_depth: 0,
                    column_index: column_index.saturating_sub(offset),
                    column_name: column_name.clone(),
                }
            } else {
                expr.kind.clone()
            }
        }
        TypedExprKind::BinaryOp { left, right, op } => TypedExprKind::BinaryOp {
            left: Box::new(reindex_typed_expr(left, offset)),
            op: op.clone(),
            right: Box::new(reindex_typed_expr(right, offset)),
        },
        TypedExprKind::UnaryOp { operand, op } => TypedExprKind::UnaryOp {
            operand: Box::new(reindex_typed_expr(operand, offset)),
            op: *op,
        },
        TypedExprKind::Cast {
            expr: inner,
            target_type,
            cast_context,
        } => TypedExprKind::Cast {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            target_type: target_type.clone(),
            cast_context: *cast_context,
        },
        TypedExprKind::IsTest {
            expr: inner,
            test,
            negated,
        } => TypedExprKind::IsTest {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            test: *test,
            negated: *negated,
        },
        TypedExprKind::Between {
            expr: inner,
            low,
            high,
            negated,
        } => TypedExprKind::Between {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            low: Box::new(reindex_typed_expr(low, offset)),
            high: Box::new(reindex_typed_expr(high, offset)),
            negated: *negated,
        },
        TypedExprKind::InList {
            expr: inner,
            list,
            negated,
        } => TypedExprKind::InList {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            list: list.iter().map(|e| reindex_typed_expr(e, offset)).collect(),
            negated: *negated,
        },
        TypedExprKind::Like {
            expr: inner,
            pattern,
            escape,
            case_insensitive,
            negated,
        } => TypedExprKind::Like {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            pattern: Box::new(reindex_typed_expr(pattern, offset)),
            escape: escape
                .as_ref()
                .map(|e| Box::new(reindex_typed_expr(e, offset))),
            case_insensitive: *case_insensitive,
            negated: *negated,
        },
        TypedExprKind::SimilarTo {
            expr: inner,
            pattern,
            escape,
            negated,
        } => TypedExprKind::SimilarTo {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            pattern: Box::new(reindex_typed_expr(pattern, offset)),
            escape: escape
                .as_ref()
                .map(|e| Box::new(reindex_typed_expr(e, offset))),
            negated: *negated,
        },
        TypedExprKind::Case {
            operand,
            when_clauses,
            else_result,
        } => TypedExprKind::Case {
            operand: operand
                .as_ref()
                .map(|e| Box::new(reindex_typed_expr(e, offset))),
            when_clauses: when_clauses
                .iter()
                .map(|(w, t)| (reindex_typed_expr(w, offset), reindex_typed_expr(t, offset)))
                .collect(),
            else_result: else_result
                .as_ref()
                .map(|e| Box::new(reindex_typed_expr(e, offset))),
        },
        TypedExprKind::Coalesce(args) => {
            TypedExprKind::Coalesce(args.iter().map(|a| reindex_typed_expr(a, offset)).collect())
        }
        TypedExprKind::NullIf(a, b) => TypedExprKind::NullIf(
            Box::new(reindex_typed_expr(a, offset)),
            Box::new(reindex_typed_expr(b, offset)),
        ),
        TypedExprKind::MinMax { args, is_greatest } => TypedExprKind::MinMax {
            args: args.iter().map(|a| reindex_typed_expr(a, offset)).collect(),
            is_greatest: *is_greatest,
        },
        TypedExprKind::FunctionCall {
            func,
            args,
            order_by,
            filter,
        } => TypedExprKind::FunctionCall {
            func: func.clone(),
            args: args.iter().map(|a| reindex_typed_expr(a, offset)).collect(),
            order_by: order_by.clone(),
            filter: filter
                .as_ref()
                .map(|f| Box::new(reindex_typed_expr(f, offset))),
        },
        TypedExprKind::AggregateCall {
            func,
            args,
            distinct,
            order_by,
            filter,
        } => TypedExprKind::AggregateCall {
            func: func.clone(),
            args: args.iter().map(|a| reindex_typed_expr(a, offset)).collect(),
            distinct: *distinct,
            order_by: order_by.clone(),
            filter: filter
                .as_ref()
                .map(|f| Box::new(reindex_typed_expr(f, offset))),
        },
        TypedExprKind::ArrayIndex { array, index } => TypedExprKind::ArrayIndex {
            array: Box::new(reindex_typed_expr(array, offset)),
            index: Box::new(reindex_typed_expr(index, offset)),
        },
        TypedExprKind::JsonAccess {
            expr: inner,
            path,
            operator,
        } => TypedExprKind::JsonAccess {
            expr: Box::new(reindex_typed_expr(inner, offset)),
            path: Box::new(reindex_typed_expr(path, offset)),
            operator: *operator,
        },
        // Leaf/opaque nodes: subqueries, constants, etc. — no reindexing needed.
        _ => expr.kind.clone(),
    };
    TypedExpr {
        kind,
        data_type: expr.data_type.clone(),
    }
}

// ── CTE ─────────────────────────────────────────────────────

/// A resolved CTE (WITH clause entry).
#[derive(Debug, Clone)]
pub struct AnalyzedCte {
    #[allow(dead_code)]
    pub name: String,
    #[allow(dead_code)]
    pub query: AnalyzedQuery,
    #[allow(dead_code)]
    pub columns: Vec<(String, DataType)>,
    /// Whether the CTE is materialized (`None` = unspecified / optimizer decides).
    #[allow(dead_code)]
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

// ── DML statements ──────────────────────────────────────────

/// A fully analyzed SQL statement (DML or query).
///
/// This is the top-level IR node produced by `Analyzer::analyze_statement()`.
/// Queries are analyzed via `analyze_query()` and produce `AnalyzedQuery` directly;
/// this enum adds DML variants that share the same typed-expression infrastructure.
#[derive(Debug, Clone)]
pub enum AnalyzedStatement {
    /// A SELECT / set operation.
    #[allow(dead_code)] // Dispatched via pattern match; inner value used transitionally
    Query(AnalyzedQuery),
    /// An INSERT statement.
    Insert(AnalyzedInsert),
    /// An UPDATE statement.
    Update(AnalyzedUpdate),
    /// A DELETE statement.
    Delete(AnalyzedDelete),
}

/// A fully analyzed INSERT statement.
#[derive(Debug, Clone)]
pub struct AnalyzedInsert {
    /// Fully qualified table name (for storage ops).
    pub table_name: String,
    /// Resolved table schema (for IR completeness; executor re-fetches from store).
    #[allow(dead_code)]
    pub table_schema: TableRefSchema,
    /// Column indices being inserted (maps to positions in `table_schema.columns`).
    pub target_columns: Vec<usize>,
    /// Row source.
    pub source: AnalyzedInsertSource,
    /// ON CONFLICT handling.
    pub on_conflict: Option<AnalyzedOnConflict>,
    /// RETURNING clause projections.
    pub returning: Option<Vec<AnalyzedProjection>>,
}

/// Source of rows for an INSERT.
#[derive(Debug, Clone)]
pub enum AnalyzedInsertSource {
    /// `VALUES (expr, ...), (expr, ...)` — each inner Vec is one row.
    Values(Vec<Vec<TypedExpr>>),
    /// `INSERT ... SELECT ...`
    Query(Box<AnalyzedQuery>),
    /// `INSERT ... DEFAULT VALUES`
    DefaultValues,
}

/// Analyzed ON CONFLICT clause.
#[derive(Debug, Clone)]
pub enum AnalyzedOnConflict {
    /// DO NOTHING — skip conflicting rows.
    DoNothing,
    /// DO UPDATE SET — update conflicting rows.
    DoUpdate {
        /// Assignments: (column_index, typed value expression).
        /// Expressions may reference the "excluded" pseudo-table.
        assignments: Vec<(usize, TypedExpr)>,
        /// Optional WHERE clause on the DO UPDATE.
        where_clause: Option<TypedExpr>,
    },
}

/// A fully analyzed UPDATE statement.
#[derive(Debug, Clone)]
pub struct AnalyzedUpdate {
    /// Fully qualified table name.
    pub table_name: String,
    /// Resolved table schema (for IR completeness; executor re-fetches from store).
    #[allow(dead_code)]
    pub table_schema: TableRefSchema,
    /// Table alias (or bare table name).
    #[allow(dead_code)]
    pub table_alias: String,
    /// SET assignments: (column_index, typed value expression).
    pub assignments: Vec<(usize, TypedExpr)>,
    /// FROM clause tables (for UPDATE ... FROM ... WHERE ...).
    pub from: Vec<AnalyzedTableRef>,
    /// WHERE predicate (type-checked to boolean).
    pub where_clause: Option<TypedExpr>,
    /// RETURNING clause projections.
    pub returning: Option<Vec<AnalyzedProjection>>,
}

/// A fully analyzed DELETE statement.
#[derive(Debug, Clone)]
pub struct AnalyzedDelete {
    /// Fully qualified table name.
    pub table_name: String,
    /// Resolved table schema (for IR completeness; executor re-fetches from store).
    #[allow(dead_code)]
    pub table_schema: TableRefSchema,
    /// Table alias (or bare table name).
    #[allow(dead_code)]
    pub table_alias: String,
    /// USING clause tables (for DELETE ... USING ... WHERE ...).
    pub using: Vec<AnalyzedTableRef>,
    /// WHERE predicate (type-checked to boolean).
    pub where_clause: Option<TypedExpr>,
    /// RETURNING clause projections.
    pub returning: Option<Vec<AnalyzedProjection>>,
}
