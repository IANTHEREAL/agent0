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
    roles: Vec<String>,
}

#[derive(Deserialize)]
struct CreateUserResponse {
    id: String,
}

#[derive(Serialize)]
struct GenerateTokenRequest {
    user_id: String,
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

    /// Create an admin user in the given namespace; returns the user's internal ID.
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
                roles: vec!["admin".to_string()],
            })
            .send()
            .await
            .map_err(|e| format!("fs9 create_user request failed: {e}"))?;

        if resp.status().is_success() {
            let user_resp: CreateUserResponse = resp
                .json()
                .await
                .map_err(|e| format!("fs9 create_user parse failed: {e}"))?;
            Ok(user_resp.id)
        } else if resp.status().as_u16() == 409 {
            // User already exists — fetch the user list and return the matching id.
            self.get_user_id(namespace, username).await
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            Err(format!(
                "fs9 create_user failed: status={status}, body={body}"
            ))
        }
    }

    /// Fetch the internal user ID for an existing user in a namespace.
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
            .map_err(|e| format!("fs9 get_users request failed: {e}"))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("fs9 get_users failed: status={status}, body={body}"));
        }

        #[derive(Deserialize)]
        struct UserEntry {
            id: String,
            username: String,
        }

        let users: Vec<UserEntry> = resp
            .json()
            .await
            .map_err(|e| format!("fs9 get_users parse failed: {e}"))?;

        users
            .into_iter()
            .find(|u| u.username == username)
            .map(|u| u.id)
            .ok_or_else(|| format!("fs9 user '{username}' not found in namespace '{namespace}'"))
    }

    /// Generate a JWT token for the given user (by internal user_id).
    pub async fn generate_token(&self, user_id: &str) -> Result<String, String> {
        let url = format!("{}/api/v1/admin/tokens", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-fs9-meta-key", &self.meta_key)
            .json(&GenerateTokenRequest {
                user_id: user_id.to_string(),
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
