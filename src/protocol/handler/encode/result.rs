use super::types::datatype_to_pgtype;
use super::value::encode_value;
use crate::sql::ExecuteResult;
use crate::types::{DataType, Value};
use futures::stream;
use pgwire::api::results::{DataRowEncoder, FieldFormat, FieldInfo, QueryResponse, Response, Tag};
use pgwire::api::Type;
use pgwire::error::PgWireResult;
use pgwire::messages::data::DataRow;
use std::sync::Arc;

pub(in crate::protocol::handler) fn result_to_response(
    result: ExecuteResult,
) -> PgWireResult<Response<'static>> {
    match result {
        ExecuteResult::Select {
            columns,
            column_types,
            rows,
            timezone,
        } => {
            let tz = crate::types::timestamp::TimeZoneSpec::parse(timezone.as_ref());
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

            let fixed_columns: Vec<String> = if columns.len() == 1 && columns[0] == "?column?" {
                if let Some(first_row) = rows.first() {
                    if let Some(first_val) = first_row.values.first() {
                        match first_val {
                            Value::Text(s) if s.starts_with("PostgreSQL") => {
                                vec!["version".to_string()]
                            }
                            Value::Text(s) if s == "postgres" || !s.contains(' ') => {
                                vec!["?column?".to_string()]
                            }
                            _ => columns,
                        }
                    } else {
                        columns
                    }
                } else {
                    columns
                }
            } else {
                columns
            };

            let fields: Vec<FieldInfo> = fixed_columns
                .iter()
                .enumerate()
                .map(|(i, name)| {
                    let pg_type = inferred_types.get(i).cloned().unwrap_or(Type::TEXT);
                    FieldInfo::new(name.clone(), None, None, pg_type, FieldFormat::Text)
                })
                .collect();

            let fields = Arc::new(fields);

            let internal_types: Vec<DataType> = if let Some(types) = column_types.as_ref() {
                types.clone()
            } else {
                // INTENTIONAL: wire protocol encoding — Text OID is universally safe
                crate::types::infer_column_types_from_rows(&rows, fixed_columns.len())
            };

            let mut data_rows: Vec<PgWireResult<DataRow>> = Vec::new();
            for row in rows {
                let mut encoder = DataRowEncoder::new(fields.clone());
                for (i, value) in row.values.iter().enumerate() {
                    let col_type = internal_types.get(i);
                    encode_value(&mut encoder, value, col_type, tz)?;
                }
                data_rows.push(encoder.finish());
            }

            let row_stream = stream::iter(data_rows);
            let results = QueryResponse::new(fields, row_stream);

            Ok(Response::Query(results))
        }

        ExecuteResult::CreateTable { .. } => Ok(Response::Execution(Tag::new("CREATE TABLE"))),

        ExecuteResult::DropTable { .. } => Ok(Response::Execution(Tag::new("DROP TABLE"))),

        ExecuteResult::TruncateTable { .. } => Ok(Response::Execution(Tag::new("TRUNCATE TABLE"))),

        ExecuteResult::CreateIndex { .. } => Ok(Response::Execution(Tag::new("CREATE INDEX"))),

        ExecuteResult::DropIndex { .. } => Ok(Response::Execution(Tag::new("DROP INDEX"))),

        ExecuteResult::CreateView { .. } => Ok(Response::Execution(Tag::new("CREATE VIEW"))),

        ExecuteResult::DropView { .. } => Ok(Response::Execution(Tag::new("DROP VIEW"))),

        ExecuteResult::CreateMaterializedView { .. } => {
            Ok(Response::Execution(Tag::new("CREATE MATERIALIZED VIEW")))
        }

        ExecuteResult::DropMaterializedView { .. } => {
            Ok(Response::Execution(Tag::new("DROP MATERIALIZED VIEW")))
        }

        ExecuteResult::RefreshMaterializedView { .. } => {
            Ok(Response::Execution(Tag::new("REFRESH MATERIALIZED VIEW")))
        }

        ExecuteResult::CreateProcedure { .. } => {
            Ok(Response::Execution(Tag::new("CREATE PROCEDURE")))
        }

        ExecuteResult::DropProcedure { .. } => Ok(Response::Execution(Tag::new("DROP PROCEDURE"))),

        ExecuteResult::CreateFunction { .. } => {
            Ok(Response::Execution(Tag::new("CREATE FUNCTION")))
        }

        ExecuteResult::DropFunction { .. } => Ok(Response::Execution(Tag::new("DROP FUNCTION"))),

        ExecuteResult::CreateTrigger { .. } => Ok(Response::Execution(Tag::new("CREATE TRIGGER"))),

        ExecuteResult::DropTrigger { .. } => Ok(Response::Execution(Tag::new("DROP TRIGGER"))),

        ExecuteResult::CreateExtension { .. } => {
            Ok(Response::Execution(Tag::new("CREATE EXTENSION")))
        }

        ExecuteResult::DropExtension { .. } => Ok(Response::Execution(Tag::new("DROP EXTENSION"))),

        ExecuteResult::Call => Ok(Response::Execution(Tag::new("CALL"))),

        ExecuteResult::AlterTable { .. } => Ok(Response::Execution(Tag::new("ALTER TABLE"))),

        ExecuteResult::AlterSequence { .. } => Ok(Response::Execution(Tag::new("ALTER SEQUENCE"))),

        ExecuteResult::AlterFunction { .. } => Ok(Response::Execution(Tag::new("ALTER FUNCTION"))),

        ExecuteResult::AlterIndex { .. } => Ok(Response::Execution(Tag::new("ALTER INDEX"))),

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

        ExecuteResult::Describe { schema } => {
            let fields = vec![
                FieldInfo::new(
                    "column_name".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                ),
                FieldInfo::new(
                    "data_type".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                ),
                FieldInfo::new(
                    "nullable".to_string(),
                    None,
                    None,
                    Type::BOOL,
                    FieldFormat::Text,
                ),
                FieldInfo::new(
                    "primary_key".to_string(),
                    None,
                    None,
                    Type::BOOL,
                    FieldFormat::Text,
                ),
                FieldInfo::new(
                    "default".to_string(),
                    None,
                    None,
                    Type::TEXT,
                    FieldFormat::Text,
                ),
            ];
            let fields = Arc::new(fields);

            let mut data_rows: Vec<PgWireResult<DataRow>> = Vec::new();
            for col in &schema.columns {
                let mut encoder = DataRowEncoder::new(fields.clone());
                encoder.encode_field(&col.name)?;
                encoder.encode_field(&col.data_type.to_string())?;
                encoder.encode_field(&col.nullable)?;
                encoder.encode_field(&col.primary_key)?;

                let default_val = if col.is_serial {
                    Some("SERIAL (AUTO_INC)".to_string())
                } else {
                    col.default_expr.clone()
                };
                encoder.encode_field(&default_val)?;

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
    }
}
