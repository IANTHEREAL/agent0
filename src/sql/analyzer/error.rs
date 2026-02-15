//! Analyzer error types.
//!
//! All errors follow PostgreSQL error message conventions where applicable.

use crate::types::DataType;
use std::fmt;

/// Errors produced during semantic analysis.
///
/// The Analyzer emits these when it encounters an invalid query during the
/// single-pass name-resolution + type-checking walk.
#[derive(Debug, Clone)]
pub enum AnalyzerError {
    /// Column not found in any visible scope.
    ColumnNotFound {
        name: String,
        available: Vec<String>,
    },

    /// Column reference is ambiguous (present in multiple tables).
    AmbiguousColumn { name: String, tables: Vec<String> },

    /// Table/relation not found in catalog.
    TableNotFound(String),

    /// Function not found in registry or catalog.
    FunctionNotFound {
        name: String,
        arg_types: Vec<DataType>,
    },

    /// Wrong number of arguments to function.
    ArgumentCountMismatch {
        function: String,
        expected_min: usize,
        expected_max: Option<usize>,
        got: usize,
    },

    /// Operator type mismatch (no operator exists for the given operand types).
    OperatorTypeMismatch {
        operator: String,
        left: DataType,
        right: DataType,
    },

    /// Type mismatch in context (e.g. WHERE clause is not boolean).
    TypeMismatch {
        expected: DataType,
        found: DataType,
        context: String,
    },

    /// Branches/arguments have incompatible types (CASE, COALESCE, ARRAY, etc.).
    TypesCannotBeMatched {
        types: Vec<DataType>,
        context: String,
    },

    /// Scalar subquery must return exactly one column.
    ScalarSubqueryMultipleColumns { got: usize },

    /// Invalid typed literal (e.g. `DATE 'not-a-date'`).
    InvalidLiteral {
        value: String,
        target_type: DataType,
        parse_error: String,
    },

    /// Aggregate function used in wrong context (e.g. WHERE clause).
    AggregateNotAllowed { function: String, context: String },

    /// Non-aggregated column appears in aggregate query without GROUP BY coverage.
    UngroupedColumn { name: String },

    /// Window function used in wrong context.
    WindowNotAllowed { function: String, context: String },

    /// Set operation (UNION/INTERSECT/EXCEPT) column count mismatch.
    SetOperationColumnMismatch { left: usize, right: usize },

    /// DML: column not found in target table.
    DmlColumnNotFound { column: String, table: String },

    /// DML: INSERT column count mismatch (columns vs values).
    InsertColumnCountMismatch { columns: usize, values: usize },

    /// DML: assignment type mismatch (SET col = expr).
    AssignmentTypeMismatch {
        column: String,
        expected: DataType,
        found: DataType,
    },

    /// DML: WHERE clause in DML is not boolean.
    DmlWhereNotBoolean { found: DataType },

    /// Unsupported SQL feature.
    Unsupported(String),

    /// Internal analyzer error (should not happen; indicates a bug).
    Internal(String),
}

impl fmt::Display for AnalyzerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ColumnNotFound { name, available } => {
                write!(f, "column \"{}\" does not exist", name)?;
                if !available.is_empty() {
                    let hint: Vec<_> = available.iter().take(5).map(String::as_str).collect();
                    write!(f, "\nHINT: Perhaps you meant: {}", hint.join(", "))?;
                }
                Ok(())
            }
            Self::AmbiguousColumn { name, tables } => {
                write!(f, "column reference \"{}\" is ambiguous", name)?;
                if !tables.is_empty() {
                    write!(f, " (candidates: {})", tables.join(", "))?;
                }
                Ok(())
            }
            Self::TableNotFound(name) => write!(f, "relation \"{}\" does not exist", name),
            Self::FunctionNotFound { name, arg_types } => {
                let types: Vec<_> = arg_types.iter().map(|t| t.to_string()).collect();
                write!(f, "function {}({}) does not exist", name, types.join(", "))
            }
            Self::ArgumentCountMismatch {
                function,
                expected_min,
                expected_max,
                got,
            } => match expected_max {
                Some(max) if *max == *expected_min => write!(
                    f,
                    "function {} requires {} argument{}, got {}",
                    function,
                    expected_min,
                    if *expected_min == 1 { "" } else { "s" },
                    got,
                ),
                Some(max) => write!(
                    f,
                    "function {} requires {}-{} arguments, got {}",
                    function, expected_min, max, got,
                ),
                None => write!(
                    f,
                    "function {} requires at least {} argument{}, got {}",
                    function,
                    expected_min,
                    if *expected_min == 1 { "" } else { "s" },
                    got,
                ),
            },
            Self::OperatorTypeMismatch {
                operator,
                left,
                right,
            } => write!(
                f,
                "operator does not exist: {} {} {}",
                left, operator, right
            ),
            Self::TypeMismatch {
                expected,
                found,
                context,
            } => write!(f, "expected {}, found {} in {}", expected, found, context),
            Self::TypesCannotBeMatched { types, context } => {
                let types_str: Vec<_> = types.iter().map(|t| t.to_string()).collect();
                write!(
                    f,
                    "{} types {} cannot be matched",
                    context,
                    types_str.join(" and ")
                )
            }
            Self::ScalarSubqueryMultipleColumns { got } => {
                write!(f, "subquery must return only one column, got {}", got)
            }
            Self::InvalidLiteral {
                value,
                target_type,
                parse_error,
            } => write!(
                f,
                "invalid input syntax for type {}: \"{}\": {}",
                target_type, value, parse_error,
            ),
            Self::AggregateNotAllowed { function, context } => {
                write!(
                    f,
                    "aggregate function {} is not allowed in {}",
                    function, context,
                )
            }
            Self::UngroupedColumn { name } => write!(
                f,
                "column \"{}\" must appear in the GROUP BY clause or be used in an aggregate function",
                name
            ),
            Self::WindowNotAllowed { function, context } => {
                write!(
                    f,
                    "window function {} is not allowed in {}",
                    function, context,
                )
            }
            Self::SetOperationColumnMismatch { left, right } => write!(
                f,
                "each UNION/INTERSECT/EXCEPT query must have the same number of columns ({} vs {})",
                left, right,
            ),
            Self::DmlColumnNotFound { column, table } => {
                write!(
                    f,
                    "column \"{}\" of relation \"{}\" does not exist",
                    column, table,
                )
            }
            Self::InsertColumnCountMismatch { columns, values } => write!(
                f,
                "INSERT has more target columns than expressions ({} columns, {} values)",
                columns, values,
            ),
            Self::AssignmentTypeMismatch {
                column,
                expected,
                found,
            } => write!(
                f,
                "column \"{}\" is of type {} but expression is of type {}",
                column, expected, found,
            ),
            Self::DmlWhereNotBoolean { found } => {
                write!(
                    f,
                    "argument of WHERE must be type boolean, not type {}",
                    found,
                )
            }
            Self::Unsupported(msg) => write!(f, "{}", msg),
            Self::Internal(msg) => write!(f, "internal error: {}", msg),
        }
    }
}

impl std::error::Error for AnalyzerError {}
