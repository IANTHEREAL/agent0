// Rust 1.94+ deepened async-block layout computation; the `do_query` async
// chain in `protocol::handler::dynamic::query` needs depth 130, exceeding the
// default limit of 128.  256 gives comfortable headroom.
#![recursion_limit = "256"]

mod auth;
mod cli;
mod config;
mod cron;
mod extensions;
mod model;
mod observability;
mod pool;
mod protocol;
mod session_context;
mod sql;
mod storage;
mod tls;
mod txn;
mod worker;

use crate::config::ServerConfig;
use anyhow::Result;
use pgwire::tokio::process_socket;
use pool::TikvClientPool;
use protocol::DynamicHandlerFactory;
use std::env;
use std::net::IpAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, TryAcquireError};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_PG_PORT: u16 = 5433;
const DEFAULT_PD_ENDPOINTS: &str = "127.0.0.1:2379";
const DEFAULT_PG_LISTEN_ADDR: &str = "127.0.0.1";
const DEFAULT_TOKIO_STACK_MB: usize = 8;

fn main() -> Result<()> {
    // Record process start time before anything else.
    sql::expr::typed_eval::init_postmaster_start_time();

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
                "Error: {}\nTry 'db9-server --help' for usage information.",
                msg
            );
            std::process::exit(1);
        }
    };

    // Critical execution boundaries (execute_via_optimizer, execute_subquery,
    // try_execute_analyzed) return boxed futures to keep async frame sizes
    // bounded for deep call chains (#907).
    // Override via DB9_TOKIO_STACK_MB for operational edge cases.
    let stack_mb: usize = env::var("DB9_TOKIO_STACK_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&mb| mb > 0)
        .unwrap_or(DEFAULT_TOKIO_STACK_MB);
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
    let dev_mode = config::env_bool("DB9_DEV");
    let insecure_mode = config::env_bool("DB9_INSECURE");
    let server_config = ServerConfig::from_env().shared();
    config::init_embedding_config();
    info!(
        "Statement timeout default: {}ms, idle-in-transaction timeout default: {}ms",
        server_config.read().unwrap().statement_timeout_ms,
        server_config
            .read()
            .unwrap()
            .idle_in_transaction_session_timeout_ms
    );

    let tls_cert = cli_args.tls_cert.or_else(|| env::var("PG_TLS_CERT").ok());
    let tls_key = cli_args.tls_key.or_else(|| env::var("PG_TLS_KEY").ok());

    info!("db9-server starting up...");
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
            "DB9_DEV=1 enabled: legacy insecure dev behaviors may be allowed (DO NOT use in production)"
        );
    }
    if insecure_mode {
        warn!(
            "DB9_INSECURE=1 enabled: allowing explicitly insecure pgwire posture (DO NOT use in production)"
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
            "Refusing to start without TLS on non-loopback PG_LISTEN_ADDR={}. Enable TLS (PG_TLS_CERT/PG_TLS_KEY) or explicitly opt into insecure mode (DB9_INSECURE=1 or DB9_DEV=1).",
            pg_listen_addr
        ));
    }

    storage::backpressure::init(storage::backpressure::BackpressureConfig::from_env());
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
    // Per-connection auth path skips bootstrap when already initialized (#1171).
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
                    let metrics = engine.metrics().clone();
                    worker::set_worker_metrics(metrics.clone());
                    tokio::spawn(async move { engine.run().await });

                    let gc = Arc::new(worker::gc::WorkerGc::new(
                        system_store,
                        client_pool.clone(),
                        worker_config,
                        metrics,
                    ));
                    gc.spawn();

                    info!("WorkerEngine and GC started");
                }
                Ok(None) => {
                    info!("Worker engine disabled");
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to initialize system store: {}. \
                         Worker is enabled (DB9_WORKER_ENABLED=true) but cannot start. \
                         Either fix the system store connection or set DB9_WORKER_ENABLED=false.",
                        e
                    ));
                }
            }
        }
    }

    // fs9 WebSocket server
    {
        let fs9_cfg = extensions::fs::config::fs9_config();
        if fs9_cfg.s3.is_some() && fs9_cfg.upload_token_secret.is_none() {
            return Err(anyhow::anyhow!(
                "fs9: FS9_UPLOAD_TOKEN_SECRET must be configured when FS9_S3_BUCKET is set"
            ));
        }

        let ws_port: u16 = env::var("FS9_WS_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(extensions::fs::ws::protocol::DEFAULT_WS_PORT);

        if ws_port > 0 {
            let ws_listen_addr = env::var("FS9_WS_LISTEN_ADDR")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    extensions::fs::ws::protocol::DEFAULT_WS_LISTEN_ADDR.to_string()
                });

            let ws_is_loopback = is_loopback_listen_addr(&ws_listen_addr);

            // Security: refuse non-loopback without TLS unless explicitly insecure
            if !ws_is_loopback && tls_acceptor.is_none() && !(insecure_mode || dev_mode) {
                warn!(
                    "fs9 WebSocket disabled: non-loopback FS9_WS_LISTEN_ADDR={} without TLS. Enable TLS or set DB9_INSECURE=1/DB9_DEV=1.",
                    ws_listen_addr
                );
            } else if require_tls && tls_acceptor.is_none() {
                warn!(
                    "fs9 WebSocket disabled: PG_REQUIRE_TLS=1 but TLS is not configured. Set PG_TLS_CERT and PG_TLS_KEY."
                );
            } else {
                // Build WebSocket-specific TLS (no ALPN, reuse same cert/key)
                let ws_tls: Option<Arc<TlsAcceptor>> = if tls_acceptor.is_some() {
                    match (env::var("PG_TLS_CERT").ok(), env::var("PG_TLS_KEY").ok()) {
                        (Some(cert), Some(key)) => match tls::setup_ws_tls(&cert, &key) {
                            Ok(acceptor) => Some(Arc::new(acceptor)),
                            Err(e) => {
                                warn!(
                                    "fs9 WebSocket TLS setup failed: {}. Running without TLS.",
                                    e
                                );
                                None
                            }
                        },
                        _ => None,
                    }
                } else {
                    None
                };

                match TcpListener::bind((ws_listen_addr.as_str(), ws_port)).await {
                    Ok(ws_listener) => {
                        info!("fs9 WebSocket listening on {}:{}", ws_listen_addr, ws_port);
                        let pool = client_pool.clone();
                        let ws_default_keyspace = default_keyspace.clone();
                        tokio::spawn(async move {
                            extensions::fs::ws::start_ws_server(
                                ws_listener,
                                pool,
                                ws_tls,
                                ws_default_keyspace,
                            )
                            .await;
                        });
                    }
                    Err(e) => {
                        warn!(
                            "fs9 WebSocket failed to bind {}:{}: {}",
                            ws_listen_addr, ws_port, e
                        );
                    }
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

    let max_connections = server_config.read().unwrap().max_connections;
    let conn_semaphore = Arc::new(Semaphore::new(max_connections as usize));
    info!("Max connections: {}", max_connections);

    loop {
        let (socket, peer_addr) = listener.accept().await?;

        let permit = match conn_semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(TryAcquireError::NoPermits) => {
                warn!(
                    "Connection limit reached (max {}), rejecting {}",
                    max_connections, peer_addr
                );
                tokio::spawn(async move {
                    reject_over_limit(socket).await;
                });
                continue;
            }
            Err(TryAcquireError::Closed) => {
                tracing::error!("Connection semaphore closed unexpectedly");
                break Ok(());
            }
        };

        let tls_acceptor = tls_acceptor.clone();
        let client_pool = client_pool.clone();
        let default_keyspace = default_keyspace.clone();

        let factory = DynamicHandlerFactory::new_with_pool(
            client_pool,
            default_keyspace,
            server_config.clone(),
        );
        let cancel_token = factory.cancel_token();

        tokio::spawn(async move {
            let _permit = permit; // held for connection lifetime
            if let Err(e) = process_socket(socket, tls_acceptor, factory, Some(cancel_token)).await
            {
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

/// Send a pgwire FATAL ErrorResponse (SQLSTATE 53300 "too_many_connections")
/// and close the socket. Matches PostgreSQL's rejection behavior.
async fn reject_over_limit(mut socket: tokio::net::TcpStream) {
    use tokio::io::AsyncWriteExt;

    // pgwire ErrorResponse: 'E' | Int32 len | (Byte1 field_type + CString)... | '\0'
    let fields: &[(&[u8], &[u8])] = &[
        (b"S", b"FATAL"),
        (b"V", b"FATAL"),
        (b"C", b"53300"),
        (b"M", b"sorry, too many clients already"),
    ];

    let mut body = Vec::new();
    for (tag, value) in fields {
        body.extend_from_slice(tag);
        body.extend_from_slice(value);
        body.push(0);
    }
    body.push(0); // terminator

    let len = (body.len() as u32 + 4).to_be_bytes();
    let mut msg = Vec::with_capacity(1 + 4 + body.len());
    msg.push(b'E');
    msg.extend_from_slice(&len);
    msg.extend_from_slice(&body);

    let _ = socket.write_all(&msg).await;
    let _ = socket.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn test_semaphore_admission_control() {
        let max_connections: u32 = 2;
        let semaphore = Arc::new(Semaphore::new(max_connections as usize));

        // Acquire two permits — should succeed immediately.
        let permit1 = semaphore.clone().acquire_owned().await.unwrap();
        let permit2 = semaphore.clone().acquire_owned().await.unwrap();
        assert_eq!(semaphore.available_permits(), 0);

        // Third acquire must block (use try_acquire to verify).
        assert!(semaphore.clone().try_acquire_owned().is_err());

        // Dropping one permit frees a slot.
        drop(permit1);
        assert_eq!(semaphore.available_permits(), 1);
        let _permit3 = semaphore.clone().acquire_owned().await.unwrap();
        assert_eq!(semaphore.available_permits(), 0);

        drop(permit2);
        drop(_permit3);
        assert_eq!(semaphore.available_permits(), 2);
    }

    #[tokio::test]
    async fn test_accept_loop_rejects_over_limit() {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                // Some sandboxed/instrumented environments deny local binds.
                // Skip rather than make unrelated coverage jobs flaky.
                return;
            }
            Err(e) => panic!("failed to bind test listener: {e}"),
        };
        let addr = listener.local_addr().unwrap();
        let max_connections: u32 = 2;
        let semaphore = Arc::new(Semaphore::new(max_connections as usize));

        let sem = semaphore.clone();
        let handle = tokio::spawn(async move {
            let mut handles = Vec::new();
            // Accept exactly 3 connections: 2 get permits, 3rd gets rejected.
            for _ in 0..3 {
                let (socket, _) = listener.accept().await.unwrap();
                let permit = match sem.clone().try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        // Over limit — send PG error and close (matches production).
                        reject_over_limit(socket).await;
                        continue;
                    }
                };
                handles.push(tokio::spawn(async move {
                    let _permit = permit;
                    let _socket = socket;
                    tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                }));
            }
            handles
        });

        // Open 3 client connections.
        let _c1 = tokio::net::TcpStream::connect(addr).await.unwrap();
        let _c2 = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut c3 = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Give the server a moment to process all 3 accepts.
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Only 2 permits should be held.
        assert_eq!(semaphore.available_permits(), 0);

        // The 3rd connection should receive a pgwire FATAL ErrorResponse
        // with SQLSTATE 53300 ("too_many_connections").
        let mut buf = [0u8; 256];
        let n = c3.read(&mut buf).await.unwrap();
        assert!(n > 0, "expected error response from server");
        assert_eq!(buf[0], b'E', "expected pgwire ErrorResponse");
        assert!(
            buf[..n].windows(5).any(|w| w == b"53300"),
            "expected SQLSTATE 53300 in error response"
        );

        let handles = handle.await.unwrap();
        for h in handles {
            h.abort();
        }
    }
}
