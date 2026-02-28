//! Structured SQL error types with PostgreSQL SQLSTATE codes.
//!
//! Provides typed error variants that carry SQLSTATE codes for proper
//! PostgreSQL wire protocol error responses. Uses `#[from] anyhow::Error`
//! as a bridge so existing `anyhow!()` call sites can be migrated gradually.

use crate::model::DataType;
use crate::sql::analyzer::AnalyzerError;

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
#[allow(dead_code)] // framework: PG error code compatibility
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

    #[error("sequence \"{0}\" does not exist")]
    SequenceNotFound(String),

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
    UniqueViolation {
        constraint: String,
        message: String,
        row_offset: Option<usize>,
    },

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

    #[error("terminating connection due to idle-in-transaction timeout")]
    IdleInTransactionTimeout,

    #[error("canceling statement due to lock timeout")]
    LockTimeout,

    #[error(
        "tenant memory quota exceeded in {component}: requested {requested_bytes} bytes, used {used_bytes} bytes, quota {quota_bytes} bytes"
    )]
    TenantMemoryQuotaExceeded {
        component: String,
        requested_bytes: usize,
        used_bytes: usize,
        quota_bytes: usize,
    },

    #[error("could not obtain lock on row in relation \"{relation}\"")]
    LockNotAvailable { relation: String },

    #[error("too many advisory locks held by this session (limit: {limit})")]
    AdvisoryLockLimitExceeded { limit: usize },

    #[error("advisory lock reentrant acquisition count overflow")]
    AdvisoryLockCounterOverflow,

    #[error("{message}")]
    StatementTooComplex { message: String },

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

    // Parameter errors
    #[error("could not determine data type of parameter ${index}")]
    IndeterminateParameterType { index: usize },

    #[error("inconsistent types deduced for parameter ${index}: {first} vs {second}")]
    InconsistentParameterTypes {
        index: usize,
        first: DataType,
        second: DataType,
    },

    #[error("there is no parameter ${index}")]
    InvalidParameterUsage { index: usize, context: String },

    // Foreign key violations
    #[error("{message}")]
    ForeignKeyViolation { constraint: String, message: String },

    // Schema errors
    #[error("schema \"{0}\" does not exist")]
    InvalidSchemaName(String),

    #[error("schema \"{0}\" already exists")]
    DuplicateSchema(String),

    // Object errors
    #[error("{0}")]
    UndefinedObject(String),

    #[error("{0}")]
    DuplicateObject(String),

    // Parameter value errors
    #[error("{message}")]
    InvalidParameterValue { message: String },

    #[error("{message}")]
    NullValueNotAllowed { message: String },

    // Dependency errors
    #[error("{message}")]
    DependentObjectsStillExist { message: String },

    // Transaction state errors
    #[error("{message}")]
    NoActiveTransaction { message: String },

    // Type/grouping/window errors
    #[error("{message}")]
    DataTypeMismatch { message: String },

    #[error("{message}")]
    GroupingError { message: String },

    #[error("{message}")]
    WindowFunctionError { message: String },

    #[error("{message}")]
    OperatorResolution { message: String },

    #[error("{message}")]
    AmbiguousOperator { message: String },

    // Structure errors (42601 without "syntax error: " prefix)
    #[error("{0}")]
    SqlStructure(String),

    // Catalog errors
    #[error("{0}")]
    InvalidCatalogName(String),

    // Sequence errors
    #[error("{message}")]
    SequenceLimitExceeded { message: String },

    // PL/pgSQL STRICT errors
    #[error("query returned no rows")]
    NoDataFound,

    #[error("query returned more than one row")]
    TooManyRows,

    // Auth errors
    #[error("{message}")]
    InvalidAuthorizationSpecification { message: String },

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
            Self::SequenceNotFound(_) => "42P01",
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
            Self::IdleInTransactionTimeout => "25P03",
            Self::LockTimeout => "55P03",
            Self::TenantMemoryQuotaExceeded { .. } => "53200",
            Self::LockNotAvailable { .. } => "55P03",
            Self::AdvisoryLockLimitExceeded { .. } => "54000",
            Self::AdvisoryLockCounterOverflow => "54000",
            Self::StatementTooComplex { .. } => "54001",
            Self::InFailedTransaction => "25P02",
            Self::PermissionDenied { .. } => "42501",
            Self::DuplicateRelation(_) => "42P07",
            Self::IndeterminateParameterType { .. } => "42P18",
            Self::InconsistentParameterTypes { .. } => "42P18",
            Self::InvalidParameterUsage { .. } => "42P02",
            Self::ForeignKeyViolation { .. } => "23503",
            Self::InvalidSchemaName(_) => "3F000",
            Self::DuplicateSchema(_) => "42P06",
            Self::UndefinedObject(_) => "42704",
            Self::DuplicateObject(_) => "42710",
            Self::InvalidParameterValue { .. } => "22023",
            Self::NullValueNotAllowed { .. } => "22004",
            Self::DependentObjectsStillExist { .. } => "2BP01",
            Self::NoActiveTransaction { .. } => "25P01",
            Self::DataTypeMismatch { .. } => "42804",
            Self::GroupingError { .. } => "42803",
            Self::WindowFunctionError { .. } => "42P20",
            Self::OperatorResolution { .. } => "42883",
            Self::AmbiguousOperator { .. } => "42725",
            Self::SqlStructure(_) => "42601",
            Self::InvalidCatalogName(_) => "3D000",
            Self::SequenceLimitExceeded { .. } => "2200H",
            Self::NoDataFound => "P0002",
            Self::TooManyRows => "P0003",
            Self::InvalidAuthorizationSpecification { .. } => "28000",
            Self::Unsupported(_) => "0A000",
            Self::Internal(_) => "XX000",
        }
    }

    #[allow(dead_code)] // framework: PG error reporting API
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
            AnalyzerError::IndeterminateParameterType { index } => {
                SqlError::IndeterminateParameterType { index }
            }
            AnalyzerError::InconsistentParameterTypes {
                index,
                first,
                second,
            } => SqlError::InconsistentParameterTypes {
                index,
                first,
                second,
            },
            AnalyzerError::InvalidParameterUsage { index, context } => {
                SqlError::InvalidParameterUsage { index, context }
            }
            AnalyzerError::Unsupported(msg) => SqlError::Unsupported(msg),
            AnalyzerError::OperatorTypeMismatch { .. } => SqlError::OperatorResolution {
                message: e.to_string(),
            },
            AnalyzerError::AmbiguousOperator { .. } => SqlError::AmbiguousOperator {
                message: e.to_string(),
            },
            AnalyzerError::ArgumentCountMismatch { .. } => SqlError::OperatorResolution {
                message: e.to_string(),
            },
            AnalyzerError::TypeMismatch { .. } => SqlError::DataTypeMismatch {
                message: e.to_string(),
            },
            AnalyzerError::TypesCannotBeMatched { .. } => SqlError::DataTypeMismatch {
                message: e.to_string(),
            },
            AnalyzerError::UngroupedColumn { .. } => SqlError::GroupingError {
                message: e.to_string(),
            },
            AnalyzerError::AggregateNotAllowed { .. } => SqlError::GroupingError {
                message: e.to_string(),
            },
            AnalyzerError::WindowNotAllowed { .. } => SqlError::WindowFunctionError {
                message: e.to_string(),
            },
            AnalyzerError::ScalarSubqueryMultipleColumns { .. } => {
                SqlError::SqlStructure(e.to_string())
            }
            AnalyzerError::SetOperationColumnMismatch { .. } => {
                SqlError::SqlStructure(e.to_string())
            }
            AnalyzerError::InsertColumnCountMismatch { .. } => {
                SqlError::SqlStructure(e.to_string())
            }
            AnalyzerError::AssignmentTypeMismatch { .. } => SqlError::DataTypeMismatch {
                message: e.to_string(),
            },
            AnalyzerError::DmlWhereNotBoolean { .. } => SqlError::DataTypeMismatch {
                message: e.to_string(),
            },
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
        assert_eq!(SqlError::SequenceNotFound("s".into()).sqlstate(), "42P01");
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
                message: "dup".into(),
                row_offset: None,
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
        assert_eq!(SqlError::LockTimeout.sqlstate(), "55P03");
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
            SqlError::AdvisoryLockLimitExceeded { limit: 64 }.sqlstate(),
            "54000"
        );
        assert_eq!(
            SqlError::StatementTooComplex {
                message: "too deep".into()
            }
            .sqlstate(),
            "54001"
        );
        assert_eq!(
            SqlError::DuplicateRelation("idx".into()).sqlstate(),
            "42P07"
        );
        assert_eq!(
            SqlError::IndeterminateParameterType { index: 1 }.sqlstate(),
            "42P18"
        );
        assert_eq!(
            SqlError::InconsistentParameterTypes {
                index: 1,
                first: DataType::Int32,
                second: DataType::Boolean,
            }
            .sqlstate(),
            "42P18"
        );
        assert_eq!(
            SqlError::InvalidParameterUsage {
                index: 0,
                context: "test".into()
            }
            .sqlstate(),
            "42P02"
        );
        assert_eq!(
            SqlError::ForeignKeyViolation {
                constraint: "fk".into(),
                message: "msg".into()
            }
            .sqlstate(),
            "23503"
        );
        assert_eq!(SqlError::InvalidSchemaName("s".into()).sqlstate(), "3F000");
        assert_eq!(SqlError::DuplicateSchema("s".into()).sqlstate(), "42P06");
        assert_eq!(SqlError::UndefinedObject("o".into()).sqlstate(), "42704");
        assert_eq!(SqlError::DuplicateObject("o".into()).sqlstate(), "42710");
        assert_eq!(
            SqlError::InvalidParameterValue {
                message: "bad".into()
            }
            .sqlstate(),
            "22023"
        );
        assert_eq!(
            SqlError::DependentObjectsStillExist {
                message: "dep".into()
            }
            .sqlstate(),
            "2BP01"
        );
        assert_eq!(
            SqlError::NoActiveTransaction {
                message: "no txn".into()
            }
            .sqlstate(),
            "25P01"
        );
        assert_eq!(
            SqlError::DataTypeMismatch {
                message: "mismatch".into()
            }
            .sqlstate(),
            "42804"
        );
        assert_eq!(
            SqlError::GroupingError {
                message: "group".into()
            }
            .sqlstate(),
            "42803"
        );
        assert_eq!(
            SqlError::WindowFunctionError {
                message: "win".into()
            }
            .sqlstate(),
            "42P20"
        );
        assert_eq!(
            SqlError::OperatorResolution {
                message: "op".into()
            }
            .sqlstate(),
            "42883"
        );
        assert_eq!(
            SqlError::AmbiguousOperator {
                message: "operator is not unique: unknown + unknown".into()
            }
            .sqlstate(),
            "42725"
        );
        assert_eq!(SqlError::SqlStructure("struct".into()).sqlstate(), "42601");
        assert_eq!(
            SqlError::InvalidCatalogName("db".into()).sqlstate(),
            "3D000"
        );
        assert_eq!(
            SqlError::SequenceLimitExceeded {
                message: "limit".into()
            }
            .sqlstate(),
            "2200H"
        );
        assert_eq!(SqlError::NoDataFound.sqlstate(), "P0002");
        assert_eq!(SqlError::TooManyRows.sqlstate(), "P0003");
        assert_eq!(
            SqlError::InvalidAuthorizationSpecification {
                message: "auth".into()
            }
            .sqlstate(),
            "28000"
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
            SqlError::SequenceNotFound("my_seq".into()).to_string(),
            "sequence \"my_seq\" does not exist"
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
        // New variants: message passthrough (no prefixes)
        assert_eq!(
            SqlError::OperatorResolution {
                message: "operator does not exist: int + text".into()
            }
            .to_string(),
            "operator does not exist: int + text"
        );
        assert_eq!(
            SqlError::AmbiguousOperator {
                message: "operator is not unique: unknown + unknown".into()
            }
            .to_string(),
            "operator is not unique: unknown + unknown"
        );
        assert_eq!(
            SqlError::SqlStructure("each UNION query must have the same number of columns".into())
                .to_string(),
            "each UNION query must have the same number of columns"
        );
        assert_eq!(
            SqlError::InvalidSchemaName("myschema".into()).to_string(),
            "schema \"myschema\" does not exist"
        );
        assert_eq!(
            SqlError::DuplicateSchema("myschema".into()).to_string(),
            "schema \"myschema\" already exists"
        );
        assert_eq!(SqlError::NoDataFound.to_string(), "query returned no rows");
        assert_eq!(
            SqlError::TooManyRows.to_string(),
            "query returned more than one row"
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
    fn test_anyhow_bridge_roundtrip_new_variants() {
        // ForeignKeyViolation
        let sql_err = SqlError::ForeignKeyViolation {
            constraint: "fk_test".into(),
            message: "violates fk".into(),
        };
        let anyhow_err: anyhow::Error = sql_err.into();
        let recovered = anyhow_err.downcast_ref::<SqlError>().unwrap();
        assert_eq!(recovered.sqlstate(), "23503");

        // InvalidSchemaName
        let sql_err = SqlError::InvalidSchemaName("test".into());
        let anyhow_err: anyhow::Error = sql_err.into();
        let recovered = anyhow_err.downcast_ref::<SqlError>().unwrap();
        assert_eq!(recovered.sqlstate(), "3F000");

        // NoDataFound
        let sql_err = SqlError::NoDataFound;
        let anyhow_err: anyhow::Error = sql_err.into();
        let recovered = anyhow_err.downcast_ref::<SqlError>().unwrap();
        assert_eq!(recovered.sqlstate(), "P0002");

        // InvalidAuthorizationSpecification
        let sql_err = SqlError::InvalidAuthorizationSpecification {
            message: "auth fail".into(),
        };
        let anyhow_err: anyhow::Error = sql_err.into();
        let recovered = anyhow_err.downcast_ref::<SqlError>().unwrap();
        assert_eq!(recovered.sqlstate(), "28000");
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

        // UngroupedColumn → 42803 (was XX000 before error unification)
        let ae = AnalyzerError::UngroupedColumn { name: "x".into() };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42803");

        // OperatorTypeMismatch → 42883
        let ae = AnalyzerError::OperatorTypeMismatch {
            operator: "+".into(),
            left: "integer".into(),
            right: "text".into(),
        };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42883");

        // AmbiguousOperator → 42725
        let ae = AnalyzerError::AmbiguousOperator {
            operator: "+".into(),
            left: "unknown".into(),
            right: "unknown".into(),
        };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42725");

        // TypeMismatch → 42804
        let ae = AnalyzerError::TypeMismatch {
            expected: DataType::Boolean,
            found: DataType::Int32,
            context: "WHERE clause".into(),
        };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42804");

        // SetOperationColumnMismatch → 42601
        let ae = AnalyzerError::SetOperationColumnMismatch { left: 2, right: 3 };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42601");

        // WindowNotAllowed → 42P20
        let ae = AnalyzerError::WindowNotAllowed {
            function: "row_number".into(),
            context: "WHERE clause".into(),
        };
        let sql: SqlError = ae.into();
        assert_eq!(sql.sqlstate(), "42P20");
    }
}
