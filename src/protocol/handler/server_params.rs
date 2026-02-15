use pgwire::api::auth::ServerParameterProvider;
use pgwire::api::ClientInfo;
use pgwire::api::METADATA_USER;
use std::collections::HashMap;

use crate::sql::SessionSettings;

pub struct PgServerParameterProvider;

impl ServerParameterProvider for PgServerParameterProvider {
    fn server_parameters<C: ClientInfo>(&self, client: &C) -> Option<HashMap<String, String>> {
        // Keep pgwire `ParameterStatus` consistent with SQL-level `SHOW` /
        // `current_setting()` readbacks by deriving values from the same
        // `SessionSettings` defaults + startup overrides.
        let mut settings = SessionSettings::new();
        if let Some(options) = client.metadata().get("options") {
            for (key, value) in super::parse_startup_options(options) {
                let _ = settings.set_known_setting(&key.to_ascii_lowercase(), value);
            }
        }
        if let Some(app_name) = client.metadata().get("application_name") {
            let _ = settings.set_known_setting("application_name", app_name.clone());
        }

        let session_authorization = client
            .metadata()
            .get(super::METADATA_ACTUAL_USER)
            .or_else(|| client.metadata().get(METADATA_USER))
            .cloned()
            .unwrap_or_else(|| "postgres".to_string());
        let is_superuser = client
            .metadata()
            .get(super::METADATA_AUTH_IS_SUPERUSER)
            .cloned()
            .unwrap_or_else(|| "off".to_string());

        let mut params = HashMap::new();
        let mut insert_setting = |ps_key: &str, show_key: &str| {
            if let Some(v) = settings.show_value(show_key) {
                params.insert(ps_key.to_owned(), v);
            }
        };

        insert_setting("server_version", "server_version");
        insert_setting("server_version_num", "server_version_num");
        insert_setting("server_encoding", "server_encoding");
        insert_setting("client_encoding", "client_encoding");
        insert_setting("DateStyle", "datestyle");
        insert_setting("IntervalStyle", "intervalstyle");
        insert_setting("integer_datetimes", "integer_datetimes");
        insert_setting("TimeZone", "timezone");
        insert_setting("standard_conforming_strings", "standard_conforming_strings");
        insert_setting("application_name", "application_name");
        insert_setting("search_path", "search_path");

        params.insert("session_authorization".to_owned(), session_authorization);
        params.insert("is_superuser".to_owned(), is_superuser);
        Some(params)
    }
}

#[cfg(test)]
mod tests {
    use super::PgServerParameterProvider;
    use pgwire::api::auth::ServerParameterProvider;
    use pgwire::api::{ClientInfo, DefaultClient, METADATA_USER};

    #[test]
    fn test_pg_server_parameter_provider_key_set() {
        let mut client = DefaultClient::<()>::new("127.0.0.1:0".parse().unwrap(), false);
        client
            .metadata_mut()
            .insert(METADATA_USER.to_string(), "admin".to_string());
        client.metadata_mut().insert(
            super::super::METADATA_AUTH_IS_SUPERUSER.to_string(),
            "on".to_string(),
        );
        client
            .metadata_mut()
            .insert("application_name".to_string(), "unit-test".to_string());
        client.metadata_mut().insert(
            "options".to_string(),
            "-c timezone=Asia/Shanghai -c client_encoding=LATIN1".to_string(),
        );

        let params = PgServerParameterProvider
            .server_parameters(&client)
            .expect("expected server parameters");

        for key in [
            "server_version",
            "server_version_num",
            "server_encoding",
            "client_encoding",
            "DateStyle",
            "IntervalStyle",
            "integer_datetimes",
            "TimeZone",
            "standard_conforming_strings",
            "application_name",
            "search_path",
            "session_authorization",
            "is_superuser",
        ] {
            assert!(
                params.contains_key(key),
                "missing ParameterStatus key '{key}'"
            );
        }

        assert_eq!(
            params.get("TimeZone").map(String::as_str),
            Some("Asia/Shanghai")
        );
        // Server is UTF-8 only: invalid startup options must not change advertised encoding.
        assert_eq!(
            params.get("client_encoding").map(String::as_str),
            Some("UTF8")
        );
        assert_eq!(
            params.get("application_name").map(String::as_str),
            Some("unit-test")
        );
        assert_eq!(
            params.get("search_path").map(String::as_str),
            Some("public, extensions")
        );

        assert_eq!(
            params.get("DateStyle").map(String::as_str),
            Some("ISO, MDY")
        );
        assert_eq!(
            params.get("IntervalStyle").map(String::as_str),
            Some("postgres")
        );
        assert_eq!(
            params.get("integer_datetimes").map(String::as_str),
            Some("on")
        );

        assert_eq!(
            params.get("session_authorization").map(String::as_str),
            Some("admin")
        );
        assert_eq!(params.get("is_superuser").map(String::as_str), Some("on"));
    }
}
