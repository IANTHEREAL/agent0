use super::super::{ExecuteResult, ExecuteResults, Session};
use super::core::Executor;
use super::triggers::strip_leading_sql_comments;
use crate::sql::error::SqlError;
use crate::storage::LifecycleTenantStatus;
use anyhow::{anyhow, Context, Result};
use tikv_client::TimestampExt;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
struct CreateDatabaseCommand {
    name: String,
    if_not_exists: bool,
    owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DropDatabaseCommand {
    name: String,
    if_exists: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AlterDatabaseCommand {
    Rename { old_name: String, new_name: String },
    Owner { name: String, new_owner: String },
}

type CreateDatabaseLifecycleLive = (u64, u64);
type CreateDatabaseTxnResult = (
    Vec<ExecuteResult>,
    Option<u64>,
    Option<CreateDatabaseLifecycleLive>,
);

fn is_reserved_database_name(name: &str) -> bool {
    matches!(name, "postgres" | "template0" | "template1")
}

fn validate_database_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 63 {
        return Err(anyhow!("invalid database name: {}", name));
    }
    if !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(anyhow!("invalid database name: {}", name));
    }
    Ok(())
}

fn validate_database_name_for_create(name: &str) -> Result<()> {
    validate_database_name(name)?;
    if is_reserved_database_name(name) {
        return Err(anyhow!("cannot use reserved database name \"{}\"", name));
    }
    Ok(())
}

fn parse_single_quoted_literal(token: &str) -> Result<String> {
    if token.len() < 2 || !token.starts_with('\'') || !token.ends_with('\'') {
        return Err(anyhow!("invalid string literal"));
    }
    let inner = &token[1..token.len() - 1];
    // PostgreSQL escapes single quotes as doubled quotes inside string literals.
    Ok(inner.replace("''", "'"))
}

fn parse_identifier_token(token: &str) -> Result<String> {
    let token = token.trim();
    if token.is_empty() {
        return Err(anyhow!("missing identifier"));
    }
    if token.starts_with('"') {
        if token.len() < 2 || !token.ends_with('"') {
            return Err(anyhow!("invalid quoted identifier"));
        }
        let inner = &token[1..token.len() - 1];
        Ok(inner.replace("\"\"", "\""))
    } else {
        Ok(token.to_string())
    }
}

fn parse_value_as_string(token: &str) -> Result<String> {
    if token.starts_with('\'') {
        parse_single_quoted_literal(token)
    } else {
        parse_identifier_token(token)
    }
}

fn tokenize_sql(input: &str) -> Vec<&str> {
    use crate::sql::scanner::SqlCharScanner;

    let mut tokens = Vec::new();
    let mut token_start: Option<usize> = None;
    let mut prev_in_string = false;

    for ctx in SqlCharScanner::new(input) {
        if ctx.in_string() {
            if token_start.is_none() {
                token_start = Some(ctx.pos);
            }
            prev_in_string = true;
            continue;
        }

        // Just exited a string — emit it as a token.
        if prev_in_string {
            if let Some(start) = token_start {
                tokens.push(&input[start..ctx.pos]);
                token_start = None;
            }
            prev_in_string = false;
        }

        let b = ctx.byte;
        if b.is_ascii_whitespace() {
            if let Some(start) = token_start {
                tokens.push(&input[start..ctx.pos]);
                token_start = None;
            }
            continue;
        }

        if matches!(b, b'(' | b')' | b',' | b';' | b'=') {
            if let Some(start) = token_start {
                tokens.push(&input[start..ctx.pos]);
                token_start = None;
            }
            tokens.push(&input[ctx.pos..ctx.pos + 1]);
            continue;
        }

        if token_start.is_none() {
            token_start = Some(ctx.pos);
        }
    }

    // Flush remaining token.
    if let Some(start) = token_start {
        tokens.push(&input[start..]);
    }

    tokens
}

fn strip_trailing_semicolons(tokens: &mut Vec<&str>) {
    while matches!(tokens.last().copied(), Some(";")) {
        tokens.pop();
    }
}

fn parse_create_database_sql(sql: &str) -> Result<CreateDatabaseCommand> {
    let sql = strip_leading_sql_comments(sql).trim();
    let mut tokens = tokenize_sql(sql);
    strip_trailing_semicolons(&mut tokens);

    let mut pos = 0usize;
    let expect = |tokens: &[&str], pos: &mut usize, kw: &str| -> Result<()> {
        let tok = tokens.get(*pos).copied().unwrap_or("");
        if !tok.eq_ignore_ascii_case(kw) {
            return Err(anyhow!("Invalid CREATE DATABASE syntax"));
        }
        *pos += 1;
        Ok(())
    };

    expect(&tokens, &mut pos, "CREATE")?;
    expect(&tokens, &mut pos, "DATABASE")?;

    let mut if_not_exists = false;
    if tokens
        .get(pos)
        .copied()
        .unwrap_or("")
        .eq_ignore_ascii_case("IF")
        && tokens
            .get(pos + 1)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("NOT")
        && tokens
            .get(pos + 2)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("EXISTS")
    {
        if_not_exists = true;
        pos += 3;
    }

    let name_token = tokens
        .get(pos)
        .copied()
        .ok_or_else(|| anyhow!("Missing database name"))?;
    pos += 1;
    let name = parse_identifier_token(name_token)?.to_ascii_lowercase();
    validate_database_name_for_create(&name)?;

    let mut owner: Option<String> = None;

    while pos < tokens.len() {
        let tok = tokens[pos];
        if tok.eq_ignore_ascii_case("WITH") {
            pos += 1;
            continue;
        }

        if tok.eq_ignore_ascii_case("OWNER") {
            pos += 1;
            if tokens.get(pos).copied() == Some("=") {
                pos += 1;
            }
            let owner_token = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing owner name"))?;
            pos += 1;
            owner = Some(parse_identifier_token(owner_token)?.to_ascii_lowercase());
            continue;
        }

        if tok.eq_ignore_ascii_case("TEMPLATE") {
            pos += 1;
            if tokens.get(pos).copied() == Some("=") {
                pos += 1;
            }
            let tmpl_token = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing TEMPLATE value"))?;
            pos += 1;
            let tmpl = parse_identifier_token(tmpl_token)?.to_ascii_lowercase();
            if !matches!(tmpl.as_str(), "template0" | "template1") {
                return Err(anyhow!(
                    "CREATE DATABASE TEMPLATE is not supported (got \"{}\")",
                    tmpl
                ));
            }
            continue;
        }

        if tok.eq_ignore_ascii_case("ENCODING") {
            pos += 1;
            if tokens.get(pos).copied() == Some("=") {
                pos += 1;
            }
            let enc_token = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing ENCODING value"))?;
            pos += 1;
            let enc = parse_value_as_string(enc_token)?.to_ascii_lowercase();
            if enc != "utf8" && enc != "utf-8" {
                return Err(anyhow!("only UTF8 encoding is supported"));
            }
            continue;
        }

        if tok.eq_ignore_ascii_case("LC_COLLATE")
            || tok.eq_ignore_ascii_case("LC_CTYPE")
            || tok.eq_ignore_ascii_case("LOCALE")
        {
            pos += 1;
            if tokens.get(pos).copied() == Some("=") {
                pos += 1;
            }
            // Accept and ignore locale values from pg_dump/pg_restore scripts.
            let _ = tokens
                .get(pos)
                .copied()
                .ok_or_else(|| anyhow!("Missing {} value", tok))?;
            pos += 1;
            continue;
        }

        return Err(
            SqlError::Unsupported(format!("Unsupported CREATE DATABASE option: {}", tok)).into(),
        );
    }

    Ok(CreateDatabaseCommand {
        name,
        if_not_exists,
        owner,
    })
}

fn parse_drop_database_sql(sql: &str) -> Result<DropDatabaseCommand> {
    let sql = strip_leading_sql_comments(sql).trim();
    let mut tokens = tokenize_sql(sql);
    strip_trailing_semicolons(&mut tokens);

    let mut pos = 0usize;
    let expect = |tokens: &[&str], pos: &mut usize, kw: &str| -> Result<()> {
        let tok = tokens.get(*pos).copied().unwrap_or("");
        if !tok.eq_ignore_ascii_case(kw) {
            return Err(anyhow!("Invalid DROP DATABASE syntax"));
        }
        *pos += 1;
        Ok(())
    };

    expect(&tokens, &mut pos, "DROP")?;
    expect(&tokens, &mut pos, "DATABASE")?;

    let mut if_exists = false;
    if tokens
        .get(pos)
        .copied()
        .unwrap_or("")
        .eq_ignore_ascii_case("IF")
        && tokens
            .get(pos + 1)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("EXISTS")
    {
        if_exists = true;
        pos += 2;
    }

    let name_token = tokens
        .get(pos)
        .copied()
        .ok_or_else(|| anyhow!("Missing database name"))?;
    pos += 1;
    let name = parse_identifier_token(name_token)?.to_ascii_lowercase();
    validate_database_name(&name)?;

    if pos < tokens.len() {
        return Err(SqlError::Unsupported("Unsupported DROP DATABASE syntax".into()).into());
    }

    Ok(DropDatabaseCommand { name, if_exists })
}

fn parse_alter_database_sql(sql: &str) -> Result<AlterDatabaseCommand> {
    let sql = strip_leading_sql_comments(sql).trim();
    let mut tokens = tokenize_sql(sql);
    strip_trailing_semicolons(&mut tokens);

    let mut pos = 0usize;
    let expect = |tokens: &[&str], pos: &mut usize, kw: &str| -> Result<()> {
        let tok = tokens.get(*pos).copied().unwrap_or("");
        if !tok.eq_ignore_ascii_case(kw) {
            return Err(anyhow!("Invalid ALTER DATABASE syntax"));
        }
        *pos += 1;
        Ok(())
    };

    expect(&tokens, &mut pos, "ALTER")?;
    expect(&tokens, &mut pos, "DATABASE")?;

    let name_token = tokens
        .get(pos)
        .copied()
        .ok_or_else(|| anyhow!("Missing database name"))?;
    pos += 1;
    let name = parse_identifier_token(name_token)?.to_ascii_lowercase();
    validate_database_name(&name)?;

    let op = tokens.get(pos).copied().unwrap_or("");
    if op.eq_ignore_ascii_case("RENAME") {
        pos += 1;
        expect(&tokens, &mut pos, "TO")?;
        let new_token = tokens
            .get(pos)
            .copied()
            .ok_or_else(|| anyhow!("Missing new database name"))?;
        pos += 1;
        let new_name = parse_identifier_token(new_token)?.to_ascii_lowercase();
        validate_database_name(&new_name)?;
        if pos < tokens.len() {
            return Err(SqlError::Unsupported("Unsupported ALTER DATABASE syntax".into()).into());
        }
        return Ok(AlterDatabaseCommand::Rename {
            old_name: name,
            new_name,
        });
    }

    if op.eq_ignore_ascii_case("OWNER") {
        pos += 1;
        if tokens
            .get(pos)
            .copied()
            .unwrap_or("")
            .eq_ignore_ascii_case("TO")
            || tokens.get(pos).copied() == Some("=")
        {
            pos += 1;
        }
        let owner_token = tokens
            .get(pos)
            .copied()
            .ok_or_else(|| anyhow!("Missing owner name"))?;
        pos += 1;
        let new_owner = parse_identifier_token(owner_token)?.to_ascii_lowercase();
        if pos < tokens.len() {
            return Err(SqlError::Unsupported("Unsupported ALTER DATABASE syntax".into()).into());
        }
        return Ok(AlterDatabaseCommand::Owner { name, new_owner });
    }

    Err(SqlError::Unsupported("Unsupported ALTER DATABASE operation".into()).into())
}

impl Executor {
    pub(crate) async fn execute_create_database_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResults> {
        let cmd = parse_create_database_sql(sql)?;

        if !session.is_superuser() {
            return Err(SqlError::PermissionDenied {
                object_type: "database".into(),
                object_name: cmd.name.clone(),
            }
            .into());
        }
        if session.is_in_transaction() {
            return Err(anyhow!(
                "CREATE DATABASE cannot run inside a transaction block"
            ));
        }

        let lifecycle_store = crate::worker::db_lifecycle_store()?.clone();
        let keyspace = self.tenant_keyspace().to_string();

        session.begin().await?;
        let result: Result<CreateDatabaseTxnResult> = async {
            let owner = cmd
                .owner
                .clone()
                .or_else(|| session.current_user().map(|u| u.to_string()))
                .unwrap_or_else(|| "postgres".to_string());

            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let created = self
                .store()
                .create_database(txn, &cmd.name, &owner, cmd.if_not_exists)
                .await?;

            let mut lifecycle_live = None;
            if let Some(def) = created.as_ref() {
                let incarnation =
                    crate::worker::lifecycle::allocate_tenant_incarnation(&lifecycle_store).await?;
                self.store()
                    .put_tenant_incarnation_stamp(txn, def.id, incarnation)
                    .await?;
                lifecycle_live = Some((def.id, incarnation));
            }

            let registry_db_id = match created.as_ref() {
                Some(def) => Some(def.id),
                None => self.store().get_database_id(txn, &cmd.name).await?,
            };

            let mut results = Vec::new();
            if created.is_none() {
                results.push(ExecuteResult::Notice {
                    message: format!("database \"{}\" already exists, skipping", cmd.name),
                    severity: "NOTICE".to_string(),
                    sqlstate: "00000".to_string(),
                });
            }
            results.push(ExecuteResult::CommandComplete {
                tag: "CREATE DATABASE",
            });
            Ok((results, registry_db_id, lifecycle_live))
        }
        .await;

        let (results, registry_db_id, lifecycle_live) = match result {
            Ok(value) => value,
            Err(e) => {
                session.rollback().await?;
                return Err(e);
            }
        };

        session.commit().await?;

        if let Some((db_id, incarnation)) = lifecycle_live {
            if let Err(e) = crate::worker::lifecycle::publish_tenant_lifecycle_status(
                &lifecycle_store,
                &keyspace,
                db_id,
                incarnation,
                LifecycleTenantStatus::Live,
            )
            .await
            {
                warn!(
                    "CREATE DATABASE lifecycle Live publish failed for database '{}' \
                     (keyspace='{}', db_id={}, incarnation={}): {}",
                    cmd.name, keyspace, db_id, incarnation, e
                );
            }
        }

        if let (Some(system_store), Some(db_id)) =
            (crate::worker::get_system_store(), registry_db_id)
        {
            if let Err(e) = crate::worker::ensure_database_inventory_row_with_retry(
                system_store.as_ref(),
                &keyspace,
                db_id,
                3,
            )
            .await
            {
                warn!(
                    "CREATE DATABASE worker inventory registration failed for database '{}' \
                     (keyspace='{}', db_id={}): {}",
                    cmd.name, keyspace, db_id, e
                );
            }
        }
        Ok(ExecuteResults(results))
    }

    pub(crate) async fn execute_drop_database_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResults> {
        let cmd = parse_drop_database_sql(sql)?;

        if !session.is_superuser() {
            return Err(SqlError::PermissionDenied {
                object_type: "database".into(),
                object_name: cmd.name.clone(),
            }
            .into());
        }
        if session.is_in_transaction() {
            return Err(anyhow!(
                "DROP DATABASE cannot run inside a transaction block"
            ));
        }

        let keyspace = self.tenant_keyspace().to_string();

        let lifecycle_store = crate::worker::db_lifecycle_store()?.clone();

        session.begin().await?;

        let result: Result<_> = async {
            let current_db_id = session.current_database_id();
            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            let lifecycle_incarnation = match self.store().get_database_id(txn, &cmd.name).await? {
                Some(db_id) => {
                    match self
                        .store()
                        .get_tenant_incarnation_stamp_for_update(txn, db_id)
                        .await?
                    {
                        Some(incarnation) => Some(incarnation),
                        None => {
                            let incarnation =
                                crate::worker::lifecycle::allocate_tenant_incarnation(
                                    &lifecycle_store,
                                )
                                .await?;
                            self.store()
                                .put_tenant_incarnation_stamp(txn, db_id, incarnation)
                                .await?;
                            Some(incarnation)
                        }
                    }
                }
                None => None,
            };

            let dropped = self
                .store()
                .drop_database_metadata(txn, &cmd.name, cmd.if_exists, current_db_id)
                .await?;

            let mut results = Vec::new();
            if dropped.is_none() && cmd.if_exists {
                results.push(ExecuteResult::Notice {
                    message: format!("database \"{}\" does not exist, skipping", cmd.name),
                    severity: "NOTICE".to_string(),
                    sqlstate: "00000".to_string(),
                });
            }
            Ok((dropped, lifecycle_incarnation, results))
        }
        .await;

        let (dropped_result, lifecycle_incarnation, mut results) = match result {
            Ok(result) => result,
            Err(e) => {
                session.rollback().await?;
                return Err(e);
            }
        };

        // Record HNSW S3 cleanup only after DROP preflight succeeds, but
        // before committing tenant metadata deletion. Writing the durable
        // system-store intent does not require this SQL node to have S3
        // credentials; only executing the cleanup does. That keeps rejected
        // drops from leaving stale external intents while preserving the
        // crash-safe handoff for successful drops on any SQL node.
        let mut hnsw_s3_db_id: Option<u64> = None;
        if let Some((db_id, _)) = dropped_result.as_ref() {
            let drop_txn_start_ts = {
                let (txn, _, _) = session
                    .get_mut_txn_sequence_values_and_search_path()
                    .expect("Transaction must be active");
                txn.start_timestamp().version()
            };
            if let Err(e) = crate::worker::request_hnsw_s3_db_prefix_cleanup(
                &keyspace,
                *db_id,
                drop_txn_start_ts,
                "drop_database",
            )
            .await
            {
                session.rollback().await?;
                return Err(anyhow!(
                    "DROP DATABASE '{}' could not record HNSW S3 cleanup intent \
                     (keyspace='{}', db_id={}): {}",
                    cmd.name,
                    keyspace,
                    db_id,
                    e
                ));
            }
            hnsw_s3_db_id = Some(*db_id);
        }

        let drop_system_store = if dropped_result.is_some() {
            match crate::worker::system_store() {
                Ok(store) => Some(store.clone()),
                Err(e) => {
                    session.rollback().await?;
                    return Err(e);
                }
            }
        } else {
            None
        };

        let mut dropping_intent_created_at_ms = None;
        if let Some((db_id, _)) = dropped_result.as_ref() {
            let system_store = drop_system_store
                .as_ref()
                .expect("DROP system store prechecked before tenant commit");
            let intent_created_at_ms = crate::worker::now_epoch_ms();
            if let Err(e) = system_store
                .put_dropping_db_intent_with_retry(&keyspace, *db_id, intent_created_at_ms)
                .await
                .with_context(|| {
                    format!(
                        "DROP DATABASE '{}' could not publish dropping intent \
                         (keyspace='{}', db_id={})",
                        cmd.name, keyspace, db_id
                    )
                })
            {
                session.rollback().await?;
                return Err(e);
            }
            dropping_intent_created_at_ms = Some(intent_created_at_ms);
        }

        let mut tenant_commit_error: Option<anyhow::Error> = None;
        if let Err(e) = session.commit().await {
            let mut committed_despite_error = false;
            if let (Some((db_id, _)), Some(_system_store), Some(_intent_created_at_ms)) = (
                dropped_result.as_ref(),
                drop_system_store.as_ref(),
                dropping_intent_created_at_ms,
            ) {
                let db_still_exists = async {
                    let mut txn = self.store().begin().await?;
                    let exists = self
                        .store()
                        .get_database_by_id(&mut txn, *db_id)
                        .await?
                        .is_some();
                    txn.rollback().await.ok();
                    Ok::<bool, anyhow::Error>(exists)
                }
                .await;
                match db_still_exists {
                    Ok(true) => {
                        warn!(
                            "DROP DATABASE '{}' tenant commit failed and DB still exists \
                             (keyspace='{}', db_id={}); retaining dropping intent because \
                             commit errors can be outcome-unknown and repair owns stale-intent cleanup",
                            cmd.name, keyspace, db_id
                        );
                    }
                    Ok(false) => {
                        warn!(
                            "DROP DATABASE '{}' tenant commit returned an error but DB metadata \
                             is already gone (keyspace='{}', db_id={}); treating DROP as \
                             committed for cleanup/lifecycle repair while returning the original error",
                            cmd.name, keyspace, db_id
                        );
                        committed_despite_error = true;
                    }
                    Err(read_err) => {
                        warn!(
                            "DROP DATABASE '{}' tenant commit returned an error and outcome \
                             could not be resolved (keyspace='{}', db_id={}): {}; retaining \
                             dropping intent for repair",
                            cmd.name, keyspace, db_id, read_err
                        );
                    }
                }
            }
            if committed_despite_error {
                tenant_commit_error = Some(e);
            } else {
                session.rollback().await.ok();
                return Err(e);
            }
        }

        if let Some((db_id, _)) = dropped_result.as_ref() {
            let system_store = drop_system_store
                .as_ref()
                .expect("DROP system store prechecked before tenant commit");

            let worker_cleanup_done = match system_store
                .reap_db_queue_entries_then_delete_worker_registry(&keyspace, *db_id)
                .await
            {
                Ok(n) => {
                    if n > 0 {
                        tracing::info!(
                            "DROP DATABASE '{}': reaped {} worker queue entries for db_id={}",
                            cmd.name,
                            n,
                            db_id
                        );
                    }
                    true
                }
                Err(e) => {
                    warn!(
                        "DROP DATABASE '{}' committed tenant metadata deletion but could not \
                         reap worker queue/registry rows for db_id={}; continuing physical \
                         data cleanup and retaining dropping intent/worker registry for retry: {}",
                        cmd.name, db_id, e
                    );
                    false
                }
            };

            if worker_cleanup_done {
                if let Err(e) = system_store
                    .finalize_dropping_db_intent_with_retry(&keyspace, *db_id)
                    .await
                {
                    warn!(
                        "DROP DATABASE '{}' committed tenant metadata deletion and reaped \
                         worker rows, but could not finalize the dropped-DB tombstone for \
                         db_id={}; continuing physical data cleanup and retaining dropping intent for retry: {}",
                        cmd.name, db_id, e
                    );
                }
            }

            if let Some(incarnation) = lifecycle_incarnation {
                if let Err(e) = crate::worker::lifecycle::publish_tenant_lifecycle_status(
                    &lifecycle_store,
                    &keyspace,
                    *db_id,
                    incarnation,
                    LifecycleTenantStatus::Dropped,
                )
                .await
                {
                    warn!(
                        "DROP DATABASE '{}' lifecycle Dropped publish failed \
                         (keyspace='{}', db_id={}, incarnation={}): {}",
                        cmd.name, keyspace, db_id, incarnation, e
                    );
                }
            } else {
                warn!(
                    "DROP DATABASE '{}' has no tenant incarnation stamp \
                     (keyspace='{}', db_id={}); lifecycle drop inventory not published",
                    cmd.name, keyspace, db_id
                );
            }
        }

        if let Some((db_id, mut dropping_guard)) = dropped_result {
            // Step 1: Delete all HNSW text-format keys for this database.
            // HNSW keys use text format (d_{db_id}_hnsw_...) which falls
            // OUTSIDE the binary range that unsafe_destroy_range deletes.
            // Without this, they persist as permanent orphans.
            //
            // This also closes the concurrent-merge race: any in-flight
            // merge worker's get_for_update(hnsw_meta_key) will return
            // None after this scan-delete, causing the worker to abort
            // BEFORE uploading to S3. This prevents new S3 orphans from
            // being created between cleanup and destroy_range.
            {
                let prefix_start = crate::sql::hnsw::storage::hnsw_db_prefix(db_id);
                let prefix_end = crate::sql::hnsw::storage::hnsw_db_prefix_end(db_id);
                let mut cursor = prefix_start.clone();
                loop {
                    let mut txn = self.store().begin().await?;
                    let range: tikv_client::BoundRange =
                        (cursor.clone()..prefix_end.clone()).into();
                    let keys: Vec<tikv_client::Key> = txn.scan_keys(range, 1_000).await?.collect();
                    if keys.is_empty() {
                        txn.rollback().await.ok();
                        break;
                    }
                    // Advance cursor past the last key for the next batch.
                    let last: Vec<u8> = keys.last().unwrap().clone().into();
                    let mut next = last;
                    next.push(0x00);
                    cursor = next;
                    for key in &keys {
                        txn.delete(key.clone()).await?;
                    }
                    txn.commit().await?;
                }
            }

            // Step 2: Clean up HNSW S3 objects.
            // Now that all meta keys are deleted, no new S3 uploads can
            // start for this database. Await (not spawn) because
            // unsafe_destroy_range would remove any remaining markers.
            if let Some(s3_db_id) = hnsw_s3_db_id {
                match crate::worker::complete_hnsw_s3_db_prefix_cleanup_for_dropped_db(
                    &keyspace, s3_db_id,
                )
                .await
                {
                    Ok(deleted) if deleted > 0 => {
                        tracing::info!(
                            "DROP DATABASE '{}': deleted {} HNSW S3 objects for db_id={}",
                            cmd.name,
                            deleted,
                            s3_db_id
                        );
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!(
                            db_id = s3_db_id,
                            error = %e,
                            "DROP DATABASE: S3 HNSW cleanup failed; durable cleanup intent retained"
                        );
                    }
                }
            }

            // Step 3: Destroy the binary-format key range.
            if let Err(e) = self.store().unsafe_destroy_database_data(db_id).await {
                warn!(
                    "DROP DATABASE '{}': failed to destroy data range for db_id={}: {}",
                    cmd.name, db_id, e
                );
            }

            // Step 4: Finalize the dropping guard — remove the registry entry.
            // This allows the (keyspace, db_id) to be reused if the same
            // database name is re-created.
            dropping_guard.commit();
        }

        if let Some(e) = tenant_commit_error {
            session.rollback().await.ok();
            return Err(e);
        }

        results.push(ExecuteResult::CommandComplete {
            tag: "DROP DATABASE",
        });
        Ok(ExecuteResults(results))
    }

    pub(crate) async fn execute_alter_database_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResults> {
        let cmd = parse_alter_database_sql(sql)?;

        if !session.is_superuser() {
            let db_name = match &cmd {
                AlterDatabaseCommand::Rename { old_name, .. } => old_name.clone(),
                AlterDatabaseCommand::Owner { name, .. } => name.clone(),
            };
            return Err(SqlError::PermissionDenied {
                object_type: "database".into(),
                object_name: db_name,
            }
            .into());
        }
        if session.is_in_transaction() {
            return Err(anyhow!(
                "ALTER DATABASE cannot run inside a transaction block"
            ));
        }

        session.begin().await?;
        let result: Result<()> = async {
            let current_db_id = session.current_database_id();
            let (txn, _sequence_values, _search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .expect("Transaction must be active");

            match cmd {
                AlterDatabaseCommand::Rename { old_name, new_name } => {
                    self.store()
                        .rename_database(txn, &old_name, &new_name, current_db_id)
                        .await?;
                }
                AlterDatabaseCommand::Owner { name, new_owner } => {
                    self.store()
                        .set_database_owner(txn, &name, &new_owner)
                        .await?;
                }
            }
            Ok(())
        }
        .await;

        if result.is_ok() {
            session.commit().await?;
        } else {
            session.rollback().await?;
        }

        result?;
        Ok(ExecuteResults::single(ExecuteResult::CommandComplete {
            tag: "ALTER DATABASE",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize_respects_single_quoted_strings_with_spaces() {
        let tokens = tokenize_sql("LC_COLLATE = 'English_United States.1252';");
        assert_eq!(
            tokens,
            vec!["LC_COLLATE", "=", "'English_United States.1252'", ";"]
        );
    }

    #[test]
    fn test_parse_create_database_minimal() {
        let cmd = parse_create_database_sql("CREATE DATABASE testdb;").unwrap();
        assert_eq!(
            cmd,
            CreateDatabaseCommand {
                name: "testdb".to_string(),
                if_not_exists: false,
                owner: None
            }
        );
    }

    #[test]
    fn test_parse_create_database_if_not_exists_and_owner() {
        let cmd =
            parse_create_database_sql("CREATE DATABASE IF NOT EXISTS testdb WITH OWNER = admin;")
                .unwrap();
        assert_eq!(cmd.name, "testdb");
        assert!(cmd.if_not_exists);
        assert_eq!(cmd.owner.as_deref(), Some("admin"));
    }

    #[test]
    fn test_parse_create_database_pg_dump_style_options() {
        let cmd = parse_create_database_sql(
            "CREATE DATABASE dvdrental WITH TEMPLATE = template0 ENCODING = 'UTF8' LC_COLLATE = 'English_United States.1252' LC_CTYPE = 'English_United States.1252';",
        )
        .unwrap();
        assert_eq!(cmd.name, "dvdrental");
        assert_eq!(cmd.owner, None);
    }

    #[test]
    fn test_parse_drop_database_if_exists() {
        let cmd = parse_drop_database_sql("DROP DATABASE IF EXISTS testdb;").unwrap();
        assert_eq!(
            cmd,
            DropDatabaseCommand {
                name: "testdb".to_string(),
                if_exists: true
            }
        );
    }

    #[test]
    fn drop_database_uses_ordered_worker_inventory_cleanup() {
        let source = include_str!("database.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("database.rs must contain #[cfg(test)]");
        let drop_fn = prod_source
            .split("pub(crate) async fn execute_drop_database_cmd(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) async fn execute_alter_database_cmd(")
                    .next()
            })
            .expect("DROP DATABASE executor must exist before ALTER DATABASE executor");
        let cleanup_pos = drop_fn
            .find("reap_db_queue_entries_then_delete_worker_registry")
            .expect("DROP DATABASE worker cleanup block must exist");
        let cleanup_source = &drop_fn[cleanup_pos..];

        assert!(
            cleanup_source.contains("reap_db_queue_entries_then_delete_worker_registry"),
            "DROP DATABASE must use the ordered worker cleanup helper"
        );

        let reap_error_branch = cleanup_source
            .split("Err(e) => {")
            .nth(1)
            .expect("DROP DATABASE worker cleanup error branch must exist");
        assert!(
            reap_error_branch.contains("retaining dropping intent/worker registry for retry"),
            "worker cleanup failures must leave the registry row as retry inventory"
        );
        assert!(
            !reap_error_branch.contains("delete_worker_registry"),
            "worker cleanup failure branch must not delete the registry row"
        );
    }

    #[test]
    fn drop_database_resolves_lifecycle_store_before_opening_session_txn() {
        let source = include_str!("database.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("database.rs must contain #[cfg(test)]");
        let drop_fn = prod_source
            .split("pub(crate) async fn execute_drop_database_cmd(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) async fn execute_alter_database_cmd(")
                    .next()
            })
            .expect("DROP DATABASE executor must exist before ALTER DATABASE executor");
        let lifecycle_store_pos = drop_fn
            .find("db_lifecycle_store()?.clone()")
            .expect("DROP DATABASE must resolve lifecycle store");
        let begin_pos = drop_fn
            .find("session.begin().await?")
            .expect("DROP DATABASE must open a session transaction");
        assert!(
            lifecycle_store_pos < begin_pos,
            "fallible lifecycle-store lookup must happen before session.begin(), or an early error can leave the session transaction open"
        );
    }

    #[test]
    fn drop_database_publishes_precommit_worker_fence_then_finalizes_tombstone() {
        let source = include_str!("database.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("database.rs must contain #[cfg(test)]");
        let drop_fn = prod_source
            .split("pub(crate) async fn execute_drop_database_cmd(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) async fn execute_alter_database_cmd(")
                    .next()
            })
            .expect("DROP DATABASE executor must exist before ALTER DATABASE executor");

        let metadata_drop_pos = drop_fn
            .find("drop_database_metadata")
            .expect("DROP DATABASE must delete tenant metadata");
        let precommit_intent_pos = drop_fn
            .find("put_dropping_db_intent_with_retry")
            .expect("DROP DATABASE must publish a pre-commit worker enqueue fence");
        let commit_pos = drop_fn
            .find("session.commit().await")
            .expect("DROP DATABASE must commit tenant metadata deletion");
        let tombstone_pos = drop_fn[commit_pos..]
            .find("finalize_dropping_db_intent_with_retry")
            .map(|offset| commit_pos + offset)
            .expect("DROP DATABASE must finalize the dropping intent into a tombstone after tenant commit");
        let lifecycle_pos = drop_fn
            .find("LifecycleTenantStatus::Dropped")
            .expect("DROP DATABASE must publish lifecycle Dropped inventory");
        let s3_cleanup_pos = drop_fn
            .find("complete_hnsw_s3_db_prefix_cleanup_for_dropped_db")
            .expect("DROP DATABASE must run HNSW S3 cleanup");
        let range_destroy_pos = drop_fn
            .find("unsafe_destroy_database_data")
            .expect("DROP DATABASE must destroy the database key range");
        let worker_reap_pos = drop_fn
            .find("reap_db_queue_entries_then_delete_worker_registry")
            .expect("DROP DATABASE must still reap old worker rows");

        assert!(
            metadata_drop_pos < precommit_intent_pos
                && precommit_intent_pos < commit_pos
                && commit_pos < worker_reap_pos
                && worker_reap_pos < tombstone_pos
                && tombstone_pos < lifecycle_pos
                && tombstone_pos < s3_cleanup_pos
                && worker_reap_pos < range_destroy_pos,
            "DROP DATABASE must publish the worker enqueue fence before tenant commit, reap worker rows before deleting the dropping-intent retry handle, and finalize before lifecycle/physical cleanup"
        );
        assert!(
            drop_fn
                .split("if let Err(e) = session.commit().await")
                .nth(1)
                .and_then(|rest| rest.split("if let Some((db_id, _)) = dropped_result.as_ref()").next())
                .is_some_and(|branch| branch.contains("get_database_by_id")
                    && branch.contains("session.rollback().await.ok()")
                    && !branch.contains("delete_dropping_db_intent_if_created_at_with_retry")
                    && branch.contains("committed for cleanup/lifecycle repair")
                    && branch.contains("retaining dropping intent")),
            "tenant commit errors must not delete the only pre-commit fence; if metadata is already gone, post-commit cleanup/lifecycle still runs before returning the original error"
        );
        assert!(
            drop_fn
                .split("if let Some(e) = tenant_commit_error")
                .nth(1)
                .and_then(|rest| rest.split("return Err(e);").next())
                .is_some_and(|branch| branch.contains("session.rollback().await.ok()")),
            "ambiguous-success DROP must clear the autocommit session transaction state before returning the original commit error"
        );
        assert!(
            drop_fn
                .split("put_dropping_db_intent_with_retry")
                .nth(1)
                .and_then(|rest| rest.split("session.commit().await").next())
                .is_some_and(|branch| branch.contains("session.rollback().await?")
                    && branch.contains("return Err(e);")),
            "if the pre-commit dropping-intent publish fails, DROP DATABASE must roll back the active tenant transaction before returning"
        );
        let tombstone_failure_branch = drop_fn
            .split("continuing physical")
            .nth(1)
            .expect("cleanup/finalize failure must warn that physical cleanup continues");
        assert!(
            tombstone_failure_branch.contains("retaining dropping intent"),
            "post-commit worker cleanup/finalize failure must retain dropping intent as retry inventory"
        );
        let tombstone_to_destroy = &drop_fn[tombstone_pos..range_destroy_pos];
        assert!(
            !tombstone_to_destroy.contains(".with_context(||"),
            "post-commit tombstone finalize failure must not return before physical data cleanup"
        );
    }

    #[test]
    fn drop_database_records_hnsw_s3_cleanup_intent_before_metadata_commit() {
        let source = include_str!("database.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("database.rs must contain #[cfg(test)]");
        let drop_fn = prod_source
            .split("pub(crate) async fn execute_drop_database_cmd(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) async fn execute_alter_database_cmd(")
                    .next()
            })
            .expect("DROP DATABASE executor must exist before ALTER DATABASE executor");

        let intent_pos = drop_fn
            .find("request_hnsw_s3_db_prefix_cleanup")
            .expect("DROP DATABASE must record durable S3 cleanup intent");
        let metadata_drop_pos = drop_fn
            .find("drop_database_metadata")
            .expect("DROP DATABASE must perform metadata preflight/drop");
        let commit_pos = drop_fn
            .find("session.commit().await")
            .expect("DROP DATABASE must commit tenant metadata deletion");
        assert!(
            metadata_drop_pos < intent_pos,
            "DROP DATABASE must not record S3 cleanup intent before metadata preflight succeeds"
        );
        assert!(
            intent_pos < commit_pos,
            "DROP DATABASE must record the S3 cleanup intent before committing metadata deletion"
        );
        assert!(
            !drop_fn[..intent_pos].contains("hnsw_s3_client().is_some()"),
            "DROP DATABASE must record the durable S3 cleanup intent even when this SQL node has no S3 client"
        );
        let preflight_section = &drop_fn[..metadata_drop_pos];
        assert!(
            !preflight_section.contains("request_hnsw_s3_db_prefix_cleanup"),
            "rejected DROP DATABASE preflight paths must not leave durable S3 cleanup intents"
        );

        assert!(
            drop_fn.contains("complete_hnsw_s3_db_prefix_cleanup_for_dropped_db"),
            "DROP DATABASE must complete S3 prefix cleanup through the external-object protocol"
        );
        let cleanup_failure = drop_fn
            .split("durable cleanup intent retained")
            .nth(1)
            .expect("DROP DATABASE must retain cleanup intent on inline S3 failure");
        assert!(
            !cleanup_failure.contains("delete_hnsw_s3_db_prefix_cleanup_intent"),
            "inline S3 cleanup failure must not delete the durable retry intent"
        );
    }

    #[test]
    fn create_database_stamps_tenant_before_commit_then_publishes_lifecycle() {
        let source = include_str!("database.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("database.rs must contain #[cfg(test)]");
        let create_fn = prod_source
            .split("pub(crate) async fn execute_create_database_cmd(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) async fn execute_drop_database_cmd(")
                    .next()
            })
            .expect("CREATE DATABASE executor must exist before DROP DATABASE executor");

        let allocate_pos = create_fn
            .find("allocate_tenant_incarnation")
            .expect("CREATE DATABASE must allocate a tenant incarnation for new DBs");
        let stamp_pos = create_fn
            .find("put_tenant_incarnation_stamp")
            .expect("CREATE DATABASE must write tenant-local incarnation stamp");
        let live_pos = create_fn
            .find("LifecycleTenantStatus::Live")
            .expect("CREATE DATABASE must publish lifecycle Live inventory");
        let commit_pos = create_fn
            .find("session.commit().await?")
            .expect("CREATE DATABASE must commit the tenant transaction");
        assert!(
            allocate_pos < stamp_pos && stamp_pos < commit_pos && commit_pos < live_pos,
            "CREATE DATABASE must stamp tenant identity before commit, then publish repairable lifecycle inventory after commit"
        );

        let registry_pos = create_fn
            .find("ensure_database_inventory_row_with_retry")
            .expect("CREATE DATABASE should still best-effort populate legacy worker inventory");
        assert!(
            commit_pos < registry_pos,
            "legacy worker inventory must be best-effort after tenant commit, not a foreground commit dependency"
        );

        let registry_block = create_fn
            .split("if let Err(e) = crate::worker::ensure_database_inventory_row_with_retry")
            .nth(1)
            .expect("CREATE DATABASE inventory registration block must exist");
        assert!(
            !registry_block.contains("session.rollback().await?"),
            "legacy worker inventory failure must not roll back an already-committed CREATE DATABASE"
        );
    }

    #[test]
    fn drop_database_deletes_stamp_before_commit_then_publishes_lifecycle_drop() {
        let source = include_str!("database.rs");
        let prod_source = source
            .split("#[cfg(test)]")
            .next()
            .expect("database.rs must contain #[cfg(test)]");
        let drop_fn = prod_source
            .split("pub(crate) async fn execute_drop_database_cmd(")
            .nth(1)
            .and_then(|rest| {
                rest.split("pub(crate) async fn execute_alter_database_cmd(")
                    .next()
            })
            .expect("DROP DATABASE executor must exist before ALTER DATABASE executor");

        let stamp_read_pos = drop_fn
            .find("get_tenant_incarnation_stamp_for_update")
            .expect("DROP DATABASE must lock tenant-local incarnation before deleting metadata");
        let allocate_pos = drop_fn
            .find("allocate_tenant_incarnation")
            .expect("DROP DATABASE must allocate an incarnation for unstamped pre-upgrade DBs");
        let stamp_put_pos = drop_fn
            .find("put_tenant_incarnation_stamp")
            .expect("DROP DATABASE must stamp unstamped pre-upgrade DBs before metadata deletion");
        let metadata_drop_pos = drop_fn
            .find("drop_database_metadata")
            .expect("DROP DATABASE must delete tenant metadata");
        let dropped_status_pos = drop_fn
            .find("LifecycleTenantStatus::Dropped")
            .expect("DROP DATABASE must publish lifecycle Dropped inventory");
        let commit_pos = drop_fn
            .find("session.commit().await")
            .expect("DROP DATABASE must commit tenant metadata deletion");
        assert!(
            stamp_read_pos < metadata_drop_pos
                && allocate_pos < metadata_drop_pos
                && stamp_put_pos < metadata_drop_pos
                && metadata_drop_pos < commit_pos
                && commit_pos < dropped_status_pos,
            "DROP DATABASE must establish tenant identity before deleting metadata, then publish repairable lifecycle Dropped after commit"
        );
    }

    #[test]
    fn test_parse_alter_database_rename() {
        let cmd = parse_alter_database_sql("ALTER DATABASE testdb RENAME TO newdb;").unwrap();
        assert_eq!(
            cmd,
            AlterDatabaseCommand::Rename {
                old_name: "testdb".to_string(),
                new_name: "newdb".to_string()
            }
        );
    }

    #[test]
    fn test_parse_alter_database_owner_to() {
        let cmd = parse_alter_database_sql("ALTER DATABASE testdb OWNER TO admin;").unwrap();
        assert_eq!(
            cmd,
            AlterDatabaseCommand::Owner {
                name: "testdb".to_string(),
                new_owner: "admin".to_string()
            }
        );
    }
}
