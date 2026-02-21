//! Unit tests for session settings.

#[cfg(test)]
mod tests {
    use crate::sql::session::settings::SessionSettings;
    use std::time::Duration;

    #[test]
    fn test_session_settings_new_with_defaults() {
        let settings = SessionSettings::new_with_defaults(1_500, 2_500);

        assert_eq!(settings.statement_timeout_ms, 1_500);
        assert_eq!(settings.default_statement_timeout_ms, 1_500);
        assert_eq!(settings.idle_in_transaction_session_timeout_ms, 2_500);
        assert_eq!(
            settings.default_idle_in_transaction_session_timeout_ms,
            2_500
        );
        assert_eq!(
            settings.statement_timeout(),
            Some(Duration::from_millis(1_500))
        );
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("2500ms")
        );
    }

    #[test]
    fn test_session_settings_defaults_and_overrides() {
        let mut settings = SessionSettings::new();

        assert_eq!(
            settings.show_value("server_version").as_deref(),
            Some("16.0")
        );
        assert_eq!(
            settings.show_value("server_version_num").as_deref(),
            Some("160000")
        );
        assert_eq!(
            settings.show_value("server_encoding").as_deref(),
            Some("UTF8")
        );
        assert_eq!(
            settings.show_value("datestyle").as_deref(),
            Some("ISO, MDY")
        );
        assert_eq!(
            settings.show_value("integer_datetimes").as_deref(),
            Some("on")
        );
        assert_eq!(
            settings.show_value("intervalstyle").as_deref(),
            Some("postgres")
        );
        assert_eq!(settings.show_value("timezone").as_deref(), Some("UTC"));
        assert_eq!(settings.show_value("application_name").as_deref(), Some(""));
        assert_eq!(
            settings.show_value("search_path").as_deref(),
            Some("\"$user\", public")
        );
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("0")
        );
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("268435456")
        );
        assert_eq!(
            settings.show_value("client_encoding").as_deref(),
            Some("UTF8")
        );

        assert!(settings
            .set_known_setting("client_min_messages", "warning".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("client_min_messages").as_deref(),
            Some("warning")
        );

        assert!(settings
            .set_known_setting("timezone", "Asia/Shanghai".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("Asia/Shanghai")
        );

        assert!(settings
            .set_known_setting("timezone", "localtime".to_string())
            .is_err());
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("Asia/Shanghai")
        );

        assert!(settings
            .set_known_setting("application_name", "pg-tikv-tests".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("application_name").as_deref(),
            Some("pg-tikv-tests")
        );

        // Unknown GUCs are stored in extra_settings for driver compatibility
        assert!(settings
            .set_known_setting("unknown_setting", "x".to_string())
            .unwrap());
        assert_eq!(settings.show_value("unknown_setting").as_deref(), Some("x"));

        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );
        assert_eq!(
            settings
                .show_value("transaction.isolation.level")
                .as_deref(),
            Some("repeatable read")
        );
    }

    #[test]
    fn test_session_settings_client_encoding_utf8_only() {
        let mut settings = SessionSettings::new();

        assert!(settings
            .set_known_setting("client_encoding", "LATIN1".to_string())
            .is_err());
        assert_eq!(
            settings.show_value("client_encoding").as_deref(),
            Some("UTF8")
        );

        assert!(settings
            .set_known_setting("client_encoding", "UTF-8".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("client_encoding").as_deref(),
            Some("UTF8")
        );
    }

    #[test]
    fn test_session_settings_timeout_parsing() {
        let mut settings = SessionSettings::new();

        settings
            .set_known_setting("statement_timeout", "20".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("20ms")
        );

        settings
            .set_known_setting("statement_timeout", "1s".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("1000ms")
        );

        assert!(settings
            .set_known_setting("statement_timeout", "-1".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("statement_timeout", "abc".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("statement_timeout", "1unknown".to_string())
            .is_err());
    }

    #[test]
    fn test_session_settings_max_sort_bytes_parsing() {
        let mut settings = SessionSettings::new();

        settings
            .set_known_setting("pgtikv.max_sort_bytes", "268435456".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("268435456")
        );

        settings
            .set_known_setting("pgtikv.max_sort_bytes", "256MB".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("268435456")
        );

        settings
            .set_known_setting("pgtikv.max_sort_bytes", "1gb".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("1073741824")
        );

        settings
            .set_known_setting("pgtikv.max_sort_bytes", "0".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("pgtikv.max_sort_bytes").as_deref(),
            Some("0")
        );

        assert!(settings
            .set_known_setting("pgtikv.max_sort_bytes", "-1".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("pgtikv.max_sort_bytes", "1TB".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("pgtikv.max_sort_bytes", "abc".to_string())
            .is_err());
    }

    #[test]
    fn test_session_settings_transaction_isolation() {
        let mut settings = SessionSettings::new();

        // Default value — TiKV snapshot isolation = REPEATABLE READ
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );
        assert_eq!(
            settings
                .show_value("default_transaction_read_only")
                .as_deref(),
            Some("off")
        );

        // Set and readback
        assert!(settings
            .set_known_setting("transaction_isolation", "repeatable read".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );
        // SHOW transaction isolation level parses as "transaction.isolation.level"
        assert_eq!(
            settings
                .show_value("transaction.isolation.level")
                .as_deref(),
            Some("repeatable read")
        );

        assert!(settings
            .set_known_setting("default_transaction_read_only", "on".to_string())
            .unwrap());
        assert_eq!(
            settings
                .show_value("default_transaction_read_only")
                .as_deref(),
            Some("on")
        );

        // Reset back
        assert!(settings
            .set_known_setting("default_transaction_read_only", "off".to_string())
            .unwrap());
        assert_eq!(
            settings
                .show_value("default_transaction_read_only")
                .as_deref(),
            Some("off")
        );

        // SERIALIZABLE must be rejected
        assert!(settings
            .set_known_setting("transaction_isolation", "serializable".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("transaction_isolation", "SERIALIZABLE".to_string())
            .is_err());
        // Value should remain unchanged after rejection
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );

        // Garbage values must be rejected
        assert!(settings
            .set_known_setting("transaction_isolation", "garbage".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("transaction_isolation", "snapshot".to_string())
            .is_err());

        // READ UNCOMMITTED is upgraded to REPEATABLE READ (TiKV snapshot isolation)
        assert!(settings
            .set_known_setting("transaction_isolation", "read uncommitted".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );

        // READ COMMITTED is also upgraded to REPEATABLE READ
        assert!(settings
            .set_known_setting("transaction_isolation", "read committed".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("repeatable read")
        );

        // default_transaction_read_only: boolean aliases
        for val in &["true", "yes", "1", "on", "TRUE", "Yes"] {
            assert!(settings
                .set_known_setting("default_transaction_read_only", val.to_string())
                .unwrap());
            assert_eq!(
                settings
                    .show_value("default_transaction_read_only")
                    .as_deref(),
                Some("on")
            );
        }
        for val in &["false", "no", "0", "off", "FALSE", "No"] {
            assert!(settings
                .set_known_setting("default_transaction_read_only", val.to_string())
                .unwrap());
            assert_eq!(
                settings
                    .show_value("default_transaction_read_only")
                    .as_deref(),
                Some("off")
            );
        }
        // Garbage boolean must be rejected
        assert!(settings
            .set_known_setting("default_transaction_read_only", "maybe".to_string())
            .is_err());
    }

    #[test]
    fn test_session_settings_reset_setting() {
        let mut settings = SessionSettings::new();

        // Change a few settings
        settings
            .set_known_setting("statement_timeout", "5000".to_string())
            .unwrap();
        settings
            .set_known_setting("timezone", "US/Eastern".to_string())
            .unwrap();
        settings
            .set_known_setting("extra_float_digits", "3".to_string())
            .unwrap();

        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("5000ms")
        );
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("US/Eastern")
        );
        assert_eq!(
            settings.show_value("extra_float_digits").as_deref(),
            Some("3")
        );

        // Reset individual settings
        settings.reset_setting("statement_timeout");
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("0")
        );

        settings.reset_setting("timezone");
        assert_eq!(settings.show_value("timezone").as_deref(), Some("UTC"));

        settings.reset_setting("extra_float_digits");
        assert_eq!(
            settings.show_value("extra_float_digits").as_deref(),
            Some("1")
        );
    }

    #[test]
    fn test_default_value_fallback_and_set_precedence() {
        let mut settings = SessionSettings::new();

        // Before SET: fallback table provides defaults.
        assert_eq!(
            settings.show_value("extra_float_digits").as_deref(),
            Some("1")
        );
        assert_eq!(settings.show_value("bytea_output").as_deref(), Some("hex"));
        assert_eq!(
            settings.show_value("max_identifier_length").as_deref(),
            Some("63")
        );
        assert_eq!(
            settings.show_value("default_text_search_config").as_deref(),
            Some(crate::sql::fts_tokenizers::default_text_search_config())
        );

        // SET overrides fallback via extra_settings.
        settings
            .set_known_setting("extra_float_digits", "3".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("extra_float_digits").as_deref(),
            Some("3")
        );
        settings
            .set_known_setting("default_text_search_config", "english".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("default_text_search_config").as_deref(),
            Some("english")
        );

        // RESET restores fallback values.
        settings.reset_setting("extra_float_digits");
        assert_eq!(
            settings.show_value("extra_float_digits").as_deref(),
            Some("1")
        );
        settings.reset_setting("default_text_search_config");
        assert_eq!(
            settings.show_value("default_text_search_config").as_deref(),
            Some(crate::sql::fts_tokenizers::default_text_search_config())
        );

        // Unknown GUC remains unknown.
        assert_eq!(settings.show_value("totally_unknown_guc"), None);
    }

    #[test]
    fn test_session_settings_reset_all() {
        let mut settings = SessionSettings::new();

        settings
            .set_known_setting("statement_timeout", "5000".to_string())
            .unwrap();
        settings
            .set_known_setting("timezone", "US/Eastern".to_string())
            .unwrap();
        settings
            .set_known_setting("application_name", "myapp".to_string())
            .unwrap();

        settings.reset_all_settings();

        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("0")
        );
        assert_eq!(settings.show_value("timezone").as_deref(), Some("UTC"));
        assert_eq!(settings.show_value("application_name").as_deref(), Some(""));
        assert_eq!(
            settings.show_value("search_path").as_deref(),
            Some("\"$user\", public")
        );
    }

    #[test]
    fn test_session_settings_reset_preserves_set_sentinel_value() {
        // SET application_name TO '__RESET__' must NOT trigger a reset — it's a normal SET.
        let mut settings = SessionSettings::new();
        settings
            .set_known_setting("application_name", "__RESET__".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("application_name").as_deref(),
            Some("__RESET__")
        );
    }

    #[test]
    fn test_in_failed_sql_transaction_message() {
        use crate::sql::error::SqlError;
        assert_eq!(
            SqlError::InFailedTransaction.to_string(),
            "current transaction is aborted, commands ignored until end of transaction block"
        );
    }

    #[test]
    fn test_reset_statement_timeout_restores_server_default() {
        let mut settings = SessionSettings::new_with_defaults(5_000, 3_000);

        // Verify initial value is the server default
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("5000ms")
        );

        // Change it
        settings
            .set_known_setting("statement_timeout", "10000".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("10000ms")
        );

        // RESET should restore to server default (5000), not 0
        settings.reset_setting("statement_timeout");
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("5000ms")
        );
        assert_eq!(
            settings.statement_timeout(),
            Some(Duration::from_millis(5_000))
        );
    }

    #[test]
    fn test_reset_idle_in_transaction_timeout_restores_server_default() {
        let mut settings = SessionSettings::new_with_defaults(5_000, 3_000);

        // Verify initial value
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("3000ms")
        );

        // Change it
        settings
            .set_known_setting("idle_in_transaction_session_timeout", "10000".to_string())
            .unwrap();
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("10000ms")
        );

        // RESET should restore to server default (3000), not 0
        settings.reset_setting("idle_in_transaction_session_timeout");
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("3000ms")
        );
    }

    #[test]
    fn test_reset_all_restores_server_defaults() {
        let mut settings = SessionSettings::new_with_defaults(5_000, 3_000);

        // Change both timeouts
        settings
            .set_known_setting("statement_timeout", "20000".to_string())
            .unwrap();
        settings
            .set_known_setting("idle_in_transaction_session_timeout", "15000".to_string())
            .unwrap();

        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("20000ms")
        );
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("15000ms")
        );

        // RESET ALL should restore both to server defaults
        settings.reset_all_settings();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("5000ms")
        );
        assert_eq!(
            settings
                .show_value("idle_in_transaction_session_timeout")
                .as_deref(),
            Some("3000ms")
        );
    }

    #[test]
    fn test_statement_timeout_getter_with_nonzero_default() {
        let settings = SessionSettings::new_with_defaults(5_000, 0);
        assert_eq!(
            settings.statement_timeout(),
            Some(Duration::from_millis(5_000))
        );

        // Zero default means no timeout
        let settings_zero = SessionSettings::new_with_defaults(0, 0);
        assert_eq!(settings_zero.statement_timeout(), None);
    }

    #[test]
    fn test_parse_timeout_value() {
        // Pure numeric (milliseconds)
        assert_eq!(SessionSettings::parse_timeout_value("5000").unwrap(), 5_000);
        assert_eq!(SessionSettings::parse_timeout_value("0").unwrap(), 0);

        // With suffix
        assert_eq!(SessionSettings::parse_timeout_value("1s").unwrap(), 1_000);
        assert_eq!(SessionSettings::parse_timeout_value("5s").unwrap(), 5_000);
        assert_eq!(SessionSettings::parse_timeout_value("100ms").unwrap(), 100);
        assert_eq!(
            SessionSettings::parse_timeout_value("1min").unwrap(),
            60_000
        );
        assert_eq!(
            SessionSettings::parse_timeout_value("2h").unwrap(),
            7_200_000
        );

        // Invalid
        assert!(SessionSettings::parse_timeout_value("-1").is_err());
        assert!(SessionSettings::parse_timeout_value("abc").is_err());
        assert!(SessionSettings::parse_timeout_value("1unknown").is_err());
    }
}
