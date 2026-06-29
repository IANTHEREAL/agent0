use super::*;
use crate::storage::backpressure::tikv_op;

const TENANT_INCARNATION_STAMP_VALUE_VERSION: u8 = 1;
const TENANT_INCARNATION_STAMP_VALUE_LEN: usize = 9;

fn encode_tenant_incarnation_stamp_value(incarnation: u64) -> Vec<u8> {
    let mut value = Vec::with_capacity(TENANT_INCARNATION_STAMP_VALUE_LEN);
    value.push(TENANT_INCARNATION_STAMP_VALUE_VERSION);
    value.extend_from_slice(&incarnation.to_be_bytes());
    value
}

fn decode_tenant_incarnation_stamp_value(value: &[u8]) -> Result<u64> {
    if value.len() != TENANT_INCARNATION_STAMP_VALUE_LEN {
        return Err(anyhow!(
            "invalid tenant incarnation stamp length: {}",
            value.len()
        ));
    }
    if value[0] != TENANT_INCARNATION_STAMP_VALUE_VERSION {
        return Err(anyhow!(
            "unsupported tenant incarnation stamp version: {}",
            value[0]
        ));
    }
    Ok(u64::from_be_bytes(value[1..9].try_into()?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DefaultDatabaseRepairAction {
    UseVisible(u64),
    RestoreNameMapping(u64),
    BootstrapNew,
}

fn visible_default_database_id(
    mapped_id: Option<u64>,
    mapped_def: Option<&DatabaseDef>,
    matching_ids: &[u64],
) -> Result<Option<u64>> {
    const DEFAULT_DB: &str = "postgres";

    match (mapped_id, mapped_def) {
        (Some(id), Some(def)) if def.name == DEFAULT_DB => match matching_ids {
            [only_id] if *only_id == id => Ok(Some(id)),
            [] => Err(anyhow!(
                "default database '{}' visible mapping points at ID {} but no exact-name definitions were found during validation",
                DEFAULT_DB,
                id
            )),
            [only_id] => Err(anyhow!(
                "default database '{}' visible mapping points at ID {} but exact-name definition resolves to ID {}",
                DEFAULT_DB,
                id,
                only_id
            )),
            _ => Err(anyhow!(
                "default database '{}' is ambiguous: multiple database definitions exist {:?}",
                DEFAULT_DB,
                matching_ids
            )),
        },
        _ => Ok(None),
    }
}

fn choose_default_database_repair_action(
    mapped_id: Option<u64>,
    mapped_def: Option<&DatabaseDef>,
    databases: &[DatabaseDef],
) -> Result<DefaultDatabaseRepairAction> {
    const DEFAULT_DB: &str = "postgres";

    let matching_ids: Vec<u64> = databases
        .iter()
        .filter(|db| db.name == DEFAULT_DB)
        .map(|db| db.id)
        .collect();

    match matching_ids.len() {
        0 => Ok(DefaultDatabaseRepairAction::BootstrapNew),
        1 => {
            let only_id = matching_ids[0];
            if let (Some(id), Some(def)) = (mapped_id, mapped_def) {
                if id == only_id && def.name == DEFAULT_DB {
                    return Ok(DefaultDatabaseRepairAction::UseVisible(id));
                }
            }
            Ok(DefaultDatabaseRepairAction::RestoreNameMapping(only_id))
        }
        _ => Err(anyhow!(
            "default database '{}' is ambiguous: multiple database definitions exist {:?}",
            DEFAULT_DB,
            matching_ids
        )),
    }
}

fn tikv_error_contains_write_conflict(err: &tikv_client::Error) -> bool {
    match err {
        tikv_client::Error::KeyError(key_error) => key_error.conflict.is_some(),
        tikv_client::Error::PessimisticLockError { inner, .. } => {
            tikv_error_contains_write_conflict(inner)
        }
        tikv_client::Error::UndeterminedError(inner) => tikv_error_contains_write_conflict(inner),
        tikv_client::Error::ExtractedErrors(errors)
        | tikv_client::Error::MultipleKeyErrors(errors) => {
            errors.iter().any(tikv_error_contains_write_conflict)
        }
        _ => false,
    }
}

impl TikvStore {
    async fn list_exact_default_database_definition_ids(
        &self,
        txn: &mut Transaction,
    ) -> Result<Vec<u64>> {
        const DEFAULT_DB: &str = "postgres";
        let prefix = encode_database_id_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut matching_ids = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }

            let Some(db_id) = key
                .strip_prefix(prefix.as_slice())
                .and_then(|suffix| <[u8; 8]>::try_from(suffix).ok())
                .map(u64::from_be_bytes)
            else {
                tracing::warn!(
                    "skipping malformed database definition key while validating default database visibility: {:?}",
                    key
                );
                continue;
            };

            match bincode::deserialize::<DatabaseDef>(pair.value()) {
                Ok(def) if def.name == DEFAULT_DB => {
                    matching_ids.push(def.id);
                    if matching_ids.len() > 1 {
                        break;
                    }
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(
                    "skipping malformed database definition id {} while validating default database visibility: {}",
                    db_id,
                    err
                ),
            }
        }
        Ok(matching_ids)
    }

    /// Allocate and persist the next database ID (storage format v2).
    pub async fn next_database_id(&self, txn: &mut Transaction) -> Result<u64> {
        const FIRST_DATABASE_ID: u64 = 1;

        let key = self.key(&encode_next_database_id_key());
        let current = tikv_op!(txn.get(key.clone()).await)?;
        let next_val = match current {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                let id = u64::from_be_bytes(bytes);
                id.checked_add(1)
                    .ok_or_else(|| anyhow!("Database ID overflow"))?
            }
            None => FIRST_DATABASE_ID,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    /// Look up a database ID by name (storage format v2).
    pub async fn get_database_id(
        &self,
        txn: &mut Transaction,
        db_name: &str,
    ) -> Result<Option<u64>> {
        let key = self.key(&encode_database_name_key(db_name));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                Ok(Some(u64::from_be_bytes(bytes)))
            }
            None => Ok(None),
        }
    }

    /// Look up a database ID in its own short-lived transaction.
    pub async fn lookup_database_id(&self, db_name: &str) -> Result<Option<u64>> {
        let mut txn = self.begin_optimistic().await?;
        let result = self.get_database_id(&mut txn, db_name).await;
        if let Err(err) = txn.rollback().await {
            tracing::warn!(
                "rollback failed after database lookup for '{}': {}",
                db_name,
                err
            );
        }
        result
    }

    /// Ensure the default `postgres` database is both bootstrapped and visible.
    pub async fn ensure_default_database_visible(&self, owner: &str) -> Result<u64> {
        const DEFAULT_DB: &str = "postgres";
        let mut allow_conflict_retry = true;

        loop {
            let mut txn = self.begin().await?;
            let mapped_id = self.get_database_id(&mut txn, DEFAULT_DB).await?;
            let mapped_def = match mapped_id {
                Some(id) => self.get_database_by_id(&mut txn, id).await?,
                None => None,
            };

            if matches!((mapped_id, mapped_def.as_ref()), (Some(_), Some(def)) if def.name == DEFAULT_DB)
            {
                let matching_ids = self
                    .list_exact_default_database_definition_ids(&mut txn)
                    .await?;
                if let Some(id) =
                    visible_default_database_id(mapped_id, mapped_def.as_ref(), &matching_ids)?
                {
                    txn.rollback().await.ok();
                    return Ok(id);
                }
            }

            if let Some(id) = mapped_id {
                match mapped_def.as_ref() {
                    Some(def) if def.name != DEFAULT_DB => tracing::warn!(
                        "default database '{}' name mapping points at non-default database '{}' (id={})",
                        DEFAULT_DB,
                        def.name,
                        id
                    ),
                    None => tracing::warn!(
                        "default database '{}' name mapping points at missing database id {}",
                        DEFAULT_DB,
                        id
                    ),
                    _ => {}
                }
            }
            let databases = self.list_databases(&mut txn).await?;
            let repair_action =
                choose_default_database_repair_action(mapped_id, mapped_def.as_ref(), &databases)?;

            match repair_action {
                DefaultDatabaseRepairAction::UseVisible(id) => {
                    txn.rollback().await.ok();
                    return Ok(id);
                }
                DefaultDatabaseRepairAction::RestoreNameMapping(id) => {
                    let name_key = self.key(&encode_database_name_key(DEFAULT_DB));
                    txn_put(&mut txn, name_key, id.to_be_bytes().to_vec()).await?;
                    match tikv_op!(txn.commit().await) {
                        Ok(_) => {
                            info!(
                                "Restored default database '{}' name mapping to existing ID {}",
                                DEFAULT_DB, id
                            );
                            return Ok(id);
                        }
                        Err(err)
                            if allow_conflict_retry && tikv_error_contains_write_conflict(&err) =>
                        {
                            allow_conflict_retry = false;
                            if let Some(visible_id) =
                                self.read_visible_default_database_id().await?
                            {
                                info!(
                                    "Recovered default database '{}' after commit conflict with visible ID {}",
                                    DEFAULT_DB, visible_id
                                );
                                return Ok(visible_id);
                            }
                            tracing::info!(
                                "Retrying default database '{}' repair after commit conflict",
                                DEFAULT_DB
                            );
                        }
                        Err(err) => return Err(anyhow!(err)),
                    }
                }
                DefaultDatabaseRepairAction::BootstrapNew => {
                    let db_id = self.next_database_id(&mut txn).await?;
                    let def = DatabaseDef::default_postgres(db_id, owner.to_string());

                    let name_key = self.key(&encode_database_name_key(DEFAULT_DB));
                    txn_put(&mut txn, name_key, db_id.to_be_bytes().to_vec()).await?;

                    let id_key = self.key(&encode_database_id_key(db_id));
                    let data = bincode::serialize(&def)
                        .context("Failed to serialize database definition")?;
                    txn_put(&mut txn, id_key, data).await?;

                    match tikv_op!(txn.commit().await) {
                        Ok(_) => {
                            info!(
                                "Bootstrapped default database '{}' with ID {}",
                                DEFAULT_DB, db_id
                            );
                            return Ok(db_id);
                        }
                        Err(err)
                            if allow_conflict_retry && tikv_error_contains_write_conflict(&err) =>
                        {
                            allow_conflict_retry = false;
                            if let Some(visible_id) =
                                self.read_visible_default_database_id().await?
                            {
                                info!(
                                    "Recovered default database '{}' after bootstrap conflict with visible ID {}",
                                    DEFAULT_DB, visible_id
                                );
                                return Ok(visible_id);
                            }
                            tracing::info!(
                                "Retrying default database '{}' bootstrap after commit conflict",
                                DEFAULT_DB
                            );
                        }
                        Err(err) => return Err(anyhow!(err)),
                    }
                }
            }
        }
    }

    async fn read_visible_default_database_id(&self) -> Result<Option<u64>> {
        const DEFAULT_DB: &str = "postgres";
        let mut txn = self.begin_optimistic().await?;
        let result = async {
            let mapped_id = self.get_database_id(&mut txn, DEFAULT_DB).await?;
            let mapped_def = match mapped_id {
                Some(id) => self.get_database_by_id(&mut txn, id).await?,
                None => None,
            };
            let matching_ids = self
                .list_exact_default_database_definition_ids(&mut txn)
                .await?;
            visible_default_database_id(mapped_id, mapped_def.as_ref(), &matching_ids)
        }
        .await;
        txn.rollback().await.ok();
        result
    }

    /// Fetch a database definition by ID (storage format v2).
    pub async fn get_database_by_id(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Option<DatabaseDef>> {
        let key = self.key(&encode_database_id_key(db_id));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize database definition")?,
            )),
            None => Ok(None),
        }
    }

    /// Lock the database metadata row before committing background writes.
    ///
    /// DROP DATABASE removes this row before destroying the database key range.
    /// A worker that writes tenant data after resolving the DB earlier in its
    /// execution must take this lock in the same transaction it is about to
    /// commit; otherwise it can recreate orphan keys after range destruction.
    pub async fn assert_database_alive_for_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<()> {
        if self.database_alive_for_update(txn, db_id).await? {
            Ok(())
        } else {
            Err(anyhow!(
                "database with id {} no longer exists; aborting background tenant write",
                db_id
            ))
        }
    }

    /// Same liveness fence as `assert_database_alive_for_update`, but reports a
    /// dropped database as `Ok(false)` instead of an error.
    ///
    /// Use this when a background re-enqueue must distinguish "the DB was
    /// dropped, so produce no further work" (return `false` → caller suppresses
    /// the enqueue) from a genuine TiKV failure (return `Err` → caller retries).
    /// It still takes `get_for_update` so the read conflicts with DROP DATABASE,
    /// fencing the enqueue against a concurrent drop just like the assert form.
    pub async fn database_alive_for_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<bool> {
        let key = self.key(&encode_database_id_key(db_id));
        let Some(data) = tikv_op!(txn.get_for_update(key).await)? else {
            return Ok(false);
        };
        let _: DatabaseDef =
            bincode::deserialize(&data).context("Failed to deserialize database definition")?;
        Ok(true)
    }

    /// Lock and rewrite the live database metadata row.
    ///
    /// Dropping-intent repair uses this before clearing a stale live-DB intent:
    /// the rewrite creates a tenant-store version that makes any older in-flight
    /// DROP transaction conflict instead of committing after the system-store
    /// fence has been removed.
    pub async fn touch_database_alive_for_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<bool> {
        let key = self.key(&encode_database_id_key(db_id));
        let Some(data) = tikv_op!(txn.get_for_update(key.clone()).await)? else {
            return Ok(false);
        };
        let _: DatabaseDef =
            bincode::deserialize(&data).context("Failed to deserialize database definition")?;
        txn_put(txn, key, data).await?;
        Ok(true)
    }

    /// Write this database's tenant-local lifecycle incarnation stamp.
    ///
    /// The Core lifecycle allocator is outside the tenant keyspace, but worker
    /// effects must validate ownership inside the same tenant transaction as
    /// the effect. This stamp is that same-keyspace fence.
    pub async fn put_tenant_incarnation_stamp(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        incarnation: u64,
    ) -> Result<()> {
        let key = self.key(&encode_tenant_incarnation_key(db_id));
        txn_put(txn, key, encode_tenant_incarnation_stamp_value(incarnation)).await?;
        Ok(())
    }

    /// Read the tenant-local incarnation stamp for this database.
    #[allow(dead_code)]
    pub async fn get_tenant_incarnation_stamp(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Option<u64>> {
        let key = self.key(&encode_tenant_incarnation_key(db_id));
        match tikv_op!(txn.get(key).await)? {
            Some(value) => Ok(Some(decode_tenant_incarnation_stamp_value(&value)?)),
            None => Ok(None),
        }
    }

    pub async fn get_tenant_incarnation_stamp_for_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Option<u64>> {
        let key = self.key(&encode_tenant_incarnation_key(db_id));
        match tikv_op!(txn.get_for_update(key).await)? {
            Some(value) => Ok(Some(decode_tenant_incarnation_stamp_value(&value)?)),
            None => Ok(None),
        }
    }

    pub async fn delete_tenant_incarnation_stamp(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_tenant_incarnation_key(db_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    /// List all databases in the current keyspace (storage format v2).
    pub async fn list_databases(&self, txn: &mut Transaction) -> Result<Vec<DatabaseDef>> {
        let prefix = encode_database_id_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut dbs = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let def: DatabaseDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize database")?;
            dbs.push(def);
        }
        Ok(dbs)
    }

    /// List one raw-key cursor page of databases in the current keyspace.
    pub async fn scan_databases_page(
        &self,
        txn: &mut Transaction,
        start_after: Option<&[u8]>,
        limit: usize,
    ) -> Result<(Vec<DatabaseDef>, Option<Vec<u8>>)> {
        if limit == 0 {
            return Ok((Vec::new(), None));
        }

        let prefix = encode_database_id_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let start = match start_after {
            Some(last_key) => {
                let mut next_start = last_key.to_vec();
                next_start.push(0x00);
                next_start
            }
            None => prefix.clone(),
        };
        let range: BoundRange = (start..end).into();
        let pairs = tikv_op!(txn.scan(range, scan_limit_to_u32(Some(limit))).await)?;

        let mut dbs = Vec::new();
        let mut last_key = None;
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let def: DatabaseDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize database")?;
            dbs.push(def);
            last_key = Some(key.to_vec());
        }

        let next_cursor = if dbs.len() == limit { last_key } else { None };
        Ok((dbs, next_cursor))
    }

    /// Create a new database (storage format v2).
    ///
    /// Returns `Ok(None)` if the database exists and `if_not_exists` is true.
    pub async fn create_database(
        &self,
        txn: &mut Transaction,
        name: &str,
        owner: &str,
        if_not_exists: bool,
    ) -> Result<Option<DatabaseDef>> {
        let name_key = self.key(&encode_database_name_key(name));
        if tikv_op!(txn.get(name_key.clone()).await)?.is_some() {
            if if_not_exists {
                return Ok(None);
            }
            return Err(anyhow!("database \"{}\" already exists", name));
        }

        let db_id = self.next_database_id(txn).await?;
        let def = DatabaseDef::new(db_id, name.to_string(), owner.to_string());

        txn_put(txn, name_key, db_id.to_be_bytes().to_vec()).await?;

        let id_key = self.key(&encode_database_id_key(db_id));
        let data = bincode::serialize(&def).context("Failed to serialize database definition")?;
        txn_put(txn, id_key, data).await?;

        info!("Created database '{}' with ID {}", name, db_id);
        Ok(Some(def))
    }

    /// Drop database metadata (storage format v2).
    ///
    /// Returns `Ok(None)` if the database does not exist and `if_exists` is true.
    ///
    /// Note: this does NOT delete the database's data range. Call
    /// `unsafe_destroy_database_data(db_id)` after committing the transaction.
    pub async fn drop_database_metadata(
        &self,
        txn: &mut Transaction,
        db_name: &str,
        if_exists: bool,
        current_database_id: u64,
    ) -> Result<Option<(u64, crate::sql::session::db_connections::DroppingGuard)>> {
        if db_name.eq_ignore_ascii_case("postgres")
            || db_name.eq_ignore_ascii_case("template0")
            || db_name.eq_ignore_ascii_case("template1")
        {
            return Err(anyhow!(
                "cannot drop database \"{}\": it is a system database",
                db_name
            ));
        }

        let name_key = self.key(&encode_database_name_key(db_name));
        let db_id = match tikv_op!(txn.get(name_key.clone()).await)? {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                u64::from_be_bytes(bytes)
            }
            None => {
                if if_exists {
                    return Ok(None);
                }
                return Err(anyhow!("database \"{}\" does not exist", db_name));
            }
        };

        if db_id == current_database_id {
            return Err(anyhow!("cannot drop the currently open database"));
        }

        // PostgreSQL 55006: atomically check no other sessions are connected
        // AND mark the database as "dropping" to block new connections.
        // The Mutex in the registry ensures no window between check and mark.
        let dropping_guard = crate::sql::session::db_connections::db_connection_registry()
            .try_mark_dropping(self.keyspace().unwrap_or("default"), db_id)
            .map_err(|count| crate::sql::error::SqlError::ObjectInUse {
                message: format!(
                    "database \"{}\" is being accessed by {} other user(s)",
                    db_name, count
                ),
            })?;

        txn_delete(txn, name_key).await?;

        let id_key = self.key(&encode_database_id_key(db_id));
        txn_delete(txn, id_key).await?;
        self.delete_tenant_incarnation_stamp(txn, db_id).await?;

        Ok(Some((db_id, dropping_guard)))
    }

    /// Rename a database (storage format v2).
    pub async fn rename_database(
        &self,
        txn: &mut Transaction,
        old_name: &str,
        new_name: &str,
        current_database_id: u64,
    ) -> Result<()> {
        if old_name.eq_ignore_ascii_case("postgres")
            || old_name.eq_ignore_ascii_case("template0")
            || old_name.eq_ignore_ascii_case("template1")
        {
            return Err(anyhow!("cannot rename database \"{}\"", old_name));
        }
        if new_name.eq_ignore_ascii_case("postgres")
            || new_name.eq_ignore_ascii_case("template0")
            || new_name.eq_ignore_ascii_case("template1")
        {
            return Err(anyhow!("cannot rename database to \"{}\"", new_name));
        }

        let old_key = self.key(&encode_database_name_key(old_name));
        let db_id = match tikv_op!(txn.get(old_key.clone()).await)? {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                u64::from_be_bytes(bytes)
            }
            None => return Err(anyhow!("database \"{}\" does not exist", old_name)),
        };

        if db_id == current_database_id {
            return Err(anyhow!("cannot rename the currently open database"));
        }

        let new_key = self.key(&encode_database_name_key(new_name));
        if tikv_op!(txn.get(new_key.clone()).await)?.is_some() {
            return Err(anyhow!("database \"{}\" already exists", new_name));
        }

        txn_delete(txn, old_key).await?;
        txn_put(txn, new_key, db_id.to_be_bytes().to_vec()).await?;

        let id_key = self.key(&encode_database_id_key(db_id));
        let mut def = self
            .get_database_by_id(txn, db_id)
            .await?
            .ok_or_else(|| anyhow!("database metadata corrupted"))?;
        def.name = new_name.to_string();
        let data = bincode::serialize(&def).context("Failed to serialize database definition")?;
        txn_put(txn, id_key, data).await?;

        Ok(())
    }

    /// Update the owner for a database (storage format v2).
    pub async fn set_database_owner(
        &self,
        txn: &mut Transaction,
        db_name: &str,
        new_owner: &str,
    ) -> Result<()> {
        let name_key = self.key(&encode_database_name_key(db_name));
        let db_id = match tikv_op!(txn.get(name_key).await)? {
            Some(data) => {
                let bytes: [u8; 8] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid database ID format"))?;
                u64::from_be_bytes(bytes)
            }
            None => return Err(anyhow!("database \"{}\" does not exist", db_name)),
        };

        let id_key = self.key(&encode_database_id_key(db_id));
        let mut def = self
            .get_database_by_id(txn, db_id)
            .await?
            .ok_or_else(|| anyhow!("database metadata corrupted"))?;
        def.owner = new_owner.to_string();
        let data = bincode::serialize(&def).context("Failed to serialize database definition")?;
        txn_put(txn, id_key, data).await?;
        Ok(())
    }

    /// Efficiently delete all data within a database using TiKV's `unsafe_destroy_range`.
    ///
    /// This is a non-transactional, best-effort cleanup step intended for DROP DATABASE/TABLE.
    pub async fn unsafe_destroy_database_data(&self, db_id: u64) -> Result<()> {
        let (start, end) = encode_database_data_range(db_id);
        let range: BoundRange = (start..end).into();
        self.client()
            .unsafe_destroy_range(range)
            .await
            .map_err(|e| anyhow!(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn postgres_def(id: u64) -> DatabaseDef {
        DatabaseDef::default_postgres(id, "admin".to_string())
    }

    fn named_db(id: u64, name: &str) -> DatabaseDef {
        DatabaseDef::new(id, name.to_string(), "admin".to_string())
    }

    #[test]
    fn tenant_incarnation_stamp_value_is_versioned() {
        let encoded = encode_tenant_incarnation_stamp_value(123);
        assert_eq!(encoded.len(), TENANT_INCARNATION_STAMP_VALUE_LEN);
        assert_eq!(
            decode_tenant_incarnation_stamp_value(&encoded).unwrap(),
            123
        );

        let mut bad_version = encoded.clone();
        bad_version[0] = 2;
        assert!(decode_tenant_incarnation_stamp_value(&bad_version)
            .unwrap_err()
            .to_string()
            .contains("unsupported tenant incarnation stamp version"));

        assert!(
            decode_tenant_incarnation_stamp_value(&encoded[..encoded.len() - 1])
                .unwrap_err()
                .to_string()
                .contains("invalid tenant incarnation stamp length")
        );
    }

    #[test]
    fn default_database_repair_uses_visible_mapping_when_target_exists() {
        let mapped = postgres_def(1);
        let action = choose_default_database_repair_action(
            Some(1),
            Some(&mapped),
            std::slice::from_ref(&mapped),
        )
        .unwrap();
        assert_eq!(action, DefaultDatabaseRepairAction::UseVisible(1));
    }

    #[test]
    fn visible_default_database_id_returns_healthy_default_mapping() {
        let mapped = postgres_def(1);
        assert_eq!(
            visible_default_database_id(Some(1), Some(&mapped), &[1]).unwrap(),
            Some(1)
        );
    }

    #[test]
    fn visible_default_database_id_rejects_non_default_mapping() {
        let app = named_db(7, "appdb");
        assert_eq!(
            visible_default_database_id(Some(7), Some(&app), &[7]).unwrap(),
            None
        );
    }

    #[test]
    fn visible_default_database_id_rejects_missing_definition() {
        assert_eq!(
            visible_default_database_id(Some(7), None, &[]).unwrap(),
            None
        );
    }

    #[test]
    fn visible_default_database_id_fails_closed_when_visible_mapping_has_ghost_duplicate() {
        let mapped = postgres_def(1);
        let err = visible_default_database_id(Some(1), Some(&mapped), &[1, 42]).unwrap_err();
        assert!(
            err.to_string().contains("ambiguous"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn default_database_repair_fails_closed_when_visible_mapping_has_ghost_duplicate() {
        let mapped = postgres_def(1);
        let err = choose_default_database_repair_action(
            Some(1),
            Some(&mapped),
            &[mapped.clone(), postgres_def(42)],
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("ambiguous"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn default_database_repair_restores_unique_existing_postgres_definition() {
        let other = named_db(7, "appdb");
        let ghost = postgres_def(42);
        let action = choose_default_database_repair_action(None, None, &[other, ghost]).unwrap();
        assert_eq!(action, DefaultDatabaseRepairAction::RestoreNameMapping(42));
    }

    #[test]
    fn default_database_repair_restores_when_mapping_points_to_missing_definition() {
        let ghost = postgres_def(42);
        let action =
            choose_default_database_repair_action(Some(5), None, std::slice::from_ref(&ghost))
                .unwrap();
        assert_eq!(action, DefaultDatabaseRepairAction::RestoreNameMapping(42));
    }

    #[test]
    fn default_database_repair_bootstraps_when_no_postgres_definition_exists() {
        let action =
            choose_default_database_repair_action(None, None, &[named_db(7, "appdb")]).unwrap();
        assert_eq!(action, DefaultDatabaseRepairAction::BootstrapNew);
    }

    #[test]
    fn default_database_repair_bootstraps_when_only_mixed_case_postgres_exists() {
        let mixed_case = named_db(42, "Postgres");
        let action = choose_default_database_repair_action(
            Some(42),
            Some(&mixed_case),
            std::slice::from_ref(&mixed_case),
        )
        .unwrap();
        assert_eq!(action, DefaultDatabaseRepairAction::BootstrapNew);
    }

    #[test]
    fn default_database_repair_fails_closed_on_ambiguous_postgres_definitions() {
        let err =
            choose_default_database_repair_action(None, None, &[postgres_def(1), postgres_def(2)])
                .unwrap_err();
        assert!(
            err.to_string().contains("ambiguous"),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn default_database_repair_ignores_distinct_mixed_case_postgres_name() {
        let mapped = postgres_def(1);
        let action = choose_default_database_repair_action(
            Some(1),
            Some(&mapped),
            &[mapped.clone(), named_db(42, "Postgres")],
        )
        .unwrap();
        assert_eq!(action, DefaultDatabaseRepairAction::UseVisible(1));
    }

    #[test]
    fn default_database_commit_conflict_detector_matches_key_error() {
        let err = tikv_client::Error::KeyError(Box::new(tikv_client::proto::kvrpcpb::KeyError {
            conflict: Some(tikv_client::proto::kvrpcpb::WriteConflict::default()),
            ..Default::default()
        }));
        assert!(tikv_error_contains_write_conflict(&err));
    }

    #[test]
    fn default_database_commit_conflict_detector_ignores_non_conflict_errors() {
        let err = tikv_client::Error::StringError("boom".to_string());
        assert!(!tikv_error_contains_write_conflict(&err));
    }

    // ── Liveness fence behavior (#2: dropped-DB enqueue suppression) ─────────
    //
    // TiKV-backed; run with a reachable PD cluster (CI integration-tests job).
    // Drives BOTH branches of `database_alive_for_update`: a live DB row yields
    // Ok(true); after the metadata row is deleted (exactly what DROP DATABASE
    // does before destroying the data range) it yields Ok(false) — NOT an Err —
    // so background re-enqueue call sites can suppress work for a dropped DB.

    async fn liveness_test_store(tag: &str) -> TikvStore {
        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let keyspace = format!(
            "_db_alive_{tag}_test_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        // `new_system` connects with `with_keyspace`, which requires the keyspace
        // to already exist in PD (the vendored client does NOT auto-create it).
        // Pre-create it with the canonical PD-API helper so this promoted CI test
        // does not fail at connect with "keyspace does not exist".
        crate::worker::ensure_system_keyspace(&pd_endpoints, &keyspace)
            .await
            .expect("pre-create liveness test keyspace in PD");
        // Raw system store: no bootstrap, so the only DB row present is the one
        // this test writes — keeping the assertions deterministic.
        TikvStore::new_system(pd_endpoints, &keyspace)
            .await
            .expect("init raw system store")
    }

    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn database_alive_for_update_reports_live_then_dropped() {
        let store = liveness_test_store("alive").await;
        let db_id = 7_u64;
        let id_key = store.key(&encode_database_id_key(db_id));

        // Write a live DB metadata row directly (mirrors create_database's id_key
        // write) so this test exercises the storage primitive in isolation.
        {
            let mut txn = store.begin().await.expect("begin");
            let def = DatabaseDef::new(db_id, "appdb".to_string(), "admin".to_string());
            let data = bincode::serialize(&def).expect("serialize def");
            txn_put(&mut txn, id_key.clone(), data).await.expect("put");
            txn.commit().await.expect("commit");
        }

        // Live DB → Ok(true).
        {
            let mut txn = store.begin().await.expect("begin");
            let alive = store
                .database_alive_for_update(&mut txn, db_id)
                .await
                .expect("alive check must not error for a live DB");
            assert!(alive, "live DB metadata row must report alive == true");
            txn.rollback().await.ok();
        }

        // Delete the metadata row exactly as DROP DATABASE does before destroying
        // the data range.
        {
            let mut txn = store.begin().await.expect("begin");
            txn_delete(&mut txn, id_key.clone()).await.expect("delete");
            txn.commit().await.expect("commit");
        }

        // Dropped DB → Ok(false), NOT Err. This is the branch the enqueue-
        // suppression fix depends on: a missing row is a definitive "dropped"
        // answer, not a transient failure to retry.
        {
            let mut txn = store.begin().await.expect("begin");
            let alive = store
                .database_alive_for_update(&mut txn, db_id)
                .await
                .expect("dropped DB must return Ok(false), not Err");
            assert!(!alive, "dropped DB metadata row must report alive == false");
            txn.rollback().await.ok();
        }

        // And the assert form must turn the same dropped state into an error so
        // single-store fences (finalize/load_next) abort their commit.
        {
            let mut txn = store.begin().await.expect("begin");
            let err = store
                .assert_database_alive_for_update(&mut txn, db_id)
                .await
                .expect_err("assert form must error for a dropped DB");
            assert!(
                err.to_string().contains("no longer exists"),
                "unexpected error: {err}"
            );
            txn.rollback().await.ok();
        }
    }
}
