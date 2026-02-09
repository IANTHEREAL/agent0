use serde_json::json;

pub struct PdClient {
    base_url: String,
    client: reqwest::Client,
}

impl PdClient {
    pub fn new(pd_endpoints: &str, client: &reqwest::Client) -> Self {
        let first = pd_endpoints.split(',').next().unwrap_or("127.0.0.1:2379").trim();
        let base_url = if first.starts_with("http") {
            first.to_string()
        } else {
            format!("http://{first}")
        };
        Self { base_url, client: client.clone() }
    }

    pub async fn create_keyspace(&self, name: &str) -> bool {
        let url = format!("{}/pd/api/v2/keyspaces", self.base_url);
        let body = json!({
            "name": name,
            "config": { "gc_management_type": "global_gc" }
        });
        match self.client.post(&url).json(&body).send().await {
            Ok(resp) => resp.status().is_success(),
            Err(e) => { tracing::warn!("PD create_keyspace failed: {e}"); false }
        }
    }

    pub async fn get_keyspace(&self, name: &str) -> Option<serde_json::Value> {
        let url = format!("{}/pd/api/v2/keyspaces/{name}", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => resp.json().await.ok(),
            Ok(_) => None,
            Err(e) => { tracing::warn!("PD get_keyspace failed: {e}"); None }
        }
    }

    pub async fn disable_keyspace(&self, name: &str) -> bool {
        let url = format!("{}/pd/api/v2/keyspaces/{name}/state", self.base_url);
        let body = json!({ "action": "DISABLE" });
        match self.client.put(&url).json(&body).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return true;
                }
                // Already disabled or not found — treat as success for idempotent removal
                if status == reqwest::StatusCode::NOT_FOUND
                    || status == reqwest::StatusCode::CONFLICT
                {
                    tracing::info!("PD disable_keyspace {name}: {status} (treating as success)");
                    return true;
                }
                let body_text = resp.text().await.unwrap_or_default();
                tracing::warn!("PD disable_keyspace {name} failed: {status} {body_text}");
                false
            }
            Err(e) => { tracing::warn!("PD disable_keyspace failed: {e}"); false }
        }
    }

    pub async fn list_keyspaces(&self) -> Vec<serde_json::Value> {
        let url = format!("{}/pd/api/v2/keyspaces", self.base_url);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = resp.json().await.unwrap_or_default();
                body.get("keyspaces")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default()
            }
            _ => Vec::new(),
        }
    }

    pub async fn check_health(&self) -> bool {
        let url = format!("{}/pd/api/v1/health", self.base_url);
        matches!(self.client.get(&url).send().await, Ok(r) if r.status().is_success())
    }
}
