use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rand::Rng;
use sha2::{Digest, Sha256};

use crate::auth::CustomerAuth;
use crate::db;
use crate::error::AppError;
use crate::models::*;
use crate::services::pd_client::PdClient;
use crate::services::pg_client::PgClient;
use crate::{
    tenant_state, AppState, DEFAULT_ADMIN_PASSWORD, DEFAULT_ADMIN_USER, DEFAULT_PG_PORT,
    KEYSPACE_PREFIX, OBSERVABILITY_USER, TENANT_ID_LEN,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/me", get(me))
        .route("/tokens", get(list_tokens))
        .route("/tokens/:token_id", delete(revoke_token))
        .route("/databases", post(create_database).get(list_databases))
        .route(
            "/databases/:database_id",
            get(get_database).delete(delete_database),
        )
        .route(
            "/databases/:database_id/reset-password",
            post(reset_database_password),
        )
        .route(
            "/databases/:database_id/observability",
            get(get_database_observability),
        )
}

// ── Helper functions ────────────────────────────────────────────

fn generate_tenant_id() -> String {
    let mut rng = rand::thread_rng();
    let charset = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..TENANT_ID_LEN)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

fn make_keyspace(id: &str) -> String {
    format!("{}{id}", KEYSPACE_PREFIX)
}

fn generate_password() -> String {
    let mut rng = rand::thread_rng();
    let charset = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_=+.~";
    (0..16)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

fn build_connection_string(
    tenant_id: &str,
    user: &str,
    password: &str,
    host: &str,
    port: u16,
) -> String {
    format!("postgresql://{tenant_id}.{user}:{password}@{host}:{port}/postgres")
}

fn parse_region_from_tags(tags: &Option<String>) -> Option<String> {
    tags.as_ref()
        .and_then(|t| serde_json::from_str::<Vec<String>>(t).ok())
        .and_then(|v| v.into_iter().next())
}

// ── POST /register ───────────────────────────────────────────────

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<RegisterRequest>,
) -> Result<(StatusCode, Json<CustomerResponse>), AppError> {
    if !req.email.contains('@') || req.email.len() < 3 || req.email.len() > 254 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Invalid email address",
        ));
    }
    if req.password.len() < 8 {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Password must be at least 8 characters",
        ));
    }

    use argon2::{password_hash::SaltString, Argon2, PasswordHasher};
    use rand::rngs::OsRng;
    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(req.password.as_bytes(), &salt)
        .map_err(|e| AppError::internal(format!("Password hashing failed: {e}")))?
        .to_string();

    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    if db::get_customer_by_email(&state.db, &req.email)
        .await?
        .is_some()
    {
        return Err(AppError::conflict("Email already registered"));
    }

    db::create_customer(&state.db, &id, &req.email, &password_hash)
        .await
        .map_err(|e| {
            if e.to_string().contains("UNIQUE") {
                AppError::conflict("Email already registered")
            } else {
                AppError::from(e)
            }
        })?;

    Ok((
        StatusCode::CREATED,
        Json(CustomerResponse {
            id,
            email: req.email,
            created_at: now,
            status: "active".to_string(),
        }),
    ))
}

// ── POST /login ──────────────────────────────────────────────────

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<LoginRequest>,
) -> Result<Json<LoginResponse>, AppError> {
    let customer = db::get_customer_by_email(&state.db, &req.email)
        .await?
        .ok_or_else(|| AppError::unauthorized("Invalid email or password"))?;

    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    let parsed_hash = PasswordHash::new(&customer.password_hash)
        .map_err(|_| AppError::internal("Invalid stored password hash"))?;
    Argon2::default()
        .verify_password(req.password.as_bytes(), &parsed_hash)
        .map_err(|_| AppError::unauthorized("Invalid email or password"))?;

    use rand::RngCore;
    let mut token_bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut token_bytes);
    let token = token_bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let hash_bytes = Sha256::digest(token.as_bytes());
    let token_hash = hash_bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let expires_at = (chrono::Utc::now() + chrono::Duration::days(90)).to_rfc3339();
    let token_id = uuid::Uuid::new_v4().to_string();

    db::create_customer_token(&state.db, &token_id, &customer.id, &token_hash, "default", &expires_at)
        .await?;

    Ok(Json(LoginResponse { token, expires_at }))
}

// ── GET /me ──────────────────────────────────────────────────────

pub async fn me(
    State(state): State<AppState>,
    auth: CustomerAuth,
) -> Result<Json<CustomerResponse>, AppError> {
    let customer = db::get_customer_by_id(&state.db, &auth.customer_id)
        .await?
        .ok_or_else(|| AppError::not_found("Customer not found"))?;

    Ok(Json(CustomerResponse {
        id: customer.id,
        email: customer.email,
        created_at: customer.created_at,
        status: customer.status,
    }))
}

// ── GET /tokens ──────────────────────────────────────────────────

pub async fn list_tokens(
    State(state): State<AppState>,
    auth: CustomerAuth,
) -> Result<Json<Vec<TokenResponse>>, AppError> {
    let tokens = db::list_customer_tokens(&state.db, &auth.customer_id).await?;
    let responses: Vec<TokenResponse> = tokens
        .into_iter()
        .map(|t| TokenResponse {
            id: t.id,
            name: t.name,
            created_at: t.created_at,
            expires_at: t.expires_at,
        })
        .collect();
    Ok(Json(responses))
}

// ── DELETE /tokens/:token_id ─────────────────────────────────────

pub async fn revoke_token(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(token_id): Path<String>,
) -> Result<Json<MessageResponse>, AppError> {
    let deleted = db::delete_customer_token(&state.db, &token_id, &auth.customer_id).await?;
    if !deleted {
        return Err(AppError::not_found("Token not found"));
    }
    Ok(Json(MessageResponse {
        message: "Token revoked".to_string(),
    }))
}

// ── POST /databases ──────────────────────────────────────────────

pub async fn create_database(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Json(req): Json<CreateDatabaseRequest>,
) -> Result<(StatusCode, Json<DatabaseResponse>), AppError> {
    let tenant_id = generate_tenant_id();
    let keyspace = make_keyspace(&tenant_id);
    let admin_user = DEFAULT_ADMIN_USER.to_string();
    let password = generate_password();

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
            "DATABASE",
            &tenant_id,
            Some(&tenant_id),
            Some(&auth.customer_id),
            false,
            Some("Failed to create keyspace in PD"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal("Failed to create database"));
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
            "DATABASE",
            &tenant_id,
            Some(&tenant_id),
            Some(&auth.customer_id),
            false,
            Some("Password bootstrap failed"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal(
            "Failed to initialize database. Please retry.",
        ));
    }

    db::update_tenant_state(&state.db, &tenant_id, tenant_state::ACTIVE, None).await?;
    db::set_tenant_customer_id(&state.db, &tenant_id, &auth.customer_id).await?;

    db::upsert_credential(
        &state.db,
        &tenant_id,
        "admin",
        &admin_user,
        &password,
        state.config.credential_key.as_deref(),
    )
    .await
    .ok();

    let tags_json = req
        .region
        .as_ref()
        .map(|r| serde_json::to_string(&vec![r]).unwrap_or_else(|_| "[]".into()));
    db::update_tenant_metadata(
        &state.db,
        &tenant_id,
        Some(&req.name),
        tags_json.as_deref(),
    )
    .await?;

    db::insert_audit_log(
        &state.db,
        "CREATE",
        "DATABASE",
        &tenant_id,
        Some(&tenant_id),
        Some(&auth.customer_id),
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
        build_connection_string(&tenant_id, &admin_user, &password, &host, port);

    Ok((
        StatusCode::CREATED,
        Json(DatabaseResponse {
            id: tenant_id,
            name: req.name,
            state: tenant_state::ACTIVE.to_string(),
            region: req.region,
            endpoints: None,
            admin_user: Some(admin_user),
            admin_password: Some(password),
            created_at: now,
            connection_string: Some(connection_string),
        }),
    ))
}

// ── GET /databases ───────────────────────────────────────────────

pub async fn list_databases(
    State(state): State<AppState>,
    auth: CustomerAuth,
) -> Result<Json<Vec<DatabaseResponse>>, AppError> {
    let tenants = db::list_customer_tenants(&state.db, &auth.customer_id).await?;

    let databases: Vec<DatabaseResponse> = tenants
        .into_iter()
        .filter(|t| t.state != tenant_state::DISABLED && t.state != tenant_state::CREATE_FAILED)
        .map(|t| {
            let region = parse_region_from_tags(&t.tags);
            DatabaseResponse {
                id: t.id,
                name: t.notes.unwrap_or_default(),
                state: t.state,
                region,
                endpoints: None,
                admin_user: None,
                admin_password: None,
                created_at: t.created_at,
                connection_string: None,
            }
        })
        .collect();

    Ok(Json(databases))
}

// ── GET /databases/:database_id ──────────────────────────────────

pub async fn get_database(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
) -> Result<Json<DatabaseResponse>, AppError> {
    let tenant = db::get_tenant_for_customer(&state.db, &database_id, &auth.customer_id)
        .await?
        .ok_or_else(|| AppError::not_found("Database not found"))?;

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

    let (host, port) = endpoint_tuples
        .first()
        .cloned()
        .unwrap_or_else(|| ("127.0.0.1".into(), DEFAULT_PG_PORT));
    let connection_string = format!(
        "postgresql://{}.{}@{host}:{port}/postgres",
        tenant.id, DEFAULT_ADMIN_USER
    );

    let region = parse_region_from_tags(&tenant.tags);

    Ok(Json(DatabaseResponse {
        id: tenant.id,
        name: tenant.notes.unwrap_or_default(),
        state: tenant.state,
        region,
        endpoints: Some(endpoints),
        admin_user: Some(DEFAULT_ADMIN_USER.to_string()),
        admin_password: None,
        created_at: tenant.created_at,
        connection_string: Some(connection_string),
    }))
}

// ── DELETE /databases/:database_id ───────────────────────────────

pub async fn delete_database(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
) -> Result<Json<MessageResponse>, AppError> {
    let tenant = db::get_tenant_for_customer(&state.db, &database_id, &auth.customer_id)
        .await?
        .ok_or_else(|| AppError::not_found("Database not found"))?;

    db::update_tenant_state(&state.db, &database_id, tenant_state::DISABLING, None).await?;

    let pd = PdClient::new(&state.config.pd_endpoints, &state.http_client);
    if !pd.disable_keyspace(&tenant.keyspace).await {
        db::update_tenant_state(
            &state.db,
            &database_id,
            tenant_state::ACTIVE,
            Some("Failed to disable keyspace in PD"),
        )
        .await?;
        db::insert_audit_log(
            &state.db,
            "DELETE",
            "DATABASE",
            &database_id,
            Some(&database_id),
            Some(&auth.customer_id),
            false,
            Some("Failed to disable keyspace"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal("Failed to disable database"));
    }

    db::update_tenant_state(
        &state.db,
        &database_id,
        tenant_state::DISABLED,
        Some("Deleted by customer"),
    )
    .await?;

    db::insert_audit_log(
        &state.db,
        "DELETE",
        "DATABASE",
        &database_id,
        Some(&database_id),
        Some(&auth.customer_id),
        true,
        None,
        None,
    )
    .await
    .ok();

    Ok(Json(MessageResponse {
        message: "Database disabled".to_string(),
    }))
}

// ── POST /databases/:database_id/reset-password ──────────────

pub async fn reset_database_password(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
) -> Result<Json<CustomerPasswordResetResponse>, AppError> {
    let tenant = db::get_tenant_for_customer(&state.db, &database_id, &auth.customer_id)
        .await?
        .ok_or_else(|| AppError::not_found("Database not found"))?;

    let cred = db::get_credential(
        &state.db,
        &tenant.id,
        "admin",
        state.config.credential_key.as_deref(),
    )
    .await?;

    let cred = cred.ok_or_else(|| {
        AppError::conflict("No stored admin credential for this database. Cannot reset password.")
    })?;

    let new_password = generate_password();

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let success = pg
        .reset_password(
            &tenant.id,
            &cred.username,
            &cred.password_plain,
            &cred.username,
            &new_password,
        )
        .await;

    if !success {
        db::insert_audit_log(
            &state.db,
            "RESET_PASSWORD",
            "DATABASE",
            &database_id,
            Some(&database_id),
            Some(&auth.customer_id),
            false,
            Some("Failed to reset password in pg-tikv"),
            None,
        )
        .await
        .ok();
        return Err(AppError::bad_gateway(
            "Failed to reset password. The database may be unreachable.",
        ));
    }

    db::upsert_credential(
        &state.db,
        &tenant.id,
        "admin",
        &cred.username,
        &new_password,
        state.config.credential_key.as_deref(),
    )
    .await
    .ok();

    db::insert_audit_log(
        &state.db,
        "RESET_PASSWORD",
        "DATABASE",
        &database_id,
        Some(&database_id),
        Some(&auth.customer_id),
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
        build_connection_string(&tenant.id, &cred.username, &new_password, &host, port);

    Ok(Json(CustomerPasswordResetResponse {
        admin_user: cred.username,
        admin_password: new_password,
        connection_string,
    }))
}

// ── GET /databases/:database_id/observability ─────────────────

pub async fn get_database_observability(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
) -> Result<Json<TenantObservabilityResponse>, AppError> {
    let tenant = db::get_tenant_for_customer(&state.db, &database_id, &auth.customer_id)
        .await?
        .ok_or_else(|| AppError::not_found("Database not found"))?;

    let obs_cred = db::get_credential(
        &state.db,
        &tenant.id,
        "OBSERVABILITY",
        state.config.credential_key.as_deref(),
    )
    .await?;

    let obs_cred = match obs_cred {
        Some(c) => c,
        None => {
            let admin_cred = db::get_credential(
                &state.db,
                &tenant.id,
                "admin",
                state.config.credential_key.as_deref(),
            )
            .await?
            .ok_or_else(|| {
                AppError::new(
                    StatusCode::BAD_REQUEST,
                    "Cannot enable observability: admin credentials not stored. Run 'db9 db reset-password <id>' first.",
                )
            })?;

            let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
            let obs_password = generate_password();

            let created = pg
                .create_user(
                    &tenant.id,
                    &admin_cred.username,
                    &admin_cred.password_plain,
                    OBSERVABILITY_USER,
                    &obs_password,
                    false,
                )
                .await;

            if !created {
                let rotated = pg
                    .reset_password(
                        &tenant.id,
                        &admin_cred.username,
                        &admin_cred.password_plain,
                        OBSERVABILITY_USER,
                        &obs_password,
                    )
                    .await;
                if !rotated {
                    return Err(AppError::bad_gateway(
                        "Failed to bootstrap observability account",
                    ));
                }
            }

            db::upsert_credential(
                &state.db,
                &tenant.id,
                "OBSERVABILITY",
                OBSERVABILITY_USER,
                &obs_password,
                state.config.credential_key.as_deref(),
            )
            .await?;

            db::get_credential(
                &state.db,
                &tenant.id,
                "OBSERVABILITY",
                state.config.credential_key.as_deref(),
            )
            .await?
            .ok_or_else(|| {
                AppError::internal("Failed to read back observer credential after bootstrap")
            })?
        }
    };

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);

    let summary_val = pg
        .get_observability_summary(&tenant.id, &obs_cred.username, &obs_cred.password_plain)
        .await
        .map_err(|e| {
            tracing::warn!("observability query failed for tenant {}: {e}", tenant.id);
            AppError::new(
                StatusCode::CONFLICT,
                "Observability query failed: observer credential may be stale. Please retry or contact support.",
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
        .get_observability_samples(&tenant.id, &obs_cred.username, &obs_cred.password_plain)
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
