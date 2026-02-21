use std::collections::HashMap;
use std::io::{self, Write};
use std::process;

use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use pgtikv_admin::cli_common::{
    format_time, format_val, print_csv, print_json, print_table, ApiClient,
};
use serde_json::Value;

mod repl;
use repl::ExpandedMode;

const DEFAULT_API_URL: &str = "https://db9.shared.aws.tidbcloud.com/api";

// ── Output format enum ──────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, ValueEnum)]
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
    Login {
        /// Use API key directly instead of email/password
        #[arg(long)]
        api_key: Option<String>,
    },
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
    /// Filesystem operations (fs9/sh9)
    Fs {
        #[command(subcommand)]
        action: FsAction,
    },
    /// Generate shell completion scripts
    Completion {
        /// Shell to generate for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[derive(Subcommand)]
enum FsAction {
    /// Launch interactive filesystem shell for a database
    Sh {
        /// Database ID (omit to auto-select or choose interactively)
        id: Option<String>,
        /// Execute a shell command and exit
        #[arg(short = 'c')]
        command: Option<String>,
    },
    /// Show filesystem audit event log
    Events {
        /// Database ID (omit to auto-select or choose interactively)
        id: Option<String>,
        /// Maximum number of events to show
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        /// Skip first N events
        #[arg(short = 'o', long, default_value_t = 0)]
        offset: usize,
        /// Filter by path prefix
        #[arg(short, long)]
        path: Option<String>,
        /// Filter by event type (create, delete, mkdir, rename, truncate, chmod, upload)
        #[arg(short = 't', long = "type")]
        event_type: Option<String>,
    },
    /// Copy files between local filesystem and fs9
    Cp {
        /// Database ID (omit to auto-select or choose interactively)
        #[arg(long)]
        id: Option<String>,
        /// Source path (local path or fs9:/remote/path)
        source: String,
        /// Destination path (local path or fs9:/remote/path)
        destination: String,
        /// Copy directories recursively
        #[arg(short)]
        r: bool,
        /// Verbose output — print each file as copied
        #[arg(short)]
        v: bool,
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
        /// Admin password (default: random)
        #[arg(long)]
        password: Option<String>,
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
        /// Database ID (omit to auto-select or choose interactively)
        id: Option<String>,
        /// SQL query string
        #[arg(long, short)]
        query: Option<String>,
        /// Path to SQL file
        #[arg(long, short)]
        file: Option<String>,
        /// Use direct pgwire connection instead of HTTP API
        #[arg(long, short = 'D')]
        direct: bool,
        /// Connection string for direct mode (overrides API-provided DSN)
        #[arg(long)]
        dsn: Option<String>,
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
    /// Manage cron jobs (requires pg_cron extension)
    Cron {
        /// Database ID
        id: String,
        #[command(subcommand)]
        action: CronAction,
    },
}

#[derive(Subcommand)]
enum CronAction {
    /// List scheduled cron jobs
    List,
    /// Create a new cron job
    Create {
        /// Cron schedule expression (e.g., '*/5 * * * *')
        schedule: String,
        /// SQL command to execute (provide this or --file, not both)
        command: Option<String>,
        /// Read SQL command from a .sql file
        #[arg(short, long, conflicts_with = "command")]
        file: Option<String>,
        /// Optional job name (enables upsert semantics)
        #[arg(long)]
        name: Option<String>,
    },
    /// Delete a cron job by ID or name
    Delete {
        /// Job ID (number) or job name (text)
        job: String,
    },
    /// Show cron job execution history
    History {
        /// Filter by job ID or name
        #[arg(long)]
        job: Option<String>,
        /// Maximum number of entries to show
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Enable a cron job
    Enable {
        /// Job ID (number) or job name (text)
        job: String,
    },
    /// Disable a cron job
    Disable {
        /// Job ID (number) or job name (text)
        job: String,
    },
    /// Show cron job execution status
    Status {
        /// Optional: job ID (number) or job name (text) for single-job detail
        job: Option<String>,
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
        Commands::Login { ref api_key } => {
            cmd_login(&api, &cli.effective_output(), api_key.clone()).await
        }
        Commands::Claim => cmd_claim(&api, &cli.effective_output()).await,
        Commands::Logout => cmd_logout(),
        Commands::Init => cmd_init(&api, &cli.effective_output()).await,
        Commands::Fs { ref action } => match action {
            FsAction::Sh {
                ref id,
                ref command,
            } => cmd_sh(&api, &cli.api_url, id.as_deref(), command.as_deref()).await,
            FsAction::Events {
                ref id,
                limit,
                offset,
                ref path,
                ref event_type,
            } => {
                cmd_fs_events(
                    &api,
                    &cli.api_url,
                    &cli.effective_output(),
                    id.as_deref(),
                    *limit,
                    *offset,
                    path.as_deref(),
                    event_type.as_deref(),
                )
                .await
            }
            FsAction::Cp {
                ref id,
                ref source,
                ref destination,
                r,
                v,
            } => {
                cmd_fs_cp(
                    &api,
                    &cli.api_url,
                    id.as_deref(),
                    source,
                    destination,
                    *r,
                    *v,
                )
                .await
            }
        },
        Commands::Completion { shell } => cmd_completion(shell),
        Commands::Db { ref action } => match action {
            DbAction::Create {
                name,
                region,
                password,
            } => {
                cmd_db_create(
                    &api,
                    &cli.effective_output(),
                    name,
                    region.as_deref(),
                    password.as_deref(),
                )
                .await
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
            DbAction::Sql {
                id,
                query,
                file,
                direct,
                dsn,
            } => {
                cmd_db_sql(
                    &api,
                    &cli.effective_output(),
                    id.as_deref(),
                    query.as_deref(),
                    file.as_deref(),
                    *direct,
                    dsn.as_deref(),
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
            DbAction::Cron { id, action } => match action {
                CronAction::List => cmd_cron_list(&api, &cli.effective_output(), id).await,
                CronAction::Create {
                    schedule,
                    command,
                    file,
                    name,
                } => {
                    let sql_command = resolve_cron_command(command.as_deref(), file.as_deref());
                    cmd_cron_create(
                        &api,
                        &cli.effective_output(),
                        id,
                        schedule,
                        &sql_command,
                        name.as_deref(),
                    )
                    .await
                }
                CronAction::Delete { job } => {
                    cmd_cron_delete(&api, &cli.effective_output(), id, job).await
                }
                CronAction::History { job, limit } => {
                    cmd_cron_history(&api, &cli.effective_output(), id, job.as_deref(), *limit)
                        .await
                }
                CronAction::Enable { job } => {
                    cmd_cron_enable(&api, &cli.effective_output(), id, job).await
                }
                CronAction::Disable { job } => {
                    cmd_cron_disable(&api, &cli.effective_output(), id, job).await
                }
                CronAction::Status { job } => {
                    cmd_cron_status(&api, &cli.effective_output(), id, job.as_deref()).await
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

async fn resolve_db_id(
    api: &ApiClient,
    id: Option<&str>,
    headers: &HashMap<String, String>,
) -> String {
    if let Some(id) = id {
        return id.to_string();
    }

    let data = api
        .request("GET", "/customer/databases", None, Some(headers))
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
}

fn derive_fs9_url(api_url: &str, db_id: &str) -> String {
    let base_url = api_url.strip_suffix("/api").unwrap_or(api_url);
    format!("{base_url}/fs9/{db_id}")
}

fn is_fs9_path(path: &str) -> bool {
    path.starts_with("fs9:")
}

fn parse_fs9_path(path: &str) -> String {
    let stripped = if path.starts_with("fs9://") {
        &path[6..]
    } else if path.starts_with("fs9:") {
        &path[4..]
    } else {
        return path.to_string();
    };
    if stripped.is_empty() {
        "/".to_string()
    } else if stripped.starts_with('/') {
        stripped.to_string()
    } else {
        format!("/{stripped}")
    }
}

const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

async fn cmd_fs_cp(
    api: &ApiClient,
    api_url: &str,
    id: Option<&str>,
    source: &str,
    destination: &str,
    recursive: bool,
    verbose: bool,
) {
    let src_is_fs9 = is_fs9_path(source);
    let dst_is_fs9 = is_fs9_path(destination);

    if src_is_fs9 && dst_is_fs9 {
        eprintln!("error: remote-to-remote copy not supported");
        process::exit(1);
    }
    if !src_is_fs9 && !dst_is_fs9 {
        eprintln!("error: one of source or destination must be an fs9: path");
        process::exit(1);
    }

    // Validate local source path eagerly before any network calls
    if !src_is_fs9 {
        let p = std::path::Path::new(source);
        if !p.exists() {
            eprintln!("error: '{}' does not exist", source);
            process::exit(1);
        }
        if p.is_dir() && !recursive {
            eprintln!("error: '{}' is a directory (not copied); use -r", source);
            process::exit(1);
        }
    }

    let token = require_token();
    let headers = make_auth_headers(&token);
    let db_id = resolve_db_id(api, id, &headers).await;
    let fs9_url = derive_fs9_url(api_url, &db_id);
    let client = reqwest::Client::new();

    if dst_is_fs9 {
        let remote_dest = parse_fs9_path(destination);
        upload_path(
            &client,
            &fs9_url,
            &token,
            source,
            &remote_dest,
            recursive,
            verbose,
        )
        .await;
    } else {
        let remote_src = parse_fs9_path(source);
        download_path(
            &client,
            &fs9_url,
            &token,
            &remote_src,
            destination,
            recursive,
            verbose,
        )
        .await;
    }
}

async fn upload_path(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    local_path: &str,
    remote_path: &str,
    recursive: bool,
    verbose: bool,
) {
    let path = std::path::Path::new(local_path);

    if !path.exists() {
        eprintln!("error: '{}' does not exist", local_path);
        process::exit(1);
    }

    if path.is_dir() && !recursive {
        eprintln!(
            "error: '{}' is a directory (not copied); use -r",
            local_path
        );
        process::exit(1);
    }

    let final_remote = resolve_upload_dest(client, fs9_url, token, local_path, remote_path).await;

    if path.is_file() {
        let skipped =
            upload_single_file(client, fs9_url, token, path, &final_remote, verbose).await;
        if skipped {
            process::exit(1);
        }
    } else if path.is_dir() {
        let mut copied = 0u64;
        let mut skipped = 0u64;
        upload_dir_recursive(
            client,
            fs9_url,
            token,
            path,
            &final_remote,
            verbose,
            &mut copied,
            &mut skipped,
        )
        .await;
        if verbose || skipped > 0 {
            eprintln!("{copied} file(s) copied, {skipped} skipped");
        }
        if skipped > 0 {
            process::exit(1);
        }
    }
}

async fn resolve_upload_dest(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    local_path: &str,
    remote_path: &str,
) -> String {
    // Trailing `/` signals directory intent — always append basename,
    // regardless of whether the directory exists yet.
    if remote_path.ends_with('/') {
        let basename = std::path::Path::new(local_path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let dest = remote_path.trim_end_matches('/');
        return format!("{dest}/{basename}");
    }

    match fs9_stat(client, fs9_url, token, remote_path).await {
        Ok((true, _)) => {
            let basename = std::path::Path::new(local_path)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let dest = remote_path.trim_end_matches('/');
            format!("{dest}/{basename}")
        }
        _ => remote_path.to_string(),
    }
}

async fn upload_single_file(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    local_path: &std::path::Path,
    remote_path: &str,
    verbose: bool,
) -> bool {
    let metadata = std::fs::metadata(local_path).unwrap_or_else(|e| {
        eprintln!("error: cannot read '{}': {e}", local_path.display());
        process::exit(1);
    });

    if metadata.len() > MAX_FILE_SIZE {
        eprintln!(
            "warning: '{}' exceeds 10MB limit ({} bytes), skipping",
            local_path.display(),
            metadata.len()
        );
        return true;
    }

    let content = std::fs::read(local_path).unwrap_or_else(|e| {
        eprintln!("error: cannot read '{}': {e}", local_path.display());
        process::exit(1);
    });

    if let Some(parent) = std::path::Path::new(remote_path).parent() {
        let parent_str = parent.to_string_lossy();
        if parent_str != "/" && !parent_str.is_empty() {
            if let Err(e) = fs9_mkdir(client, fs9_url, token, &parent_str).await {
                if verbose {
                    eprintln!("warning: mkdir '{}': {e}", parent_str);
                }
            }
        }
    }

    match fs9_upload(client, fs9_url, token, remote_path, content).await {
        Ok(_) => {
            if verbose {
                println!("'{}' -> 'fs9:{}'", local_path.display(), remote_path);
            }
            false
        }
        Err(e) => {
            eprintln!(
                "error: upload '{}' -> '{}': {e}",
                local_path.display(),
                remote_path
            );
            process::exit(1);
        }
    }
}

async fn upload_dir_recursive(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    local_dir: &std::path::Path,
    remote_dir: &str,
    verbose: bool,
    copied: &mut u64,
    skipped: &mut u64,
) {
    if let Err(e) = fs9_mkdir(client, fs9_url, token, remote_dir).await {
        if verbose {
            eprintln!("warning: mkdir '{}': {e}", remote_dir);
        }
    }

    let entries = std::fs::read_dir(local_dir).unwrap_or_else(|e| {
        eprintln!(
            "error: cannot read directory '{}': {e}",
            local_dir.display()
        );
        process::exit(1);
    });

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!("warning: read entry: {e}");
                *skipped += 1;
                continue;
            }
        };

        let local_child = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let remote_child = format!("{}/{}", remote_dir.trim_end_matches('/'), name);

        if local_child.is_dir() {
            Box::pin(upload_dir_recursive(
                client,
                fs9_url,
                token,
                &local_child,
                &remote_child,
                verbose,
                copied,
                skipped,
            ))
            .await;
        } else if local_child.is_file() {
            let metadata = match std::fs::metadata(&local_child) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("warning: cannot stat '{}': {e}", local_child.display());
                    *skipped += 1;
                    continue;
                }
            };

            if metadata.len() > MAX_FILE_SIZE {
                eprintln!(
                    "warning: '{}' exceeds 10MB limit ({} bytes), skipping",
                    local_child.display(),
                    metadata.len()
                );
                *skipped += 1;
                continue;
            }

            let content = match std::fs::read(&local_child) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("warning: cannot read '{}': {e}", local_child.display());
                    *skipped += 1;
                    continue;
                }
            };

            match fs9_upload(client, fs9_url, token, &remote_child, content).await {
                Ok(_) => {
                    if verbose {
                        println!("'{}' -> 'fs9:{}'", local_child.display(), remote_child);
                    }
                    *copied += 1;
                }
                Err(e) => {
                    eprintln!("warning: upload '{}': {e}", local_child.display());
                    *skipped += 1;
                }
            }
        }
    }
}

async fn download_path(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    remote_path: &str,
    local_path: &str,
    recursive: bool,
    verbose: bool,
) {
    let (is_dir, _size) = match fs9_stat(client, fs9_url, token, remote_path).await {
        Ok(stat) => stat,
        Err(e) => {
            eprintln!("error: cannot stat 'fs9:{}': {e}", remote_path);
            process::exit(1);
        }
    };

    if is_dir && !recursive {
        eprintln!(
            "error: 'fs9:{}' is a directory (not copied); use -r",
            remote_path
        );
        process::exit(1);
    }

    let dest = std::path::Path::new(local_path);
    let final_local = if dest.is_dir() {
        let basename = std::path::Path::new(remote_path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        dest.join(basename)
    } else {
        dest.to_path_buf()
    };

    if !is_dir {
        download_single_file(client, fs9_url, token, remote_path, &final_local, verbose).await;
    } else {
        let mut copied = 0u64;
        let mut skipped = 0u64;
        download_dir_recursive(
            client,
            fs9_url,
            token,
            remote_path,
            &final_local,
            verbose,
            &mut copied,
            &mut skipped,
        )
        .await;
        if verbose || skipped > 0 {
            eprintln!("{copied} file(s) copied, {skipped} skipped");
        }
        if skipped > 0 {
            process::exit(1);
        }
    }
}

async fn download_single_file(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    remote_path: &str,
    local_path: &std::path::Path,
    verbose: bool,
) {
    let content = match fs9_download(client, fs9_url, token, remote_path).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: download 'fs9:{}': {e}", remote_path);
            process::exit(1);
        }
    };

    if let Some(parent) = local_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                eprintln!("error: cannot create directory '{}': {e}", parent.display());
                process::exit(1);
            });
        }
    }

    std::fs::write(local_path, &content).unwrap_or_else(|e| {
        eprintln!("error: cannot write '{}': {e}", local_path.display());
        process::exit(1);
    });

    if verbose {
        println!("'fs9:{}' -> '{}'", remote_path, local_path.display());
    }
}

async fn download_dir_recursive(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    remote_dir: &str,
    local_dir: &std::path::Path,
    verbose: bool,
    copied: &mut u64,
    skipped: &mut u64,
) {
    if let Err(e) = std::fs::create_dir_all(local_dir) {
        eprintln!(
            "error: cannot create directory '{}': {e}",
            local_dir.display()
        );
        process::exit(1);
    }

    let entries = match fs9_readdir(client, fs9_url, token, remote_dir).await {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: readdir 'fs9:{}': {e}", remote_dir);
            process::exit(1);
        }
    };

    for (entry_path, is_dir, _size) in entries {
        let name = std::path::Path::new(&entry_path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let local_child = local_dir.join(&name);
        let remote_child = format!("{}/{}", remote_dir.trim_end_matches('/'), name);

        if is_dir {
            Box::pin(download_dir_recursive(
                client,
                fs9_url,
                token,
                &remote_child,
                &local_child,
                verbose,
                copied,
                skipped,
            ))
            .await;
        } else {
            match fs9_download(client, fs9_url, token, &remote_child).await {
                Ok(content) => match std::fs::write(&local_child, &content) {
                    Ok(_) => {
                        if verbose {
                            println!("'fs9:{}' -> '{}'", remote_child, local_child.display());
                        }
                        *copied += 1;
                    }
                    Err(e) => {
                        eprintln!("warning: write '{}': {e}", local_child.display());
                        *skipped += 1;
                    }
                },
                Err(e) => {
                    eprintln!("warning: download '{}': {e}", remote_child);
                    *skipped += 1;
                }
            }
        }
    }
}

async fn fs9_upload(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    remote_path: &str,
    content: Vec<u8>,
) -> Result<usize, String> {
    let len = content.len();
    let url = format!("{fs9_url}/api/v1/upload");
    let resp = client
        .put(&url)
        .query(&[("path", remote_path)])
        .header("Authorization", format!("Bearer {token}"))
        .header("Content-Type", "application/octet-stream")
        .body(content)
        .send()
        .await
        .map_err(|e| format!("fs9 upload failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("fs9 upload error ({status}): {body}"));
    }
    Ok(len)
}

async fn fs9_download(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    remote_path: &str,
) -> Result<Vec<u8>, String> {
    let url = format!("{fs9_url}/api/v1/download");
    let resp = client
        .get(&url)
        .query(&[("path", remote_path)])
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("fs9 download failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("fs9 download error ({status}): {body}"));
    }
    resp.bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| format!("fs9 download read error: {e}"))
}

async fn fs9_mkdir(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    remote_path: &str,
) -> Result<(), String> {
    let url = format!("{fs9_url}/api/v1/mkdir");
    let resp = client
        .post(&url)
        .query(&[("path", remote_path), ("recursive", "true")])
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("fs9 mkdir failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("fs9 mkdir error ({status}): {body}"));
    }
    Ok(())
}

async fn fs9_stat(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    remote_path: &str,
) -> Result<(bool, u64), String> {
    let url = format!("{fs9_url}/api/v1/stat");
    let resp = client
        .get(&url)
        .query(&[("path", remote_path)])
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("fs9 stat failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("fs9 stat error ({status}): {body}"));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("fs9 stat parse error: {e}"))?;
    let is_dir = json
        .get("file_type")
        .and_then(|v| v.as_str())
        .map(|t| t == "directory")
        .unwrap_or(false);
    let size = json.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
    Ok((is_dir, size))
}

async fn fs9_readdir(
    client: &reqwest::Client,
    fs9_url: &str,
    token: &str,
    remote_path: &str,
) -> Result<Vec<(String, bool, u64)>, String> {
    let url = format!("{fs9_url}/api/v1/readdir");
    let resp = client
        .get(&url)
        .query(&[("path", remote_path)])
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .map_err(|e| format!("fs9 readdir failed: {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("fs9 readdir error ({status}): {body}"));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| format!("fs9 readdir parse error: {e}"))?;
    let entries = json.as_array().ok_or("fs9 readdir: expected JSON array")?;
    let mut result = Vec::new();
    for entry in entries {
        let path = entry
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let is_dir = entry
            .get("file_type")
            .and_then(|v| v.as_str())
            .map(|t| t == "directory")
            .unwrap_or(false);
        let size = entry.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
        result.push((path, is_dir, size));
    }
    Ok(result)
}

async fn cmd_sh(api: &ApiClient, api_url: &str, id: Option<&str>, command: Option<&str>) {
    let token = require_token();
    let headers = make_auth_headers(&token);
    let db_id = resolve_db_id(api, id, &headers).await;
    let fs9_url = derive_fs9_url(api_url, &db_id);

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

async fn cmd_fs_events(
    api: &ApiClient,
    api_url: &str,
    output: &OutputFormat,
    id: Option<&str>,
    limit: usize,
    offset: usize,
    path: Option<&str>,
    event_type: Option<&str>,
) {
    let token = require_token();
    let headers = make_auth_headers(&token);
    let db_id = resolve_db_id(api, id, &headers).await;
    let fs9_url = derive_fs9_url(api_url, &db_id);

    let mut url = format!("{fs9_url}/api/v1/events?limit={limit}&offset={offset}");
    if let Some(p) = path {
        url.push_str(&format!("&path={p}"));
    }
    if let Some(t) = event_type {
        url.push_str(&format!("&type={t}"));
    }

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap_or_else(|e| {
            eprintln!("Failed to create HTTP client: {e}");
            process::exit(1);
        });

    let resp = client
        .get(&url)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap_or_else(|e| {
            eprintln!("Failed to connect to fs9: {e}");
            process::exit(1);
        });

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eprintln!("Error {status}: {body}");
        process::exit(1);
    }

    let events: Vec<Value> = resp.json().await.unwrap_or_else(|e| {
        eprintln!("Failed to parse response: {e}");
        process::exit(1);
    });

    match output {
        OutputFormat::Json => print_json(&Value::Array(events)),
        OutputFormat::Csv => {
            print_csv(
                &events,
                &[
                    ("TIMESTAMP", "timestamp", 12),
                    ("TYPE", "event_type", 10),
                    ("PATH", "path", 30),
                    ("COUNT", "count", 5),
                ],
            );
        }
        OutputFormat::Table => {
            if events.is_empty() {
                println!("No events found.");
                return;
            }
            print_table(
                &events,
                &[
                    ("TIMESTAMP", "timestamp", 12),
                    ("TYPE", "event_type", 10),
                    ("PATH", "path", 30),
                    ("COUNT", "count", 5),
                ],
            );
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

async fn cmd_login(api: &ApiClient, output: &OutputFormat, api_key: Option<String>) {
    // If --api-key is provided, save it directly and verify
    if let Some(key) = api_key {
        if let Err(e) = save_token(&key) {
            eprintln!("{e}");
            process::exit(1);
        }

        // Verify the token works by creating a new client with it and calling /customer/me
        let verify_api = ApiClient::new(api.base_url(), Some(&key));
        let result = verify_api.request("GET", "/customer/me", None, None).await;

        if result.get("error").is_some() || result.get("id").is_none() {
            // Token invalid, remove it
            let cred_path = config_dir().join("credentials");
            let _ = std::fs::remove_file(&cred_path);
            eprintln!("Invalid API key");
            process::exit(1);
        }

        // Clear any anonymous credentials
        if let Err(e) = clear_anonymous_credentials() {
            eprintln!("Warning: failed to clear anonymous credentials: {e}");
        }

        match output {
            OutputFormat::Json => {
                let safe = serde_json::json!({
                    "status": "ok",
                    "email": result.get("email"),
                });
                print_json(&safe);
            }
            _ => {
                println!(
                    "Login successful! Logged in as: {}",
                    result["email"].as_str().unwrap_or("unknown")
                );
            }
        }
        return;
    }

    // Interactive login with email/password
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

    // Real login replaces any anonymous session — clear leftover flags.
    if let Err(e) = clear_anonymous_credentials() {
        eprintln!("Warning: failed to clear anonymous credentials: {e}");
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
    let token = match load_token() {
        Ok(t) => t,
        Err(_) => {
            eprintln!("You're not logged in.");
            eprint!("Would you like to [L]ogin or [R]egister? ");
            io::stdout().flush().ok();
            let mut answer = String::new();
            io::stdin().read_line(&mut answer).ok();
            match answer.trim().to_ascii_lowercase().as_str() {
                "l" | "login" => cmd_login(api, output, None).await,
                "r" | "register" => {
                    cmd_register(api, output).await;
                    println!();
                    cmd_login(api, output, None).await;
                }
                _ => {
                    eprintln!("Aborted. Run 'db9 login' or 'db9 register' first.");
                    process::exit(1);
                }
            }
            match load_token() {
                Ok(t) => t,
                Err(_) => {
                    eprintln!("Login failed.");
                    process::exit(1);
                }
            }
        }
    };

    let is_anon = {
        let cred_path = config_dir().join("credentials");
        let content = std::fs::read_to_string(&cred_path).unwrap_or_default();
        let parsed: toml::Table = content.parse().unwrap_or_default();
        parsed
            .get("is_anonymous")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    };

    if !is_anon {
        match output {
            OutputFormat::Json => {
                print_json(&serde_json::json!({"status": "already_claimed"}));
            }
            _ => println!("Your account is already registered. Nothing to claim."),
        }
        return;
    }

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
            cmd_login(api, output, None).await;
        } else {
            println!("\n--- Register ---");
            cmd_register(api, output).await;
            println!("\n--- Login ---");
            cmd_login(api, output, None).await;
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
        cmd_db_create(api, output, name, None, None).await;
    }

    println!("\nYou're all set! Run 'db9 --help' to see all available commands.");
}

async fn cmd_db_create(
    api: &ApiClient,
    output: &OutputFormat,
    name: &str,
    region: Option<&str>,
    password: Option<&str>,
) {
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
    if let Some(p) = password {
        if p.is_empty() {
            eprintln!("Error: --password cannot be empty");
            process::exit(1);
        }
        body["admin_password"] = Value::String(p.to_string());
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
        _ => repl::output::print_sql_result(
            &data,
            output,
            false,
            &None,
            ExpandedMode::Off,
            "NULL",
            1,
            repl::LinestyleMode::Ascii,
        ),
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
        _ => repl::output::print_sql_result(
            &data,
            output,
            false,
            &None,
            ExpandedMode::Off,
            "NULL",
            1,
            repl::LinestyleMode::Ascii,
        ),
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
        _ => repl::output::print_sql_result(
            &data,
            output,
            false,
            &None,
            ExpandedMode::Off,
            "NULL",
            1,
            repl::LinestyleMode::Ascii,
        ),
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

async fn build_executor(
    api: &ApiClient,
    id: &str,
    direct: bool,
    dsn: Option<&str>,
) -> repl::SqlExecutor {
    if !direct {
        return repl::SqlExecutor::Api;
    }

    let connection_dsn = if let Some(d) = dsn {
        d.to_string()
    } else {
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
        data["connection_string"]
            .as_str()
            .unwrap_or_else(|| {
                eprintln!("No connection string found for database. Use --dsn to provide one.");
                process::exit(1);
            })
            .to_string()
    };

    match repl::direct::DirectExecutor::connect(&connection_dsn).await {
        Ok(exec) => repl::SqlExecutor::Direct(exec),
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    }
}

async fn cmd_db_sql(
    api: &ApiClient,
    output: &OutputFormat,
    id: Option<&str>,
    query: Option<&str>,
    file: Option<&str>,
    direct: bool,
    dsn: Option<&str>,
) {
    let token = require_token();
    let headers = make_auth_headers(&token);
    let id_owned = resolve_db_id(api, id, &headers).await;
    let id = id_owned.as_str();
    let sql = if let Some(q) = query {
        q.to_string()
    } else if let Some(f) = file {
        std::fs::read_to_string(f).unwrap_or_else(|e| {
            eprintln!("Failed to read file '{f}': {e}");
            process::exit(1);
        })
    } else if atty::is(atty::Stream::Stdin) {
        let executor = build_executor(api, id, direct, dsn).await;
        return repl::run(api, output, id, executor).await;
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

    if direct {
        let executor = build_executor(api, id, true, dsn).await;
        if let repl::SqlExecutor::Direct(exec) = &executor {
            match exec.execute(&sql).await {
                Ok(data) => repl::output::print_sql_result(
                    &data,
                    output,
                    false,
                    &None,
                    ExpandedMode::Off,
                    "NULL",
                    1,
                    repl::LinestyleMode::Ascii,
                ),
                Err(e) => {
                    eprintln!("\x1b[31mERROR:\x1b[0m {e}");
                    process::exit(1);
                }
            }
        }
    } else {
        let data = execute_sql(api, id, &sql).await;
        if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
            eprintln!("\x1b[31mERROR:\x1b[0m {err}");
            process::exit(1);
        }
        repl::output::print_sql_result(
            &data,
            output,
            false,
            &None,
            ExpandedMode::Off,
            "NULL",
            1,
            repl::LinestyleMode::Ascii,
        );
    }
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

fn escape_sql(s: &str) -> String {
    s.replace('\'', "''")
}

fn resolve_job_id_expr(job: &str) -> String {
    if job.parse::<i64>().is_ok() {
        job.to_string()
    } else {
        format!(
            "(SELECT jobid FROM cron.job WHERE jobname = '{}')",
            escape_sql(job)
        )
    }
}

fn check_sql_error(data: &Value) {
    if let Some(err) = data.get("error").and_then(|v| v.as_str()) {
        eprintln!("\x1b[31mERROR:\x1b[0m {err}");
        process::exit(1);
    }
}

async fn cmd_cron_list(api: &ApiClient, output: &OutputFormat, id: &str) {
    let data = execute_sql(
        api,
        id,
        "SELECT jobid, schedule, command, nodename, nodeport, database, username, active, jobname FROM cron.job ORDER BY jobid",
    )
    .await;
    check_sql_error(&data);

    match output {
        OutputFormat::Json => print_json(&data),
        _ => repl::output::print_sql_result(
            &data,
            output,
            false,
            &None,
            ExpandedMode::Off,
            "NULL",
            1,
            repl::LinestyleMode::Ascii,
        ),
    }
}

fn resolve_cron_command(command: Option<&str>, file: Option<&str>) -> String {
    match (command, file) {
        (Some(cmd), None) => cmd.to_string(),
        (None, Some(path)) => {
            let content = std::fs::read_to_string(path).unwrap_or_else(|e| {
                eprintln!("error: cannot read '{}': {e}", path);
                process::exit(1);
            });
            let trimmed = content.trim();
            if trimmed.is_empty() {
                eprintln!("error: '{}' is empty", path);
                process::exit(1);
            }
            trimmed.to_string()
        }
        (None, None) => {
            eprintln!("error: provide a SQL command or --file");
            process::exit(1);
        }
        (Some(_), Some(_)) => unreachable!(),
    }
}

async fn cmd_cron_create(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    schedule: &str,
    command: &str,
    name: Option<&str>,
) {
    let sql = match name {
        Some(n) => format!(
            "SELECT cron.schedule('{}', '{}', '{}')",
            escape_sql(n),
            escape_sql(schedule),
            escape_sql(command)
        ),
        None => format!(
            "SELECT cron.schedule('{}', '{}')",
            escape_sql(schedule),
            escape_sql(command)
        ),
    };
    let data = execute_sql(api, id, &sql).await;
    check_sql_error(&data);

    match output {
        OutputFormat::Json => print_json(&data),
        _ => repl::output::print_sql_result(
            &data,
            output,
            false,
            &None,
            ExpandedMode::Off,
            "NULL",
            1,
            repl::LinestyleMode::Ascii,
        ),
    }
}

async fn cmd_cron_delete(api: &ApiClient, output: &OutputFormat, id: &str, job: &str) {
    let sql = if job.parse::<i64>().is_ok() {
        format!("SELECT cron.unschedule({})", job)
    } else {
        format!("SELECT cron.unschedule('{}')", escape_sql(job))
    };
    let data = execute_sql(api, id, &sql).await;
    check_sql_error(&data);

    match output {
        OutputFormat::Json => print_json(&data),
        _ => println!("Job '{}' deleted.", job),
    }
}

async fn cmd_cron_history(
    api: &ApiClient,
    output: &OutputFormat,
    id: &str,
    job: Option<&str>,
    limit: u32,
) {
    let where_clause = match job {
        Some(j) if j.parse::<i64>().is_ok() => format!(" WHERE jobid = {}", j),
        Some(j) => format!(
            " WHERE jobid IN (SELECT jobid FROM cron.job WHERE jobname = '{}')",
            escape_sql(j)
        ),
        None => String::new(),
    };
    let sql = format!(
        "SELECT runid, jobid, status, return_message, start_time, end_time FROM cron.job_run_details{} ORDER BY runid DESC LIMIT {}",
        where_clause, limit
    );
    let data = execute_sql(api, id, &sql).await;
    check_sql_error(&data);

    match output {
        OutputFormat::Json => print_json(&data),
        _ => repl::output::print_sql_result(
            &data,
            output,
            false,
            &None,
            ExpandedMode::Off,
            "NULL",
            1,
            repl::LinestyleMode::Ascii,
        ),
    }
}

async fn cmd_cron_enable(api: &ApiClient, output: &OutputFormat, id: &str, job: &str) {
    let job_id = resolve_job_id_expr(job);
    let sql = format!(
        "SELECT cron.alter_job({}, NULL, NULL, NULL, NULL, true)",
        job_id
    );
    let data = execute_sql(api, id, &sql).await;
    check_sql_error(&data);

    match output {
        OutputFormat::Json => print_json(&data),
        _ => println!("Job '{}' enabled.", job),
    }
}

async fn cmd_cron_disable(api: &ApiClient, output: &OutputFormat, id: &str, job: &str) {
    let job_id = resolve_job_id_expr(job);
    let sql = format!(
        "SELECT cron.alter_job({}, NULL, NULL, NULL, NULL, false)",
        job_id
    );
    let data = execute_sql(api, id, &sql).await;
    check_sql_error(&data);

    match output {
        OutputFormat::Json => print_json(&data),
        _ => println!("Job '{}' disabled.", job),
    }
}

async fn cmd_cron_status(api: &ApiClient, output: &OutputFormat, id: &str, job: Option<&str>) {
    match job {
        None => {
            let sql = "SELECT j.jobid, j.jobname, j.schedule, j.active, j.next_run_at, \
                        d.last_status, d.last_message, d.last_run_at, \
                        agg.total_runs, agg.succeeded, agg.failed \
                        FROM cron.job j \
                        LEFT JOIN ( \
                          SELECT jobid, \
                                 COUNT(*) as total_runs, \
                                 COUNT(*) FILTER (WHERE status = 'succeeded') as succeeded, \
                                 COUNT(*) FILTER (WHERE status = 'failed') as failed \
                          FROM cron.job_run_details GROUP BY jobid \
                        ) agg ON agg.jobid = j.jobid \
                        LEFT JOIN ( \
                          SELECT jobid, status as last_status, return_message as last_message, end_time as last_run_at \
                          FROM cron.job_run_details d2 \
                          WHERE runid = (SELECT MAX(runid) FROM cron.job_run_details d3 WHERE d3.jobid = d2.jobid) \
                        ) d ON d.jobid = j.jobid \
                        ORDER BY j.jobid";
            let data = execute_sql(api, id, sql).await;
            check_sql_error(&data);

            match output {
                OutputFormat::Json => print_json(&data),
                _ => repl::output::print_sql_result(
                    &data,
                    output,
                    false,
                    &None,
                    ExpandedMode::Off,
                    "NULL",
                    1,
                    repl::LinestyleMode::Ascii,
                ),
            }
        }
        Some(j) => {
            let where_clause = if j.parse::<i64>().is_ok() {
                format!("jobid = {}", j)
            } else {
                format!(
                    "jobid IN (SELECT jobid FROM cron.job WHERE jobname = '{}')",
                    escape_sql(j)
                )
            };

            let sql1 = format!(
                "SELECT jobid, jobname, schedule, command, active, database, username, next_run_at \
                 FROM cron.job WHERE {}",
                where_clause
            );
            let data1 = execute_sql(api, id, &sql1).await;
            check_sql_error(&data1);

            if data1["rows"].as_array().map_or(true, |r| r.is_empty()) {
                eprintln!("Job '{}' not found.", j);
                process::exit(1);
            }

            let sql2 = format!(
                "SELECT COUNT(*) as total, \
                 COUNT(*) FILTER (WHERE status = 'succeeded') as succeeded, \
                 COUNT(*) FILTER (WHERE status = 'failed') as failed \
                 FROM cron.job_run_details WHERE {}",
                where_clause
            );
            let data2 = execute_sql(api, id, &sql2).await;
            check_sql_error(&data2);

            let sql3 = format!(
                "SELECT runid, status, return_message, start_time, end_time \
                 FROM cron.job_run_details WHERE {} \
                 ORDER BY runid DESC LIMIT 10",
                where_clause
            );
            let data3 = execute_sql(api, id, &sql3).await;
            check_sql_error(&data3);

            match output {
                OutputFormat::Json => {
                    let combined = serde_json::json!({
                        "job": data1,
                        "stats": data2,
                        "recent_runs": data3,
                    });
                    print_json(&combined);
                }
                _ => {
                    let row = &data1["rows"][0];
                    let val = |i: usize| row[i].as_str().unwrap_or("NULL");

                    println!("Job:       {} (ID: {})", val(1), val(0));
                    println!("Schedule:  {}", val(2));
                    println!("Command:   {}", val(3));
                    println!("Active:    {}", val(4));
                    println!("Next Run:  {}", val(7));
                    println!("Database:  {}", val(5));
                    println!();

                    let srow = &data2["rows"][0];
                    let sval = |i: usize| srow[i].as_str().unwrap_or("0");
                    println!("Execution Summary:");
                    println!("  Total Runs:  {}", sval(0));
                    println!("  Succeeded:   {}", sval(1));
                    println!("  Failed:      {}", sval(2));

                    println!();
                    println!("Recent Runs:");
                    repl::output::print_sql_result(
                        &data3,
                        output,
                        false,
                        &None,
                        ExpandedMode::Off,
                        "NULL",
                        1,
                        repl::LinestyleMode::Ascii,
                    );
                }
            }
        }
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

    #[test]
    fn test_is_fs9_path() {
        assert!(is_fs9_path("fs9:/data/file.txt"));
        assert!(is_fs9_path("fs9://data/file.txt"));
        assert!(is_fs9_path("fs9:/"));
        assert!(is_fs9_path("fs9://"));
        assert!(!is_fs9_path("/local/file.txt"));
        assert!(!is_fs9_path("./relative"));
        assert!(!is_fs9_path("relative/path"));
        assert!(!is_fs9_path(""));
    }

    #[test]
    fn test_parse_fs9_path_single_colon() {
        assert_eq!(parse_fs9_path("fs9:/data/file.txt"), "/data/file.txt");
        assert_eq!(parse_fs9_path("fs9:/"), "/");
        assert_eq!(parse_fs9_path("fs9:/data/"), "/data/");
    }

    #[test]
    fn test_parse_fs9_path_double_slash() {
        assert_eq!(parse_fs9_path("fs9://data/file.txt"), "/data/file.txt");
        assert_eq!(parse_fs9_path("fs9://"), "/");
        assert_eq!(parse_fs9_path("fs9://data/"), "/data/");
    }

    #[test]
    fn test_parse_fs9_path_non_fs9() {
        assert_eq!(parse_fs9_path("/local/file"), "/local/file");
        assert_eq!(parse_fs9_path("./relative"), "./relative");
    }

    #[test]
    fn test_copy_direction_detection() {
        assert!(!is_fs9_path("./local") && is_fs9_path("fs9:/remote"));
        assert!(is_fs9_path("fs9:/remote") && !is_fs9_path("./local"));
        assert!(is_fs9_path("fs9:/a") && is_fs9_path("fs9:/b"));
        assert!(!is_fs9_path("/a") && !is_fs9_path("/b"));
    }
}
