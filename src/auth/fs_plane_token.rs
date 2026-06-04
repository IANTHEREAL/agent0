//! Mints fs9 JWTs by calling auth9 `POST /v1/jwt/sign` directly, with a
//! process-wide cache for data-plane tokens keyed by `(tenant_id, role)`.
//!
//! # Why this exists
//!
//! fs9 v2 requires every data-plane RPC to carry a JWT whose
//! `aud="fs-plane"`, `tid=<tenant_id>`, and `scp` covers the requested
//! mode. Lazy volume materialization uses a separate short-lived
//! `aud="fs-plane-admin"` token for `FsPlaneAdmin.InitVolume`.
//! db9-server is **not** an issuer — auth9 holds the signing key.
//! db9-server is a trusted service whose `X-API-Key` authorises it to ask
//! auth9 for these fs9 tokens; auth9 enforces the audience whitelist via
//! `services.db9-server.allowed_audiences`.
//!
//! Mint contract (see `docs/design/fs9_auth9_direct_mint.md`):
//!
//!   POST {AUTH9_SIGN_URL}
//!   Headers:
//!     X-API-Key: <DB9_AUTH9_SERVICE_API_KEY>
//!     Content-Type: application/json
//!   Body: { "aud": "fs-plane", "ttl_secs": 900,
//!           "claims": { "tid", "usr", "scp" } }
//!   Admin body: { "aud": "fs-plane-admin", "ttl_secs": 60,
//!                 "claims": { "tid", "usr", "scp": "fs:admin",
//!                             "sub": "db9-server" } }
//!   200: { "token": "<jwt>", "expires_at": "<RFC3339>" }
//!
//! The minted token has no `sub` claim: fs9 v2 parses but never reads
//! `sub` (verified by grep against fs9 internal/auth + interceptors),
//! so a per-session value would only create the illusion of an audit
//! contract while leaking through the cache to later sessions.
//!
//! Configuration env vars (both required for JuiceFS tenants):
//!   - `AUTH9_SIGN_URL`            full URL of auth9 `/v1/jwt/sign`
//!   - `DB9_AUTH9_SERVICE_API_KEY` service `X-API-Key`
//!
//! Either being absent leaves the mint client unusable — fs9 v2 gRPC
//! tenants will then fail at backend-init with a clear error rather than
//! attempting to call an unconfigured endpoint.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// Audience accepted by fs9 v2 interceptors.
const FS_PLANE_AUDIENCE: &str = "fs-plane";
const FS_PLANE_ADMIN_AUDIENCE: &str = "fs-plane-admin";
const FS_PLANE_ADMIN_SCOPE: &str = "fs:admin";

/// Requested TTL. auth9 may clamp lower via
/// `services.db9-server.max_jwt_ttl_secs`; the cache honours
/// `expires_at` from the response, not this request.
const FS_PLANE_TTL_SECS: u64 = 900;
const FS_PLANE_ADMIN_TTL_SECS: u64 = 60;

/// HTTP request body for auth9 `POST /v1/jwt/sign`. Field names + types
/// mirror `auth9-server::api::jwt::SignBody`.
#[derive(Serialize)]
struct SignBody<'a> {
    aud: &'a str,
    ttl_secs: u64,
    claims: Value,
}

/// HTTP response body. Field names + types mirror
/// `auth9-server::api::jwt::SignResponse`. `expires_at` is RFC3339.
#[derive(Deserialize)]
struct SignResponse {
    token: String,
    expires_at: String,
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
    /// Refresh ahead of expiry. Keeps a comfortable safety margin so
    /// concurrent in-flight RPCs can't race the expiry under realistic
    /// clock skew.
    const REFRESH_LEAD: Duration = Duration::from_secs(60);

    pub fn is_fresh(&self) -> bool {
        let now = std::time::SystemTime::now();
        match self.expires_at.duration_since(now) {
            Ok(remaining) => remaining > Self::REFRESH_LEAD,
            Err(_) => false,
        }
    }
}

/// Configuration for talking to auth9's sign endpoint. Resolved once at
/// process start; nothing about it is per-tenant.
#[derive(Debug, Clone)]
pub(crate) struct Auth9MintConfig {
    pub sign_url: String,
    pub api_key: String,
}

impl Auth9MintConfig {
    /// Build from explicit values. `init_juicefs_backend` reads the env
    /// vars itself (so it can report partial-config misconfiguration
    /// loudly) and hands the values in here.
    pub fn new(sign_url: String, api_key: String) -> Self {
        Self {
            sign_url: sign_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }
}

/// Cache key. The minted token's `scp` claim depends on both the
/// principal's identity (cache namespace) and capability (`rw` vs `r`
/// suffix), so the key must include both — otherwise a rw token
/// cached for an SD-elevated session would be handed back to a later
/// ordinary read-only call for the same role.
///
/// Concretely: `_db9_sys_readonly` normally maps to `ReadOnly`, but
/// inside a superuser-owned SECURITY DEFINER fs9 wrapper it is
/// elevated to `ReadWrite` while keeping `_db9_sys_readonly` as the
/// identity (so the audit `usr` claim still names the caller). If the
/// cache were keyed only on `(tenant, role)`, the rw token minted
/// under SD would survive in cache and be served to a later
/// non-elevated ro request until expiry, defeating fs9's scope
/// enforcement. PR #2547 review #4.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct CacheKey {
    tenant_id: String,
    role: String,
    access: Fs9Access,
}

/// Process-wide token cache. Single-flight not needed at this layer:
/// db9-cli's `refresh.rs` proves a per-key Mutex is sufficient at the
/// concurrency we expect (one in-flight mint per tenant+role).
#[derive(Default)]
pub(crate) struct Fs9PlaneTokenCache {
    inner: Mutex<std::collections::HashMap<CacheKey, Fs9PlaneToken>>,
}

impl Fs9PlaneTokenCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn lookup_fresh(
        &self,
        tenant_id: &str,
        role: &str,
        access: Fs9Access,
    ) -> Option<Fs9PlaneToken> {
        let key = CacheKey {
            tenant_id: tenant_id.to_string(),
            role: role.to_string(),
            access,
        };
        let mut guard = self.inner.lock();
        match guard.get(&key) {
            Some(entry) if entry.is_fresh() => Some(entry.clone()),
            Some(_) => {
                guard.remove(&key);
                None
            }
            None => None,
        }
    }

    fn store(&self, tenant_id: &str, role: &str, access: Fs9Access, token: Fs9PlaneToken) {
        let key = CacheKey {
            tenant_id: tenant_id.to_string(),
            role: role.to_string(),
            access,
        };
        self.inner.lock().insert(key, token);
    }
}

/// Access tier on the fs-plane volume. Decouples the *capability* a
/// caller has on fs9 from the *role name* they authenticate as.
///
/// Why split: SQL fs9 authorization (`fs9::ensure_permissions`) and
/// `DB9_BOOTSTRAP_ADMIN_USER` already let any superuser act as
/// `admin` — `postgres`, `svc_admin`, custom `CREATE ROLE ... SUPERUSER`
/// — so role name matching at the mint layer (`match role { "admin" =>
/// rw, .. }`) breaks parity with the SQL permission contract. PR #2547
/// review finding #1. The fix is to thread capability explicitly, not
/// re-derive it from a string.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub(crate) enum Fs9Access {
    ReadWrite,
    ReadOnly,
}

impl Fs9Access {
    pub(crate) fn as_scope_suffix(self) -> &'static str {
        match self {
            Fs9Access::ReadWrite => "rw",
            Fs9Access::ReadOnly => "r",
        }
    }
}

/// Identity + capability carried into fs9 backend init. `role` is
/// identity (cache key + `usr` claim); `access` is capability
/// (`scp` claim). Always construct via
/// [`fs_plane_access_for`] / [`effective_fs_plane_principal`] so the
/// two fields agree.
#[derive(Debug, Clone)]
pub(crate) struct Fs9Principal {
    pub role: String,
    pub access: Fs9Access,
}

/// Single source of truth for fs-plane access classification, used by
/// every entry point that issues an fs9 token (SQL session backend
/// init, WebSocket auth handshake). Inputs are the privilege facts the
/// auth/RBAC layer already computed:
/// - `is_superuser` — the session principal carries DB-level superuser
///   authority (true for any superuser regardless of the role's name,
///   matching SQL `fs9::ensure_permissions`).
/// - `role` — the PG role string. Used only to recognise the fixed
///   system read-only role `_db9_sys_readonly`; not used as a
///   capability tag.
///
/// Order matters: a superuser named `_db9_sys_readonly` still gets
/// `ReadWrite` (capability wins over name), and a non-superuser with
/// any other name gets `None` (fail-closed).
pub(crate) fn fs_plane_access_for(is_superuser: bool, role: &str) -> Option<Fs9Access> {
    if is_superuser {
        Some(Fs9Access::ReadWrite)
    } else if role == SYS_READONLY_ROLE {
        Some(Fs9Access::ReadOnly)
    } else {
        None
    }
}

/// Fixed system role recognized for fs-plane read-only access without
/// requiring superuser. Not a regular PG role in the user table — a
/// service-account convention shared with fs9 v2.
pub(crate) const SYS_READONLY_ROLE: &str = "_db9_sys_readonly";

/// Map `(tenant_id, access)` to the fs-plane `scp` claim. fs9's
/// interceptor parses this exact shape.
pub(crate) fn fs_plane_scope_for_access(tenant_id: &str, access: Fs9Access) -> String {
    let volume = crate::extensions::fs::jfs_volume_id(tenant_id);
    format!("fs:volume:{volume}:{}", access.as_scope_suffix())
}

/// Mint a fresh fs-plane token from auth9. Caller decides whether to
/// consult the cache first (typically via `mint_or_reuse`).
///
/// `role` is identity (used only for the `usr` claim and cache key);
/// `access` is capability (drives the `scp` claim). The two are kept
/// separate so a custom-named superuser (`postgres`, `svc_admin`, any
/// `CREATE ROLE ... SUPERUSER`) gets the same `rw` token a session
/// authenticated as `admin` would.
async fn mint(
    cfg: &Auth9MintConfig,
    tenant_id: &str,
    role: &str,
    access: Fs9Access,
) -> Result<Fs9PlaneToken> {
    let claims = fs_plane_claims(tenant_id, role, access);
    mint_with_claims(cfg, FS_PLANE_AUDIENCE, FS_PLANE_TTL_SECS, claims).await
}

fn fs_plane_claims(tenant_id: &str, role: &str, access: Fs9Access) -> Value {
    json!({
        "tid": tenant_id,
        "usr": format!("{tenant_id}.{role}"),
        "scp": fs_plane_scope_for_access(tenant_id, access),
    })
}

fn fs_plane_admin_claims(tenant_id: &str) -> Value {
    json!({
        "tid": tenant_id,
        "usr": format!("{tenant_id}.admin"),
        "scp": FS_PLANE_ADMIN_SCOPE,
        "sub": "db9-server",
    })
}

/// Mint a short-lived fs-plane-admin token for server-internal fs9 admin RPCs.
///
/// This is intentionally not cached: admin calls are rare and the narrower
/// replay window matters more than avoiding one auth9 round trip.
pub(crate) async fn mint_fs_plane_admin_token(
    cfg: &Auth9MintConfig,
    tenant_id: &str,
) -> Result<Fs9PlaneToken> {
    mint_with_claims(
        cfg,
        FS_PLANE_ADMIN_AUDIENCE,
        FS_PLANE_ADMIN_TTL_SECS,
        fs_plane_admin_claims(tenant_id),
    )
    .await
}

async fn mint_with_claims(
    cfg: &Auth9MintConfig,
    aud: &'static str,
    ttl_secs: u64,
    claims: Value,
) -> Result<Fs9PlaneToken> {
    let body = SignBody {
        aud,
        ttl_secs,
        claims,
    };

    let resp = crate::auth::http_client()
        .post(&cfg.sign_url)
        .header("X-API-Key", &cfg.api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("fs9: sign request to {}: {e}", cfg.sign_url))?;
    let status = resp.status();
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        // Don't echo the body verbatim — it may include caller-supplied
        // identifiers which are fine, but defense in depth keeps the
        // error small. The status alone is the actionable signal.
        return Err(anyhow!("{}", sign_failure_message(status, &text, aud)));
    }
    let payload: SignResponse = resp
        .json()
        .await
        .map_err(|e| anyhow!("fs9: parse auth9 sign response: {e}"))?;
    let expires_at = parse_rfc3339_to_systemtime(&payload.expires_at)?;
    Ok(Fs9PlaneToken {
        token: Arc::from(payload.token.as_str()),
        expires_at,
    })
}

/// Parse auth9's RFC3339 `expires_at` into `SystemTime`. Returns
/// `UNIX_EPOCH` when the timestamp is at or before the epoch — the
/// freshness check (`is_fresh`) will then immediately classify the
/// token as stale, which is the right behavior for a malformed /
/// negative expiry.
fn parse_rfc3339_to_systemtime(raw: &str) -> Result<std::time::SystemTime> {
    let parsed = chrono::DateTime::parse_from_rfc3339(raw)
        .map_err(|e| anyhow!("fs9: parse auth9 expires_at='{raw}' as RFC3339: {e}"))?;
    let unix = parsed.timestamp();
    Ok(std::time::UNIX_EPOCH + Duration::from_secs(unix.max(0) as u64))
}

/// Truncate long server-side error strings before logging / propagation.
///
/// `max` is a byte ceiling. `&s[..max]` would panic when byte `max`
/// falls inside a UTF-8 multibyte character — easy to hit if auth9
/// returns a localized error body with non-ASCII content. Round down
/// to the nearest char boundary so the slice is always valid.
fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

fn sign_failure_message(status: reqwest::StatusCode, body: &str, aud: &str) -> String {
    let mut message = format!(
        "fs9: auth9 /v1/jwt/sign for aud=\"{aud}\" returned {status}: {}",
        truncate(body, 200)
    );
    if status == reqwest::StatusCode::FORBIDDEN
        && (aud == FS_PLANE_AUDIENCE || aud == FS_PLANE_ADMIN_AUDIENCE)
    {
        message.push_str(
            "; ensure auth9 services.db9-server.allowed_audiences includes \
             [\"fs-plane\", \"fs-plane-admin\"]",
        );
    }
    message
}

/// Lookup-then-mint with cache. Single entry point for callers (gRPC
/// interceptor) so the cache discipline lives in one place. Every
/// caller for the same `(tenant_id, role, access)` shares a token
/// under the same volume + scope binding.
///
/// The cache key must include `access`, not just `(tenant, role)`:
/// SECURITY DEFINER elevation can hand the SAME role two distinct
/// capabilities in the same process lifetime (e.g. `_db9_sys_readonly`
/// under an SD-superuser wrapper acts as ReadWrite, but as itself
/// otherwise). Without `access` in the key, the rw token minted under
/// SD would be served to a later ro request until expiry, leaking
/// capability across the boundary fs9's scope enforcement relies on.
/// PR #2547 review #4.
pub(crate) async fn mint_or_reuse(
    cache: &Fs9PlaneTokenCache,
    cfg: &Auth9MintConfig,
    tenant_id: &str,
    role: &str,
    access: Fs9Access,
) -> Result<Fs9PlaneToken> {
    if let Some(hit) = cache.lookup_fresh(tenant_id, role, access) {
        return Ok(hit);
    }
    let fresh = mint(cfg, tenant_id, role, access).await?;
    cache.store(tenant_id, role, access, fresh.clone());
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
        assert!(cache
            .lookup_fresh("t1", "admin", Fs9Access::ReadWrite)
            .is_none());
    }

    #[test]
    fn cache_returns_fresh_entry_and_drops_stale_on_lookup() {
        let cache = Fs9PlaneTokenCache::new();
        cache.store("t1", "admin", Fs9Access::ReadWrite, make_token(300));
        assert!(cache
            .lookup_fresh("t1", "admin", Fs9Access::ReadWrite)
            .is_some());

        cache.store("t1", "admin", Fs9Access::ReadWrite, make_token(-5));
        assert!(cache
            .lookup_fresh("t1", "admin", Fs9Access::ReadWrite)
            .is_none());
    }

    #[test]
    fn cache_segregates_by_role() {
        let cache = Fs9PlaneTokenCache::new();
        cache.store("t1", "admin", Fs9Access::ReadWrite, make_token(300));
        assert!(cache
            .lookup_fresh("t1", "admin", Fs9Access::ReadWrite)
            .is_some());
        // Readonly role has no entry yet — must not borrow admin's token.
        assert!(cache
            .lookup_fresh("t1", "_db9_sys_readonly", Fs9Access::ReadOnly)
            .is_none());
    }

    #[test]
    fn cache_segregates_by_tenant() {
        let cache = Fs9PlaneTokenCache::new();
        cache.store("t1", "admin", Fs9Access::ReadWrite, make_token(300));
        assert!(cache
            .lookup_fresh("t2", "admin", Fs9Access::ReadWrite)
            .is_none());
    }

    /// PR #2547 review #4 regression. SECURITY DEFINER elevation can
    /// hand the SAME (tenant, role) two access tiers: under SD-superuser
    /// wrapper `_db9_sys_readonly` resolves to ReadWrite; outside SD
    /// the same role resolves to ReadOnly. The cache MUST hold those
    /// as separate entries so the rw token can never be served to a
    /// later ro request.
    #[test]
    fn cache_segregates_by_access_after_sd_elevation() {
        let cache = Fs9PlaneTokenCache::new();
        // SD elevation path mints rw for an identity that is normally ro.
        let rw_token = make_token(300);
        cache.store("t1", "_db9_sys_readonly", Fs9Access::ReadWrite, rw_token);

        // A later non-elevated request for the same identity asking for
        // ro must NOT receive the rw token.
        assert!(
            cache
                .lookup_fresh("t1", "_db9_sys_readonly", Fs9Access::ReadOnly)
                .is_none(),
            "ro lookup must not see SD-elevated rw token"
        );

        // The rw entry is still cached for any later SD-elevated call
        // — the segregation is symmetric, not destructive.
        assert!(cache
            .lookup_fresh("t1", "_db9_sys_readonly", Fs9Access::ReadWrite)
            .is_some());
    }

    /// Symmetric to the SD regression: cache same role with both
    /// tiers independently and confirm each lookup returns its own.
    #[test]
    fn cache_can_hold_both_access_tiers_for_one_role() {
        let cache = Fs9PlaneTokenCache::new();
        cache.store("t1", "alice", Fs9Access::ReadWrite, make_token(300));
        cache.store("t1", "alice", Fs9Access::ReadOnly, make_token(300));
        assert!(cache
            .lookup_fresh("t1", "alice", Fs9Access::ReadWrite)
            .is_some());
        assert!(cache
            .lookup_fresh("t1", "alice", Fs9Access::ReadOnly)
            .is_some());
    }

    #[test]
    fn mint_config_new_trims_trailing_slash() {
        let cfg =
            Auth9MintConfig::new("https://auth9.example/v1/jwt/sign/".into(), "secret".into());
        assert_eq!(cfg.sign_url, "https://auth9.example/v1/jwt/sign");
        assert_eq!(cfg.api_key, "secret");
    }

    #[test]
    fn scope_for_rw_access_emits_rw_suffix() {
        let scp = super::fs_plane_scope_for_access("tenant_abc", Fs9Access::ReadWrite);
        assert_eq!(scp, "fs:volume:jfs_t_tenant_abc:rw");
    }

    #[test]
    fn scope_for_readonly_access_emits_r_suffix() {
        let scp = super::fs_plane_scope_for_access("tenant_abc", Fs9Access::ReadOnly);
        assert_eq!(scp, "fs:volume:jfs_t_tenant_abc:r");
    }

    #[test]
    fn data_plane_claims_include_tenant_user_and_scope() {
        let claims = super::fs_plane_claims("tenant_abc", "alice", Fs9Access::ReadWrite);
        assert_eq!(claims["tid"], "tenant_abc");
        assert_eq!(claims["usr"], "tenant_abc.alice");
        assert_eq!(claims["scp"], "fs:volume:jfs_t_tenant_abc:rw");
        assert!(claims.get("sub").is_none());
    }

    #[test]
    fn admin_claims_use_fixed_admin_scope() {
        let claims = super::fs_plane_admin_claims("tenant_abc");
        assert_eq!(claims["tid"], "tenant_abc");
        assert_eq!(claims["usr"], "tenant_abc.admin");
        assert_eq!(claims["scp"], "fs:admin");
        assert_eq!(claims["sub"], "db9-server");
    }

    /// PR #2547 review #1 regression: a deployment bootstrapped via
    /// `DB9_BOOTSTRAP_ADMIN_USER=postgres` (or `svc_admin`, or any
    /// custom `CREATE ROLE ... SUPERUSER`) must reach the same `rw`
    /// fs-plane scope a session named `admin` would. Previously the
    /// mint layer string-matched the role name and rejected anything
    /// not literally `admin`, which broke parity with SQL fs9 perms
    /// (which only check `is_superuser`).
    #[test]
    fn access_for_custom_named_superusers_is_rw() {
        for role in ["admin", "postgres", "svc_admin", "alice", ""] {
            assert_eq!(
                super::fs_plane_access_for(true, role),
                Some(Fs9Access::ReadWrite),
                "superuser named {role:?} should get rw"
            );
        }
    }

    #[test]
    fn access_for_sys_readonly_is_r() {
        assert_eq!(
            super::fs_plane_access_for(false, super::SYS_READONLY_ROLE),
            Some(Fs9Access::ReadOnly)
        );
    }

    #[test]
    fn access_for_non_superuser_other_role_is_rejected() {
        for role in ["guest", "alice", "postgres", ""] {
            assert_eq!(
                super::fs_plane_access_for(false, role),
                None,
                "non-superuser named {role:?} should be rejected"
            );
        }
    }

    #[test]
    fn access_superuser_named_readonly_still_gets_rw() {
        // Capability wins over name. A superuser who happens to be
        // named `_db9_sys_readonly` is still a superuser.
        assert_eq!(
            super::fs_plane_access_for(true, super::SYS_READONLY_ROLE),
            Some(Fs9Access::ReadWrite)
        );
    }

    #[test]
    fn truncate_under_limit_returns_input() {
        assert_eq!(super::truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_at_char_boundary_appends_ellipsis() {
        // Pure ASCII: byte boundary == char boundary, no rounding.
        assert_eq!(super::truncate("hello world", 5), "hello…");
    }

    /// PR #2547 review #2 regression: a long UTF-8 auth9 error body
    /// must not panic `truncate`. Byte 5 falls in the middle of `好`
    /// (3 bytes); the function should round down to byte 3.
    #[test]
    fn truncate_non_ascii_rounds_to_char_boundary() {
        let s = "你好世界"; // 12 bytes, 4 chars
        let out = super::truncate(s, 5);
        // Boundary rounded down to 3, capturing only `你`.
        assert_eq!(out, "你…");
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn truncate_handles_long_non_ascii_body() {
        // Simulates a localized auth9 error body large enough to
        // require slicing inside a multibyte sequence.
        let s = "中国人".repeat(200); // 1800 bytes
        let out = super::truncate(&s, 200);
        // Function must not panic and must produce a valid &str.
        assert!(out.starts_with('中'));
        assert!(out.ends_with('…'));
    }

    #[test]
    fn forbidden_sign_error_names_audience_allowlist_prereq() {
        let msg = super::sign_failure_message(
            reqwest::StatusCode::FORBIDDEN,
            "audience_not_allowed",
            super::FS_PLANE_ADMIN_AUDIENCE,
        );
        assert!(msg.contains("aud=\"fs-plane-admin\""), "{msg}");
        assert!(
            msg.contains("services.db9-server.allowed_audiences"),
            "{msg}"
        );
        assert!(msg.contains("\"fs-plane\""), "{msg}");
        assert!(msg.contains("\"fs-plane-admin\""), "{msg}");
    }

    #[test]
    fn parse_rfc3339_z_suffix() {
        let t = super::parse_rfc3339_to_systemtime("2026-05-13T09:14:34Z").unwrap();
        let unix = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(unix, 1_778_663_674);
    }

    #[test]
    fn parse_rfc3339_with_offset_and_fraction() {
        let t = super::parse_rfc3339_to_systemtime("2026-05-13T09:14:34.123+00:00").unwrap();
        let unix = t.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(unix, 1_778_663_674);
    }

    #[test]
    fn parse_rfc3339_rejects_unix_seconds_as_number() {
        let err = super::parse_rfc3339_to_systemtime("1778663674").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("RFC3339"), "expected RFC3339 in {msg}");
    }

    #[test]
    fn parse_rfc3339_negative_epoch_clamps_to_epoch() {
        let t = super::parse_rfc3339_to_systemtime("1900-01-01T00:00:00Z").unwrap();
        assert_eq!(t, std::time::UNIX_EPOCH);
    }
}
