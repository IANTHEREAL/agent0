use std::collections::HashMap;
use std::io::{self, Write};
use std::process;

use clap::{Parser, Subcommand};
use pgtikv_admin::cli_common::{ApiClient, format_time, format_val, print_json, print_table};
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
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long)]
        tag: Option<String>,
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
    /// Batch create tenants
    BatchCreate {
        #[arg(long)]
        count: u32,
        #[arg(long, default_value = DEFAULT_ADMIN_USER)]
        admin_user: String,
        #[arg(long)]
        admin_password: Option<String>,
    },
    /// Batch delete tenants
    BatchDelete { ids: Vec<String> },
    /// Batch update tenant metadata
    BatchUpdate {
        ids: Vec<String>,
        #[arg(long)]
        notes: Option<String>,
        #[arg(long)]
        tags: Option<String>,
    },
    /// Export tenants to stdout (JSONL or CSV)
    Export {
        #[arg(long, default_value = "jsonl")]
        format: String,
        #[arg(long)]
        state: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        #[arg(long, default_value_t = 500)]
        batch_size: u32,
    },
    /// Import tenants from JSONL file (one tenant per line)
    Import {
        #[arg(long)]
        file: String,
        #[arg(long, default_value = DEFAULT_ADMIN_USER)]
        admin_user: String,
        #[arg(long)]
        admin_password: Option<String>,
        #[arg(long, default_value_t = 100)]
        batch_size: u32,
        #[arg(long, default_value_t = 4)]
        parallel: u32,
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
                cursor,
                tag,
            } => {
                let mut path = format!("/tenants?page={page}&size={size}");
                if let Some(st) = &state {
                    path.push_str(&format!("&state={st}"));
                }
                if let Some(q) = &query {
                    path.push_str(&format!("&q={q}"));
                }
                if let Some(c) = &cursor {
                    path.push_str(&format!("&cursor={c}"));
                }
                if let Some(t) = &tag {
                    path.push_str(&format!("&tag={t}"));
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

                if total >= 0 {
                    let pages = ((total as f64) / (size as f64)).ceil().max(1.0) as i64;
                    println!("\n{total} tenant(s), page {page}/{pages}");
                } else {
                    println!("\n{} tenant(s) returned", items.len());
                }

                if let Some(nc) = data["next_cursor"].as_str() {
                    println!("Next cursor: {nc}");
                }
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
                println!(
                    "Admin user:     {}",
                    data["admin_user"].as_str().unwrap_or("-")
                );
                println!(
                    "Admin password: {}",
                    data["admin_password"].as_str().unwrap_or("-")
                );
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

            TenantAction::BatchCreate {
                count,
                admin_user,
                admin_password,
            } => {
                let mut body = serde_json::json!({ "count": count, "admin_user": admin_user });
                if let Some(pw) = &admin_password {
                    body["admin_password"] = Value::String(pw.clone());
                }
                let data = api
                    .request("POST", "/tenants/batch", Some(&body), None)
                    .await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                let created = data["total_created"].as_u64().unwrap_or(0);
                let requested = data["total_requested"].as_u64().unwrap_or(0);
                println!("Created {created}/{requested} tenant(s)");

                if let Some(items) = data["created"].as_array() {
                    for item in items {
                        println!(
                            "  {} (password: {})",
                            item["id"].as_str().unwrap_or("-"),
                            item["admin_password"].as_str().unwrap_or("-")
                        );
                    }
                }
                if let Some(items) = data["failed"].as_array() {
                    for item in items {
                        eprintln!(
                            "  FAILED {}: {}",
                            item["id"].as_str().unwrap_or("-"),
                            item["error"].as_str().unwrap_or("-")
                        );
                    }
                }
            }

            TenantAction::BatchDelete { ids } => {
                let body = serde_json::json!({ "ids": ids });
                let data = api
                    .request("POST", "/tenants/batch-delete", Some(&body), None)
                    .await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                if let Some(items) = data["deleted"].as_array() {
                    println!("Deleted {} tenant(s)", items.len());
                    for id in items {
                        println!("  {}", id.as_str().unwrap_or("-"));
                    }
                }
                if let Some(items) = data["failed"].as_array() {
                    for item in items {
                        eprintln!(
                            "  FAILED {}: {}",
                            item["id"].as_str().unwrap_or("-"),
                            item["error"].as_str().unwrap_or("-")
                        );
                    }
                }
            }

            TenantAction::BatchUpdate { ids, notes, tags } => {
                let mut body = serde_json::json!({ "ids": ids });
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
                    .request("POST", "/tenants/batch-update", Some(&body), None)
                    .await;

                if cli.json {
                    print_json(&data);
                    return;
                }

                if let Some(items) = data["updated"].as_array() {
                    println!("Updated {} tenant(s)", items.len());
                }
                if let Some(items) = data["failed"].as_array() {
                    for item in items {
                        eprintln!(
                            "  FAILED {}: {}",
                            item["id"].as_str().unwrap_or("-"),
                            item["error"].as_str().unwrap_or("-")
                        );
                    }
                }
            }

            TenantAction::Export {
                format,
                state,
                tag,
                batch_size,
            } => {
                let stdout = io::stdout();
                let mut out = stdout.lock();
                let mut cursor: Option<String> = None;
                let mut total_exported = 0u64;

                if format == "csv" {
                    writeln!(out, "id,state,created_at,notes,tags").ok();
                }

                loop {
                    let mut path = format!("/tenants?size={batch_size}");
                    if let Some(st) = &state {
                        path.push_str(&format!("&state={st}"));
                    }
                    if let Some(t) = &tag {
                        path.push_str(&format!("&tag={t}"));
                    }
                    if let Some(c) = &cursor {
                        path.push_str(&format!("&cursor={c}"));
                    }

                    let data = api.request("GET", &path, None, None).await;
                    let items = data["items"].as_array().cloned().unwrap_or_default();

                    if items.is_empty() {
                        break;
                    }

                    for item in &items {
                        if format == "csv" {
                            let id = item["id"].as_str().unwrap_or("");
                            let st = item["state"].as_str().unwrap_or("");
                            let created = item["created_at"].as_str().unwrap_or("");
                            let notes = item["notes"].as_str().unwrap_or("");
                            let tags = format_val(item.get("tags"));
                            writeln!(out, "{id},{st},{created},{notes},{tags}").ok();
                        } else {
                            writeln!(out, "{}", serde_json::to_string(item).unwrap_or_default())
                                .ok();
                        }
                        total_exported += 1;
                    }

                    match data["next_cursor"].as_str() {
                        Some(nc) => cursor = Some(nc.to_string()),
                        None => break,
                    }
                }

                eprintln!("Exported {total_exported} tenant(s)");
            }

            TenantAction::Import {
                file,
                admin_user,
                admin_password,
                batch_size,
                parallel: _parallel,
            } => {
                let content = std::fs::read_to_string(&file).unwrap_or_else(|e| {
                    eprintln!("Failed to read {file}: {e}");
                    process::exit(1);
                });

                let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
                let total_lines = lines.len();
                let mut total_created = 0u64;
                let mut total_failed = 0u64;

                for chunk in lines.chunks(batch_size as usize) {
                    let count = chunk.len() as u32;
                    let mut body = serde_json::json!({
                        "count": count,
                        "admin_user": admin_user,
                    });
                    if let Some(pw) = &admin_password {
                        body["admin_password"] = Value::String(pw.clone());
                    }

                    let data = api
                        .request("POST", "/tenants/batch", Some(&body), None)
                        .await;
                    let created = data["total_created"].as_u64().unwrap_or(0);
                    let failed_items = data["failed"]
                        .as_array()
                        .map(|a| a.len() as u64)
                        .unwrap_or(0);
                    total_created += created;
                    total_failed += failed_items;

                    eprint!("\rImported {total_created}/{total_lines}...");
                }

                eprintln!("\nImport complete: {total_created} created, {total_failed} failed");
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
                .request(
                    "POST",
                    &format!("/tenants/{tenant_id}/connect"),
                    Some(&body),
                    None,
                )
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
            UserAction::List { tenant_id, session } => {
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
                println!(
                    "Connect:      {}",
                    data["connection"].as_str().unwrap_or("-")
                );
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
