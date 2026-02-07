use pgwire::api::auth::ServerParameterProvider;
use pgwire::api::ClientInfo;
use std::collections::HashMap;

pub struct PgServerParameterProvider;

impl ServerParameterProvider for PgServerParameterProvider {
    fn server_parameters<C: ClientInfo>(&self, _client: &C) -> Option<HashMap<String, String>> {
        let mut params = HashMap::new();
        params.insert("server_version".to_owned(), "16.0".to_owned());
        params.insert("server_version_num".to_owned(), "160000".to_owned());
        params.insert("server_encoding".to_owned(), "UTF8".to_owned());
        params.insert("client_encoding".to_owned(), "UTF8".to_owned());
        params.insert("DateStyle".to_owned(), "ISO, MDY".to_owned());
        params.insert("TimeZone".to_owned(), "UTC".to_owned());
        params.insert("standard_conforming_strings".to_owned(), "on".to_owned());
        Some(params)
    }
}
