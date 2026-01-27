mod auth;
mod extensions;
mod observability;
mod pool;
mod protocol;
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

async fn create_keyspace(pd_endpoint: &str, keyspace_name: &str) -> Result<()> {
    let url = format!("http://{}/pd/api/v2/keyspaces", pd_endpoint);
    let body = serde_json::json!({ "name": keyspace_name });

    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("HTTP request failed: {}", e))?;

    if resp.status().is_success() {
        info!("Successfully created keyspace '{}'", keyspace_name);
        Ok(())
    } else {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if text.contains("already exists") {
            info!("Keyspace '{}' already exists", keyspace_name);
            Ok(())
        } else {
            Err(anyhow::anyhow!("PD returned {}: {}", status, text))
        }
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
    let default_keyspace = env::var("PG_KEYSPACE").ok();

    let tls_cert = env::var("PG_TLS_CERT").ok();
    let tls_key = env::var("PG_TLS_KEY").ok();

    info!("pg-tikv starting up...");
    info!("PD endpoints: {}", pd_endpoints);
    info!("PostgreSQL port: {}", pg_port);
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

    let startup_keyspace = default_keyspace
        .clone()
        .unwrap_or_else(|| "default".to_string());
    info!("Connecting to TiKV with keyspace '{}'...", startup_keyspace);

    let connect_result = client_pool.get_client(Some(startup_keyspace.clone())).await;

    if let Err(e) = connect_result {
        let err_str = format!("{:?}", e);
        if err_str.contains("does not exist") {
            info!(
                "Keyspace '{}' does not exist, attempting to create...",
                startup_keyspace
            );

            if let Err(create_err) = create_keyspace(&pd_addrs[0], &startup_keyspace).await {
                tracing::error!(
                    "Failed to create keyspace '{}': {}",
                    startup_keyspace,
                    create_err
                );
                return Err(e);
            }

            info!(
                "Keyspace '{}' created, retrying connection...",
                startup_keyspace
            );
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

            client_pool
                .get_client(Some(startup_keyspace))
                .await
                .map_err(|e| {
                    tracing::error!("Failed to connect to TiKV after creating keyspace: {}", e);
                    e
                })?;
        } else {
            tracing::error!("Failed to connect to TiKV: {}", e);
            return Err(e);
        }
    }

    info!("TiKV connection verified");

    // Background async AFTER-trigger queue worker (enabled by default).
    sql::trigger_worker::spawn_trigger_worker(client_pool.clone());

    let addr = format!("0.0.0.0:{}", pg_port);
    let listener = TcpListener::bind(&addr).await?;
    info!("PostgreSQL server listening on {}", addr);
    info!(
        "Connect using: psql -h 127.0.0.1 -p {} -U <keyspace>.<user>",
        pg_port
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
