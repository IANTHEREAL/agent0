use super::*;

impl SessionSettings {
    /// Return the boot-default display value for a GUC (what SHOW returns after RESET).
    ///
    /// Static variant that returns the hardcoded PG boot default.
    /// For tenant-configurable timeouts, prefer `reset_default_show_value()`
    /// which uses the session's configured defaults.
    pub(crate) fn boot_default_show_value(name: &str) -> String {
        let canonical = Self::canonical_setting_name(name);
        match canonical {
            "search_path" => Self::format_search_path_show(&Self::default_search_path()),
            "db9.dml_table_scan_max_rows" => DEFAULT_DML_TABLE_SCAN_MAX_ROWS.to_string(),
            "db9.hash_join_work_mem" => DEFAULT_HASH_JOIN_WORK_MEM.to_string(),
            "db9.max_sort_bytes" => DEFAULT_MAX_SORT_BYTES.to_string(),
            "default_text_search_config" => {
                crate::sql::fts_tokenizers::default_text_search_config().to_string()
            }
            "statement_timeout"
            | "lock_timeout"
            | "idle_in_transaction_session_timeout"
            | "db9.retry_timeout" => Self::format_timeout_show(0),
            _ => GUC_TABLE
                .iter()
                .find(|g| g.name == canonical)
                .map(|g| g.boot_default.to_string())
                .unwrap_or_default(),
        }
    }

    /// Return the effective post-reset display value for a GUC.
    ///
    /// Like `boot_default_show_value()` but uses the session's configured
    /// defaults for `statement_timeout` and
    /// `idle_in_transaction_session_timeout` instead of hardcoded 0.
    pub(crate) fn reset_default_show_value(&self, name: &str) -> String {
        let canonical = Self::canonical_setting_name(name);
        match canonical {
            "statement_timeout" => Self::format_timeout_show(self.default_statement_timeout_ms),
            "idle_in_transaction_session_timeout" => {
                Self::format_timeout_show(self.default_idle_in_transaction_session_timeout_ms)
            }
            _ => Self::boot_default_show_value(name),
        }
    }

    /// Get a session setting value in a Postgres-like string form, for `SHOW`.
    pub(crate) fn show_value(&self, name: &str) -> Option<String> {
        let canonical = Self::canonical_setting_name(name);
        if !Self::is_immutable_setting(canonical) {
            if let Some(v) = self.local_overrides.get(canonical) {
                return Some(v.clone());
            }
        }

        // ── Structural reverse guard ──
        // If the name is not in GUC_TABLE and not in dynamic maps, return None early.
        // This is a best-effort runtime guard: any typed-field match arm below for an
        // unregistered name would be unreachable dead code, nudging developers to add
        // new GUCs to GUC_TABLE first.
        let is_registered = GUC_TABLE.iter().any(|g| g.name == canonical);
        if !is_registered
            && !self.server_reserved_settings.contains_key(canonical)
            && !self.extra_settings.contains_key(canonical)
            && !self.local_overrides.contains_key(canonical)
        {
            return None;
        }

        match canonical {
            // These are used heavily by drivers for feature detection.
            "server_version" => Some("16.0".to_string()),
            "server_version_num" => Some("160000".to_string()),
            "server_encoding" => Some("UTF8".to_string()),
            "search_path" => Some(Self::format_search_path_show(
                self.local_search_path
                    .as_deref()
                    .unwrap_or(&self.search_path),
            )),
            "datestyle" => Some("ISO, MDY".to_string()),
            "integer_datetimes" => Some("on".to_string()),
            "intervalstyle" => Some("postgres".to_string()),
            "statement_timeout" => Some(Self::format_timeout_show(self.statement_timeout_ms)),
            "lock_timeout" => Some(Self::format_timeout_show(self.lock_timeout_ms)),
            "idle_in_transaction_session_timeout" => Some(Self::format_timeout_show(
                self.idle_in_transaction_session_timeout_ms,
            )),
            "db9.dml_table_scan_max_rows" => Some(self.dml_table_scan_max_rows.to_string()),
            "db9.hash_join_work_mem" => Some(self.hash_join_work_mem.to_string()),
            "db9.max_sort_bytes" => Some(self.max_sort_bytes.to_string()),
            "db9.prepared_plan_cache_size" => Some(self.prepared_plan_cache_size.to_string()),
            "db9.prepared_plan_cache_min_exec" => {
                Some(self.prepared_plan_cache_min_exec.to_string())
            }
            "hnsw.ef_search" => Some(self.hnsw_ef_search.to_string()),
            "db9.retry_max_attempts" => Some(self.retry_max_attempts.to_string()),
            "db9.retry_timeout" => Some(Self::format_timeout_show(self.retry_timeout_ms)),
            "db9.use_optimizer" => Some("on".to_string()),
            "embedding.model" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| crate::config::get_embedding_config().model.clone()),
            ),
            "embedding.dimensions" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| {
                        crate::config::get_embedding_config().dimensions.to_string()
                    }),
            ),
            "embedding.max_calls" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| "100".to_string()),
            ),
            "embedding.concurrency" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| "5".to_string()),
            ),
            "embedding.provider" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| crate::config::get_embedding_config().provider_name.clone()),
            ),
            "embedding.endpoint" => Some(
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .unwrap_or_else(|| crate::config::get_embedding_config().endpoint.clone()),
            ),
            "embedding.api_key" => {
                // Return the raw value so that internal snapshot consumers
                // (e.g. call_embedding_api) receive the real key.
                // Public SQL readback masking is applied by public_setting_value()
                // through Session::show_setting_value() / QueryContext lookups.
                self.extra_settings
                    .get(canonical)
                    .cloned()
                    .or_else(|| crate::config::get_embedding_config().api_key.clone())
                    .or_else(|| Some("".to_string()))
            }
            "timezone" => Some(self.timezone.as_deref().unwrap_or("UTC").to_string()),
            "application_name" => Some(self.application_name.as_deref().unwrap_or("").to_string()),
            "client_encoding" => Some(
                self.client_encoding
                    .as_deref()
                    .unwrap_or("UTF8")
                    .to_string(),
            ),
            "standard_conforming_strings" => Some(
                self.standard_conforming_strings
                    .as_deref()
                    .unwrap_or("on")
                    .to_string(),
            ),
            "check_function_bodies" => Some(
                self.check_function_bodies
                    .as_deref()
                    .unwrap_or("on")
                    .to_string(),
            ),
            "xmloption" => Some(self.xmloption.as_deref().unwrap_or("content").to_string()),
            "client_min_messages" => Some(
                self.client_min_messages
                    .as_deref()
                    .unwrap_or("notice")
                    .to_string(),
            ),
            "row_security" => Some(self.row_security.as_deref().unwrap_or("on").to_string()),
            "default_tablespace" => {
                Some(self.default_tablespace.as_deref().unwrap_or("").to_string())
            }
            "default_table_access_method" => Some(
                self.default_table_access_method
                    .as_deref()
                    .unwrap_or("heap")
                    .to_string(),
            ),
            "transaction_deferrable" | "default_transaction_deferrable" => Some("off".to_string()),
            // `transaction.isolation.level` alias is canonicalized above.
            "transaction_isolation" => Some(
                self.transaction_isolation
                    .as_deref()
                    .unwrap_or("repeatable read")
                    .to_string(),
            ),
            "default_transaction_isolation" => Some(
                self.extra_settings
                    .get("default_transaction_isolation")
                    .cloned()
                    .unwrap_or_else(|| "read committed".to_string()),
            ),
            "default_transaction_read_only" => Some(
                self.default_transaction_read_only
                    .as_deref()
                    .unwrap_or("off")
                    .to_string(),
            ),
            _ => self
                .server_reserved_settings
                .get(canonical)
                .cloned()
                .or_else(|| self.extra_settings.get(canonical).cloned())
                .or_else(|| {
                    // default_text_search_config uses a runtime OnceLock fn, not a const.
                    if canonical == "default_text_search_config" {
                        return Some(
                            crate::sql::fts_tokenizers::default_text_search_config().to_string(),
                        );
                    }
                    GUC_TABLE
                        .iter()
                        .find(|g| g.name == canonical)
                        .filter(|g| !g.boot_default.is_empty())
                        .map(|g| g.boot_default.to_string())
                }),
        }
    }

    /// Collect all settings for SHOW ALL.
    /// Returns Vec<(name, value, description)> sorted alphabetically by name.
    pub(crate) fn show_all(&self) -> Vec<(String, String, String)> {
        let mut result = BTreeMap::new();

        // 1. All registered GUCs (respects local_override > typed field > default precedence)
        for guc in GUC_TABLE {
            if let Some(value) = self.show_value(guc.name) {
                result.insert(guc.name.to_string(), (value, guc.description.to_string()));
            }
        }

        // 2. Server-authored reserved settings not in registry.
        for name in self.server_reserved_settings.keys() {
            result
                .entry(name.clone())
                .or_insert_with(|| (self.show_value(name).unwrap_or_default(), String::new()));
        }

        // 3. User-SET extra_settings not in registry
        for name in self.extra_settings.keys() {
            result
                .entry(name.clone())
                .or_insert_with(|| (self.show_value(name).unwrap_or_default(), String::new()));
        }

        // 4. Local overrides not already covered
        for name in self.local_overrides.keys() {
            result
                .entry(name.clone())
                .or_insert_with(|| (self.show_value(name).unwrap_or_default(), String::new()));
        }

        result.into_iter().map(|(n, (v, d))| (n, v, d)).collect()
    }

    pub(crate) fn statement_timeout(&self) -> Option<Duration> {
        if let Some(v) = self.local_overrides.get("statement_timeout") {
            match Self::parse_timeout_millis(v) {
                Ok(0) => return None,
                Ok(ms) => return Some(Duration::from_millis(ms)),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local statement_timeout override"
                    );
                }
            }
        }

        if self.statement_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.statement_timeout_ms))
        }
    }

    pub(crate) fn lock_timeout(&self) -> Option<Duration> {
        if let Some(v) = self.local_overrides.get("lock_timeout") {
            match Self::parse_timeout_millis(v) {
                Ok(0) => return None,
                Ok(ms) => return Some(Duration::from_millis(ms)),
                Err(e) => {
                    tracing::error!(error = %e, value = v, "invalid local lock_timeout override");
                }
            }
        }

        if self.lock_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(self.lock_timeout_ms))
        }
    }

    pub(crate) fn idle_in_transaction_session_timeout(&self) -> Option<Duration> {
        if let Some(v) = self
            .local_overrides
            .get("idle_in_transaction_session_timeout")
        {
            match Self::parse_timeout_millis(v) {
                Ok(0) => return None,
                Ok(ms) => return Some(Duration::from_millis(ms)),
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local idle_in_transaction_session_timeout override"
                    );
                }
            }
        }

        if self.idle_in_transaction_session_timeout_ms == 0 {
            None
        } else {
            Some(Duration::from_millis(
                self.idle_in_transaction_session_timeout_ms,
            ))
        }
    }

    pub(crate) fn hash_join_work_mem(&self) -> usize {
        if let Some(v) = self.local_overrides.get("db9.hash_join_work_mem") {
            match Self::parse_byte_size(v) {
                Ok(bytes) => return bytes,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local db9.hash_join_work_mem override"
                    );
                }
            }
        }
        self.hash_join_work_mem
    }

    pub(crate) fn max_sort_bytes(&self) -> usize {
        if let Some(v) = self.local_overrides.get("db9.max_sort_bytes") {
            match Self::parse_byte_size(v) {
                Ok(bytes) => return bytes,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local db9.max_sort_bytes override"
                    );
                }
            }
        }
        self.max_sort_bytes
    }

    #[allow(dead_code)] // framework: accessed via settings snapshot in DML executor
    pub(crate) fn dml_table_scan_max_rows(&self) -> usize {
        if let Some(v) = self.local_overrides.get("db9.dml_table_scan_max_rows") {
            match v.parse::<usize>() {
                Ok(rows) => return rows,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local db9.dml_table_scan_max_rows override"
                    );
                }
            }
        }
        self.dml_table_scan_max_rows
    }

    #[allow(dead_code)]
    pub(crate) fn prepared_plan_cache_size(&self) -> usize {
        if let Some(v) = self.local_overrides.get("db9.prepared_plan_cache_size") {
            match v.parse::<usize>() {
                Ok(size) => return size,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local db9.prepared_plan_cache_size override"
                    );
                }
            }
        }
        self.prepared_plan_cache_size
    }

    #[allow(dead_code)]
    pub(crate) fn prepared_plan_cache_min_exec(&self) -> u64 {
        if let Some(v) = self.local_overrides.get("db9.prepared_plan_cache_min_exec") {
            match v.parse::<u64>() {
                Ok(min_exec) => return min_exec,
                Err(e) => {
                    tracing::error!(
                        error = %e,
                        value = v,
                        "invalid local db9.prepared_plan_cache_min_exec override"
                    );
                }
            }
        }
        self.prepared_plan_cache_min_exec
    }

    /// Collect all current settings into a flat map.
    ///
    /// Resolves every key through `show_value()` so precedence
    /// (local_overrides > typed fields > server_reserved_settings >
    /// extra_settings > default_value) is identical to `SHOW`.
    pub(crate) fn all_values(&self) -> HashMap<String, String> {
        use std::collections::HashSet;

        let all_keys: HashSet<&str> = GUC_TABLE
            .iter()
            .map(|g| g.name)
            .chain(self.server_reserved_settings.keys().map(String::as_str))
            .chain(self.extra_settings.keys().map(String::as_str))
            .chain(self.local_overrides.keys().map(String::as_str))
            .collect();

        let mut map = HashMap::with_capacity(all_keys.len());
        for key in all_keys {
            if let Some(v) = self.show_value(key) {
                map.insert(key.to_string(), v);
            }
        }
        map
    }
}
