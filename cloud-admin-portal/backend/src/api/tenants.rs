use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use base64::Engine;
use rand::Rng;

use crate::auth::{ApiKeyAuth, TenantSessionExtractor};
use crate::db;
use crate::error::AppError;
use crate::models::*;
use crate::services::pd_client::PdClient;
use crate::services::pg_client::PgClient;
use crate::{
    tenant_state, AppState, DEFAULT_ADMIN_PASSWORD, DEFAULT_ADMIN_USER, DEFAULT_PG_PORT,
    OBSERVABILITY_USER, TENANT_ID_LEN,
};

fn encode_cursor(created_at: &str, id: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!("{created_at}|{id}"))
}

fn decode_cursor(cursor: &str) -> Option<(String, String)> {
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .ok()?;
    let s = String::from_utf8(bytes).ok()?;
    let parts: Vec<&str> = s.splitn(2, '|').collect();
    if parts.len() == 2 {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

fn generate_tenant_id() -> String {
    let mut rng = rand::thread_rng();
    let charset = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..TENANT_ID_LEN)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

fn make_keyspace(id: &str) -> String {
    format!("{}{id}", crate::KEYSPACE_PREFIX)
}

fn generate_password() -> String {
    let mut rng = rand::thread_rng();
    let charset = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!@#$%^&*";
    (0..16)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

// ── List tenants ─────────────────────────────────────────────────

pub async fn list_tenants(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Query(params): Query<ListTenantsParams>,
) -> Result<Json<TenantListResponse>, AppError> {
    let page = params.page.unwrap_or(1);
    let size = params.size.unwrap_or(50);

    let cursor_pair = params.cursor.as_deref().and_then(decode_cursor);
    let cursor_ref = cursor_pair
        .as_ref()
        .map(|(ts, id)| (ts.as_str(), id.as_str()));

    let opts = db::ListTenantsOpts {
        page,
        size,
        state_filter: params.state.as_deref(),
        q: params.q.as_deref(),
        tag: params.tag.as_deref(),
        cursor: cursor_ref,
    };

    let result = db::list_tenants(&state.db, &opts).await?;
    let items: Vec<TenantResponse> = result.tenants.iter().map(|t| t.to_response()).collect();
    let next_cursor = result.next_cursor.map(|(ts, id)| encode_cursor(&ts, &id));

    Ok(Json(TenantListResponse {
        items,
        total: result.total,
        page,
        size,
        next_cursor,
    }))
}

// ── Create tenant ────────────────────────────────────────────────

pub async fn create_tenant(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Json(request): Json<CreateTenantRequest>,
) -> Result<(StatusCode, Json<CreateTenantResponse>), AppError> {
    let tenant_id = generate_tenant_id();
    let keyspace = make_keyspace(&tenant_id);
    let admin_user = request
        .admin_user
        .unwrap_or_else(|| DEFAULT_ADMIN_USER.to_string());
    let password = request.admin_password.unwrap_or_else(generate_password);

    // Check for ID collision
    if db::get_tenant_by_id(&state.db, &tenant_id).await?.is_some() {
        return Err(AppError::conflict("ID collision, please retry"));
    }

    let now = chrono::Utc::now().to_rfc3339();
    db::insert_tenant(
        &state.db,
        &tenant_id,
        &keyspace,
        tenant_state::CREATING,
        &now,
    )
    .await?;

    let pd = PdClient::new(&state.config.pd_endpoints, &state.http_client);
    if !pd.create_keyspace(&keyspace).await {
        db::update_tenant_state(
            &state.db,
            &tenant_id,
            tenant_state::CREATE_FAILED,
            Some("Failed to create keyspace in PD"),
        )
        .await?;
        db::insert_audit_log(
            &state.db,
            "CREATE",
            "TENANT",
            &tenant_id,
            Some(&tenant_id),
            None,
            false,
            Some("Failed to create keyspace in PD"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal("Failed to create keyspace in TiKV"));
    }

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    if !pg
        .bootstrap_admin_password(&tenant_id, &admin_user, DEFAULT_ADMIN_PASSWORD, &password)
        .await
    {
        db::update_tenant_state(
            &state.db,
            &tenant_id,
            tenant_state::CREATE_FAILED,
            Some("Keyspace created but password bootstrap failed"),
        )
        .await?;
        db::insert_audit_log(
            &state.db,
            "CREATE",
            "TENANT",
            &tenant_id,
            Some(&tenant_id),
            None,
            false,
            Some("Password bootstrap failed"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal(
            "Failed to set admin password. Keyspace created but password unchanged.",
        ));
    }

    pg.bootstrap_default_extensions(&tenant_id, &admin_user, &password)
        .await;

    db::update_tenant_state(&state.db, &tenant_id, tenant_state::ACTIVE, None).await?;
    db::insert_audit_log(
        &state.db,
        "CREATE",
        "TENANT",
        &tenant_id,
        Some(&tenant_id),
        None,
        true,
        None,
        None,
    )
    .await
    .ok();

    let endpoints = state.config.parse_public_endpoints();
    let (host, port) = endpoints
        .first()
        .cloned()
        .unwrap_or_else(|| ("127.0.0.1".into(), DEFAULT_PG_PORT));

    let connection_string =
        format!("postgresql://{tenant_id}.{admin_user}:{password}@{host}:{port}/postgres");

    Ok((
        StatusCode::CREATED,
        Json(CreateTenantResponse {
            id: tenant_id,
            admin_user: admin_user.clone(),
            admin_password: password.clone(),
            connection_string,
            created_at: now,
        }),
    ))
}

// ── Get tenant ───────────────────────────────────────────────────

pub async fn get_tenant(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
) -> Result<Json<TenantResponse>, AppError> {
    let tenant = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    let endpoint_tuples = state.config.parse_public_endpoints();
    let endpoints: Vec<Endpoint> = endpoint_tuples
        .iter()
        .enumerate()
        .map(|(i, (host, port))| Endpoint {
            host: host.clone(),
            port: *port,
            ep_type: if endpoint_tuples.len() > 1 {
                "load_balancer".into()
            } else {
                "primary".into()
            },
            region: None,
            priority: 100 - (i as i32) * 10,
            description: if endpoint_tuples.len() > 1 {
                Some(format!("pg-tikv endpoint {}", i + 1))
            } else {
                Some("pg-tikv primary endpoint".into())
            },
            enabled: true,
        })
        .collect();

    let mut resp = tenant.to_response();
    resp.endpoints = Some(endpoints);
    Ok(Json(resp))
}

// ── Delete tenant ────────────────────────────────────────────────

pub async fn delete_tenant(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
) -> Result<Json<MessageResponse>, AppError> {
    let tenant = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    db::update_tenant_state(&state.db, &tenant_id, tenant_state::DISABLING, None).await?;

    let pd = PdClient::new(&state.config.pd_endpoints, &state.http_client);
    if !pd.disable_keyspace(&tenant.keyspace).await {
        db::update_tenant_state(
            &state.db,
            &tenant_id,
            tenant_state::ACTIVE,
            Some("Failed to disable keyspace in PD"),
        )
        .await?;
        db::insert_audit_log(
            &state.db,
            "DELETE",
            "TENANT",
            &tenant_id,
            Some(&tenant_id),
            None,
            false,
            Some("Failed to disable keyspace"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal("Failed to disable tenant"));
    }

    db::update_tenant_state(
        &state.db,
        &tenant_id,
        tenant_state::DISABLED,
        Some("Deleted via API"),
    )
    .await?;
    db::insert_audit_log(
        &state.db,
        "DELETE",
        "TENANT",
        &tenant_id,
        Some(&tenant_id),
        None,
        true,
        None,
        None,
    )
    .await
    .ok();

    Ok(Json(MessageResponse {
        message: format!("Tenant '{tenant_id}' disabled"),
    }))
}

// ── Remove tenant ────────────────────────────────────────────────

pub async fn remove_tenant(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
) -> Result<Json<MessageResponse>, AppError> {
    let tenant = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    db::update_tenant_state(&state.db, &tenant_id, tenant_state::DISABLING, None).await?;

    let pd = PdClient::new(&state.config.pd_endpoints, &state.http_client);
    if !pd.disable_keyspace(&tenant.keyspace).await {
        db::update_tenant_state(
            &state.db,
            &tenant_id,
            tenant_state::ACTIVE,
            Some("Failed to disable keyspace in PD"),
        )
        .await?;
        db::insert_audit_log(
            &state.db,
            "DELETE",
            "TENANT",
            &tenant_id,
            Some(&tenant_id),
            None,
            false,
            Some("Failed to disable keyspace"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal("Failed to disable keyspace in TiKV"));
    }

    db::update_tenant_state(
        &state.db,
        &tenant_id,
        tenant_state::DISABLED,
        Some("Removed via portal"),
    )
    .await?;
    db::insert_audit_log(
        &state.db,
        "DELETE",
        "TENANT",
        &tenant_id,
        Some(&tenant_id),
        None,
        true,
        None,
        None,
    )
    .await
    .ok();

    Ok(Json(MessageResponse {
        message: format!("Tenant '{tenant_id}' removed from portal"),
    }))
}

// ── Update tenant ────────────────────────────────────────────────

pub async fn update_tenant(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
    Json(request): Json<TenantUpdateRequest>,
) -> Result<Json<TenantResponse>, AppError> {
    let _tenant = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    let tags_json = request
        .tags
        .as_ref()
        .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".into()));

    db::update_tenant_metadata(
        &state.db,
        &tenant_id,
        request.notes.as_deref(),
        tags_json.as_deref(),
    )
    .await?;

    db::insert_audit_log(
        &state.db,
        "UPDATE",
        "TENANT",
        &tenant_id,
        Some(&tenant_id),
        None,
        true,
        None,
        None,
    )
    .await
    .ok();

    let updated = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::internal("Tenant disappeared after update"))?;

    Ok(Json(updated.to_response()))
}

// ── Connect tenant ───────────────────────────────────────────────

pub async fn connect_tenant(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
    Json(request): Json<TenantConnectRequest>,
) -> Result<Json<TenantConnectResponse>, AppError> {
    let tenant = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    if tenant.state == tenant_state::SUSPENDED {
        return Err(AppError::forbidden("Tenant is suspended"));
    }

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    if !pg
        .test_connection(&tenant_id, &request.admin_user, &request.admin_password)
        .await
    {
        return Err(AppError::unauthorized("Invalid tenant credentials"));
    }

    let session =
        state
            .sessions
            .create_session(&tenant_id, &request.admin_user, &request.admin_password);

    Ok(Json(TenantConnectResponse {
        session_id: session.session_id,
        expires_at: session.expires_at,
    }))
}

// ── Execute query ────────────────────────────────────────────────

pub async fn execute_query(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<SqlQueryRequest>,
) -> Result<Json<SqlQueryResponse>, AppError> {
    let session = TenantSessionExtractor::from_headers(&headers, &tenant_id, &state)?;

    let sql = request.sql.trim().to_string();
    if sql.is_empty() {
        return Ok(Json(SqlQueryResponse {
            success: false,
            result: None,
            error: Some("Empty SQL query".into()),
        }));
    }

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    match pg
        .run_sql(
            &session.tenant_id,
            &session.admin_user,
            &session.admin_password,
            &sql,
        )
        .await
    {
        Ok(output) => Ok(Json(SqlQueryResponse {
            success: true,
            result: Some(output),
            error: None,
        })),
        Err(e) => Ok(Json(SqlQueryResponse {
            success: false,
            result: None,
            error: Some(e),
        })),
    }
}

// ── Get observability ────────────────────────────────────────────

pub async fn get_observability(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
) -> Result<Json<TenantObservabilityResponse>, AppError> {
    db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    let cred = db::get_credential(
        &state.db,
        &tenant_id,
        "OBSERVABILITY",
        state.config.credential_key.as_deref(),
    )
    .await?
    .ok_or_else(|| {
        AppError::new(
            StatusCode::CONFLICT,
            "Observability account not bootstrapped for this tenant",
        )
    })?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);

    let summary_val = pg
        .get_observability_summary(&tenant_id, &cred.username, &cred.password_plain)
        .await
        .map_err(|e| {
            tracing::warn!("observability query failed for tenant {tenant_id}: {e}");
            AppError::new(
                StatusCode::CONFLICT,
                "Observer credential may be stale. Please re-bootstrap observability.",
            )
        })?
        .ok_or_else(|| AppError::bad_gateway("Failed to fetch observability summary"))?;

    let summary = ObservabilitySummary {
        window_seconds: summary_val["window_seconds"].as_i64().unwrap_or(0),
        statement_count: summary_val["statement_count"].as_i64().unwrap_or(0),
        txn_commit_count: summary_val["txn_commit_count"].as_i64().unwrap_or(0),
        error_count: summary_val["error_count"].as_i64().unwrap_or(0),
        qps: summary_val["qps"].as_f64().unwrap_or(0.0),
        tps: summary_val["tps"].as_f64().unwrap_or(0.0),
        latency_avg_ms: summary_val["latency_avg_ms"].as_f64().unwrap_or(0.0),
        latency_p99_ms: summary_val["latency_p99_ms"].as_f64().unwrap_or(0.0),
        active_connections: summary_val["active_connections"].as_i64().unwrap_or(0),
    };

    let samples_val = pg
        .get_observability_samples(&tenant_id, &cred.username, &cred.password_plain)
        .await;

    let samples: Vec<QuerySample> = samples_val
        .iter()
        .map(|s| QuerySample {
            query: s["query"].as_str().unwrap_or("").to_string(),
            sample_count: s["sample_count"].as_i64().unwrap_or(0),
            error_count: s["error_count"].as_i64().unwrap_or(0),
            latency_avg_ms: s["latency_avg_ms"].as_f64().unwrap_or(0.0),
            latency_p99_ms: s["latency_p99_ms"].as_f64().unwrap_or(0.0),
            latency_max_ms: s["latency_max_ms"].as_f64().unwrap_or(0.0),
            last_seen_ms_ago: s["last_seen_ms_ago"].as_i64().unwrap_or(0),
        })
        .collect();

    Ok(Json(TenantObservabilityResponse { summary, samples }))
}

// ── Bootstrap observability ──────────────────────────────────────

pub async fn bootstrap_observability(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
    Json(request): Json<TenantConnectRequest>,
) -> Result<Json<MessageResponse>, AppError> {
    db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    if !pg
        .test_connection(&tenant_id, &request.admin_user, &request.admin_password)
        .await
    {
        return Err(AppError::unauthorized("Invalid tenant credentials"));
    }

    let obs_password = generate_password();

    let created = pg
        .create_user(
            &tenant_id,
            &request.admin_user,
            &request.admin_password,
            OBSERVABILITY_USER,
            &obs_password,
            false,
        )
        .await;

    if !created {
        let rotated = pg
            .reset_password(
                &tenant_id,
                &request.admin_user,
                &request.admin_password,
                OBSERVABILITY_USER,
                &obs_password,
            )
            .await;
        if !rotated {
            return Err(AppError::bad_gateway(
                "Failed to create or rotate observability account",
            ));
        }
    }

    db::upsert_credential(
        &state.db,
        &tenant_id,
        "OBSERVABILITY",
        OBSERVABILITY_USER,
        &obs_password,
        state.config.credential_key.as_deref(),
    )
    .await?;

    Ok(Json(MessageResponse {
        message: format!(
            "Observability account '{OBSERVABILITY_USER}' bootstrapped for tenant '{tenant_id}'"
        ),
    }))
}

// ── Batch create tenants ─────────────────────────────────────────

pub async fn batch_create_tenants(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Json(request): Json<BatchCreateRequest>,
) -> Result<(StatusCode, Json<BatchCreateResponse>), AppError> {
    if request.count == 0 || request.count > 1000 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "count must be between 1 and 1000",
        ));
    }

    let admin_user = request
        .admin_user
        .unwrap_or_else(|| DEFAULT_ADMIN_USER.to_string());
    let mut created = Vec::new();
    let mut failed = Vec::new();

    for _ in 0..request.count {
        let tenant_id = generate_tenant_id();
        let keyspace = make_keyspace(&tenant_id);
        let password = request
            .admin_password
            .clone()
            .unwrap_or_else(generate_password);

        if db::get_tenant_by_id(&state.db, &tenant_id).await?.is_some() {
            failed.push(BatchItemError {
                id: tenant_id,
                error: "ID collision".into(),
            });
            continue;
        }

        let now = chrono::Utc::now().to_rfc3339();
        if let Err(e) = db::insert_tenant(
            &state.db,
            &tenant_id,
            &keyspace,
            tenant_state::CREATING,
            &now,
        )
        .await
        {
            failed.push(BatchItemError {
                id: tenant_id,
                error: format!("DB insert failed: {e}"),
            });
            continue;
        }

        let pd = PdClient::new(&state.config.pd_endpoints, &state.http_client);
        if !pd.create_keyspace(&keyspace).await {
            db::update_tenant_state(
                &state.db,
                &tenant_id,
                tenant_state::CREATE_FAILED,
                Some("PD keyspace creation failed"),
            )
            .await
            .ok();
            failed.push(BatchItemError {
                id: tenant_id,
                error: "Failed to create keyspace in PD".into(),
            });
            continue;
        }

        let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
        if !pg
            .bootstrap_admin_password(&tenant_id, &admin_user, DEFAULT_ADMIN_PASSWORD, &password)
            .await
        {
            db::update_tenant_state(
                &state.db,
                &tenant_id,
                tenant_state::CREATE_FAILED,
                Some("Password bootstrap failed"),
            )
            .await
            .ok();
            failed.push(BatchItemError {
                id: tenant_id,
                error: "Password bootstrap failed".into(),
            });
            continue;
        }

        db::update_tenant_state(&state.db, &tenant_id, tenant_state::ACTIVE, None)
            .await
            .ok();
        db::insert_audit_log(
            &state.db,
            "CREATE",
            "TENANT",
            &tenant_id,
            Some(&tenant_id),
            None,
            true,
            None,
            None,
        )
        .await
        .ok();

        let endpoints = state.config.parse_public_endpoints();
        let (host, port) = endpoints
            .first()
            .cloned()
            .unwrap_or_else(|| ("127.0.0.1".into(), DEFAULT_PG_PORT));
        let connection_string =
            format!("postgresql://{tenant_id}.{admin_user}:{password}@{host}:{port}/postgres");

        created.push(CreateTenantResponse {
            id: tenant_id,
            admin_user: admin_user.clone(),
            admin_password: password,
            connection_string,
            created_at: now,
        });
    }

    let total_created = created.len() as u32;
    Ok((
        StatusCode::CREATED,
        Json(BatchCreateResponse {
            created,
            failed,
            total_requested: request.count,
            total_created,
        }),
    ))
}

// ── Batch delete tenants ─────────────────────────────────────────

pub async fn batch_delete_tenants(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Json(request): Json<BatchDeleteRequest>,
) -> Result<Json<BatchDeleteResponse>, AppError> {
    if request.ids.is_empty() || request.ids.len() > 1000 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "ids must contain 1 to 1000 entries",
        ));
    }

    let mut deleted = Vec::new();
    let mut failed = Vec::new();

    for tenant_id in &request.ids {
        let tenant = match db::get_tenant(&state.db, tenant_id).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                failed.push(BatchItemError {
                    id: tenant_id.clone(),
                    error: "Tenant not found".into(),
                });
                continue;
            }
            Err(e) => {
                failed.push(BatchItemError {
                    id: tenant_id.clone(),
                    error: format!("DB error: {e}"),
                });
                continue;
            }
        };

        db::update_tenant_state(&state.db, tenant_id, tenant_state::DISABLING, None)
            .await
            .ok();

        let pd = PdClient::new(&state.config.pd_endpoints, &state.http_client);
        if !pd.disable_keyspace(&tenant.keyspace).await {
            db::update_tenant_state(
                &state.db,
                tenant_id,
                tenant_state::ACTIVE,
                Some("Failed to disable keyspace in PD"),
            )
            .await
            .ok();
            failed.push(BatchItemError {
                id: tenant_id.clone(),
                error: "Failed to disable keyspace in PD".into(),
            });
            continue;
        }

        db::update_tenant_state(
            &state.db,
            tenant_id,
            tenant_state::DISABLED,
            Some("Batch delete"),
        )
        .await
        .ok();
        db::insert_audit_log(
            &state.db,
            "DELETE",
            "TENANT",
            tenant_id,
            Some(tenant_id),
            None,
            true,
            None,
            None,
        )
        .await
        .ok();
        deleted.push(tenant_id.clone());
    }

    Ok(Json(BatchDeleteResponse { deleted, failed }))
}

// ── Batch update tenants ─────────────────────────────────────────

pub async fn batch_update_tenants(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Json(request): Json<BatchUpdateRequest>,
) -> Result<Json<BatchUpdateResponse>, AppError> {
    if request.ids.is_empty() || request.ids.len() > 1000 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "ids must contain 1 to 1000 entries",
        ));
    }

    let tags_json = request
        .tags
        .as_ref()
        .map(|t| serde_json::to_string(t).unwrap_or_else(|_| "[]".into()));

    let existing_refs: Vec<&str> = request.ids.iter().map(|s| s.as_str()).collect();
    let existing = db::check_tenants_exist(&state.db, &existing_refs).await?;

    let mut updated = Vec::new();
    let mut failed = Vec::new();

    for id in &request.ids {
        if !existing.contains(id) {
            failed.push(BatchItemError {
                id: id.clone(),
                error: "Tenant not found".into(),
            });
        } else {
            updated.push(id.clone());
        }
    }

    if !updated.is_empty() {
        db::batch_update_metadata(
            &state.db,
            &updated,
            request.notes.as_deref(),
            tags_json.as_deref(),
        )
        .await?;

        for id in &updated {
            db::insert_audit_log(
                &state.db,
                "UPDATE",
                "TENANT",
                id,
                Some(id),
                None,
                true,
                None,
                None,
            )
            .await
            .ok();
        }
    }

    Ok(Json(BatchUpdateResponse { updated, failed }))
}
