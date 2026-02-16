use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub struct Fs9Client {
    base_url: String,
    meta_key: String,
    client: reqwest::Client,
}

#[derive(Serialize)]
struct CreateNamespaceRequest {
    name: String,
}

#[derive(Serialize)]
struct CreateMountRequest {
    path: String,
    provider: String,
    config: serde_json::Value,
}

#[derive(Serialize)]
struct GenerateTokenRequest {
    user_id: String,
    namespace: String,
    roles: Vec<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

impl Fs9Client {
    pub fn new(base_url: String, meta_key: String, client: reqwest::Client) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            meta_key,
            client,
        }
    }

    /// Create an fs9 namespace for a tenant.
    pub async fn create_namespace(&self, name: &str) -> Result<(), String> {
        let url = format!("{}/api/v1/namespaces", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-fs9-meta-key", &self.meta_key)
            .json(&CreateNamespaceRequest {
                name: name.to_string(),
            })
            .send()
            .await
            .map_err(|e| format!("fs9 create_namespace request failed: {e}"))?;

        if resp.status().is_success() || resp.status().as_u16() == 409 {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!(
                "fs9 create_namespace failed: status={status}, body={body}"
            ))
        }
    }

    /// Create a pagefs mount backed by TiKV for the given namespace.
    pub async fn create_mount(
        &self,
        namespace: &str,
        pd_endpoints: &[String],
        keyspace: &str,
        ca_path: Option<&str>,
        cert_path: Option<&str>,
        key_path: Option<&str>,
    ) -> Result<(), String> {
        let url = format!("{}/api/v1/namespaces/{}/mounts", self.base_url, namespace);

        let mut tikv_config = serde_json::json!({
            "type": "tikv",
            "pd_endpoints": pd_endpoints,
            "keyspace": keyspace,
        });

        if let Some(ca) = ca_path {
            tikv_config["ca_path"] = serde_json::Value::String(ca.to_string());
        }
        if let Some(cert) = cert_path {
            tikv_config["cert_path"] = serde_json::Value::String(cert.to_string());
        }
        if let Some(key) = key_path {
            tikv_config["key_path"] = serde_json::Value::String(key.to_string());
        }

        let config = serde_json::json!({
            "uid": 1000,
            "gid": 1000,
            "backend": tikv_config,
        });

        let resp = self
            .client
            .post(&url)
            .header("x-fs9-meta-key", &self.meta_key)
            .json(&CreateMountRequest {
                path: "/".to_string(),
                provider: "pagefs".to_string(),
                config,
            })
            .send()
            .await
            .map_err(|e| format!("fs9 create_mount request failed: {e}"))?;

        if resp.status().is_success() || resp.status().as_u16() == 409 {
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!(
                "fs9 create_mount failed: status={status}, body={body}"
            ))
        }
    }

    /// Generate a JWT token for accessing the filesystem namespace.
    pub async fn generate_token(&self, user_id: &str, namespace: &str) -> Result<String, String> {
        let url = format!("{}/api/v1/tokens/generate", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-fs9-meta-key", &self.meta_key)
            .json(&GenerateTokenRequest {
                user_id: user_id.to_string(),
                namespace: namespace.to_string(),
                roles: vec!["admin".to_string()],
            })
            .send()
            .await
            .map_err(|e| format!("fs9 generate_token request failed: {e}"))?;

        if resp.status().is_success() {
            let token_resp: TokenResponse = resp
                .json()
                .await
                .map_err(|e| format!("fs9 generate_token parse failed: {e}"))?;
            Ok(token_resp.token)
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!(
                "fs9 generate_token failed: status={status}, body={body}"
            ))
        }
    }
}
