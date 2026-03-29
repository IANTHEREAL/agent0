use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub fn hash_password(password: &str, salt: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(password.as_bytes());
    hasher.update(salt.as_bytes());
    let result = hasher.finalize();
    hex::encode(result)
}

#[cfg(test)]
pub fn verify_password(password: &str, salt: &str, hash: &str) -> bool {
    hash_password(password, salt) == hash
}

/// Constant-time password verification to prevent timing side-channel attacks.
pub fn verify_password_ct(password: &str, salt: &str, hash: &str) -> bool {
    let computed = hash_password(password, salt);
    computed.as_bytes().ct_eq(hash.as_bytes()).into()
}

pub fn generate_salt() -> String {
    use rand::Rng;
    let salt: [u8; 16] = rand::thread_rng().gen();
    hex::encode(salt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_and_verify() {
        let password = "secret123";
        let salt = generate_salt();
        let hash = hash_password(password, &salt);

        assert!(verify_password(password, &salt, &hash));
        assert!(!verify_password("wrong", &salt, &hash));
    }

    #[test]
    fn test_different_salts_produce_different_hashes() {
        let password = "secret123";
        let salt1 = "salt1";
        let salt2 = "salt2";

        let hash1 = hash_password(password, salt1);
        let hash2 = hash_password(password, salt2);

        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_generate_salt_uniqueness() {
        let salt1 = generate_salt();
        let salt2 = generate_salt();
        assert_ne!(salt1, salt2);
        assert_eq!(salt1.len(), 32);
    }
}
