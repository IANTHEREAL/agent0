//! Type inference errors

use crate::types::DataType;

#[allow(dead_code)] // type inference module
#[derive(Debug, Clone, PartialEq)]
pub enum TypeError {
    /// Column does not exist
    ColumnNotFound {
        name: String,
        available: Vec<String>,
    },
    /// Ambiguous column reference (multiple tables have the same column name)
    AmbiguousColumn { name: String, tables: Vec<String> },
    /// Function does not exist
    UnknownFunction(String),
    /// Argument count mismatch
    ArgumentCountMismatch {
        function: String,
        expected: usize,
        got: usize,
    },
    /// Argument type mismatch
    ArgumentTypeMismatch {
        function: String,
        position: usize,
        expected: DataType,
        got: DataType,
    },
    /// Operator type mismatch
    OperatorTypeMismatch {
        operator: String,
        left: DataType,
        right: DataType,
    },
    /// CASE branches have incompatible types
    CaseBranchTypeMismatch { types: Vec<DataType> },
    /// Scalar subquery must return exactly one column
    ScalarSubqueryMultipleColumns,
    /// Unsupported expression
    UnsupportedExpression(String),
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ColumnNotFound { name, available } => {
                write!(f, "column '{}' does not exist", name)?;
                if !available.is_empty() {
                    write!(f, ". Available columns: {}", available.join(", "))?;
                }
                Ok(())
            }
            Self::AmbiguousColumn { name, tables } => {
                write!(
                    f,
                    "column '{}' is ambiguous, could be: {}",
                    name,
                    tables
                        .iter()
                        .map(|t| format!("{}.{}", t, name))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Self::UnknownFunction(name) => write!(f, "function '{}' does not exist", name),
            Self::ArgumentCountMismatch {
                function,
                expected,
                got,
            } => {
                write!(
                    f,
                    "function '{}' expects {} arguments, got {}",
                    function, expected, got
                )
            }
            Self::ArgumentTypeMismatch {
                function,
                position,
                expected,
                got,
            } => {
                write!(
                    f,
                    "function '{}' argument {} expects {}, got {}",
                    function, position, expected, got
                )
            }
            Self::OperatorTypeMismatch {
                operator,
                left,
                right,
            } => {
                write!(
                    f,
                    "operator '{}' cannot be applied to {} and {}",
                    operator, left, right
                )
            }
            Self::CaseBranchTypeMismatch { types } => {
                write!(f, "CASE branches have incompatible types: {:?}", types)
            }
            Self::ScalarSubqueryMultipleColumns => {
                write!(f, "scalar subquery must return exactly one column")
            }
            Self::UnsupportedExpression(desc) => write!(f, "unsupported expression: {}", desc),
        }
    }
}

impl std::error::Error for TypeError {}
