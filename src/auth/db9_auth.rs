use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::config;
use anyhow::Result as AnyhowResult;
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use tikv_client::Transaction;
use tokio::sync::Mutex;

use super::{AuthManager, User};

const KEYSPACE_PREFIX: &str = "db9_tenant_";
const DEFAULT_AUDIENCE: &str = "db9-server";
const JWKS_CACHE_TTL: Duration = Duration::from_secs(60);
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

#[derive(Debug, thiserror::Error)]
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
        claims.insert("exp".to_string(), JsonValue::from(self.exp as u64));
        claims.insert("tid".to_string(), JsonValue::from(self.tid));
        claims.insert("usr".to_string(), JsonValue::from(self.usr));

        let all_claims_json =
            serde_json::to_string(&claims).map_err(|err| Db9AuthError::InvalidJwt {
                reason: format!("validated JWT claims could not be serialized: {err}"),
            })?;

        let mut settings = BTreeMap::new();
        settings.insert("request.jwt.claims".to_string(), all_claims_json);
        for (claim_name, value) in claims {
            if value.is_null() {
                continue;
            }
            let setting_value = claim_value_to_setting_string(&value);
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

fn claim_value_to_setting_string(value: &JsonValue) -> String {
    match value {
        JsonValue::String(s) => s.clone(),
        JsonValue::Bool(v) => v.to_string(),
        JsonValue::Number(n) => n.to_string(),
        JsonValue::Null => String::new(),
        JsonValue::Array(_) | JsonValue::Object(_) => serde_json::to_string(value)
            .expect("serde_json::Value from JWT claims must be serializable"),
    }
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

struct JwksCacheEntry {
    jwks_url: String,
    fetched_at: Instant,
    keys_by_kid: HashMap<String, Arc<DecodingKey>>,
    singleton_key: Option<Arc<DecodingKey>>,
}

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static JWKS_CACHE: OnceLock<Mutex<Option<JwksCacheEntry>>> = OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(3))
            .build()
            .expect("failed to build reqwest HTTP client")
    })
}

fn jwks_cache() -> &'static Mutex<Option<JwksCacheEntry>> {
    JWKS_CACHE.get_or_init(|| Mutex::new(None))
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
    if claims.usr != expected_role {
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
        return Err(Db9AuthError::InvalidConnectKey {
            reason: format!("introspection HTTP {}", status.as_u16()),
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
    let kid = header.kid;

    // Try cache first (if fresh and for same URL).
    {
        let cache = jwks_cache().lock().await;
        if let Some(entry) = cache.as_ref() {
            if entry.jwks_url == jwks_url && entry.fetched_at.elapsed() < JWKS_CACHE_TTL {
                if let Some(kid) = kid.as_deref() {
                    if let Some(key) = entry.keys_by_kid.get(kid) {
                        return Ok(Arc::clone(key));
                    }
                } else if let Some(key) = entry.singleton_key.as_ref() {
                    return Ok(Arc::clone(key));
                }
            }
        }
    }

    // Cache miss / stale / rotated. Refresh JWKS and try again.
    let (keys_by_kid, singleton_key) = fetch_and_parse_jwks(jwks_url).await?;
    {
        let mut cache = jwks_cache().lock().await;
        *cache = Some(JwksCacheEntry {
            jwks_url: jwks_url.to_string(),
            fetched_at: Instant::now(),
            keys_by_kid: keys_by_kid.clone(),
            singleton_key: singleton_key.clone(),
        });
    }

    match kid {
        Some(kid) => keys_by_kid
            .get(&kid)
            .cloned()
            .ok_or(Db9AuthError::JwksKidNotFound { kid }),
        None => singleton_key.ok_or(Db9AuthError::JwksKidMissing),
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
        return Err(Db9AuthError::JwksFetchFailed {
            reason: format!("HTTP {} from JWKS endpoint", status.as_u16()),
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
    use std::sync::Mutex as StdMutex;
    use std::sync::OnceLock;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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
    async fn jwks_no_kid_multi_key_is_rejected() {
        let _guard = test_lock().lock().unwrap();
        *jwks_cache().lock().await = None;

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
}
