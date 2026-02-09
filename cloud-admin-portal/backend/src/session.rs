use std::collections::HashMap;
use std::sync::RwLock;

use chrono::{Duration, Utc};
use rand::Rng;

#[derive(Clone, Debug)]
pub struct TenantSession {
    pub session_id: String,
    pub tenant_id: String,
    pub admin_user: String,
    pub admin_password: String,
    pub expires_at: String,
}

pub struct SessionManager {
    sessions: RwLock<HashMap<String, TenantSession>>,
    ttl_hours: i64,
}

impl SessionManager {
    pub fn new(ttl_hours: u64) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            ttl_hours: ttl_hours as i64,
        }
    }

    pub fn create_session(
        &self,
        tenant_id: &str,
        admin_user: &str,
        admin_password: &str,
    ) -> TenantSession {
        let session_id = generate_session_id();
        let expires_at = (Utc::now() + Duration::hours(self.ttl_hours)).to_rfc3339();

        let session = TenantSession {
            session_id: session_id.clone(),
            tenant_id: tenant_id.to_string(),
            admin_user: admin_user.to_string(),
            admin_password: admin_password.to_string(),
            expires_at,
        };

        if let Ok(mut map) = self.sessions.write() {
            map.insert(session_id, session.clone());
        }
        session
    }

    pub fn validate_session(&self, session_id: &str, tenant_id: &str) -> Option<TenantSession> {
        let map = self.sessions.read().ok()?;
        let session = map.get(session_id)?;

        if session.tenant_id != tenant_id {
            return None;
        }

        if let Ok(expires) = chrono::DateTime::parse_from_rfc3339(&session.expires_at) {
            if Utc::now() > expires {
                return None;
            }
        }

        Some(session.clone())
    }

    pub fn sweep_expired(&self) -> usize {
        if let Ok(mut map) = self.sessions.write() {
            return self.evict_expired_from_map(&mut map);
        }
        0
    }

    pub fn session_count(&self) -> usize {
        self.sessions.read().map(|m| m.len()).unwrap_or(0)
    }

    pub fn clear_all(&self) {
        if let Ok(mut map) = self.sessions.write() {
            map.clear();
        }
    }

    fn evict_expired_from_map(&self, map: &mut HashMap<String, TenantSession>) -> usize {
        let now = Utc::now();
        let before = map.len();
        map.retain(|_, s| {
            chrono::DateTime::parse_from_rfc3339(&s.expires_at)
                .map(|exp| now < exp)
                .unwrap_or(false)
        });
        before - map.len()
    }
}

fn generate_session_id() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
