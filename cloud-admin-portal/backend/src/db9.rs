use std::collections::HashMap;
use std::io::{self, Write};
use std::process;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use pgtikv_admin::cli_common::{
    format_time, format_val, print_csv, print_json, print_table, ApiClient,
};
use serde_json::Value;

mod repl;

const DEFAULT_API_URL: &str = "https://db9.shared.aws.tidbcloud.com/api";

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
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")")
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

    /// Skip TLS certificate verification (env: DB9_INSECURE)
    #[arg(long, global = true, env = "DB9_INSECURE")]
    insecure: bool,

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
    /// Claim anonymous account with email and password
    Claim,
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
    /// Generate code from database schema
    Gen {
        #[command(subcommand)]
        action: GenAction,
    },
    /// Database migration management
    Migration {
        #[command(subcommand)]
        action: MigrationAction,
    },
    /// Guided setup: register, login, and create your first database
    Init,
    /// Launch interactive filesystem shell for a database
    Sh {
        /// Database ID (omit to auto-select or choose interactively)
        id: Option<String>,
        /// Execute a shell command and exit
        #[arg(short = 'c')]
        command: Option<String>,
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
    Delete {
        id: String,
        /// Skip confirmation prompt
        #[arg(long, short)]
        yes: bool,
    },
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
    /// Execute SQL query against a database
    Sql {
        /// Database ID
        id: String,
        /// SQL query string
        #[arg(long, short)]
        query: Option<String>,
        /// Path to SQL file
        #[arg(long, short)]
        file: Option<String>,
    },
    /// Manage database users
    Users {
        /// Database ID
        id: String,
        #[command(subcommand)]
        action: UserAction,
    },
    /// Execute a seed SQL file
    Seed {
        /// Database ID
        id: String,
        /// Path to seed SQL file
        file: String,
    },
    /// Export database schema (and optionally data) as SQL
    Dump {
        /// Database ID
        id: String,
        /// Export DDL only (no data)
        #[arg(long)]
        ddl_only: bool,
        /// Write output to file instead of stdout
        #[arg(short, long)]
        output_file: Option<String>,
    },
    /// Database branching
    Branch {
        #[command(subcommand)]
        action: BranchAction,
    },
}

#[derive(Subcommand)]
enum BranchAction {
    /// Create a branch (schema copy) from a database
    Create {
        /// Source database ID
        id: String,
        /// Branch name
        #[arg(long)]
        name: String,
    },
    /// List branches of a database
    List {
        /// Source database ID
        id: String,
    },
    /// Delete a branch database
    Delete {
        /// Branch database ID
        id: String,
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
    /// List database schemas
    Schemas,
    /// List database tables
    Tables,
    /// List database indexes
    Indexes,
    /// Show slow queries sorted by p99 latency
    SlowQueries,
}

#[derive(Subcommand)]
enum UserAction {
    /// List database users
    List,
    /// Create a new database user
    Create {
        /// Username
        #[arg(long)]
        username: String,
        /// Password
        #[arg(long)]
        password: String,
    },
    /// Delete a database user
    Delete {
        /// Username to delete
        #[arg(long)]
        username: String,
    },
}

#[derive(Subcommand)]
enum GenAction {
    /// Generate type definitions from database schema
    Types {
        /// Database ID
        id: String,
        /// Target language
        #[arg(long, default_value = "typescript")]
        lang: TypeLang,
        /// Schema to generate types for
        #[arg(long, default_value = "public")]
        schema: String,
    },
}

#[derive(Clone, Debug, ValueEnum)]
enum TypeLang {
    Typescript,
    Python,
}

#[derive(Subcommand)]
enum MigrationAction {
    /// Create a new migration file
    New {
        /// Migration name (used in filename)
        name: String,
        /// Directory for migration files
        #[arg(long, default_value = "./migrations")]
        dir: String,
    },
    /// List local migration files
    List {
        /// Directory for migration files
        #[arg(long, default_value = "./migrations")]
        dir: String,
    },
    /// Apply pending migrations to a database
    Up {
        /// Database ID
        id: String,
        /// Directory for migration files
        #[arg(long, default_value = "./migrations")]
        dir: String,
    },
    /// Show migration status (applied vs pending)
    Status {
        /// Database ID
        id: String,
        /// Directory for migration files
        #[arg(long, default_value = "./migrations")]
        dir: String,
    },
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
    let existing = std::fs::read_to_string(&cred_path).unwrap_or_default();
    let mut parsed: toml::Table = existing.parse().unwrap_or_default();
    parsed.insert("token".to_string(), toml::Value::String(token.to_string()));
    let content =
        toml::to_string(&parsed).map_err(|e| format!("Failed to serialize credentials: {e}"))?;
    std::fs::write(&cred_path, &content).map_err(|e| format!("Failed to save credentials: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&cred_path, perms)
            .map_err(|e| format!("Failed to set file permissions: {e}"))?;
    }
    Ok(())
}

fn save_anonymous_flag(is_anonymous: bool) -> Result<(), String> {
    let dir = ensure_config_dir();
    let cred_path = dir.join("credentials");
    let existing = std::fs::read_to_string(&cred_path).unwrap_or_default();
    let mut parsed: toml::Table = existing.parse().unwrap_or_default();
    if is_anonymous {
        parsed.insert("is_anonymous".to_string(), toml::Value::Boolean(true));
    } else {
        parsed.remove("is_anonymous");
    }
    let content =
        toml::to_string(&parsed).map_err(|e| format!("Failed to serialize credentials: {e}"))?;
    std::fs::write(&cred_path, content).map_err(|e| format!("Failed to save credentials: {e}"))?;
    Ok(())
}

fn save_anonymous_credentials(anonymous_id: &str, anonymous_secret: &str) -> Result<(), String> {
    let dir = ensure_config_dir();
    let cred_path = dir.join("credentials");
    let existing = std::fs::read_to_string(&cred_path).unwrap_or_default();
    let mut parsed: toml::Table = existing.parse().unwrap_or_default();
    parsed.insert(
        "anonymous_id".to_string(),
        toml::Value::String(anonymous_id.to_string()),
    );
    parsed.insert(
        "anonymous_secret".to_string(),
        toml::Value::String(anonymous_secret.to_string()),
    );
    let content =
        toml::to_string(&parsed).map_err(|e| format!("Failed to serialize credentials: {e}"))?;
    std::fs::write(&cred_path, content).map_err(|e| format!("Failed to save credentials: {e}"))?;
    Ok(())
}

fn clear_anonymous_credentials() -> Result<(), String> {
    let cred_path = config_dir().join("credentials");
    if !cred_path.exists() {
        return Ok(());
    }
    let existing = std::fs::read_to_string(&cred_path).unwrap_or_default();
    let mut parsed: toml::Table = existing.parse().unwrap_or_default();
    parsed.remove("anonymous_id");
    parsed.remove("anonymous_secret");
    parsed.remove("is_anonymous");
    let content =
        toml::to_string(&parsed).map_err(|e| format!("Failed to serialize credentials: {e}"))?;
    std::fs::write(&cred_path, content).map_err(|e| format!("Failed to save credentials: {e}"))?;
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

async fn migrate_anonymous_secret_if_needed(api: &ApiClient) {
    let cred_path = config_dir().join("credentials");
    let content = match std::fs::read_to_string(&cred_path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let parsed: toml::Table = match content.parse() {
        Ok(t) => t,
        Err(_) => return,
    };

    let is_anon = parsed
        .get("is_anonymous")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_secret = parsed.get("anonymous_secret").is_some();
    let has_token = parsed.get("token").is_some();

    if !is_anon || has_secret || !has_token {
        return;
    }

    let token = parsed.get("token").and_then(|v| v.as_str()).unwrap();
    let headers = make_auth_headers(token);

    let data = api
        .request(
            "POST",
            "/customer/anonymous-secret",
            None::<&serde_json::Value>,
            Some(&headers),
        )
        .await;

    if let (Some(aid), Some(asec)) = (
        data["anonymous_id"].as_str(),
        data["anonymous_secret"].as_str(),
    ) {
        save_anonymous_credentials(aid, asec).ok();
    }
}

// ── Main ────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let api = ApiClient::new_with_options(&cli.api_url, None, cli.insecure).with_auto_reauth();

    migrate_anonymous_secret_if_needed(&api).await;

    match cli.command {
        Commands::Register => cmd_register(&api, &cli.effective_output()).await,
        Commands::Login => cmd_login(&api, &cli.effective_output()).await,
        Commands::Claim => cmd_claim(&api, &cli.effective_output()).await,
        Commands::Logout => cmd_logout(),
        Commands::Init => cmd_init(&api, &cli.effective_output()).await,
        Commands::Sh {
            ref id,
            ref command,
        } => cmd_sh(&api, &cli.api_url, id.as_deref(), command.as_deref()).await,
        Commands::Completion { shell } => cmd_completion(shell),
        Commands::Db { ref action } => match action {
            DbAction::Create { name, region } => {
                cmd_db_create(&api, &cli.effective_output(), name, region.as_deref()).await
            }
            DbAction::List => cmd_db_list(&api, &cli.effective_output()).await,
            DbAction::Status { id } => cmd_db_status(&api, &cli.effective_output(), id).await,
            DbAction::Delete { id, yes } => {
                cmd_db_delete(&api, &cli.effective_output(), id, *yes).await
            }
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
                Some(InspectAction::Schemas) => {
                    cmd_db_inspect_schemas(&api, &cli.effective_output(), id).await
                }
                Some(InspectAction::Tables) => {
                    cmd_db_inspect_tables(&api, &cli.effective_output(), id).await
                }
                Some(InspectAction::Indexes) => {
                    cmd_db_inspect_indexes(&api, &cli.effective_output(), id).await
                }
                Some(InspectAction::SlowQueries) => {
                    cmd_db_inspect_slow_queries(&api, &cli.effective_output(), id).await
                }
            },
            DbAction::Sql { id, query, file } => {
                cmd_db_sql(
                    &api,
                    &cli.effective_output(),
                    id,
                    query.as_deref(),
                    file.as_deref(),
                )
                .await
            }
            DbAction::Users { id, action } => match action {
                UserAction::List => cmd_db_users_list(&api, &cli.effective_output(), id).await,
                UserAction::Create { username, password } => {
                    cmd_db_users_create(&api, &cli.effective_output(), id, username, password).await
                }
                UserAction::Delete { username } => {
                    cmd_db_users_delete(&api, &cli.effective_output(), id, username).await
                }
            },
            DbAction::Seed { id, file } => {
                cmd_db_seed(&api, &cli.effective_output(), id, file).await
            }
            DbAction::Dump {
                id,
                ddl_only,
                output_file,
            } => {
                cmd_db_dump(
                    &api,
                    &cli.effective_output(),
                    id,
                    *ddl_only,
                    output_file.as_deref(),
                )
                .await
            }
            DbAction::Branch { action } => match action {
                BranchAction::Create { id, name } => {
                    cmd_db_branch_create(&api, &cli.effective_output(), id, name).await
                }
                BranchAction::List { id } => {
                    cmd_db_branch_list(&api, &cli.effective_output(), id).await
                }
                BranchAction::Delete { id } => {
                    cmd_db_branch_delete(&api, &cli.effective_output(), id).await
                }
            },
        },
        Commands::Gen { ref action } => match action {
            GenAction::Types { id, lang, schema } => {
                cmd_gen_types(&api, &cli.effective_output(), id, lang, schema).await
            }
        },
        Commands::Migration { ref action } => match action {
            MigrationAction::New { name, dir } => {
                cmd_migration_new(name, dir, &cli.effective_output())
            }
            MigrationAction::List { dir } => cmd_migration_list(dir, &cli.effective_output()),
            MigrationAction::Up { id, dir } => {
                cmd_migration_up(&api, id, dir, &cli.effective_output()).await
            }
            MigrationAction::Status { id, dir } => {
                cmd_migration_status(&api, id, dir, &cli.effective_output()).await
            }
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

async fn cmd_sh(api: &ApiClient, api_url: &str, id: Option<&str>, command: Option<&str>) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    // Resolve database ID
    let db_id = if let Some(id) = id {
        id.to_string()
    } else {
        let data = api
            .request("GET", "/customer/databases", None, Some(&headers))
            .await;
        let databases = data.as_array().cloned().unwrap_or_default();

        match databases.len() {
            0 => {
                eprintln!("No databases found. Create one with 'db9 db create --name <name>'.");
                process::exit(1);
            }
            1 => {
                let id = databases[0]["id"].as_str().unwrap_or_else(|| {
                    eprintln!("Failed to read database ID.");
                    process::exit(1);
                });
                let name = databases[0]["name"].as_str().unwrap_or("(unnamed)");
                eprintln!("Using database: {} ({})", name, id);
                id.to_string()
            }
            _ => {
                eprintln!("Select a database:");
                for (i, db) in databases.iter().enumerate() {
                    let id = db["id"].as_str().unwrap_or("?");
                    let name = db["name"].as_str().unwrap_or("(unnamed)");
                    let state = db["state"].as_str().unwrap_or("?");
                    eprintln!("  [{}] {} ({}) - {}", i + 1, name, id, state);
                }
                eprint!("Enter number: ");
                io::stderr().flush().ok();
                let mut input = String::new();
                io::stdin().read_line(&mut input).unwrap_or_else(|e| {
                    eprintln!("Failed to read input: {e}");
                    process::exit(1);
                });
                let choice: usize = input.trim().parse().unwrap_or_else(|_| {
                    eprintln!("Invalid selection.");
                    process::exit(1);
                });
                if choice < 1 || choice > databases.len() {
                    eprintln!("Selection out of range.");
                    process::exit(1);
                }
                let db = &databases[choice - 1];
                db["id"]
                    .as_str()
                    .unwrap_or_else(|| {
                        eprintln!("Failed to read database ID.");
                        process::exit(1);
                    })
                    .to_string()
            }
        }
    };

    // Derive fs9 URL: strip /api suffix, append /fs9/<db_id>
    let base_url = api_url.strip_suffix("/api").unwrap_or(api_url);
    let fs9_url = format!("{base_url}/fs9/{db_id}");

    // Build sh9 command
    let mut cmd = std::process::Command::new("sh9");
    cmd.arg("--server").arg(&fs9_url);
    cmd.arg("--token").arg(&token);
    if let Some(c) = command {
        cmd.arg("-c").arg(c);
    }

    // On Unix, exec replaces the process
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = cmd.exec();
        if err.kind() == io::ErrorKind::NotFound {
            eprintln!("sh9 not found. Install it with:");
            eprintln!("  curl -fsSL https://db9.shared.aws.tidbcloud.com/install-sh9 | sh");
            process::exit(1);
        }
        eprintln!("Failed to exec sh9: {err}");
        process::exit(1);
    }

    // On non-Unix, spawn and wait
    #[cfg(not(unix))]
    {
        let status = cmd.status().unwrap_or_else(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                eprintln!("sh9 not found. Install it with:");
                eprintln!("  curl -fsSL https://db9.shared.aws.tidbcloud.com/install-sh9 | sh");
                process::exit(1);
            }
            eprintln!("Failed to run sh9: {e}");
            process::exit(1);
        });
        if !status.success() {
            process::exit(status.code().unwrap_or(1));
        }
    }
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

async fn cmd_claim(api: &ApiClient, output: &OutputFormat) {
    let token = require_token();
    let headers = make_auth_headers(&token);
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
        .request("POST", "/customer/claim", Some(&body), Some(&headers))
        .await;

    if let Err(e) = clear_anonymous_credentials() {
        eprintln!("Warning: account claimed but failed to update local credentials: {e}");
    }

    match output {
        OutputFormat::Json => print_json(&data),
        _ => {
            println!(
                "Account claimed successfully: {}",
                data["email"].as_str().unwrap_or("(unknown)")
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

async fn cmd_init(api: &ApiClient, output: &OutputFormat) {
    println!("Welcome to db9! Let's get you set up.\n");

    if load_token().is_ok() {
        println!("You're already logged in.\n");
    } else {
        print!("Do you have an account? [y/N] ");
        io::stdout().flush().ok();
        let mut answer = String::new();
        io::stdin().read_line(&mut answer).ok();
        let has_account = answer.trim().eq_ignore_ascii_case("y");

        if has_account {
            println!("\n--- Login ---");
            cmd_login(api, output).await;
        } else {
            println!("\n--- Register ---");
            cmd_register(api, output).await;
            println!("\n--- Login ---");
            cmd_login(api, output).await;
        }
    }

    print!("\nCreate your first database? [Y/n] ");
    io::stdout().flush().ok();
    let mut answer = String::new();
    io::stdin().read_line(&mut answer).ok();
    let answer = answer.trim();
    if answer.is_empty() || answer.eq_ignore_ascii_case("y") {
        print!("Database name: ");
        io::stdout().flush().ok();
        let mut name = String::new();
        io::stdin().read_line(&mut name).ok();
        let name = name.trim();
        if name.is_empty() {
            eprintln!("Database name cannot be empty.");
            process::exit(1);
        }
        cmd_db_create(api, output, name, None).await;
    }

    println!("\nYou're all set! Run 'db9 --help' to see all available commands.");
}

async fn cmd_db_create(api: &ApiClient, output: &OutputFormat, name: &str, region: Option<&str>) {
    let token = match load_token() {
        Ok(t) => t,
        Err(_) => {
            eprintln!("No account found. Creating anonymous account...");
            let data = api
                .request(
                    "POST",
                    "/customer/anonymous-register",
                    None::<&serde_json::Value>,
                    None,
                )
                .await;
            let token = data["token"].as_str().unwrap_or_else(|| {
                eprintln!("Failed to create anonymous account");
                process::exit(1);
            });
            if let Err(e) = save_token(token) {
                eprintln!("{e}");
                process::exit(1);
            }
            save_anonymous_flag(true).ok();
            if let (Some(aid), Some(asec)) = (
                data["anonymous_id"].as_str(),
                data["anonymous_secret"].as_str(),
            ) {
                save_anonymous_credentials(aid, asec).ok();
            }
            eprintln!("Anonymous account created. You can claim it later with 'db9 claim'.");
            token.to_string()
        }
    };
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

async fn cmd_db_delete(api: &ApiClient, output: &OutputFormat, id: &str, skip_confirm: bool) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    if !skip_confirm && !matches!(output, OutputFormat::Json) {
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

async fn execute_sql(api: &ApiClient, id: &str, sql: &str) -> Value {
    let token = require_token();
    let headers = make_auth_headers(&token);
    let body = serde_json::json!({ "query": sql });
    api.request(
        "POST",
        &format!("/customer/databases/{id}/sql"),
        Some(&body),
        Some(&headers),
    )
    .await
}

async fn cmd_db_inspect_schemas(api: &ApiClient, output: &OutputFormat, id: &str) {
    let data = execute_sql(
        api,
        id,
        "SELECT schema_name FROM information_schema.schemata WHERE schema_name NOT IN ('pg_catalog', 'information_schema') ORDER BY schema_name",
    )
    .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => repl::output::print_sql_result(&data, output, false, &None),
    }
}

async fn cmd_db_inspect_tables(api: &ApiClient, output: &OutputFormat, id: &str) {
    let data = execute_sql(
        api,
        id,
        "SELECT table_schema, table_name, table_type FROM information_schema.tables WHERE table_schema NOT IN ('pg_catalog', 'information_schema') ORDER BY table_schema, table_name",
    )
    .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => repl::output::print_sql_result(&data, output, false, &None),
    }
}

async fn cmd_db_inspect_indexes(api: &ApiClient, output: &OutputFormat, id: &str) {
    let data = execute_sql(
        api,
        id,
        "SELECT schemaname, tablename, indexname, indexdef FROM pg_indexes WHERE schemaname NOT IN ('pg_catalog', 'information_schema') ORDER BY schemaname, tablename, indexname",
    )
    .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => repl::output::print_sql_result(&data, output, false, &None),
    }
}

async fn cmd_db_inspect_slow_queries(api: &ApiClient, output: &OutputFormat, id: &str) {
    let data = fetch_observability(api, id).await;

    match output {
        OutputFormat::Json => {
            let mut samples = data["samples"].as_array().cloned().unwrap_or_default();
            samples.sort_by(|a, b| {
                let a_p99 = a["latency_p99_ms"].as_f64().unwrap_or(0.0);
                let b_p99 = b["latency_p99_ms"].as_f64().unwrap_or(0.0);
                b_p99
                    .partial_cmp(&a_p99)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            print_json(&Value::Array(samples));
        }
        _ => {
            let mut samples = data["samples"].as_array().cloned().unwrap_or_default();
            samples.sort_by(|a, b| {
                let a_p99 = a["latency_p99_ms"].as_f64().unwrap_or(0.0);
                let b_p99 = b["latency_p99_ms"].as_f64().unwrap_or(0.0);
                b_p99
                    .partial_cmp(&a_p99)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });

            if samples.is_empty() {
                println!("No slow queries found.");
                return;
            }

            println!("Slow Queries (sorted by P99 latency)\n");
            println!(
                "{:<45} {:>8} {:>8} {:>8} {:>6}",
                "Query", "P99(ms)", "Avg(ms)", "Max(ms)", "Count"
            );
            println!("{}", "─".repeat(80));

            for s in samples.iter().take(20) {
                let query = s["query"].as_str().unwrap_or("-");
                let truncated = if query.len() > 45 {
                    format!("{}...", &query[..42])
                } else {
                    query.to_string()
                };
                println!(
                    "{:<45} {:>8.1} {:>8.1} {:>8.1} {:>6}",
                    truncated,
                    s["latency_p99_ms"].as_f64().unwrap_or(0.0),
                    s["latency_avg_ms"].as_f64().unwrap_or(0.0),
                    s["latency_max_ms"].as_f64().unwrap_or(0.0),
                    s["sample_count"].as_i64().unwrap_or(0),
                );
            }
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

async fn cmd_db_sql(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    query: Option<&str>,
    file: Option<&str>,
) {
    let sql = if let Some(q) = query {
        q.to_string()
    } else if let Some(f) = file {
        std::fs::read_to_string(f).unwrap_or_else(|e| {
            eprintln!("Failed to read file '{f}': {e}");
            process::exit(1);
        })
    } else if atty::is(atty::Stream::Stdin) {
        return repl::run(api, output, id).await;
    } else {
        use std::io::Read;
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf).unwrap_or_else(|e| {
            eprintln!("Failed to read stdin: {e}");
            process::exit(1);
        });
        buf
    };

    if sql.trim().is_empty() {
        eprintln!("No SQL provided. Use --query, --file, or pipe via stdin.");
        process::exit(1);
    }

    let data = execute_sql(api, id, &sql).await;
    repl::output::print_sql_result(&data, output, false, &None);
}

async fn cmd_db_users_list(api: &ApiClient, output: &OutputFormat, id: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let data = api
        .request(
            "GET",
            &format!("/customer/databases/{id}/users"),
            None,
            Some(&headers),
        )
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        OutputFormat::Csv => {
            let items = data.as_array().cloned().unwrap_or_default();
            print_csv(
                &items,
                &[
                    ("USERNAME", "name", 15),
                    ("SUPERUSER", "is_superuser", 9),
                    ("CAN_LOGIN", "can_login", 9),
                ],
            );
        }
        OutputFormat::Table => {
            let items = data.as_array().cloned().unwrap_or_default();
            print_table(
                &items,
                &[
                    ("USERNAME", "name", 15),
                    ("SUPERUSER", "is_superuser", 9),
                    ("CAN_LOGIN", "can_login", 9),
                ],
            );
        }
    }
}

async fn cmd_db_users_create(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    username: &str,
    password: &str,
) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let body = serde_json::json!({
        "username": username,
        "password": password,
    });

    let data = api
        .request(
            "POST",
            &format!("/customer/databases/{id}/users"),
            Some(&body),
            Some(&headers),
        )
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => {
            println!("User created successfully!");
            if let Some(user) = data.get("username").and_then(|v| v.as_str()) {
                println!("Username: {user}");
            }
        }
    }
}

async fn cmd_db_users_delete(api: &ApiClient, output: &OutputFormat, id: &str, username: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let data = api
        .request(
            "DELETE",
            &format!("/customer/databases/{id}/users/{username}"),
            None,
            Some(&headers),
        )
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => println!("User '{username}' deleted."),
    }
}

async fn cmd_db_seed(api: &ApiClient, output: &OutputFormat, id: &str, file: &str) {
    let content = std::fs::read_to_string(file).unwrap_or_else(|e| {
        eprintln!("Failed to read seed file '{file}': {e}");
        process::exit(1);
    });

    let data = execute_sql(api, id, &content).await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => {
            println!("Seed executed successfully.");
            if let Some(cmd) = data["command"].as_str() {
                println!("Last command: {cmd}");
            }
        }
    }
}

async fn cmd_db_branch_create(api: &ApiClient, output: &OutputFormat, id: &str, name: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let body = serde_json::json!({ "name": name });

    let data = api
        .request(
            "POST",
            &format!("/customer/databases/{id}/branch"),
            Some(&body),
            Some(&headers),
        )
        .await;

    match output {
        OutputFormat::Json => print_json(&data),
        _ => {
            println!("Branch '{}' created from database {}.\n", name, id);
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

async fn cmd_db_branch_list(api: &ApiClient, output: &OutputFormat, _id: &str) {
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
            println!("Branches are independent databases. Showing all databases.");
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
            println!("Branches are independent databases. Showing all databases.\n");
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

async fn cmd_db_branch_delete(api: &ApiClient, output: &OutputFormat, id: &str) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    if !matches!(output, OutputFormat::Json) {
        print!("Are you sure you want to delete branch {id}? This cannot be undone. [y/N] ");
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
        _ => println!("Branch {id} has been deleted."),
    }
}

async fn cmd_db_dump(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    ddl_only: bool,
    output_file: Option<&str>,
) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let body = serde_json::json!({ "ddl_only": ddl_only });
    let data = api
        .request(
            "POST",
            &format!("/customer/databases/{id}/dump"),
            Some(&body),
            Some(&headers),
        )
        .await;

    if matches!(output, OutputFormat::Json) {
        print_json(&data);
        return;
    }

    let sql = data["sql"].as_str().unwrap_or("");
    let object_count = data["object_count"].as_u64().unwrap_or(0);

    if let Some(path) = output_file {
        std::fs::write(path, sql).unwrap_or_else(|e| {
            eprintln!("Failed to write to '{path}': {e}");
            process::exit(1);
        });
        eprintln!("Exported {object_count} objects to {path}");
    } else {
        print!("{sql}");
        eprintln!("Exported {object_count} objects");
    }
}

async fn cmd_gen_types(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    lang: &TypeLang,
    schema_filter: &str,
) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let data = api
        .request(
            "GET",
            &format!("/customer/databases/{id}/schema"),
            None,
            Some(&headers),
        )
        .await;

    if matches!(output, OutputFormat::Json) {
        print_json(&data);
        return;
    }

    let tables = data["tables"].as_array().cloned().unwrap_or_default();
    let filtered: Vec<&Value> = tables
        .iter()
        .filter(|t| t["schema"].as_str().unwrap_or("") == schema_filter)
        .collect();

    let generated = match lang {
        TypeLang::Typescript => gen_typescript(&filtered),
        TypeLang::Python => gen_python(&filtered),
    };

    print!("{generated}");
}

fn to_pascal_case(s: &str) -> String {
    s.split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{upper}{}", chars.as_str().to_lowercase())
                }
                None => String::new(),
            }
        })
        .collect()
}

fn pg_type_to_ts(pg_type: &str) -> &'static str {
    match pg_type.to_lowercase().as_str() {
        "integer" | "bigint" | "smallint" | "serial" | "bigserial" | "int" | "int4" | "int8" => {
            "number"
        }
        "real" | "double precision" | "float4" | "float8" | "numeric" | "decimal" => "number",
        "text" | "varchar" | "char" | "character varying" => "string",
        "boolean" | "bool" => "boolean",
        "timestamp"
        | "timestamp without time zone"
        | "timestamp with time zone"
        | "timestamptz"
        | "date"
        | "time"
        | "time without time zone"
        | "time with time zone" => "string",
        "json" | "jsonb" => "Record<string, unknown>",
        "uuid" => "string",
        "bytea" => "string",
        "interval" => "string",
        _ => "unknown",
    }
}

fn pg_type_to_python(pg_type: &str) -> &'static str {
    match pg_type.to_lowercase().as_str() {
        "integer" | "bigint" | "smallint" | "serial" | "bigserial" | "int" | "int4" | "int8" => {
            "int"
        }
        "real" | "double precision" | "float4" | "float8" | "numeric" | "decimal" => "float",
        "text" | "varchar" | "char" | "character varying" => "str",
        "boolean" | "bool" => "bool",
        "timestamp"
        | "timestamp without time zone"
        | "timestamp with time zone"
        | "timestamptz"
        | "date"
        | "time"
        | "time without time zone"
        | "time with time zone" => "str",
        "json" | "jsonb" => "dict",
        "uuid" => "str",
        "bytea" => "bytes",
        "interval" => "str",
        _ => "Any",
    }
}

fn pg_type_to_ts_with_array(pg_type: &str) -> String {
    let lower = pg_type.to_lowercase();
    if let Some(inner) = lower.strip_suffix("[]") {
        let base = pg_type_to_ts(inner);
        return format!("{base}[]");
    }
    pg_type_to_ts(pg_type).to_string()
}

fn pg_type_to_python_with_array(pg_type: &str) -> String {
    let lower = pg_type.to_lowercase();
    if let Some(inner) = lower.strip_suffix("[]") {
        let base = pg_type_to_python(inner);
        return format!("list[{base}]");
    }
    pg_type_to_python(pg_type).to_string()
}

fn gen_typescript(tables: &[&Value]) -> String {
    let mut out = String::from("// Generated by db9 gen types\n\n");

    for table in tables {
        let table_name = table["name"].as_str().unwrap_or("unknown");
        let iface_name = to_pascal_case(table_name);
        out.push_str(&format!("export interface {iface_name} {{\n"));

        if let Some(columns) = table["columns"].as_array() {
            for col in columns {
                let col_name = col["name"].as_str().unwrap_or("unknown");
                let col_type = col["type"].as_str().unwrap_or("text");
                let nullable = col["nullable"].as_bool().unwrap_or(false);
                let ts_type = pg_type_to_ts_with_array(col_type);
                if nullable {
                    out.push_str(&format!("  {col_name}: {ts_type} | null;\n"));
                } else {
                    out.push_str(&format!("  {col_name}: {ts_type};\n"));
                }
            }
        }

        out.push_str("}\n\n");
    }

    out
}

fn gen_python(tables: &[&Value]) -> String {
    let mut out = String::from(
        "# Generated by db9 gen types\n\nfrom typing import TypedDict, Optional, Any\n\n",
    );

    for table in tables {
        let table_name = table["name"].as_str().unwrap_or("unknown");
        let class_name = to_pascal_case(table_name);
        out.push_str(&format!("class {class_name}(TypedDict):\n"));

        if let Some(columns) = table["columns"].as_array() {
            if columns.is_empty() {
                out.push_str("    pass\n");
            } else {
                for col in columns {
                    let col_name = col["name"].as_str().unwrap_or("unknown");
                    let col_type = col["type"].as_str().unwrap_or("text");
                    let nullable = col["nullable"].as_bool().unwrap_or(false);
                    let py_type = pg_type_to_python_with_array(col_type);
                    if nullable {
                        out.push_str(&format!("    {col_name}: Optional[{py_type}]\n"));
                    } else {
                        out.push_str(&format!("    {col_name}: {py_type}\n"));
                    }
                }
            }
        } else {
            out.push_str("    pass\n");
        }

        out.push('\n');
    }

    out
}

struct LocalMigration {
    filename: String,
    name: String,
    timestamp: String,
    path: std::path::PathBuf,
}

fn scan_migration_files(dir: &str) -> Vec<LocalMigration> {
    let dir_path = std::path::Path::new(dir);
    if !dir_path.exists() {
        return Vec::new();
    }

    let entries = std::fs::read_dir(dir_path).unwrap_or_else(|e| {
        eprintln!("Failed to read directory '{dir}': {e}");
        process::exit(1);
    });

    let mut migrations: Vec<LocalMigration> = entries
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let filename = entry.file_name().to_string_lossy().to_string();
            if !filename.ends_with(".sql") {
                return None;
            }
            let stem = filename.trim_end_matches(".sql");
            let underscore_pos = stem.find('_')?;
            let timestamp = stem[..underscore_pos].to_string();
            if timestamp.len() != 14 || !timestamp.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            Some(LocalMigration {
                filename: filename.clone(),
                name: stem.to_string(),
                timestamp,
                path: entry.path(),
            })
        })
        .collect();

    migrations.sort_by(|a, b| a.filename.cmp(&b.filename));
    migrations
}

fn format_migration_timestamp(ts: &str) -> String {
    if ts.len() == 14 {
        format!(
            "{}-{}-{} {}:{}:{}",
            &ts[0..4],
            &ts[4..6],
            &ts[6..8],
            &ts[8..10],
            &ts[10..12],
            &ts[12..14]
        )
    } else {
        ts.to_string()
    }
}

fn cmd_migration_new(name: &str, dir: &str, output: &OutputFormat) {
    std::fs::create_dir_all(dir).unwrap_or_else(|e| {
        eprintln!("Failed to create directory '{dir}': {e}");
        process::exit(1);
    });

    let now = chrono::Utc::now();
    let timestamp = now.format("%Y%m%d%H%M%S").to_string();
    let safe_name: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let filename = format!("{timestamp}_{safe_name}.sql");
    let filepath = std::path::Path::new(dir).join(&filename);

    let header = format!(
        "-- Migration: {safe_name}\n-- Created: {}\n\n",
        now.format("%Y-%m-%d %H:%M:%S UTC")
    );
    std::fs::write(&filepath, &header).unwrap_or_else(|e| {
        eprintln!("Failed to create migration file: {e}");
        process::exit(1);
    });

    match output {
        OutputFormat::Json => {
            print_json(&serde_json::json!({
                "name": safe_name,
                "filename": filename,
                "path": filepath.to_string_lossy(),
            }));
        }
        _ => {
            println!("Created migration: {}", filepath.display());
        }
    }
}

fn cmd_migration_list(dir: &str, output: &OutputFormat) {
    let migrations = scan_migration_files(dir);

    if migrations.is_empty() {
        match output {
            OutputFormat::Json => print_json(&serde_json::json!([])),
            _ => println!("No migration files found in '{dir}'."),
        }
        return;
    }

    match output {
        OutputFormat::Json => {
            let items: Vec<Value> = migrations
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "created": format_migration_timestamp(&m.timestamp),
                        "filename": m.filename,
                    })
                })
                .collect();
            print_json(&Value::Array(items));
        }
        OutputFormat::Csv => {
            let items: Vec<Value> = migrations
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "created": format_migration_timestamp(&m.timestamp),
                    })
                })
                .collect();
            print_csv(&items, &[("NAME", "name", 40), ("CREATED", "created", 20)]);
        }
        OutputFormat::Table => {
            let items: Vec<Value> = migrations
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "name": m.name,
                        "created": format_migration_timestamp(&m.timestamp),
                    })
                })
                .collect();
            print_table(&items, &[("NAME", "name", 40), ("CREATED", "created", 20)]);
        }
    }
}

async fn cmd_migration_up(api: &ApiClient, id: &str, dir: &str, output: &OutputFormat) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let local = scan_migration_files(dir);
    if local.is_empty() {
        match output {
            OutputFormat::Json => {
                print_json(&serde_json::json!({ "applied": 0, "up_to_date": 0 }));
            }
            _ => println!("No migration files found in '{dir}'."),
        }
        return;
    }

    let remote_data = api
        .request(
            "GET",
            &format!("/customer/databases/{id}/migrations"),
            None,
            Some(&headers),
        )
        .await;

    let applied_names: std::collections::HashSet<String> = remote_data
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|m| m["name"].as_str().map(|s| s.to_string()))
        .collect();

    let mut applied_count = 0u32;
    let mut up_to_date_count = 0u32;

    for m in &local {
        if applied_names.contains(&m.name) {
            up_to_date_count += 1;
            if !matches!(output, OutputFormat::Json) {
                println!("Applying: {} ... ALREADY APPLIED", m.filename);
            }
            continue;
        }

        let content = std::fs::read_to_string(&m.path).unwrap_or_else(|e| {
            eprintln!("Failed to read '{}': {e}", m.path.display());
            process::exit(1);
        });

        use sha2::{Digest, Sha256};
        let hash = Sha256::digest(content.as_bytes());
        let checksum: String = hash.iter().map(|b| format!("{b:02x}")).collect();

        let body = serde_json::json!({
            "name": m.name,
            "sql": content,
            "checksum": checksum,
        });

        let result = api
            .request(
                "POST",
                &format!("/customer/databases/{id}/migrations"),
                Some(&body),
                Some(&headers),
            )
            .await;

        let status = result["status"].as_str().unwrap_or("unknown");
        if status == "already_applied" {
            up_to_date_count += 1;
            if !matches!(output, OutputFormat::Json) {
                println!("Applying: {} ... ALREADY APPLIED", m.filename);
            }
        } else {
            applied_count += 1;
            if !matches!(output, OutputFormat::Json) {
                println!("Applying: {} ... OK", m.filename);
            }
        }
    }

    match output {
        OutputFormat::Json => {
            print_json(&serde_json::json!({
                "applied": applied_count,
                "up_to_date": up_to_date_count,
            }));
        }
        _ => {
            println!(
                "\nApplied {applied_count} migration(s). {up_to_date_count} already up-to-date."
            );
        }
    }
}

async fn cmd_migration_status(api: &ApiClient, id: &str, dir: &str, output: &OutputFormat) {
    let token = require_token();
    let headers = make_auth_headers(&token);

    let local = scan_migration_files(dir);

    let remote_data = api
        .request(
            "GET",
            &format!("/customer/databases/{id}/migrations"),
            None,
            Some(&headers),
        )
        .await;

    let applied_map: HashMap<String, String> = remote_data
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|m| {
            let name = m["name"].as_str()?.to_string();
            let applied_at = m["applied_at"].as_str().unwrap_or("-").to_string();
            Some((name, applied_at))
        })
        .collect();

    if local.is_empty() && applied_map.is_empty() {
        match output {
            OutputFormat::Json => print_json(&serde_json::json!([])),
            _ => println!("No migrations found."),
        }
        return;
    }

    match output {
        OutputFormat::Json => {
            let items: Vec<Value> = local
                .iter()
                .map(|m| {
                    let (status, applied_at) = if let Some(at) = applied_map.get(&m.name) {
                        ("applied", at.as_str())
                    } else {
                        ("pending", "")
                    };
                    serde_json::json!({
                        "name": m.name,
                        "status": status,
                        "applied_at": applied_at,
                    })
                })
                .collect();
            print_json(&Value::Array(items));
        }
        _ => {
            println!("{:<40} {:<10} APPLIED AT", "NAME", "STATUS");
            println!("{}", "\u{2500}".repeat(72));
            for m in &local {
                if let Some(at) = applied_map.get(&m.name) {
                    println!("{:<40} \u{2713} applied  {at}", m.name);
                } else {
                    println!("{:<40} \u{25CB} pending", m.name);
                }
            }
        }
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

    #[test]
    fn test_to_pascal_case() {
        assert_eq!(to_pascal_case("user_accounts"), "UserAccounts");
        assert_eq!(to_pascal_case("users"), "Users");
        assert_eq!(to_pascal_case("order_line_items"), "OrderLineItems");
        assert_eq!(to_pascal_case("a"), "A");
        assert_eq!(to_pascal_case(""), "");
    }

    #[test]
    fn test_pg_type_to_ts() {
        assert_eq!(pg_type_to_ts("integer"), "number");
        assert_eq!(pg_type_to_ts("bigint"), "number");
        assert_eq!(pg_type_to_ts("text"), "string");
        assert_eq!(pg_type_to_ts("boolean"), "boolean");
        assert_eq!(pg_type_to_ts("jsonb"), "Record<string, unknown>");
        assert_eq!(pg_type_to_ts("uuid"), "string");
        assert_eq!(pg_type_to_ts("bytea"), "string");
        assert_eq!(pg_type_to_ts("interval"), "string");
        assert_eq!(pg_type_to_ts("timestamp"), "string");
        assert_eq!(pg_type_to_ts("custom_type"), "unknown");
    }

    #[test]
    fn test_pg_type_to_python() {
        assert_eq!(pg_type_to_python("integer"), "int");
        assert_eq!(pg_type_to_python("double precision"), "float");
        assert_eq!(pg_type_to_python("text"), "str");
        assert_eq!(pg_type_to_python("boolean"), "bool");
        assert_eq!(pg_type_to_python("jsonb"), "dict");
        assert_eq!(pg_type_to_python("bytea"), "bytes");
        assert_eq!(pg_type_to_python("custom_type"), "Any");
    }

    #[test]
    fn test_pg_type_to_ts_with_array() {
        assert_eq!(pg_type_to_ts_with_array("text[]"), "string[]");
        assert_eq!(pg_type_to_ts_with_array("integer[]"), "number[]");
        assert_eq!(pg_type_to_ts_with_array("text"), "string");
    }

    #[test]
    fn test_pg_type_to_python_with_array() {
        assert_eq!(pg_type_to_python_with_array("text[]"), "list[str]");
        assert_eq!(pg_type_to_python_with_array("integer[]"), "list[int]");
        assert_eq!(pg_type_to_python_with_array("text"), "str");
    }

    #[test]
    fn test_gen_typescript() {
        let table = serde_json::json!({
            "name": "user_accounts",
            "schema": "public",
            "columns": [
                { "name": "id", "type": "integer", "nullable": false },
                { "name": "email", "type": "text", "nullable": false },
                { "name": "bio", "type": "text", "nullable": true },
                { "name": "tags", "type": "text[]", "nullable": true },
            ]
        });
        let tables = vec![&table];
        let result = gen_typescript(&tables);
        assert!(result.contains("// Generated by db9 gen types"));
        assert!(result.contains("export interface UserAccounts {"));
        assert!(result.contains("  id: number;"));
        assert!(result.contains("  email: string;"));
        assert!(result.contains("  bio: string | null;"));
        assert!(result.contains("  tags: string[] | null;"));
    }

    #[test]
    fn test_gen_python() {
        let table = serde_json::json!({
            "name": "user_accounts",
            "schema": "public",
            "columns": [
                { "name": "id", "type": "integer", "nullable": false },
                { "name": "email", "type": "text", "nullable": false },
                { "name": "bio", "type": "text", "nullable": true },
                { "name": "metadata", "type": "jsonb", "nullable": true },
            ]
        });
        let tables = vec![&table];
        let result = gen_python(&tables);
        assert!(result.contains("# Generated by db9 gen types"));
        assert!(result.contains("from typing import TypedDict, Optional, Any"));
        assert!(result.contains("class UserAccounts(TypedDict):"));
        assert!(result.contains("    id: int"));
        assert!(result.contains("    email: str"));
        assert!(result.contains("    bio: Optional[str]"));
        assert!(result.contains("    metadata: Optional[dict]"));
    }

    #[test]
    fn test_save_anonymous_flag_set_true() {
        let temp_dir = std::env::temp_dir().join(format!("db9-anon-test-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cred_path = temp_dir.join("credentials");

        std::fs::write(&cred_path, "token = \"mytoken\"\n").unwrap();

        let existing = std::fs::read_to_string(&cred_path).unwrap_or_default();
        let mut parsed: toml::Table = existing.parse().unwrap();
        parsed.insert("is_anonymous".to_string(), toml::Value::Boolean(true));
        let content = toml::to_string(&parsed).unwrap();
        std::fs::write(&cred_path, &content).unwrap();

        let read_back = std::fs::read_to_string(&cred_path).unwrap();
        let re_parsed: toml::Table = read_back.parse().unwrap();
        assert_eq!(
            re_parsed.get("token").and_then(|v| v.as_str()),
            Some("mytoken")
        );
        assert_eq!(
            re_parsed.get("is_anonymous").and_then(|v| v.as_bool()),
            Some(true)
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_save_anonymous_flag_set_false_removes_key() {
        let temp_dir = std::env::temp_dir().join(format!("db9-anon-rm-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cred_path = temp_dir.join("credentials");

        std::fs::write(&cred_path, "token = \"mytoken\"\nis_anonymous = true\n").unwrap();

        let existing = std::fs::read_to_string(&cred_path).unwrap_or_default();
        let mut parsed: toml::Table = existing.parse().unwrap();
        parsed.remove("is_anonymous");
        let content = toml::to_string(&parsed).unwrap();
        std::fs::write(&cred_path, &content).unwrap();

        let read_back = std::fs::read_to_string(&cred_path).unwrap();
        let re_parsed: toml::Table = read_back.parse().unwrap();
        assert_eq!(
            re_parsed.get("token").and_then(|v| v.as_str()),
            Some("mytoken")
        );
        assert!(re_parsed.get("is_anonymous").is_none());

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_save_token_preserves_existing_fields() {
        let temp_dir = std::env::temp_dir().join(format!("db9-preserve-{}", std::process::id()));
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cred_path = temp_dir.join("credentials");

        std::fs::write(&cred_path, "is_anonymous = true\n").unwrap();

        let existing = std::fs::read_to_string(&cred_path).unwrap_or_default();
        let mut parsed: toml::Table = existing.parse().unwrap();
        parsed.insert(
            "token".to_string(),
            toml::Value::String("new-token".to_string()),
        );
        let content = toml::to_string(&parsed).unwrap();
        std::fs::write(&cred_path, &content).unwrap();

        let read_back = std::fs::read_to_string(&cred_path).unwrap();
        let re_parsed: toml::Table = read_back.parse().unwrap();
        assert_eq!(
            re_parsed.get("token").and_then(|v| v.as_str()),
            Some("new-token")
        );
        assert_eq!(
            re_parsed.get("is_anonymous").and_then(|v| v.as_bool()),
            Some(true)
        );

        std::fs::remove_dir_all(&temp_dir).ok();
    }

    #[test]
    fn test_toml_credentials_roundtrip_with_anonymous() {
        let content = "token = \"abc123\"\nis_anonymous = true\n";
        let parsed: toml::Table = content.parse().unwrap();
        assert_eq!(parsed.get("token").and_then(|v| v.as_str()), Some("abc123"));
        assert_eq!(
            parsed.get("is_anonymous").and_then(|v| v.as_bool()),
            Some(true)
        );
    }

    #[test]
    fn test_toml_credentials_without_anonymous() {
        let content = "token = \"abc123\"\n";
        let parsed: toml::Table = content.parse().unwrap();
        assert_eq!(parsed.get("token").and_then(|v| v.as_str()), Some("abc123"));
        assert!(parsed.get("is_anonymous").is_none());
    }
}
