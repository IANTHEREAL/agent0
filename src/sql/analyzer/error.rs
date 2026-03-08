//! Analyzer error types.
//!
//! All errors follow PostgreSQL error message conventions where applicable.

use crate::model::DataType;
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
    /// Fields carry lowercase PG-style type names (e.g. "text", "integer", "unknown").
    OperatorTypeMismatch {
        operator: String,
        left: String,
        right: String,
    },

    /// Operator resolution is ambiguous (multiple candidates match).
    /// Fields carry lowercase PG-style type names (typically "unknown").
    AmbiguousOperator {
        operator: String,
        left: String,
        right: String,
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

    /// Invalid function/config parameter value (SQLSTATE 22023).
    InvalidParameterValue { message: String },

    /// SQL structure/syntax error (SQLSTATE 42601).
    SqlStructure(String),

    /// Could not determine data type of parameter (SQLSTATE 42P18).
    IndeterminateParameterType { index: usize },

    /// Parameter referenced in inconsistent type contexts.
    InconsistentParameterTypes {
        index: usize,
        first: DataType,
        second: DataType,
    },

    /// Parameter in non-parameterizable statement context (SQLSTATE 42P02).
    InvalidParameterUsage { index: usize, context: String },

    /// Schema does not exist (SQLSTATE 3F000).
    SchemaNotFound(String),

    /// Collation does not exist in the given schema (SQLSTATE 42704).
    CollationNotFound(String),

    /// Cross-database reference (SQLSTATE 0A000).
    CrossDatabaseReference(String),

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
                    // Use edit-distance to find the closest match (PG-style hint).
                    let best = available
                        .iter()
                        .filter_map(|candidate| {
                            let col_part = candidate.rsplit('.').next().unwrap_or(candidate);
                            let dist = strsim_damerau_levenshtein(
                                &name.to_lowercase(),
                                &col_part.to_lowercase(),
                            );
                            if dist <= 3 { Some((dist, candidate.as_str())) } else { None }
                        })
                        .min_by_key(|(d, _)| *d)
                        .map(|(_, c)| c);
                    if let Some(suggestion) = best {
                        write!(
                            f,
                            "\nHINT:  Perhaps you meant to reference the column \"{}\".",
                            suggestion
                        )?;
                    }
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
                let types: Vec<_> = arg_types
                    .iter()
                    .map(|t| t.to_string().to_lowercase())
                    .collect();
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
                "operator does not exist: {} {} {}\nHINT:  No operator matches the given name and argument types. You might need to add explicit type casts.",
                left, operator, right
            ),
            Self::AmbiguousOperator {
                operator,
                left,
                right,
            } => write!(
                f,
                "operator is not unique: {} {} {}\nHINT:  Could not choose a best candidate operator. You might need to add explicit type casts.",
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
            Self::InvalidParameterValue { message } => write!(f, "{}", message),
            Self::SqlStructure(msg) => write!(f, "{}", msg),
            Self::IndeterminateParameterType { index } => {
                write!(f, "could not determine data type of parameter ${}", index)
            }
            Self::InconsistentParameterTypes {
                index,
                first,
                second,
            } => write!(
                f,
                "inconsistent types deduced for parameter ${}: {} vs {}",
                index, first, second,
            ),
            Self::InvalidParameterUsage { index, context } => {
                write!(f, "there is no parameter ${}: {}", index, context)
            }
            Self::SchemaNotFound(name) => {
                write!(f, "schema \"{}\" does not exist", name)
            }
            Self::CollationNotFound(name) => {
                write!(f, "collation \"{}\" does not exist", name)
            }
            Self::CrossDatabaseReference(name) => {
                write!(f, "cross-database references are not implemented: {}", name)
            }
            Self::Unsupported(msg) => write!(f, "{}", msg),
            Self::Internal(msg) => write!(f, "internal error: {}", msg),
        }
    }
}

impl std::error::Error for AnalyzerError {}

/// Simple Damerau-Levenshtein distance for column-name suggestions.
fn strsim_damerau_levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let la = a.len();
    let lb = b.len();
    if la == 0 {
        return lb;
    }
    if lb == 0 {
        return la;
    }
    let mut d = vec![vec![0usize; lb + 1]; la + 1];
    for (i, row) in d.iter_mut().enumerate().take(la + 1) {
        row[0] = i;
    }
    for (j, val) in d[0].iter_mut().enumerate().take(lb + 1) {
        *val = j;
    }
    for i in 1..=la {
        for j in 1..=lb {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + cost);
            }
        }
    }
    d[la][lb]
}
