use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

use db9_admin::config::Config;
use db9_admin::session::SessionManager;
use db9_admin::{api, db, AppState};

async fn setup() -> (Router, AppState) {
    let db_id = uuid::Uuid::new_v4().to_string();
    let url = format!("sqlite:///tmp/db9_test_{db_id}.db?mode=rwc");
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
        fs9_server_url: None,
    };

    let state = AppState {
        db: pool,
        config: Arc::new(config),
        sessions: Arc::new(SessionManager::new(1)),
        device_codes: Arc::new(db9_admin::device_code::DeviceCodeStore::new(600)),
        http_client: reqwest::Client::new(),
        fs9_client: None,
    };

    let app = api::router().with_state(state.clone());
    (app, state)
}

async fn seed_tenants(state: &AppState, count: usize) -> Vec<String> {
    let mut ids = Vec::new();
    for i in 0..count {
        let id = format!("tenant{i:06}");
        let keyspace = format!("db9_tenant_{id}");
        let ts = format!("2025-01-{:02}T00:00:00+00:00", (i % 28) + 1);
        db::insert_tenant(&state.db, &id, &keyspace, "ACTIVE", &ts)
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

async fn seed_tenant_full(
    state: &AppState,
    id: &str,
    state_str: &str,
    ts: &str,
    notes: Option<&str>,
    tags: Option<&str>,
) {
    let keyspace = format!("db9_tenant_{id}");
    db::insert_tenant(&state.db, id, &keyspace, state_str, ts)
        .await
        .unwrap();
    if notes.is_some() || tags.is_some() {
        db::update_tenant_metadata(&state.db, id, notes, tags)
            .await
            .unwrap();
    }
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

// ── Offset pagination ────────────────────────────────────────────

#[tokio::test]
async fn list_offset_pagination_basic() {
    let (app, state) = setup().await;
    seed_tenants(&state, 12).await;

    let (status, data) = get_json(app, "/tenants?page=1&size=5").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["items"].as_array().unwrap().len(), 5);
    assert_eq!(data["total"].as_i64().unwrap(), 12);
    assert_eq!(data["page"].as_u64().unwrap(), 1);
    assert_eq!(data["size"].as_u64().unwrap(), 5);
}

#[tokio::test]
async fn list_offset_pagination_last_page() {
    let (app, state) = setup().await;
    seed_tenants(&state, 12).await;

    let (status, data) = get_json(app, "/tenants?page=3&size=5").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["items"].as_array().unwrap().len(), 2);
    assert_eq!(data["total"].as_i64().unwrap(), 12);
}

#[tokio::test]
async fn list_empty() {
    let (app, _state) = setup().await;

    let (status, data) = get_json(app, "/tenants?page=1&size=10").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["items"].as_array().unwrap().len(), 0);
    assert_eq!(data["total"].as_i64().unwrap(), 0);
}

// ── Cursor pagination ────────────────────────────────────────────

#[tokio::test]
async fn cursor_pagination_returns_next_cursor() {
    let (app, state) = setup().await;
    seed_tenants(&state, 10).await;

    let (status, data) = get_json(app, "/tenants?size=5").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["items"].as_array().unwrap().len(), 5);
    assert!(
        data["next_cursor"].is_string(),
        "should have next_cursor when more results"
    );
}

#[tokio::test]
async fn cursor_pagination_no_cursor_on_last_page() {
    let (app, state) = setup().await;
    seed_tenants(&state, 3).await;

    let (status, data) = get_json(app, "/tenants?size=10").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["items"].as_array().unwrap().len(), 3);
    assert!(data["next_cursor"].is_null(), "no next_cursor on last page");
}

#[tokio::test]
async fn cursor_pagination_walk_all_pages() {
    let (_, state) = setup().await;
    seed_tenants(&state, 20).await;

    let mut all_ids: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;

    loop {
        let (app, _) = (api::router().with_state(state.clone()), ());
        let uri = match &cursor {
            Some(c) => format!("/tenants?size=7&cursor={c}"),
            None => "/tenants?size=7".to_string(),
        };
        let (status, data) = get_json(app, &uri).await;
        assert_eq!(status, StatusCode::OK);

        let items = data["items"].as_array().unwrap();
        for item in items {
            all_ids.push(item["id"].as_str().unwrap().to_string());
        }

        pages += 1;

        match data["next_cursor"].as_str() {
            Some(nc) => cursor = Some(nc.to_string()),
            None => break,
        }
    }

    assert_eq!(all_ids.len(), 20, "should retrieve all 20 tenants");
    let deduped: std::collections::HashSet<&String> = all_ids.iter().collect();
    assert_eq!(deduped.len(), 20, "no duplicates across pages");
    assert_eq!(pages, 3, "20 items / 7 per page = 3 pages");
}

#[tokio::test]
async fn cursor_pagination_returns_negative_total() {
    let (_, state) = setup().await;
    seed_tenants(&state, 5).await;

    let app = api::router().with_state(state.clone());
    let (_, first_page) = get_json(app, "/tenants?size=3").await;
    let nc = first_page["next_cursor"].as_str().unwrap();

    let app2 = api::router().with_state(state.clone());
    let (_, second_page) = get_json(app2, &format!("/tenants?size=3&cursor={nc}")).await;
    assert_eq!(
        second_page["total"].as_i64().unwrap(),
        -1,
        "cursor mode skips COUNT"
    );
}

// ── Full-text search ─────────────────────────────────────────────

#[tokio::test]
async fn search_by_id() {
    let (app, state) = setup().await;
    seed_tenants(&state, 5).await;

    let (status, data) = get_json(app, "/tenants?q=tenant000003").await;
    assert_eq!(status, StatusCode::OK);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), "tenant000003");
}

#[tokio::test]
async fn search_by_notes() {
    let (app, state) = setup().await;
    seed_tenant_full(
        &state,
        "abc123456789",
        "ACTIVE",
        "2025-06-01T00:00:00+00:00",
        Some("production database for billing"),
        None,
    )
    .await;
    seed_tenant_full(
        &state,
        "def987654321",
        "ACTIVE",
        "2025-06-02T00:00:00+00:00",
        Some("staging environment"),
        None,
    )
    .await;

    let (status, data) = get_json(app, "/tenants?q=billing").await;
    assert_eq!(status, StatusCode::OK);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), "abc123456789");
}

#[tokio::test]
async fn search_by_tags() {
    let (app, state) = setup().await;
    seed_tenant_full(
        &state,
        "tag_tenant_01",
        "ACTIVE",
        "2025-06-01T00:00:00+00:00",
        None,
        Some(r#"["production","us-east"]"#),
    )
    .await;
    seed_tenant_full(
        &state,
        "tag_tenant_02",
        "ACTIVE",
        "2025-06-02T00:00:00+00:00",
        None,
        Some(r#"["staging","eu-west"]"#),
    )
    .await;

    let (status, data) = get_json(app, "/tenants?q=us-east").await;
    assert_eq!(status, StatusCode::OK);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), "tag_tenant_01");
}

// ── Tag filter ───────────────────────────────────────────────────

#[tokio::test]
async fn filter_by_tag() {
    let (app, state) = setup().await;
    seed_tenant_full(
        &state,
        "tagged_prod01",
        "ACTIVE",
        "2025-06-01T00:00:00+00:00",
        None,
        Some(r#"["production","us-east"]"#),
    )
    .await;
    seed_tenant_full(
        &state,
        "tagged_stg01",
        "ACTIVE",
        "2025-06-02T00:00:00+00:00",
        None,
        Some(r#"["staging","eu-west"]"#),
    )
    .await;
    seed_tenant_full(
        &state,
        "tagged_prod02",
        "ACTIVE",
        "2025-06-03T00:00:00+00:00",
        None,
        Some(r#"["production","ap-south"]"#),
    )
    .await;

    let (status, data) = get_json(app, "/tenants?tag=production").await;
    assert_eq!(status, StatusCode::OK);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
}

#[tokio::test]
async fn filter_by_tag_no_match() {
    let (app, state) = setup().await;
    seed_tenant_full(
        &state,
        "tagged_only01",
        "ACTIVE",
        "2025-06-01T00:00:00+00:00",
        None,
        Some(r#"["staging"]"#),
    )
    .await;

    let (status, data) = get_json(app, "/tenants?tag=production").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["items"].as_array().unwrap().len(), 0);
}

// ── State filter ─────────────────────────────────────────────────

#[tokio::test]
async fn filter_by_state() {
    let (app, state) = setup().await;
    seed_tenant_full(
        &state,
        "active_one_01",
        "ACTIVE",
        "2025-06-01T00:00:00+00:00",
        None,
        None,
    )
    .await;
    seed_tenant_full(
        &state,
        "suspend_one1",
        "SUSPENDED",
        "2025-06-02T00:00:00+00:00",
        None,
        None,
    )
    .await;
    seed_tenant_full(
        &state,
        "active_two_01",
        "ACTIVE",
        "2025-06-03T00:00:00+00:00",
        None,
        None,
    )
    .await;

    let (status, data) = get_json(app, "/tenants?state=SUSPENDED").await;
    assert_eq!(status, StatusCode::OK);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), "suspend_one1");
}

// ── Combined filters ─────────────────────────────────────────────

#[tokio::test]
async fn combined_state_and_tag_filter() {
    let (app, state) = setup().await;
    seed_tenant_full(
        &state,
        "combo_act_pr",
        "ACTIVE",
        "2025-06-01T00:00:00+00:00",
        None,
        Some(r#"["production"]"#),
    )
    .await;
    seed_tenant_full(
        &state,
        "combo_sus_pr",
        "SUSPENDED",
        "2025-06-02T00:00:00+00:00",
        None,
        Some(r#"["production"]"#),
    )
    .await;
    seed_tenant_full(
        &state,
        "combo_act_st",
        "ACTIVE",
        "2025-06-03T00:00:00+00:00",
        None,
        Some(r#"["staging"]"#),
    )
    .await;

    let (status, data) = get_json(app, "/tenants?state=ACTIVE&tag=production").await;
    assert_eq!(status, StatusCode::OK);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), "combo_act_pr");
}

#[tokio::test]
async fn combined_search_and_tag_filter() {
    let (app, state) = setup().await;
    seed_tenant_full(
        &state,
        "search_tag_01",
        "ACTIVE",
        "2025-06-01T00:00:00+00:00",
        Some("billing service"),
        Some(r#"["production"]"#),
    )
    .await;
    seed_tenant_full(
        &state,
        "search_tag_02",
        "ACTIVE",
        "2025-06-02T00:00:00+00:00",
        Some("billing service"),
        Some(r#"["staging"]"#),
    )
    .await;

    let (status, data) = get_json(app, "/tenants?q=billing&tag=production").await;
    assert_eq!(status, StatusCode::OK);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), "search_tag_01");
}

// ── Batch update ─────────────────────────────────────────────────

#[tokio::test]
async fn batch_update_sets_notes_and_tags() {
    let (app, state) = setup().await;
    seed_tenants(&state, 3).await;

    let (status, data) = post_json(
        app,
        "/tenants/batch-update",
        &json!({
            "ids": ["tenant000000", "tenant000001"],
            "notes": "updated via batch",
            "tags": ["production", "us-east"]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["updated"].as_array().unwrap().len(), 2);
    assert!(data["failed"].as_array().unwrap().is_empty());

    let app2 = api::router().with_state(state.clone());
    let (_, t0) = get_json(app2, "/tenants/tenant000000").await;
    assert_eq!(t0["notes"].as_str().unwrap(), "updated via batch");
    assert_eq!(t0["tags"].as_array().unwrap().len(), 2);

    let app3 = api::router().with_state(state.clone());
    let (_, t2) = get_json(app3, "/tenants/tenant000002").await;
    assert!(
        t2["notes"].is_null(),
        "unchanged tenant should not have notes"
    );
}

#[tokio::test]
async fn batch_update_partial_failure() {
    let (app, state) = setup().await;
    seed_tenants(&state, 1).await;

    let (status, data) = post_json(
        app,
        "/tenants/batch-update",
        &json!({
            "ids": ["tenant000000", "nonexistent1"],
            "notes": "partial test"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(data["updated"].as_array().unwrap().len(), 1);
    assert_eq!(data["failed"].as_array().unwrap().len(), 1);
    assert_eq!(data["failed"][0]["id"].as_str().unwrap(), "nonexistent1");
}

// ── Batch validation ─────────────────────────────────────────────

#[tokio::test]
async fn batch_create_rejects_zero_count() {
    let (app, _state) = setup().await;

    let (status, data) = post_json(app, "/tenants/batch", &json!({ "count": 0 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(data["message"].as_str().unwrap().contains("count"));
}

#[tokio::test]
async fn batch_create_rejects_over_1000() {
    let (app, _state) = setup().await;

    let (status, data) = post_json(app, "/tenants/batch", &json!({ "count": 1001 })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(data["message"].as_str().unwrap().contains("count"));
}

#[tokio::test]
async fn batch_delete_rejects_empty_ids() {
    let (app, _state) = setup().await;

    let (status, _) = post_json(app, "/tenants/batch-delete", &json!({ "ids": [] })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn batch_update_rejects_empty_ids() {
    let (app, _state) = setup().await;

    let (status, _) = post_json(
        app,
        "/tenants/batch-update",
        &json!({ "ids": [], "notes": "x" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ── Batch delete ─────────────────────────────────────────────────

#[tokio::test]
async fn batch_delete_nonexistent_returns_failures() {
    let (app, _state) = setup().await;

    let (status, data) = post_json(
        app,
        "/tenants/batch-delete",
        &json!({ "ids": ["ghost1ghost1", "ghost2ghost2"] }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(data["deleted"].as_array().unwrap().is_empty());
    assert_eq!(data["failed"].as_array().unwrap().len(), 2);
}

// ── CREATE_FAILED exclusion ──────────────────────────────────────

#[tokio::test]
async fn list_excludes_create_failed() {
    let (app, state) = setup().await;
    seed_tenant_full(
        &state,
        "good_tenant1",
        "ACTIVE",
        "2025-06-01T00:00:00+00:00",
        None,
        None,
    )
    .await;
    seed_tenant_full(
        &state,
        "bad_tenant_1",
        "CREATE_FAILED",
        "2025-06-02T00:00:00+00:00",
        None,
        None,
    )
    .await;

    let (status, data) = get_json(app, "/tenants?size=100").await;
    assert_eq!(status, StatusCode::OK);
    let items = data["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str().unwrap(), "good_tenant1");
}

// ── Sort order ───────────────────────────────────────────────────

#[tokio::test]
async fn list_returns_newest_first() {
    let (app, state) = setup().await;
    seed_tenant_full(
        &state,
        "oldest_ten_1",
        "ACTIVE",
        "2025-01-01T00:00:00+00:00",
        None,
        None,
    )
    .await;
    seed_tenant_full(
        &state,
        "newest_ten_1",
        "ACTIVE",
        "2025-12-31T00:00:00+00:00",
        None,
        None,
    )
    .await;
    seed_tenant_full(
        &state,
        "middle_ten_1",
        "ACTIVE",
        "2025-06-15T00:00:00+00:00",
        None,
        None,
    )
    .await;

    let (_, data) = get_json(app, "/tenants?size=10").await;
    let items = data["items"].as_array().unwrap();
    assert_eq!(items[0]["id"].as_str().unwrap(), "newest_ten_1");
    assert_eq!(items[1]["id"].as_str().unwrap(), "middle_ten_1");
    assert_eq!(items[2]["id"].as_str().unwrap(), "oldest_ten_1");
}
