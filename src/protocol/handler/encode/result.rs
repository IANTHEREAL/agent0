use super::types::datatype_to_pgtype;
use super::value::encode_value;
use crate::model::DataType;
use crate::pool::try_shrink_statement_memory_scope;
use crate::sql::bytea::ByteaOutput;
use crate::sql::memory::estimate_row_size;
use crate::sql::ExecuteResult;
use futures::stream;
use futures::StreamExt;
use pgwire::api::portal::Format;
use pgwire::api::results::{DataRowEncoder, FieldFormat, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::Type;
use pgwire::error::{PgWireError, PgWireResult};
use pgwire::messages::data::DataRow;
use std::sync::Arc;

pub(in crate::protocol::handler) fn result_to_response(
    result: ExecuteResult,
    bytea_output: ByteaOutput,
) -> PgWireResult<Response<'static>> {
    result_to_response_with_format(result, &Format::UnifiedText, bytea_output)
}

fn supports_binary_result_type(pg_type: &Type) -> bool {
    matches!(
        *pg_type,
        Type::BOOL
            | Type::INT4
            | Type::INT8
            | Type::FLOAT8
            | Type::TEXT
            | Type::VARCHAR
            | Type::BPCHAR
            | Type::NAME
            | Type::BYTEA
            | Type::TIMESTAMP
            | Type::TIMESTAMPTZ
            | Type::UUID
            | Type::JSON
            | Type::JSONB
            | Type::DATE
            | Type::TIME
            | Type::NUMERIC
            | Type::INTERVAL
    )
}

pub(in crate::protocol::handler) fn effective_result_format(
    pg_type: &Type,
    requested_format: FieldFormat,
) -> FieldFormat {
    if requested_format == FieldFormat::Binary && !supports_binary_result_type(pg_type) {
        FieldFormat::Text
    } else {
        requested_format
    }
}

pub(in crate::protocol::handler) fn result_to_response_with_format(
    result: ExecuteResult,
    result_format: &Format,
    bytea_output: ByteaOutput,
) -> PgWireResult<Response<'static>> {
    match result {
        ExecuteResult::Select {
            columns,
            column_types,
            rows,
            timezone,
        } => {
            let tz = crate::model::timestamp::TimeZoneSpec::parse(timezone.as_ref());
            let inferred_types: Vec<Type> = if let Some(types) = column_types.as_ref() {
                types
                    .iter()
                    .map(|dt| datatype_to_pgtype(Some(dt)))
                    .collect()
            } else if let Some(first_row) = rows.first() {
                first_row
                    .values
                    .iter()
                    .map(|v| {
                        let dt = v.data_type();
                        datatype_to_pgtype(dt.as_ref())
                    })
                    .collect()
            } else {
                vec![Type::TEXT; columns.len()]
            };

            let field_formats: Vec<FieldFormat> = inferred_types
                .iter()
                .enumerate()
                .map(|(i, pg_type)| effective_result_format(pg_type, result_format.format_for(i)))
                .collect();

            let fields: Vec<FieldInfo> = columns
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let pg_type = inferred_types.get(i).cloned().unwrap_or(Type::TEXT);
                    let format = field_formats.get(i).copied().unwrap_or(FieldFormat::Text);
                    FieldInfo::new(name.clone(), None, None, pg_type, format)
                })
                .collect();

            let fields = Arc::new(fields);

            let internal_types: Vec<DataType> = if let Some(types) = column_types.as_ref() {
                types.clone()
            } else {
                // INTENTIONAL: wire protocol encoding — Text OID is universally safe
                crate::model::infer_column_types_from_rows(&rows, columns.len())
            };

            let internal_types = Arc::new(internal_types);
            let field_formats = Arc::new(field_formats);
            let row_fields = fields.clone();
            // Stream row encoding directly to avoid building a second full in-memory
            // `Vec<DataRow>` buffer on top of executor row storage.
            let row_stream = stream::iter(rows).map(move |row| {
                let row_bytes = estimate_row_size(&row);
                let mut encoder = DataRowEncoder::new(row_fields.clone());
                let encoded = (|| {
                    for (i, value) in row.values.iter().enumerate() {
                        let col_type = internal_types.get(i);
                        let format = field_formats.get(i).copied().unwrap_or(FieldFormat::Text);
                        encode_value(&mut encoder, value, col_type, tz, format, bytea_output)?;
                    }
                    encoder.finish()
                })();
                // One row was fully consumed from executor materialization.
                try_shrink_statement_memory_scope(row_bytes);
                encoded
            });
            let results = QueryResponse::new(fields, row_stream);

            Ok(Response::Query(results))
        }

        ExecuteResult::CreateTable => Ok(Response::Execution(Tag::new("CREATE TABLE"))),

        ExecuteResult::DropTable => Ok(Response::Execution(Tag::new("DROP TABLE"))),

        ExecuteResult::TruncateTable => Ok(Response::Execution(Tag::new("TRUNCATE TABLE"))),

        ExecuteResult::CreateIndex => Ok(Response::Execution(Tag::new("CREATE INDEX"))),

        ExecuteResult::DropIndex => Ok(Response::Execution(Tag::new("DROP INDEX"))),

        ExecuteResult::CreateView => Ok(Response::Execution(Tag::new("CREATE VIEW"))),

        ExecuteResult::DropView => Ok(Response::Execution(Tag::new("DROP VIEW"))),

        ExecuteResult::CreateMaterializedView => {
            Ok(Response::Execution(Tag::new("CREATE MATERIALIZED VIEW")))
        }

        ExecuteResult::DropMaterializedView => {
            Ok(Response::Execution(Tag::new("DROP MATERIALIZED VIEW")))
        }

        ExecuteResult::RefreshMaterializedView => {
            Ok(Response::Execution(Tag::new("REFRESH MATERIALIZED VIEW")))
        }

        ExecuteResult::CreateProcedure => Ok(Response::Execution(Tag::new("CREATE PROCEDURE"))),

        ExecuteResult::DropProcedure => Ok(Response::Execution(Tag::new("DROP PROCEDURE"))),

        ExecuteResult::CreateFunction => Ok(Response::Execution(Tag::new("CREATE FUNCTION"))),

        ExecuteResult::DropFunction => Ok(Response::Execution(Tag::new("DROP FUNCTION"))),

        ExecuteResult::CreateTrigger => Ok(Response::Execution(Tag::new("CREATE TRIGGER"))),

        ExecuteResult::DropTrigger => Ok(Response::Execution(Tag::new("DROP TRIGGER"))),

        ExecuteResult::CreateExtension => Ok(Response::Execution(Tag::new("CREATE EXTENSION"))),

        ExecuteResult::DropExtension => Ok(Response::Execution(Tag::new("DROP EXTENSION"))),

        ExecuteResult::Call => Ok(Response::Execution(Tag::new("CALL"))),

        ExecuteResult::AlterTable => Ok(Response::Execution(Tag::new("ALTER TABLE"))),

        ExecuteResult::AlterSequence => Ok(Response::Execution(Tag::new("ALTER SEQUENCE"))),

        ExecuteResult::AlterFunction => Ok(Response::Execution(Tag::new("ALTER FUNCTION"))),

        ExecuteResult::AlterIndex => Ok(Response::Execution(Tag::new("ALTER INDEX"))),

        ExecuteResult::Insert { affected_rows } => Ok(Response::Execution(
            Tag::new("INSERT")
                .with_oid(0)
                .with_rows(affected_rows as usize),
        )),

        ExecuteResult::Delete { affected_rows } => Ok(Response::Execution(
            Tag::new("DELETE").with_rows(affected_rows as usize),
        )),

        ExecuteResult::Update { affected_rows } => Ok(Response::Execution(
            Tag::new("UPDATE").with_rows(affected_rows as usize),
        )),

        ExecuteResult::ShowTables { tables } => {
            let fields = vec![FieldInfo::new(
                "table_name".to_string(),
                None,
                None,
                Type::TEXT,
                FieldFormat::Text,
            )];
            let fields = Arc::new(fields);

            let mut data_rows: Vec<PgWireResult<DataRow>> = Vec::new();
            for table in tables {
                let mut encoder = DataRowEncoder::new(fields.clone());
                encoder.encode_field(&table)?;
                data_rows.push(encoder.finish());
            }

            let row_stream = stream::iter(data_rows);
            let results = QueryResponse::new(fields, row_stream);

            Ok(Response::Query(results))
        }

        ExecuteResult::CommandComplete { tag } => Ok(Response::Execution(Tag::new(tag))),

        ExecuteResult::TransactionStart { tag } => Ok(Response::TransactionStart(Tag::new(tag))),

        ExecuteResult::TransactionEnd { tag } => Ok(Response::TransactionEnd(Tag::new(tag))),

        ExecuteResult::Empty => Ok(Response::EmptyQuery),

        ExecuteResult::Notice { .. } => Ok(Response::Execution(Tag::new("DO"))),

        ExecuteResult::CreateRole => Ok(Response::Execution(Tag::new("CREATE ROLE"))),

        ExecuteResult::AlterRole => Ok(Response::Execution(Tag::new("ALTER ROLE"))),

        ExecuteResult::DropRole => Ok(Response::Execution(Tag::new("DROP ROLE"))),

        ExecuteResult::Grant => Ok(Response::Execution(Tag::new("GRANT"))),

        ExecuteResult::Revoke => Ok(Response::Execution(Tag::new("REVOKE"))),

        ExecuteResult::SelectStream { .. } => Err(PgWireError::UserError(Box::new(
            pgwire::error::ErrorInfo::new(
                "ERROR".to_string(),
                "0A000".to_string(),
                "streaming result cannot be sent directly to client".to_string(),
            ),
        ))),
    }
}
