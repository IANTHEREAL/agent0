use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use rand::Rng;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

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

const SYSTEM_USER_PREFIX: &str = "_pgtikv_sys_";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/register", post(register))
        .route("/anonymous-register", post(anonymous_register))
        .route("/anonymous-refresh", post(anonymous_refresh))
        .route("/anonymous-secret", post(get_anonymous_secret))
        .route("/login", post(login))
        .route("/claim", post(claim_account))
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
        .route(
            "/databases/:database_id/sql",
            post(execute_database_sql_structured),
        )
        .route(
            "/databases/:database_id/users",
            get(list_database_users).post(create_database_user),
        )
        .route(
            "/databases/:database_id/users/:username",
            delete(delete_database_user),
        )
        .route("/databases/:database_id/dump", post(dump_database))
        .route("/databases/:database_id/schema", get(get_database_schema))
        .route(
            "/databases/:database_id/migrations",
            post(apply_database_migration).get(list_database_migrations),
        )
        .route("/databases/:database_id/branch", post(branch_database))
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

fn escape_identifier(identifier: &str) -> String {
    identifier.replace('"', "\"\"")
}

fn escape_sql_literal(value: &str) -> String {
    value.replace('\'', "''")
}

fn sql_literal_from_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "NULL".to_string(),
        serde_json::Value::Bool(v) => {
            if *v {
                "TRUE".to_string()
            } else {
                "FALSE".to_string()
            }
        }
        serde_json::Value::Number(v) => v.to_string(),
        serde_json::Value::String(v) => format!("'{}'", escape_sql_literal(v)),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            format!("'{}'", escape_sql_literal(&value.to_string()))
        }
    }
}

fn sql_result_column_index(result: &SqlResult, name: &str) -> Result<usize, AppError> {
    result
        .columns
        .iter()
        .position(|c| c.name == name)
        .ok_or_else(|| {
            AppError::internal(format!("Missing expected column '{}' in SQL result", name))
        })
}

fn row_string_at(
    row: &[serde_json::Value],
    idx: usize,
    field_name: &str,
) -> Result<String, AppError> {
    row.get(idx)
        .and_then(|v| v.as_str())
        .map(|v| v.to_string())
        .ok_or_else(|| AppError::internal(format!("Invalid '{}' value in SQL result", field_name)))
}

fn row_optional_string_at(row: &[serde_json::Value], idx: usize) -> Option<String> {
    row.get(idx)
        .and_then(|v| if v.is_null() { None } else { v.as_str() })
        .map(|v| v.to_string())
}

async fn get_customer_tenant_and_admin_credential(
    state: &AppState,
    customer_id: &str,
    database_id: &str,
) -> Result<(TenantRow, CredentialRow), AppError> {
    let tenant = db::get_tenant_for_customer(&state.db, database_id, customer_id)
        .await?
        .ok_or_else(|| AppError::not_found("Database not found"))?;

    if tenant.state != tenant_state::ACTIVE {
        return Err(AppError::conflict(
            "Database is not active. Retry when state is ACTIVE.",
        ));
    }

    let cred = db::get_credential(
        &state.db,
        &tenant.id,
        "admin",
        state.config.credential_key.as_deref(),
    )
    .await?
    .ok_or_else(|| {
        AppError::conflict("No stored admin credential for this database. Reset password first.")
    })?;

    Ok((tenant, cred))
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

pub async fn anonymous_register(
    State(state): State<AppState>,
) -> Result<Json<AnonymousRegisterResponse>, AppError> {
    let id = uuid::Uuid::new_v4().to_string();
    let email = format!("anon_{id}@anonymous.local");
    let placeholder_hash = "$anon$not-a-real-hash";

    db::create_anonymous_customer(&state.db, &id, &email, placeholder_hash, 5).await?;

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

    db::create_customer_token(
        &state.db,
        &token_id,
        &id,
        &token_hash,
        "default",
        &expires_at,
    )
    .await?;

    let mut secret_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret_bytes);
    let anonymous_secret = secret_bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let secret_hash_bytes = Sha256::digest(anonymous_secret.as_bytes());
    let secret_hash = secret_hash_bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    db::store_anonymous_secret(&state.db, &id, &secret_hash).await?;

    Ok(Json(AnonymousRegisterResponse {
        token,
        expires_at,
        is_anonymous: true,
        anonymous_id: id,
        anonymous_secret,
    }))
}

pub async fn anonymous_refresh(
    State(state): State<AppState>,
    Json(req): Json<AnonymousRefreshRequest>,
) -> Result<Json<AnonymousRefreshResponse>, AppError> {
    let secret_hash_bytes = Sha256::digest(req.anonymous_secret.as_bytes());
    let secret_hash = secret_hash_bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let customer =
        db::get_anonymous_customer_by_id_and_secret(&state.db, &req.anonymous_id, &secret_hash)
            .await?
            .ok_or_else(|| AppError::unauthorized("Invalid anonymous credentials"))?;

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

    db::create_customer_token(
        &state.db,
        &token_id,
        &customer.id,
        &token_hash,
        "default",
        &expires_at,
    )
    .await?;

    Ok(Json(AnonymousRefreshResponse { token, expires_at }))
}

pub async fn get_anonymous_secret(
    State(state): State<AppState>,
    auth: CustomerAuth,
) -> Result<Json<AnonymousSecretResponse>, AppError> {
    let customer = db::get_customer_by_id(&state.db, &auth.customer_id)
        .await?
        .ok_or_else(|| AppError::not_found("Customer not found"))?;

    if !customer.is_anonymous {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Only anonymous accounts can request a secret",
        ));
    }

    use rand::RngCore;
    let mut secret_bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut secret_bytes);
    let anonymous_secret = secret_bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    let secret_hash_bytes = Sha256::digest(anonymous_secret.as_bytes());
    let secret_hash = secret_hash_bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

    db::store_anonymous_secret(&state.db, &auth.customer_id, &secret_hash).await?;

    Ok(Json(AnonymousSecretResponse {
        anonymous_id: auth.customer_id,
        anonymous_secret,
    }))
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

    db::create_customer_token(
        &state.db,
        &token_id,
        &customer.id,
        &token_hash,
        "default",
        &expires_at,
    )
    .await?;

    Ok(Json(LoginResponse { token, expires_at }))
}

pub async fn claim_account(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Json(req): Json<ClaimRequest>,
) -> Result<Json<ClaimResponse>, AppError> {
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

    if db::get_customer_by_email(&state.db, &req.email)
        .await?
        .is_some()
    {
        return Err(AppError::conflict("Email already registered"));
    }

    use argon2::{password_hash::SaltString, Argon2, PasswordHasher};
    use rand::rngs::OsRng;
    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(req.password.as_bytes(), &salt)
        .map_err(|e| AppError::internal(format!("Password hashing failed: {e}")))?
        .to_string();

    let claimed =
        db::claim_anonymous_customer(&state.db, &auth.customer_id, &req.email, &password_hash)
            .await?;
    if !claimed {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Account is not anonymous or already claimed",
        ));
    }

    Ok(Json(ClaimResponse {
        id: auth.customer_id,
        email: req.email,
        claimed: true,
    }))
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
    let customer = db::get_customer_by_id(&state.db, &auth.customer_id)
        .await?
        .ok_or_else(|| AppError::not_found("Customer not found"))?;

    if customer.is_anonymous {
        let tenant_count = db::count_customer_tenants(&state.db, &auth.customer_id).await?;
        let limit = customer.database_limit.unwrap_or(5) as i64;
        if tenant_count >= limit {
            return Err(AppError::forbidden(format!(
                "Anonymous account database limit reached (max {limit}). Run 'db9 claim' to upgrade your account."
            )));
        }
    }

    if let Some(ref p) = req.admin_password {
        if p.is_empty() {
            return Err(AppError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "admin_password cannot be empty",
            ));
        }
    }

    let tenant_id = generate_tenant_id();
    let keyspace = make_keyspace(&tenant_id);
    let admin_user = DEFAULT_ADMIN_USER.to_string();
    let password = req.admin_password.clone().unwrap_or_else(generate_password);

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

    // ── FS9 integration (best-effort) ────────────────────────────
    if let Some(ref fs9) = state.fs9_client {
        let fs_keyspace = format!("tipg_fs_{}", tenant_id);

        // Create filesystem keyspace in PD
        if !pd.create_keyspace(&fs_keyspace).await {
            tracing::warn!(
                tenant_id,
                fs_keyspace,
                "Failed to create fs keyspace in PD (non-fatal)"
            );
        }

        // Create fs9 namespace
        if let Err(e) = fs9.create_namespace(&tenant_id).await {
            tracing::warn!(tenant_id, error = %e, "Failed to create fs9 namespace (non-fatal)");
        } else {
            // Create a user in the namespace so a token can be issued.
            // fs9-server auto-provisions the pagefs mount via default_pagefs config.
            let fs9_user_id = match fs9.create_user(&tenant_id, &auth.customer_id).await {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!(tenant_id, error = %e, "Failed to create fs9 user (non-fatal)");
                    None
                }
            };

            // Generate and store fs9 token
            if let Some(user_id) = fs9_user_id {
                match fs9.generate_token(&user_id).await {
                    Ok(token) => {
                        db::upsert_credential(
                            &state.db,
                            &tenant_id,
                            "fs9_token",
                            &auth.customer_id,
                            &token,
                            state.config.credential_key.as_deref(),
                        )
                        .await
                        .ok();
                    }
                    Err(e) => {
                        tracing::warn!(tenant_id, error = %e, "Failed to generate fs9 token (non-fatal)");
                    }
                }
            }
        }
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

    // Install default extensions (non-fatal)
    pg.bootstrap_default_extensions(&tenant_id, &admin_user, &password)
        .await;

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
    db::update_tenant_metadata(&state.db, &tenant_id, Some(&req.name), tags_json.as_deref())
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

pub async fn dump_database(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
    Json(req): Json<DumpRequest>,
) -> Result<Json<DumpResponse>, AppError> {
    let (tenant, cred) =
        get_customer_tenant_and_admin_credential(&state, &auth.customer_id, &database_id).await?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let ddl_result = pg
        .run_sql_structured(
            &tenant.id,
            &cred.username,
            &cred.password_plain,
            "SELECT * FROM _pgtikv_sys_export_ddl() ORDER BY object_type",
        )
        .await
        .map_err(|e| AppError::bad_gateway(format!("Failed to export DDL: {e}")))?;

    let ddl_sql_idx = sql_result_column_index(&ddl_result, "ddl_sql")?;
    let mut ddl_statements = Vec::new();
    for row in &ddl_result.rows {
        if let Some(stmt) = row.get(ddl_sql_idx).and_then(|v| v.as_str()) {
            let trimmed = stmt.trim();
            if !trimmed.is_empty() {
                ddl_statements.push(trimmed.to_string());
            }
        }
    }

    let object_count = ddl_statements.len();
    let mut sections = Vec::new();
    if !ddl_statements.is_empty() {
        sections.push(ddl_statements.join(";\n\n"));
    }

    if !req.ddl_only {
        let table_result = pg
            .run_sql_structured(
                &tenant.id,
                &cred.username,
                &cred.password_plain,
                "SELECT table_schema, table_name FROM information_schema.tables WHERE table_type = 'BASE TABLE' AND table_schema NOT IN ('pg_catalog', 'information_schema') ORDER BY table_schema, table_name",
            )
            .await
            .map_err(|e| AppError::bad_gateway(format!("Failed to enumerate tables: {e}")))?;

        let table_schema_idx = sql_result_column_index(&table_result, "table_schema")?;
        let table_name_idx = sql_result_column_index(&table_result, "table_name")?;

        let mut insert_statements = Vec::new();
        for row in &table_result.rows {
            let table_schema = row_string_at(row, table_schema_idx, "table_schema")?;
            let table_name = row_string_at(row, table_name_idx, "table_name")?;
            let qualified_table = format!(
                "\"{}\".\"{}\"",
                escape_identifier(&table_schema),
                escape_identifier(&table_name)
            );
            let select_sql = format!("SELECT * FROM {qualified_table}");

            let table_data = pg
                .run_sql_structured(
                    &tenant.id,
                    &cred.username,
                    &cred.password_plain,
                    &select_sql,
                )
                .await
                .map_err(|e| {
                    AppError::bad_gateway(format!(
                        "Failed to export rows from {}.{}: {e}",
                        table_schema, table_name
                    ))
                })?;

            if table_data.columns.is_empty() || table_data.rows.is_empty() {
                continue;
            }

            let column_list = table_data
                .columns
                .iter()
                .map(|c| format!("\"{}\"", escape_identifier(&c.name)))
                .collect::<Vec<_>>()
                .join(", ");

            for data_row in &table_data.rows {
                let value_list = data_row
                    .iter()
                    .map(sql_literal_from_json)
                    .collect::<Vec<_>>()
                    .join(", ");
                insert_statements.push(format!(
                    "INSERT INTO {qualified_table} ({column_list}) VALUES ({value_list});"
                ));
            }
        }

        if !insert_statements.is_empty() {
            sections.push(insert_statements.join("\n"));
        }
    }

    let sql = sections.join("\n\n");
    Ok(Json(DumpResponse { sql, object_count }))
}

pub async fn get_database_schema(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
) -> Result<Json<SchemaResponse>, AppError> {
    let (tenant, cred) =
        get_customer_tenant_and_admin_credential(&state, &auth.customer_id, &database_id).await?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);

    let columns_result = pg
        .run_sql_structured(
            &tenant.id,
            &cred.username,
            &cred.password_plain,
            "SELECT table_schema, table_name, column_name, data_type, is_nullable, column_default FROM information_schema.columns WHERE table_schema NOT IN ('pg_catalog', 'information_schema') ORDER BY table_schema, table_name, ordinal_position",
        )
        .await
        .map_err(|e| AppError::bad_gateway(format!("Failed to load schema columns: {e}")))?;

    let table_schema_idx = sql_result_column_index(&columns_result, "table_schema")?;
    let table_name_idx = sql_result_column_index(&columns_result, "table_name")?;
    let column_name_idx = sql_result_column_index(&columns_result, "column_name")?;
    let data_type_idx = sql_result_column_index(&columns_result, "data_type")?;
    let is_nullable_idx = sql_result_column_index(&columns_result, "is_nullable")?;
    let column_default_idx = sql_result_column_index(&columns_result, "column_default")?;

    let mut grouped: BTreeMap<(String, String), Vec<ColumnMetadata>> = BTreeMap::new();
    for row in &columns_result.rows {
        let schema = row_string_at(row, table_schema_idx, "table_schema")?;
        let table = row_string_at(row, table_name_idx, "table_name")?;
        let column_name = row_string_at(row, column_name_idx, "column_name")?;
        let data_type = row_string_at(row, data_type_idx, "data_type")?;
        let nullable = row
            .get(is_nullable_idx)
            .and_then(|v| v.as_str())
            .map(|v| v.eq_ignore_ascii_case("YES"))
            .unwrap_or(false);
        let default_value = row_optional_string_at(row, column_default_idx);

        grouped
            .entry((schema, table))
            .or_default()
            .push(ColumnMetadata {
                name: column_name,
                data_type,
                nullable,
                default_value,
            });
    }

    let tables = grouped
        .into_iter()
        .map(|((schema, name), columns)| TableMetadata {
            name,
            schema,
            columns,
        })
        .collect::<Vec<_>>();

    let views_result = pg
        .run_sql_structured(
            &tenant.id,
            &cred.username,
            &cred.password_plain,
            "SELECT table_schema, table_name FROM information_schema.tables WHERE table_type = 'VIEW' AND table_schema NOT IN ('pg_catalog', 'information_schema') ORDER BY table_schema, table_name",
        )
        .await
        .map_err(|e| AppError::bad_gateway(format!("Failed to load views: {e}")))?;

    let view_schema_idx = sql_result_column_index(&views_result, "table_schema")?;
    let view_name_idx = sql_result_column_index(&views_result, "table_name")?;
    let mut views = Vec::new();
    for row in &views_result.rows {
        views.push(ViewMetadata {
            schema: row_string_at(row, view_schema_idx, "table_schema")?,
            name: row_string_at(row, view_name_idx, "table_name")?,
        });
    }

    Ok(Json(SchemaResponse { tables, views }))
}

pub async fn apply_database_migration(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
    Json(req): Json<MigrationApplyRequest>,
) -> Result<Json<MigrationApplyResponse>, AppError> {
    if req.name.trim().is_empty() || req.sql.trim().is_empty() || req.checksum.trim().is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "'name', 'sql', and 'checksum' are required",
        ));
    }

    let (tenant, cred) =
        get_customer_tenant_and_admin_credential(&state, &auth.customer_id, &database_id).await?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let migrations_result = pg
        .run_sql_structured(
            &tenant.id,
            &cred.username,
            &cred.password_plain,
            "SELECT * FROM _pgtikv_sys_migrations()",
        )
        .await
        .map_err(|e| AppError::bad_gateway(format!("Failed to list migrations: {e}")))?;

    let name_idx = sql_result_column_index(&migrations_result, "name")?;
    let checksum_idx = sql_result_column_index(&migrations_result, "checksum")?;

    let existing = migrations_result.rows.iter().find_map(|row| {
        let migration_name = row.get(name_idx)?.as_str()?;
        if migration_name == req.name {
            row.get(checksum_idx)
                .and_then(|v| v.as_str())
                .map(|checksum| checksum.to_string())
        } else {
            None
        }
    });

    if let Some(existing_checksum) = existing {
        if existing_checksum == req.checksum {
            return Ok(Json(MigrationApplyResponse {
                status: "already_applied".to_string(),
                name: req.name,
            }));
        }

        return Err(AppError::new(
            StatusCode::CONFLICT,
            format!("Migration '{}' exists with different checksum", req.name),
        ));
    }

    pg.run_sql_structured(
        &tenant.id,
        &cred.username,
        &cred.password_plain,
        req.sql.trim(),
    )
    .await
    .map_err(|e| AppError::bad_gateway(format!("Failed to apply migration SQL: {e}")))?;

    // Record the migration after successful SQL execution
    let preview = if req.sql.len() > 200 {
        &req.sql[..200]
    } else {
        &req.sql
    };
    let record_sql = format!(
        "SELECT * FROM _pgtikv_sys_record_migration('{}', '{}', '{}')",
        req.name.replace('\'', "''"),
        req.checksum.replace('\'', "''"),
        preview.replace('\'', "''"),
    );
    pg.run_sql_structured(
        &tenant.id,
        &cred.username,
        &cred.password_plain,
        &record_sql,
    )
    .await
    .map_err(|e| AppError::bad_gateway(format!("Failed to record migration: {e}")))?;

    Ok(Json(MigrationApplyResponse {
        status: "applied".to_string(),
        name: req.name,
    }))
}

pub async fn list_database_migrations(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
) -> Result<Json<Vec<MigrationMetadata>>, AppError> {
    let (tenant, cred) =
        get_customer_tenant_and_admin_credential(&state, &auth.customer_id, &database_id).await?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let result = pg
        .run_sql_structured(
            &tenant.id,
            &cred.username,
            &cred.password_plain,
            "SELECT * FROM _pgtikv_sys_migrations()",
        )
        .await
        .map_err(|e| AppError::bad_gateway(format!("Failed to list migrations: {e}")))?;

    let name_idx = sql_result_column_index(&result, "name")?;
    let applied_at_idx = sql_result_column_index(&result, "applied_at")?;
    let checksum_idx = sql_result_column_index(&result, "checksum")?;
    let sql_preview_idx = sql_result_column_index(&result, "sql_preview")?;

    let mut migrations = Vec::new();
    for row in &result.rows {
        migrations.push(MigrationMetadata {
            name: row_string_at(row, name_idx, "name")?,
            applied_at: row_string_at(row, applied_at_idx, "applied_at")?,
            checksum: row_string_at(row, checksum_idx, "checksum")?,
            sql_preview: row_string_at(row, sql_preview_idx, "sql_preview")?,
        });
    }

    Ok(Json(migrations))
}

pub async fn branch_database(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
    Json(req): Json<BranchRequest>,
) -> Result<(StatusCode, Json<DatabaseResponse>), AppError> {
    if req.name.trim().is_empty() {
        return Err(AppError::new(
            StatusCode::BAD_REQUEST,
            "Branch name cannot be empty",
        ));
    }

    let (source_tenant, source_cred) =
        get_customer_tenant_and_admin_credential(&state, &auth.customer_id, &database_id).await?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let export_result = pg
        .run_sql_structured(
            &source_tenant.id,
            &source_cred.username,
            &source_cred.password_plain,
            "SELECT * FROM _pgtikv_sys_export_ddl() ORDER BY object_type",
        )
        .await
        .map_err(|e| AppError::bad_gateway(format!("Failed to export source schema: {e}")))?;

    let ddl_sql_idx = sql_result_column_index(&export_result, "ddl_sql")?;
    let ddl_script = export_result
        .rows
        .iter()
        .filter_map(|row| row.get(ddl_sql_idx).and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(";\n\n");

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
        return Err(AppError::internal("Failed to create branch database"));
    }

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
        return Err(AppError::internal(
            "Failed to initialize branch database. Please retry.",
        ));
    }

    if !ddl_script.is_empty() {
        if let Err(err) = pg
            .run_sql_structured(&tenant_id, &admin_user, &password, &ddl_script)
            .await
        {
            db::update_tenant_state(
                &state.db,
                &tenant_id,
                tenant_state::CREATE_FAILED,
                Some("Schema bootstrap failed"),
            )
            .await?;
            return Err(AppError::bad_gateway(format!(
                "Failed to apply branch schema: {err}"
            )));
        }
    }

    pg.bootstrap_default_extensions(&tenant_id, &admin_user, &password)
        .await;

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

    let source_region = parse_region_from_tags(&source_tenant.tags);
    let tags_json = source_region
        .as_ref()
        .map(|r| serde_json::to_string(&vec![r]).unwrap_or_else(|_| "[]".into()));
    let notes = format!("{} [branch-of:{}]", req.name.trim(), database_id);
    db::update_tenant_metadata(&state.db, &tenant_id, Some(&notes), tags_json.as_deref()).await?;

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
            region: source_region,
            endpoints: None,
            admin_user: Some(admin_user),
            admin_password: Some(password),
            created_at: now,
            connection_string: Some(connection_string),
        }),
    ))
}

pub async fn execute_database_sql_structured(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
    Json(req): Json<SqlExecuteRequest>,
) -> Result<Json<SqlResult>, AppError> {
    let (tenant, cred) =
        get_customer_tenant_and_admin_credential(&state, &auth.customer_id, &database_id).await?;

    let sql = req
        .query
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            req.file_content
                .as_ref()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
        })
        .ok_or_else(|| {
            AppError::new(
                StatusCode::BAD_REQUEST,
                "Provide non-empty 'query' or 'file_content'",
            )
        })?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let result = match pg
        .run_sql_structured(&tenant.id, &cred.username, &cred.password_plain, sql)
        .await
    {
        Ok(r) => r,
        Err(e) => SqlResult {
            columns: Vec::new(),
            rows: Vec::new(),
            row_count: 0,
            command: "ERROR".to_string(),
            error: Some(e),
        },
    };

    Ok(Json(result))
}

pub async fn list_database_users(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
) -> Result<Json<Vec<UserResponse>>, AppError> {
    let (tenant, cred) =
        get_customer_tenant_and_admin_credential(&state, &auth.customer_id, &database_id).await?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let users = pg
        .list_users(&tenant.id, &cred.username, &cred.password_plain)
        .await
        .into_iter()
        .filter(|u| !u.name.starts_with(SYSTEM_USER_PREFIX))
        .collect::<Vec<_>>();

    Ok(Json(users))
}

pub async fn create_database_user(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path(database_id): Path<String>,
    Json(req): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<MessageResponse>), AppError> {
    let (tenant, cred) =
        get_customer_tenant_and_admin_credential(&state, &auth.customer_id, &database_id).await?;

    if req.username.starts_with(SYSTEM_USER_PREFIX) {
        return Err(AppError::forbidden(
            "Cannot create reserved system user names",
        ));
    }

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let success = pg
        .create_user(
            &tenant.id,
            &cred.username,
            &cred.password_plain,
            &req.username,
            &req.password,
            false,
        )
        .await;

    if !success {
        return Err(AppError::bad_gateway("Failed to create user"));
    }

    Ok((
        StatusCode::CREATED,
        Json(MessageResponse {
            message: format!("User '{}' created", req.username),
        }),
    ))
}

pub async fn delete_database_user(
    State(state): State<AppState>,
    auth: CustomerAuth,
    Path((database_id, username)): Path<(String, String)>,
) -> Result<Json<MessageResponse>, AppError> {
    let (tenant, cred) =
        get_customer_tenant_and_admin_credential(&state, &auth.customer_id, &database_id).await?;

    if username == DEFAULT_ADMIN_USER || username.starts_with(SYSTEM_USER_PREFIX) {
        return Err(AppError::forbidden(format!(
            "Cannot delete protected user '{username}'"
        )));
    }

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let success = pg
        .drop_user(&tenant.id, &cred.username, &cred.password_plain, &username)
        .await;

    if !success {
        return Err(AppError::bad_gateway("Failed to delete user"));
    }

    Ok(Json(MessageResponse {
        message: format!("User '{username}' deleted"),
    }))
}
