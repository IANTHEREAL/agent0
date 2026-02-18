mod auth;
mod cli;
mod config;
mod cron;
mod extensions;
mod observability;
mod pool;
mod protocol;
mod session_context;
mod sql;
mod storage;
mod tls;
mod txn;
mod types;
mod worker;

use anyhow::Result;
use pgwire::tokio::process_socket;
use pool::TikvClientPool;
use protocol::DynamicHandlerFactory;
use std::env;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_PG_PORT: u16 = 5433;
const DEFAULT_PD_ENDPOINTS: &str = "127.0.0.1:2379";
const DEFAULT_PG_LISTEN_ADDR: &str = "127.0.0.1";

/// Lightweight PD health check — just verifies PD is reachable without
/// creating any TiKV client or keyspace connection.
async fn check_pd_health(pd_endpoint: &str) -> Result<()> {
    // Skip HTTP health check when TLS is configured (PD requires mTLS);
    // the tikv-client will verify connectivity when it connects.
    if std::env::var("TIKV_CA_PATH").is_ok() {
        info!(
            "PD health check skipped (TLS mode; tikv-client will verify connectivity to {})",
            pd_endpoint
        );
        return Ok(());
    }

    let url = format!("http://{}/pd/api/v1/health", pd_endpoint);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| anyhow::anyhow!("HTTP client error: {}", e))?;

    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("PD health check failed ({}): {}", url, e))?;

    if resp.status().is_success() {
        info!("PD health check passed ({})", pd_endpoint);
        Ok(())
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        Err(anyhow::anyhow!(
            "PD health check returned {}: {}",
            status,
            text
        ))
    }
}

fn main() -> Result<()> {
    // Parse CLI args first (before tokio runtime, so --help/--version work without async)
    let args: Vec<String> = std::env::args().collect();
    let cli_args = match cli::parse_args(&args) {
        Ok(cli::CliAction::ShowHelp) => {
            cli::print_help();
            std::process::exit(0);
        }
        Ok(cli::CliAction::ShowVersion) => {
            cli::print_version();
            std::process::exit(0);
        }
        Ok(cli::CliAction::Run(cli_args)) => cli_args,
        Err(msg) => {
            eprintln!(
                "Error: {}\nTry 'pg-tikv --help' for usage information.",
                msg
            );
            std::process::exit(1);
        }
    };

    let stack_mb: usize = env::var("PGTIKV_TOKIO_STACK_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&mb| mb > 0)
        .unwrap_or(4);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack_mb * 1024 * 1024)
        .build()
        .unwrap()
        .block_on(async_main(cli_args))
}

async fn async_main(cli_args: cli::CliArgs) -> Result<()> {
    let subscriber = fmt::Subscriber::builder()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let pd_endpoints = cli_args.pd_endpoints.unwrap_or_else(|| {
        env::var("PD_ENDPOINTS").unwrap_or_else(|_| DEFAULT_PD_ENDPOINTS.to_string())
    });
    let pg_port: u16 = cli_args.port.unwrap_or_else(|| {
        env::var("PG_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(DEFAULT_PG_PORT)
    });
    let pg_listen_addr = cli_args.host.unwrap_or_else(|| {
        env::var("PG_LISTEN_ADDR")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_PG_LISTEN_ADDR.to_string())
    });
    let default_keyspace = cli_args.keyspace.or_else(|| env::var("PG_KEYSPACE").ok());

    let require_tls = config::env_bool("PG_REQUIRE_TLS");
    let dev_mode = config::env_bool("PGTIKV_DEV");
    let insecure_mode = config::env_bool("PGTIKV_INSECURE");

    let tls_cert = cli_args.tls_cert.or_else(|| env::var("PG_TLS_CERT").ok());
    let tls_key = cli_args.tls_key.or_else(|| env::var("PG_TLS_KEY").ok());

    info!("pg-tikv starting up...");
    info!("PD endpoints: {}", pd_endpoints);
    info!("PostgreSQL port: {}", pg_port);
    info!("PostgreSQL listen addr: {}", pg_listen_addr);
    if let Some(ks) = &default_keyspace {
        info!("Default keyspace: {}", ks);
    } else {
        info!("Default keyspace: default");
    }
    info!("Password authentication: enabled (via AuthManager)");
    if require_tls {
        info!("TLS requirement: enabled (PG_REQUIRE_TLS=1)");
    }
    if dev_mode {
        warn!(
            "PGTIKV_DEV=1 enabled: legacy insecure dev behaviors may be allowed (DO NOT use in production)"
        );
    }
    if insecure_mode {
        warn!(
            "PGTIKV_INSECURE=1 enabled: allowing explicitly insecure pgwire posture (DO NOT use in production)"
        );
    }

    let pd_addrs: Vec<String> = pd_endpoints
        .split(',')
        .map(|s| s.trim().to_string())
        .collect();

    let tls_acceptor: Option<Arc<TlsAcceptor>> = match (tls_cert, tls_key) {
        (Some(cert), Some(key)) => match tls::setup_tls(&cert, &key) {
            Ok(acceptor) => {
                info!("TLS enabled with cert: {}, key: {}", cert, key);
                Some(Arc::new(acceptor))
            }
            Err(e) => {
                warn!("Failed to setup TLS: {}. Running without TLS.", e);
                None
            }
        },
        (Some(_), None) | (None, Some(_)) => {
            warn!("Both PG_TLS_CERT and PG_TLS_KEY must be set. Running without TLS.");
            None
        }
        (None, None) => {
            info!("TLS: disabled (set PG_TLS_CERT and PG_TLS_KEY to enable)");
            None
        }
    };

    if require_tls && tls_acceptor.is_none() {
        return Err(anyhow::anyhow!(
            "PG_REQUIRE_TLS=1 but TLS is not configured. Set PG_TLS_CERT and PG_TLS_KEY."
        ));
    }

    let listen_is_loopback = is_loopback_listen_addr(&pg_listen_addr);
    if !listen_is_loopback && tls_acceptor.is_none() && !(insecure_mode || dev_mode) {
        return Err(anyhow::anyhow!(
            "Refusing to start without TLS on non-loopback PG_LISTEN_ADDR={}. Enable TLS (PG_TLS_CERT/PG_TLS_KEY) or explicitly opt into insecure mode (PGTIKV_INSECURE=1 or PGTIKV_DEV=1).",
            pg_listen_addr
        ));
    }

    let client_pool = Arc::new(TikvClientPool::new(pd_addrs.clone()));

    let startup_keyspace = default_keyspace
        .clone()
        .unwrap_or_else(|| "default".to_string());
    info!("Connecting to TiKV with keyspace '{}'...", startup_keyspace);

    let store = client_pool
        .get_client(Some(startup_keyspace.clone()))
        .await
        .map_err(|e| {
            tracing::error!(
                "Failed to connect to TiKV (keyspace '{}'): {}",
                startup_keyspace,
                e
            );
            e
        })?;

    info!("TiKV connection verified");

    // Fail-fast auth bootstrap for the startup keyspace (secure-by-default posture).
    // Per-connection bootstrap still runs in pgwire auth path (idempotent).
    {
        let auth_manager = auth::AuthManager::new();
        let mut txn = store.begin().await?;
        auth_manager.bootstrap(&mut txn).await?;
        txn.commit().await?;
    }

    client_pool.spawn_reaper();

    // Unified worker engine (system-keyspace task queue + GC)
    {
        let worker_config = worker::config::WorkerConfig::from_env();
        if worker_config.enabled {
            match worker::init_system_store(pd_addrs.clone(), &worker_config).await {
                Ok(Some(system_store)) => {
                    worker::set_system_store(system_store.clone());

                    let engine = worker::engine::WorkerEngine::new(
                        worker_config.clone(),
                        system_store.clone(),
                        client_pool.clone(),
                    );
                    tokio::spawn(async move { engine.run().await });

                    let gc = worker::gc::WorkerGc::new(
                        system_store,
                        client_pool.clone(),
                        worker_config,
                    );
                    tokio::spawn(async move { gc.run().await });

                    info!("WorkerEngine and GC started");
                }
                Ok(None) => {
                    info!("Worker engine disabled");
                }
                Err(e) => {
                    warn!(
                        "Failed to initialize system store: {}. Worker engine not started.",
                        e
                    );
                }
            }
        }
    }

    let listener = TcpListener::bind((pg_listen_addr.as_str(), pg_port)).await?;
    info!("PostgreSQL server listening on {}", listener.local_addr()?);
    let connect_host: &str = if pg_listen_addr == "0.0.0.0" {
        "127.0.0.1"
    } else if pg_listen_addr == "::" {
        "::1"
    } else {
        &pg_listen_addr
    };
    info!(
        "Connect using: psql -h {} -p {} -U <keyspace>.<user>",
        connect_host, pg_port
    );

    loop {
        let (socket, _peer_addr) = listener.accept().await?;

        let tls_acceptor = tls_acceptor.clone();
        let client_pool = client_pool.clone();
        let default_keyspace = default_keyspace.clone();

        let factory = DynamicHandlerFactory::new_with_pool(client_pool, default_keyspace);

        tokio::spawn(async move {
            if let Err(e) = process_socket(socket, tls_acceptor, factory).await {
                tracing::error!("Connection error: {}", e);
            }
        });
    }
}

fn is_loopback_listen_addr(addr: &str) -> bool {
    let trimmed = addr.trim();
    if trimmed.eq_ignore_ascii_case("localhost") {
        return true;
    }

    match trimmed.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}
