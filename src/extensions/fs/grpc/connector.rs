//! Builds and caches the shared tonic `Channel` to fs9 v2's public
//! listener. One Channel per process; multiplexed by tonic so every
//! per-tenant `GrpcFsBackend` reuses it.
//!
//! Configuration is env-driven, matching the staging deploy shape
//! discovered during live-fire (see project memory):
//!
//! - `FS9_GRPC_ENDPOINT` (required): host:port of the fs9 public listener
//!   (`fs9-public.db9.svc.cluster.local:5481` in staging).
//! - `FS9_GRPC_TLS_SERVER_NAME` (required): SNI to override on the
//!   ServerName check. ELB hostnames don't match the cert's SAN, so this
//!   must be set to `fs9.staging.db9.io` (or its prod equivalent).
//! - `FS9_GRPC_CA_PATH` (optional): path to a PEM CA bundle to trust. If
//!   absent we fall back to system roots via tonic's `tls-roots`
//!   feature. Staging uses a cluster-internal CA — see
//!   `cert-manager/tidb-serverless-ca-secret` — so the cluster manifest
//!   wires this to a Secret-mounted file.
//!
//! Connection tuning matches what v1's `create_channel` settled on
//! (initial stream/connection windows, keep-alive cadence) — those were
//! tuned for cross-pod TCP+TLS throughput and are still appropriate for
//! the v2 public listener.

#![cfg(fsplane_v2_generated)]

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tracing::info;

use crate::config;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(5);
// 8 MiB stream window + 16 MiB connection window — same values v1 ended
// up at after the cross-pod throughput regression (see deleted v1
// client.rs comment block at /tmp/v1/client.rs:91-103).
const INITIAL_STREAM_WINDOW: u32 = 8 * 1024 * 1024;
const INITIAL_CONN_WINDOW: u32 = 16 * 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct ConnectorConfig {
    pub endpoint: String,
    pub server_name: String,
    pub ca_pem: Option<Vec<u8>>,
}

impl ConnectorConfig {
    /// Pull from env. Returns `Err` if either required field is missing;
    /// the caller (init_backend) surfaces this as "fs9 v2 not configured"
    /// and refuses to route a tenant to the gRPC backend.
    pub fn from_env() -> Result<Self> {
        let endpoint = config::env_string("FS9_GRPC_ENDPOINT")
            .ok_or_else(|| anyhow!("FS9_GRPC_ENDPOINT must be set to host:port of fs9 v2"))?;
        let server_name = config::env_string("FS9_GRPC_TLS_SERVER_NAME").ok_or_else(|| {
            anyhow!(
                "FS9_GRPC_TLS_SERVER_NAME must be set (fs9 cert SAN; cluster cert \
                 doesn't match the LB hostname)"
            )
        })?;
        let ca_pem = config::env_string("FS9_GRPC_CA_PATH")
            .map(|path| {
                std::fs::read(&path).map_err(|e| anyhow!("read FS9_GRPC_CA_PATH={path}: {e}"))
            })
            .transpose()?;
        Ok(Self {
            endpoint,
            server_name,
            ca_pem,
        })
    }
}

/// Process-wide channel cache. tonic Channel is `Clone` (cheap — wraps
/// an `Arc<Inner>` internally), so handing out clones from a OnceLock is
/// the correct shape.
static CHANNEL: OnceLock<Channel> = OnceLock::new();

/// Return the shared channel, initializing it on first call.
///
/// The first caller bears the connect cost; subsequent callers hand out
/// channel handles instantly. Initialization is async because tonic's
/// `Endpoint::connect()` does the TCP+TLS handshake eagerly.
pub(crate) async fn shared_channel() -> Result<Channel> {
    if let Some(ch) = CHANNEL.get() {
        return Ok(ch.clone());
    }
    let cfg = ConnectorConfig::from_env()?;
    let ch = build_channel(&cfg).await?;
    // OnceLock::set is a no-op if another task won the race — that's
    // fine, we discard our half-built channel and read the winner's.
    let _ = CHANNEL.set(ch.clone());
    Ok(CHANNEL.get().cloned().unwrap_or(ch))
}

async fn build_channel(cfg: &ConnectorConfig) -> Result<Channel> {
    let uri = if cfg.endpoint.starts_with("http://") || cfg.endpoint.starts_with("https://") {
        cfg.endpoint.clone()
    } else {
        format!("https://{}", cfg.endpoint)
    };
    info!(
        endpoint = %uri,
        server_name = %cfg.server_name,
        ca_present = cfg.ca_pem.is_some(),
        "connecting to fs9 v2 public listener"
    );
    let mut tls = ClientTlsConfig::new().domain_name(cfg.server_name.clone());
    if let Some(pem) = cfg.ca_pem.as_deref() {
        tls = tls.ca_certificate(Certificate::from_pem(pem));
    }
    let endpoint = Endpoint::try_from(uri.clone())
        .map_err(|e| anyhow!("invalid endpoint {uri}: {e}"))?
        .connect_timeout(CONNECT_TIMEOUT)
        .http2_keep_alive_interval(KEEPALIVE_INTERVAL)
        .keep_alive_timeout(KEEPALIVE_TIMEOUT)
        .keep_alive_while_idle(true)
        .initial_stream_window_size(INITIAL_STREAM_WINDOW)
        .initial_connection_window_size(INITIAL_CONN_WINDOW)
        .tls_config(tls)
        .map_err(|e| anyhow!("tls config: {e}"))?;
    endpoint
        .connect()
        .await
        .map_err(|e| anyhow!("connect to fs9 v2 at {uri}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_env_requires_endpoint() {
        // Save and clear; we don't want test order to matter.
        let saved_e = std::env::var("FS9_GRPC_ENDPOINT").ok();
        let saved_s = std::env::var("FS9_GRPC_TLS_SERVER_NAME").ok();
        std::env::remove_var("FS9_GRPC_ENDPOINT");
        std::env::remove_var("FS9_GRPC_TLS_SERVER_NAME");
        let err = ConnectorConfig::from_env().expect_err("missing endpoint must error");
        assert!(err.to_string().contains("FS9_GRPC_ENDPOINT"));

        // Set endpoint only — server_name still required.
        std::env::set_var("FS9_GRPC_ENDPOINT", "fs9.example:5481");
        let err = ConnectorConfig::from_env().expect_err("missing server_name must error");
        assert!(err.to_string().contains("FS9_GRPC_TLS_SERVER_NAME"));

        // Restore env to whatever the surrounding process had.
        match saved_e {
            Some(v) => std::env::set_var("FS9_GRPC_ENDPOINT", v),
            None => std::env::remove_var("FS9_GRPC_ENDPOINT"),
        }
        match saved_s {
            Some(v) => std::env::set_var("FS9_GRPC_TLS_SERVER_NAME", v),
            None => std::env::remove_var("FS9_GRPC_TLS_SERVER_NAME"),
        }
    }
}
