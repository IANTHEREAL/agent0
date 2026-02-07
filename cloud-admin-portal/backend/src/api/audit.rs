use axum::extract::{Query, State};
use axum::Json;

use crate::auth::ApiKeyAuth;
use crate::db;
use crate::error::AppError;
use crate::models::{AuditLogParams, AuditLogResponse};
use crate::AppState;

pub async fn query_audit_logs(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Query(params): Query<AuditLogParams>,
) -> Result<Json<Vec<AuditLogResponse>>, AppError> {
    let logs = db::query_audit_logs(
        &state.db,
        params.tenant_id.as_deref(),
        params.operation_type.as_deref(),
        params.resource_type.as_deref(),
        params.success,
        params.limit.unwrap_or(100),
        params.offset.unwrap_or(0),
    )
    .await?;

    Ok(Json(logs))
}
