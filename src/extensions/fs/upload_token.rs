// Embedded/PageFS upload-token signing: no longer reached by production routing
// after the JuiceFS-only migration (#2567); retained for the deferred
// embedded->JuiceFS data-migration track. Allow dead_code so `cargo clippy
// -D warnings` stays green without deleting code we intend to revive.
#![allow(dead_code)]

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::extensions::fs::config::fs9_config;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UploadTokenClaims {
    pub(crate) keyspace: String,
    pub(crate) fs_instance_id: [u8; 16],
    pub(crate) staging_inode_id: u64,
    pub(crate) target_path_hash: String,
    pub(crate) expected_parent_inode: Option<u64>,
    pub(crate) expected_prior_inode: Option<u64>,
    #[serde(default)]
    pub(crate) expected_prior_generation: Option<u64>,
    pub(crate) upload_id: String,
    pub(crate) target_version: u64,
    pub(crate) nonce: u64,
    pub(crate) expires_at: i64,
}

pub(crate) fn normalized_path_hash_hex(path: &str) -> String {
    hex::encode(Sha256::digest(path.as_bytes()))
}

pub(crate) fn sign_upload_token(claims: &UploadTokenClaims) -> Result<String> {
    sign_upload_token_with_secret(&upload_token_secret()?, claims)
}

pub(crate) fn verify_upload_token(token: &str) -> Result<UploadTokenClaims> {
    verify_upload_token_with_secret(&upload_token_secret()?, token)
}

fn sign_upload_token_with_secret(secret: &str, claims: &UploadTokenClaims) -> Result<String> {
    let payload = serde_json::to_vec(claims)?;
    let signature = sign_bytes(secret, &payload)?;
    Ok(format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

fn verify_upload_token_with_secret(secret: &str, token: &str) -> Result<UploadTokenClaims> {
    let (payload_b64, signature_b64) = token
        .split_once('.')
        .ok_or_else(|| anyhow!("fs9: invalid upload token format"))?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| anyhow!("fs9: invalid upload token payload: {e}"))?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature_b64)
        .map_err(|e| anyhow!("fs9: invalid upload token signature: {e}"))?;

    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| anyhow!("fs9: {e}"))?;
    mac.update(&payload);
    mac.verify_slice(&signature)
        .map_err(|_| anyhow!("fs9: upload token signature mismatch"))?;

    serde_json::from_slice(&payload).map_err(|e| anyhow!("fs9: invalid upload token claims: {e}"))
}

fn sign_bytes(secret: &str, payload: &[u8]) -> Result<Vec<u8>> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).map_err(|e| anyhow!("fs9: {e}"))?;
    mac.update(payload);
    Ok(mac.finalize().into_bytes().to_vec())
}

fn upload_token_secret() -> Result<String> {
    fs9_config()
        .upload_token_secret
        .clone()
        .ok_or_else(|| anyhow!("fs9: FS9_UPLOAD_TOKEN_SECRET is not configured"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalized_path_hash_hex_changes_with_path() {
        let a = normalized_path_hash_hex("/a");
        let b = normalized_path_hash_hex("/b");
        assert_ne!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn upload_token_roundtrip_with_fixed_secret() {
        let claims = UploadTokenClaims {
            keyspace: "tenant_a".to_string(),
            fs_instance_id: [7u8; 16],
            staging_inode_id: 42,
            target_path_hash: normalized_path_hash_hex("/data/object.bin"),
            expected_parent_inode: Some(7),
            expected_prior_inode: Some(11),
            expected_prior_generation: Some(2),
            upload_id: "upload-1".to_string(),
            target_version: 42,
            nonce: 99,
            expires_at: 1234567890,
        };

        let token = sign_upload_token_with_secret("test-secret", &claims).expect("token must sign");
        let decoded =
            verify_upload_token_with_secret("test-secret", &token).expect("token must verify");
        assert_eq!(decoded, claims);
    }

    #[test]
    fn upload_token_rejects_tampered_signature() {
        let claims = UploadTokenClaims {
            keyspace: "tenant_a".to_string(),
            fs_instance_id: [9u8; 16],
            staging_inode_id: 1,
            target_path_hash: normalized_path_hash_hex("/x"),
            expected_parent_inode: None,
            expected_prior_inode: None,
            expected_prior_generation: None,
            upload_id: "u".to_string(),
            target_version: 1,
            nonce: 1,
            expires_at: 1,
        };
        let token = sign_upload_token_with_secret("test-secret", &claims).expect("token must sign");
        let tampered = format!("{}A", token);
        let err = verify_upload_token_with_secret("test-secret", &tampered)
            .expect_err("tampered token must fail");
        assert!(err.to_string().contains("signature"));
    }
}
