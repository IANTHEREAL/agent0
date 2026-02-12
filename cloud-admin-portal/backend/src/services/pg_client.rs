use crate::models::{ColumnInfo, SqlResult, UserResponse};
use serde_json::Value;
use tokio_postgres::NoTls;
use tokio_postgres::{types::Type, Row};

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
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while let Some(ch) = chars.next() {
        if in_line_comment {
            current.push(ch);
            if ch == '\n' {
                in_line_comment = false;
            }
            continue;
        }

        if in_block_comment {
            current.push(ch);
            if ch == '*' && chars.peek() == Some(&'/') {
                current.push('/');
                chars.next();
                in_block_comment = false;
            }
            continue;
        }

        if !in_single && !in_double {
            if ch == '-' && chars.peek() == Some(&'-') {
                current.push(ch);
                current.push('-');
                chars.next();
                in_line_comment = true;
                continue;
            }
            if ch == '/' && chars.peek() == Some(&'*') {
                current.push(ch);
                current.push('*');
                chars.next();
                in_block_comment = true;
                continue;
            }
        }

        if ch == '\'' && !in_double {
            in_single = !in_single;
            current.push(ch);
            continue;
        }

        if ch == '"' && !in_single {
            in_double = !in_double;
            current.push(ch);
            continue;
        }

        if ch == ';' && !in_single && !in_double {
            let stmt = current.trim();
            if !stmt.is_empty() {
                statements.push(stmt.to_string());
            }
            current.clear();
            continue;
        }

        current.push(ch);
    }

    let stmt = current.trim();
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

fn row_value_to_json(row: &Row, idx: usize) -> Value {
    let ty = row.columns()[idx].type_();
    match *ty {
        Type::BOOL => row
            .try_get::<_, Option<bool>>(idx)
            .ok()
            .flatten()
            .map_or(Value::Null, Value::from),
        Type::INT2 => row
            .try_get::<_, Option<i16>>(idx)
            .ok()
            .flatten()
            .map_or(Value::Null, Value::from),
        Type::INT4 => row
            .try_get::<_, Option<i32>>(idx)
            .ok()
            .flatten()
            .map_or(Value::Null, Value::from),
        Type::INT8 => row
            .try_get::<_, Option<i64>>(idx)
            .ok()
            .flatten()
            .map_or(Value::Null, Value::from),
        Type::FLOAT4 => row
            .try_get::<_, Option<f32>>(idx)
            .ok()
            .flatten()
            .map_or(Value::Null, Value::from),
        Type::FLOAT8 => row
            .try_get::<_, Option<f64>>(idx)
            .ok()
            .flatten()
            .map_or(Value::Null, Value::from),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => row
            .try_get::<_, Option<String>>(idx)
            .ok()
            .flatten()
            .map_or(Value::Null, Value::from),
        Type::BYTEA => row
            .try_get::<_, Option<Vec<u8>>>(idx)
            .ok()
            .flatten()
            .map_or(Value::Null, |v| {
                Value::from(
                    v.iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(""),
                )
            }),
        _ => row
            .try_get::<_, Option<String>>(idx)
            .ok()
            .flatten()
            .map_or(Value::Null, Value::from),
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
        let rows = client.simple_query(sql).await.map_err(|e| e.to_string())?;
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
        };

        for statement in statements {
            let command = detect_command(&statement);
            if query_like_statement(&statement) {
                let rows = client
                    .query(&statement, &[])
                    .await
                    .map_err(|e| e.to_string())?;

                let columns = if let Some(first_row) = rows.first() {
                    first_row
                        .columns()
                        .iter()
                        .map(|c| ColumnInfo {
                            name: c.name().to_string(),
                            data_type: column_type_name(c.type_()),
                        })
                        .collect()
                } else {
                    let stmt = client
                        .prepare(&statement)
                        .await
                        .map_err(|e| e.to_string())?;
                    stmt.columns()
                        .iter()
                        .map(|c| ColumnInfo {
                            name: c.name().to_string(),
                            data_type: column_type_name(c.type_()),
                        })
                        .collect()
                };

                let out_rows = rows
                    .iter()
                    .map(|row| {
                        (0..row.len())
                            .map(|idx| row_value_to_json(row, idx))
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();

                last_result = SqlResult {
                    columns,
                    row_count: out_rows.len(),
                    rows: out_rows,
                    command,
                };
                continue;
            }

            let affected = client
                .execute(&statement, &[])
                .await
                .map_err(|e| e.to_string())?;

            last_result = SqlResult {
                columns: Vec::new(),
                rows: Vec::new(),
                row_count: affected as usize,
                command,
            };
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
