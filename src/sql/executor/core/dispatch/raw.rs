//! Raw-SQL dispatch: routes statements classified by [`RawSqlKind`] before
//! `sqlparser` parsing.
//!
//! Two clearly separated paths:
//!
//! 1. **Instrumented** ([`Executor::try_dispatch_raw_instrumented`]) — uses
//!    the same record + mark-failed contract as the former `dispatch_raw!`
//!    macro. Only entered for non-observability users.
//!
//! 2. **Passthrough** ([`Executor::try_dispatch_raw_passthrough`]) — handles
//!    `ALTER SYSTEM SET` and `RESET` outside instrumented dispatch, preserving
//!    invariant I2: no raw observability record, no raw
//!    auto-`mark_transaction_failed`.

use super::super::*;
use super::scaffold::DispatchContext;
use crate::sql::raw_sql::RawSqlKind;

impl Executor {
    /// Attempt to dispatch the statement through instrumented raw-SQL paths.
    ///
    /// Returns `Some(result)` if the statement was handled (caller should
    /// return).  Returns `None` if no raw handler matched (caller should
    /// continue to parse/AST).
    ///
    /// Only entered for non-observability users.  The helpers
    /// `finish_raw_single` / `finish_raw_multi` record observability and mark
    /// transaction failure on error.
    ///
    /// Preserves the original two-block ordering:
    ///
    /// 1. First raw block (database/extension/comment/function/trigger ops)
    /// 2. Skip-reason check
    /// 3. Second raw block (alter/sequence/materialized-view/call/procedure/type/collation/analyze)
    pub(super) async fn try_dispatch_raw_instrumented(
        &self,
        session: &mut Session,
        sql: &str,
        ctx: &DispatchContext,
    ) -> Option<Result<ExecuteResults>> {
        debug_assert!(!ctx.is_observability_user);

        // ── First raw block ──────────────────────────────────────────
        if let Some(result) = self.try_first_raw_block(session, sql, ctx).await {
            return Some(result);
        }

        // ── Skip-reason gate (between first and second raw blocks) ──
        if let Some(reason) = get_skip_reason(&ctx.sql_upper) {
            return Some(Err(SqlError::Unsupported(reason).into()));
        }

        // ── Second raw block ─────────────────────────────────────────
        self.try_second_raw_block(session, sql, ctx).await
    }

    /// First raw block: database, extension, comment, function, trigger ops.
    async fn try_first_raw_block(
        &self,
        session: &mut Session,
        sql: &str,
        ctx: &DispatchContext,
    ) -> Option<Result<ExecuteResults>> {
        let kind = ctx.raw_kind?;
        match kind {
            RawSqlKind::CreateDatabase => {
                let start = Instant::now();
                let res = self.execute_create_database_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropDatabase => {
                let start = Instant::now();
                let res = self.execute_drop_database_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterDatabase => {
                let start = Instant::now();
                let res = self.execute_alter_database_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateExtension => {
                let start = Instant::now();
                let res = self.execute_create_extension_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropExtension => {
                let start = Instant::now();
                let res = self.execute_drop_extension_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CommentOn => {
                let start = Instant::now();
                let res = self.execute_comment_on_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateFunction => {
                let start = Instant::now();
                let res = self.execute_create_function_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropFunction => {
                let start = Instant::now();
                let res = self.execute_drop_function_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateTrigger => {
                let start = Instant::now();
                let res = self.execute_create_trigger_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropTrigger => {
                let start = Instant::now();
                let res = self.execute_drop_trigger_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            _ => None,
        }
    }

    /// Second raw block: alter/sequence/materialized-view/call/procedure/type/collation/analyze.
    async fn try_second_raw_block(
        &self,
        session: &mut Session,
        sql: &str,
        ctx: &DispatchContext,
    ) -> Option<Result<ExecuteResults>> {
        let kind = ctx.raw_kind?;
        match kind {
            RawSqlKind::AlterOwnerTo => {
                let start = Instant::now();
                let res = self.execute_alter_owner_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterDefaultPrivileges => {
                let start = Instant::now();
                let res = self
                    .execute_alter_default_privileges_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterSequenceOwnedBy => {
                let start = Instant::now();
                let res = self.execute_alter_sequence_owned_by_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::RefreshMaterializedView => {
                let start = Instant::now();
                let res = self
                    .execute_refresh_materialized_view_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropMaterializedView => {
                let start = Instant::now();
                let res = self.execute_drop_materialized_view_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::Call => {
                let start = Instant::now();
                let res = self.execute_call_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropProcedure => {
                let start = Instant::now();
                let res = self.execute_drop_procedure_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateProcedure => {
                let start = Instant::now();
                let res = self.execute_create_procedure_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateTypeEnum => {
                let start = Instant::now();
                let res = self.execute_create_type_enum_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterType => {
                let start = Instant::now();
                let res = self.execute_alter_type_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropType => {
                let start = Instant::now();
                let res = self.execute_drop_type_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateCollation => {
                let start = Instant::now();
                let res = self.execute_create_collation_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropCollation => {
                let start = Instant::now();
                let res = self.execute_drop_collation_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::Analyze => {
                let start = Instant::now();
                let res = self.execute_analyze_cmd(session, &ctx.sql_trimmed).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::Do => {
                let start = Instant::now();
                let res = self.execute_do_block_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterIndexIfExists => {
                let start = Instant::now();
                let res = self
                    .execute_alter_index_if_exists_rename(session, &ctx.sql_trimmed)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateTextSearchConfiguration => {
                let start = Instant::now();
                let res = self
                    .execute_create_text_search_configuration_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropTextSearchConfiguration => {
                let start = Instant::now();
                let res = self
                    .execute_drop_text_search_configuration_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterTextSearchConfiguration => {
                let start = Instant::now();
                let res = self
                    .execute_alter_text_search_configuration_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            _ => None,
        }
    }

    /// Attempt to dispatch `ALTER SYSTEM SET` or `RESET` through the
    /// passthrough path that bypasses instrumented dispatch.
    ///
    /// **Invariant I2**: These branches produce no raw observability record
    /// and no raw auto-`mark_transaction_failed` on either success or error.
    ///
    /// Returns `Some(result)` if handled, `None` otherwise.
    pub(super) fn try_dispatch_raw_passthrough(
        &self,
        session: &mut Session,
        ctx: &DispatchContext,
    ) -> Option<Result<ExecuteResults>> {
        match ctx.raw_kind {
            Some(RawSqlKind::AlterSystemSet) => {
                Some(self.execute_alter_system_set(session, &ctx.sql_trimmed))
            }
            Some(RawSqlKind::Reset) => Some(Self::execute_reset(session, &ctx.sql_trimmed)),
            _ => None,
        }
    }

    // ── Instrumented raw helpers (equivalent to dispatch_raw! macro) ──

    /// Finish an instrumented raw dispatch for a command returning
    /// `ExecuteResult`.
    fn finish_raw_single(
        &self,
        session: &mut Session,
        sql_obs: &str,
        start: Instant,
        res: Result<ExecuteResult>,
    ) -> Result<ExecuteResults> {
        if res.is_err() && session.is_in_transaction() {
            session.mark_transaction_failed();
        }
        self.observability
            .record_statement(start.elapsed(), res.is_ok(), || sql_obs.to_string());
        res.map(ExecuteResults::single)
    }

    /// Finish an instrumented raw dispatch for a command returning
    /// `ExecuteResults`.
    fn finish_raw_multi(
        &self,
        session: &mut Session,
        sql_obs: &str,
        start: Instant,
        res: Result<ExecuteResults>,
    ) -> Result<ExecuteResults> {
        if res.is_err() && session.is_in_transaction() {
            session.mark_transaction_failed();
        }
        self.observability
            .record_statement(start.elapsed(), res.is_ok(), || sql_obs.to_string());
        res
    }

    // ── Passthrough implementations (I2) ─────────────────────────────

    /// Execute `ALTER SYSTEM SET <guc> = <value>`.
    ///
    /// Passthrough: no instrumented dispatch, no observability record, no auto
    /// `mark_transaction_failed`.
    fn execute_alter_system_set(
        &self,
        session: &mut Session,
        sql_trimmed: &str,
    ) -> Result<ExecuteResults> {
        if !session.is_superuser() {
            return Err(SqlError::PermissionDenied {
                object_type: "system".to_string(),
                object_name: "ALTER SYSTEM SET".to_string(),
            }
            .into());
        }

        let rest = sql_trimmed.get(17..).unwrap_or("").trim();
        let rest_clean = rest.trim_end_matches(';').trim();
        let (name, raw_value) = if let Some(pos) = rest_clean.find('=') {
            (&rest_clean[..pos], &rest_clean[pos + 1..])
        } else {
            let rest_upper = rest_clean.to_ascii_uppercase();
            if let Some(pos) = rest_upper.find(" TO ") {
                (&rest_clean[..pos], &rest_clean[pos + 4..])
            } else {
                return Err(anyhow!("syntax error in ALTER SYSTEM SET"));
            }
        };

        let name_lower = name.trim().to_lowercase();
        let value_clean = raw_value.trim().trim_matches('\'').trim_matches('"').trim();

        match name_lower.as_str() {
            "statement_timeout" | "idle_in_transaction_session_timeout" => {}
            _ => {
                return Err(anyhow!(
                    "ALTER SYSTEM SET is only supported for statement_timeout and idle_in_transaction_session_timeout"
                ));
            }
        }

        let ms = crate::sql::session::SessionSettings::parse_timeout_value(value_clean)?;

        let server_config = session
            .server_config()
            .ok_or_else(|| anyhow!("server configuration not available"))?;

        {
            let mut cfg = server_config.write().unwrap();
            match name_lower.as_str() {
                "statement_timeout" => cfg.statement_timeout_ms = ms,
                "idle_in_transaction_session_timeout" => {
                    cfg.idle_in_transaction_session_timeout_ms = ms
                }
                _ => unreachable!(),
            }
        }

        Ok(ExecuteResults::single(ExecuteResult::CommandComplete {
            tag: "ALTER SYSTEM",
        }))
    }

    /// Execute `RESET <guc>` or `RESET ALL`.
    ///
    /// Passthrough: no instrumented dispatch, no observability record, no auto
    /// `mark_transaction_failed`.
    fn execute_reset(session: &mut Session, sql_trimmed: &str) -> Result<ExecuteResults> {
        let after_kw = sql_trimmed.get(5..).unwrap_or("");
        let name = crate::sql::raw_sql::extract_reset_name(after_kw)
            .ok_or_else(|| anyhow!("syntax error at or near \"RESET\""))?;
        if name.eq_ignore_ascii_case("ALL") {
            session.reset_all_settings();
        } else {
            crate::sql::executor::check_reserved_guc_reset(name)?;
            session.reset_setting(&name.to_lowercase());
        }
        Ok(ExecuteResults::single(ExecuteResult::CommandComplete {
            tag: "RESET",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::sql::executor::core::dispatch::scaffold::DispatchContext;

    fn make_executor_and_session(
        is_superuser: bool,
        with_server_config: bool,
    ) -> (Executor, Session) {
        let store = crate::storage::TikvStore::new_stub();
        let keyspace = format!(
            "raw_dispatch_tests_{}",
            if is_superuser { "su" } else { "nsu" }
        );
        let observability = crate::observability::registry().tenant(&keyspace);
        let trigger_cache = std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new());
        let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
        let executor = Executor::new(
            store.clone(),
            keyspace,
            observability.clone(),
            crate::pool::TenantMemoryAccountant::unlimited("raw_dispatch_tests".to_string()),
            trigger_cache,
            stats_cache,
        );
        let mut session = Session::new_with_user_and_database(
            store,
            observability,
            "tester".to_string(),
            is_superuser,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        );
        if with_server_config {
            session.set_server_config(ServerConfig::default().shared());
        }
        (executor, session)
    }

    fn assert_command_tag(results: ExecuteResults, expected_tag: &'static str) {
        match results.last() {
            ExecuteResult::CommandComplete { tag } => assert_eq!(tag, expected_tag),
            other => panic!("expected command complete, got {other:?}"),
        }
    }

    #[test]
    fn alter_system_set_requires_superuser() {
        let (executor, mut session) = make_executor_and_session(false, true);
        let err = executor
            .execute_alter_system_set(&mut session, "ALTER SYSTEM SET statement_timeout = '5s'")
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"));
    }

    #[test]
    fn alter_system_set_rejects_invalid_syntax_and_unknown_guc() {
        let (executor, mut session) = make_executor_and_session(true, true);
        let syntax_err = executor
            .execute_alter_system_set(&mut session, "ALTER SYSTEM SET")
            .unwrap_err()
            .to_string();
        assert!(syntax_err.contains("syntax error"));

        let unsupported_err = executor
            .execute_alter_system_set(&mut session, "ALTER SYSTEM SET work_mem = '4MB'")
            .unwrap_err()
            .to_string();
        assert!(unsupported_err.contains("only supported for"));
    }

    #[test]
    fn alter_system_set_updates_server_config_for_supported_gucs() {
        let (executor, mut session) = make_executor_and_session(true, true);

        let r1 = executor
            .execute_alter_system_set(
                &mut session,
                "ALTER SYSTEM SET statement_timeout = '1500ms'",
            )
            .unwrap();
        assert_command_tag(r1, "ALTER SYSTEM");

        let r2 = executor
            .execute_alter_system_set(
                &mut session,
                "ALTER SYSTEM SET idle_in_transaction_session_timeout TO '2s'",
            )
            .unwrap();
        assert_command_tag(r2, "ALTER SYSTEM");

        let cfg = session.server_config().unwrap().read().unwrap().clone();
        assert_eq!(cfg.statement_timeout_ms, 1500);
        assert_eq!(cfg.idle_in_transaction_session_timeout_ms, 2000);
    }

    #[test]
    fn alter_system_set_requires_server_config_handle() {
        let (executor, mut session) = make_executor_and_session(true, false);
        let err = executor
            .execute_alter_system_set(&mut session, "ALTER SYSTEM SET statement_timeout = 100")
            .unwrap_err()
            .to_string();
        assert!(err.contains("server configuration not available"));
    }

    #[test]
    fn execute_reset_handles_single_and_all() {
        let (_, mut session) = make_executor_and_session(true, false);
        session
            .set_known_setting("statement_timeout", "2500".to_string())
            .unwrap();
        assert_eq!(
            session.show_setting_value("statement_timeout").as_deref(),
            Some("2500ms")
        );

        let single = Executor::execute_reset(&mut session, "RESET statement_timeout").unwrap();
        assert_command_tag(single, "RESET");
        assert_eq!(
            session.show_setting_value("statement_timeout").as_deref(),
            Some("0")
        );

        session
            .set_known_setting("statement_timeout", "3000".to_string())
            .unwrap();
        let all = Executor::execute_reset(&mut session, "RESET ALL").unwrap();
        assert_command_tag(all, "RESET");
    }

    #[test]
    fn execute_reset_rejects_invalid_syntax() {
        let (_, mut session) = make_executor_and_session(true, false);
        let err = Executor::execute_reset(&mut session, "RESET")
            .unwrap_err()
            .to_string();
        assert!(err.contains("syntax error"));
    }

    #[test]
    fn alter_system_set_rejects_invalid_timeout_literal() {
        let (executor, mut session) = make_executor_and_session(true, true);
        let err = executor
            .execute_alter_system_set(
                &mut session,
                "ALTER SYSTEM SET statement_timeout = 'not_a_timeout'",
            )
            .unwrap_err()
            .to_string();
        assert!(err.to_lowercase().contains("timeout"));
    }

    #[test]
    fn finish_raw_helpers_mark_failed_only_when_in_transaction() {
        let (executor, mut session) = make_executor_and_session(true, false);
        session.force_test_transaction_state(true, false);

        let err_single = executor.finish_raw_single(
            &mut session,
            "SELECT 1",
            Instant::now(),
            Err(anyhow!("boom")),
        );
        assert!(err_single.is_err());
        assert!(session.is_transaction_failed());

        session.force_test_transaction_state(false, false);
        let err_multi = executor.finish_raw_multi(
            &mut session,
            "SELECT 1",
            Instant::now(),
            Err(anyhow!("boom2")),
        );
        assert!(err_multi.is_err());
        assert!(!session.is_transaction_failed());
    }

    #[test]
    fn passthrough_dispatch_routes_reset_and_alter_system_set() {
        let (executor, mut session) = make_executor_and_session(true, true);

        let alter_ctx =
            DispatchContext::new("ALTER SYSTEM SET statement_timeout = '1200ms'", &session);
        let alter = executor
            .try_dispatch_raw_passthrough(&mut session, &alter_ctx)
            .expect("should dispatch alter system")
            .unwrap();
        assert_command_tag(alter, "ALTER SYSTEM");

        let reset_ctx = DispatchContext::new("RESET statement_timeout", &session);
        let reset = executor
            .try_dispatch_raw_passthrough(&mut session, &reset_ctx)
            .expect("should dispatch reset")
            .unwrap();
        assert_command_tag(reset, "RESET");

        let other_ctx = DispatchContext::new("SELECT 1", &session);
        assert!(executor
            .try_dispatch_raw_passthrough(&mut session, &other_ctx)
            .is_none());
    }
}
