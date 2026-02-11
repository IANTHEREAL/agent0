//! Structured SQL error types with PostgreSQL SQLSTATE codes.
//!
//! Provides typed error variants that carry SQLSTATE codes for proper
//! PostgreSQL wire protocol error responses. Uses `#[from] anyhow::Error`
//! as a bridge so existing `anyhow!()` call sites can be migrated gradually.

use crate::sql::types::TypeError;
use crate::types::DataType;

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

    // Object not found
    #[error("relation \"{0}\" does not exist")]
    RelationNotFound(String),

    #[error("column \"{column}\" does not exist")]
    ColumnNotFound { column: String },

    #[error("column reference \"{0}\" is ambiguous")]
    AmbiguousColumn(String),

    #[error("function {0} does not exist")]
    FunctionNotFound(String),

    // Type errors
    #[error("invalid input syntax for type {type_name}: \"{value}\"")]
    InvalidInputSyntax { type_name: String, value: String },

    #[error("cannot cast type {from} to {to}")]
    InvalidCast { from: DataType, to: DataType },

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

    // Runtime errors
    #[error("Division by zero")]
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
            Self::RelationNotFound(_) => "42P01",
            Self::ColumnNotFound { .. } => "42703",
            Self::AmbiguousColumn(_) => "42702",
            Self::FunctionNotFound(_) => "42883",
            Self::InvalidInputSyntax { .. } => "22P02",
            Self::InvalidCast { .. } => "42846",
            Self::UniqueViolation { .. } => "23505",
            Self::NotNullViolation { .. } => "23502",
            Self::CheckViolation { .. } => "23514",
            Self::DivisionByZero => "22012",
            Self::StatementTimeout => "57014",
            Self::LockNotAvailable { .. } => "55P03",
            Self::InFailedTransaction => "25P02",
            Self::PermissionDenied { .. } => "42501",
            Self::Unsupported(_) => "0A000",
            Self::Internal(_) => "XX000",
        }
    }

    #[allow(dead_code)] // PG error reporting API
    pub fn severity(&self) -> &'static str {
        "ERROR"
    }
}

impl From<TypeError> for SqlError {
    fn from(e: TypeError) -> Self {
        match e {
            TypeError::ColumnNotFound { name, .. } => SqlError::ColumnNotFound { column: name },
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
        assert_eq!(SqlError::RelationNotFound("t".into()).sqlstate(), "42P01");
        assert_eq!(
            SqlError::ColumnNotFound { column: "c".into() }.sqlstate(),
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
                from: DataType::Text,
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
                column: "age".into()
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
        assert_eq!(SqlError::DivisionByZero.to_string(), "Division by zero");
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
}
