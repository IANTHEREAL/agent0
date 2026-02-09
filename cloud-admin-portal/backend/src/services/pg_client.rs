use tokio_postgres::NoTls;
use crate::models::UserResponse;

pub struct PgClient {
    host: String,
    port: u16,
}

impl PgClient {
    pub fn new(host: &str, port: u16) -> Self {
        Self { host: host.to_string(), port }
    }

    async fn connect(&self, tenant_id: &str, user: &str, password: &str) -> Result<tokio_postgres::Client, String> {
        let connstr = format!(
            "host={} port={} user={}.{} password={} dbname=postgres",
            self.host, self.port, tenant_id, user, password
        );
        let (client, conn) = tokio_postgres::connect(&connstr, NoTls)
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move { conn.await.ok(); });
        Ok(client)
    }

    pub async fn test_connection(&self, keyspace: &str, user: &str, password: &str) -> bool {
        match self.connect(keyspace, user, password).await {
            Ok(c) => c.simple_query("SELECT 1").await.is_ok(),
            Err(_) => false,
        }
    }

    pub async fn bootstrap_admin_password(&self, keyspace: &str, user: &str, password: &str) -> bool {
        for attempt in 0..3 {
            if attempt > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            if self.test_connection(keyspace, user, password).await {
                return true;
            }
        }
        false
    }

    pub async fn list_users(&self, tenant_id: &str, admin_user: &str, admin_password: &str) -> Vec<UserResponse> {
        let client = match self.connect(tenant_id, admin_user, admin_password).await {
            Ok(c) => c,
            Err(e) => { tracing::warn!("list_users connect failed: {e}"); return Vec::new(); }
        };
        match client.query(
            "SELECT rolname, rolsuper, rolcanlogin, rolcreatedb, rolcreaterole FROM pg_roles",
            &[],
        ).await {
            Ok(rows) => rows.iter().map(|r| {
                UserResponse {
                    name: r.get::<_, String>(0),
                    is_superuser: r.get::<_, bool>(1),
                    can_login: r.get::<_, bool>(2),
                    can_create_db: r.get::<_, bool>(3),
                    can_create_role: r.get::<_, bool>(4),
                }
            }).collect(),
            Err(e) => { tracing::warn!("list_users query failed: {e}"); Vec::new() }
        }
    }

    pub async fn create_user(
        &self, keyspace: &str, admin_user: &str, admin_password: &str,
        new_user: &str, new_password: &str, superuser: bool,
    ) -> bool {
        let client = match self.connect(keyspace, admin_user, admin_password).await {
            Ok(c) => c,
            Err(_) => return false,
        };
        let su = if superuser { " SUPERUSER" } else { "" };
        let sql = format!("CREATE ROLE {new_user} WITH LOGIN PASSWORD '{new_password}'{su}");
        client.simple_query(&sql).await.is_ok()
    }

    pub async fn drop_user(&self, keyspace: &str, admin_user: &str, admin_password: &str, username: &str) -> bool {
        let client = match self.connect(keyspace, admin_user, admin_password).await {
            Ok(c) => c,
            Err(_) => return false,
        };
        client.simple_query(&format!("DROP ROLE {username}")).await.is_ok()
    }

    pub async fn reset_password(
        &self, keyspace: &str, admin_user: &str, admin_password: &str,
        target_user: &str, new_password: &str,
    ) -> bool {
        let client = match self.connect(keyspace, admin_user, admin_password).await {
            Ok(c) => c,
            Err(_) => return false,
        };
        client.simple_query(&format!("ALTER ROLE {target_user} WITH PASSWORD '{new_password}'")).await.is_ok()
    }

    pub async fn run_sql(&self, keyspace: &str, user: &str, password: &str, sql: &str) -> Result<String, String> {
        let client = self.connect(keyspace, user, password).await?;
        let rows = client.simple_query(sql).await.map_err(|e| e.to_string())?;
        let mut output = String::new();
        for msg in rows {
            if let tokio_postgres::SimpleQueryMessage::Row(row) = msg {
                let cols: Vec<String> = (0..row.len()).map(|i| row.get(i).unwrap_or("").to_string()).collect();
                output.push_str(&cols.join("|"));
                output.push('\n');
            }
        }
        Ok(output)
    }

    pub async fn get_observability_summary(&self, keyspace: &str, user: &str, password: &str) -> Result<Option<serde_json::Value>, String> {
        let out = self.run_sql(keyspace, user, password, "SELECT * FROM _pgtikv_sys_observability()").await?;
        if out.is_empty() { return Ok(None); }
        let parts: Vec<&str> = out.trim().split('|').collect();
        if parts.len() < 9 { return Ok(None); }
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

    pub async fn get_observability_samples(&self, keyspace: &str, user: &str, password: &str) -> Vec<serde_json::Value> {
        match self.run_sql(keyspace, user, password, "SELECT * FROM _pgtikv_sys_query_samples()").await {
            Ok(out) => {
                out.lines().filter_map(|line| {
                    let parts: Vec<&str> = line.split('|').collect();
                    if parts.len() < 7 { return None; }
                    Some(serde_json::json!({
                        "query": parts[0],
                        "sample_count": parts[1].parse::<i64>().unwrap_or(0),
                        "error_count": parts[2].parse::<i64>().unwrap_or(0),
                        "latency_avg_ms": parts[3].parse::<f64>().unwrap_or(0.0),
                        "latency_p99_ms": parts[4].parse::<f64>().unwrap_or(0.0),
                        "latency_max_ms": parts[5].parse::<f64>().unwrap_or(0.0),
                        "last_seen_ms_ago": parts[6].parse::<i64>().unwrap_or(0),
                    }))
                }).collect()
            }
            Err(_) => Vec::new(),
        }
    }
}
