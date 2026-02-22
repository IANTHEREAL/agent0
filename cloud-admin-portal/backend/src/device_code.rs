use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{Duration, Utc};
use rand::Rng;

#[derive(Clone, Debug)]
pub enum DeviceCodeStatus {
    Pending,
    Approved { token: String, expires_at: String },
    Denied,
}

#[derive(Clone, Debug)]
pub struct PendingDevice {
    pub device_code: String,
    pub user_code: String,
    pub status: DeviceCodeStatus,
    pub created_at: String,
    pub expires_at: String,
}

pub struct DeviceCodeStore {
    entries: RwLock<HashMap<String, PendingDevice>>,
    ttl_seconds: i64,
}

const USER_CODE_LEN: usize = 8;
const DEVICE_CODE_LEN: usize = 40;

impl DeviceCodeStore {
    pub fn new(ttl_seconds: u64) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl_seconds: ttl_seconds as i64,
        }
    }

    pub fn create(&self) -> PendingDevice {
        let device_code = generate_device_code();
        let user_code = generate_user_code();
        let now = Utc::now();
        let expires_at = (now + Duration::seconds(self.ttl_seconds)).to_rfc3339();

        let entry = PendingDevice {
            device_code: device_code.clone(),
            user_code: user_code.clone(),
            status: DeviceCodeStatus::Pending,
            created_at: now.to_rfc3339(),
            expires_at,
        };

        let normalized_uc = user_code.replace(['-', ' '], "");
        if let Ok(mut map) = self.entries.write() {
            map.insert(device_code, entry.clone());
            map.insert(format!("uc:{normalized_uc}"), entry.clone());
        }
        entry
    }

    pub fn poll_and_consume(&self, device_code: &str) -> Option<PendingDevice> {
        let mut map = self.entries.write().ok()?;
        let entry = map.get(device_code)?;

        if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(&entry.expires_at) {
            if Utc::now() > exp {
                return None;
            }
        }

        let result = entry.clone();

        if matches!(result.status, DeviceCodeStatus::Approved { .. }) {
            let uc_normalized = result.user_code.replace(['-', ' '], "");
            map.remove(device_code);
            map.remove(&format!("uc:{uc_normalized}"));
        }

        Some(result)
    }

    pub fn lookup_by_user_code(&self, user_code: &str) -> Option<PendingDevice> {
        let normalized = user_code.to_ascii_uppercase().replace(['-', ' '], "");
        let map = self.entries.read().ok()?;
        let entry = map.get(&format!("uc:{normalized}"))?;

        if let Ok(exp) = chrono::DateTime::parse_from_rfc3339(&entry.expires_at) {
            if Utc::now() > exp {
                return None;
            }
        }

        Some(entry.clone())
    }

    pub fn approve(&self, user_code: &str, token: String, expires_at: String) -> bool {
        let normalized = user_code.to_ascii_uppercase().replace(['-', ' '], "");
        if let Ok(mut map) = self.entries.write() {
            let device_code = {
                let entry = match map.get(&format!("uc:{normalized}")) {
                    Some(e) => e,
                    None => return false,
                };
                entry.device_code.clone()
            };

            let status = DeviceCodeStatus::Approved {
                token: token.clone(),
                expires_at: expires_at.clone(),
            };

            if let Some(entry) = map.get_mut(&device_code) {
                entry.status = status.clone();
            }
            if let Some(entry) = map.get_mut(&format!("uc:{normalized}")) {
                entry.status = status;
            }
            true
        } else {
            false
        }
    }

    pub fn sweep_expired(&self) -> usize {
        if let Ok(mut map) = self.entries.write() {
            let now = Utc::now();
            let before = map.len();
            map.retain(|_, e| {
                chrono::DateTime::parse_from_rfc3339(&e.expires_at)
                    .map(|exp| now < exp)
                    .unwrap_or(false)
            });
            before - map.len()
        } else {
            0
        }
    }
}

fn generate_device_code() -> String {
    let mut bytes = [0u8; DEVICE_CODE_LEN];
    rand::thread_rng().fill(&mut bytes[..]);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn generate_user_code() -> String {
    let charset = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    let code: String = (0..USER_CODE_LEN)
        .map(|_| charset[rng.gen_range(0..charset.len())] as char)
        .collect();
    format!("{}-{}", &code[..4], &code[4..])
}
