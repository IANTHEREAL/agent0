use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use rand::RngCore;

const NONCE_LEN: usize = 12;

pub fn encrypt(plaintext: &str, key_b64: &str) -> Result<String, String> {
    let key_bytes = B64.decode(key_b64).map_err(|e| format!("bad key: {e}"))?;
    if key_bytes.len() != 32 {
        return Err(format!("key must be 32 bytes, got {}", key_bytes.len()));
    }
    let cipher = Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| format!("cipher init: {e}"))?;

    let mut nonce_bytes = [0u8; NONCE_LEN];
    rand::thread_rng().fill_bytes(&mut nonce_bytes);
    let nonce = Nonce::from_slice(&nonce_bytes);

    let ciphertext = cipher
        .encrypt(nonce, plaintext.as_bytes())
        .map_err(|e| format!("encrypt: {e}"))?;

    let mut combined = Vec::with_capacity(NONCE_LEN + ciphertext.len());
    combined.extend_from_slice(&nonce_bytes);
    combined.extend_from_slice(&ciphertext);

    Ok(B64.encode(&combined))
}

pub fn decrypt(encoded: &str, key_b64: &str) -> Result<String, String> {
    let key_bytes = B64.decode(key_b64).map_err(|e| format!("bad key: {e}"))?;
    if key_bytes.len() != 32 {
        return Err(format!("key must be 32 bytes, got {}", key_bytes.len()));
    }
    let cipher = Aes256Gcm::new_from_slice(&key_bytes).map_err(|e| format!("cipher init: {e}"))?;

    let combined = B64
        .decode(encoded)
        .map_err(|e| format!("bad ciphertext: {e}"))?;
    if combined.len() < NONCE_LEN {
        return Err("ciphertext too short".into());
    }

    let (nonce_bytes, ciphertext) = combined.split_at(NONCE_LEN);
    let nonce = Nonce::from_slice(nonce_bytes);

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| format!("decrypt: {e}"))?;

    String::from_utf8(plaintext).map_err(|e| format!("utf8: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> String {
        B64.encode([0xABu8; 32])
    }

    #[test]
    fn round_trip() {
        let key = test_key();
        let plaintext = "hunter2!@#$%";
        let encrypted = encrypt(plaintext, &key).unwrap();
        assert_ne!(encrypted, plaintext);
        let decrypted = decrypt(&encrypted, &key).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn wrong_key_fails() {
        let key1 = test_key();
        let key2 = B64.encode([0xCDu8; 32]);
        let encrypted = encrypt("secret", &key1).unwrap();
        assert!(decrypt(&encrypted, &key2).is_err());
    }

    #[test]
    fn bad_key_length() {
        let short_key = B64.encode([0xABu8; 16]);
        assert!(encrypt("test", &short_key).is_err());
    }
}
