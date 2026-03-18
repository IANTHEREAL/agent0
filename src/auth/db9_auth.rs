use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::config;
use anyhow::Result as AnyhowResult;
use futures::future::{BoxFuture, FutureExt, Shared};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tikv_client::Transaction;
use tokio::sync::Mutex;

use super::{AuthManager, User};

const KEYSPACE_PREFIX: &str = "db9_tenant_";
const DEFAULT_AUDIENCE: &str = "db9-server";
const JWKS_CACHE_TTL: Duration = Duration::from_secs(60);
const JWKS_UNKNOWN_KID_COOLDOWN: Duration = Duration::from_secs(10);
const JWKS_MISSING_SELECTOR_CACHE_LIMIT: usize = 64;
const DEFAULT_JWT_ALGORITHM: Algorithm = Algorithm::RS256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Db9AuthMaterialKind {
    Jwt,
    ConnectKey,
    Password,
}

pub(crate) fn classify_db9_auth_material(secret: &str) -> Db9AuthMaterialKind {
    let secret = secret.trim();
    if secret.starts_with("db9ck_") {
        return Db9AuthMaterialKind::ConnectKey;
    }
    if secret.split('.').count() == 3 && decode_header(secret).is_ok() {
        return Db9AuthMaterialKind::Jwt;
    }
    Db9AuthMaterialKind::Password
}

#[derive(Debug)]
pub(crate) enum Db9AuthDispatchFailure {
    TokenRequired,
    JwtFailed(Db9AuthError),
    JwtUserNotFound,
    ConnectKeyFailed(Db9AuthError),
    ConnectKeyUserNotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerifiedJwtClaims {
    settings: BTreeMap<String, String>,
}

impl VerifiedJwtClaims {
    pub(crate) fn iter_settings(&self) -> impl Iterator<Item = (&String, &String)> {
        self.settings.iter()
    }

    #[cfg(test)]
    fn setting(&self, name: &str) -> Option<&str> {
        self.settings.get(name).map(String::as_str)
    }
}

pub(crate) async fn dispatch_db9_auth(
    auth_manager: &AuthManager,
    txn: &mut Transaction,
    auth_mode: config::Db9AuthMode,
    keyspace: &str,
    username: &str,
    password: &str,
) -> AnyhowResult<(
    Option<User>,
    Option<VerifiedJwtClaims>,
    Option<Db9AuthDispatchFailure>,
)> {
    match auth_mode {
        config::Db9AuthMode::Password => Ok((
            auth_manager.authenticate(txn, username, password).await?,
            None,
            None,
        )),
        config::Db9AuthMode::Both | config::Db9AuthMode::Token => {
            let require_token = auth_mode == config::Db9AuthMode::Token;
            let material_kind = classify_db9_auth_material(password);
            let token_material = password.trim();

            match material_kind {
                Db9AuthMaterialKind::Password => {
                    if require_token {
                        return Ok((None, None, Some(Db9AuthDispatchFailure::TokenRequired)));
                    }

                    Ok((
                        auth_manager.authenticate(txn, username, password).await?,
                        None,
                        None,
                    ))
                }
                Db9AuthMaterialKind::Jwt => {
                    match verify_jwt_connect_token(token_material, keyspace, username).await {
                        Ok(claims) => {
                            let user = auth_manager.get_user(txn, username).await?;
                            if user.is_none() {
                                return Ok((
                                    None,
                                    None,
                                    Some(Db9AuthDispatchFailure::JwtUserNotFound),
                                ));
                            }
                            Ok((user, Some(claims), None))
                        }
                        Err(err) => Ok((None, None, Some(Db9AuthDispatchFailure::JwtFailed(err)))),
                    }
                }
                Db9AuthMaterialKind::ConnectKey => {
                    match verify_connect_key(token_material, keyspace, username).await {
                        Ok(()) => {
                            let user = auth_manager.get_user(txn, username).await?;
                            if user.is_none() {
                                return Ok((
                                    None,
                                    None,
                                    Some(Db9AuthDispatchFailure::ConnectKeyUserNotFound),
                                ));
                            }
                            Ok((user, None, None))
                        }
                        Err(err) => Ok((
                            None,
                            None,
                            Some(Db9AuthDispatchFailure::ConnectKeyFailed(err)),
                        )),
                    }
                }
            }
        }
    }
}

pub(crate) fn tenant_id_from_keyspace(keyspace: &str) -> Option<&str> {
    keyspace.strip_prefix(KEYSPACE_PREFIX)
}

#[derive(Debug, Clone, thiserror::Error)]
pub(crate) enum Db9AuthError {
    #[error("token mode requires tenant-qualified username (<tenant>.<role>)")]
    MissingTenantInUsername,

    #[error(
        "token verification is not configured (set DB9_AUTH_JWKS_URL or DB9_AUTH_JWT_PUBLIC_KEY)"
    )]
    TokenVerificationNotConfigured,

    #[error("JWT validation failed: {reason}")]
    InvalidJwt { reason: String },

    #[error("connect key validation is not configured (set DB9_AUTH_CONNECT_KEY_INTROSPECT_URL)")]
    ConnectKeyNotConfigured,

    #[error("connect key validation failed: {reason}")]
    InvalidConnectKey { reason: String },

    #[error("token tenant mismatch: expected '{expected}', got '{actual}'")]
    TenantMismatch { expected: String, actual: String },

    #[error("token role mismatch: expected '{expected}', got '{actual}'")]
    RoleMismatch { expected: String, actual: String },

    #[error("failed to fetch JWKS: {reason}")]
    JwksFetchFailed { reason: String },

    #[error("failed to parse JWKS: {reason}")]
    JwksParseFailed { reason: String },

    #[error("JWKS does not contain key for kid '{kid}'")]
    JwksKidNotFound { kid: String },

    #[error("JWT header missing kid but JWKS contains multiple keys")]
    JwksKidMissing,

    #[error("invalid public key for JWT verification: {reason}")]
    InvalidJwtPublicKey { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Db9ConnectTokenClaims {
    tid: String,
    usr: String,
    #[allow(dead_code)] // Required by JWT spec; validated by jsonwebtoken.
    exp: usize,
    #[serde(default, flatten)]
    extra: BTreeMap<String, JsonValue>,
}

impl Db9ConnectTokenClaims {
    fn into_verified_jwt_claims(self) -> Result<VerifiedJwtClaims, Db9AuthError> {
        let mut claims = self.extra;
        claims.insert("exp".to_string(), serde_json::json!(self.exp));
        claims.insert("tid".to_string(), JsonValue::from(self.tid));
        claims.insert("usr".to_string(), JsonValue::from(self.usr));

        let all_claims_json =
            serde_json::to_string(&claims).map_err(|err| Db9AuthError::InvalidJwt {
                reason: format!("validated JWT claims could not be serialized: {err}"),
            })?;

        let mut settings = BTreeMap::new();
        settings.insert("request.jwt.claims".to_string(), all_claims_json);
        for (claim_name, value) in claims {
            let Some(setting_value) = claim_value_to_setting_string(&value) else {
                continue;
            };
            if claim_name.eq_ignore_ascii_case("sub") {
                settings.insert("auth.uid".to_string(), setting_value.clone());
            }
            settings.insert(
                format!("request.jwt.claim.{}", claim_name.to_ascii_lowercase()),
                setting_value,
            );
        }

        Ok(VerifiedJwtClaims { settings })
    }
}

fn claim_value_to_setting_string(value: &JsonValue) -> Option<String> {
    Some(match value {
        JsonValue::String(s) => s.clone(),
        JsonValue::Bool(v) => v.to_string(),
        JsonValue::Number(n) => n.to_string(),
        JsonValue::Null => return None,
        JsonValue::Array(_) | JsonValue::Object(_) => serde_json::to_string(value)
            .expect("serde_json::Value from JWT claims must be serializable"),
    })
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ConnectKeyIntrospectTimeValue {
    Seconds(i64),
    String(String),
}

#[derive(Debug, Deserialize)]
struct ConnectKeyIntrospectResponse {
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    revoked: Option<bool>,
    #[serde(default)]
    expired: Option<bool>,
    #[serde(default, alias = "tenantId", alias = "tid", alias = "tenant_id")]
    tenant_id: Option<String>,
    #[serde(default, alias = "usr", alias = "user", alias = "role")]
    role: Option<String>,
    #[serde(default, alias = "expiresAt", alias = "expires_at")]
    expires_at: Option<ConnectKeyIntrospectTimeValue>,
    #[serde(default, alias = "revokedAt", alias = "revoked_at")]
    revoked_at: Option<ConnectKeyIntrospectTimeValue>,
}

#[derive(Debug, Deserialize)]
struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kty: String,
    #[serde(default)]
    kid: Option<String>,
    #[serde(default)]
    n: Option<String>,
    #[serde(default)]
    e: Option<String>,
    #[serde(default)]
    x: Option<String>,
    #[serde(default)]
    y: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum JwksSelector {
    Kid(String),
    MissingKid,
}

struct JwksCacheEntry {
    jwks_url: String,
    fetched_at: Instant,
    keys_by_kid: HashMap<String, Arc<DecodingKey>>,
    singleton_key: Option<Arc<DecodingKey>>,
    missing_selectors: HashMap<JwksSelector, Instant>,
}

type JwksRefreshFuture = Shared<BoxFuture<'static, Result<(), Db9AuthError>>>;

struct JwksCacheState {
    entry: Option<JwksCacheEntry>,
    refresh_in_flight: Option<JwksRefreshFuture>,
}

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static JWKS_CACHE: OnceLock<Mutex<JwksCacheState>> = OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("failed to build reqwest HTTP client")
    })
}

fn jwks_cache() -> &'static Mutex<JwksCacheState> {
    JWKS_CACHE.get_or_init(|| {
        Mutex::new(JwksCacheState {
            entry: None,
            refresh_in_flight: None,
        })
    })
}

fn jwks_selector_from_kid(kid: Option<String>) -> JwksSelector {
    match kid {
        Some(kid) => JwksSelector::Kid(kid),
        None => JwksSelector::MissingKid,
    }
}

fn jwks_selector_error(selector: &JwksSelector) -> Db9AuthError {
    match selector {
        JwksSelector::Kid(kid) => Db9AuthError::JwksKidNotFound { kid: kid.clone() },
        JwksSelector::MissingKid => Db9AuthError::JwksKidMissing,
    }
}

fn cached_jwks_key(
    entry: &JwksCacheEntry,
    jwks_url: &str,
    selector: &JwksSelector,
) -> Option<Arc<DecodingKey>> {
    if !jwks_entry_is_fresh(entry, jwks_url) {
        return None;
    }

    match selector {
        JwksSelector::Kid(kid) => entry.keys_by_kid.get(kid).cloned(),
        JwksSelector::MissingKid => entry.singleton_key.clone(),
    }
}

fn jwks_entry_is_fresh(entry: &JwksCacheEntry, jwks_url: &str) -> bool {
    entry.jwks_url == jwks_url && entry.fetched_at.elapsed() < JWKS_CACHE_TTL
}

fn jwks_selector_resolved(entry: &JwksCacheEntry, selector: &JwksSelector) -> bool {
    match selector {
        JwksSelector::Kid(kid) => entry.keys_by_kid.contains_key(kid),
        JwksSelector::MissingKid => entry.singleton_key.is_some(),
    }
}

fn prune_jwks_negative_cache(entry: &mut JwksCacheEntry) {
    let unresolved_selectors: Vec<JwksSelector> = entry
        .missing_selectors
        .iter()
        .filter(|(selector, recorded_at)| {
            recorded_at.elapsed() < JWKS_UNKNOWN_KID_COOLDOWN
                && !jwks_selector_resolved(entry, selector)
        })
        .map(|(selector, _)| selector.clone())
        .collect();
    entry.missing_selectors.retain(|selector, _| {
        unresolved_selectors
            .iter()
            .any(|unresolved_selector| unresolved_selector == selector)
    });
}

fn jwks_negative_cache_hit(entry: &JwksCacheEntry, selector: &JwksSelector) -> bool {
    entry
        .missing_selectors
        .get(selector)
        .is_some_and(|recorded_at| recorded_at.elapsed() < JWKS_UNKNOWN_KID_COOLDOWN)
}

fn record_missing_selector(
    entry: &mut JwksCacheEntry,
    selector: JwksSelector,
    recorded_at: Instant,
) {
    prune_jwks_negative_cache(entry);
    if jwks_selector_resolved(entry, &selector) {
        entry.missing_selectors.remove(&selector);
        return;
    }
    entry.missing_selectors.insert(selector, recorded_at);
    while entry.missing_selectors.len() > JWKS_MISSING_SELECTOR_CACHE_LIMIT {
        let Some(oldest_selector) = entry
            .missing_selectors
            .iter()
            .min_by_key(|(_, at)| *at)
            .map(|(selector, _)| selector.clone())
        else {
            break;
        };
        entry.missing_selectors.remove(&oldest_selector);
    }
}

fn build_refreshed_jwks_entry(
    previous_entry: Option<&JwksCacheEntry>,
    jwks_url: String,
    fetched_at: Instant,
    keys_by_kid: HashMap<String, Arc<DecodingKey>>,
    singleton_key: Option<Arc<DecodingKey>>,
) -> JwksCacheEntry {
    let mut entry = JwksCacheEntry {
        jwks_url: jwks_url.clone(),
        fetched_at,
        keys_by_kid,
        singleton_key,
        missing_selectors: HashMap::new(),
    };

    if let Some(previous_entry) = previous_entry {
        if previous_entry.jwks_url == jwks_url {
            for (previous_selector, recorded_at) in &previous_entry.missing_selectors {
                if recorded_at.elapsed() < JWKS_UNKNOWN_KID_COOLDOWN
                    && !jwks_selector_resolved(&entry, previous_selector)
                {
                    record_missing_selector(&mut entry, previous_selector.clone(), *recorded_at);
                }
            }
        }
    }

    entry
}

fn start_jwks_refresh(jwks_url: String) -> JwksRefreshFuture {
    async move {
        let refresh_result = fetch_and_parse_jwks(&jwks_url).await;
        let refresh_at = Instant::now();
        let mut cache = jwks_cache().lock().await;

        let result = match refresh_result {
            Ok((keys_by_kid, singleton_key)) => {
                let entry = build_refreshed_jwks_entry(
                    cache.entry.as_ref(),
                    jwks_url.clone(),
                    refresh_at,
                    keys_by_kid,
                    singleton_key,
                );
                cache.entry = Some(entry);
                Ok(())
            }
            Err(err) => Err(err),
        };

        cache.refresh_in_flight = None;
        result
    }
    .boxed()
    .shared()
}

fn validate_connect_key_introspection(
    info: &ConnectKeyIntrospectResponse,
    tenant_id: &str,
    expected_role: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), Db9AuthError> {
    if matches!(info.active, Some(false)) {
        return Err(Db9AuthError::InvalidConnectKey {
            reason: "inactive".to_string(),
        });
    }

    if matches!(info.revoked, Some(true)) {
        return Err(Db9AuthError::InvalidConnectKey {
            reason: "revoked".to_string(),
        });
    }

    if matches!(info.expired, Some(true)) {
        return Err(Db9AuthError::InvalidConnectKey {
            reason: "expired".to_string(),
        });
    }

    if info.revoked_at.is_some() {
        return Err(Db9AuthError::InvalidConnectKey {
            reason: "revoked".to_string(),
        });
    }

    if let Some(expires_at) = &info.expires_at {
        let is_expired = match expires_at {
            ConnectKeyIntrospectTimeValue::Seconds(ts) => *ts <= now.timestamp(),
            ConnectKeyIntrospectTimeValue::String(raw) => {
                chrono::DateTime::parse_from_rfc3339(raw).map_err(|err| {
                    Db9AuthError::InvalidConnectKey {
                        reason: format!("invalid expires_at: {err}"),
                    }
                })? < now
            }
        };
        if is_expired {
            return Err(Db9AuthError::InvalidConnectKey {
                reason: "expired".to_string(),
            });
        }
    }

    let actual_tenant_id =
        info.tenant_id
            .as_deref()
            .ok_or_else(|| Db9AuthError::InvalidConnectKey {
                reason: "missing tenant_id".to_string(),
            })?;
    let actual_role = info
        .role
        .as_deref()
        .ok_or_else(|| Db9AuthError::InvalidConnectKey {
            reason: "missing role".to_string(),
        })?;

    if actual_tenant_id != tenant_id {
        return Err(Db9AuthError::TenantMismatch {
            expected: tenant_id.to_string(),
            actual: actual_tenant_id.to_string(),
        });
    }
    if actual_role != expected_role {
        return Err(Db9AuthError::RoleMismatch {
            expected: expected_role.to_string(),
            actual: actual_role.to_string(),
        });
    }

    Ok(())
}

pub(crate) async fn verify_jwt_connect_token(
    token: &str,
    expected_keyspace: &str,
    expected_role: &str,
) -> Result<VerifiedJwtClaims, Db9AuthError> {
    let tenant_id =
        tenant_id_from_keyspace(expected_keyspace).ok_or(Db9AuthError::MissingTenantInUsername)?;
    let key = jwt_decoding_key(token).await?;

    let issuer = config::env_string("DB9_AUTH_ISSUER");
    let audience =
        config::env_string("DB9_AUTH_AUDIENCE").unwrap_or_else(|| DEFAULT_AUDIENCE.to_string());

    let algorithms = jwt_algorithms_from_env();
    let mut validation =
        Validation::new(algorithms.first().copied().unwrap_or(DEFAULT_JWT_ALGORITHM));
    validation.algorithms = algorithms;
    validation.set_audience(&[audience]);
    if let Some(iss) = issuer.as_deref() {
        validation.set_issuer(&[iss]);
    }

    let data =
        decode::<Db9ConnectTokenClaims>(token, key.as_ref(), &validation).map_err(|err| {
            Db9AuthError::InvalidJwt {
                reason: err.to_string(),
            }
        })?;

    let claims = data.claims;
    if claims.tid != tenant_id {
        return Err(Db9AuthError::TenantMismatch {
            expected: tenant_id.to_string(),
            actual: claims.tid,
        });
    }
    // The `usr` claim may be in "{tenant_id}.{role}" format (as generated by
    // db9-backend's connect token service) or plain "{role}" format.  Extract
    // the role part and, when the compound format is used, verify the embedded
    // tenant_id matches the already-validated `claims.tid`.
    //
    // Split on the FIRST '.' to match `parse_tenant_username()` in tenant.rs,
    // which uses `find('.')` (first occurrence).  This ensures that roles
    // containing dots (e.g., "user.name") are preserved intact.
    let (embedded_tid, actual_role) = match claims.usr.find('.') {
        Some(pos) if pos > 0 && pos < claims.usr.len() - 1 => {
            (Some(&claims.usr[..pos]), &claims.usr[pos + 1..])
        }
        _ => (None, claims.usr.as_str()),
    };
    if let Some(embedded_tid) = embedded_tid {
        if embedded_tid != tenant_id {
            return Err(Db9AuthError::TenantMismatch {
                expected: tenant_id.to_string(),
                actual: embedded_tid.to_string(),
            });
        }
    }
    if actual_role != expected_role {
        return Err(Db9AuthError::RoleMismatch {
            expected: expected_role.to_string(),
            actual: claims.usr,
        });
    }

    claims.into_verified_jwt_claims()
}

pub(crate) async fn verify_connect_key(
    connect_key: &str,
    expected_keyspace: &str,
    expected_role: &str,
) -> Result<(), Db9AuthError> {
    let tenant_id =
        tenant_id_from_keyspace(expected_keyspace).ok_or(Db9AuthError::MissingTenantInUsername)?;
    let introspect_url = config::env_string("DB9_AUTH_CONNECT_KEY_INTROSPECT_URL")
        .ok_or(Db9AuthError::ConnectKeyNotConfigured)?;
    let api_key = config::env_string("DB9_AUTH_CONNECT_KEY_INTROSPECT_API_KEY");

    #[derive(serde::Serialize)]
    struct IntrospectRequest<'a> {
        // Some backends expect `key`, others `connect_key`; send both for compatibility.
        connect_key: &'a str,
        key: &'a str,
    }

    let mut req = http_client()
        .post(&introspect_url)
        .json(&IntrospectRequest {
            connect_key,
            key: connect_key,
        });
    if let Some(api_key) = api_key.as_deref() {
        req = req.header("X-API-Key", api_key);
    }

    let resp = req
        .send()
        .await
        .map_err(|err| Db9AuthError::InvalidConnectKey {
            reason: format!("introspection request failed: {err}"),
        })?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|err| Db9AuthError::InvalidConnectKey {
            reason: format!("introspection response read failed: {err}"),
        })?;
    if !status.is_success() {
        let status_code = status.as_u16();
        return Err(Db9AuthError::InvalidConnectKey {
            reason: format!("introspection HTTP {status_code}"),
        });
    }

    let info: ConnectKeyIntrospectResponse =
        serde_json::from_str(&body).map_err(|err| Db9AuthError::InvalidConnectKey {
            reason: format!("introspection parse failed: {err}"),
        })?;

    validate_connect_key_introspection(&info, tenant_id, expected_role, chrono::Utc::now())?;

    Ok(())
}

async fn jwt_decoding_key(token: &str) -> Result<Arc<DecodingKey>, Db9AuthError> {
    if let Some(jwks_url) = config::env_string("DB9_AUTH_JWKS_URL") {
        return jwks_decoding_key(token, &jwks_url).await;
    }

    let raw_pem = config::env_string("DB9_AUTH_JWT_PUBLIC_KEY")
        .ok_or(Db9AuthError::TokenVerificationNotConfigured)?;
    let pem = normalize_pem(&raw_pem);

    let key = DecodingKey::from_rsa_pem(pem.as_bytes()).map_err(|err| {
        Db9AuthError::InvalidJwtPublicKey {
            reason: err.to_string(),
        }
    })?;
    Ok(Arc::new(key))
}

async fn jwks_decoding_key(token: &str, jwks_url: &str) -> Result<Arc<DecodingKey>, Db9AuthError> {
    let header = decode_header(token).map_err(|err| Db9AuthError::InvalidJwt {
        reason: err.to_string(),
    })?;
    let selector = jwks_selector_from_kid(header.kid);
    let mut refreshed = false;

    loop {
        let refresh = {
            let mut cache = jwks_cache().lock().await;

            if let Some(entry) = cache.entry.as_mut() {
                prune_jwks_negative_cache(entry);
                if let Some(key) = cached_jwks_key(entry, jwks_url, &selector) {
                    return Ok(key);
                }
                if jwks_entry_is_fresh(entry, jwks_url) {
                    if jwks_negative_cache_hit(entry, &selector) {
                        return Err(jwks_selector_error(&selector));
                    }
                    if refreshed {
                        record_missing_selector(entry, selector.clone(), Instant::now());
                        return Err(jwks_selector_error(&selector));
                    }
                }
            }

            if let Some(refresh) = cache.refresh_in_flight.as_ref() {
                refresh.clone()
            } else {
                let refresh = start_jwks_refresh(jwks_url.to_string());
                cache.refresh_in_flight = Some(refresh.clone());
                refresh
            }
        };

        refresh.await?;
        refreshed = true;
    }
}

async fn fetch_and_parse_jwks(
    jwks_url: &str,
) -> Result<(HashMap<String, Arc<DecodingKey>>, Option<Arc<DecodingKey>>), Db9AuthError> {
    let resp =
        http_client()
            .get(jwks_url)
            .send()
            .await
            .map_err(|err| Db9AuthError::JwksFetchFailed {
                reason: err.to_string(),
            })?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|err| Db9AuthError::JwksFetchFailed {
            reason: err.to_string(),
        })?;
    if !status.is_success() {
        let status_code = status.as_u16();
        return Err(Db9AuthError::JwksFetchFailed {
            reason: format!("HTTP {status_code} from JWKS endpoint"),
        });
    }

    let jwks: Jwks = serde_json::from_str(&body).map_err(|err| Db9AuthError::JwksParseFailed {
        reason: err.to_string(),
    })?;

    let mut keys_by_kid = HashMap::new();
    let mut singleton_candidate: Option<Arc<DecodingKey>> = None;
    let mut usable_key_count: usize = 0;
    for key in jwks.keys {
        let decoding_key = match key.kty.as_str() {
            "RSA" => {
                let (Some(n), Some(e)) = (key.n, key.e) else {
                    continue;
                };
                Arc::new(DecodingKey::from_rsa_components(&n, &e).map_err(|err| {
                    Db9AuthError::JwksParseFailed {
                        reason: err.to_string(),
                    }
                })?)
            }
            "EC" => {
                let (Some(x), Some(y)) = (key.x, key.y) else {
                    continue;
                };
                Arc::new(DecodingKey::from_ec_components(&x, &y).map_err(|err| {
                    Db9AuthError::JwksParseFailed {
                        reason: err.to_string(),
                    }
                })?)
            }
            "OKP" => {
                let Some(x) = key.x else {
                    continue;
                };
                Arc::new(DecodingKey::from_ed_components(&x).map_err(|err| {
                    Db9AuthError::JwksParseFailed {
                        reason: err.to_string(),
                    }
                })?)
            }
            _ => continue,
        };
        usable_key_count += 1;
        if let Some(kid) = key.kid {
            keys_by_kid.insert(kid, decoding_key);
        } else if singleton_candidate.is_none() {
            singleton_candidate = Some(decoding_key);
        }
    }

    let singleton = if usable_key_count == 1 {
        singleton_candidate.or_else(|| keys_by_kid.values().next().cloned())
    } else {
        None
    };

    Ok((keys_by_kid, singleton))
}

fn normalize_pem(raw: &str) -> String {
    if raw.contains("\\n") && !raw.contains('\n') {
        raw.replace("\\n", "\n")
    } else {
        raw.to_string()
    }
}

fn jwt_algorithms_from_env() -> Vec<Algorithm> {
    let Some(raw) = config::env_string("DB9_AUTH_JWT_ALGORITHM") else {
        return vec![DEFAULT_JWT_ALGORITHM];
    };

    let mut algorithms = Vec::new();
    for part in raw.split(',') {
        let candidate = part.trim();
        if candidate.is_empty() {
            continue;
        }

        let parsed = match candidate.to_ascii_uppercase().parse::<Algorithm>() {
            Ok(alg) => alg,
            Err(_) => {
                tracing::warn!(
                    "Invalid DB9_AUTH_JWT_ALGORITHM value '{candidate}', falling back to '{DEFAULT_JWT_ALGORITHM:?}'"
                );
                return vec![DEFAULT_JWT_ALGORITHM];
            }
        };

        if matches!(
            parsed,
            Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
        ) {
            tracing::warn!(
                "Unsupported DB9_AUTH_JWT_ALGORITHM value '{candidate}' (HMAC is not allowed), falling back to '{DEFAULT_JWT_ALGORITHM:?}'"
            );
            return vec![DEFAULT_JWT_ALGORITHM];
        }

        if !algorithms.contains(&parsed) {
            algorithms.push(parsed);
        }
    }

    if algorithms.is_empty() {
        tracing::warn!(
            "Empty DB9_AUTH_JWT_ALGORITHM value '{raw}', falling back to '{DEFAULT_JWT_ALGORITHM:?}'"
        );
        vec![DEFAULT_JWT_ALGORITHM]
    } else {
        algorithms
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use jsonwebtoken::{encode, EncodingKey, Header};
    use serde::Serialize;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::sync::OnceLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Notify;
    use tokio::time::timeout;

    fn test_lock() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    struct EnvVarGuard {
        key: &'static str,
        prev: Option<String>,
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(prev) = &self.prev {
                std::env::set_var(self.key, prev);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    fn set_env(key: &'static str, value: &str) -> EnvVarGuard {
        let prev = std::env::var(key).ok();
        std::env::set_var(key, value);
        EnvVarGuard { key, prev }
    }

    async fn clear_jwks_cache() {
        *jwks_cache().lock().await = JwksCacheState {
            entry: None,
            refresh_in_flight: None,
        };
    }

    async fn wait_for_request_count(counter: &Arc<AtomicUsize>, expected: usize) {
        timeout(Duration::from_secs(1), async {
            loop {
                if counter.load(Ordering::SeqCst) >= expected {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    async fn start_jwks_server(jwks_body: String) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;

                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    jwks_body.len(),
                    jwks_body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        (format!("http://{addr}/jwks"), handle)
    }

    const TEST_RSA_PRIVATE_KEY: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCvjVk3qWFad3bQ
HsXmiT5i6g3SEDk+VwmfOgNEwBwW/xjpMog9K8RPe3b7S4XSDh8vmDOh20flJQs+
T4QahMtaD75nsG7a3uJAQvBxAsNdlF6r/9swga07/gXl/UaIYYbym7DGNitvOPL3
QfCzcR3yO0ZKGBVfVbYjtQNPc3WbJvPhHWZV+8icY5v6yL9Y0p8p8RxndFdoHHdI
oIJqkSGy6vJ26iHZA5MJ+kTN1AzW0K/yLSll/4+4HWxYQy48RXNKqh6/Kn5HPJ0c
aZKmOHnrWBIisAeION+5lwj6n9HQvMzPJK46TEAMvTevnLWjnYUboKTf6au4+Whv
7+X13REnAgMBAAECggEAA2Qlnw+kk8zO/MI7bHKmQ97lmXM6x9uCkhLa0U8su7z9
zDNvsk7QIgDukXgqA57GN3MnPC8yOlj22KNMl/6MtxaqxPIBkjTQBhHE90noYDxn
f8cXgt5ebFRB5Ol5nVTU+IbNaWbOe/2Lo/8gGTdMLsu6VeAVOZw8QoBSqgw+71pP
EjdjUUNAayE1om/86QlmtK1+9uci7Jam+8Kvy527lIjCQdwR8kT0Kv8AM89EuAGp
FhdnF146YVBYTzR21drUERNh2oCaTzRdRrTYGZICH7qLnhufojI7Qp6QODgr5U1r
Y8UGyC7XpCh4cklP0/FZA0AXqVcY8h0TdMs28ZYPMQKBgQDizkZKKIRW9DPxhZkC
Uy2pju70LhKtLBtBoCDzkZyIVnJtVeQnRpDwMTr5eDFojDWFpwf8VuIpE78QybW+
5rIllszKaOE+B+W8P+zIIKjI21Ag3KOazwrmAq5lplCvT3Rhy36CDpjZSC7LYVqT
Jw1damg8YA9sJdcoA6TkR4ij7QKBgQDGJijl2nHYtTP1wHxOaEK8kNtno1HtbJ8F
csbTicit7s2xfZHy0cKfnxUxsSRa1T8j33g/gbRNQYrrn2G/wAG6G4IlcoDlsg8y
viGh/t3Oo4KdbQbQpwIA0nH46KdGmuZoWOX1u8Bg51BsjpxMSvxY6TYzonq/IK11
1U6zD5fO4wKBgG2QLf5nAj8rKuiSnC62VcmiJabJlvYW53fVTfW7sr1d3VsZ8eRT
P3L4pT+cI2oYyUYuQTpSEmC7jEIk3upAcXCdH4LsFVss33sH+m9W75JP965YR6Ri
PiaMxwiNxk5Z+KPBdPSI7qeQKiLPfby2Uct9uqrn0KtywDQxRneMYuKlAoGAI+aS
DmMvsVXTXjlLzGDzhnqwZeyfUWcWwMP05irWozzbI8dehCIhIw6Npn0z2wk78WHx
xX/YjQ7M/rfX3AgLyA5n3CUM2ZETU9xC97jXszLI3YD9dRxtLnzyjWiJti8mg81n
jMhBqM0AM0r7Yo9LfUhzu5M6rhpbkzfclHDEzoUCgYAeiCs36dfsvLN63aXtyViw
1KuUAX7ZOH/OE9AW4LXZ0SPV5xhaodEm+MiQD+0T8nI3kJ2aPLf8xct6wiEPDkRF
734vA43+3iME3SGkkH3Zucz0Xu5zg8OtM78XVW5WmBYZrxCcDnLmu8QJRhhzUI/a
KXdAckHRwyP3Ce69EGCPqw==
-----END PRIVATE KEY-----"#;

    const TEST_RSA_PUBLIC_KEY: &str = r#"-----BEGIN PUBLIC KEY-----
MIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEAr41ZN6lhWnd20B7F5ok+
YuoN0hA5PlcJnzoDRMAcFv8Y6TKIPSvET3t2+0uF0g4fL5gzodtH5SULPk+EGoTL
Wg++Z7Bu2t7iQELwcQLDXZReq//bMIGtO/4F5f1GiGGG8puwxjYrbzjy90Hws3Ed
8jtGShgVX1W2I7UDT3N1mybz4R1mVfvInGOb+si/WNKfKfEcZ3RXaBx3SKCCapEh
suryduoh2QOTCfpEzdQM1tCv8i0pZf+PuB1sWEMuPEVzSqoevyp+RzydHGmSpjh5
61gSIrAHiDjfuZcI+p/R0LzMzySuOkxADL03r5y1o52FG6Ck3+mruPlob+/l9d0R
JwIDAQAB
-----END PUBLIC KEY-----"#;

    #[derive(Debug, Serialize)]
    struct Claims<'a> {
        iss: &'a str,
        aud: &'a str,
        tid: &'a str,
        usr: &'a str,
        sub: &'a str,
        email_verified: bool,
        roles: Vec<&'a str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        nullable: Option<&'a str>,
        exp: usize,
    }

    fn test_claims(exp: usize) -> Claims<'static> {
        Claims {
            iss: "https://issuer.example",
            aud: "db9-server",
            tid: "t1",
            usr: "admin",
            sub: "auth0|admin-user",
            email_verified: true,
            roles: vec!["admin"],
            nullable: None,
            exp,
        }
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn verify_jwt_connect_token_happy_path() {
        let _guard = test_lock().lock().unwrap();
        let _k1 = set_env("DB9_AUTH_JWT_PUBLIC_KEY", TEST_RSA_PUBLIC_KEY);
        let _k2 = set_env("DB9_AUTH_ISSUER", "https://issuer.example");
        let _k3 = set_env("DB9_AUTH_AUDIENCE", "db9-server");

        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = Claims {
            iss: "https://issuer.example",
            aud: "db9-server",
            tid: "t1",
            usr: "admin",
            sub: "auth0|admin-user",
            email_verified: true,
            roles: vec!["admin", "writer"],
            nullable: None,
            exp,
        };
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("k1".to_string());
        let token = encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap();

        let verified = verify_jwt_connect_token(&token, "db9_tenant_t1", "admin")
            .await
            .unwrap();
        assert_eq!(
            verified.setting("request.jwt.claim.tid"),
            Some("t1"),
            "tenant claim should be exposed"
        );
        assert_eq!(
            verified.setting("request.jwt.claim.usr"),
            Some("admin"),
            "role claim should be exposed"
        );
        assert_eq!(
            verified.setting("request.jwt.claim.sub"),
            Some("auth0|admin-user"),
            "custom identity claims should be exposed"
        );
        assert_eq!(
            verified.setting("auth.uid"),
            Some("auth0|admin-user"),
            "sub should be mirrored into auth.uid for auth helpers"
        );
        assert_eq!(
            verified.setting("request.jwt.claim.email_verified"),
            Some("true"),
            "boolean claims should be stringified for current_setting()"
        );
        assert_eq!(
            verified.setting("request.jwt.claim.roles"),
            Some("[\"admin\",\"writer\"]"),
            "array claims should be preserved as JSON strings"
        );
        let all_claims: serde_json::Value =
            serde_json::from_str(verified.setting("request.jwt.claims").unwrap()).unwrap();
        assert_eq!(all_claims["tid"], "t1");
        assert_eq!(all_claims["usr"], "admin");
        assert_eq!(all_claims["sub"], "auth0|admin-user");
        assert_eq!(all_claims["roles"], serde_json::json!(["admin", "writer"]));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn verify_jwt_connect_token_role_mismatch() {
        let _guard = test_lock().lock().unwrap();
        let _k1 = set_env("DB9_AUTH_JWT_PUBLIC_KEY", TEST_RSA_PUBLIC_KEY);

        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = Claims {
            iss: "https://issuer.example",
            aud: "db9-server",
            tid: "t1",
            usr: "admin",
            sub: "auth0|admin-user",
            email_verified: true,
            roles: vec!["admin"],
            nullable: None,
            exp,
        };
        let token = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap();

        let err = verify_jwt_connect_token(&token, "db9_tenant_t1", "readonly")
            .await
            .unwrap_err();
        assert!(matches!(err, Db9AuthError::RoleMismatch { .. }));
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn verify_jwt_connect_token_compound_usr_claim() {
        let _guard = test_lock().lock().unwrap();
        let _k1 = set_env("DB9_AUTH_JWT_PUBLIC_KEY", TEST_RSA_PUBLIC_KEY);
        let _k2 = set_env("DB9_AUTH_ISSUER", "https://issuer.example");
        let _k3 = set_env("DB9_AUTH_AUDIENCE", "db9-server");

        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        // db9-backend generates usr as "{tenant_id}.{role}"
        let claims = Claims {
            iss: "https://issuer.example",
            aud: "db9-server",
            tid: "t1",
            usr: "t1.admin",
            sub: "auth0|admin-user",
            email_verified: true,
            roles: vec!["admin"],
            nullable: None,
            exp,
        };
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("k1".to_string());
        let token = encode(
            &header,
            &claims,
            &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap();

        let verified = verify_jwt_connect_token(&token, "db9_tenant_t1", "admin")
            .await
            .unwrap();
        assert_eq!(
            verified.setting("request.jwt.claim.tid"),
            Some("t1"),
            "tenant claim should be exposed"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn verify_jwt_connect_token_compound_usr_wrong_tenant() {
        let _guard = test_lock().lock().unwrap();
        let _k1 = set_env("DB9_AUTH_JWT_PUBLIC_KEY", TEST_RSA_PUBLIC_KEY);
        let _k2 = set_env("DB9_AUTH_ISSUER", "https://issuer.example");
        let _k3 = set_env("DB9_AUTH_AUDIENCE", "db9-server");

        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        // usr has a different tenant_id than tid
        let claims = Claims {
            iss: "https://issuer.example",
            aud: "db9-server",
            tid: "t1",
            usr: "t2.admin",
            sub: "auth0|admin-user",
            email_verified: true,
            roles: vec!["admin"],
            nullable: None,
            exp,
        };
        let token = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap();

        let err = verify_jwt_connect_token(&token, "db9_tenant_t1", "admin")
            .await
            .unwrap_err();
        assert!(
            matches!(err, Db9AuthError::TenantMismatch { .. }),
            "mismatched embedded tenant_id should be rejected"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn verify_jwt_connect_token_skips_null_claim_settings_but_keeps_claims_blob() {
        let _guard = test_lock().lock().unwrap();
        let _k1 = set_env("DB9_AUTH_JWT_PUBLIC_KEY", TEST_RSA_PUBLIC_KEY);
        let _k2 = set_env("DB9_AUTH_ISSUER", "https://issuer.example");
        let _k3 = set_env("DB9_AUTH_AUDIENCE", "db9-server");

        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = serde_json::json!({
            "iss": "https://issuer.example",
            "aud": "db9-server",
            "tid": "t1",
            "usr": "admin",
            "sub": "auth0|admin-user",
            "nullable": null,
            "exp": exp,
        });
        let token = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap();

        let verified = verify_jwt_connect_token(&token, "db9_tenant_t1", "admin")
            .await
            .unwrap();

        assert_eq!(
            verified.setting("request.jwt.claim.nullable"),
            None,
            "null claims should not get per-claim GUC entries"
        );

        let all_claims: serde_json::Value =
            serde_json::from_str(verified.setting("request.jwt.claims").unwrap()).unwrap();
        assert!(
            all_claims["nullable"].is_null(),
            "full claims blob should preserve explicit null claims"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_no_kid_multi_key_is_rejected() {
        let _guard = test_lock().lock().unwrap();
        clear_jwks_cache().await;

        let x1 = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let x2 = URL_SAFE_NO_PAD.encode([2u8; 32]);
        let jwks_body =
            format!(r#"{{"keys":[{{"kty":"OKP","x":"{x1}"}},{{"kty":"OKP","x":"{x2}"}}]}}"#);
        let (jwks_url, server_task) = start_jwks_server(jwks_body).await;

        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = Claims {
            iss: "https://issuer.example",
            aud: "db9-server",
            tid: "t1",
            usr: "admin",
            sub: "auth0|admin-user",
            email_verified: true,
            roles: vec!["admin"],
            nullable: None,
            exp,
        };
        let token = encode(
            &Header::new(Algorithm::RS256),
            &claims,
            &EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap(),
        )
        .unwrap();

        let result = jwks_decoding_key(&token, &jwks_url).await;
        assert!(matches!(result, Err(Db9AuthError::JwksKidMissing)));

        server_task.await.unwrap();
    }

    #[test]
    fn classify_db9_auth_material_basic() {
        assert_eq!(
            classify_db9_auth_material("db9ck_abc"),
            Db9AuthMaterialKind::ConnectKey
        );
        assert_eq!(
            classify_db9_auth_material("secret123"),
            Db9AuthMaterialKind::Password
        );
    }

    fn parse_utc(value: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    #[test]
    fn validate_connect_key_introspection_backend_schema_happy_path() {
        let now = parse_utc("2026-03-11T00:00:00Z");
        let body = r#"{
            "active": true,
            "revoked": false,
            "expired": false,
            "tenant_id": "t1",
            "role": "admin",
            "expires_at": "2026-03-11T00:00:10Z"
        }"#;
        let info: ConnectKeyIntrospectResponse = serde_json::from_str(body).unwrap();
        validate_connect_key_introspection(&info, "t1", "admin", now).unwrap();
    }

    #[test]
    fn validate_connect_key_introspection_inactive_is_denied() {
        let now = parse_utc("2026-03-11T00:00:00Z");
        let body = r#"{ "active": false }"#;
        let info: ConnectKeyIntrospectResponse = serde_json::from_str(body).unwrap();
        let err = validate_connect_key_introspection(&info, "t1", "admin", now).unwrap_err();
        assert!(matches!(
            err,
            Db9AuthError::InvalidConnectKey { reason } if reason == "inactive"
        ));
    }

    #[test]
    fn validate_connect_key_introspection_expired_timestamp_is_denied() {
        let now = parse_utc("2026-03-11T00:00:00Z");
        let body = r#"{
            "active": true,
            "tenant_id": "t1",
            "role": "admin",
            "expires_at": "2026-03-10T23:59:59Z"
        }"#;
        let info: ConnectKeyIntrospectResponse = serde_json::from_str(body).unwrap();
        let err = validate_connect_key_introspection(&info, "t1", "admin", now).unwrap_err();
        assert!(matches!(
            err,
            Db9AuthError::InvalidConnectKey { reason } if reason == "expired"
        ));
    }

    #[test]
    fn validate_connect_key_introspection_invalid_expires_at_is_denied() {
        let now = parse_utc("2026-03-11T00:00:00Z");
        let body = r#"{
            "active": true,
            "tenant_id": "t1",
            "role": "admin",
            "expires_at": "nope"
        }"#;
        let info: ConnectKeyIntrospectResponse = serde_json::from_str(body).unwrap();
        let err = validate_connect_key_introspection(&info, "t1", "admin", now).unwrap_err();
        assert!(matches!(
            err,
            Db9AuthError::InvalidConnectKey { reason } if reason.starts_with("invalid expires_at:")
        ));
    }

    #[test]
    fn validate_connect_key_introspection_legacy_schema_is_accepted() {
        let now = parse_utc("2026-03-11T00:00:00Z");
        let body = r#"{
            "tenantId": "t1",
            "user": "admin",
            "expiresAt": 9999999999
        }"#;
        let info: ConnectKeyIntrospectResponse = serde_json::from_str(body).unwrap();
        validate_connect_key_introspection(&info, "t1", "admin", now).unwrap();
    }

    #[test]
    fn validate_connect_key_introspection_legacy_revoked_at_is_denied() {
        let now = parse_utc("2026-03-11T00:00:00Z");
        let body = r#"{
            "tenantId": "t1",
            "user": "admin",
            "revokedAt": 1
        }"#;
        let info: ConnectKeyIntrospectResponse = serde_json::from_str(body).unwrap();
        let err = validate_connect_key_introspection(&info, "t1", "admin", now).unwrap_err();
        assert!(matches!(
            err,
            Db9AuthError::InvalidConnectKey { reason } if reason == "revoked"
        ));
    }

    /// Multi-connection JWKS server that counts how many HTTP requests it receives.
    async fn start_counting_jwks_server(
        jwks_body: String,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                counter_clone.fetch_add(1, Ordering::SeqCst);
                let body = jwks_body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        (format!("http://{addr}/jwks"), counter, handle)
    }

    /// Multi-connection JWKS server with a mutable body and request counter.
    async fn start_mutable_counting_jwks_server(
        jwks_body: Arc<StdMutex<String>>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                counter_clone.fetch_add(1, Ordering::SeqCst);
                let body = jwks_body.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let body = body.lock().unwrap().clone();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        (format!("http://{addr}/jwks"), counter, handle)
    }

    /// Counting JWKS server that pops a scripted (status, body) pair per request.
    async fn start_scripted_counting_jwks_server(
        responses: Arc<StdMutex<VecDeque<(u16, String)>>>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                counter_clone.fetch_add(1, Ordering::SeqCst);
                let responses = responses.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let (status, body) = responses
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or((500, String::new()));
                    let status_line = if status == 200 {
                        "200 OK".to_string()
                    } else {
                        format!("{status} ERROR")
                    };
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        (format!("http://{addr}/jwks"), counter, handle)
    }

    /// Counting JWKS server that waits on `release_response` before replying.
    async fn start_blocking_counting_jwks_server(
        jwks_body: String,
        request_started: Arc<Notify>,
        release_response: Arc<Notify>,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();

        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                counter_clone.fetch_add(1, Ordering::SeqCst);
                request_started.notify_waiters();
                let body = jwks_body.clone();
                let release = release_response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    release.notified().await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        (format!("http://{addr}/jwks"), counter, handle)
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_repeated_unknown_kid_is_throttled_per_selector() {
        let _guard = test_lock().lock().unwrap();
        clear_jwks_cache().await;

        // JWKS with one key whose kid is "known-kid".
        let x = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let jwks_body = format!(r#"{{"keys":[{{"kty":"OKP","kid":"known-kid","x":"{x}"}}]}}"#);
        let (jwks_url, counter, server_task) = start_counting_jwks_server(jwks_body).await;

        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap();
        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = test_claims(exp);

        // 1) First request with unknown kid — cache empty → fetches JWKS → kid not found.
        let mut h1 = Header::new(Algorithm::RS256);
        h1.kid = Some("unknown-kid-1".to_string());
        let t1 = encode(&h1, &claims, &encoding_key).unwrap();
        let r1 = jwks_decoding_key(&t1, &jwks_url).await;
        assert!(matches!(r1, Err(Db9AuthError::JwksKidNotFound { .. })));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "first call should fetch once"
        );

        // 2) Same unknown kid within cooldown — still no fetch.
        let mut h2 = Header::new(Algorithm::RS256);
        h2.kid = Some("unknown-kid-1".to_string());
        let t2 = encode(&h2, &claims, &encoding_key).unwrap();
        let r2 = jwks_decoding_key(&t2, &jwks_url).await;
        assert!(matches!(r2, Err(Db9AuthError::JwksKidNotFound { .. })));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "same unknown kid within cooldown must NOT fetch again"
        );

        // 3) Request with the known kid — should succeed from cache (no extra fetch).
        let mut h3 = Header::new(Algorithm::RS256);
        h3.kid = Some("known-kid".to_string());
        let t3 = encode(&h3, &claims, &encoding_key).unwrap();
        let r3 = jwks_decoding_key(&t3, &jwks_url).await;
        assert!(r3.is_ok(), "known kid should resolve from cache");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "known kid should hit cache without fetching"
        );

        server_task.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_prior_unknown_miss_does_not_block_rotated_kid() {
        let _guard = test_lock().lock().unwrap();
        clear_jwks_cache().await;

        let x1 = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let x2 = URL_SAFE_NO_PAD.encode([2u8; 32]);
        let body = Arc::new(StdMutex::new(format!(
            r#"{{"keys":[{{"kty":"OKP","kid":"known-kid","x":"{x1}"}}]}}"#
        )));
        let (jwks_url, counter, server_task) =
            start_mutable_counting_jwks_server(body.clone()).await;

        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap();
        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = test_claims(exp);

        let mut known_header = Header::new(Algorithm::RS256);
        known_header.kid = Some("known-kid".to_string());
        let known_token = encode(&known_header, &claims, &encoding_key).unwrap();
        assert!(jwks_decoding_key(&known_token, &jwks_url).await.is_ok());
        assert_eq!(counter.load(Ordering::SeqCst), 1);

        let mut bad_header = Header::new(Algorithm::RS256);
        bad_header.kid = Some("bad-kid".to_string());
        let bad_token = encode(&bad_header, &claims, &encoding_key).unwrap();
        let bad_result = jwks_decoding_key(&bad_token, &jwks_url).await;
        assert!(matches!(
            bad_result,
            Err(Db9AuthError::JwksKidNotFound { .. })
        ));
        assert_eq!(counter.load(Ordering::SeqCst), 2);

        *body.lock().unwrap() =
            format!(r#"{{"keys":[{{"kty":"OKP","kid":"rotated-kid","x":"{x2}"}}]}}"#);

        let mut rotated_header = Header::new(Algorithm::RS256);
        rotated_header.kid = Some("rotated-kid".to_string());
        let rotated_token = encode(&rotated_header, &claims, &encoding_key).unwrap();
        let rotated_result = timeout(
            Duration::from_secs(1),
            jwks_decoding_key(&rotated_token, &jwks_url),
        )
        .await
        .unwrap();
        assert!(
            rotated_result.is_ok(),
            "a prior unknown kid miss must not block a legitimate rotated kid"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            3,
            "rotated kid should trigger a new JWKS fetch"
        );

        server_task.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_repeated_missing_kid_is_throttled_per_selector() {
        let _guard = test_lock().lock().unwrap();
        clear_jwks_cache().await;

        let x1 = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let x2 = URL_SAFE_NO_PAD.encode([2u8; 32]);
        let jwks_body =
            format!(r#"{{"keys":[{{"kty":"OKP","x":"{x1}"}},{{"kty":"OKP","x":"{x2}"}}]}}"#);
        let (jwks_url, counter, server_task) = start_counting_jwks_server(jwks_body).await;

        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap();
        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = test_claims(exp);
        let token = encode(&Header::new(Algorithm::RS256), &claims, &encoding_key).unwrap();

        let r1 = jwks_decoding_key(&token, &jwks_url).await;
        assert!(matches!(r1, Err(Db9AuthError::JwksKidMissing)));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "first missing-kid call should fetch once"
        );

        let r2 = jwks_decoding_key(&token, &jwks_url).await;
        assert!(matches!(r2, Err(Db9AuthError::JwksKidMissing)));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "same missing-kid selector within cooldown must NOT fetch again"
        );

        server_task.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_new_kid_after_recent_fetch_triggers_refresh() {
        let _guard = test_lock().lock().unwrap();
        clear_jwks_cache().await;

        let x1 = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let x2 = URL_SAFE_NO_PAD.encode([2u8; 32]);
        let body = Arc::new(StdMutex::new(format!(
            r#"{{"keys":[{{"kty":"OKP","kid":"known-kid","x":"{x1}"}}]}}"#
        )));
        let (jwks_url, counter, server_task) =
            start_mutable_counting_jwks_server(body.clone()).await;

        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap();
        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = test_claims(exp);

        let mut h1 = Header::new(Algorithm::RS256);
        h1.kid = Some("known-kid".to_string());
        let t1 = encode(&h1, &claims, &encoding_key).unwrap();
        let r1 = jwks_decoding_key(&t1, &jwks_url).await;
        assert!(r1.is_ok(), "initial known kid should resolve");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "initial lookup should fetch once"
        );

        *body.lock().unwrap() =
            format!(r#"{{"keys":[{{"kty":"OKP","kid":"rotated-kid","x":"{x2}"}}]}}"#);

        let mut h2 = Header::new(Algorithm::RS256);
        h2.kid = Some("rotated-kid".to_string());
        let t2 = encode(&h2, &claims, &encoding_key).unwrap();
        let r2 = jwks_decoding_key(&t2, &jwks_url).await;
        assert!(
            r2.is_ok(),
            "first token for a rotated kid should force a refresh and succeed"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "rotated kid should trigger a second JWKS fetch"
        );

        server_task.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_fetch_failure_does_not_block_retry_after_recovery() {
        let _guard = test_lock().lock().unwrap();
        clear_jwks_cache().await;

        let x = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let success_body = format!(r#"{{"keys":[{{"kty":"OKP","kid":"known-kid","x":"{x}"}}]}}"#);
        let responses = Arc::new(StdMutex::new(VecDeque::from([
            (500, String::new()),
            (200, success_body),
        ])));
        let (jwks_url, counter, server_task) = start_scripted_counting_jwks_server(responses).await;

        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap();
        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = test_claims(exp);
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("known-kid".to_string());
        let token = encode(&header, &claims, &encoding_key).unwrap();

        let first_result = jwks_decoding_key(&token, &jwks_url).await;
        assert!(matches!(
            first_result,
            Err(Db9AuthError::JwksFetchFailed { .. })
        ));

        let second_result = timeout(Duration::from_secs(1), jwks_decoding_key(&token, &jwks_url))
            .await
            .unwrap();
        assert!(
            second_result.is_ok(),
            "a transient JWKS failure must not delay the next retry after recovery"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            2,
            "retry after recovery should perform a second JWKS fetch"
        );

        server_task.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_missing_selector_cache_is_bounded() {
        let _guard = test_lock().lock().unwrap();
        let mut entry = JwksCacheEntry {
            jwks_url: "http://jwks.example/test".to_string(),
            fetched_at: Instant::now(),
            keys_by_kid: HashMap::new(),
            singleton_key: None,
            missing_selectors: HashMap::new(),
        };

        for i in 0..(JWKS_MISSING_SELECTOR_CACHE_LIMIT + 8) {
            record_missing_selector(
                &mut entry,
                JwksSelector::Kid(format!("bad-kid-{i}")),
                Instant::now(),
            );
        }

        assert!(
            entry.missing_selectors.len() <= JWKS_MISSING_SELECTOR_CACHE_LIMIT,
            "negative cache must remain bounded"
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_concurrent_distinct_unknown_kids_share_one_refresh() {
        let _guard = test_lock().lock().unwrap();
        clear_jwks_cache().await;

        let x = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let jwks_body = format!(r#"{{"keys":[{{"kty":"OKP","kid":"known-kid","x":"{x}"}}]}}"#);
        let request_started = Arc::new(Notify::new());
        let release_response = Arc::new(Notify::new());
        let (jwks_url, counter, server_task) = start_blocking_counting_jwks_server(
            jwks_body,
            request_started.clone(),
            release_response.clone(),
        )
        .await;

        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap();
        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = test_claims(exp);

        let mut h1 = Header::new(Algorithm::RS256);
        h1.kid = Some("unknown-kid-1".to_string());
        let t1 = encode(&h1, &claims, &encoding_key).unwrap();
        let task1 = tokio::spawn({
            let jwks_url = jwks_url.clone();
            async move { jwks_decoding_key(&t1, &jwks_url).await }
        });

        let mut h2 = Header::new(Algorithm::RS256);
        h2.kid = Some("unknown-kid-2".to_string());
        let t2 = encode(&h2, &claims, &encoding_key).unwrap();
        let task2 = tokio::spawn({
            let jwks_url = jwks_url.clone();
            async move { jwks_decoding_key(&t2, &jwks_url).await }
        });

        wait_for_request_count(&counter, 1).await;
        release_response.notify_one();

        let result1 = timeout(Duration::from_secs(1), task1)
            .await
            .unwrap()
            .unwrap();
        let result2 = timeout(Duration::from_secs(1), task2)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result1, Err(Db9AuthError::JwksKidNotFound { .. })));
        assert!(matches!(result2, Err(Db9AuthError::JwksKidNotFound { .. })));
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "concurrent distinct unknown kids must share one in-flight JWKS refresh"
        );

        server_task.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_concurrent_waiters_share_one_refresh() {
        let _guard = test_lock().lock().unwrap();
        clear_jwks_cache().await;

        let x = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let jwks_body = format!(r#"{{"keys":[{{"kty":"OKP","kid":"known-kid","x":"{x}"}}]}}"#);
        let request_started = Arc::new(Notify::new());
        let release_response = Arc::new(Notify::new());
        let (jwks_url, counter, server_task) = start_blocking_counting_jwks_server(
            jwks_body,
            request_started.clone(),
            release_response.clone(),
        )
        .await;

        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap();
        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = test_claims(exp);
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("known-kid".to_string());
        let token = encode(&header, &claims, &encoding_key).unwrap();

        let task1 = tokio::spawn({
            let token = token.clone();
            let jwks_url = jwks_url.clone();
            async move { jwks_decoding_key(&token, &jwks_url).await }
        });
        let task2 = tokio::spawn({
            let token = token.clone();
            let jwks_url = jwks_url.clone();
            async move { jwks_decoding_key(&token, &jwks_url).await }
        });

        wait_for_request_count(&counter, 1).await;
        release_response.notify_one();

        let result1 = timeout(Duration::from_secs(1), task1)
            .await
            .unwrap()
            .unwrap();
        let result2 = timeout(Duration::from_secs(1), task2)
            .await
            .unwrap()
            .unwrap();
        assert!(result1.is_ok(), "first waiter should resolve successfully");
        assert!(result2.is_ok(), "second waiter should resolve successfully");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "concurrent waiters must share a single JWKS fetch"
        );

        server_task.abort();
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn jwks_refresh_survives_request_cancellation() {
        let _guard = test_lock().lock().unwrap();
        clear_jwks_cache().await;

        let x = URL_SAFE_NO_PAD.encode([1u8; 32]);
        let jwks_body = format!(r#"{{"keys":[{{"kty":"OKP","kid":"known-kid","x":"{x}"}}]}}"#);
        let request_started = Arc::new(Notify::new());
        let release_response = Arc::new(Notify::new());
        let (jwks_url, counter, server_task) = start_blocking_counting_jwks_server(
            jwks_body,
            request_started.clone(),
            release_response.clone(),
        )
        .await;

        let encoding_key = EncodingKey::from_rsa_pem(TEST_RSA_PRIVATE_KEY.as_bytes()).unwrap();
        let exp = (chrono::Utc::now().timestamp() + 60) as usize;
        let claims = test_claims(exp);
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("known-kid".to_string());
        let token = encode(&header, &claims, &encoding_key).unwrap();

        let initial_task = tokio::spawn({
            let token = token.clone();
            let jwks_url = jwks_url.clone();
            async move { jwks_decoding_key(&token, &jwks_url).await }
        });

        wait_for_request_count(&counter, 1).await;
        initial_task.abort();

        let retry_task = tokio::spawn({
            let token = token.clone();
            let jwks_url = jwks_url.clone();
            async move { jwks_decoding_key(&token, &jwks_url).await }
        });

        release_response.notify_one();

        let retry_result = timeout(Duration::from_secs(1), retry_task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            retry_result.is_ok(),
            "later callers must not wedge when the original refresher is cancelled"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "retry should reuse the in-flight JWKS refresh instead of starting a new one"
        );

        server_task.abort();
    }
}
