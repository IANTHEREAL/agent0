//! Unit tests for session settings.

#[cfg(test)]
#[allow(clippy::module_inception)]
mod tests {
    use crate::observability;
    use crate::sql::advisory_locks::{global_lock_manager, AdvisoryLockMode, AdvisoryLockScope};
    use crate::sql::session::settings::SessionSettings;
    use crate::sql::session::Session;
    use crate::storage::TikvStore;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
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
            settings.show_value("db9.max_sort_bytes").as_deref(),
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
            .set_known_setting("application_name", "db9-server-tests".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("application_name").as_deref(),
            Some("db9-server-tests")
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
    fn test_session_settings_embedding_model_is_syntax_only() {
        let mut settings = SessionSettings::new();
        assert!(settings
            .set_known_setting("embedding.model", "text-embedding-v4".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("embedding.model").as_deref(),
            Some("text-embedding-v4")
        );

        assert!(settings
            .set_known_setting(
                "embedding.model",
                "tidbcloud_free/amazon/titan-embed-text-v2".to_string()
            )
            .unwrap());
        assert_eq!(
            settings.show_value("embedding.model").as_deref(),
            Some("tidbcloud_free/amazon/titan-embed-text-v2")
        );

        assert!(settings
            .set_known_setting("embedding.model", "   ".to_string())
            .is_err());
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
            .set_known_setting("db9.max_sort_bytes", "268435456".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("db9.max_sort_bytes").as_deref(),
            Some("268435456")
        );

        settings
            .set_known_setting("db9.max_sort_bytes", "256MB".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("db9.max_sort_bytes").as_deref(),
            Some("268435456")
        );

        settings
            .set_known_setting("db9.max_sort_bytes", "1gb".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("db9.max_sort_bytes").as_deref(),
            Some("1073741824")
        );

        settings
            .set_known_setting("db9.max_sort_bytes", "0".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("db9.max_sort_bytes").as_deref(),
            Some("0")
        );

        assert!(settings
            .set_known_setting("db9.max_sort_bytes", "-1".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("db9.max_sort_bytes", "1TB".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("db9.max_sort_bytes", "abc".to_string())
            .is_err());
    }

    #[test]
    fn test_session_settings_dml_table_scan_max_rows_bounds() {
        let mut settings = SessionSettings::new();

        settings
            .set_known_setting("db9.dml_table_scan_max_rows", "0".to_string())
            .expect("0 should disable row guard");
        assert_eq!(
            settings
                .show_value("db9.dml_table_scan_max_rows")
                .as_deref(),
            Some("0")
        );

        let upper = (i64::MAX as usize).to_string();
        settings
            .set_known_setting("db9.dml_table_scan_max_rows", upper.clone())
            .expect("upper bound should be accepted");
        assert_eq!(
            settings
                .show_value("db9.dml_table_scan_max_rows")
                .as_deref(),
            Some(upper.as_str())
        );

        let overflow = ((i64::MAX as usize as u128) + 1).to_string();
        let err = settings
            .set_known_setting("db9.dml_table_scan_max_rows", overflow.clone())
            .expect_err("overflow value should be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("must be between 0 and"),
            "unexpected error message: {}",
            msg
        );
        assert!(
            msg.contains(&overflow),
            "error message should include bad value: {}",
            msg
        );

        // Rejected SET must not change current value.
        assert_eq!(
            settings
                .show_value("db9.dml_table_scan_max_rows")
                .as_deref(),
            Some(upper.as_str())
        );
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

        // SERIALIZABLE is accepted — SHOW returns user-set value (PG parity).
        // Internal behavior always uses repeatable read regardless.
        assert!(settings
            .set_known_setting("transaction_isolation", "serializable".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("serializable")
        );
        assert!(settings
            .set_known_setting("transaction_isolation", "SERIALIZABLE".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("serializable")
        );

        // Garbage values must be rejected
        assert!(settings
            .set_known_setting("transaction_isolation", "garbage".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("transaction_isolation", "snapshot".to_string())
            .is_err());

        // READ UNCOMMITTED — SHOW returns user-set value
        assert!(settings
            .set_known_setting("transaction_isolation", "read uncommitted".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("read uncommitted")
        );

        // READ COMMITTED — SHOW returns user-set value
        assert!(settings
            .set_known_setting("transaction_isolation", "read committed".to_string())
            .unwrap());
        assert_eq!(
            settings.show_value("transaction_isolation").as_deref(),
            Some("read committed")
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

    /// #1529: transaction_deferrable and default_transaction_deferrable must
    /// SHOW as "off" and reject SET (deferrable requires SERIALIZABLE which
    /// db9 does not support).
    #[test]
    fn test_transaction_deferrable_show_and_reject_set() {
        let settings = SessionSettings::new();

        // SHOW returns "off" for both.
        assert_eq!(
            settings.show_value("transaction_deferrable").as_deref(),
            Some("off")
        );
        assert_eq!(
            settings
                .show_value("default_transaction_deferrable")
                .as_deref(),
            Some("off")
        );

        // boot_default_show_value also returns "off".
        assert_eq!(
            SessionSettings::boot_default_show_value("transaction_deferrable"),
            "off"
        );
        assert_eq!(
            SessionSettings::boot_default_show_value("default_transaction_deferrable"),
            "off"
        );

        // SET = off must succeed (it's the only supported value).
        let mut settings = SessionSettings::new();
        assert!(settings
            .set_known_setting("transaction_deferrable", "off".to_string())
            .is_ok());
        assert!(settings
            .set_known_setting("default_transaction_deferrable", "off".to_string())
            .is_ok());

        // SET = on must be rejected with SQLSTATE 0A000 (feature not supported).
        let err = settings
            .set_known_setting("transaction_deferrable", "on".to_string())
            .unwrap_err();
        let sql_err = err.downcast_ref::<crate::sql::error::SqlError>().unwrap();
        assert_eq!(sql_err.sqlstate(), "0A000");

        let err = settings
            .set_known_setting("default_transaction_deferrable", "on".to_string())
            .unwrap_err();
        let sql_err = err.downcast_ref::<crate::sql::error::SqlError>().unwrap();
        assert_eq!(sql_err.sqlstate(), "0A000");

        // SHOW must still be "off" after rejected SET.
        assert_eq!(
            settings.show_value("transaction_deferrable").as_deref(),
            Some("off")
        );
        assert_eq!(
            settings
                .show_value("default_transaction_deferrable")
                .as_deref(),
            Some("off")
        );
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
    fn test_reset_default_show_value_uses_configured_defaults() {
        let settings_zero = SessionSettings::new();
        assert_eq!(
            settings_zero.reset_default_show_value("statement_timeout"),
            "0"
        );
        assert_eq!(
            settings_zero.reset_default_show_value("idle_in_transaction_session_timeout"),
            "0"
        );
        assert_eq!(settings_zero.reset_default_show_value("timezone"), "UTC");
        let settings_nonzero = SessionSettings::new_with_defaults(60_000, 30_000);
        assert_eq!(
            settings_nonzero.reset_default_show_value("statement_timeout"),
            "60000ms"
        );
        assert_eq!(
            settings_nonzero.reset_default_show_value("idle_in_transaction_session_timeout"),
            "30000ms"
        );
        assert_eq!(
            settings_nonzero.reset_default_show_value("lock_timeout"),
            "0"
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
    fn test_lock_timeout_getter_uses_typed_and_local_values() {
        let mut settings = SessionSettings::new();
        assert_eq!(settings.lock_timeout(), None);

        settings
            .set_known_setting("lock_timeout", "5s".to_string())
            .unwrap();
        assert_eq!(settings.lock_timeout(), Some(Duration::from_millis(5_000)));

        settings
            .set_local_override("lock_timeout", "1500".to_string())
            .unwrap();
        assert_eq!(settings.lock_timeout(), Some(Duration::from_millis(1_500)));

        settings.reset_setting("lock_timeout");
        assert_eq!(settings.lock_timeout(), None);
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

    #[test]
    fn test_set_local_override_precedence_and_regular_set_clears_it() {
        let mut settings = SessionSettings::new();

        settings
            .set_known_setting("statement_timeout", "7000".to_string())
            .unwrap();
        settings
            .set_local_override("statement_timeout", "2000".to_string())
            .unwrap();

        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("2000ms")
        );
        assert_eq!(
            settings.statement_timeout(),
            Some(Duration::from_millis(2_000))
        );

        settings
            .set_known_setting("statement_timeout", "3000".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("3000ms")
        );
        assert_eq!(
            settings.statement_timeout(),
            Some(Duration::from_millis(3_000))
        );
    }

    #[test]
    fn test_local_settings_savepoint_rollback_and_release() {
        let mut settings = SessionSettings::new();

        settings
            .set_local_override("statement_timeout", "1000".to_string())
            .unwrap();
        settings.push_settings_savepoint("sp1".to_string());

        settings
            .set_local_override("statement_timeout", "2000".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("2000ms")
        );

        settings.rollback_settings_to_savepoint("sp1");
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("1000ms")
        );

        settings.push_settings_savepoint("sp2".to_string());
        settings
            .set_local_override("statement_timeout", "3000".to_string())
            .unwrap();
        settings.release_settings_savepoint("sp2");
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("3000ms")
        );
    }

    // ── SHOW ALL tests ────────────────────────────────────────────────────

    #[test]
    fn test_known_gucs_sorted() {
        use crate::sql::session::settings::GUC_TABLE;
        let names: Vec<&str> = GUC_TABLE.iter().map(|g| g.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(
            names, sorted,
            "GUC_TABLE must be sorted alphabetically by name"
        );
    }

    #[test]
    fn test_show_all_sorted_no_duplicates() {
        let settings = SessionSettings::new();
        let all = settings.show_all();

        // No duplicates (HashSet comparison — works regardless of ordering)
        let names: Vec<&str> = all.iter().map(|(n, _, _)| n.as_str()).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(
            names.len(),
            unique.len(),
            "show_all() must have no duplicate names"
        );

        // Alphabetically sorted
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "show_all() must be sorted alphabetically");

        // Key GUCs present
        let name_set: std::collections::HashSet<&str> = names.iter().copied().collect();
        for key in &[
            "server_version",
            "timezone",
            "search_path",
            "extra_float_digits",
            "statement_timeout",
            "transaction_isolation",
        ] {
            assert!(
                name_set.contains(key),
                "show_all() missing key GUC: {}",
                key
            );
        }
    }

    #[test]
    fn test_show_all_includes_extra_settings() {
        let mut settings = SessionSettings::new();
        settings
            .set_known_setting("my_custom_guc", "val".to_string())
            .unwrap();
        let all = settings.show_all();
        let found = all
            .iter()
            .any(|(n, v, _)| n == "my_custom_guc" && v == "val");
        assert!(found, "show_all() must include user-SET extra_settings");
    }

    #[test]
    fn test_server_reserved_settings_require_server_setter() {
        let mut settings = SessionSettings::new();

        let err = settings
            .set_known_setting("request.jwt.claim.sub", "alice".to_string())
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("reserved for server-side use only"),
            "client write path must reject server-reserved namespaces"
        );

        settings
            .set_server_reserved_setting("Request.JWT.Claim.Sub", "alice".to_string())
            .unwrap();
        settings
            .set_server_reserved_setting("request.jwt.claims", "{\"sub\":\"alice\"}".to_string())
            .unwrap();

        assert_eq!(
            settings.show_value("request.jwt.claim.sub").as_deref(),
            Some("alice")
        );
        assert_eq!(
            settings.show_value("request.jwt.claims").as_deref(),
            Some("{\"sub\":\"alice\"}")
        );

        let all = settings.show_all();
        assert!(all
            .iter()
            .any(|(n, v, _)| { n == "request.jwt.claim.sub" && v == "alice" }));
        assert!(
            settings
                .all_values()
                .get("request.jwt.claim.sub")
                .map(String::as_str)
                == Some("alice")
        );

        let local_err = settings
            .set_local_override("auth.uid", "evil".to_string())
            .unwrap_err();
        assert!(
            local_err
                .to_string()
                .contains("reserved for server-side use only"),
            "SET LOCAL path must also reject server-reserved namespaces"
        );
    }

    #[test]
    fn test_reset_all_preserves_server_reserved_settings() {
        let mut settings = SessionSettings::new();
        settings
            .set_server_reserved_setting("request.jwt.claim.sub", "alice".to_string())
            .unwrap();
        settings
            .set_known_setting("application_name", "before-reset".to_string())
            .unwrap();
        settings
            .set_local_override("statement_timeout", "2500".to_string())
            .unwrap();

        settings.reset_all_settings();

        assert_eq!(
            settings.show_value("request.jwt.claim.sub").as_deref(),
            Some("alice")
        );
        assert_eq!(settings.show_value("application_name").as_deref(), Some(""));
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("0")
        );
    }

    #[test]
    fn test_reset_setting_does_not_clear_server_reserved_settings() {
        let mut settings = SessionSettings::new();
        settings
            .set_server_reserved_setting("auth.uid", "auth0|alice".to_string())
            .unwrap();

        settings.reset_setting("auth.uid");

        assert_eq!(
            settings.show_value("auth.uid").as_deref(),
            Some("auth0|alice")
        );
    }

    #[test]
    fn test_show_all_includes_local_overrides() {
        let mut settings = SessionSettings::new();
        settings
            .set_local_override("statement_timeout", "5000".to_string())
            .unwrap();
        let all = settings.show_all();
        let found = all.iter().find(|(n, _, _)| n == "statement_timeout");
        assert_eq!(
            found.map(|(_, v, _)| v.as_str()),
            Some("5000ms"),
            "show_all() must reflect local override value"
        );
    }

    #[test]
    fn test_show_all_local_only_key_appears() {
        let mut settings = SessionSettings::new();
        settings
            .set_local_override("my_local_guc", "localval".to_string())
            .unwrap();
        let all = settings.show_all();
        let found = all
            .iter()
            .any(|(n, v, _)| n == "my_local_guc" && v == "localval");
        assert!(found, "show_all() must include local-only override keys");
    }

    #[test]
    fn test_show_all_settings_forces_session_pseudo_gucs() {
        use crate::sql::session::force_insert_setting;

        // Simulate show_all_settings() contract: even when extra_settings
        // contains stale/user-set values for is_superuser or
        // session_authorization, force_insert_setting must overwrite them.
        let mut settings = SessionSettings::new();
        settings
            .set_known_setting("is_superuser", "bogus".to_string())
            .unwrap();
        settings
            .set_known_setting("session_authorization", "evil_user".to_string())
            .unwrap();

        let mut all = settings.show_all();

        // Before force-replace: stale values present
        let is_su = all.iter().find(|(n, _, _)| n == "is_superuser");
        assert_eq!(is_su.map(|(_, v, _)| v.as_str()), Some("bogus"));

        // Apply force-replace (mirrors Session::show_all_settings logic)
        force_insert_setting(&mut all, "is_superuser", "off".to_string());
        force_insert_setting(&mut all, "session_authorization", "real_user".to_string());

        // After: authoritative values win
        let is_su = all.iter().find(|(n, _, _)| n == "is_superuser");
        assert_eq!(is_su.map(|(_, v, _)| v.as_str()), Some("off"));
        let sa = all.iter().find(|(n, _, _)| n == "session_authorization");
        assert_eq!(sa.map(|(_, v, _)| v.as_str()), Some("real_user"));

        // Vec remains sorted after force-inserts
        let names: Vec<&str> = all.iter().map(|(n, _, _)| n.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "force_insert must preserve sort order");
    }

    #[test]
    fn test_session_authorization_readback_separates_pseudo_and_dotted_guc() {
        let store = TikvStore::new_stub();
        let observability = observability::registry().tenant("tenant_session_auth_readback");
        let mut session = Session::new_with_user_and_database(
            store,
            observability,
            "admin".to_string(),
            true,
            false,
            190090,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap();

        assert_eq!(
            session
                .show_setting_value("session_authorization")
                .as_deref(),
            Some("admin")
        );
        assert_eq!(session.show_setting_value("session.authorization"), None);

        session
            .set_known_setting("session.authorization", "x".to_string())
            .expect("set dotted session guc");

        assert_eq!(
            session
                .show_setting_value("session_authorization")
                .as_deref(),
            Some("admin")
        );
        assert_eq!(
            session
                .show_setting_value("session.authorization")
                .as_deref(),
            Some("x")
        );

        let snapshot = session.all_settings_snapshot();
        assert_eq!(
            snapshot.get("session_authorization").map(String::as_str),
            Some("admin")
        );
        assert_eq!(
            snapshot.get("session.authorization").map(String::as_str),
            Some("x")
        );

        session
            .set_server_reserved_setting("request.jwt.claim.sub", "auth0|alice".to_string())
            .expect("set trusted jwt claim");
        let settings_snapshot = session.all_settings_snapshot();
        let execution_snapshot = session.all_execution_settings_snapshot();
        assert_eq!(
            settings_snapshot
                .get("request.jwt.claim.sub")
                .map(String::as_str),
            Some("auth0|alice")
        );
        assert_eq!(
            execution_snapshot
                .get("request.jwt.claim.sub")
                .map(String::as_str),
            Some("auth0|alice")
        );
    }

    #[test]
    fn test_embedding_api_key_public_readback_is_masked() {
        let store = TikvStore::new_stub();
        let observability = observability::registry().tenant("tenant_embedding_api_key_readback");
        let mut session = Session::new_with_user_and_database(
            store,
            observability,
            "admin".to_string(),
            true,
            false,
            190091,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap();

        session
            .set_known_setting("embedding.api_key", "sk-secret-1234".to_string())
            .expect("set embedding api key");

        assert_eq!(
            session.show_setting_value("embedding.api_key").as_deref(),
            Some("****")
        );

        let all = session.show_all_settings();
        let found = all.iter().find(|(n, _, _)| n == "embedding.api_key");
        assert_eq!(found.map(|(_, v, _)| v.as_str()), Some("****"));
    }

    #[test]
    fn test_show_value_covers_all_known_gucs() {
        use crate::sql::session::settings::GUC_TABLE;
        let settings = SessionSettings::new();
        for guc in GUC_TABLE {
            assert!(
                settings.show_value(guc.name).is_some(),
                "show_value({:?}) returned None — add a typed-field match arm or boot_default for this GUC",
                guc.name
            );
        }
    }

    #[test]
    fn test_reset_all_clears_local_overrides_but_preserves_savepoint_snapshots() {
        let mut settings = SessionSettings::new();

        settings
            .set_local_override("statement_timeout", "1500".to_string())
            .unwrap();
        settings.push_settings_savepoint("sp1".to_string());
        settings
            .set_local_override("statement_timeout", "2500".to_string())
            .unwrap();

        settings.reset_all_settings();
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("0")
        );

        settings.rollback_settings_to_savepoint("sp1");
        assert_eq!(
            settings.show_value("statement_timeout").as_deref(),
            Some("1500ms")
        );
    }

    /// Regression: RESET ALL inside a savepoint must record BASE typed-field
    /// values to the GUC save-stack, not the effective values that include
    /// SET LOCAL overlays. Otherwise, ROLLBACK TO writes the local-overlay
    /// value into the typed field, where it persists after COMMIT.
    ///
    /// Reproduces the 237_set_local test-case-13 failure.
    #[test]
    fn test_reset_all_inside_savepoint_does_not_pollute_typed_fields() {
        let mut settings = SessionSettings::new();

        // row_security defaults to None (shows "on")
        assert_eq!(settings.show_value("row_security").as_deref(), Some("on"));

        // BEGIN
        settings.begin_transaction_settings();

        // SET LOCAL row_security = off
        settings
            .set_local_override("row_security", "off".to_string())
            .unwrap();
        assert_eq!(settings.show_value("row_security").as_deref(), Some("off"));

        // SAVEPOINT rs
        settings.push_settings_savepoint("rs".to_string());
        settings.push_guc_savepoint("rs".to_string());

        // RESET ALL
        settings.reset_all_settings();
        assert_eq!(
            settings.show_value("row_security").as_deref(),
            Some("on"),
            "after RESET ALL, row_security should be default"
        );

        // ROLLBACK TO rs
        settings.rollback_settings_to_savepoint("rs");
        settings.rollback_guc_to_savepoint("rs");
        assert_eq!(
            settings.show_value("row_security").as_deref(),
            Some("off"),
            "after ROLLBACK TO, SET LOCAL override should be restored"
        );

        // COMMIT
        settings.clear_local_overrides();
        settings.commit_transaction_settings();
        assert_eq!(
            settings.show_value("row_security").as_deref(),
            Some("on"),
            "after COMMIT, SET LOCAL should revert — typed field must not retain 'off'"
        );
    }

    /// Regression: regular SET inside a transaction must be rolled back on
    /// ROLLBACK, restoring the prior typed-field value.
    ///
    /// Reproduces the 1962_guc_set_rollback test-case-1.
    #[test]
    fn test_regular_set_rolled_back_on_transaction_rollback() {
        let mut settings = SessionSettings::new();

        // SET timezone = 'UTC'
        settings
            .set_known_setting("timezone", "UTC".to_string())
            .unwrap();
        assert_eq!(settings.show_value("timezone").as_deref(), Some("UTC"));

        // BEGIN
        settings.begin_transaction_settings();

        // SET timezone = 'Asia/Shanghai'
        settings
            .set_known_setting("timezone", "Asia/Shanghai".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("Asia/Shanghai"),
            "during txn, timezone should be Asia/Shanghai"
        );

        // ROLLBACK
        settings.rollback_transaction_settings();
        settings.clear_local_overrides();
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("UTC"),
            "after ROLLBACK, timezone should revert to UTC"
        );
    }

    /// Regression: restore_guc_value must not call remove_local_override.
    /// SET inside a savepoint + ROLLBACK TO must preserve SET LOCAL overrides.
    #[test]
    fn test_guc_rollback_preserves_local_overrides() {
        let mut settings = SessionSettings::new();

        // BEGIN
        settings.begin_transaction_settings();

        // SET LOCAL timezone = 'Asia/Shanghai'
        settings
            .set_local_override("timezone", "Asia/Shanghai".to_string())
            .unwrap();

        // SAVEPOINT sp1
        settings.push_settings_savepoint("sp1".to_string());
        settings.push_guc_savepoint("sp1".to_string());

        // SET timezone = 'Europe/London' (regular SET inside savepoint)
        settings
            .set_known_setting("timezone", "Europe/London".to_string())
            .unwrap();

        // ROLLBACK TO sp1
        settings.rollback_settings_to_savepoint("sp1");
        settings.rollback_guc_to_savepoint("sp1");

        // The SET LOCAL override should be restored by rollback_settings_to_savepoint.
        // The typed field should be restored by rollback_guc_to_savepoint.
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("Asia/Shanghai"),
            "SET LOCAL override should be restored after ROLLBACK TO"
        );

        // COMMIT — SET LOCAL reverts
        settings.clear_local_overrides();
        settings.commit_transaction_settings();
        assert_eq!(
            settings.show_value("timezone").as_deref(),
            Some("UTC"),
            "after COMMIT, SET LOCAL should revert to base value"
        );
    }

    #[test]
    fn test_record_command_complete_releases_xact_locks_when_idle() {
        struct ConnectionCleanup {
            conn_ids: [i64; 2],
        }

        impl Drop for ConnectionCleanup {
            fn drop(&mut self) {
                let manager = global_lock_manager();
                for conn_id in self.conn_ids {
                    manager.release_all_for_connection(conn_id);
                }
            }
        }

        let keyspace: Arc<str> = Arc::from("tenant_record_complete_xact_release");
        let conn_a = 190001;
        let conn_b = 190002;
        let _cleanup = ConnectionCleanup {
            conn_ids: [conn_a, conn_b],
        };

        let store = TikvStore::new_stub();
        let observability = observability::registry().tenant(&keyspace);
        let mut session = Session::new_with_database(
            store,
            observability,
            conn_a,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap();

        let manager = global_lock_manager();
        assert!(manager.try_acquire(
            &keyspace,
            424242,
            conn_a,
            AdvisoryLockMode::Exclusive,
            AdvisoryLockScope::Transaction
        ));

        session
            .has_xact_advisory_locks
            .store(true, Ordering::Release);
        session.record_command_complete();

        assert!(
            !session.has_xact_advisory_locks.load(Ordering::Acquire),
            "marker should be cleared after idle command completion"
        );
        let acquired = manager
            .try_acquire_checked(
                &keyspace,
                424242,
                conn_b,
                AdvisoryLockMode::Exclusive,
                AdvisoryLockScope::Transaction,
            )
            .expect("try lock should not hit lock-cap limit");
        assert!(
            acquired,
            "idle command completion should release xact-scoped lock"
        );
    }

    #[test]
    fn test_plan_cache_runtime_follows_session_gucs() {
        let store = TikvStore::new_stub();
        let observability = observability::registry().tenant("tenant_plan_cache_guc");
        let mut session = Session::new_with_database(
            store,
            observability,
            190101,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap();

        assert_eq!(session.plan_cache().capacity(), 128);
        assert_eq!(session.plan_cache().min_exec(), 5);

        session
            .set_known_setting("db9.prepared_plan_cache_size", "16".to_string())
            .expect("set cache size");
        session
            .set_known_setting("db9.prepared_plan_cache_min_exec", "2".to_string())
            .expect("set cache min_exec");

        assert_eq!(session.plan_cache().capacity(), 16);
        assert_eq!(session.plan_cache().min_exec(), 2);
    }

    #[test]
    fn test_plan_cache_runtime_follows_set_local_and_clear() {
        let store = TikvStore::new_stub();
        let observability = observability::registry().tenant("tenant_plan_cache_local");
        let mut session = Session::new_with_database(
            store,
            observability,
            190102,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap();

        session
            .set_local_setting("db9.prepared_plan_cache_size", "8".to_string())
            .expect("set local cache size");
        session
            .set_local_setting("db9.prepared_plan_cache_min_exec", "1".to_string())
            .expect("set local cache min_exec");
        assert_eq!(session.plan_cache().capacity(), 8);
        assert_eq!(session.plan_cache().min_exec(), 1);

        session.clear_local_overrides();
        assert_eq!(session.plan_cache().capacity(), 128);
        assert_eq!(session.plan_cache().min_exec(), 5);
    }

    #[test]
    fn set_local_session_auth_rollback_to_savepoint_restores_identity() {
        let store = TikvStore::new_stub();
        let observability = observability::registry().tenant("tenant_sp_auth");
        let mut session = Session::new_with_user_and_database(
            store,
            observability,
            "alice".to_string(),
            true,
            false,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap();

        // Simulate entering a transaction.
        session.force_test_transaction_state(true, false);

        // Snapshot at savepoint sp1 — identity is alice (superuser).
        session.push_session_auth_savepoint("sp1".to_string());

        // SET LOCAL session_authorization 'bob' (non-superuser)
        session.save_session_auth_for_local();
        session.set_session_authorization("bob".to_string(), false, false);
        assert_eq!(session.session_user(), Some("bob"));
        assert_eq!(session.current_user(), Some("bob"));
        assert!(!session.is_superuser());

        // ROLLBACK TO SAVEPOINT sp1 — identity must revert to alice.
        session.rollback_session_auth_to_savepoint("sp1");
        assert_eq!(session.session_user(), Some("alice"));
        assert_eq!(session.current_user(), Some("alice"));
        assert!(session.is_superuser());
    }

    #[test]
    fn set_local_session_auth_nested_savepoints_rollback() {
        let store = TikvStore::new_stub();
        let observability = observability::registry().tenant("tenant_nested_sp_auth");
        let mut session = Session::new_with_user_and_database(
            store,
            observability,
            "alice".to_string(),
            true,
            false,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap();

        session.force_test_transaction_state(true, false);

        // SAVEPOINT sp1
        session.push_session_auth_savepoint("sp1".to_string());

        // SET LOCAL session_authorization 'bob'
        session.save_session_auth_for_local();
        session.set_session_authorization("bob".to_string(), false, false);

        // SAVEPOINT sp2
        session.push_session_auth_savepoint("sp2".to_string());

        // SET LOCAL session_authorization 'charlie'
        session.set_session_authorization("charlie".to_string(), false, false);

        assert_eq!(session.session_user(), Some("charlie"));

        // ROLLBACK TO SAVEPOINT sp2 — reverts to bob.
        session.rollback_session_auth_to_savepoint("sp2");
        assert_eq!(session.session_user(), Some("bob"));
        assert!(!session.is_superuser());

        // ROLLBACK TO SAVEPOINT sp1 — reverts to alice.
        session.rollback_session_auth_to_savepoint("sp1");
        assert_eq!(session.session_user(), Some("alice"));
        assert!(session.is_superuser());
    }

    #[test]
    fn set_local_session_auth_release_savepoint_keeps_identity() {
        let store = TikvStore::new_stub();
        let observability = observability::registry().tenant("tenant_release_sp_auth");
        let mut session = Session::new_with_user_and_database(
            store,
            observability,
            "alice".to_string(),
            true,
            false,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap();

        session.force_test_transaction_state(true, false);

        // SAVEPOINT sp1
        session.push_session_auth_savepoint("sp1".to_string());

        // SET LOCAL session_authorization 'bob'
        session.save_session_auth_for_local();
        session.set_session_authorization("bob".to_string(), false, false);

        // SAVEPOINT sp2
        session.push_session_auth_savepoint("sp2".to_string());

        // RELEASE SAVEPOINT sp2 — identity stays bob.
        session.release_session_auth_savepoint("sp2");
        assert_eq!(session.session_user(), Some("bob"));
        assert!(!session.is_superuser());

        // ROLLBACK TO sp1 — reverts to alice.
        session.rollback_session_auth_to_savepoint("sp1");
        assert_eq!(session.session_user(), Some("alice"));
        assert!(session.is_superuser());
    }

    // --- bytea_output GUC validation (#1538) ---

    #[test]
    fn bytea_output_accepts_hex_and_escape() {
        let mut settings = SessionSettings::new_with_defaults(0, 0);
        // Default is hex.
        assert_eq!(settings.show_value("bytea_output").as_deref(), Some("hex"));
        // SET to escape.
        settings
            .set_known_setting("bytea_output", "escape".to_string())
            .unwrap();
        assert_eq!(
            settings.show_value("bytea_output").as_deref(),
            Some("escape")
        );
        // SET to hex.
        settings
            .set_known_setting("bytea_output", "hex".to_string())
            .unwrap();
        assert_eq!(settings.show_value("bytea_output").as_deref(), Some("hex"));
    }

    #[test]
    fn bytea_output_rejects_invalid_values() {
        let mut settings = SessionSettings::new_with_defaults(0, 0);
        assert!(settings
            .set_known_setting("bytea_output", "base64".to_string())
            .is_err());
        assert!(settings
            .set_known_setting("bytea_output", "".to_string())
            .is_err());
    }
}
