use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use rand::Rng;

use crate::auth::{ApiKeyAuth, TenantSessionExtractor};
use crate::db;
use crate::error::AppError;
use crate::models::*;
use crate::services::pg_client::PgClient;
use crate::AppState;

const SYSTEM_USER_PREFIX: &str = "_pgtikv_sys_";

fn is_protected_user(username: &str, session_admin_user: &str) -> bool {
    username.starts_with(SYSTEM_USER_PREFIX) || username == session_admin_user
}

fn generate_password() -> String {
    let mut rng = rand::thread_rng();
    let charset = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789!@#$%^&*";
    (0..16)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect()
}

pub async fn list_users(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Vec<UserResponse>>, AppError> {
    let session = TenantSessionExtractor::from_headers(&headers, &tenant_id, &state)?;

    let tenant = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let users: Vec<UserResponse> = pg
        .list_users(&tenant.id, &session.admin_user, &session.admin_password)
        .await
        .into_iter()
        .filter(|u| !u.name.starts_with(SYSTEM_USER_PREFIX))
        .collect();

    Ok(Json(users))
}

pub async fn create_user(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path(tenant_id): Path<String>,
    headers: HeaderMap,
    Json(request): Json<AdminCreateUserRequest>,
) -> Result<(StatusCode, Json<UserCreateResponse>), AppError> {
    let session = TenantSessionExtractor::from_headers(&headers, &tenant_id, &state)?;

    let tenant = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    let password = request.password.unwrap_or_else(generate_password);
    let superuser = request.superuser.unwrap_or(false);

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let success = pg
        .create_user(
            &tenant.id,
            &session.admin_user,
            &session.admin_password,
            &request.username,
            &password,
            superuser,
        )
        .await;

    if !success {
        db::insert_audit_log(
            &state.db,
            "CREATE",
            "USER",
            &request.username,
            Some(&tenant_id),
            Some(&session.admin_user),
            false,
            Some("PG client returned failure"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal("Failed to create user"));
    }

    db::insert_audit_log(
        &state.db,
        "CREATE",
        "USER",
        &request.username,
        Some(&tenant_id),
        Some(&session.admin_user),
        true,
        None,
        None,
    )
    .await
    .ok();

    Ok((
        StatusCode::CREATED,
        Json(UserCreateResponse {
            username: request.username.clone(),
            password,
            connection: format!(
                "psql -h {} -p {} -U {}.{}",
                state.config.pg_host, state.config.pg_port, tenant.id, request.username
            ),
        }),
    ))
}

pub async fn delete_user(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path((tenant_id, username)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<MessageResponse>, AppError> {
    let session = TenantSessionExtractor::from_headers(&headers, &tenant_id, &state)?;

    if is_protected_user(&username, &session.admin_user) {
        return Err(AppError::forbidden(format!(
            "Cannot delete protected user '{username}'"
        )));
    }

    let tenant = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let success = pg
        .drop_user(
            &tenant.id,
            &session.admin_user,
            &session.admin_password,
            &username,
        )
        .await;

    if !success {
        db::insert_audit_log(
            &state.db,
            "DELETE",
            "USER",
            &username,
            Some(&tenant_id),
            Some(&session.admin_user),
            false,
            Some("PG client returned failure"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal("Failed to delete user"));
    }

    db::insert_audit_log(
        &state.db,
        "DELETE",
        "USER",
        &username,
        Some(&tenant_id),
        Some(&session.admin_user),
        true,
        None,
        None,
    )
    .await
    .ok();

    Ok(Json(MessageResponse {
        message: format!("User '{username}' deleted"),
    }))
}

pub async fn reset_password(
    State(state): State<AppState>,
    _auth: ApiKeyAuth,
    Path((tenant_id, username)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<PasswordResetResponse>, AppError> {
    let session = TenantSessionExtractor::from_headers(&headers, &tenant_id, &state)?;

    if is_protected_user(&username, &session.admin_user) {
        return Err(AppError::forbidden(format!(
            "Cannot reset password for protected user '{username}'"
        )));
    }

    let tenant = db::get_tenant(&state.db, &tenant_id)
        .await?
        .ok_or_else(|| AppError::not_found(format!("Tenant '{tenant_id}' not found")))?;

    let new_password = generate_password();

    let pg = PgClient::new(&state.config.pg_host, state.config.pg_port);
    let success = pg
        .reset_password(
            &tenant.id,
            &session.admin_user,
            &session.admin_password,
            &username,
            &new_password,
        )
        .await;

    if !success {
        db::insert_audit_log(
            &state.db,
            "RESET_PASSWORD",
            "USER",
            &username,
            Some(&tenant_id),
            Some(&session.admin_user),
            false,
            Some("PG client returned failure"),
            None,
        )
        .await
        .ok();
        return Err(AppError::internal("Failed to reset password"));
    }

    db::insert_audit_log(
        &state.db,
        "RESET_PASSWORD",
        "USER",
        &username,
        Some(&tenant_id),
        Some(&session.admin_user),
        true,
        None,
        None,
    )
    .await
    .ok();

    Ok(Json(PasswordResetResponse {
        username,
        password: new_password,
    }))
}
