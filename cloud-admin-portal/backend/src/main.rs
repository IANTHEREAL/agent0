use std::sync::Arc;

use axum::Router;
use clap::Parser;
use tower_http::cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer};
use tracing_subscriber::EnvFilter;

use pgtikv_admin::config::Config;
use pgtikv_admin::device_code::DeviceCodeStore;
use pgtikv_admin::services::pd_client::PdClient;
use pgtikv_admin::services::reconciler::Reconciler;
use pgtikv_admin::session::SessionManager;
use pgtikv_admin::{api, db, AppState};

#[derive(Parser)]
#[command(name = "pgtikv-admin", about = "pg-tikv Admin Portal Server", version)]
struct Args {
    /// Listen port
    #[arg(short, long)]
    port: Option<u16>,

    /// Bind address
    #[arg(long)]
    host: Option<String>,

    /// Database URL (sqlite:// or postgres://)
    #[arg(short, long)]
    database_url: Option<String>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let args = Args::parse();
    let mut config = Config::from_env();
    if let Some(port) = args.port {
        config.api_port = port;
    }
    if let Some(host) = args.host {
        config.api_host = host;
    }
    if let Some(url) = args.database_url {
        config.database_url = url;
    }
    tracing::info!(
        "Starting pgtikv-admin v2.0.0 on {}:{}",
        config.api_host,
        config.api_port
    );

    // ── Database ─────────────────────────────────────────────────
    let pool = db::connect(&config.database_url)
        .await
        .expect("Failed to connect to database");

    db::create_tables(&pool)
        .await
        .expect("Failed to create tables");

    tracing::info!("Database ready: {}", config.database_url);

    // ── Shared state ─────────────────────────────────────────────
    let http_client = match (
        std::env::var("TIKV_CA_PATH"),
        std::env::var("TIKV_CERT_PATH"),
        std::env::var("TIKV_KEY_PATH"),
    ) {
        (Ok(ca_path), Ok(cert_path), Ok(key_path)) => {
            tracing::info!(
                "Building HTTP client with mTLS: ca={}, cert={}, key={}",
                ca_path,
                cert_path,
                key_path
            );
            let ca_pem = std::fs::read(&ca_path).expect("Failed to read CA cert");
            let cert_pem = std::fs::read(&cert_path).expect("Failed to read client cert");
            let key_pem = std::fs::read(&key_path).expect("Failed to read client key");

            let ca = reqwest::Certificate::from_pem(&ca_pem).expect("Failed to parse CA cert");
            let mut identity_pem = cert_pem;
            identity_pem.extend_from_slice(&key_pem);
            let identity = reqwest::Identity::from_pem(&identity_pem)
                .expect("Failed to parse client identity");

            reqwest::Client::builder()
                .add_root_certificate(ca)
                .identity(identity)
                .build()
                .expect("Failed to build mTLS HTTP client")
        }
        _ => reqwest::Client::new(),
    };
    let sessions = Arc::new(SessionManager::new(config.session_ttl_hours));
    let config = Arc::new(config);

    let fs9_client = match (&config.fs9_meta_url, &config.fs9_meta_key) {
        (Some(url), Some(key)) => {
            tracing::info!("FS9 integration enabled: {}", url);
            Some(Arc::new(
                pgtikv_admin::services::fs9_client::Fs9Client::new(
                    url.clone(),
                    key.clone(),
                    http_client.clone(),
                ),
            ))
        }
        _ => {
            tracing::info!("FS9 integration disabled (FS9_META_URL or FS9_META_KEY not set)");
            None
        }
    };

    let device_codes = Arc::new(DeviceCodeStore::new(600));
    let state = AppState {
        db: pool.clone(),
        config: config.clone(),
        sessions,
        device_codes,
        http_client: http_client.clone(),
        fs9_client,
    };

    // ── Reconciler ───────────────────────────────────────────────
    let reconciler_stop = if config.reconciler_enabled {
        let pd = PdClient::new(&config.pd_endpoints, &http_client);
        let reconciler = Reconciler::new(
            pd,
            pool.clone(),
            config.reconciler_interval_secs,
            config.reconciler_sync_keyspaces,
        );
        let stop = reconciler.stop_handle();

        if config.reconciler_sync_keyspaces {
            let synced = reconciler.sync_new_keyspaces().await;
            if synced > 0 {
                tracing::info!("Synced {synced} keyspace(s) from PD on startup");
            }
        }

        tokio::spawn(async move {
            reconciler.start().await;
        });

        tracing::info!(
            "Reconciler started (interval={}s, sync_keyspaces={})",
            config.reconciler_interval_secs,
            config.reconciler_sync_keyspaces,
        );
        Some(stop)
    } else {
        tracing::info!("Reconciler disabled");
        None
    };

    // ── Background: session sweep + audit cleanup ─────────────
    {
        let sessions_ref = state.sessions.clone();
        let device_codes_ref = state.device_codes.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            loop {
                interval.tick().await;
                let swept = sessions_ref.sweep_expired();
                if swept > 0 {
                    tracing::debug!("Session sweep: removed {swept} expired sessions");
                }
                let dc_swept = device_codes_ref.sweep_expired();
                if dc_swept > 0 {
                    tracing::debug!("Device code sweep: removed {dc_swept} expired entries");
                }
            }
        });
    }
    {
        let db_ref = pool.clone();
        let retention_days = config.audit_retention_days;
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
            loop {
                interval.tick().await;
                let cutoff = (chrono::Utc::now() - chrono::Duration::days(retention_days as i64))
                    .to_rfc3339();
                match db::delete_old_audit_logs(&db_ref, &cutoff).await {
                    Ok(n) if n > 0 => tracing::info!("Audit cleanup: removed {n} old entries"),
                    Err(e) => tracing::warn!("Audit cleanup failed: {e}"),
                    _ => {}
                }
            }
        });
    }

    // ── CORS ─────────────────────────────────────────────────────
    let origins: Vec<_> = config
        .cors_origins
        .iter()
        .filter_map(|o| o.parse().ok())
        .collect();

    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods(AllowMethods::any())
        .allow_headers(AllowHeaders::list([
            "content-type".parse().unwrap(),
            "x-api-key".parse().unwrap(),
            "x-tenant-session".parse().unwrap(),
        ]));

    // ── Router ───────────────────────────────────────────────────
    let app = Router::new()
        .nest("/api", api::router())
        // FS9 reverse-proxy: /fs9/{db_id}  and  /fs9/{db_id}/{*rest}
        .route(
            "/fs9/:db_id",
            axum::routing::any(api::fs9_proxy::fs9_proxy),
        )
        .route(
            "/fs9/:db_id/",
            axum::routing::any(api::fs9_proxy::fs9_proxy),
        )
        .route(
            "/fs9/:db_id/*rest",
            axum::routing::any(api::fs9_proxy::fs9_proxy),
        )
        .layer(cors)
        .with_state(state);

    // ── Serve ────────────────────────────────────────────────────
    let addr = format!("{}:{}", config.api_host, config.api_port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("Failed to bind address");

    tracing::info!("Listening on {addr}");

    let server = axum::serve(listener, app);

    // Graceful shutdown
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::select! {
        result = server => {
            if let Err(e) = result {
                tracing::error!("Server error: {e}");
            }
        }
        _ = ctrl_c => {
            tracing::info!("Shutting down...");
            if let Some(stop) = reconciler_stop {
                stop.store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}
