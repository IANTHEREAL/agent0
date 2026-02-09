use serde::{Deserialize, Serialize};

// ── Request types ───────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateTenantRequest {
    pub admin_user: Option<String>,
    pub admin_password: Option<String>,
}

#[derive(Deserialize)]
pub struct TenantConnectRequest {
    pub admin_user: String,
    pub admin_password: String,
}

#[derive(Deserialize)]
pub struct TenantUpdateRequest {
    pub notes: Option<String>,
    pub tags: Option<Vec<String>>,
}

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: Option<String>,
    pub superuser: Option<bool>,
}

#[derive(Deserialize)]
pub struct SqlQueryRequest {
    pub sql: String,
}

#[derive(Deserialize)]
pub struct ListTenantsParams {
    pub page: Option<u32>,
    pub size: Option<u32>,
    pub state: Option<String>,
    pub q: Option<String>,
}

#[derive(Deserialize)]
pub struct AuditLogParams {
    pub tenant_id: Option<String>,
    pub operation_type: Option<String>,
    pub resource_type: Option<String>,
    pub success: Option<bool>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

// ── Response types ──────────────────────────────────────────────

#[derive(Serialize)]
pub struct TenantResponse {
    pub id: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoints: Option<Vec<Endpoint>>,
}

#[derive(Serialize)]
pub struct TenantListResponse {
    pub items: Vec<TenantResponse>,
    pub total: i64,
    pub page: u32,
    pub size: u32,
}

#[derive(Serialize)]
pub struct CreateTenantResponse {
    pub id: String,
    pub admin_user: String,
    pub admin_password: String,
    pub connection_string: String,
    pub created_at: String,
}

#[derive(Serialize)]
pub struct TenantConnectResponse {
    pub session_id: String,
    pub expires_at: String,
}

#[derive(Serialize, Clone)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
    #[serde(rename = "type")]
    pub ep_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    pub priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub enabled: bool,
}

#[derive(Serialize)]
pub struct UserResponse {
    pub name: String,
    pub is_superuser: bool,
    pub can_login: bool,
    pub can_create_db: bool,
    pub can_create_role: bool,
}

#[derive(Serialize)]
pub struct UserCreateResponse {
    pub username: String,
    pub password: String,
    pub connection: String,
}

#[derive(Serialize)]
pub struct PasswordResetResponse {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct MessageResponse {
    pub message: String,
}

#[derive(Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub pd_healthy: bool,
}

#[derive(Serialize)]
pub struct SqlQueryResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct AuditLogResponse {
    pub id: String,
    pub timestamp: String,
    pub operation_type: String,
    pub resource_type: String,
    pub resource_name: String,
    pub tenant_id: Option<String>,
    pub operator: Option<String>,
    pub success: bool,
    pub error_message: Option<String>,
    pub extra_metadata: Option<serde_json::Value>,
}

#[derive(Serialize)]
pub struct ObservabilitySummary {
    pub window_seconds: i64,
    pub statement_count: i64,
    pub txn_commit_count: i64,
    pub error_count: i64,
    pub qps: f64,
    pub tps: f64,
    pub latency_avg_ms: f64,
    pub latency_p99_ms: f64,
    pub active_connections: i64,
}

#[derive(Serialize)]
pub struct QuerySample {
    pub query: String,
    pub sample_count: i64,
    pub error_count: i64,
    pub latency_avg_ms: f64,
    pub latency_p99_ms: f64,
    pub latency_max_ms: f64,
    pub last_seen_ms_ago: i64,
}

#[derive(Serialize)]
pub struct TenantObservabilityResponse {
    pub summary: ObservabilitySummary,
    pub samples: Vec<QuerySample>,
}

// ── DB row types ────────────────────────────────────────────────

pub struct TenantRow {
    pub id: String,
    pub keyspace: String,
    pub state: String,
    pub state_reason: Option<String>,
    pub created_at: String,
    pub created_by: Option<String>,
    pub notes: Option<String>,
    pub tags: Option<String>,
    pub updated_at: Option<String>,
}

impl TenantRow {
    pub fn to_response(&self) -> TenantResponse {
        let tags: Option<Vec<String>> = self
            .tags
            .as_ref()
            .and_then(|t| serde_json::from_str(t).ok());
        TenantResponse {
            id: self.id.clone(),
            state: self.state.clone(),
            created_at: Some(self.created_at.clone()),
            created_by: self.created_by.clone(),
            notes: self.notes.clone(),
            tags,
            updated_at: self.updated_at.clone(),
            state_reason: self.state_reason.clone(),
            endpoints: None,
        }
    }
}

pub struct CredentialRow {
    pub id: String,
    pub tenant_id: String,
    pub credential_type: String,
    pub username: String,
    pub password_enc: String,
}
