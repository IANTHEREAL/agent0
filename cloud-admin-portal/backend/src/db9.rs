use std::collections::HashMap;
use std::io::{self, Write};
use std::process;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use pgtikv_admin::cli_common::{
    format_time, format_val, print_csv, print_json, print_table, ApiClient,
};
use serde_json::Value;

const DEFAULT_API_URL: &str = "http://localhost:8090/api";

// ── Output format enum ──────────────────────────────────────────

#[derive(Clone, Debug, ValueEnum)]
enum OutputFormat {
    /// Table format (default)
    Table,
    /// JSON format
    Json,
    /// CSV format
    Csv,
}

// ── CLI definition ──────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "db9",
    about = "db9 — Customer CLI for pg-tikv database service",
    version
)]
struct Cli {
    /// API base URL (env: DB9_API_URL)
    #[arg(long, env = "DB9_API_URL", default_value = DEFAULT_API_URL)]
    api_url: String,

    /// Output format
    #[arg(long, global = true, default_value = "table")]
    output: OutputFormat,

    /// Output as JSON (alias for --output json)
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

impl Cli {
    /// Get the effective output format, respecting --json flag for backward compatibility
    fn effective_output(&self) -> OutputFormat {
        if self.json {
            OutputFormat::Json
        } else {
            self.output.clone()
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Register a new account
    Register,
    /// Login to your account
    Login,
    /// Logout (remove stored credentials)
    Logout,
    /// Database management
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
    /// Token management
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Generate shell completion scripts
    Completion {
        /// Shell to generate for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum DbAction {
    /// Create a new database
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        region: Option<String>,
    },
    /// List your databases
    List,
    /// Get database status and details
    Status { id: String },
    /// Delete a database
    Delete { id: String },
    /// Reset database admin password
    ResetPassword { id: String },
    /// Show connection info for a database
    Connect { id: String },
    /// Inspect database observability metrics
    Inspect {
        /// Database ID
        id: String,
        #[command(subcommand)]
        action: Option<InspectAction>,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    /// List your API tokens
    List,
    /// Revoke a token
    Revoke { token_id: String },
}

#[derive(Subcommand)]
enum InspectAction {
    /// Show query samples and performance
    Queries,
    /// Show combined summary + queries report
    Report,
}

// ── Config helpers ──────────────────────────────────────────────

fn config_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| {
            eprintln!("Cannot determine home directory");
            process::exit(1);
        })
        .join(".db9")
}

fn ensure_config_dir() -> std::path::PathBuf {
    let dir = config_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
            eprintln!("Failed to create config directory: {e}");
            process::exit(1);
        });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&dir, perms).ok();
        }
    }
    dir
}

fn load_token() -> Result<String, String> {
    let cred_path = config_dir().join("credentials");
    let content = std::fs::read_to_string(&cred_path)
        .map_err(|_| "Not logged in. Run 'db9 login' first.".to_string())?;
    let parsed: toml::Table = content
        .parse()
        .map_err(|_| "Not logged in. Run 'db9 login' first.".to_string())?;
    parsed
        .get("token")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "Not logged in. Run 'db9 login' first.".to_string())
}

fn save_token(token: &str) -> Result<(), String> {
    let dir = ensure_config_dir();
    let cred_path = dir.join("credentials");
    let content = format!("token = \"{token}\"\n");
    std::fs::write(&cred_path, content).map_err(|e| format!("Failed to save credentials: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&cred_path, perms)
            .map_err(|e| format!("Failed to set file permissions: {e}"))?;
    }
    Ok(())
}

fn require_token() -> String {
    match load_token() {
        Ok(t) => t,
        Err(msg) => {
            eprintln!("{msg}");
            process::exit(1);
        }
    }
}

fn make_auth_headers(token: &str) -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    headers
}

fn prompt_email() -> String {
    print!("Email: ");
    io::stdout().flush().ok();
    let mut email = String::new();
    io::stdin().read_line(&mut email).unwrap_or_else(|e| {
        eprintln!("Failed to read email: {e}");
        process::exit(1);
    });
    email.trim().to_string()
}

fn prompt_password(prompt: &str) -> String {
    rpassword::prompt_password(prompt).unwrap_or_else(|e| {
        eprintln!("Failed to read password: {e}");
        process::exit(1);
    })
}

// ── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let api = ApiClient::new(&cli.api_url, None);

    match cli.command {
        Commands::Register => cmd_register(&api, &cli.effective_output()).await,
        Commands::Login => cmd_login(&api, &cli.effective_output()).await,
        Commands::Logout => cmd_logout(),
        Commands::Completion { shell } => cmd_completion(shell),
        Commands::Db { ref action } => match action {
            DbAction::Create { name, region } => {
                cmd_db_create(&api, &cli.effective_output(), name, region.as_deref()).await
            }
            DbAction::List => cmd_db_list(&api, &cli.effective_output()).await,
            DbAction::Status { id } => cmd_db_status(&api, &cli.effective_output(), id).await,
            DbAction::Delete { id } => cmd_db_delete(&api, &cli.effective_output(), id).await,
            DbAction::ResetPassword { id } => {
                cmd_db_reset_password(&api, &cli.effective_output(), id).await
            }
            DbAction::Connect { id } => cmd_db_connect(&api, &cli.effective_output(), id).await,
            DbAction::Inspect { id, action } => match action {
                None => cmd_db_inspect(&api, &cli.effective_output(), id).await,
                Some(InspectAction::Queries) => {
                    cmd_db_inspect_queries(&api, &cli.effective_output(), id).await
                }
                Some(InspectAction::Report) => {
                    cmd_db_inspect_report(&api, &cli.effective_output(), id).await
                }
            },
        },
        Commands::Token { ref action } => match action {
            TokenAction::List => cmd_token_list(&api, &cli.effective_output()).await,
            TokenAction::Revoke { token_id } => {
                cmd_token_revoke(&api, &cli.effective_output(), token_id).await
            }
        },
    }
}

// ── Command implementations ─────────────────────────────────────

fn cmd_completion(shell: clap_complete::Shell) {
    let mut cmd = Cli::command();
    clap_complete::generate(shell, &mut cmd, "db9", &mut io::stdout());
}

async fn cmd_register(api: &ApiClient, output: &OutputFormat) {
    let email = prompt_email();
    let password = prompt_password("Password: ");
    let confirm = prompt_password("Confirm password: ");

    if password != confirm {
        eprintln!("Passwords do not match.");
        process::exit(1);
    }

    let body = serde_json::json!({
        "email": email,
        "password": password,
    });
    let data = api
        .request("POST", "/customer/register", Some(&body), None)
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => println!("Account created successfully! Run 'db9 login' to get started."),
    }
}

async fn cmd_login(api: &ApiClient, output: &OutputFormat) {
    let email = prompt_email();
    let password = prompt_password("Password: ");

    let body = serde_json::json!({
        "email": email,
        "password": password,
    });
    let data = api
        .request("POST", "/customer/login", Some(&body), None)
        .await;

    let token = data["token"].as_str().unwrap_or_else(|| {
        eprintln!("Login failed: no token in response");
        process::exit(1);
    });

    if let Err(e) = save_token(token) {
        eprintln!("{e}");
        process::exit(1);
    }

    match output {
        OutputFormat::Json => {
            let safe = serde_json::json!({
                "expires_at": data["expires_at"],
            });
            print_json(&safe);
        }
        _ => {
            println!(
                "Login successful! Token expires: {}",
                format_time(data.get("expires_at"))
            );
        }
    }
}

fn cmd_logout() {
    let cred_path = config_dir().join("credentials");
    if cred_path.exists() {
        std::fs::remove_file(&cred_path).unwrap_or_else(|e| {
            eprintln!("Failed to remove credentials: {e}");
            process::exit(1);
        });
    }
    println!("Logged out successfully.");
}

async fn cmd_db_create(api: &ApiClient, output: &OutputFormat, name: &str, region: Option<&str>) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let mut body = serde_json::json!({ "name": name });
    if let Some(r) = region {
        body["region"] = Value::String(r.to_string());
    }

    let data = api
        .request("POST", "/customer/databases", Some(&body), Some(&headers))
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => {
            println!("Database created successfully!\n");
            println!("ID:          {}", format_val(data.get("id")));
            println!("Name:        {}", format_val(data.get("name")));
            println!("State:       {}", format_val(data.get("state")));
            if let Some(r) = data.get("region").and_then(|v| v.as_str()) {
                println!("Region:      {r}");
            }
            if let Some(user) = data.get("admin_user").and_then(|v| v.as_str()) {
                println!("Admin User:  {user}");
            }
            if let Some(pass) = data.get("admin_password").and_then(|v| v.as_str()) {
                println!("Admin Pass:  {pass}");
            }

            if let Some(conn) = data.get("connection_string").and_then(|v| v.as_str()) {
                println!("\nConnection String:");
                println!("  {conn}");
                println!("\npsql Command:");
                println!("  psql \"{conn}\"");
            }
        }
    }
}

async fn cmd_db_list(api: &ApiClient, output: &OutputFormat) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let data = api
        .request("GET", "/customer/databases", None, Some(&headers))
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        OutputFormat::Csv => {
            let mut items = data.as_array().cloned().unwrap_or_default();
            for item in &mut items {
                if let Some(obj) = item.as_object_mut() {
                    let formatted = format_time(obj.get("created_at"));
                    obj.insert("created_at".into(), Value::String(formatted));
                }
            }
            print_csv(
                &items,
                &[
                    ("ID", "id", 12),
                    ("NAME", "name", 15),
                    ("STATE", "state", 8),
                    ("REGION", "region", 10),
                    ("CREATED", "created_at", 16),
                ],
            );
        }
        OutputFormat::Table => {
            let mut items = data.as_array().cloned().unwrap_or_default();
            for item in &mut items {
                if let Some(obj) = item.as_object_mut() {
                    let formatted = format_time(obj.get("created_at"));
                    obj.insert("created_at".into(), Value::String(formatted));
                }
            }
            print_table(
                &items,
                &[
                    ("ID", "id", 12),
                    ("NAME", "name", 15),
                    ("STATE", "state", 8),
                    ("REGION", "region", 10),
                    ("CREATED", "created_at", 16),
                ],
            );
        }
    }
}

async fn cmd_db_status(api: &ApiClient, output: &OutputFormat, id: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let data = api
        .request(
            "GET",
            &format!("/customer/databases/{id}"),
            None,
            Some(&headers),
        )
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => {
            println!("Database: {}", format_val(data.get("name")));
            println!("ID:       {}", format_val(data.get("id")));
            println!("State:    {}", format_val(data.get("state")));
            if let Some(r) = data.get("region").and_then(|v| v.as_str()) {
                println!("Region:   {r}");
            }
            println!("Created:  {}", format_time(data.get("created_at")));

            if let Some(eps) = data["endpoints"].as_array() {
                if !eps.is_empty() {
                    println!("\nEndpoints:");
                    for ep in eps {
                        println!(
                            "  Host: {}  Port: {}  Type: {}",
                            ep["host"].as_str().unwrap_or("?"),
                            ep["port"].as_u64().unwrap_or(0),
                            ep["type"].as_str().unwrap_or("-"),
                        );
                    }
                }
            }

            if let Some(conn) = data.get("connection_string").and_then(|v| v.as_str()) {
                println!("\nConnection:");
                println!("  {conn}");
            }
        }
    }
}

async fn cmd_db_delete(api: &ApiClient, output: &OutputFormat, id: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    if !matches!(output, OutputFormat::Json) {
        print!("Are you sure you want to delete database {id}? This cannot be undone. [y/N] ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).unwrap_or_else(|e| {
            eprintln!("Failed to read input: {e}");
            process::exit(1);
        });
        let answer = answer.trim();
        if answer != "y" && answer != "Y" {
            println!("Cancelled.");
            return;
        }
    }

    let data = api
        .request(
            "DELETE",
            &format!("/customer/databases/{id}"),
            None,
            Some(&headers),
        )
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => println!("Database {id} has been disabled."),
    }
}

async fn cmd_db_reset_password(api: &ApiClient, output: &OutputFormat, id: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let data = api
        .request(
            "POST",
            &format!("/customer/databases/{id}/reset-password"),
            None,
            Some(&headers),
        )
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => {
            println!("Password reset successfully!\n");
            if let Some(user) = data.get("admin_user").and_then(|v| v.as_str()) {
                println!("Admin User:  {user}");
            }
            if let Some(pass) = data.get("admin_password").and_then(|v| v.as_str()) {
                println!("Admin Pass:  {pass}");
            }
            if let Some(conn) = data.get("connection_string").and_then(|v| v.as_str()) {
                println!("\nConnection String:");
                println!("  {conn}");
                println!("\npsql Command:");
                println!("  psql \"{conn}\"");
            }
        }
    }
}

async fn cmd_db_connect(api: &ApiClient, output: &OutputFormat, id: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let data = api
        .request(
            "GET",
            &format!("/customer/databases/{id}"),
            None,
            Some(&headers),
        )
        .await;

    match output {
        OutputFormat::Json => {
            let mut result = serde_json::json!({});
            if let Some(conn) = data.get("connection_string") {
                result["connection_string"] = conn.clone();
            }
            if let Some(eps) = data.get("endpoints") {
                result["endpoints"] = eps.clone();
            }
            print_json(&result);
        }
        _ => {
            if let Some(conn) = data.get("connection_string").and_then(|v| v.as_str()) {
                println!("Connection String:");
                println!("  {conn}");
                println!("\npsql Command:");
                println!("  psql \"{conn}\"");
            }
        }
    }
}

fn format_duration_ago(ms: i64) -> String {
    if ms < 1000 {
        format!("{}ms ago", ms)
    } else if ms < 60_000 {
        format!("{}s ago", ms / 1000)
    } else if ms < 3_600_000 {
        format!("{}m ago", ms / 60_000)
    } else {
        format!("{}h ago", ms / 3_600_000)
    }
}

fn print_inspect_summary(data: &Value, id: &str) {
    let summary = &data["summary"];
    let window = summary["window_seconds"].as_i64().unwrap_or(0);
    println!("Database: {id}");
    println!("Window: {window} seconds\n");
    println!(" {:<20} Value", "Metric");
    println!("{}", "─".repeat(37));
    println!(
        " {:<20} {:.1}",
        "QPS",
        summary["qps"].as_f64().unwrap_or(0.0)
    );
    println!(
        " {:<20} {:.1}",
        "TPS",
        summary["tps"].as_f64().unwrap_or(0.0)
    );
    println!(
        " {:<20} {:.1} ms",
        "Latency (avg)",
        summary["latency_avg_ms"].as_f64().unwrap_or(0.0)
    );
    println!(
        " {:<20} {:.1} ms",
        "Latency (p99)",
        summary["latency_p99_ms"].as_f64().unwrap_or(0.0)
    );
    println!(
        " {:<20} {}",
        "Active Connections",
        summary["active_connections"].as_i64().unwrap_or(0)
    );
    println!(
        " {:<20} {}",
        "Statements",
        summary["statement_count"].as_i64().unwrap_or(0)
    );
    println!(
        " {:<20} {}",
        "Commits",
        summary["txn_commit_count"].as_i64().unwrap_or(0)
    );
    println!(
        " {:<20} {}",
        "Errors",
        summary["error_count"].as_i64().unwrap_or(0)
    );
}

fn print_inspect_queries(data: &Value) {
    let window = data["summary"]["window_seconds"].as_i64().unwrap_or(0);
    println!("Query Samples (last {}s)\n", window);

    let samples = match data["samples"].as_array() {
        Some(arr) if !arr.is_empty() => arr,
        _ => {
            println!("No query samples in the current window.");
            return;
        }
    };

    println!(
        "{:<45} {:>6} {:>6} {:>8} {:>8} {:>8} {:>10}",
        "Query", "Count", "Errors", "Avg(ms)", "P99(ms)", "Max(ms)", "Last Seen"
    );
    println!("{}", "─".repeat(97));

    for s in samples {
        let query = s["query"].as_str().unwrap_or("-");
        let truncated = if query.len() > 45 {
            format!("{}...", &query[..42])
        } else {
            query.to_string()
        };
        let last_seen = format_duration_ago(s["last_seen_ms_ago"].as_i64().unwrap_or(0));
        println!(
            "{:<45} {:>6} {:>6} {:>8.1} {:>8.1} {:>8.1} {:>10}",
            truncated,
            s["sample_count"].as_i64().unwrap_or(0),
            s["error_count"].as_i64().unwrap_or(0),
            s["latency_avg_ms"].as_f64().unwrap_or(0.0),
            s["latency_p99_ms"].as_f64().unwrap_or(0.0),
            s["latency_max_ms"].as_f64().unwrap_or(0.0),
            last_seen,
        );
    }
}

async fn fetch_observability(api: &ApiClient, id: &str) -> Value {
    let token = require_token();
    let headers = make_auth_headers(&token);
    api.request(
        "GET",
        &format!("/customer/databases/{id}/observability"),
        None,
        Some(&headers),
    )
    .await
}

async fn cmd_db_inspect(api: &ApiClient, output: &OutputFormat, id: &str) {
    let data = fetch_observability(api, id).await;
    match output {
        OutputFormat::Json => print_json(&data["summary"]),
        _ => print_inspect_summary(&data, id),
    }
}

async fn cmd_db_inspect_queries(api: &ApiClient, output: &OutputFormat, id: &str) {
    let data = fetch_observability(api, id).await;
    match output {
        OutputFormat::Json => print_json(&data["samples"]),
        _ => print_inspect_queries(&data),
    }
}

async fn cmd_db_inspect_report(api: &ApiClient, output: &OutputFormat, id: &str) {
    let data = fetch_observability(api, id).await;
    match output {
        OutputFormat::Json => print_json(&data),
        _ => {
            print_inspect_summary(&data, id);
            println!();
            print_inspect_queries(&data);
        }
    }
}

async fn cmd_token_list(api: &ApiClient, output: &OutputFormat) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let data = api
        .request("GET", "/customer/tokens", None, Some(&headers))
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        OutputFormat::Csv => {
            let mut items = data.as_array().cloned().unwrap_or_default();
            for item in &mut items {
                if let Some(obj) = item.as_object_mut() {
                    let created = format_time(obj.get("created_at"));
                    obj.insert("created_at".into(), Value::String(created));
                    let expires = format_time(obj.get("expires_at"));
                    obj.insert("expires_at".into(), Value::String(expires));
                }
            }
            print_csv(
                &items,
                &[
                    ("ID", "id", 36),
                    ("NAME", "name", 10),
                    ("CREATED", "created_at", 16),
                    ("EXPIRES", "expires_at", 16),
                ],
            );
        }
        OutputFormat::Table => {
            let mut items = data.as_array().cloned().unwrap_or_default();
            for item in &mut items {
                if let Some(obj) = item.as_object_mut() {
                    let created = format_time(obj.get("created_at"));
                    obj.insert("created_at".into(), Value::String(created));
                    let expires = format_time(obj.get("expires_at"));
                    obj.insert("expires_at".into(), Value::String(expires));
                }
            }
            print_table(
                &items,
                &[
                    ("ID", "id", 36),
                    ("NAME", "name", 10),
                    ("CREATED", "created_at", 16),
                    ("EXPIRES", "expires_at", 16),
                ],
            );
        }
    }
}

async fn cmd_token_revoke(api: &ApiClient, output: &OutputFormat, token_id: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let data = api
        .request(
            "DELETE",
            &format!("/customer/tokens/{token_id}"),
            None,
            Some(&headers),
        )
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => println!("Token revoked."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_make_auth_headers() {
        let headers = make_auth_headers("test-token-123");
        assert_eq!(
            headers.get("Authorization").unwrap(),
            "Bearer test-token-123"
        );
        assert_eq!(headers.len(), 1);
    }

    #[test]
    fn test_config_dir_ends_with_db9() {
        let dir = config_dir();
        assert!(dir.ends_with(".db9"));
    }

    #[test]
    fn test_save_and_load_token_roundtrip() {
        let temp_dir = std::env::temp_dir().join(format!("db9-test-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cred_path = temp_dir.join("credentials");

        let token = "abc123def456";
        let content = format!("token = \"{token}\"\n");
        std::fs::write(&cred_path, &content).unwrap();

        let parsed: toml::Table = content.parse().unwrap();
        let loaded = parsed.get("token").and_then(|v| v.as_str()).unwrap();
        assert_eq!(loaded, token);

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_toml_parsing_with_token() {
        let content = "token = \"my-secret-token\"\n";
        let parsed: toml::Table = content.parse().unwrap();
        let token = parsed.get("token").and_then(|v| v.as_str()).unwrap();
        assert_eq!(token, "my-secret-token");
    }

    #[test]
    fn test_toml_parsing_empty() {
        let content = "";
        let parsed: toml::Table = content.parse().unwrap();
        assert!(parsed.get("token").is_none());
    }
}
