use std::collections::HashMap;
use std::io::{self, Write};
use std::process;

use serde_json::Value;

// ── Credential helpers ──────────────────────────────────────────

fn credentials_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".db9").join("credentials"))
}

fn ensure_config_dir() -> std::path::PathBuf {
    let dir = dirs::home_dir()
        .unwrap_or_else(|| {
            eprintln!("Cannot determine home directory");
            process::exit(1);
        })
        .join(".db9");
    if !dir.exists() {
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| {
            eprintln!("Failed to create config directory: {e}");
            process::exit(1);
        });
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o700);
            std::fs::set_permissions(&dir, perms).ok();
        }
    }
    dir
}

fn save_credentials(token: &str) -> Result<(), String> {
    let dir = ensure_config_dir();
    let cred_path = dir.join("credentials");
    let existing = std::fs::read_to_string(&cred_path).unwrap_or_default();
    let mut parsed: toml::Table = existing.parse().unwrap_or_default();
    parsed.insert("token".to_string(), toml::Value::String(token.to_string()));
    let content =
        toml::to_string(&parsed).map_err(|e| format!("Failed to serialize credentials: {e}"))?;
    std::fs::write(&cred_path, &content).map_err(|e| format!("Failed to save credentials: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&cred_path, perms)
            .map_err(|e| format!("Failed to set file permissions: {e}"))?;
    }
    Ok(())
}

fn save_anonymous_credentials(
    token: &str,
    anonymous_id: &str,
    anonymous_secret: &str,
) -> Result<(), String> {
    let dir = ensure_config_dir();
    let cred_path = dir.join("credentials");
    let mut parsed = toml::Table::new();
    parsed.insert("token".to_string(), toml::Value::String(token.to_string()));
    parsed.insert("is_anonymous".to_string(), toml::Value::Boolean(true));
    parsed.insert(
        "anonymous_id".to_string(),
        toml::Value::String(anonymous_id.to_string()),
    );
    parsed.insert(
        "anonymous_secret".to_string(),
        toml::Value::String(anonymous_secret.to_string()),
    );
    let content =
        toml::to_string(&parsed).map_err(|e| format!("Failed to serialize credentials: {e}"))?;
    std::fs::write(&cred_path, &content).map_err(|e| format!("Failed to save credentials: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&cred_path, perms)
            .map_err(|e| format!("Failed to set file permissions: {e}"))?;
    }
    Ok(())
}

fn clear_credentials() {
    if let Some(p) = credentials_path() {
        if p.exists() {
            std::fs::remove_file(&p).ok();
        }
    }
}

fn load_anonymous_credentials() -> Option<(String, String)> {
    let p = credentials_path()?;
    let content = std::fs::read_to_string(p).ok()?;
    let parsed: toml::Table = content.parse().ok()?;
    let id = parsed.get("anonymous_id")?.as_str()?.to_string();
    let secret = parsed.get("anonymous_secret")?.as_str()?.to_string();
    Some((id, secret))
}

fn prompt_line(label: &str) -> String {
    eprint!("{label}");
    io::stderr().flush().ok();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).unwrap_or_else(|e| {
        eprintln!("Failed to read input: {e}");
        process::exit(1);
    });
    buf.trim().to_string()
}

fn prompt_password_hidden(label: &str) -> String {
    rpassword::prompt_password(label).unwrap_or_else(|e| {
        eprintln!("Failed to read password: {e}");
        process::exit(1);
    })
}

// ── HTTP helper ──────────────────────────────────────────────────

type SendResult = Result<Value, (u16, String)>;

#[derive(Clone)]
pub struct ApiClient {
    base_url: String,
    api_key: Option<String>,
    client: reqwest::Client,
    auto_reauth: bool,
}

impl ApiClient {
    pub fn new(base_url: &str, api_key: Option<&str>) -> Self {
        let insecure = std::env::var("DB9_INSECURE")
            .map(|v| v == "1" || v == "true")
            .unwrap_or(false);
        Self::new_with_options(base_url, api_key, insecure)
    }

    pub fn new_with_options(base_url: &str, api_key: Option<&str>, insecure: bool) -> Self {
        let client = if insecure {
            reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .expect("Failed to build HTTP client")
        } else {
            reqwest::Client::new()
        };
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.map(|s| s.to_string()),
            client,
            auto_reauth: false,
        }
    }

    pub fn with_auto_reauth(mut self) -> Self {
        self.auto_reauth = true;
        self
    }

    async fn send_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> SendResult {
        let url = format!("{}{path}", self.base_url);
        let mut req = match method {
            "GET" => self.client.get(&url),
            "POST" => self.client.post(&url),
            "PUT" => self.client.put(&url),
            "DELETE" => self.client.delete(&url),
            _ => self.client.get(&url),
        };

        req = req.header("Content-Type", "application/json");

        if let Some(key) = &self.api_key {
            req = req.header("X-API-Key", key);
        }
        if let Some(hdrs) = extra_headers {
            for (k, v) in hdrs {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        if let Some(b) = body {
            req = req.json(b);
        }

        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                if status.is_success() {
                    Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
                } else {
                    let err: Value = serde_json::from_str(&text).unwrap_or_default();
                    let detail = err
                        .get("message")
                        .or_else(|| err.get("detail"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Unknown error")
                        .to_string();
                    Err((status.as_u16(), detail))
                }
            }
            Err(e) => Err((0, format!("Connection failed: {e}"))),
        }
    }

    /// Non-fatal variant of `request` — returns errors as `Err` instead of
    /// calling `process::exit`.  Used by the interactive shell so a single bad
    /// query does not kill the REPL.
    pub async fn try_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Result<Value, (u16, String)> {
        self.send_request(method, path, body, extra_headers).await
    }

    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        extra_headers: Option<&HashMap<String, String>>,
    ) -> Value {
        match self.send_request(method, path, body, extra_headers).await {
            Ok(val) => val,
            Err((status, detail)) => {
                if status == 401 && self.auto_reauth {
                    let new_token = 'reauth: {
                        if let Some((anon_id, anon_secret)) = load_anonymous_credentials() {
                            if let Some(token) =
                                self.anonymous_refresh(&anon_id, &anon_secret).await
                            {
                                if let Err(e) = save_credentials(&token) {
                                    eprintln!("{e}");
                                    process::exit(1);
                                }
                                break 'reauth token;
                            }
                        }

                        clear_credentials();

                        eprintln!("\nSession expired or invalid token.");
                        eprintln!("Please re-authenticate to continue.\n");

                        let choice = prompt_line(
                            "[A] Continue anonymously (default)  [L] Login  [R] Register: ",
                        );
                        match choice.to_ascii_lowercase().as_str() {
                            "l" => {
                                let token = self.interactive_login().await;
                                if let Err(e) = save_credentials(&token) {
                                    eprintln!("{e}");
                                    process::exit(1);
                                }
                                token
                            }
                            "r" => {
                                let token = self.interactive_register().await;
                                if let Err(e) = save_credentials(&token) {
                                    eprintln!("{e}");
                                    process::exit(1);
                                }
                                token
                            }
                            _ => self.interactive_anonymous().await,
                        }
                    };

                    let mut retry_headers = extra_headers.cloned().unwrap_or_default();
                    retry_headers
                        .insert("Authorization".to_string(), format!("Bearer {new_token}"));

                    match self
                        .send_request(method, path, body, Some(&retry_headers))
                        .await
                    {
                        Ok(val) => val,
                        Err((retry_status, retry_detail)) => {
                            eprintln!("Error {}: {retry_detail}", retry_status);
                            process::exit(1);
                        }
                    }
                } else {
                    if status == 0 {
                        eprintln!("{detail}");
                    } else {
                        eprintln!("Error {status}: {detail}");
                    }
                    process::exit(1);
                }
            }
        }
    }

    async fn interactive_login(&self) -> String {
        let email = prompt_line("Email: ");
        let password = prompt_password_hidden("Password: ");

        let body = serde_json::json!({
            "email": email,
            "password": password,
        });

        match self
            .send_request("POST", "/customer/login", Some(&body), None)
            .await
        {
            Ok(data) => match data["token"].as_str() {
                Some(t) => t.to_string(),
                None => {
                    eprintln!("Login failed: no token in response");
                    process::exit(1);
                }
            },
            Err((status, detail)) => {
                eprintln!("Login failed ({}): {detail}", status);
                process::exit(1);
            }
        }
    }

    async fn interactive_register(&self) -> String {
        let email = prompt_line("Email: ");
        let password = prompt_password_hidden("Password: ");
        let confirm = prompt_password_hidden("Confirm password: ");

        if password != confirm {
            eprintln!("Passwords do not match.");
            process::exit(1);
        }

        let body = serde_json::json!({
            "email": email,
            "password": password,
        });

        match self
            .send_request("POST", "/customer/register", Some(&body), None)
            .await
        {
            Ok(_) => eprintln!("Account created. Logging in..."),
            Err((status, detail)) => {
                eprintln!("Registration failed ({}): {detail}", status);
                process::exit(1);
            }
        }

        let login_body = serde_json::json!({
            "email": email,
            "password": password,
        });

        match self
            .send_request("POST", "/customer/login", Some(&login_body), None)
            .await
        {
            Ok(data) => match data["token"].as_str() {
                Some(t) => t.to_string(),
                None => {
                    eprintln!("Login after registration failed: no token in response");
                    process::exit(1);
                }
            },
            Err((status, detail)) => {
                eprintln!("Login after registration failed ({}): {detail}", status);
                process::exit(1);
            }
        }
    }

    async fn interactive_anonymous(&self) -> String {
        match self
            .send_request("POST", "/customer/anonymous-register", None::<&Value>, None)
            .await
        {
            Ok(data) => {
                let token = data["token"].as_str().unwrap_or_else(|| {
                    eprintln!("Failed to create anonymous account");
                    process::exit(1);
                });
                let anon_id = data["anonymous_id"].as_str().unwrap_or("");
                let anon_secret = data["anonymous_secret"].as_str().unwrap_or("");

                if !anon_id.is_empty() && !anon_secret.is_empty() {
                    if let Err(e) = save_anonymous_credentials(token, anon_id, anon_secret) {
                        eprintln!("{e}");
                        process::exit(1);
                    }
                } else if let Err(e) = save_credentials(token) {
                    eprintln!("{e}");
                    process::exit(1);
                }

                eprintln!("Anonymous account created. You can claim it later with 'db9 claim'.");
                token.to_string()
            }
            Err((status, detail)) => {
                eprintln!("Anonymous registration failed ({}): {detail}", status);
                process::exit(1);
            }
        }
    }

    async fn anonymous_refresh(
        &self,
        anonymous_id: &str,
        anonymous_secret: &str,
    ) -> Option<String> {
        let body = serde_json::json!({
            "anonymous_id": anonymous_id,
            "anonymous_secret": anonymous_secret,
        });

        match self
            .send_request("POST", "/customer/anonymous-refresh", Some(&body), None)
            .await
        {
            Ok(data) => data["token"].as_str().map(|t| t.to_string()),
            Err(_) => None,
        }
    }
}

// ── Table printer ────────────────────────────────────────────────

pub fn print_table(rows: &[Value], columns: &[(&str, &str, usize)]) {
    if rows.is_empty() {
        println!("(empty)");
        return;
    }

    let widths: Vec<usize> = columns
        .iter()
        .map(|(header, key, min_w)| {
            let max_val = rows
                .iter()
                .map(|r| format_val(r.get(key)).len())
                .max()
                .unwrap_or(0);
            header.len().max(max_val).max(*min_w)
        })
        .collect();

    // Header
    let header: String = columns
        .iter()
        .zip(&widths)
        .map(|((h, _, _), w)| format!("{:<width$}", h, width = w))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{header}");

    // Separator
    let sep: String = widths
        .iter()
        .map(|w| "─".repeat(*w))
        .collect::<Vec<_>>()
        .join("  ");
    println!("{sep}");

    // Rows
    for row in rows {
        let line: String = columns
            .iter()
            .zip(&widths)
            .map(|((_, key, _), w)| {
                let v = format_val(row.get(key));
                format!("{:<width$}", v, width = w)
            })
            .collect::<Vec<_>>()
            .join("  ");
        println!("{line}");
    }
}

pub fn format_val(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "-".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Array(arr)) => {
            if arr.is_empty() {
                "-".to_string()
            } else {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
        Some(other) => other.to_string(),
    }
}

pub fn format_time(v: Option<&Value>) -> String {
    match v.and_then(|v| v.as_str()) {
        None => "-".to_string(),
        Some(s) => {
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                dt.format("%Y-%m-%d %H:%M").to_string()
            } else {
                s.to_string()
            }
        }
    }
}

pub fn print_csv(rows: &[Value], columns: &[(&str, &str, usize)]) {
    if rows.is_empty() {
        println!("(empty)");
        return;
    }

    let headers: Vec<&str> = columns.iter().map(|(h, _, _)| *h).collect();
    println!("{}", headers.join(","));

    for row in rows {
        let values: Vec<String> = columns
            .iter()
            .map(|(_, key, _)| {
                let val = format_val(row.get(key));
                if val.contains(',') || val.contains('"') || val.contains('\n') {
                    format!("\"{}\"", val.replace('"', "\"\""))
                } else {
                    val
                }
            })
            .collect();
        println!("{}", values.join(","));
    }
}

pub fn print_json(data: &Value) {
    println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn format_val_string() {
        let v = json!("hello");
        assert_eq!(format_val(Some(&v)), "hello");
    }

    #[test]
    fn format_val_null() {
        assert_eq!(format_val(None), "-");
        assert_eq!(format_val(Some(&Value::Null)), "-");
    }

    #[test]
    fn format_val_number() {
        let v = json!(42);
        assert_eq!(format_val(Some(&v)), "42");
    }

    #[test]
    fn format_val_bool() {
        let v = json!(true);
        assert_eq!(format_val(Some(&v)), "true");
    }

    #[test]
    fn format_val_empty_array() {
        let v = json!([]);
        assert_eq!(format_val(Some(&v)), "-");
    }

    #[test]
    fn format_val_string_array() {
        let v = json!(["a", "b", "c"]);
        assert_eq!(format_val(Some(&v)), "a, b, c");
    }

    #[test]
    fn format_val_object_falls_through() {
        let v = json!({"key": "val"});
        let result = format_val(Some(&v));
        assert!(result.contains("key"));
    }

    #[test]
    fn format_time_valid_rfc3339() {
        let v = json!("2026-02-10T19:30:00+00:00");
        assert_eq!(format_time(Some(&v)), "2026-02-10 19:30");
    }

    #[test]
    fn format_time_null() {
        assert_eq!(format_time(None), "-");
    }

    #[test]
    fn format_time_invalid() {
        let v = json!("not-a-date");
        assert_eq!(format_time(Some(&v)), "not-a-date");
    }

    #[test]
    fn format_time_json_null() {
        assert_eq!(format_time(Some(&Value::Null)), "-");
    }
}
