use std::sync::OnceLock;

use sqlx::any::AnyPoolOptions;
use sqlx::{AnyPool, Row};

use crate::crypto;
use crate::models::{CredentialRow, CustomerRow, CustomerTokenRow, TenantRow};
use crate::tenant_state;

/// Global flag set once during `connect()` — true when the backend is SQLite.
static IS_SQLITE: OnceLock<bool> = OnceLock::new();

// ── Credential error type ────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    #[error("credential encryption key not configured")]
    KeyNotConfigured,
    #[error("credential crypto operation failed: {0}")]
    CryptoFailed(String),
    #[error("database error: {0}")]
    Sql(#[from] sqlx::Error),
}

/// Encrypt a password for storage.
///
/// Returns `(encrypted_value, key_version)`.
/// - `key_version = 2`: encrypted with AES-256-GCM via current key
/// - `key_version = 0`: plaintext (only when key is None, i.e. dev mode with
///   `DB9_ALLOW_PLAINTEXT_CREDENTIALS=1`)
fn encrypt_password(password: &str, key: Option<&str>) -> Result<(String, i32), CredentialError> {
    match key {
        Some(k) => {
            let encrypted = crypto::encrypt(password, k).map_err(CredentialError::CryptoFailed)?;
            Ok((encrypted, 2))
        }
        None => {
            // Only reachable if startup allowed plaintext (DB9_ALLOW_PLAINTEXT_CREDENTIALS=1).
            Ok((password.to_string(), 0))
        }
    }
}

/// Decrypt a stored credential, branching on `key_version`.
///
/// - `key_version = 0`: plaintext, returned as-is (with warning)
/// - `key_version = 1`: legacy unclassified row — error (must run bootstrap first)
/// - `key_version = 2`: encrypted with current key — decrypt
fn decrypt_password(
    stored: &str,
    key_version: i32,
    key: Option<&str>,
) -> Result<String, CredentialError> {
    match key_version {
        0 => {
            tracing::warn!("credential stored as plaintext (key_version=0)");
            Ok(stored.to_string())
        }
        1 => Err(CredentialError::CryptoFailed(
            "legacy key_version=1 row — run bootstrap classification first".to_string(),
        )),
        2 => {
            let k = key.ok_or(CredentialError::KeyNotConfigured)?;
            crypto::decrypt(stored, k).map_err(CredentialError::CryptoFailed)
        }
        v => Err(CredentialError::CryptoFailed(format!(
            "unknown key_version {v}"
        ))),
    }
}

pub async fn connect(url: &str) -> Result<AnyPool, sqlx::Error> {
    sqlx::any::install_default_drivers();

    let sqlite = url.starts_with("sqlite");
    IS_SQLITE.set(sqlite).ok();

    if sqlite {
        if let Some(path) = url.strip_prefix("sqlite://") {
            let path = path.split('?').next().unwrap_or(path);
            if let Some(parent) = std::path::Path::new(path).parent() {
                std::fs::create_dir_all(parent).ok();
            }
        }
    }

    let max_conns = if sqlite { 5 } else { 20 };
    let pool = AnyPoolOptions::new()
        .max_connections(max_conns)
        .connect(url)
        .await?;

    if sqlite {
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

fn is_sqlite(_pool: &AnyPool) -> bool {
    IS_SQLITE.get().copied().unwrap_or(false)
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
    let tenants_ddl = "CREATE TABLE IF NOT EXISTS tenants (
        id TEXT PRIMARY KEY,
        keyspace TEXT NOT NULL UNIQUE,
        state TEXT NOT NULL DEFAULT 'ACTIVE',
        state_reason TEXT,
        created_at TEXT NOT NULL,
        created_by TEXT,
        notes TEXT,
        tags TEXT,
        updated_at TEXT
    )";

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

    // Customer account tables
    let customers_ddl = "CREATE TABLE IF NOT EXISTS customers (
        id TEXT PRIMARY KEY,
        email TEXT UNIQUE NOT NULL,
        password_hash TEXT NOT NULL,
        created_at TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'active'
    )";

    let customer_tokens_ddl = "CREATE TABLE IF NOT EXISTS customer_tokens (
        id TEXT PRIMARY KEY,
        customer_id TEXT NOT NULL,
        token_hash TEXT NOT NULL,
        name TEXT NOT NULL DEFAULT 'default',
        expires_at TEXT,
        created_at TEXT NOT NULL,
        FOREIGN KEY (customer_id) REFERENCES customers(id)
    )";

    sqlx::query(customers_ddl).execute(pool).await?;
    sqlx::query(customer_tokens_ddl).execute(pool).await?;

    // Add customer_id to tenants (idempotent — ignore error if column already exists)
    sqlx::query("ALTER TABLE tenants ADD COLUMN customer_id TEXT")
        .execute(pool)
        .await
        .ok();

    sqlx::query("ALTER TABLE customers ADD COLUMN is_anonymous INTEGER NOT NULL DEFAULT 0")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE customers ADD COLUMN database_limit INTEGER")
        .execute(pool)
        .await
        .ok();
    sqlx::query("ALTER TABLE customers ADD COLUMN anonymous_secret_hash TEXT")
        .execute(pool)
        .await
        .ok();

    let indexes = [
        "CREATE INDEX IF NOT EXISTS idx_tenants_state ON tenants(state)",
        "CREATE INDEX IF NOT EXISTS idx_tenants_created ON tenants(created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_tenants_state_created ON tenants(state, created_at DESC)",
        "CREATE INDEX IF NOT EXISTS idx_tenants_cursor ON tenants(created_at DESC, id DESC)",
        "CREATE INDEX IF NOT EXISTS idx_creds_tenant ON tenant_credentials(tenant_id)",
        "CREATE INDEX IF NOT EXISTS idx_audit_ts ON audit_logs(timestamp)",
        "CREATE INDEX IF NOT EXISTS idx_audit_tenant ON audit_logs(tenant_id)",
        "CREATE INDEX IF NOT EXISTS idx_audit_op ON audit_logs(operation_type)",
        "CREATE INDEX IF NOT EXISTS idx_customers_email ON customers(email)",
        "CREATE INDEX IF NOT EXISTS idx_customer_tokens_customer ON customer_tokens(customer_id)",
        "CREATE INDEX IF NOT EXISTS idx_customer_tokens_hash ON customer_tokens(token_hash)",
        "CREATE INDEX IF NOT EXISTS idx_tenants_customer ON tenants(customer_id)",
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
        param_idx += 2;
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
) -> Result<Option<CredentialRow>, CredentialError> {
    let sql = adapt_sql(
        "SELECT id, tenant_id, credential_type, username, password_plain, key_version FROM tenant_credentials WHERE tenant_id = $1 AND credential_type = $2",
        pool,
    );
    let row = sqlx::query(&sql)
        .bind(tenant_id)
        .bind(cred_type)
        .fetch_optional(pool)
        .await?;
    match row {
        Some(r) => {
            let stored: String = r.get("password_plain");
            let key_ver: i32 = r.get("key_version");
            let decrypted = decrypt_password(&stored, key_ver, credential_key)?;
            Ok(Some(CredentialRow {
                id: r.get("id"),
                tenant_id: r.get("tenant_id"),
                credential_type: r.get("credential_type"),
                username: r.get("username"),
                password_plain: decrypted,
            }))
        }
        None => Ok(None),
    }
}

pub async fn upsert_credential(
    pool: &AnyPool,
    tenant_id: &str,
    cred_type: &str,
    username: &str,
    password: &str,
    credential_key: Option<&str>,
) -> Result<(), CredentialError> {
    let (encrypted, key_ver) = encrypt_password(password, credential_key)?;
    let now = chrono::Utc::now().to_rfc3339();

    // Check for existing row by looking at the raw table (no decrypt needed for existence check).
    let check_sql = adapt_sql(
        "SELECT id FROM tenant_credentials WHERE tenant_id = $1 AND credential_type = $2",
        pool,
    );
    let existing = sqlx::query(&check_sql)
        .bind(tenant_id)
        .bind(cred_type)
        .fetch_optional(pool)
        .await?;

    if let Some(row) = existing {
        let existing_id: String = row.get("id");
        let sql = adapt_sql(
            "UPDATE tenant_credentials SET password_plain = $1, key_version = $2, rotated_at = $3 WHERE id = $4",
            pool,
        );
        sqlx::query(&sql)
            .bind(&encrypted)
            .bind(key_ver)
            .bind(&now)
            .bind(&existing_id)
            .execute(pool)
            .await?;
    } else {
        let id = uuid::Uuid::new_v4().to_string();
        let sql = adapt_sql(
            "INSERT INTO tenant_credentials (id, tenant_id, credential_type, username, password_plain, key_version, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            pool,
        );
        sqlx::query(&sql)
            .bind(&id)
            .bind(tenant_id)
            .bind(cred_type)
            .bind(username)
            .bind(&encrypted)
            .bind(key_ver)
            .bind(&now)
            .execute(pool)
            .await?;
    }
    Ok(())
}

// ── Bootstrap classification + migration ────────────────────────

pub struct BootstrapReport {
    pub total_inspected: u32,
    pub encrypted: u32,
    pub reclassified_plaintext: u32,
}

/// Classify legacy rows where key_version=1 is the untrustworthy schema default.
/// Reclassifies every key_version=1 row to either 2 (decryptable) or 0 (not decryptable).
/// After this runs, no rows should have key_version=1.
pub async fn bootstrap_classify_credentials(
    pool: &AnyPool,
    credential_key: &str,
) -> Result<BootstrapReport, sqlx::Error> {
    let sql = adapt_sql(
        "SELECT id, password_plain FROM tenant_credentials WHERE key_version = 1",
        pool,
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;

    let mut encrypted = 0u32;
    let mut reclassified_plaintext = 0u32;

    for row in &rows {
        let id: String = row.get("id");
        let stored: String = row.get("password_plain");

        match crypto::decrypt(&stored, credential_key) {
            Ok(_) => {
                let update_sql = adapt_sql(
                    "UPDATE tenant_credentials SET key_version = 2 WHERE id = $1",
                    pool,
                );
                sqlx::query(&update_sql).bind(&id).execute(pool).await?;
                encrypted += 1;
            }
            Err(_) => {
                let update_sql = adapt_sql(
                    "UPDATE tenant_credentials SET key_version = 0 WHERE id = $1",
                    pool,
                );
                sqlx::query(&update_sql).bind(&id).execute(pool).await?;
                reclassified_plaintext += 1;
                tracing::warn!(
                    credential_id = %id,
                    "credential reclassified to plaintext (key_version=0) — \
                     could not decrypt with current key"
                );
            }
        }
    }

    Ok(BootstrapReport {
        total_inspected: rows.len() as u32,
        encrypted,
        reclassified_plaintext,
    })
}

pub struct MigrationStatus {
    pub total_credentials: i64,
    pub encrypted: i64,
    pub plaintext: i64,
    pub unclassified: i64,
    pub migration_complete: bool,
}

pub async fn credential_migration_status(pool: &AnyPool) -> Result<MigrationStatus, sqlx::Error> {
    let sql = adapt_sql(
        "SELECT key_version, COUNT(*) as cnt FROM tenant_credentials GROUP BY key_version",
        pool,
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;

    let mut encrypted: i64 = 0;
    let mut plaintext: i64 = 0;
    let mut unclassified: i64 = 0;

    for row in &rows {
        let kv: i32 = row.get("key_version");
        let cnt: i64 = row.get("cnt");
        match kv {
            0 => plaintext = cnt,
            1 => unclassified = cnt,
            2 => encrypted = cnt,
            _ => {}
        }
    }

    let total = encrypted + plaintext + unclassified;
    Ok(MigrationStatus {
        total_credentials: total,
        encrypted,
        plaintext,
        unclassified,
        migration_complete: plaintext == 0 && unclassified == 0,
    })
}

/// Re-encrypt all key_version=0 (plaintext) credentials with the provided key.
/// Returns the number of rows migrated.
pub async fn migrate_credentials(
    pool: &AnyPool,
    credential_key: &str,
) -> Result<u32, CredentialError> {
    let sql = adapt_sql(
        "SELECT id, password_plain FROM tenant_credentials WHERE key_version = 0",
        pool,
    );
    let rows = sqlx::query(&sql).fetch_all(pool).await?;

    let mut migrated = 0u32;
    for row in &rows {
        let id: String = row.get("id");
        let plaintext: String = row.get("password_plain");

        let encrypted =
            crypto::encrypt(&plaintext, credential_key).map_err(CredentialError::CryptoFailed)?;

        let update_sql = adapt_sql(
            "UPDATE tenant_credentials SET password_plain = $1, key_version = 2 WHERE id = $2",
            pool,
        );
        sqlx::query(&update_sql)
            .bind(&encrypted)
            .bind(&id)
            .execute(pool)
            .await?;
        migrated += 1;
    }

    Ok(migrated)
}

// ── Audit queries ───────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
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
        sql.push_str(&format!(" AND success = CAST(${idx} AS INTEGER)"));
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

// ── Customer queries ────────────────────────────────────────────

pub async fn create_customer(
    pool: &AnyPool,
    id: &str,
    email: &str,
    password_hash: &str,
) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let sql = adapt_sql(
        "INSERT INTO customers (id, email, password_hash, created_at) VALUES ($1, $2, $3, $4)",
        pool,
    );
    sqlx::query(&sql)
        .bind(id)
        .bind(email)
        .bind(password_hash)
        .bind(&now)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn create_anonymous_customer(
    pool: &AnyPool,
    id: &str,
    email: &str,
    password_hash: &str,
    database_limit: i32,
) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let sql = adapt_sql(
        "INSERT INTO customers (id, email, password_hash, created_at, is_anonymous, database_limit) VALUES ($1, $2, $3, $4, $5, $6)",
        pool,
    );
    sqlx::query(&sql)
        .bind(id)
        .bind(email)
        .bind(password_hash)
        .bind(&now)
        .bind(1)
        .bind(database_limit)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_customer_by_email(
    pool: &AnyPool,
    email: &str,
) -> Result<Option<CustomerRow>, sqlx::Error> {
    let sql = adapt_sql("SELECT * FROM customers WHERE email = $1", pool);
    let row = sqlx::query(&sql).bind(email).fetch_optional(pool).await?;
    Ok(row.as_ref().map(row_to_customer))
}

pub async fn get_customer_by_id(
    pool: &AnyPool,
    id: &str,
) -> Result<Option<CustomerRow>, sqlx::Error> {
    let sql = adapt_sql("SELECT * FROM customers WHERE id = $1", pool);
    let row = sqlx::query(&sql).bind(id).fetch_optional(pool).await?;
    Ok(row.as_ref().map(row_to_customer))
}

pub async fn create_customer_token(
    pool: &AnyPool,
    id: &str,
    customer_id: &str,
    token_hash: &str,
    name: &str,
    expires_at: &str,
) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().to_rfc3339();
    let sql = adapt_sql(
        "INSERT INTO customer_tokens (id, customer_id, token_hash, name, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
        pool,
    );
    sqlx::query(&sql)
        .bind(id)
        .bind(customer_id)
        .bind(token_hash)
        .bind(name)
        .bind(expires_at)
        .bind(&now)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_customer_token(
    pool: &AnyPool,
    token_hash: &str,
) -> Result<Option<CustomerTokenRow>, sqlx::Error> {
    let sql = adapt_sql("SELECT * FROM customer_tokens WHERE token_hash = $1", pool);
    let row = sqlx::query(&sql)
        .bind(token_hash)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_customer_token))
}

pub async fn list_customer_tokens(
    pool: &AnyPool,
    customer_id: &str,
) -> Result<Vec<CustomerTokenRow>, sqlx::Error> {
    let sql = adapt_sql(
        "SELECT * FROM customer_tokens WHERE customer_id = $1 ORDER BY created_at DESC",
        pool,
    );
    let rows = sqlx::query(&sql).bind(customer_id).fetch_all(pool).await?;
    Ok(rows.iter().map(row_to_customer_token).collect())
}

pub async fn delete_customer_token(
    pool: &AnyPool,
    token_id: &str,
    customer_id: &str,
) -> Result<bool, sqlx::Error> {
    let sql = adapt_sql(
        "DELETE FROM customer_tokens WHERE id = $1 AND customer_id = $2",
        pool,
    );
    let result = sqlx::query(&sql)
        .bind(token_id)
        .bind(customer_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn list_customer_tenants(
    pool: &AnyPool,
    customer_id: &str,
) -> Result<Vec<TenantRow>, sqlx::Error> {
    let sql = adapt_sql(
        "SELECT * FROM tenants WHERE customer_id = $1 ORDER BY created_at DESC",
        pool,
    );
    let rows = sqlx::query(&sql).bind(customer_id).fetch_all(pool).await?;
    Ok(rows.iter().map(row_to_tenant).collect())
}

pub async fn count_customer_tenants(pool: &AnyPool, customer_id: &str) -> Result<i64, sqlx::Error> {
    let sql = adapt_sql(
        &format!(
            "SELECT COUNT(*) as cnt FROM tenants WHERE customer_id = $1 AND state NOT IN ('{}', '{}')",
            tenant_state::DISABLED,
            tenant_state::CREATE_FAILED
        ),
        pool,
    );
    let row = sqlx::query(&sql).bind(customer_id).fetch_one(pool).await?;
    Ok(row.get("cnt"))
}

pub async fn claim_anonymous_customer(
    pool: &AnyPool,
    customer_id: &str,
    email: &str,
    password_hash: &str,
) -> Result<bool, sqlx::Error> {
    let sql = adapt_sql(
        "UPDATE customers SET email = $1, password_hash = $2, is_anonymous = 0, database_limit = NULL, anonymous_secret_hash = NULL WHERE id = $3 AND is_anonymous = 1",
        pool,
    );
    let result = sqlx::query(&sql)
        .bind(email)
        .bind(password_hash)
        .bind(customer_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn store_anonymous_secret(
    pool: &AnyPool,
    customer_id: &str,
    secret_hash: &str,
) -> Result<(), sqlx::Error> {
    let sql = adapt_sql(
        "UPDATE customers SET anonymous_secret_hash = $1 WHERE id = $2 AND is_anonymous = 1",
        pool,
    );
    sqlx::query(&sql)
        .bind(secret_hash)
        .bind(customer_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn get_anonymous_customer_by_id_and_secret(
    pool: &AnyPool,
    customer_id: &str,
    secret_hash: &str,
) -> Result<Option<CustomerRow>, sqlx::Error> {
    let sql = adapt_sql(
        "SELECT * FROM customers WHERE id = $1 AND anonymous_secret_hash = $2 AND is_anonymous = 1",
        pool,
    );
    let row = sqlx::query(&sql)
        .bind(customer_id)
        .bind(secret_hash)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_customer))
}

pub async fn get_tenant_for_customer(
    pool: &AnyPool,
    tenant_id: &str,
    customer_id: &str,
) -> Result<Option<TenantRow>, sqlx::Error> {
    let sql = adapt_sql(
        &format!(
            "SELECT * FROM tenants WHERE id = $1 AND customer_id = $2 AND state NOT IN ('{}', '{}')",
            tenant_state::DISABLED,
            tenant_state::CREATE_FAILED
        ),
        pool,
    );
    let row = sqlx::query(&sql)
        .bind(tenant_id)
        .bind(customer_id)
        .fetch_optional(pool)
        .await?;
    Ok(row.as_ref().map(row_to_tenant))
}

pub async fn set_tenant_customer_id(
    pool: &AnyPool,
    tenant_id: &str,
    customer_id: &str,
) -> Result<(), sqlx::Error> {
    let sql = adapt_sql("UPDATE tenants SET customer_id = $1 WHERE id = $2", pool);
    sqlx::query(&sql)
        .bind(customer_id)
        .bind(tenant_id)
        .execute(pool)
        .await?;
    Ok(())
}

fn row_to_customer(row: &sqlx::any::AnyRow) -> CustomerRow {
    let is_anonymous: i32 = row.get("is_anonymous");
    CustomerRow {
        id: row.get("id"),
        email: row.get("email"),
        password_hash: row.get("password_hash"),
        created_at: row.get("created_at"),
        status: row.get("status"),
        is_anonymous: is_anonymous != 0,
        database_limit: row.get("database_limit"),
    }
}

fn row_to_customer_token(row: &sqlx::any::AnyRow) -> CustomerTokenRow {
    CustomerTokenRow {
        id: row.get("id"),
        customer_id: row.get("customer_id"),
        token_hash: row.get("token_hash"),
        name: row.get("name"),
        expires_at: row.get("expires_at"),
        created_at: row.get("created_at"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_customer_insert_sql_has_correct_placeholders() {
        let sql =
            "INSERT INTO customers (id, email, password_hash, created_at) VALUES ($1, $2, $3, $4)";
        assert!(sql.contains("$1"));
        assert!(sql.contains("$4"));
        // Ensure exactly 4 placeholders
        assert_eq!(sql.matches('$').count(), 4);
    }

    #[test]
    fn test_customer_token_insert_sql_has_correct_placeholders() {
        let sql = "INSERT INTO customer_tokens (id, customer_id, token_hash, name, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)";
        assert!(sql.contains("$6"));
        assert_eq!(sql.matches('$').count(), 6);
    }

    #[test]
    fn test_adapt_sql_placeholder_replacement_logic() {
        // Replicate the adapt_sql logic for SQLite conversion
        let sql = "INSERT INTO customers (id, email) VALUES ($1, $2)";
        let mut result = sql.to_string();
        for i in (1..=30).rev() {
            result = result.replace(&format!("${i}"), "?");
        }
        assert_eq!(result, "INSERT INTO customers (id, email) VALUES (?, ?)");
        assert!(!result.contains('$'));
    }

    #[test]
    fn test_adapt_sql_no_replacement_for_postgres() {
        // For PostgreSQL, adapt_sql should return the SQL unchanged
        let sql = "SELECT * FROM customers WHERE email = $1";
        // If not SQLite, the original SQL is returned as-is
        assert_eq!(sql.to_string(), "SELECT * FROM customers WHERE email = $1");
    }

    #[test]
    fn test_adapt_sql_high_numbered_placeholders() {
        let sql = "INSERT INTO audit_logs VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)";
        let mut result = sql.to_string();
        for i in (1..=30).rev() {
            result = result.replace(&format!("${i}"), "?");
        }
        assert_eq!(
            result,
            "INSERT INTO audit_logs VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        );
    }

    #[test]
    fn test_adapt_sql_reverse_replacement_avoids_double_replace() {
        // $10 must not become ?0 — reversed iteration handles this
        let sql = "SELECT $1, $10";
        let mut result = sql.to_string();
        for i in (1..=30).rev() {
            result = result.replace(&format!("${i}"), "?");
        }
        assert_eq!(result, "SELECT ?, ?");
    }

    #[test]
    fn test_customer_ddl_has_required_columns() {
        let ddl = "CREATE TABLE IF NOT EXISTS customers (
            id TEXT PRIMARY KEY,
            email TEXT UNIQUE NOT NULL,
            password_hash TEXT NOT NULL,
            created_at TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'active'
        )";
        assert!(ddl.contains("id TEXT PRIMARY KEY"));
        assert!(ddl.contains("email TEXT UNIQUE NOT NULL"));
        assert!(ddl.contains("password_hash TEXT NOT NULL"));
        assert!(ddl.contains("status TEXT NOT NULL DEFAULT 'active'"));
    }

    #[test]
    fn test_customer_tokens_ddl_has_foreign_key() {
        let ddl = "CREATE TABLE IF NOT EXISTS customer_tokens (
            id TEXT PRIMARY KEY,
            customer_id TEXT NOT NULL,
            token_hash TEXT NOT NULL,
            name TEXT NOT NULL DEFAULT 'default',
            expires_at TEXT,
            created_at TEXT NOT NULL,
            FOREIGN KEY (customer_id) REFERENCES customers(id)
        )";
        assert!(ddl.contains("FOREIGN KEY (customer_id) REFERENCES customers(id)"));
        assert!(ddl.contains("token_hash TEXT NOT NULL"));
    }

    #[test]
    fn test_anonymous_customer_insert_sql_has_correct_placeholders() {
        let sql = "INSERT INTO customers (id, email, password_hash, created_at, is_anonymous, database_limit) VALUES ($1, $2, $3, $4, $5, $6)";
        assert!(sql.contains("$6"));
        assert_eq!(sql.matches('$').count(), 6);
    }

    #[test]
    fn test_count_customer_tenants_sql_excludes_disabled() {
        let sql = format!(
            "SELECT COUNT(*) as cnt FROM tenants WHERE customer_id = $1 AND state NOT IN ('{}', '{}')",
            "DISABLED", "CREATE_FAILED"
        );
        assert!(sql.contains("DISABLED"));
        assert!(sql.contains("CREATE_FAILED"));
        assert!(sql.contains("customer_id = $1"));
        assert_eq!(sql.matches('$').count(), 1);
    }

    #[test]
    fn test_claim_anonymous_customer_sql_has_correct_placeholders() {
        let sql = "UPDATE customers SET email = $1, password_hash = $2, is_anonymous = 0, database_limit = NULL, anonymous_secret_hash = NULL WHERE id = $3 AND is_anonymous = 1";
        assert!(sql.contains("is_anonymous = 0"));
        assert!(sql.contains("database_limit = NULL"));
        assert!(sql.contains("anonymous_secret_hash = NULL"));
        assert!(sql.contains("is_anonymous = 1"));
        assert_eq!(sql.matches('$').count(), 3);
    }

    #[test]
    fn test_anonymous_schema_migration_sql() {
        let alter1 = "ALTER TABLE customers ADD COLUMN is_anonymous INTEGER NOT NULL DEFAULT 0";
        let alter2 = "ALTER TABLE customers ADD COLUMN database_limit INTEGER";
        assert!(alter1.contains("is_anonymous"));
        assert!(alter1.contains("DEFAULT 0"));
        assert!(alter2.contains("database_limit"));
        assert!(!alter2.contains("NOT NULL"));
    }

    // ── Credential encryption unit tests ─────────────────────────

    use super::*;
    use base64::{engine::general_purpose::STANDARD as B64, Engine};

    fn test_key() -> String {
        B64.encode([0xABu8; 32])
    }

    fn test_key_alt() -> String {
        B64.encode([0xCDu8; 32])
    }

    #[test]
    fn encrypt_with_valid_key_returns_key_version_2() {
        let key = test_key();
        let (encrypted, kv) = encrypt_password("hunter2", Some(&key)).unwrap();
        assert_eq!(kv, 2);
        assert_ne!(encrypted, "hunter2");
    }

    #[test]
    fn encrypt_without_key_returns_plaintext_key_version_0() {
        let (value, kv) = encrypt_password("hunter2", None).unwrap();
        assert_eq!(kv, 0);
        assert_eq!(value, "hunter2");
    }

    #[test]
    fn encrypt_with_bad_key_returns_error() {
        let bad_key = B64.encode([0xABu8; 16]); // 16 bytes, not 32
        let result = encrypt_password("test", Some(&bad_key));
        assert!(result.is_err());
        assert!(matches!(result, Err(CredentialError::CryptoFailed(_))));
    }

    #[test]
    fn decrypt_key_version_2_roundtrip() {
        let key = test_key();
        let (encrypted, _) = encrypt_password("secret123", Some(&key)).unwrap();
        let decrypted = decrypt_password(&encrypted, 2, Some(&key)).unwrap();
        assert_eq!(decrypted, "secret123");
    }

    #[test]
    fn decrypt_key_version_2_wrong_key_fails() {
        let key1 = test_key();
        let key2 = test_key_alt();
        let (encrypted, _) = encrypt_password("secret", Some(&key1)).unwrap();
        let result = decrypt_password(&encrypted, 2, Some(&key2));
        assert!(result.is_err());
        assert!(matches!(result, Err(CredentialError::CryptoFailed(_))));
    }

    #[test]
    fn decrypt_key_version_2_no_key_fails() {
        let key = test_key();
        let (encrypted, _) = encrypt_password("secret", Some(&key)).unwrap();
        let result = decrypt_password(&encrypted, 2, None);
        assert!(result.is_err());
        assert!(matches!(result, Err(CredentialError::KeyNotConfigured)));
    }

    #[test]
    fn decrypt_key_version_0_returns_plaintext() {
        let result = decrypt_password("plaintext_pass", 0, Some(&test_key())).unwrap();
        assert_eq!(result, "plaintext_pass");
    }

    #[test]
    fn decrypt_key_version_0_no_key_returns_plaintext() {
        let result = decrypt_password("plaintext_pass", 0, None).unwrap();
        assert_eq!(result, "plaintext_pass");
    }

    #[test]
    fn decrypt_key_version_1_fails_with_bootstrap_message() {
        let result = decrypt_password("anything", 1, Some(&test_key()));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("bootstrap"));
    }

    #[test]
    fn decrypt_unknown_key_version_fails() {
        let result = decrypt_password("anything", 99, Some(&test_key()));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("unknown key_version 99"));
    }
}
