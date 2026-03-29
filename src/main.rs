// Rust 1.94+ deepened async-block layout computation; the `do_query` async
// chain in `protocol::handler::dynamic::query` needs depth 130, exceeding the
// default limit of 128.  256 gives comfortable headroom.
#![recursion_limit = "256"]
// Stable Clippy keeps tightening format-string style lints. Treating
// `uninlined_format_args` as a hard error blocks CI on bulk mechanical churn
// without changing behavior, so keep it out of the warning budget.
#![allow(clippy::uninlined_format_args)]

// Use jemalloc instead of glibc malloc.  glibc's per-thread arena policy
// causes severe RSS bloat in multi-tenant deployments (30 GB+ with 22
// keyspaces, see #2141).  jemalloc purges unused pages aggressively and
// keeps fragmentation under control.
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

mod admin;
mod auth;
mod cli;
mod config;
mod cron;
mod export;
mod extensions;
mod model;
mod observability;
mod pool;
mod protocol;
mod session_context;
mod sql;
mod storage;
mod storage_stats;
mod tls;
mod txn;
mod worker;

use crate::config::ServerConfig;
use anyhow::Result;
use pgwire::tokio::{process_socket, CancellationToken};
use pool::TikvClientPool;
use protocol::DynamicHandlerFactory;
use socket2::{SockRef, TcpKeepalive};
use std::collections::HashMap;
use std::env;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, TryAcquireError};
use tokio::task::{Id as TaskId, JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_PG_PORT: u16 = 5433;
const DEFAULT_PD_ENDPOINTS: &str = "127.0.0.1:2379";
const DEFAULT_PG_LISTEN_ADDR: &str = "127.0.0.1";
const DEFAULT_TOKIO_STACK_MB: usize = 8;
const CONNECTION_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const WORKER_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

struct WorkerRuntimeHandles {
    engine_handle: JoinHandle<()>,
    engine_shutdown: CancellationToken,
    gc_loop_handle: JoinHandle<()>,
    hnsw_sweep_handle: JoinHandle<()>,
}

struct ConnectionTaskRegistry {
    tasks: JoinSet<()>,
    cancel_tokens: HashMap<TaskId, CancellationToken>,
}

impl ConnectionTaskRegistry {
    fn new() -> Self {
        Self {
            tasks: JoinSet::new(),
            cancel_tokens: HashMap::new(),
        }
    }

    fn spawn<F>(&mut self, cancel_token: CancellationToken, fut: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let task_id = self.tasks.spawn(fut).id();
        self.cancel_tokens.insert(task_id, cancel_token);
    }

    fn reap_finished(&mut self) {
        while let Some(result) = self.tasks.try_join_next_with_id() {
            self.finish_task(result);
        }
    }

    async fn shutdown(&mut self) {
        self.reap_finished();
        if self.cancel_tokens.is_empty() {
            return;
        }

        for token in self.cancel_tokens.values() {
            token.cancel();
        }

        let graceful = async {
            while let Some(result) = self.tasks.join_next_with_id().await {
                self.finish_task(result);
            }
        };

        if tokio::time::timeout(CONNECTION_SHUTDOWN_GRACE, graceful)
            .await
            .is_err()
        {
            warn!(
                "Timed out waiting for {} connection task(s) to exit; aborting remaining tasks",
                self.cancel_tokens.len()
            );
            self.tasks.abort_all();
            while let Some(result) = self.tasks.join_next_with_id().await {
                self.finish_task(result);
            }
        }
    }

    fn finish_task(&mut self, result: std::result::Result<(TaskId, ()), tokio::task::JoinError>) {
        match result {
            Ok((task_id, ())) => {
                self.cancel_tokens.remove(&task_id);
            }
            Err(e) => {
                self.cancel_tokens.remove(&e.id());
                if !e.is_cancelled() {
                    warn!("Connection task join failed during shutdown: {}", e);
                }
            }
        }
    }
}

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
    let initial_server_config = server_config.read().unwrap().clone();
    config::init_embedding_config();

    // Initialize HNSW S3 offload client.
    // When HNSW_S3_BUCKET is explicitly set, S3 init failure is fatal —
    // the operator intends S3 storage and silent fallback to TiKV would
    // defeat that intent and risk mixed-backend inconsistency.
    // When HNSW_S3_BUCKET is not set, init succeeds with S3 disabled.
    if let Err(e) = crate::sql::hnsw::s3::init_hnsw_s3() {
        eprintln!("FATAL: HNSW S3 initialization failed: {}", e);
        return Err(e);
    }

    info!(
        "Statement timeout default: {}ms, idle-in-transaction timeout default: {}ms, pgwire TCP keepalive idle: {}ms",
        initial_server_config.statement_timeout_ms,
        initial_server_config.idle_in_transaction_session_timeout_ms,
        initial_server_config.tcp_keepalive_idle_ms
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
    let client_pool = Arc::new(
        TikvClientPool::new(pd_addrs.clone())
            .with_concurrency_limit(initial_server_config.max_concurrent_queries_per_principal),
    );

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

    // ================================================================
    // GC registry: UNCONDITIONAL for all SQL-serving processes.
    // This is a cluster invariant — not gated by any config flag.
    // Every process that accepts SQL connections MUST publish its
    // min_start_ts so other instances' safepoint advancement doesn't
    // overrun active transactions.
    // ================================================================
    let active_txn_registry = Arc::new(worker::active_txn_registry::ActiveTxnRegistry::new());
    worker::active_txn_registry::set_global_registry(active_txn_registry.clone());

    let worker_config = worker::config::WorkerConfig::from_env();

    // GC registry store — init unconditionally. Fail-fast if unavailable.
    let gc_store = worker::init_gc_registry_store(pd_addrs.clone(), &worker_config)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to initialize GC registry store: {}. \
                 Every SQL-serving db9 process must participate in the GC registry.",
                e
            )
        })?;
    worker::set_gc_registry_store(gc_store.clone());

    // Validate GC config UNCONDITIONALLY — even if this node doesn't advance
    // the safepoint, another node in the cluster might. This only checks
    // structural GC invariants (for example interval < life_time); foreground
    // and worker transactions are protected by direct registry tracking.
    worker_config.validate_gc_config();

    worker::gc::publish_gc_instance_state_once(&gc_store, &worker_config)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to publish GC registry state during startup: {}. \
                 A SQL-serving db9 process must publish GC liveness before it accepts traffic.",
                e
            )
        })?;
    info!("GC registry startup publish completed");

    // Note: we do NOT scan keyspaces at startup to detect S3-backed indexes
    // when HNSW_S3_BUCKET is unset. That check used get_client() which
    // bootstraps inactive tenants (format marker + postgres database) as a
    // side effect. Instead, we rely on the runtime check in load_base_graph()
    // at storage.rs:457-465 which returns a clear, actionable error:
    // "HNSW index requires S3 storage. Set HNSW_S3_BUCKET to enable."

    // GC registry publisher — UNCONDITIONAL. Runs on every SQL-serving node.
    // Publishes this instance's min_start_ts to _sys_worker every interval.
    // This is NOT inside any if-block — it always runs.
    let publisher_handle = {
        let publisher_store = gc_store.clone();
        let publisher_config = worker_config.clone();
        let handle = tokio::spawn(async move {
            supervised_background_loop("GC publisher", || {
                worker::gc::run_gc_publisher_loop(&publisher_store, &publisher_config)
            })
            .await;
        });
        info!("GC registry publisher started (unconditional, supervised)");
        handle
    };

    // ================================================================
    // GC safepoint advancer: OPTIONAL — reads all instances' states
    // from shared registry, computes global min, advances PD safepoint.
    // ================================================================
    let advancer_handle = if worker_config.gc_safepoint_enabled {
        let advancer_store = gc_store.clone();
        let advancer_config = worker_config.clone();
        let advancer_metrics = Arc::new(worker::metrics::WorkerMetrics::new());
        let advancer_metrics_clone = advancer_metrics.clone();
        let handle = tokio::spawn(async move {
            supervised_background_loop("GC advancer", || {
                worker::gc::run_gc_advancer_loop(
                    &advancer_store,
                    &advancer_config,
                    &advancer_metrics_clone,
                )
            })
            .await;
        });
        info!("GC safepoint advancer started (supervised)");
        Some(handle)
    } else {
        None
    };

    // ================================================================
    // Worker engine: OPTIONAL — cron, triggers, HNSW, DDL, BgSql.
    // ================================================================
    let worker_runtime = if worker_config.enabled {
        let system_store = match worker::init_system_store(pd_addrs.clone(), &worker_config).await {
            Ok(Some(system_store)) => system_store,
            Ok(None) => unreachable!("worker init returned None while worker is enabled"),
            Err(e) => {
                return Err(anyhow::anyhow!(
                    "Failed to initialize system store: {}. \
                         Worker is enabled (DB9_WORKER_ENABLED=true) but cannot start. \
                         Either fix the system store connection or set DB9_WORKER_ENABLED=false.",
                    e
                ));
            }
        };

        worker::set_system_store(system_store.clone());

        let engine = worker::engine::WorkerEngine::new(
            worker_config.clone(),
            system_store.clone(),
            client_pool.clone(),
        );
        let metrics = engine.metrics().clone();
        let engine_shutdown = engine.shutdown_token();
        worker::set_worker_metrics(metrics.clone());
        let engine_handle = tokio::spawn(async move { engine.run().await });

        // WorkerGc: orphan claims + cron cleanup + HNSW sweep ONLY.
        // Publisher and advancer are spawned above, not here.
        let gc = Arc::new(worker::gc::WorkerGc::new(
            system_store,
            client_pool.clone(),
            worker_config.clone(),
            metrics,
        ));
        let gc_handles = gc.spawn_worker_gc_only();

        info!("WorkerEngine started (cron/triggers/HNSW/DDL)");
        Some(WorkerRuntimeHandles {
            engine_handle,
            engine_shutdown,
            gc_loop_handle: gc_handles.gc_loop_handle,
            hnsw_sweep_handle: gc_handles.hnsw_sweep_handle,
        })
    } else {
        None
    };

    // Export snapshot janitor (Backup v2 prerequisite).
    // Runs unconditionally — lightweight no-op when no export snapshots exist.
    if let Some(tikv_client) = store.transaction_client() {
        let registry = Arc::new(export::registry::ExportSnapshotRegistry::new(tikv_client));

        // Crash recovery: expire any stale snapshots from previous run.
        if let Err(e) = export::lifecycle::recover_stale_snapshots(&registry).await {
            warn!("export snapshot crash recovery failed: {e}");
        }

        let janitor_registry = registry.clone();
        tokio::spawn(async move {
            export::lifecycle::export_snapshot_janitor_loop(janitor_registry).await;
        });

        export::set_global_registry(registry);
        info!("Export snapshot janitor started");
    }

    // Redis (required dependency for fs9 event streaming)
    extensions::fs::redis_events::init_redis_client().await?;
    extensions::fs::redis_events::spawn_event_loop();
    info!("Redis event streaming initialized");

    // fs9 storage stats background worker — periodically computes and caches
    // aggregate FS stats for O(1) reads by `db9 inspect`.
    if let Some(fs9_tikv_client) = store.transaction_client() {
        tokio::spawn(async move {
            supervised_background_loop("fs9 stats worker", || {
                extensions::fs::stats_worker::run_fs9_stats_worker(fs9_tikv_client.clone())
            })
            .await;
        });
        info!("fs9 stats background worker started (supervised)");
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
    let mut connection_tasks = ConnectionTaskRegistry::new();
    info!("Max connections: {}", max_connections);

    let mut shutdown = std::pin::pin!(shutdown_signal());
    let serve_result: Result<()> = loop {
        connection_tasks.reap_finished();
        let (socket, peer_addr) = tokio::select! {
            shutdown_reason = &mut shutdown => {
                match shutdown_reason {
                    Ok(reason) => {
                        info!("Shutdown signal received: {}", reason);
                        break Ok(());
                    }
                    Err(e) => break Err(e),
                }
            }
            accept_result = listener.accept() => {
                match accept_result {
                    Ok(connection) => connection,
                    Err(e) => break Err(e.into()),
                }
            }
        };
        let accept_config = server_config.read().unwrap().clone();

        if let Err(e) = configure_pgwire_socket_keepalive(&socket, &accept_config) {
            warn!("Failed to configure TCP keepalive for {}: {}", peer_addr, e);
        }

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

        connection_tasks.spawn(cancel_token.clone(), async move {
            let _permit = permit; // held for connection lifetime
            if let Err(e) = process_socket(socket, tls_acceptor, factory, Some(cancel_token)).await
            {
                tracing::error!("Connection error: {}", e);
            }
        });
    };

    shutdown_server_runtime(
        &gc_store,
        &worker_config,
        &mut connection_tasks,
        worker_runtime,
        publisher_handle,
        advancer_handle,
    )
    .await;
    serve_result
}

fn configure_pgwire_socket_keepalive(
    socket: &tokio::net::TcpStream,
    server_config: &ServerConfig,
) -> std::io::Result<()> {
    let socket_ref = SockRef::from(socket);
    if server_config.tcp_keepalive_idle_ms == 0 {
        socket_ref.set_keepalive(false)?;
        return Ok(());
    }

    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_millis(server_config.tcp_keepalive_idle_ms))
        .with_interval(Duration::from_secs(10))
        .with_retries(3);
    socket_ref.set_tcp_keepalive(&keepalive)
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

async fn shutdown_server_runtime(
    gc_store: &storage::TikvStore,
    worker_config: &worker::config::WorkerConfig,
    connection_tasks: &mut ConnectionTaskRegistry,
    worker_runtime: Option<WorkerRuntimeHandles>,
    publisher_handle: JoinHandle<()>,
    advancer_handle: Option<JoinHandle<()>>,
) {
    connection_tasks.shutdown().await;
    shutdown_worker_runtime(worker_runtime).await;
    shutdown_gc_runtime(gc_store, worker_config, publisher_handle, advancer_handle).await;
}

async fn shutdown_worker_runtime(worker_runtime: Option<WorkerRuntimeHandles>) {
    let Some(worker_runtime) = worker_runtime else {
        return;
    };
    let WorkerRuntimeHandles {
        engine_handle,
        engine_shutdown,
        gc_loop_handle,
        hnsw_sweep_handle,
    } = worker_runtime;

    engine_shutdown.cancel();
    worker::wake_worker();

    let mut engine_handle = engine_handle;
    match tokio::time::timeout(WORKER_SHUTDOWN_GRACE, &mut engine_handle).await {
        Ok(Ok(())) => info!("WorkerEngine stopped"),
        Ok(Err(e)) if e.is_cancelled() => info!("WorkerEngine stopped"),
        Ok(Err(e)) => warn!("WorkerEngine join failed during shutdown: {}", e),
        Err(_) => {
            warn!("Timed out waiting for WorkerEngine to stop; aborting");
            abort_task("WorkerEngine", engine_handle).await;
        }
    }

    abort_task("Worker GC loop", gc_loop_handle).await;
    abort_task("HNSW sweep loop", hnsw_sweep_handle).await;
}

async fn shutdown_gc_runtime(
    gc_store: &storage::TikvStore,
    worker_config: &worker::config::WorkerConfig,
    publisher_handle: JoinHandle<()>,
    advancer_handle: Option<JoinHandle<()>>,
) {
    abort_task("GC registry publisher", publisher_handle).await;
    if let Some(handle) = advancer_handle {
        abort_task("GC safepoint advancer", handle).await;
    }

    match worker::gc::clear_gc_instance_state(gc_store, worker_config).await {
        Ok(()) => info!("Cleared local GC registry state during shutdown"),
        Err(e) => warn!(
            "Failed to clear local GC registry state during shutdown: {}",
            e
        ),
    }
}

/// Restart-on-panic supervisor for infinite background loops.
///
/// Runs `make_fut()` repeatedly.  If the future panics or exits
/// unexpectedly, logs an error and retries after a 5 s cooldown.
/// This ensures a single transient panic does not permanently kill a
/// critical background task (e.g., the GC publisher or advancer).
async fn supervised_background_loop<F, Fut>(name: &str, make_fut: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use futures_util::FutureExt;

    loop {
        let result = std::panic::AssertUnwindSafe(make_fut())
            .catch_unwind()
            .await;
        match result {
            Ok(()) => {
                tracing::error!("{name} loop exited unexpectedly; restarting in 5 s");
            }
            Err(panic) => {
                let msg = if let Some(s) = panic.downcast_ref::<&str>() {
                    s.to_string()
                } else if let Some(s) = panic.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "(non-string panic)".to_string()
                };
                tracing::error!("{name} loop panicked: {msg}; restarting in 5 s");
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn abort_task(task_name: &str, handle: JoinHandle<()>) {
    handle.abort();
    match handle.await {
        Ok(()) => info!("{} stopped", task_name),
        Err(e) if e.is_cancelled() => info!("{} stopped", task_name),
        Err(e) => warn!("{} join failed during shutdown: {}", task_name, e),
    }
}

async fn shutdown_signal() -> Result<&'static str> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => Ok("SIGINT"),
            _ = terminate.recv() => Ok("SIGTERM"),
        }
    }

    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok("ctrl_c")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use socket2::SockRef;
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

    #[tokio::test]
    async fn test_configure_pgwire_socket_keepalive_applies_and_disables_keepalive() {
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return;
            }
            Err(e) => panic!("failed to bind test listener: {e}"),
        };
        let addr = listener.local_addr().unwrap();

        let client =
            tokio::spawn(async move { tokio::net::TcpStream::connect(addr).await.unwrap() });
        let (server, _) = listener.accept().await.unwrap();
        let _client = client.await.unwrap();

        let mut cfg = ServerConfig {
            tcp_keepalive_idle_ms: 4_000,
            ..Default::default()
        };
        configure_pgwire_socket_keepalive(&server, &cfg).unwrap();

        let socket_ref = SockRef::from(&server);
        assert!(socket_ref.keepalive().unwrap());
        #[cfg(not(any(
            windows,
            target_os = "haiku",
            target_os = "openbsd",
            target_os = "vita"
        )))]
        assert_eq!(
            socket_ref.tcp_keepalive_time().unwrap(),
            Duration::from_secs(4)
        );

        cfg.tcp_keepalive_idle_ms = 0;
        configure_pgwire_socket_keepalive(&server, &cfg).unwrap();
        assert!(!socket_ref.keepalive().unwrap());
    }

    #[test]
    fn gc_startup_publish_happens_before_listener_accepts_connections() {
        let source = include_str!("main.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("main.rs must contain #[cfg(test)]");
        let startup_publish = prod_source
            .find("publish_gc_instance_state_once(&gc_store, &worker_config)")
            .expect("main.rs must publish GC registry state during startup");
        let listener_bind = prod_source
            .find("let listener = TcpListener::bind")
            .expect("main.rs must bind the pgwire listener");

        assert!(
            startup_publish < listener_bind,
            "GC registry startup publish must complete before the server starts accepting SQL traffic"
        );
    }

    #[test]
    fn gc_shutdown_clears_local_state_after_stopping_gc_tasks() {
        let source = include_str!("main.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("main.rs must contain #[cfg(test)]");
        let shutdown_fn = prod_source
            .split("async fn shutdown_gc_runtime")
            .nth(1)
            .and_then(|rest| rest.split("async fn abort_task").next())
            .expect("main.rs must define shutdown_gc_runtime");

        let publisher_abort = shutdown_fn
            .find("abort_task(\"GC registry publisher\", publisher_handle)")
            .expect("shutdown must stop the publisher loop");
        let clear_state = shutdown_fn
            .find("clear_gc_instance_state(gc_store, worker_config)")
            .expect("shutdown must clear the local GC registry row");

        assert!(
            publisher_abort < clear_state,
            "shutdown must stop GC loops before clearing the local GC registry row"
        );
    }

    #[test]
    fn server_shutdown_quiesces_connections_and_workers_before_gc_clear() {
        let source = include_str!("main.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("main.rs must contain #[cfg(test)]");
        let shutdown_fn = prod_source
            .split("async fn shutdown_server_runtime")
            .nth(1)
            .and_then(|rest| rest.split("async fn shutdown_worker_runtime").next())
            .expect("main.rs must define shutdown_server_runtime");

        let shutdown_connections = shutdown_fn
            .find("connection_tasks.shutdown().await")
            .expect("server shutdown must wait for connection tasks");
        let shutdown_workers = shutdown_fn
            .find("shutdown_worker_runtime(worker_runtime).await")
            .expect("server shutdown must wait for worker runtime");
        let shutdown_gc = shutdown_fn
            .find("shutdown_gc_runtime(gc_store, worker_config, publisher_handle, advancer_handle)")
            .expect("server shutdown must stop GC runtime last");

        assert!(
            shutdown_connections < shutdown_gc,
            "server shutdown must quiesce connection tasks before clearing the local GC registry row"
        );
        assert!(
            shutdown_workers < shutdown_gc,
            "server shutdown must quiesce worker runtime before clearing the local GC registry row"
        );
    }

    #[test]
    fn gc_loops_are_supervised_with_panic_restart() {
        let source = include_str!("main.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("main.rs must contain #[cfg(test)]");

        // Both GC loops must be wrapped in the supervised_background_loop supervisor.
        assert!(
            prod_source.contains("supervised_background_loop(\"GC publisher\""),
            "GC publisher must be wrapped in supervised_background_loop for panic restart"
        );
        assert!(
            prod_source.contains("supervised_background_loop(\"GC advancer\""),
            "GC advancer must be wrapped in supervised_background_loop for panic restart"
        );

        // The supervisor must use catch_unwind (via FutureExt) to catch panics.
        let supervisor_fn = prod_source
            .split("async fn supervised_background_loop")
            .nth(1)
            .expect("main.rs must define supervised_background_loop");
        assert!(
            supervisor_fn.contains("catch_unwind"),
            "supervised_background_loop must use catch_unwind to restart on panic"
        );
    }

    #[test]
    fn worker_shutdown_signals_engine_before_abort_fallback() {
        let source = include_str!("main.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("main.rs must contain #[cfg(test)]");
        let shutdown_fn = prod_source
            .split("async fn shutdown_worker_runtime")
            .nth(1)
            .and_then(|rest| rest.split("async fn shutdown_gc_runtime").next())
            .expect("main.rs must define shutdown_worker_runtime");

        let cancel_engine = shutdown_fn
            .find("engine_shutdown.cancel();")
            .expect("worker shutdown must signal engine cancellation");
        let wake_worker = shutdown_fn
            .find("worker::wake_worker();")
            .expect("worker shutdown must wake the engine loop");
        let abort_engine = shutdown_fn
            .find("abort_task(\"WorkerEngine\", engine_handle).await")
            .expect("worker shutdown must retain an abort fallback");

        assert!(
            cancel_engine < abort_engine && wake_worker < abort_engine,
            "worker shutdown must try graceful cancellation before aborting the engine task"
        );
    }
}
