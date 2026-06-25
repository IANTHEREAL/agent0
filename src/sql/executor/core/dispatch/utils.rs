//! Transaction mode validation, schema drift detection, notice collection,
//! and shared runtime helpers for the statement dispatch layer.

use super::super::*;
use std::future::Future;
use std::time::{Duration, Instant};

/// Validate and apply transaction modes (isolation level, access mode) from
/// `BEGIN`, `START TRANSACTION`, `SET TRANSACTION`, or
/// `SET SESSION CHARACTERISTICS AS TRANSACTION`.
///
/// Downgrades SERIALIZABLE to REPEATABLE READ (TiKV snapshot isolation)
/// and stores access mode on either the current/pending transaction or the
/// session default depending on `session_default`.
pub(in crate::sql::executor::core) fn validate_transaction_modes(
    session: &mut Session,
    modes: &[TransactionMode],
    session_default: bool,
) -> Result<()> {
    for mode in modes {
        match mode {
            TransactionMode::IsolationLevel(level) => {
                // Pass through the user-requested level. The validator in
                // validate_transaction_isolation() handles normalization
                // logging and stores the user-facing value for SHOW readback.
                let level_str = match level {
                    TransactionIsolationLevel::ReadUncommitted => "read uncommitted",
                    TransactionIsolationLevel::ReadCommitted => "read committed",
                    TransactionIsolationLevel::RepeatableRead => "repeatable read",
                    TransactionIsolationLevel::Serializable => "serializable",
                };
                session.set_known_setting("transaction_isolation", level_str.to_string())?;
            }
            TransactionMode::AccessMode(access_mode) => {
                let mode_str = match access_mode {
                    TransactionAccessMode::ReadOnly => "on",
                    TransactionAccessMode::ReadWrite => "off",
                };
                let setting = if session_default {
                    "default_transaction_read_only"
                } else {
                    "transaction_read_only"
                };
                session.set_known_setting(setting, mode_str.to_string())?;
            }
        }
    }
    Ok(())
}

/// Apply BEGIN/START TRANSACTION modes only when starting a new transaction.
/// PostgreSQL treats nested BEGIN as a no-op for the active transaction, so a
/// nested `BEGIN READ WRITE` must not revoke an existing READ ONLY transaction.
pub(in crate::sql::executor::core) fn validate_begin_transaction_modes(
    session: &mut Session,
    modes: &[TransactionMode],
) -> Result<()> {
    if session.is_in_transaction() {
        return Ok(());
    }
    validate_transaction_modes(session, modes, false)
}

impl Executor {
    /// Detect schema drift by checking `(table_id, schema_version)` pairs.
    ///
    /// Returns the first mismatch as `(table_name, expected_version, current_version)`.
    /// Uses `table_id` for lookup when available (non-zero), falling back to
    /// name-based lookup for backwards compatibility.
    pub(in crate::sql::executor::core) async fn first_schema_drift_on_txn(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_versions: &[(String, u64, u64)],
    ) -> Result<Option<(String, u64, Option<u64>)>> {
        for (table_name, table_id, expected_version) in table_versions {
            let current_schema = self.store().get_schema(txn, db_id, table_name).await?;
            let has_drift = match current_schema.as_ref() {
                // Backward-compat fallback for historical dependencies that did
                // not record `table_id` (0 sentinel). New dependencies always
                // carry `(table_id, schema_version)`.
                Some(schema) if *table_id == 0 => schema.version != *expected_version,
                Some(schema) => schema.table_id != *table_id || schema.version != *expected_version,
                None => true,
            };

            if has_drift {
                let current_version = current_schema.and_then(|schema| {
                    if *table_id == 0 || schema.table_id == *table_id {
                        Some(schema.version)
                    } else {
                        None
                    }
                });
                return Ok(Some((
                    table_name.clone(),
                    *expected_version,
                    current_version,
                )));
            }
        }
        Ok(None)
    }

    pub(in crate::sql::executor::core) async fn collect_notices_before_statement(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        search_path: &[String],
        stmt: &Statement,
    ) -> Result<Vec<ExecuteResult>> {
        use sqlparser::ast::ObjectType;

        match stmt {
            Statement::Drop {
                object_type: ObjectType::Table,
                names: drop_names,
                if_exists: true,
                ..
            } => {
                let mut notices = Vec::new();
                for name in drop_names {
                    let exists = names::resolve_existing_table_name(
                        self.store.as_ref(),
                        txn,
                        db_id,
                        name,
                        search_path,
                    )
                    .await?
                    .is_some();

                    if exists {
                        continue;
                    }

                    let base = name
                        .0
                        .last()
                        .map(|ident| ident.value.as_str())
                        .unwrap_or("?");
                    notices.push(ExecuteResult::Notice {
                        message: format!("table \"{}\" does not exist, skipping", base),
                        severity: "NOTICE".to_string(),
                        sqlstate: "00000".to_string(),
                    });
                }
                Ok(notices)
            }
            _ => Ok(Vec::new()),
        }
    }
}
pub(in crate::sql::executor::core) fn apply_pending_set_config_mutations(
    session: &mut Session,
) -> Result<()> {
    use crate::sql::query_context::QueryContext;

    let pending = QueryContext::take_set_config_mutations();
    for mutation in pending {
        // LOCAL mutations outside an explicit transaction:
        // - Single-statement implicit tx: LOCAL has no lasting effect (PG parity).
        //   Clean up the runtime override so it doesn't leak.
        // - Multi-statement batch (implicit transaction): LOCAL persists for the
        //   remainder of the batch, matching PostgreSQL's implicit transaction
        //   semantics.  The caller (execute()) clears local_overrides after the
        //   batch completes.
        if mutation.is_local && !session.is_in_transaction() && !session.in_implicit_batch() {
            QueryContext::remove_runtime_setting_override(&mutation.name);
            continue;
        }

        session.ensure_transaction_characteristics_change_allowed(&mutation.name)?;

        // is_reset: NULL value in set_config() → RESET to boot default.
        if mutation.is_reset {
            if mutation.is_local {
                // LOCAL reset: set a LOCAL override to the effective default so
                // the reset is transaction-scoped and reverts on COMMIT/ROLLBACK.
                // Using reset_setting() here would mutate the session-level
                // value and leak past transaction end.
                let boot = session.settings().reset_default_show_value(&mutation.name);
                if mutation.name == "search_path" {
                    let entries = parse_search_path_guc_value(&boot);
                    session.set_local_search_path(entries);
                } else {
                    session.set_local_setting(&mutation.name, boot)?;
                }
            } else {
                // PG parity: for custom GUCs (not in the known registry),
                // NULL reset preserves the setting as empty string rather
                // than removing it.  reset_setting() removes custom GUCs
                // from extra_settings, so use set_known_setting("") instead.
                let is_known = crate::sql::session::settings::GUC_TABLE
                    .iter()
                    .any(|g| g.name == mutation.name);
                if is_known {
                    session.reset_setting(&mutation.name);
                } else {
                    session.set_known_setting(&mutation.name, String::new())?;
                }
            }
            continue;
        }

        if mutation.name == "search_path" {
            // Values from the typed eval handler are already normalized;
            // just parse them back into a list without re-applying the
            // DEFAULT keyword rewrite (which belongs only in SET statement).
            let entries = if mutation.value.is_empty() {
                Vec::new()
            } else {
                parse_search_path_guc_value(&mutation.value)
            };
            if mutation.is_local {
                session.set_local_search_path(entries);
            } else {
                session.set_search_path(entries);
            }
            continue;
        }

        if mutation.is_local {
            session.set_local_setting(&mutation.name, mutation.value)?;
        } else {
            session.set_known_setting(&mutation.name, mutation.value)?;
        }
    }
    Ok(())
}

fn parse_search_path_guc_value(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    for token in s.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let schema = if token.starts_with('\"') && token.ends_with('\"') && token.len() >= 2 {
            token[1..token.len() - 1].to_string()
        } else {
            token.to_lowercase()
        };
        out.push(schema);
    }
    out
}

/// Apply an optional statement timeout to a future.
///
/// If `timeout` is `Some`, wraps the future with `tokio::time::timeout` and
/// converts the elapsed error into `StatementTimeoutError`. If `None`,
/// runs the future directly.
pub(in crate::sql::executor::core) async fn apply_statement_timeout<T>(
    timeout: Option<Duration>,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, fut).await {
            Ok(res) => res,
            Err(_) => Err(anyhow::Error::new(StatementTimeoutError)),
        },
        None => fut.await,
    }
}

pub(in crate::sql::executor::core) fn effective_retry_timeout(
    retry_timeout: Option<Duration>,
    statement_timeout: Option<Duration>,
) -> Option<Duration> {
    match (retry_timeout, statement_timeout) {
        (Some(retry), Some(statement)) => Some(retry.min(statement)),
        (Some(retry), None) => Some(retry),
        (None, Some(statement)) => Some(statement),
        (None, None) => None,
    }
}

pub(in crate::sql::executor::core) fn remaining_statement_timeout(
    started_at: Instant,
    statement_timeout: Option<Duration>,
) -> Option<Duration> {
    statement_timeout.map(|timeout| timeout.saturating_sub(started_at.elapsed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_session(is_superuser: bool) -> Session {
        let store = crate::storage::TikvStore::new_stub();
        let obs = crate::observability::registry().tenant("ut_dispatch_utils");
        Session::new_with_user_and_database(
            store,
            obs,
            "tester".to_string(),
            is_superuser,
            false,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap()
    }

    #[test]
    fn validate_transaction_modes_accepts_supported_modes() {
        let mut session = test_session(true);
        validate_transaction_modes(
            &mut session,
            &[
                TransactionMode::IsolationLevel(TransactionIsolationLevel::ReadCommitted),
                TransactionMode::AccessMode(TransactionAccessMode::ReadOnly),
            ],
            false,
        )
        .unwrap();

        assert_eq!(
            session
                .show_setting_value("transaction_isolation")
                .as_deref(),
            Some("repeatable read")
        );
        assert_eq!(
            session
                .show_setting_value("transaction_read_only")
                .as_deref(),
            Some("on")
        );
        assert_eq!(
            session
                .show_setting_value("default_transaction_read_only")
                .as_deref(),
            Some("off")
        );
    }

    #[test]
    fn validate_begin_transaction_modes_ignores_modes_inside_active_transaction() {
        let mut session = test_session(false);
        validate_transaction_modes(
            &mut session,
            &[TransactionMode::AccessMode(TransactionAccessMode::ReadOnly)],
            false,
        )
        .unwrap();
        session.force_test_transaction_state(true, false);

        validate_begin_transaction_modes(
            &mut session,
            &[TransactionMode::AccessMode(
                TransactionAccessMode::ReadWrite,
            )],
        )
        .unwrap();

        assert_eq!(
            session
                .show_setting_value("transaction_read_only")
                .as_deref(),
            Some("on")
        );
    }

    #[test]
    fn validate_transaction_modes_downgrades_serializable_to_repeatable_read() {
        let mut session = test_session(false);
        validate_transaction_modes(
            &mut session,
            &[TransactionMode::IsolationLevel(
                TransactionIsolationLevel::Serializable,
            )],
            false,
        )
        .unwrap();
        assert_eq!(
            session
                .show_setting_value("transaction_isolation")
                .as_deref(),
            Some("repeatable read")
        );
    }

    #[test]
    fn runtime_settings_reads_session_snapshot() {
        let mut session = test_session(true);
        session
            .set_known_setting("timezone", "Asia/Shanghai".to_string())
            .unwrap();
        session
            .set_known_setting("search_path", "public, pg_catalog".to_string())
            .unwrap();
        session
            .set_known_setting("max_sort_bytes", "4096".to_string())
            .unwrap();

        let settings = crate::sql::runtime_context::RuntimeSettings::from_session(&session);
        assert_eq!(settings.timezone.as_ref(), "Asia/Shanghai");
        assert_eq!(settings.max_sort_bytes, session.max_sort_bytes());
        assert!(settings.search_path.iter().any(|s| s == "public"));
        assert!(settings.is_superuser);
    }

    #[tokio::test]
    async fn apply_statement_timeout_handles_timeout_and_success() {
        let ok = apply_statement_timeout(Some(Duration::from_millis(50)), async {
            tokio::time::sleep(Duration::from_millis(1)).await;
            Ok::<_, anyhow::Error>(7)
        })
        .await
        .unwrap();
        assert_eq!(ok, 7);

        let err = apply_statement_timeout(Some(Duration::from_millis(1)), async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            Ok::<_, anyhow::Error>(1)
        })
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("statement timeout"));
    }

    #[test]
    fn effective_retry_timeout_uses_the_tighter_statement_or_retry_limit() {
        let retry = Duration::from_millis(200);
        let statement = Duration::from_millis(50);

        assert_eq!(
            effective_retry_timeout(Some(retry), Some(statement)),
            Some(statement)
        );
        assert_eq!(
            effective_retry_timeout(Some(statement), Some(retry)),
            Some(statement)
        );
        assert_eq!(
            effective_retry_timeout(None, Some(statement)),
            Some(statement)
        );
        assert_eq!(effective_retry_timeout(Some(retry), None), Some(retry));
        assert_eq!(effective_retry_timeout(None, None), None);
    }

    #[test]
    fn remaining_statement_timeout_saturates_at_zero() {
        let started = Instant::now() - Duration::from_millis(10);

        assert_eq!(
            remaining_statement_timeout(started, Some(Duration::from_millis(5))),
            Some(Duration::ZERO)
        );
        assert_eq!(remaining_statement_timeout(started, None), None);
    }

    #[tokio::test]
    async fn apply_pending_set_config_mutations_preserves_explicit_empty_search_path() {
        let mut session = test_session(true);
        let qctx = crate::sql::query_context::QueryContext::for_tests();

        crate::sql::query_context::with_scoped_query_context(&qctx, async {
            crate::sql::query_context::QueryContext::record_set_config_mutation(
                "search_path",
                "",
                false,
            );
            apply_pending_set_config_mutations(&mut session)
        })
        .await
        .unwrap();

        assert_eq!(
            session.show_setting_value("search_path").as_deref(),
            Some("")
        );
        assert!(session.search_path().is_empty());
    }

    #[tokio::test]
    async fn apply_pending_local_mutation_outside_tx_clears_runtime_override() {
        use crate::sql::query_context::QueryContext;
        let mut session = test_session(true);
        let qctx = QueryContext::for_tests();

        crate::sql::query_context::with_scoped_query_context(&qctx, async {
            // Record a LOCAL mutation (is_local = true).
            QueryContext::record_set_config_mutation("statement_timeout", "550ms", true);
            // Same-statement visibility: override is present.
            assert_eq!(
                QueryContext::current_setting_snapshot("statement_timeout").as_deref(),
                Some("550ms")
            );

            // Session is NOT in a transaction → LOCAL is dropped.
            apply_pending_set_config_mutations(&mut session).unwrap();

            // Runtime override must also be cleaned up so it doesn't leak
            // to later statements in the same batch.
            assert_ne!(
                QueryContext::current_setting_snapshot("statement_timeout").as_deref(),
                Some("550ms"),
                "LOCAL override must be cleaned up after statement flush outside transaction"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn apply_pending_local_mutation_in_implicit_batch_persists() {
        use crate::sql::query_context::QueryContext;
        let mut session = test_session(true);
        // Simulate multi-statement batch (implicit transaction).
        session.set_in_implicit_batch(true);
        let qctx = QueryContext::for_tests();

        crate::sql::query_context::with_scoped_query_context(&qctx, async {
            QueryContext::record_set_config_mutation("statement_timeout", "550ms", true);
            apply_pending_set_config_mutations(&mut session).unwrap();

            // LOCAL mutation must persist as a local_override on the session
            // (not dropped) because we are in an implicit batch.
            assert_eq!(
                session.show_setting_value("statement_timeout").as_deref(),
                Some("550ms"),
                "LOCAL must persist across statements in an implicit batch"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn autocommit_backoff_returns() {
        crate::sql::executor::core::retry::autocommit_backoff(0).await;
        crate::sql::executor::core::retry::autocommit_backoff(8).await;
    }
}
