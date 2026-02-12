use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use pgtikv_admin::config::Config;
use pgtikv_admin::session::SessionManager;
use pgtikv_admin::{api, db, AppState};

// ── Setup ────────────────────────────────────────────────────────

async fn setup() -> (Router, AppState) {
    let db_id = uuid::Uuid::new_v4().to_string();
    let url = format!("sqlite:///tmp/pgtikv_test_{db_id}.db?mode=rwc");
    let pool = db::connect(&url).await.unwrap();
    db::create_tables(&pool).await.unwrap();

    let config = Config {
        pd_endpoints: "127.0.0.1:2379".into(),
        pg_host: "127.0.0.1".into(),
        pg_port: 5433,
        pg_public_endpoints: "127.0.0.1:5433".into(),
        api_port: 8090,
        api_host: "0.0.0.0".into(),
        database_url: "sqlite::memory:".into(),
        cors_origins: vec![],
        api_keys: vec![],
        reconciler_enabled: false,
        reconciler_interval_secs: 300,
        reconciler_sync_keyspaces: false,
        session_ttl_hours: 1,
        audit_retention_days: 90,
        credential_key: None,
    };

    let state = AppState {
        db: pool,
        config: Arc::new(config),
        sessions: Arc::new(SessionManager::new(1)),
        http_client: reqwest::Client::new(),
    };

    let app = api::router().with_state(state.clone());
    (app, state)
}

// ── HTTP helpers ─────────────────────────────────────────────────

async fn post_json(app: Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn get_json(app: Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn post_json_auth(app: Router, uri: &str, body: &Value, token: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::from(serde_json::to_vec(body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn get_json_auth(app: Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

#[allow(dead_code)]
async fn delete_json_auth(app: Router, uri: &str, token: &str) -> (StatusCode, Value) {
    let resp = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(uri)
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

// ── Composite helpers ────────────────────────────────────────────

/// Register + login, returning the bearer token.
async fn register_and_login(state: &AppState, email: &str, password: &str) -> String {
    let app = api::router().with_state(state.clone());
    let (status, _) = post_json(
        app,
        "/customer/register",
        &json!({ "email": email, "password": password }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let app = api::router().with_state(state.clone());
    let (status, body) = post_json(
        app,
        "/customer/login",
        &json!({ "email": email, "password": password }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["token"].as_str().unwrap().to_string()
}

/// GET /customer/me → extract customer id.
async fn get_customer_id(state: &AppState, token: &str) -> String {
    let app = api::router().with_state(state.clone());
    let (status, body) = get_json_auth(app, "/customer/me", token).await;
    assert_eq!(status, StatusCode::OK);
    body["id"].as_str().unwrap().to_string()
}

/// Insert a tenant into DB and assign it to the given customer.
async fn seed_customer_tenant(state: &AppState, customer_id: &str) -> String {
    let tenant_id = format!("t{}", &uuid::Uuid::new_v4().to_string()[..11]);
    let keyspace = format!("tipg_tenant_{tenant_id}");
    let now = chrono::Utc::now().to_rfc3339();
    db::insert_tenant(&state.db, &tenant_id, &keyspace, "ACTIVE", &now)
        .await
        .unwrap();
    db::set_tenant_customer_id(&state.db, &tenant_id, customer_id)
        .await
        .unwrap();
    tenant_id
}

// ── Tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn register_success() {
    let (app, _state) = setup().await;
    let email = format!("reg-{}@example.com", uuid::Uuid::new_v4());
    let (status, body) = post_json(
        app,
        "/customer/register",
        &json!({ "email": email, "password": "TestPassword123!" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    assert!(body["id"].is_string());
    assert_eq!(body["email"].as_str().unwrap(), email);
    assert_eq!(body["status"].as_str().unwrap(), "active");
}

#[tokio::test]
async fn register_duplicate_email() {
    let (_app, state) = setup().await;
    let email = format!("dup-{}@example.com", uuid::Uuid::new_v4());

    let app = api::router().with_state(state.clone());
    let (status, _) = post_json(
        app,
        "/customer/register",
        &json!({ "email": email, "password": "TestPassword123!" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let app = api::router().with_state(state.clone());
    let (status, body) = post_json(
        app,
        "/customer/register",
        &json!({ "email": email, "password": "AnotherPassword9!" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["message"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("already"));
}

#[tokio::test]
async fn login_success() {
    let (_app, state) = setup().await;
    let email = format!("login-{}@example.com", uuid::Uuid::new_v4());

    let app = api::router().with_state(state.clone());
    let (status, _) = post_json(
        app,
        "/customer/register",
        &json!({ "email": email, "password": "GoodPass123!" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let app = api::router().with_state(state.clone());
    let (status, body) = post_json(
        app,
        "/customer/login",
        &json!({ "email": email, "password": "GoodPass123!" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["token"].is_string());
    assert!(!body["token"].as_str().unwrap().is_empty());
    assert!(body["expires_at"].is_string());
}

#[tokio::test]
async fn login_wrong_password() {
    let (_app, state) = setup().await;
    let email = format!("wrongpw-{}@example.com", uuid::Uuid::new_v4());

    let app = api::router().with_state(state.clone());
    let (status, _) = post_json(
        app,
        "/customer/register",
        &json!({ "email": email, "password": "CorrectPass1!" }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let app = api::router().with_state(state.clone());
    let (status, _body) = post_json(
        app,
        "/customer/login",
        &json!({ "email": email, "password": "WrongPass999!" }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_requires_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = get_json(app, "/customer/me").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_with_valid_token() {
    let (_app, state) = setup().await;
    let email = format!("me-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;

    let app = api::router().with_state(state.clone());
    let (status, body) = get_json_auth(app, "/customer/me", &token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["email"].as_str().unwrap(), email);
    assert!(body["id"].is_string());
    assert_eq!(body["status"].as_str().unwrap(), "active");
}

#[tokio::test]
async fn reset_password_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = post_json(
        app,
        "/customer/databases/nonexistent123/reset-password",
        &json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reset_password_db_not_found() {
    let (_app, state) = setup().await;
    let email = format!("reset404-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;

    let app = api::router().with_state(state.clone());
    let (status, body) = post_json_auth(
        app,
        "/customer/databases/nonexistent123/reset-password",
        &json!({}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["message"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("not found"));
}

#[tokio::test]
async fn reset_password_no_credential() {
    let (_app, state) = setup().await;
    let email = format!("resetnocred-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;
    let customer_id = get_customer_id(&state, &token).await;

    // Seed a tenant owned by this customer, but do NOT store a credential
    let tenant_id = seed_customer_tenant(&state, &customer_id).await;

    let app = api::router().with_state(state.clone());
    let uri = format!("/customer/databases/{tenant_id}/reset-password");
    let (status, body) = post_json_auth(app, &uri, &json!({}), &token).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body["message"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("credential"));
}

#[tokio::test]
async fn reset_password_with_credential_but_no_pgclient() {
    let (_app, state) = setup().await;
    let email = format!("resetpg-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;
    let customer_id = get_customer_id(&state, &token).await;

    // Seed tenant + credential so the handler reaches the PgClient step
    let tenant_id = seed_customer_tenant(&state, &customer_id).await;
    db::upsert_credential(&state.db, &tenant_id, "admin", "admin", "oldpass123", None)
        .await
        .unwrap();

    let app = api::router().with_state(state.clone());
    let uri = format!("/customer/databases/{tenant_id}/reset-password");
    let (status, body) = post_json_auth(app, &uri, &json!({}), &token).await;

    // PgClient cannot connect → handler returns 502 BAD_GATEWAY
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body["message"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("reset password"));
}

#[tokio::test]
async fn test_sql_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = post_json(
        app,
        "/customer/databases/nonexistent123/sql",
        &json!({"query": "SELECT 1"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_sql_db_not_found() {
    let (_app, state) = setup().await;
    let email = format!("sql404-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;

    let app = api::router().with_state(state.clone());
    let (status, body) = post_json_auth(
        app,
        "/customer/databases/nonexistent123/sql",
        &json!({"query": "SELECT 1"}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["message"]
        .as_str()
        .map(|msg| msg.to_lowercase().contains("not found"))
        .unwrap_or(false));
}

#[tokio::test]
async fn test_sql_with_tenant_but_no_pgclient() {
    let (_app, state) = setup().await;
    let email = format!("sqlpg-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;
    let customer_id = get_customer_id(&state, &token).await;
    let tenant_id = seed_customer_tenant(&state, &customer_id).await;
    db::upsert_credential(&state.db, &tenant_id, "admin", "admin", "oldpass123", None)
        .await
        .unwrap();

    let app = api::router().with_state(state.clone());
    let uri = format!("/customer/databases/{tenant_id}/sql");
    let (status, _body) = post_json_auth(app, &uri, &json!({"query": "SELECT 1"}), &token).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_users_list_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = get_json(app, "/customer/databases/nonexistent123/users").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_users_list_db_not_found() {
    let (_app, state) = setup().await;
    let email = format!("users404-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;

    let app = api::router().with_state(state.clone());
    let (status, body) = get_json_auth(app, "/customer/databases/nonexistent123/users", &token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["message"]
        .as_str()
        .map(|msg| msg.to_lowercase().contains("not found"))
        .unwrap_or(false));
}

#[tokio::test]
async fn test_users_create_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = post_json(
        app,
        "/customer/databases/nonexistent123/users",
        &json!({"username": "testuser", "password": "Secret123!"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_users_delete_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = delete_json_auth(
        app,
        "/customer/databases/nonexistent123/users/testuser",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_dump_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = post_json(
        app,
        "/customer/databases/nonexistent123/dump",
        &json!({"ddl_only": true}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_dump_db_not_found() {
    let (_app, state) = setup().await;
    let email = format!("dump404-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;

    let app = api::router().with_state(state.clone());
    let (status, body) = post_json_auth(
        app,
        "/customer/databases/nonexistent123/dump",
        &json!({"ddl_only": true}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["message"]
        .as_str()
        .map(|msg| msg.to_lowercase().contains("not found"))
        .unwrap_or(false));
}

#[tokio::test]
async fn test_dump_with_tenant_but_no_pgclient() {
    let (_app, state) = setup().await;
    let email = format!("dumppg-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;
    let customer_id = get_customer_id(&state, &token).await;
    let tenant_id = seed_customer_tenant(&state, &customer_id).await;
    db::upsert_credential(&state.db, &tenant_id, "admin", "admin", "oldpass123", None)
        .await
        .unwrap();

    let app = api::router().with_state(state.clone());
    let uri = format!("/customer/databases/{tenant_id}/dump");
    let (status, _body) = post_json_auth(app, &uri, &json!({"ddl_only": true}), &token).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn test_schema_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = get_json(app, "/customer/databases/nonexistent123/schema").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_schema_db_not_found() {
    let (_app, state) = setup().await;
    let email = format!("schema404-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;

    let app = api::router().with_state(state.clone());
    let (status, body) = get_json_auth(app, "/customer/databases/nonexistent123/schema", &token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["message"]
        .as_str()
        .map(|msg| msg.to_lowercase().contains("not found"))
        .unwrap_or(false));
}

#[tokio::test]
async fn test_migrations_list_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = get_json(app, "/customer/databases/nonexistent123/migrations").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_migrations_list_db_not_found() {
    let (_app, state) = setup().await;
    let email = format!("miglist404-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;

    let app = api::router().with_state(state.clone());
    let (status, body) = get_json_auth(app, "/customer/databases/nonexistent123/migrations", &token).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["message"]
        .as_str()
        .map(|msg| msg.to_lowercase().contains("not found"))
        .unwrap_or(false));
}

#[tokio::test]
async fn test_migrations_apply_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = post_json(
        app,
        "/customer/databases/nonexistent123/migrations",
        &json!({
            "name": "202602120001_init",
            "sql": "SELECT 1",
            "checksum": "abc123"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_migrations_apply_db_not_found() {
    let (_app, state) = setup().await;
    let email = format!("migapply404-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;

    let app = api::router().with_state(state.clone());
    let (status, body) = post_json_auth(
        app,
        "/customer/databases/nonexistent123/migrations",
        &json!({
            "name": "202602120001_init",
            "sql": "SELECT 1",
            "checksum": "abc123"
        }),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["message"]
        .as_str()
        .map(|msg| msg.to_lowercase().contains("not found"))
        .unwrap_or(false));
}

#[tokio::test]
async fn test_branch_no_auth() {
    let (app, _state) = setup().await;
    let (status, _body) = post_json(
        app,
        "/customer/databases/nonexistent123/branch",
        &json!({"name": "feature-branch"}),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_branch_db_not_found() {
    let (_app, state) = setup().await;
    let email = format!("branch404-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;

    let app = api::router().with_state(state.clone());
    let (status, body) = post_json_auth(
        app,
        "/customer/databases/nonexistent123/branch",
        &json!({"name": "feature-branch"}),
        &token,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["message"]
        .as_str()
        .map(|msg| msg.to_lowercase().contains("not found"))
        .unwrap_or(false));
}

#[tokio::test]
async fn test_branch_with_tenant_but_no_pgclient() {
    let (_app, state) = setup().await;
    let email = format!("branchpg-{}@example.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "SecurePass1!").await;
    let customer_id = get_customer_id(&state, &token).await;
    let tenant_id = seed_customer_tenant(&state, &customer_id).await;
    db::upsert_credential(&state.db, &tenant_id, "admin", "admin", "oldpass123", None)
        .await
        .unwrap();

    let app = api::router().with_state(state.clone());
    let uri = format!("/customer/databases/{tenant_id}/branch");
    let (status, _body) = post_json_auth(app, &uri, &json!({"name": "feature-branch"}), &token).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
