use crate::models::UserResponse;
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
