//! COPY TO STDOUT response building.

use super::super::super::errors::{
    error_info, pg_error_message, sqlstate_for_executor_error, user_error,
};
use super::super::DynamicPgHandler;
use crate::sql::ExecuteResult;
use futures::{Sink, SinkExt};
use pgwire::api::results::CopyResponse;
use pgwire::api::ClientInfo;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::PgWireBackendMessage;
use std::fmt::Debug;

fn map_copy_to_executor_error(err: anyhow::Error) -> PgWireError {
    let sqlstate = sqlstate_for_executor_error(&err);
    user_error(sqlstate, pg_error_message(&err, sqlstate))
}

impl DynamicPgHandler {
    #[allow(clippy::result_large_err)]
    fn copy_out_response_from_select_result(
        result: ExecuteResult,
    ) -> Result<(CopyResponse, Vec<String>, Vec<crate::model::Row>), ErrorInfo> {
        match result {
            ExecuteResult::Select { columns, rows, .. } => {
                let col_count = columns.len();
                let column_formats: Vec<i16> = vec![0; col_count];
                Ok((
                    CopyResponse::new(0, col_count, column_formats),
                    columns,
                    rows,
                ))
            }
            ExecuteResult::SelectStream { .. } => Err(error_info(
                "0A000",
                "streaming result cannot be used in COPY context",
            )),
            _ => Err(error_info(
                "0A000",
                "COPY TO STDOUT is only supported for tables",
            )),
        }
    }

    pub(in crate::protocol::handler) async fn handle_copy_to_stdout<'a, C>(
        &self,
        client: &mut C,
        table_name: &str,
        columns: &[String],
        copy_opts: &crate::protocol::copy_format::CopyOptions,
    ) -> PgWireResult<Vec<pgwire::api::results::Response<'a>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let state = self.auth();
        let executor = &state.executor;

        let select_sql = if columns.is_empty() {
            format!("SELECT * FROM {}", table_name)
        } else {
            format!("SELECT {} FROM {}", columns.join(", "), table_name)
        };

        let mut session = state.session.lock().await;

        if self.cancel_token.is_cancelled() {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                "25P03".to_string(),
                "terminating connection due to idle-in-transaction timeout".to_string(),
            ))));
        }

        if let Err(e) = session.check_idle_in_transaction_timeout() {
            let _ = session.rollback().await;
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "FATAL".to_string(),
                e.sqlstate().to_string(),
                e.to_string(),
            ))));
        }

        let result = executor
            .execute(&mut session, &select_sql)
            .await
            .map(|r| r.last())
            .map_err(map_copy_to_executor_error)?;

        let (copy_resp, col_names, rows) = Self::copy_out_response_from_select_result(result)
            .map_err(|e| PgWireError::UserError(Box::new(e)))?;

        drop(session);

        pgwire::api::copy::send_copy_out_response(client, copy_resp).await?;

        let mut buf = Vec::with_capacity(4096);

        // Emit HEADER row if requested, encoding through format-specific logic
        // so column names containing delimiter/quote/newline are handled correctly.
        if copy_opts.header {
            let header_values: Vec<crate::model::Value> = col_names
                .iter()
                .map(|name| crate::model::Value::Text(name.clone()))
                .collect();
            crate::protocol::copy_format::encode_row_with_options(
                &header_values,
                &mut buf,
                copy_opts,
            )
            .map_err(map_copy_to_executor_error)?;
            let data = pgwire::messages::copy::CopyData::new(bytes::Bytes::copy_from_slice(&buf));
            client.send(PgWireBackendMessage::CopyData(data)).await?;
        }

        for row in &rows {
            buf.clear();
            crate::protocol::copy_format::encode_row_with_options(&row.values, &mut buf, copy_opts)
                .map_err(map_copy_to_executor_error)?;
            let data = pgwire::messages::copy::CopyData::new(bytes::Bytes::copy_from_slice(&buf));
            client.send(PgWireBackendMessage::CopyData(data)).await?;
        }

        let done = pgwire::messages::copy::CopyDone::new();
        client.send(PgWireBackendMessage::CopyDone(done)).await?;

        let complete =
            pgwire::messages::response::CommandComplete::new(format!("COPY {}", rows.len()));
        client
            .send(PgWireBackendMessage::CommandComplete(complete))
            .await?;

        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::error::SqlError;

    #[test]
    fn copy_to_executor_error_mapping_preserves_tenant_quota_sqlstate() {
        let err: anyhow::Error = SqlError::TenantMemoryQuotaExceeded {
            component: "copy_to".to_string(),
            requested_bytes: 256,
            used_bytes: 1024,
            quota_bytes: 1024,
        }
        .into();
        let mapped = map_copy_to_executor_error(err);
        match mapped {
            PgWireError::UserError(info) => assert_eq!(info.code, "53200"),
            other => panic!("expected user error, got {other:?}"),
        }
    }
}
