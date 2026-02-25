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
struct CreateUserRequest {
    username: String,
}

#[derive(Deserialize)]
struct UserResponse {
    id: String,
    username: String,
}

#[derive(Serialize)]
struct GenerateTokenRequest {
    user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl_seconds: Option<u64>,
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
        let url = format!("{}/api/v1/admin/namespaces", self.base_url);
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

    /// Create a user (per-namespace); returns the user's internal ID.
    /// If user already exists (409), fetches and returns the existing ID.
    pub async fn create_user(&self, namespace: &str, username: &str) -> Result<String, String> {
        let url = format!(
            "{}/api/v1/admin/namespaces/{}/users",
            self.base_url, namespace
        );
        let resp = self
            .client
            .post(&url)
            .header("x-fs9-meta-key", &self.meta_key)
            .json(&CreateUserRequest {
                username: username.to_string(),
            })
            .send()
            .await
            .map_err(|e| format!("fs9 create_user request failed: {e}"))?;

        if resp.status().is_success() {
            let user: UserResponse = resp
                .json()
                .await
                .map_err(|e| format!("fs9 create_user parse failed: {e}"))?;
            Ok(user.id)
        } else if resp.status().as_u16() == 409 {
            // User already exists — look up by username.
            self.get_user_id(namespace, username).await
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!(
                "fs9 create_user failed: status={status}, body={body}"
            ))
        }
    }

    /// Fetch the internal user ID for an existing user by username.
    async fn get_user_id(&self, namespace: &str, username: &str) -> Result<String, String> {
        let url = format!(
            "{}/api/v1/admin/namespaces/{}/users",
            self.base_url, namespace
        );
        let resp = self
            .client
            .get(&url)
            .header("x-fs9-meta-key", &self.meta_key)
            .send()
            .await
            .map_err(|e| format!("fs9 list_users request failed: {e}"))?;

        if resp.status().is_success() {
            let users: Vec<UserResponse> = resp
                .json()
                .await
                .map_err(|e| format!("fs9 list_users parse failed: {e}"))?;
            users
                .into_iter()
                .find(|u| u.username == username)
                .map(|u| u.id)
                .ok_or_else(|| {
                    format!(
                        "fs9 user '{}' not found in namespace '{}'",
                        username, namespace
                    )
                })
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!(
                "fs9 list_users failed: status={status}, body={body}"
            ))
        }
    }

    /// Generate a JWT token for the given user in a namespace.
    /// Uses a 1-year TTL to avoid frequent token expiration.
    pub async fn generate_token(&self, user_id: &str, _namespace: &str) -> Result<String, String> {
        let url = format!("{}/api/v1/admin/tokens", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-fs9-meta-key", &self.meta_key)
            .json(&GenerateTokenRequest {
                user_id: user_id.to_string(),
                ttl_seconds: Some(365 * 24 * 3600), // 1 year
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
