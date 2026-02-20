use crate::models::{ColumnInfo, SqlResult, UserResponse};
use serde_json::Value;
use tokio_postgres::types::Type;
use tokio_postgres::NoTls;

pub struct PgClient {
    host: String,
    port: u16,
}

/// Validate a SQL identifier (username/role name).
/// Only allows alphanumeric + underscore, must be non-empty, max 63 chars.
fn validate_identifier(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("Identifier cannot be empty".into());
    }
    if name.len() > 63 {
        return Err("Identifier too long (max 63 chars)".into());
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(format!(
            "Invalid identifier '{}': only [a-zA-Z0-9_] allowed",
            name
        ));
    }
    Ok(())
}

/// Escape a value for use inside a SQL single-quoted string literal.
/// Doubles any single-quote characters.
fn escape_sql_string(s: &str) -> String {
    s.replace('\'', "''")
}

/// Escape a value for use in a libpq connection string.
/// Wraps in single quotes, escaping backslashes and single quotes.
fn escape_connstr_value(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn split_sql_statements(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut statements = Vec::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut dollar_delim: Option<Vec<u8>> = None;
    let mut start = 0usize;
    let mut i = 0usize;

    while i < bytes.len() {
        if let Some(ref delim) = dollar_delim {
            let dlen = delim.len();
            if i + dlen <= bytes.len() && bytes[i..i + dlen] == *delim.as_slice() {
                dollar_delim = None;
                i += dlen;
            } else {
                i += 1;
            }
            continue;
        }

        if in_line_comment {
            if bytes[i] == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if in_block_comment {
            if bytes[i] == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        let b = bytes[i];

        if !in_single && !in_double {
            if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                in_line_comment = true;
                i += 2;
                continue;
            }
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                in_block_comment = true;
                i += 2;
                continue;
            }
        }

        if b == b'\'' && !in_double {
            in_single = !in_single;
            i += 1;
            continue;
        }

        if b == b'"' && !in_single {
            in_double = !in_double;
            i += 1;
            continue;
        }

        if !in_single && !in_double {
            if b == b'$' {
                let prev_ok =
                    i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
                if prev_ok {
                    if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                        dollar_delim = Some(b"$$".to_vec());
                        i += 2;
                        continue;
                    }
                    let mut j = i + 1;
                    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_')
                    {
                        j += 1;
                    }
                    if j > i + 1 && j < bytes.len() && bytes[j] == b'$' {
                        dollar_delim = Some(bytes[i..=j].to_vec());
                        i = j + 1;
                        continue;
                    }
                }
            }

            if b == b';' {
                let stmt = sql[start..i].trim();
                if !stmt.is_empty() {
                    statements.push(stmt.to_string());
                }
                start = i + 1;
                i += 1;
                continue;
            }
        }

        i += 1;
    }

    let stmt = sql[start..].trim();
    if !stmt.is_empty() {
        statements.push(stmt.to_string());
    }

    statements
}

fn detect_command(sql: &str) -> String {
    let compact = sql.split_whitespace().collect::<Vec<_>>();
    if compact.is_empty() {
        return "UNKNOWN".to_string();
    }
    let first = compact[0].to_ascii_uppercase();
    if matches!(first.as_str(), "CREATE" | "ALTER" | "DROP") && compact.len() >= 2 {
        format!("{} {}", first, compact[1].to_ascii_uppercase())
    } else {
        first
    }
}

fn column_type_name(column_type: &Type) -> String {
    match *column_type {
        Type::BOOL => "boolean".to_string(),
        Type::INT2 => "smallint".to_string(),
        Type::INT4 => "integer".to_string(),
        Type::INT8 => "bigint".to_string(),
        Type::FLOAT4 => "real".to_string(),
        Type::FLOAT8 => "double precision".to_string(),
        Type::NUMERIC => "numeric".to_string(),
        Type::TEXT => "text".to_string(),
        Type::VARCHAR => "character varying".to_string(),
        Type::BPCHAR => "character".to_string(),
        Type::BYTEA => "bytea".to_string(),
        Type::DATE => "date".to_string(),
        Type::TIMESTAMP => "timestamp".to_string(),
        Type::TIMESTAMPTZ => "timestamp with time zone".to_string(),
        Type::TIME => "time".to_string(),
        Type::TIMETZ => "time with time zone".to_string(),
        Type::UUID => "uuid".to_string(),
        Type::JSON => "json".to_string(),
        Type::JSONB => "jsonb".to_string(),
        Type::UNKNOWN => "unknown".to_string(),
        _ => column_type.name().to_string(),
    }
}

fn text_to_json(val: Option<&str>, ty: &Type) -> Value {
    let s = match val {
        Some(v) => v,
        None => return Value::Null,
    };
    match *ty {
        Type::BOOL => Value::from(s.eq_ignore_ascii_case("t") || s.eq_ignore_ascii_case("true")),
        Type::INT2 | Type::INT4 => s
            .parse::<i64>()
            .map_or(Value::from(s.to_string()), Value::from),
        Type::INT8 => s
            .parse::<i64>()
            .map_or(Value::from(s.to_string()), Value::from),
        Type::FLOAT4 | Type::FLOAT8 | Type::NUMERIC => s
            .parse::<f64>()
            .map_or(Value::from(s.to_string()), Value::from),
        _ => Value::from(s.to_string()),
    }
}

fn format_pg_error(e: &tokio_postgres::Error) -> String {
    if let Some(db_err) = e.as_db_error() {
        let mut parts = vec![format!("{}: {}", db_err.severity(), db_err.message())];
        if let Some(detail) = db_err.detail() {
            parts.push(format!("DETAIL: {detail}"));
        }
        if let Some(hint) = db_err.hint() {
            parts.push(format!("HINT: {hint}"));
        }
        let code = db_err.code().code();
        if !code.is_empty() {
            parts.push(format!("SQLSTATE: {code}"));
        }
        parts.join("\n")
    } else {
        e.to_string()
    }
}

fn query_like_statement(sql: &str) -> bool {
    let upper = sql.trim_start().to_ascii_uppercase();
    upper.starts_with("SELECT")
        || upper.starts_with("WITH")
        || upper.starts_with("SHOW")
        || upper.starts_with("VALUES")
        || upper.contains(" RETURNING ")
}

impl PgClient {
    pub fn new(host: &str, port: u16) -> Self {
        Self {
            host: host.to_string(),
            port,
        }
    }

    async fn connect(
        &self,
        tenant_id: &str,
        user: &str,
        password: &str,
    ) -> Result<tokio_postgres::Client, String> {
        let connstr = format!(
            "host={} port={} user={} password={} dbname=postgres",
            escape_connstr_value(&self.host),
            self.port,
            escape_connstr_value(&format!("{}.{}", tenant_id, user)),
            escape_connstr_value(password),
        );
        let (client, conn) = tokio_postgres::connect(&connstr, NoTls)
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            conn.await.ok();
        });
        Ok(client)
    }

    pub async fn test_connection(&self, keyspace: &str, user: &str, password: &str) -> bool {
        match self.connect(keyspace, user, password).await {
            Ok(c) => c.simple_query("SELECT 1").await.is_ok(),
            Err(_) => false,
        }
    }

    pub async fn bootstrap_admin_password(
        &self,
        keyspace: &str,
        user: &str,
        default_password: &str,
        desired_password: &str,
    ) -> bool {
        // If desired password already works, nothing to do
        if self.test_connection(keyspace, user, desired_password).await {
            return true;
        }

        // Wait for keyspace to become available, then connect with default password
        for attempt in 0..5 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            if self.test_connection(keyspace, user, default_password).await {
                // Change password to desired one
                return self
                    .reset_password(keyspace, user, default_password, user, desired_password)
                    .await;
            }
        }
        false
    }

    pub async fn bootstrap_default_extensions(&self, keyspace: &str, user: &str, password: &str) {
        const DEFAULT_EXTENSIONS: &[&str] = &["http", "fs9", "pg_cron"];
        let client = match self.connect(keyspace, user, password).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("bootstrap_default_extensions: connect failed: {e}");
                return;
            }
        };
        for ext in DEFAULT_EXTENSIONS {
            let sql = format!("CREATE EXTENSION IF NOT EXISTS \"{ext}\"");
            if let Err(e) = client.simple_query(&sql).await {
                tracing::warn!("bootstrap_default_extensions: {ext}: {e}");
            }
        }
    }

    pub async fn list_users(
        &self,
        tenant_id: &str,
        admin_user: &str,
        admin_password: &str,
    ) -> Vec<UserResponse> {
        let client = match self.connect(tenant_id, admin_user, admin_password).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("list_users connect failed: {e}");
                return Vec::new();
            }
        };
        match client
            .query(
                "SELECT rolname, rolsuper, rolcanlogin, rolcreatedb, rolcreaterole FROM pg_roles",
                &[],
            )
            .await
        {
            Ok(rows) => rows
                .iter()
                .map(|r| UserResponse {
                    name: r.get::<_, String>(0),
                    is_superuser: r.get::<_, bool>(1),
                    can_login: r.get::<_, bool>(2),
                    can_create_db: r.get::<_, bool>(3),
                    can_create_role: r.get::<_, bool>(4),
                })
                .collect(),
            Err(e) => {
                tracing::warn!("list_users query failed: {e}");
                Vec::new()
            }
        }
    }

    pub async fn create_user(
        &self,
        keyspace: &str,
        admin_user: &str,
        admin_password: &str,
        new_user: &str,
        new_password: &str,
        superuser: bool,
    ) -> bool {
        if let Err(e) = validate_identifier(new_user) {
            tracing::warn!("create_user rejected: {e}");
            return false;
        }
        let client = match self.connect(keyspace, admin_user, admin_password).await {
            Ok(c) => c,
            Err(_) => return false,
        };
        let su = if superuser { " SUPERUSER" } else { "" };
        let sql = format!(
            "CREATE ROLE {new_user} WITH LOGIN PASSWORD '{}'{su}",
            escape_sql_string(new_password),
        );
        client.simple_query(&sql).await.is_ok()
    }

    pub async fn drop_user(
        &self,
        keyspace: &str,
        admin_user: &str,
        admin_password: &str,
        username: &str,
    ) -> bool {
        if let Err(e) = validate_identifier(username) {
            tracing::warn!("drop_user rejected: {e}");
            return false;
        }
        let client = match self.connect(keyspace, admin_user, admin_password).await {
            Ok(c) => c,
            Err(_) => return false,
        };
        client
            .simple_query(&format!("DROP ROLE {username}"))
            .await
            .is_ok()
    }

    pub async fn reset_password(
        &self,
        keyspace: &str,
        admin_user: &str,
        admin_password: &str,
        target_user: &str,
        new_password: &str,
    ) -> bool {
        if let Err(e) = validate_identifier(target_user) {
            tracing::warn!("reset_password rejected: {e}");
            return false;
        }
        let client = match self.connect(keyspace, admin_user, admin_password).await {
            Ok(c) => c,
            Err(_) => return false,
        };
        let sql = format!(
            "ALTER ROLE {target_user} WITH PASSWORD '{}'",
            escape_sql_string(new_password),
        );
        client.simple_query(&sql).await.is_ok()
    }

    pub async fn run_sql(
        &self,
        keyspace: &str,
        user: &str,
        password: &str,
        sql: &str,
    ) -> Result<String, String> {
        let client = self.connect(keyspace, user, password).await?;
        let rows = client
            .simple_query(sql)
            .await
            .map_err(|e| format_pg_error(&e))?;
        let mut output = String::new();
        for msg in rows {
            if let tokio_postgres::SimpleQueryMessage::Row(row) = msg {
                let cols: Vec<String> = (0..row.len())
                    .map(|i| row.get(i).unwrap_or("").to_string())
                    .collect();
                output.push_str(&cols.join("|"));
                output.push('\n');
            }
        }
        Ok(output)
    }

    /// Execute SQL and return structured results.
    ///
    /// Uses prepare() for column type metadata and simple_query() for data
    /// retrieval (text format) to avoid binary encoding mismatches with pg-tikv.
    pub async fn run_sql_structured(
        &self,
        keyspace: &str,
        user: &str,
        password: &str,
        sql: &str,
    ) -> Result<SqlResult, String> {
        let client = self.connect(keyspace, user, password).await?;
        let statements = split_sql_statements(sql);
        if statements.is_empty() {
            return Err("Empty SQL query".to_string());
        }

        let mut last_result = SqlResult {
            columns: Vec::new(),
            rows: Vec::new(),
            row_count: 0,
            command: "UNKNOWN".to_string(),
            error: None,
        };

        for statement in &statements {
            let command = detect_command(statement);

            if query_like_statement(statement) {
                let col_types = match client.prepare(statement).await {
                    Ok(stmt) => stmt
                        .columns()
                        .iter()
                        .map(|c| (c.name().to_string(), c.type_().clone()))
                        .collect::<Vec<_>>(),
                    Err(e) => return Err(format_pg_error(&e)),
                };

                let messages = client
                    .simple_query(statement)
                    .await
                    .map_err(|e| format_pg_error(&e))?;

                let columns: Vec<ColumnInfo> = col_types
                    .iter()
                    .map(|(name, ty)| ColumnInfo {
                        name: name.clone(),
                        data_type: column_type_name(ty),
                    })
                    .collect();

                let mut out_rows: Vec<Vec<Value>> = Vec::new();
                for msg in messages {
                    if let tokio_postgres::SimpleQueryMessage::Row(row) = msg {
                        let vals: Vec<Value> = (0..col_types.len())
                            .map(|i| text_to_json(row.get(i), &col_types[i].1))
                            .collect();
                        out_rows.push(vals);
                    }
                }

                last_result = SqlResult {
                    row_count: out_rows.len(),
                    columns,
                    rows: out_rows,
                    command,
                    error: None,
                };
            } else {
                let messages = client
                    .simple_query(statement)
                    .await
                    .map_err(|e| format_pg_error(&e))?;

                let mut affected: u64 = 0;
                for msg in messages {
                    if let tokio_postgres::SimpleQueryMessage::CommandComplete(n) = msg {
                        affected = n;
                    }
                }

                last_result = SqlResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    row_count: affected as usize,
                    command,
                    error: None,
                };
            }
        }

        Ok(last_result)
    }

    pub async fn get_observability_summary(
        &self,
        keyspace: &str,
        user: &str,
        password: &str,
    ) -> Result<Option<serde_json::Value>, String> {
        let out = self
            .run_sql(
                keyspace,
                user,
                password,
                "SELECT * FROM _pgtikv_sys_observability()",
            )
            .await?;
        if out.is_empty() {
            return Ok(None);
        }
        let parts: Vec<&str> = out.trim().split('|').collect();
        if parts.len() < 9 {
            return Ok(None);
        }
        Ok(Some(serde_json::json!({
            "window_seconds": parts[0].parse::<i64>().unwrap_or(0),
            "statement_count": parts[1].parse::<i64>().unwrap_or(0),
            "txn_commit_count": parts[2].parse::<i64>().unwrap_or(0),
            "error_count": parts[3].parse::<i64>().unwrap_or(0),
            "qps": parts[4].parse::<f64>().unwrap_or(0.0),
            "tps": parts[5].parse::<f64>().unwrap_or(0.0),
            "latency_avg_ms": parts[6].parse::<f64>().unwrap_or(0.0),
            "latency_p99_ms": parts[7].parse::<f64>().unwrap_or(0.0),
            "active_connections": parts[8].parse::<i64>().unwrap_or(0),
        })))
    }

    pub async fn get_observability_samples(
        &self,
        keyspace: &str,
        user: &str,
        password: &str,
    ) -> Vec<serde_json::Value> {
        match self
            .run_sql(
                keyspace,
                user,
                password,
                "SELECT * FROM _pgtikv_sys_query_samples()",
            )
            .await
        {
            Ok(out) => out
                .lines()
                .filter_map(|line| {
                    let parts: Vec<&str> = line.split('|').collect();
                    if parts.len() < 7 {
                        return None;
                    }
                    Some(serde_json::json!({
                        "query": parts[0],
                        "sample_count": parts[1].parse::<i64>().unwrap_or(0),
                        "error_count": parts[2].parse::<i64>().unwrap_or(0),
                        "latency_avg_ms": parts[3].parse::<f64>().unwrap_or(0.0),
                        "latency_p99_ms": parts[4].parse::<f64>().unwrap_or(0.0),
                        "latency_max_ms": parts[5].parse::<f64>().unwrap_or(0.0),
                        "last_seen_ms_ago": parts[6].parse::<i64>().unwrap_or(0),
                    }))
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}
