mod auth;
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

use anyhow::Result;
use pgwire::tokio::process_socket;
use pool::TikvClientPool;
use protocol::DynamicHandlerFactory;
use std::env;
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
        .block_on(async_main())
}

async fn async_main() -> Result<()> {
    let subscriber = fmt::Subscriber::builder()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let pd_endpoints =
        env::var("PD_ENDPOINTS").unwrap_or_else(|_| DEFAULT_PD_ENDPOINTS.to_string());
    let pg_port: u16 = env::var("PG_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_PG_PORT);
    let pg_listen_addr = env::var("PG_LISTEN_ADDR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PG_LISTEN_ADDR.to_string());
    let default_keyspace = env::var("PG_KEYSPACE").ok();

    let tls_cert = env::var("PG_TLS_CERT").ok();
    let tls_key = env::var("PG_TLS_KEY").ok();

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

    let client_pool = Arc::new(TikvClientPool::new(pd_addrs.clone()));

    // Verify PD is reachable before accepting connections.
    // TiKV client connections are created lazily per-keyspace on first client request.
    check_pd_health(&pd_addrs[0]).await?;

    client_pool.spawn_reaper();

    sql::trigger_worker::spawn_trigger_worker(client_pool.clone());

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
