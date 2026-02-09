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

    let is_sqlite = url.starts_with("sqlite");

    if is_sqlite {
        if let Some(path) = url.strip_prefix("sqlite://") {
            let path = path.split('?').next().unwrap_or(path);
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
        }
    }

    let max_conns = if is_sqlite { 5 } else { 20 };
    let pool = AnyPoolOptions::new()
        .max_connections(max_conns)
        .connect(url)
        .await?;

    if is_sqlite {
        sqlx::query("PRAGMA journal_mode=WAL")
            .execute(&pool)
            .await
            .ok();
        sqlx::query("PRAGMA busy_timeout=5000")
            .execute(&pool)
            .await
            .ok();
        sqlx::query("PRAGMA synchronous=NORMAL")
            .execute(&pool)
            .await
            .ok();
    }

    Ok(pool)
}

fn is_sqlite(pool: &AnyPool) -> bool {
    // Detect SQLite by checking the connection URL pattern
    // AnyPool doesn't expose the backend kind directly in newer sqlx
    format!("{:?}", pool).contains("Sqlite") || format!("{:?}", pool).contains("sqlite")
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
        "CREATE INDEX IF NOT EXISTS idx_tenants_created ON tenants(created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_tenants_state_created ON tenants(state, created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_tenants_cursor ON tenants(created_at DESC, id DESC)",
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

pub struct ListTenantsOpts<'a> {
    pub page: u32,
    pub size: u32,
    pub state_filter: Option<&'a str>,
    pub q: Option<&'a str>,
    pub tag: Option<&'a str>,
    pub cursor: Option<(&'a str, &'a str)>,
}

pub struct ListTenantsResult {
    pub tenants: Vec<TenantRow>,
    pub total: i64,
    pub next_cursor: Option<(String, String)>,
}

pub async fn list_tenants(
    pool: &AnyPool,
    opts: &ListTenantsOpts<'_>,
) -> Result<ListTenantsResult, sqlx::Error> {
    let base_where = format!("state != '{}'", tenant_state::CREATE_FAILED);
    let mut filters = Vec::new();
    let mut binds: Vec<String> = Vec::new();
    let mut param_idx = 1;

    if let Some(st) = opts.state_filter {
        filters.push(format!("state = ${param_idx}"));
        binds.push(st.to_string());
        param_idx += 1;
    }
    if let Some(query) = opts.q {
        let pattern = format!("%{query}%");
        filters.push(format!(
            "(id LIKE ${p} OR COALESCE(notes,'') LIKE ${p} OR COALESCE(tags,'') LIKE ${p})",
            p = param_idx
        ));
        binds.push(pattern);
        param_idx += 1;
    }
    if let Some(tag) = opts.tag {
        filters.push(format!("COALESCE(tags,'') LIKE ${param_idx}"));
        binds.push(format!("%\"{tag}\"%"));
        param_idx += 1;
    }

    let where_clause = if filters.is_empty() {
        base_where
    } else {
        format!("{base_where} AND {}", filters.join(" AND "))
    };

    let use_cursor = opts.cursor.is_some();

    let total = if use_cursor {
        -1
    } else {
        let count_sql = adapt_sql(
            &format!("SELECT COUNT(*) as cnt FROM tenants WHERE {where_clause}"),
            pool,
        );
        let mut count_q = sqlx::query(&count_sql);
        for b in &binds {
            count_q = count_q.bind(b);
        }
        count_q.fetch_one(pool).await?.get("cnt")
    };

    let mut list_where = where_clause.clone();
    if let Some((cursor_ts, cursor_id)) = opts.cursor {
        list_where.push_str(&format!(
            " AND (created_at < ${p1} OR (created_at = ${p1} AND id < ${p2}))",
            p1 = param_idx,
            p2 = param_idx + 1
        ));
        binds.push(cursor_ts.to_string());
        binds.push(cursor_id.to_string());
        param_idx = param_idx + 2;
    }

    let mut extra_binds: Vec<i64> = Vec::new();

    let list_sql = if use_cursor {
        let sql = format!(
            "SELECT * FROM tenants WHERE {list_where} ORDER BY created_at DESC, id DESC LIMIT ${param_idx}"
        );
        extra_binds.push((opts.size + 1) as i64);
        adapt_sql(&sql, pool)
    } else {
        let offset = ((opts.page.max(1)) - 1) * opts.size;
        let sql = format!(
            "SELECT * FROM tenants WHERE {list_where} ORDER BY created_at DESC, id DESC LIMIT ${} OFFSET ${}",
            param_idx,
            param_idx + 1
        );
        extra_binds.push(opts.size as i64);
        extra_binds.push(offset as i64);
        adapt_sql(&sql, pool)
    };

    let mut list_q = sqlx::query(&list_sql);
    for b in &binds {
        list_q = list_q.bind(b);
    }
    for b in &extra_binds {
        list_q = list_q.bind(*b);
    }

    let rows = list_q.fetch_all(pool).await?;

    let (tenants, next_cursor) = if use_cursor {
        let has_more = rows.len() > opts.size as usize;
        let tenants: Vec<TenantRow> = rows
            .iter()
            .take(opts.size as usize)
            .map(row_to_tenant)
            .collect();
        let nc = if has_more {
            tenants.last().map(|t| (t.created_at.clone(), t.id.clone()))
        } else {
            None
        };
        (tenants, nc)
    } else {
        let tenants: Vec<TenantRow> = rows.iter().map(row_to_tenant).collect();
        let offset = ((opts.page.max(1)) - 1) * opts.size;
        let has_more_offset = (offset as i64 + tenants.len() as i64) < total;
        let nc = if has_more_offset {
            tenants.last().map(|t| (t.created_at.clone(), t.id.clone()))
        } else {
            None
        };
        (tenants, nc)
    };

    Ok(ListTenantsResult {
        tenants,
        total,
        next_cursor,
    })
}

pub async fn batch_update_metadata(
    pool: &AnyPool,
    ids: &[String],
    notes: Option<&str>,
    tags: Option<&str>,
) -> Result<u64, sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let mut affected = 0u64;
    for chunk in ids.chunks(100) {
        let placeholders: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("${}", i + 4))
            .collect();
        let sql = format!(
            "UPDATE tenants SET notes = $1, tags = $2, updated_at = $3 WHERE id IN ({})",
            placeholders.join(",")
        );
        let sql = adapt_sql(&sql, pool);
        let mut q = sqlx::query(&sql);
        q = q.bind(notes).bind(tags).bind(&now);
        for id in chunk {
            q = q.bind(id);
        }
        let result = q.execute(pool).await?;
        affected += result.rows_affected();
    }
    Ok(affected)
}

pub async fn get_tenant(pool: &AnyPool, tenant_id: &str) -> Result<Option<TenantRow>, sqlx::Error> {
    let sql = adapt_sql(
        &format!(
            "SELECT * FROM tenants WHERE id = $1 AND state NOT IN ('{}', '{}')",
            tenant_state::DISABLED,
            tenant_state::CREATE_FAILED
        ),
        pool,
    );
    let row = sqlx::query(&sql)
        .bind(tenant_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_tenant))
}

pub async fn get_tenant_by_id(
    pool: &AnyPool,
    tenant_id: &str,
) -> Result<Option<TenantRow>, sqlx::Error> {
    let sql = adapt_sql("SELECT * FROM tenants WHERE id = $1", pool);
    let row = sqlx::query(&sql)
        .bind(tenant_id)
        .fetch_optional(pool)
        .await?;
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
    sqlx::query(&sql)
        .bind(notes)
        .bind(tags)
        .bind(&now)
        .bind(id)
        .execute(pool)
        .await?;
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
        sqlx::query(&sql)
            .bind(&encrypted)
            .bind(&now)
            .bind(&cred.id)
            .execute(pool)
            .await?;
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

    sql.push_str(&format!(
        " ORDER BY timestamp DESC LIMIT ${idx} OFFSET ${}",
        idx + 1
    ));

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

pub async fn get_stuck_tenants(
    pool: &AnyPool,
    state: &str,
    before: &str,
) -> Result<Vec<TenantRow>, sqlx::Error> {
    let sql = adapt_sql(
        "SELECT * FROM tenants WHERE state = $1 AND created_at < $2",
        pool,
    );
    let rows = sqlx::query(&sql)
        .bind(state)
        .bind(before)
        .fetch_all(pool)
        .await?;
    Ok(rows.iter().map(row_to_tenant).collect())
}

pub async fn check_tenants_exist(
    pool: &AnyPool,
    ids: &[&str],
) -> Result<std::collections::HashSet<String>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(std::collections::HashSet::new());
    }

    let mut result = std::collections::HashSet::new();

    for chunk in ids.chunks(100) {
        let placeholders: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("${}", i + 1))
            .collect();
        let sql = format!(
            "SELECT id FROM tenants WHERE id IN ({})",
            placeholders.join(",")
        );
        let sql = adapt_sql(&sql, pool);

        let mut q = sqlx::query(&sql);
        for id in chunk {
            q = q.bind(*id);
        }
        let rows = q.fetch_all(pool).await?;
        for r in &rows {
            result.insert(r.get::<String, _>("id"));
        }
    }

    Ok(result)
}

pub async fn delete_old_audit_logs(pool: &AnyPool, before: &str) -> Result<u64, sqlx::Error> {
    let sql = adapt_sql("DELETE FROM audit_logs WHERE timestamp < $1", pool);
    let result = sqlx::query(&sql).bind(before).execute(pool).await?;
    Ok(result.rows_affected())
}
