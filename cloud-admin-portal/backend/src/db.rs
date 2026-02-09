use sqlx::any::AnyPoolOptions;
use sqlx::{AnyPool, Row};

use crate::crypto;
use crate::models::{CredentialRow, TenantRow};
use crate::tenant_state;

fn encrypt_password(password: &str, key: Option<&str>) -> String {
    match key {
        Some(k) => crypto::encrypt(password, k).unwrap_or_else(|e| {
            tracing::error!("credential encryption failed: {e}");
            password.to_string()
        }),
        None => password.to_string(),
    }
}

fn decrypt_password(stored: &str, key: Option<&str>) -> String {
    match key {
        Some(k) => crypto::decrypt(stored, k).unwrap_or_else(|e| {
            tracing::warn!("credential decryption failed (might be plaintext): {e}");
            stored.to_string()
        }),
        None => stored.to_string(),
    }
}

pub async fn connect(url: &str) -> Result<AnyPool, sqlx::Error> {
    sqlx::any::install_default_drivers();

    if url.starts_with("sqlite") {
        if let Some(path) = url.strip_prefix("sqlite://") {
            let path = path.split('?').next().unwrap_or(path);
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
        }
    }

    let pool = AnyPoolOptions::new()
        .max_connections(10)
        .connect(url)
        .await?;
    Ok(pool)
}

fn is_sqlite(pool: &AnyPool) -> bool {
    // Detect SQLite by checking the connection URL pattern
    // AnyPool doesn't expose the backend kind directly in newer sqlx
    format!("{:?}", pool).contains("Sqlite")
        || format!("{:?}", pool).contains("sqlite")
}

pub fn adapt_sql(sql: &str, pool: &AnyPool) -> String {
    if is_sqlite(pool) {
        let mut result = sql.to_string();
        for i in (1..=30).rev() {
            result = result.replace(&format!("${i}"), "?");
        }
        result
    } else {
        sql.to_string()
    }
}

pub async fn create_tables(pool: &AnyPool) -> Result<(), sqlx::Error> {
    let is_pg = !is_sqlite(pool);

    let tenants_ddl = if is_pg {
        "CREATE TABLE IF NOT EXISTS tenants (
            id TEXT PRIMARY KEY,
            keyspace TEXT NOT NULL UNIQUE,
            state TEXT NOT NULL DEFAULT 'ACTIVE',
            state_reason TEXT,
            created_at TEXT NOT NULL,
            created_by TEXT,
            notes TEXT,
            tags TEXT,
            updated_at TEXT
        )"
    } else {
        "CREATE TABLE IF NOT EXISTS tenants (
            id TEXT PRIMARY KEY,
            keyspace TEXT NOT NULL UNIQUE,
            state TEXT NOT NULL DEFAULT 'ACTIVE',
            state_reason TEXT,
            created_at TEXT NOT NULL,
            created_by TEXT,
            notes TEXT,
            tags TEXT,
            updated_at TEXT
        )"
    };

    let creds_ddl = "CREATE TABLE IF NOT EXISTS tenant_credentials (
        id TEXT PRIMARY KEY,
        tenant_id TEXT NOT NULL,
        credential_type TEXT NOT NULL,
        username TEXT NOT NULL,
        password_plain TEXT NOT NULL,
        key_version INTEGER NOT NULL DEFAULT 1,
        created_at TEXT NOT NULL,
        rotated_at TEXT,
        UNIQUE(tenant_id, credential_type, username)
    )";

    let audit_ddl = "CREATE TABLE IF NOT EXISTS audit_logs (
        id TEXT PRIMARY KEY,
        timestamp TEXT NOT NULL,
        operation_type TEXT NOT NULL,
        resource_type TEXT NOT NULL,
        resource_name TEXT NOT NULL,
        tenant_id TEXT,
        operator TEXT,
        success INTEGER NOT NULL,
        error_message TEXT,
        extra_metadata TEXT
    )";

    sqlx::query(tenants_ddl).execute(pool).await?;
    sqlx::query(creds_ddl).execute(pool).await?;
    sqlx::query(audit_ddl).execute(pool).await?;

    let indexes = [
        "CREATE INDEX IF NOT EXISTS idx_tenants_state ON tenants(state)",
        "CREATE INDEX IF NOT EXISTS idx_creds_tenant ON tenant_credentials(tenant_id)",
        "CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_logs(timestamp)",
        "CREATE INDEX IF NOT EXISTS idx_audit_tenant ON audit_logs(tenant_id)",
        "CREATE INDEX IF NOT EXISTS idx_audit_op ON audit_logs(operation_type)",
    ];
    for idx in indexes {
        sqlx::query(idx).execute(pool).await?;
    }

    Ok(())
}

// ── Tenant queries ──────────────────────────────────────────────

fn row_to_tenant(row: &sqlx::any::AnyRow) -> TenantRow {
    TenantRow {
        id: row.get("id"),
        keyspace: row.get("keyspace"),
        state: row.get("state"),
        state_reason: row.get("state_reason"),
        created_at: row.get("created_at"),
        created_by: row.get("created_by"),
        notes: row.get("notes"),
        tags: row.get("tags"),
        updated_at: row.get("updated_at"),
    }
}

pub async fn list_tenants(
    pool: &AnyPool,
    page: u32,
    size: u32,
    state_filter: Option<&str>,
    q: Option<&str>,
) -> Result<(Vec<TenantRow>, i64), sqlx::Error> {
    let offset = (page - 1) * size;

    let mut count_sql = format!("SELECT COUNT(*) as cnt FROM tenants WHERE state != '{}'", tenant_state::CREATE_FAILED);
    let mut list_sql = format!("SELECT * FROM tenants WHERE state != '{}'", tenant_state::CREATE_FAILED);
    let mut binds: Vec<String> = Vec::new();
    let mut param_idx = 1;

    if let Some(st) = state_filter {
        count_sql.push_str(&format!(" AND state = ${param_idx}"));
        list_sql.push_str(&format!(" AND state = ${param_idx}"));
        binds.push(st.to_string());
        param_idx += 1;
    }
    if let Some(query) = q {
        count_sql.push_str(&format!(" AND id LIKE ${param_idx}"));
        list_sql.push_str(&format!(" AND id LIKE ${param_idx}"));
        binds.push(format!("%{query}%"));
        param_idx += 1;
    }

    list_sql.push_str(&format!(" ORDER BY created_at DESC LIMIT ${param_idx} OFFSET ${}", param_idx + 1));
    let _ = param_idx;

    let count_sql = adapt_sql(&count_sql, pool);
    let list_sql = adapt_sql(&list_sql, pool);

    let mut count_q = sqlx::query(&count_sql);
    for b in &binds {
        count_q = count_q.bind(b);
    }
    let total: i64 = count_q.fetch_one(pool).await?.get("cnt");

    let mut list_q = sqlx::query(&list_sql);
    for b in &binds {
        list_q = list_q.bind(b);
    }
    list_q = list_q.bind(size as i64).bind(offset as i64);

    let rows = list_q.fetch_all(pool).await?;
    let tenants: Vec<TenantRow> = rows.iter().map(row_to_tenant).collect();
    Ok((tenants, total))
}

pub async fn get_tenant(pool: &AnyPool, tenant_id: &str) -> Result<Option<TenantRow>, sqlx::Error> {
    let sql = adapt_sql(
        &format!("SELECT * FROM tenants WHERE id = $1 AND state NOT IN ('{}', '{}')", tenant_state::DISABLED, tenant_state::CREATE_FAILED),
        pool,
    );
    let row = sqlx::query(&sql).bind(tenant_id).fetch_optional(pool).await?;
    Ok(row.as_ref().map(row_to_tenant))
}

pub async fn get_tenant_by_id(pool: &AnyPool, tenant_id: &str) -> Result<Option<TenantRow>, sqlx::Error> {
    let sql = adapt_sql("SELECT * FROM tenants WHERE id = $1", pool);
    let row = sqlx::query(&sql).bind(tenant_id).fetch_optional(pool).await?;
    Ok(row.as_ref().map(row_to_tenant))
}

pub async fn insert_tenant(
    pool: &AnyPool,
    id: &str,
    keyspace: &str,
    state: &str,
    created_at: &str,
) -> Result<(), sqlx::Error> {
    let sql = adapt_sql(
        "INSERT INTO tenants (id, keyspace, state, created_at) VALUES ($1, $2, $3, $4)",
        pool,
    );
    sqlx::query(&sql)
        .bind(id)
        .bind(keyspace)
        .bind(state)
        .bind(created_at)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_tenant_state(
    pool: &AnyPool,
    id: &str,
    state: &str,
    reason: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let sql = adapt_sql(
        "UPDATE tenants SET state = $1, state_reason = $2, updated_at = $3 WHERE id = $4",
        pool,
    );
    sqlx::query(&sql)
        .bind(state)
        .bind(reason)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn update_tenant_metadata(
    pool: &AnyPool,
    id: &str,
    notes: Option<&str>,
    tags: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let sql = adapt_sql(
        "UPDATE tenants SET notes = $1, tags = $2, updated_at = $3 WHERE id = $4",
        pool,
    );
    sqlx::query(&sql).bind(notes).bind(tags).bind(&now).bind(id).execute(pool).await?;
    Ok(())
}

// ── Credential queries ──────────────────────────────────────────

pub async fn get_credential(
    pool: &AnyPool,
    tenant_id: &str,
    cred_type: &str,
    credential_key: Option<&str>,
) -> Result<Option<CredentialRow>, sqlx::Error> {
    let sql = adapt_sql(
        "SELECT id, tenant_id, credential_type, username, password_plain FROM tenant_credentials WHERE tenant_id = $1 AND credential_type = $2",
        pool,
    );
    let row = sqlx::query(&sql)
        .bind(tenant_id)
        .bind(cred_type)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|r| {
        let stored: String = r.get("password_plain");
        CredentialRow {
            id: r.get("id"),
            tenant_id: r.get("tenant_id"),
            credential_type: r.get("credential_type"),
            username: r.get("username"),
            password_plain: decrypt_password(&stored, credential_key),
        }
    }))
}

pub async fn upsert_credential(
    pool: &AnyPool,
    tenant_id: &str,
    cred_type: &str,
    username: &str,
    password: &str,
    credential_key: Option<&str>,
) -> Result<(), sqlx::Error> {
    let encrypted = encrypt_password(password, credential_key);
    let existing = get_credential(pool, tenant_id, cred_type, credential_key).await?;
    let now = chrono::Utc::now().to_rfc3339();

    if let Some(cred) = existing {
        let sql = adapt_sql(
            "UPDATE tenant_credentials SET password_plain = $1, rotated_at = $2 WHERE id = $3",
            pool,
        );
        sqlx::query(&sql).bind(&encrypted).bind(&now).bind(&cred.id).execute(pool).await?;
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let sql = adapt_sql(
            "INSERT INTO tenant_credentials (id, tenant_id, credential_type, username, password_plain, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
            pool,
        );
        sqlx::query(&sql)
            .bind(&id)
            .bind(tenant_id)
            .bind(cred_type)
            .bind(username)
            .bind(&encrypted)
            .bind(&now)
            .execute(pool)
            .await?;
    }
    Ok(())
}

// ── Audit queries ───────────────────────────────────────────────

pub async fn insert_audit_log(
    pool: &AnyPool,
    operation_type: &str,
    resource_type: &str,
    resource_name: &str,
    tenant_id: Option<&str>,
    operator: Option<&str>,
    success: bool,
    error_message: Option<&str>,
    extra_metadata: Option<&str>,
) -> Result<(), sqlx::Error> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let sql = adapt_sql(
        "INSERT INTO audit_logs (id, timestamp, operation_type, resource_type, resource_name, tenant_id, operator, success, error_message, extra_metadata) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        pool,
    );
    sqlx::query(&sql)
        .bind(&id)
        .bind(&now)
        .bind(operation_type)
        .bind(resource_type)
        .bind(resource_name)
        .bind(tenant_id)
        .bind(operator)
        .bind(success as i32)
        .bind(error_message)
        .bind(extra_metadata)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn query_audit_logs(
    pool: &AnyPool,
    tenant_id: Option<&str>,
    operation_type: Option<&str>,
    resource_type: Option<&str>,
    success: Option<bool>,
    limit: u32,
    offset: u32,
) -> Result<Vec<crate::models::AuditLogResponse>, sqlx::Error> {
    let mut sql = String::from("SELECT * FROM audit_logs WHERE 1=1");
    let mut binds: Vec<String> = Vec::new();
    let mut idx = 1;

    if let Some(tid) = tenant_id {
        sql.push_str(&format!(" AND tenant_id = ${idx}"));
        binds.push(tid.to_string());
        idx += 1;
    }
    if let Some(op) = operation_type {
        sql.push_str(&format!(" AND operation_type = ${idx}"));
        binds.push(op.to_string());
        idx += 1;
    }
    if let Some(rt) = resource_type {
        sql.push_str(&format!(" AND resource_type = ${idx}"));
        binds.push(rt.to_string());
        idx += 1;
    }
    if let Some(s) = success {
        sql.push_str(&format!(" AND success = ${idx}"));
        binds.push((s as i32).to_string());
        idx += 1;
    }

    sql.push_str(&format!(" ORDER BY timestamp DESC LIMIT ${idx} OFFSET ${}", idx + 1));

    let adapted = adapt_sql(&sql, pool);
    let mut q = sqlx::query(&adapted);
    for b in &binds {
        q = q.bind(b);
    }
    q = q.bind(limit as i64).bind(offset as i64);

    let rows = q.fetch_all(pool).await?;
    let mut logs = Vec::new();
    for r in &rows {
        let success_int: i32 = r.get("success");
        let meta: Option<String> = r.get("extra_metadata");
        logs.push(crate::models::AuditLogResponse {
            id: r.get("id"),
            timestamp: r.get("timestamp"),
            operation_type: r.get("operation_type"),
            resource_type: r.get("resource_type"),
            resource_name: r.get("resource_name"),
            tenant_id: r.get("tenant_id"),
            operator: r.get("operator"),
            success: success_int != 0,
            error_message: r.get("error_message"),
            extra_metadata: meta.and_then(|m| serde_json::from_str(m.as_str()).ok()),
        });
    }
    Ok(logs)
}

// ── Reconciler queries ──────────────────────────────────────────

pub async fn get_stuck_tenants(pool: &AnyPool, state: &str, before: &str) -> Result<Vec<TenantRow>, sqlx::Error> {
    let sql = adapt_sql(
        "SELECT * FROM tenants WHERE state = $1 AND created_at < $2",
        pool,
    );
    let rows = sqlx::query(&sql).bind(state).bind(before).fetch_all(pool).await?;
    Ok(rows.iter().map(row_to_tenant).collect())
}

pub async fn get_all_keyspaces_from_db(pool: &AnyPool) -> Result<Vec<(String, String)>, sqlx::Error> {
    let sql = format!("SELECT id, keyspace FROM tenants WHERE state NOT IN ('{}', '{}')", tenant_state::DISABLED, tenant_state::CREATE_FAILED);
    let rows = sqlx::query(&sql)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(|r| (r.get::<String, _>("id"), r.get::<String, _>("keyspace"))).collect())
}
