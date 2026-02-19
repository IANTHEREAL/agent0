//! Structured SQL error types with PostgreSQL SQLSTATE codes.
//!
//! Provides typed error variants that carry SQLSTATE codes for proper
//! PostgreSQL wire protocol error responses. Uses `#[from] anyhow::Error`
//! as a bridge so existing `anyhow!()` call sites can be migrated gradually.

use crate::sql::analyzer::AnalyzerError;
use crate::sql::types::TypeError;
use crate::types::DataType;

fn column_not_found_display(column: &str, hint: &Option<String>) -> String {
    match hint {
        Some(h) => format!("column \"{}\" does not exist\n{}", column, h),
        None => format!("column \"{}\" does not exist", column),
    }
}

/// Structured SQL error with SQLSTATE code support.
/// Structured SQL error with SQLSTATE code support.
///
/// Variants are defined for all major PostgreSQL error categories.
/// Not all variants are actively constructed yet — they exist for
/// gradual migration from `anyhow!()` call sites.
#[allow(dead_code)] // PG error code compatibility
#[derive(Debug, thiserror::Error)]
pub enum SqlError {
    // Syntax / parsing
    #[error("syntax error: {0}")]
    Syntax(String),

    #[error("syntax error in tsquery: \"{query}\"")]
    TsquerySyntax { query: String },

    #[error("no operand in tsquery: \"{query}\"")]
    TsqueryNoOperand { query: String },

    // Object not found
    #[error("relation \"{0}\" does not exist")]
    RelationNotFound(String),

    #[error("{}", column_not_found_display(.column, .hint))]
    ColumnNotFound {
        column: String,
        hint: Option<String>,
    },

    #[error("column reference \"{0}\" is ambiguous")]
    AmbiguousColumn(String),

    #[error("function {0} does not exist")]
    FunctionNotFound(String),

    // Type errors
    #[error("invalid input syntax for type {type_name}: \"{value}\"")]
    InvalidInputSyntax { type_name: String, value: String },

    #[error("cannot cast type {from} to {to}")]
    InvalidCast { from: String, to: DataType },

    // Constraint violations
    #[error("{message}")]
    UniqueViolation { constraint: String, message: String },

    #[error("{message}")]
    NotNullViolation {
        column: String,
        relation: String,
        message: String,
    },

    #[error("new row for relation \"{table}\" violates check constraint \"{constraint}\"\nDETAIL:  Failing row contains ({detail}).")]
    CheckViolation {
        table: String,
        constraint: String,
        detail: String,
    },

    #[error("{message}")]
    NumericValueOutOfRange { message: String },

    #[error("value too long for type character varying({max_length})")]
    StringDataRightTruncation { max_length: u64 },

    // Runtime errors
    #[error("division by zero")]
    DivisionByZero,

    #[error("canceling statement due to statement timeout")]
    StatementTimeout,

    #[error("could not obtain lock on row in relation \"{relation}\"")]
    LockNotAvailable { relation: String },

    #[error("current transaction is aborted, commands ignored until end of transaction block")]
    InFailedTransaction,

    // Permission errors
    #[error("permission denied for {object_type} {object_name}")]
    PermissionDenied {
        object_type: String,
        object_name: String,
    },

    // Duplicate object
    #[error("relation \"{0}\" already exists")]
    DuplicateRelation(String),

    // Unsupported features
    #[error("{0}")]
    Unsupported(String),

    // Bridge for unmigrated anyhow errors
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl SqlError {
    /// PostgreSQL SQLSTATE error code.
    pub fn sqlstate(&self) -> &'static str {
        match self {
            Self::Syntax(_) => "42601",
            Self::TsquerySyntax { .. } => "42601",
            Self::TsqueryNoOperand { .. } => "42601",
            Self::RelationNotFound(_) => "42P01",
            Self::ColumnNotFound { .. } => "42703",
            Self::AmbiguousColumn(_) => "42702",
            Self::FunctionNotFound(_) => "42883",
            Self::InvalidInputSyntax { .. } => "22P02",
            Self::InvalidCast { .. } => "42846",
            Self::UniqueViolation { .. } => "23505",
            Self::NotNullViolation { .. } => "23502",
            Self::CheckViolation { .. } => "23514",
            Self::NumericValueOutOfRange { .. } => "22003",
            Self::StringDataRightTruncation { .. } => "22001",
            Self::DivisionByZero => "22012",
            Self::StatementTimeout => "57014",
            Self::LockNotAvailable { .. } => "55P03",
            Self::InFailedTransaction => "25P02",
            Self::PermissionDenied { .. } => "42501",
            Self::DuplicateRelation(_) => "42P07",
            Self::Unsupported(_) => "0A000",
            Self::Internal(_) => "XX000",
        }
    }

    #[allow(dead_code)] // PG error reporting API
    pub fn severity(&self) -> &'static str {
        "ERROR"
    }
}

impl From<AnalyzerError> for SqlError {
    fn from(e: AnalyzerError) -> Self {
        match e {
            AnalyzerError::ColumnNotFound { ref name, .. } => {
                // Preserve the full Display output which includes the HINT line.
                let full = e.to_string();
                let hint = full.find("\nHINT:").map(|pos| full[pos + 1..].to_string());
                SqlError::ColumnNotFound {
                    column: name.clone(),
                    hint,
                }
            }
            AnalyzerError::AmbiguousColumn { name, .. } => SqlError::AmbiguousColumn(name),
            AnalyzerError::TableNotFound(name) => SqlError::RelationNotFound(name),
            AnalyzerError::FunctionNotFound { name, arg_types } => {
                let types: Vec<_> = arg_types.iter().map(|t| t.to_string()).collect();
                SqlError::FunctionNotFound(format!("{}({})", name, types.join(", ")))
            }
            AnalyzerError::InvalidLiteral {
                value, target_type, ..
            } => SqlError::InvalidInputSyntax {
                type_name: target_type.to_string(),
                value,
            },
            AnalyzerError::DmlColumnNotFound { column, .. } => {
                SqlError::ColumnNotFound { column, hint: None }
            }
            AnalyzerError::Unsupported(msg) => SqlError::Unsupported(msg),
            other => SqlError::Internal(anyhow::anyhow!("{}", other)),
        }
    }
}

impl From<TypeError> for SqlError {
    fn from(e: TypeError) -> Self {
        match e {
            TypeError::ColumnNotFound { name, .. } => SqlError::ColumnNotFound {
                column: name,
                hint: None,
            },
            TypeError::AmbiguousColumn { name, .. } => SqlError::AmbiguousColumn(name),
            TypeError::UnknownFunction(name) => SqlError::FunctionNotFound(name),
            other => SqlError::Internal(anyhow::anyhow!("{}", other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sqlstate_codes() {
        assert_eq!(SqlError::Syntax("bad".into()).sqlstate(), "42601");
        assert_eq!(
            SqlError::TsquerySyntax { query: "x".into() }.sqlstate(),
            "42601"
        );
        assert_eq!(
            SqlError::TsqueryNoOperand { query: "x".into() }.sqlstate(),
            "42601"
        );
        assert_eq!(SqlError::RelationNotFound("t".into()).sqlstate(), "42P01");
        assert_eq!(
            SqlError::ColumnNotFound {
                column: "c".into(),
                hint: None
            }
            .sqlstate(),
            "42703"
        );
        assert_eq!(SqlError::AmbiguousColumn("c".into()).sqlstate(), "42702");
        assert_eq!(SqlError::FunctionNotFound("f".into()).sqlstate(), "42883");
        assert_eq!(
            SqlError::InvalidInputSyntax {
                type_name: "integer".into(),
                value: "abc".into()
            }
            .sqlstate(),
            "22P02"
        );
        assert_eq!(
            SqlError::InvalidCast {
                from: "text".to_string(),
                to: DataType::Int32
            }
            .sqlstate(),
            "42846"
        );
        assert_eq!(
            SqlError::UniqueViolation {
                constraint: "pk".into(),
                message: "dup".into()
            }
            .sqlstate(),
            "23505"
        );
        assert_eq!(
            SqlError::NotNullViolation {
                column: "c".into(),
                relation: "t".into(),
                message: "null".into()
            }
            .sqlstate(),
            "23502"
        );
        assert_eq!(
            SqlError::CheckViolation {
                table: "t".into(),
                constraint: "ck".into(),
                detail: String::new(),
            }
            .sqlstate(),
            "23514"
        );
        assert_eq!(SqlError::DivisionByZero.sqlstate(), "22012");
        assert_eq!(SqlError::StatementTimeout.sqlstate(), "57014");
        assert_eq!(SqlError::InFailedTransaction.sqlstate(), "25P02");
        assert_eq!(
            SqlError::PermissionDenied {
                object_type: "table".into(),
                object_name: "t".into()
            }
            .sqlstate(),
            "42501"
        );
        assert_eq!(
            SqlError::LockNotAvailable {
                relation: "t".into()
            }
            .sqlstate(),
            "55P03"
        );
        assert_eq!(
            SqlError::DuplicateRelation("idx".into()).sqlstate(),
            "42P07"
        );
        assert_eq!(SqlError::Unsupported("x".into()).sqlstate(), "0A000");
        let internal = SqlError::Internal(anyhow::anyhow!("boom"));
        assert_eq!(internal.sqlstate(), "XX000");
    }

    #[test]
    fn test_display_matches_postgres_format() {
        assert_eq!(
            SqlError::RelationNotFound("users".into()).to_string(),
            "relation \"users\" does not exist"
        );
        assert_eq!(
            SqlError::ColumnNotFound {
                column: "age".into(),
                hint: None,
            }
            .to_string(),
            "column \"age\" does not exist"
        );
        assert_eq!(
            SqlError::AmbiguousColumn("id".into()).to_string(),
            "column reference \"id\" is ambiguous"
        );
        assert_eq!(
            SqlError::InvalidInputSyntax {
                type_name: "integer".into(),
                value: "abc".into()
            }
            .to_string(),
            "invalid input syntax for type integer: \"abc\""
        );
        assert_eq!(SqlError::DivisionByZero.to_string(), "division by zero");
        assert_eq!(
            SqlError::DuplicateRelation("my_idx".into()).to_string(),
            "relation \"my_idx\" already exists"
        );
    }

    #[test]
    fn test_anyhow_bridge_roundtrip() {
        let sql_err = SqlError::DivisionByZero;
        let anyhow_err: anyhow::Error = sql_err.into();
        let recovered = anyhow_err.downcast_ref::<SqlError>().unwrap();
        assert_eq!(recovered.sqlstate(), "22012");
    }

    #[test]
    fn test_from_anyhow_produces_internal() {
        let anyhow_err = anyhow::anyhow!("something went wrong");
        let sql_err = SqlError::from(anyhow_err);
        assert_eq!(sql_err.sqlstate(), "XX000");
        assert!(matches!(sql_err, SqlError::Internal(_)));
    }

    #[test]
    fn test_analyzer_error_roundtrip_preserves_sqlstate() {
        use crate::sql::analyzer::AnalyzerError;

        // ColumnNotFound → 42703
        let ae = AnalyzerError::ColumnNotFound {
            name: "age".into(),
            available: vec![],
        };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42703");
        let anyhow_err: anyhow::Error = sql.into();
        let recovered = anyhow_err.downcast_ref::<SqlError>().unwrap();
        assert_eq!(recovered.sqlstate(), "42703");

        // TableNotFound → 42P01
        let ae = AnalyzerError::TableNotFound("users".into());
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42P01");
        let anyhow_err: anyhow::Error = sql.into();
        let recovered = anyhow_err.downcast_ref::<SqlError>().unwrap();
        assert_eq!(recovered.sqlstate(), "42P01");

        // AmbiguousColumn → 42702
        let ae = AnalyzerError::AmbiguousColumn {
            name: "id".into(),
            tables: vec![],
        };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42702");

        // FunctionNotFound → 42883
        let ae = AnalyzerError::FunctionNotFound {
            name: "foo".into(),
            arg_types: vec![DataType::Int32, DataType::Text],
        };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42883");

        // InvalidLiteral → 22P02
        let ae = AnalyzerError::InvalidLiteral {
            value: "abc".into(),
            target_type: DataType::Int32,
            parse_error: "bad".into(),
        };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "22P02");

        // DmlColumnNotFound → 42703
        let ae = AnalyzerError::DmlColumnNotFound {
            column: "x".into(),
            table: "t".into(),
        };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42703");

        // Unsupported → 0A000
        let ae = AnalyzerError::Unsupported("nope".into());
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "0A000");

        // Unmapped variant → XX000
        let ae = AnalyzerError::UngroupedColumn { name: "x".into() };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "XX000");
    }
}
