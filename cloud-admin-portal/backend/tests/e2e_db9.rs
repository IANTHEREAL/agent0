use std::io::Write;
use std::process::Command;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use pgtikv_admin::config::Config;
use pgtikv_admin::session::SessionManager;
use pgtikv_admin::{api, db, AppState};

// ── Test infrastructure ─────────────────────────────────────────

/// Temporary HOME directory for credential isolation.
struct TempHome {
    dir: std::path::PathBuf,
}

impl TempHome {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "db9-e2e-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self { dir }
    }

    fn path(&self) -> &std::path::Path {
        &self.dir
    }

    fn write_credentials(&self, token: &str) {
        let db9_dir = self.dir.join(".db9");
        std::fs::create_dir_all(&db9_dir).unwrap();
        let cred_path = db9_dir.join("credentials");
        let mut f = std::fs::File::create(&cred_path).unwrap();
        writeln!(f, "token = \"{}\"", token).unwrap();
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

async fn setup() -> AppState {
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
        fs9_meta_url: None,
        fs9_meta_key: None,
        fs9_jwt_secret: None,
    };

    AppState {
        db: pool,
        config: Arc::new(config),
        sessions: Arc::new(SessionManager::new(1)),
        http_client: reqwest::Client::new(),
        fs9_client: None,
    }
}

async fn start_server() -> (std::net::SocketAddr, AppState) {
    let state = setup().await;
    let app = api::router().with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    // Give the server a moment to start accepting connections
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    (addr, state)
}

/// Register + login via HTTP, return the bearer token.
async fn register_and_login(state: &AppState, email: &str, password: &str) -> String {
    let app = api::router().with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/customer/register")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "email": email, "password": password })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let app = api::router().with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/customer/login")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&json!({ "email": email, "password": password })).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    body["token"].as_str().unwrap().to_string()
}

/// GET /customer/me -> customer id.
async fn get_customer_id(state: &AppState, token: &str) -> String {
    let app = api::router().with_state(state.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/customer/me")
                .header("Authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    body["id"].as_str().unwrap().to_string()
}

/// Seed a tenant and assign to a customer.
async fn seed_tenant(state: &AppState, customer_id: &str) -> String {
    let tenant_id = format!("t{}", &uuid::Uuid::new_v4().to_string()[..11]);
    let keyspace = format!("tipg_tenant_{tenant_id}");
    let now = chrono::Utc::now().to_rfc3339();
    db::insert_tenant(&state.db, &tenant_id, &keyspace, "ACTIVE", &now)
        .await
        .unwrap();
    db::set_tenant_customer_id(&state.db, &tenant_id, customer_id)
        .await
        .unwrap();
    db::upsert_credential(&state.db, &tenant_id, "admin", "admin", "testpass123", None)
        .await
        .unwrap();
    tenant_id
}

/// Build a db9 Command pointing at a test server with credential isolation.
fn db9_cmd(api_url: &str, home: &TempHome) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_db9"));
    cmd.arg("--api-url").arg(api_url);
    cmd.env("HOME", home.path());
    cmd.env("DB9_API_URL", api_url);
    cmd.stdin(std::process::Stdio::null());
    cmd
}

// ══════════════════════════════════════════════════════════════════
// Binary-only tests (no server needed)
// ══════════════════════════════════════════════════════════════════

#[test]
fn version_includes_git_hash() {
    let output = Command::new(env!("CARGO_BIN_EXE_db9"))
        .arg("--version")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("db9 "),
        "version should start with 'db9 ', got: {stdout}"
    );
    assert!(
        stdout.contains('(') && stdout.contains(')'),
        "version should contain git hash in parentheses, got: {stdout}"
    );
}

#[test]
fn help_lists_db_subcommand() {
    let output = Command::new(env!("CARGO_BIN_EXE_db9"))
        .arg("--help")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("db"),
        "help should mention 'db' subcommand, got: {stdout}"
    );
}

#[test]
fn db_sql_help_shows_query_and_file_flags() {
    let output = Command::new(env!("CARGO_BIN_EXE_db9"))
        .args(["db", "sql", "--help"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--query") || stdout.contains("-q"),
        "sql help should mention --query flag, got: {stdout}"
    );
    assert!(
        stdout.contains("--file") || stdout.contains("-f"),
        "sql help should mention --file flag, got: {stdout}"
    );
}

// ══════════════════════════════════════════════════════════════════
// Server-based tests
// ══════════════════════════════════════════════════════════════════

#[tokio::test(flavor = "multi_thread")]
async fn sql_query_no_auth_fails() {
    let (addr, _state) = start_server().await;
    let api_url = format!("http://{addr}");
    let home = TempHome::new();
    // No credentials written

    let output = db9_cmd(&api_url, &home)
        .args(["db", "sql", "nonexistent", "-q", "SELECT 1"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail without auth, stderr: {stderr}"
    );
    assert!(
        stderr.contains("Not logged in") || stderr.contains("login"),
        "should mention login requirement, got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_query_db_not_found() {
    let (addr, state) = start_server().await;
    let api_url = format!("http://{addr}");
    let home = TempHome::new();

    let email = format!("e2e-sql404-{}@test.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "TestPass123!").await;
    home.write_credentials(&token);

    let output = db9_cmd(&api_url, &home)
        .args(["db", "sql", "nonexistent123", "-q", "SELECT 1"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "should fail for nonexistent db, stderr: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("not found")
            || stderr.to_lowercase().contains("404"),
        "should mention not found, got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_query_with_tenant_reaches_execution() {
    let (addr, state) = start_server().await;
    let api_url = format!("http://{addr}");
    let home = TempHome::new();

    let email = format!("e2e-sqlexec-{}@test.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "TestPass123!").await;
    let customer_id = get_customer_id(&state, &token).await;
    let tenant_id = seed_tenant(&state, &customer_id).await;
    home.write_credentials(&token);

    let output = db9_cmd(&api_url, &home)
        .args(["db", "sql", &tenant_id, "-q", "SELECT 1"])
        .output()
        .unwrap();

    // The query will fail because there is no actual pg-tikv backend,
    // but it should get past auth and routing. The error should NOT be
    // "not found" or "not logged in".
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        stderr
    );
    let combined_lower = combined.to_lowercase();
    assert!(
        !combined_lower.contains("not logged in"),
        "should not fail on auth, got: {combined}"
    );
    assert!(
        !combined_lower.contains("not found"),
        "should not fail on 404, got: {combined}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_piped_stdin_does_not_enter_repl() {
    let (addr, state) = start_server().await;
    let api_url = format!("http://{addr}");
    let home = TempHome::new();

    let email = format!("e2e-pipe-{}@test.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "TestPass123!").await;
    let customer_id = get_customer_id(&state, &token).await;
    let tenant_id = seed_tenant(&state, &customer_id).await;
    home.write_credentials(&token);

    // Override stdin to piped (not null) so we can write SQL
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_db9"));
    cmd.arg("--api-url").arg(&api_url);
    cmd.env("HOME", home.path());
    cmd.env("DB9_API_URL", &api_url);
    cmd.args(["db", "sql", &tenant_id]);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"SELECT 1;\n").unwrap();
    }

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !stderr.contains("\\q to quit"),
        "piped stdin should NOT enter REPL mode, got stderr: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_empty_piped_stdin_shows_error() {
    let (addr, state) = start_server().await;
    let api_url = format!("http://{addr}");
    let home = TempHome::new();

    let email = format!("e2e-empty-{}@test.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "TestPass123!").await;
    let customer_id = get_customer_id(&state, &token).await;
    let tenant_id = seed_tenant(&state, &customer_id).await;
    home.write_credentials(&token);

    // Use piped stdin and close immediately to simulate empty input
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_db9"));
    cmd.arg("--api-url").arg(&api_url);
    cmd.env("HOME", home.path());
    cmd.env("DB9_API_URL", &api_url);
    cmd.args(["db", "sql", &tenant_id]);
    cmd.stdin(std::process::Stdio::piped());
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().unwrap();
    drop(child.stdin.take());

    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "empty piped stdin should fail, stderr: {stderr}"
    );
    assert!(
        stderr.to_lowercase().contains("no sql provided"),
        "should mention 'no sql provided', got: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn db_list_with_auth_succeeds() {
    let (addr, state) = start_server().await;
    let api_url = format!("http://{addr}");
    let home = TempHome::new();

    let email = format!("e2e-list-{}@test.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "TestPass123!").await;
    home.write_credentials(&token);

    let output = db9_cmd(&api_url, &home)
        .args(["db", "list"])
        .output()
        .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "db list should succeed with auth, stderr: {stderr}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn db_list_json_output() {
    let (addr, state) = start_server().await;
    let api_url = format!("http://{addr}");
    let home = TempHome::new();

    let email = format!("e2e-listjson-{}@test.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "TestPass123!").await;
    home.write_credentials(&token);

    let output = db9_cmd(&api_url, &home)
        .args(["--json", "db", "list"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&stdout).expect("output should be valid JSON");
    assert!(parsed.is_array(), "JSON output should be an array");
}

#[tokio::test(flavor = "multi_thread")]
async fn sql_query_with_file_flag() {
    let (addr, state) = start_server().await;
    let api_url = format!("http://{addr}");
    let home = TempHome::new();

    let email = format!("e2e-sqlfile-{}@test.com", uuid::Uuid::new_v4());
    let token = register_and_login(&state, &email, "TestPass123!").await;
    let customer_id = get_customer_id(&state, &token).await;
    let tenant_id = seed_tenant(&state, &customer_id).await;
    home.write_credentials(&token);

    // Write a temp SQL file
    let sql_file = home.path().join("test.sql");
    std::fs::write(&sql_file, "SELECT 1;").unwrap();

    let output = db9_cmd(&api_url, &home)
        .args(["db", "sql", &tenant_id, "-f", sql_file.to_str().unwrap()])
        .output()
        .unwrap();

    // Should get past auth and routing (will fail at PG layer)
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let combined_lower = combined.to_lowercase();
    assert!(
        !combined_lower.contains("not logged in"),
        "should not fail on auth, got: {combined}"
    );
    assert!(
        !combined_lower.contains("not found"),
        "should not fail on 404, got: {combined}"
    );
}
