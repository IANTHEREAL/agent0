use std::collections::HashMap;
use std::process;

use clap::{Parser, Subcommand};
use pgtikv_admin::{DEFAULT_ADMIN_USER, TENANT_ID_LEN};
use serde_json::Value;

const DEFAULT_API_URL: &str = "http://localhost:8090/api";

#[derive(Parser)]
#[command(name = "pgtikv-ctl", about = "pg-tikv Admin Portal CLI", version)]
struct Cli {
    /// API base URL (env: PGTIKV_API_URL)
    #[arg(long, env = "PGTIKV_API_URL", default_value = DEFAULT_API_URL)]
    api_url: String,

    /// API key (env: PGTIKV_API_KEY)
    #[arg(long, env = "PGTIKV_API_KEY")]
    api_key: Option<String>,

    /// Output as JSON
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Tenant management
    Tenants {
        #[command(subcommand)]
        action: TenantAction,
    },
    /// Get tenant session for user management
    Connect {
        tenant_id: String,
        #[arg(long)]
        admin_user: String,
        #[arg(long)]
        admin_password: String,
    },
    /// User management (requires --session)
    Users {
        #[command(subcommand)]
        action: UserAction,
    },
    /// Health check
    Health,
    /// API info
    Info,
}

#[derive(Subcommand)]
enum TenantAction {
    /// List tenants
    List {
        #[arg(long, default_value_t = 1)]
        page: u32,
        #[arg(long, default_value_t = 50)]
        size: u32,
        #[arg(long)]
        state: Option<String>,
        #[arg(short, long)]
        query: Option<String>,
    },
    /// Get tenant details
    Get { tenant_id: String },
    /// Create tenant
    Create {
        #[arg(long, default_value = DEFAULT_ADMIN_USER)]
        admin_user: String,
        #[arg(long)]
        admin_password: Option<String>,
    },
    /// Remove tenant (ACTIVE → DISABLED)
    Remove { tenant_id: String },
    /// Delete tenant (alias for remove)
    Delete { tenant_id: String },
    /// Update tenant metadata
    Update {
        tenant_id: String,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        tags: Option<String>,
    },
}

#[derive(Subcommand)]
enum UserAction {
    /// List users
    List {
        tenant_id: String,
        #[arg(long)]
        session: String,
    },
    /// Create user
    Create {
        tenant_id: String,
        #[arg(long)]
        username: String,
        #[arg(long)]
        password: Option<String>,
        #[arg(long)]
        superuser: bool,
        #[arg(long)]
        session: String,
    },
    /// Delete user
    Delete {
        tenant_id: String,
        username: String,
        #[arg(long)]
        session: String,
    },
    /// Reset user password
    ResetPassword {
        tenant_id: String,
        username: String,
        #[arg(long)]
        session: String,
    },
}

// ── HTTP helper ──────────────────────────────────────────────────

struct ApiClient {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl ApiClient {
    fn new(base_url: &str, api_key: Option<&str>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.map(|s| s.to_string()),
            client: reqwest::Client::new(),
        }
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Value {
        let url = format!("{}{path}", self.base_url);
        let mut req = match method {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "DELETE" => self.client.delete(&url),
            _ => self.client.get(&url),
        };

        req = req.header("Content-Type", "application/json");

        if let Some(key) = &self.api_key {
            req = req.header("X-API-Key", key);
        }
        if let Some(hdrs) = extra_headers {
            for (k, v) in hdrs {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        if let Some(b) = body {
            req = req.json(b);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    serde_json::from_str(&text).unwrap_or(Value::Null)
                } else {
                    let err: Value = serde_json::from_str(&text).unwrap_or_default();
                    let detail = err
                        .get("message")
                        .or_else(|| err.get("detail"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown error");
                    eprintln!("Error {}: {detail}", status.as_u16());
                    process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("Connection failed: {e}");
                process::exit(1);
            }
        }
    }
}

// ── Table printer ────────────────────────────────────────────────

fn print_table(rows: &[Value], columns: &[(&str, &str, usize)]) {
    if rows.is_empty() {
        println!("(empty)");
        return;
    }

    let widths: Vec<usize> = columns
        .iter()
        .map(|(header, key, min_w)| {
            let max_val = rows
                .iter()
                .map(|r| format_val(r.get(key)).len())
                .max()
                .unwrap_or(0);
            header.len().max(max_val).max(*min_w)
        })
        .collect();

    // Header
    let header: String = columns
        .iter()
        .zip(&widths)
        .map(|((h, _, _), w)| format!("{:<width$}", h, width = w))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{header}");

    // Separator
    let sep: String = widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("  ");
    println!("{sep}");

    // Rows
    for row in rows {
        let line: String = columns
            .iter()
            .zip(&widths)
            .map(|((_, key, _), w)| {
                let v = format_val(row.get(key));
                format!("{:<width$}", v, width = w)
            })
            .collect::<Vec<_>>()
            .join("  ");
        println!("{line}");
    }
}

fn format_val(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "-".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Array(arr)) => {
            if arr.is_empty() {
                "-".to_string()
            } else {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
        Some(other) => other.to_string(),
    }
}

fn format_time(v: Option<&Value>) -> String {
    match v.and_then(|v| v.as_str()) {
        None => "-".to_string(),
        Some(s) => {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                dt.format("%Y-%m-%d %H:%M").to_string()
            } else {
                s.to_string()
            }
        }
    }
}

fn print_json(data: &Value) {
    println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
}

// ── Commands ─────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let api = ApiClient::new(&cli.api_url, cli.api_key.as_deref());

    match cli.command {
        Commands::Tenants { action } => match action {
            TenantAction::List {
                page,
                size,
                state,
                query,
            } => {
                let mut path = format!("/tenants?page={page}&size={size}");
                if let Some(st) = &state {
                    path.push_str(&format!("&state={st}"));
                }
                if let Some(q) = &query {
                    path.push_str(&format!("&q={q}"));
                }
                let data = api.request("GET", &path, None, None).await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                let items = data["items"].as_array().cloned().unwrap_or_default();
                let total = data["total"].as_i64().unwrap_or(0);

                let mut display_items = items.clone();
                for item in &mut display_items {
                    if let Some(obj) = item.as_object_mut() {
                        let formatted = format_time(obj.get("created_at").map(|v| v));
                        obj.insert("created_at".into(), Value::String(formatted));
                    }
                }

                print_table(
                    &display_items,
                    &[
                        ("ID", "id", TENANT_ID_LEN),
                        ("STATE", "state", 8),
                        ("CREATED", "created_at", 16),
                        ("TAGS", "tags", 10),
                    ],
                );

                let pages = ((total as f64) / (size as f64)).ceil().max(1.0) as i64;
                println!("\n{total} tenant(s), page {page}/{pages}");
            }

            TenantAction::Get { tenant_id } => {
                let data = api
                    .request("GET", &format!("/tenants/{tenant_id}"), None, None)
                    .await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                println!("Tenant:       {}", data["id"].as_str().unwrap_or("-"));
                println!("State:        {}", data["state"].as_str().unwrap_or("-"));
                println!("Created:      {}", format_time(data.get("created_at")));
                if let Some(reason) = data["state_reason"].as_str() {
                    println!("State Reason: {reason}");
                }
                if let Some(notes) = data["notes"].as_str() {
                    println!("Notes:        {notes}");
                }
                if let Some(tags) = data["tags"].as_array() {
                    let ts: Vec<&str> = tags.iter().filter_map(|v| v.as_str()).collect();
                    if !ts.is_empty() {
                        println!("Tags:         {}", ts.join(", "));
                    }
                }
                if let Some(eps) = data["endpoints"].as_array() {
                    if !eps.is_empty() {
                        println!("\nEndpoints:");
                        for ep in eps {
                            println!(
                                "  {}:{}  ({}, priority={})",
                                ep["host"].as_str().unwrap_or("?"),
                                ep["port"].as_u64().unwrap_or(0),
                                ep["type"].as_str().unwrap_or("-"),
                                ep["priority"].as_i64().unwrap_or(0),
                            );
                        }
                    }
                }
            }

            TenantAction::Create {
                admin_user,
                admin_password,
            } => {
                let mut body = serde_json::json!({ "admin_user": admin_user });
                if let Some(pw) = &admin_password {
                    body["admin_password"] = Value::String(pw.clone());
                }
                let data = api.request("POST", "/tenants", Some(&body), None).await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                println!("Tenant created: {}", data["id"].as_str().unwrap_or("-"));
                println!("Admin user:     {}", data["admin_user"].as_str().unwrap_or("-"));
                println!("Admin password: {}", data["admin_password"].as_str().unwrap_or("-"));
                println!(
                    "Connection:     {}",
                    data["connection_string"].as_str().unwrap_or("-")
                );
            }

            TenantAction::Remove { tenant_id } => {
                let data = api
                    .request("POST", &format!("/tenants/{tenant_id}/remove"), None, None)
                    .await;
                println!("{}", data["message"].as_str().unwrap_or("Done"));
            }

            TenantAction::Delete { tenant_id } => {
                let data = api
                    .request("DELETE", &format!("/tenants/{tenant_id}"), None, None)
                    .await;
                println!("{}", data["message"].as_str().unwrap_or("Done"));
            }

            TenantAction::Update {
                tenant_id,
                notes,
                tags,
            } => {
                let mut body = serde_json::json!({});
                if let Some(n) = &notes {
                    if n.is_empty() {
                        body["notes"] = Value::Null;
                    } else {
                        body["notes"] = Value::String(n.clone());
                    }
                }
                if let Some(t) = &tags {
                    if t.is_empty() {
                        body["tags"] = Value::Null;
                    } else {
                        let tag_list: Vec<Value> = t
                            .split(',')
                            .map(|s| Value::String(s.trim().to_string()))
                            .filter(|v| v.as_str().map(|s| !s.is_empty()).unwrap_or(false))
                            .collect();
                        body["tags"] = Value::Array(tag_list);
                    }
                }

                let data = api
                    .request("PUT", &format!("/tenants/{tenant_id}"), Some(&body), None)
                    .await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                println!(
                    "Updated {} (state={})",
                    data["id"].as_str().unwrap_or("-"),
                    data["state"].as_str().unwrap_or("-")
                );
            }
        },

        Commands::Connect {
            tenant_id,
            admin_user,
            admin_password,
        } => {
            let body = serde_json::json!({
                "admin_user": admin_user,
                "admin_password": admin_password,
            });
            let data = api
                .request("POST", &format!("/tenants/{tenant_id}/connect"), Some(&body), None)
                .await;

            if cli.json {
                print_json(&data);
                return;
            }

            println!("Session:  {}", data["session_id"].as_str().unwrap_or("-"));
            println!("Expires:  {}", format_time(data.get("expires_at")));
            println!(
                "\nUse with: pgtikv-ctl users list {tenant_id} --session {}",
                data["session_id"].as_str().unwrap_or("<session_id>")
            );
        }

        Commands::Users { action } => match action {
            UserAction::List {
                tenant_id,
                session,
            } => {
                let mut hdrs = HashMap::new();
                hdrs.insert("X-Tenant-Session".into(), session);

                let data = api
                    .request(
                        "GET",
                        &format!("/tenants/{tenant_id}/users"),
                        None,
                        Some(&hdrs),
                    )
                    .await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                let items = data.as_array().cloned().unwrap_or_default();
                print_table(
                    &items,
                    &[
                        ("NAME", "name", 15),
                        ("SUPERUSER", "is_superuser", 9),
                        ("LOGIN", "can_login", 5),
                    ],
                );
            }

            UserAction::Create {
                tenant_id,
                username,
                password,
                superuser,
                session,
            } => {
                let mut hdrs = HashMap::new();
                hdrs.insert("X-Tenant-Session".into(), session);

                let mut body = serde_json::json!({ "username": username });
                if let Some(pw) = &password {
                    body["password"] = Value::String(pw.clone());
                }
                if superuser {
                    body["superuser"] = Value::Bool(true);
                }

                let data = api
                    .request(
                        "POST",
                        &format!("/tenants/{tenant_id}/users"),
                        Some(&body),
                        Some(&hdrs),
                    )
                    .await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                println!("User created: {}", data["username"].as_str().unwrap_or("-"));
                println!("Password:     {}", data["password"].as_str().unwrap_or("-"));
                println!("Connect:      {}", data["connection"].as_str().unwrap_or("-"));
            }

            UserAction::Delete {
                tenant_id,
                username,
                session,
            } => {
                let mut hdrs = HashMap::new();
                hdrs.insert("X-Tenant-Session".into(), session);

                let data = api
                    .request(
                        "DELETE",
                        &format!("/tenants/{tenant_id}/users/{username}"),
                        None,
                        Some(&hdrs),
                    )
                    .await;
                println!("{}", data["message"].as_str().unwrap_or("Done"));
            }

            UserAction::ResetPassword {
                tenant_id,
                username,
                session,
            } => {
                let mut hdrs = HashMap::new();
                hdrs.insert("X-Tenant-Session".into(), session);

                let data = api
                    .request(
                        "POST",
                        &format!("/tenants/{tenant_id}/users/{username}/password"),
                        None,
                        Some(&hdrs),
                    )
                    .await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                println!(
                    "Password reset for {}: {}",
                    data["username"].as_str().unwrap_or("-"),
                    data["password"].as_str().unwrap_or("-")
                );
            }
        },

        Commands::Health => {
            let data = api.request("GET", "/health", None, None).await;

            if cli.json {
                print_json(&data);
                return;
            }

            let status = data["status"].as_str().unwrap_or("unknown");
            let pd = if data["pd_healthy"].as_bool().unwrap_or(false) {
                "✓"
            } else {
                "✗"
            };
            println!("Status: {status}  PD: {pd}");
        }

        Commands::Info => {
            let data = api.request("GET", "/info", None, None).await;

            if cli.json {
                print_json(&data);
                return;
            }

            println!(
                "{} v{}",
                data["name"].as_str().unwrap_or("pg-tikv Admin API"),
                data["version"].as_str().unwrap_or("?")
            );
        }
    }
}
