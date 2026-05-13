//! Obtains `aud="fs-plane"` tokens from db9-backend's
//! `/internal/connect-token/exchange` endpoint, with a process-wide cache
//! keyed by `(tenant_id, role)`.
//!
//! # Why this exists
//!
//! fs9 v2 requires every RPC to carry a JWT whose `aud="fs-plane"`,
//! `tid=<tenant_id>`, and `scp` covers the requested mode. db9-server is
//! **not** an issuer — it forwards the user's connect-token to db9-backend
//! and asks db9-backend to mint a per-tenant fs-plane token under the
//! caller's existing authority.
//!
//! Contract (referenced from `db9-backend/src/api/connect.rs::exchange_connect_token`):
//!
//!   POST {DB9_BACKEND_URL}/internal/connect-token/exchange
//!   Headers:
//!     X-API-Key: <DB9_SERVER_API_KEY>                — operator-issued service principal
//!     Authorization: Bearer <user's connect-token>   — customer authority for the exchange
//!   Body: { "tenant_id": "...", "role": "...", "audience": "fs-plane" }
//!   200: { "token": "<jwt>", "expires_at": <unix-seconds> }
//!
//! Configuration env vars:
//!   - `DB9_BACKEND_URL`         e.g. `http://db9-backend-api-server.cloud-admin-portal.svc.cluster.local:8090`
//!   - `DB9_SERVER_API_KEY`      operator-provisioned, matches one of db9-backend's `config.api_keys`
//!
//! Either being absent leaves the exchange client unusable — fs9 v2 gRPC
//! tenants will then fail at backend-init with a clear error rather than
//! attempting to call an unconfigured endpoint.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// HTTP request to db9-backend exchange endpoint. Field names mirror
/// `backend/src/models.rs::ConnectTokenExchangeRequest`.
#[derive(Serialize)]
struct ExchangeRequest<'a> {
    tenant_id: &'a str,
    role: &'a str,
    audience: &'a str,
}

/// HTTP response from db9-backend exchange endpoint. Field names mirror
/// `backend/src/models.rs::ConnectTokenExchangeResponse`.
#[derive(Deserialize)]
struct ExchangeResponse {
    token: String,
    /// Unix seconds at which the minted JWT expires.
    expires_at: i64,
}

/// Minted fs-plane token plus its expiry. The cache stores it as
/// `(expires_at_unix, jwt)` so the refresh decision is a single
/// `Instant::now()` comparison.
#[derive(Debug, Clone)]
pub(crate) struct Fs9PlaneToken {
    pub token: Arc<str>,
    pub expires_at: std::time::SystemTime,
}

impl Fs9PlaneToken {
    /// Refresh ahead of expiry — same window db9-cli's `refresh.rs` uses.
    /// Keeps a comfortable safety margin so concurrent in-flight RPCs
    /// can't race the expiry under realistic clock skew.
    const REFRESH_LEAD: Duration = Duration::from_secs(60);

    pub fn is_fresh(&self) -> bool {
        let now = std::time::SystemTime::now();
        match self.expires_at.duration_since(now) {
            Ok(remaining) => remaining > Self::REFRESH_LEAD,
            Err(_) => false,
        }
    }
}

/// Configuration for talking to db9-backend's exchange endpoint. Resolved
/// once at process start; nothing about it is per-tenant.
#[derive(Debug, Clone)]
pub(crate) struct ExchangeConfig {
    pub backend_url: String,
    pub api_key: String,
}

impl ExchangeConfig {
    /// Build from explicit values. The fs9 v2 backend init reads the
    /// env vars itself (so it can report partial-config misconfiguration
    /// loudly) and hands the values in here.
    pub fn new(backend_url: String, api_key: String) -> Self {
        Self {
            backend_url: backend_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

/// Cache key. fs9 mints a different scp depending on the role
/// (`admin` → rw, `_db9_sys_readonly` → r); both should be cached
/// independently so a read-only client doesn't accidentally pick up an
/// rw token meant for an admin session sharing the same tenant.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    tenant_id: String,
    role: String,
}

/// Process-wide token cache. Single-flight not needed at this layer:
/// db9-cli's `refresh.rs` proves a per-key Mutex is sufficient at the
/// concurrency we expect (one in-flight exchange per tenant+role).
#[derive(Default)]
pub(crate) struct Fs9PlaneTokenCache {
    inner: Mutex<std::collections::HashMap<CacheKey, Fs9PlaneToken>>,
}

impl Fs9PlaneTokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lookup_fresh(&self, tenant_id: &str, role: &str) -> Option<Fs9PlaneToken> {
        let key = CacheKey {
            tenant_id: tenant_id.to_string(),
            role: role.to_string(),
        };
        let guard = self.inner.lock();
        let entry = guard.get(&key)?;
        if entry.is_fresh() {
            Some(entry.clone())
        } else {
            None
        }
    }

    fn store(&self, tenant_id: &str, role: &str, token: Fs9PlaneToken) {
        let key = CacheKey {
            tenant_id: tenant_id.to_string(),
            role: role.to_string(),
        };
        self.inner.lock().insert(key, token);
    }
}

/// Mint a fresh fs-plane token from db9-backend. Caller decides whether
/// to consult the cache first (typically via `mint_or_reuse`).
async fn mint(
    cfg: &ExchangeConfig,
    user_bearer: &str,
    tenant_id: &str,
    role: &str,
) -> Result<Fs9PlaneToken> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| anyhow!("fs9: build http client: {e}"))?;
    let url = format!("{}/internal/connect-token/exchange", cfg.backend_url);
    let body = ExchangeRequest {
        tenant_id,
        role,
        audience: "fs-plane",
    };
    let resp = client
        .post(&url)
        .header("X-API-Key", &cfg.api_key)
        .header("Authorization", format!("Bearer {user_bearer}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("fs9: exchange request to {url}: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        // Don't echo the body verbatim — it may contain caller-supplied
        // tenant_id which is fine, but defense in depth keeps the error
        // small. The status alone is the actionable signal.
        return Err(anyhow!(
            "fs9: exchange endpoint returned {status}: {}",
            truncate(&text, 200)
        ));
    }
    let payload: ExchangeResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("fs9: parse exchange response: {e}"))?;
    Ok(Fs9PlaneToken {
        token: Arc::from(payload.token.as_str()),
        expires_at: std::time::UNIX_EPOCH + Duration::from_secs(payload.expires_at.max(0) as u64),
    })
}

/// Truncate long server-side error strings before logging / propagation.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

/// Lookup-then-mint with cache. Single entry point for callers (gRPC
/// interceptor) so the cache discipline lives in one place.
pub(crate) async fn mint_or_reuse(
    cache: &Fs9PlaneTokenCache,
    cfg: &ExchangeConfig,
    user_bearer: &str,
    tenant_id: &str,
    role: &str,
) -> Result<Fs9PlaneToken> {
    if let Some(hit) = cache.lookup_fresh(tenant_id, role) {
        return Ok(hit);
    }
    let fresh = mint(cfg, user_bearer, tenant_id, role).await?;
    cache.store(tenant_id, role, fresh.clone());
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn make_token(expires_in_secs: i64) -> Fs9PlaneToken {
        let now = SystemTime::now();
        let when = if expires_in_secs >= 0 {
            now + Duration::from_secs(expires_in_secs as u64)
        } else {
            now - Duration::from_secs((-expires_in_secs) as u64)
        };
        Fs9PlaneToken {
            token: Arc::from("dummy.jwt.signature"),
            expires_at: when,
        }
    }

    #[test]
    fn token_with_distant_expiry_is_fresh() {
        let t = make_token(300);
        assert!(t.is_fresh());
    }

    #[test]
    fn token_within_refresh_lead_is_not_fresh() {
        // 30s remaining < 60s REFRESH_LEAD → stale.
        let t = make_token(30);
        assert!(!t.is_fresh());
    }

    #[test]
    fn token_already_expired_is_not_fresh() {
        let t = make_token(-1);
        assert!(!t.is_fresh());
    }

    #[test]
    fn cache_lookup_returns_none_when_empty() {
        let cache = Fs9PlaneTokenCache::new();
        assert!(cache.lookup_fresh("t1", "admin").is_none());
    }

    #[test]
    fn cache_returns_fresh_entry_and_drops_stale_on_lookup() {
        let cache = Fs9PlaneTokenCache::new();
        cache.store("t1", "admin", make_token(300));
        assert!(cache.lookup_fresh("t1", "admin").is_some());

        cache.store("t1", "admin", make_token(-5));
        assert!(cache.lookup_fresh("t1", "admin").is_none());
    }

    #[test]
    fn cache_segregates_by_role() {
        let cache = Fs9PlaneTokenCache::new();
        cache.store("t1", "admin", make_token(300));
        assert!(cache.lookup_fresh("t1", "admin").is_some());
        // Readonly role has no entry yet — must not borrow admin's token.
        assert!(cache.lookup_fresh("t1", "_db9_sys_readonly").is_none());
    }

    #[test]
    fn cache_segregates_by_tenant() {
        let cache = Fs9PlaneTokenCache::new();
        cache.store("t1", "admin", make_token(300));
        assert!(cache.lookup_fresh("t2", "admin").is_none());
    }

    #[test]
    fn exchange_config_new_trims_trailing_slash() {
        let cfg = ExchangeConfig::new("https://backend.example/".into(), "secret".into());
        assert_eq!(cfg.backend_url, "https://backend.example");
        assert_eq!(cfg.api_key, "secret");
    }
}
