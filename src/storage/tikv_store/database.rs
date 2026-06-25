use super::*;
use crate::storage::backpressure::tikv_op;
use serde::{Deserialize, Serialize};

const DATABASE_LIFECYCLE_FORMAT_V1: u8 = 1;
const DATABASE_NODE_LEASE_FORMAT_V1: u8 = 1;
const DATABASE_DRAIN_STATE_FORMAT_V1: u8 = 1;
const DATABASE_DROP_CLAIM_FORMAT_V1: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DatabaseLifecycleState {
    Active,
    Fencing,
    Dropped,
    Purging,
    Purged,
}

impl DatabaseLifecycleState {
    fn is_active(self) -> bool {
        matches!(self, Self::Active)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DatabaseLifecycle {
    pub db_id: u64,
    pub epoch: u64,
    pub state: DatabaseLifecycleState,
    pub created_at_ms: i64,
    pub fence_ts_ms: Option<i64>,
    pub drop_started_at_ms: Option<i64>,
    pub drop_completed_at_ms: Option<i64>,
    pub purge_job_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DatabaseNodeLease {
    pub node_id: String,
    pub generation: u64,
    pub lease_until_ms: i64,
    pub published_at_ms: i64,
    pub accepts_sql: bool,
}

#[allow(dead_code)] // Wired by the Phase 1C drop coordinator.
impl DatabaseNodeLease {
    pub(crate) fn live_until_with_guard_ms(&self, guard_ms: i64) -> i64 {
        self.lease_until_ms.saturating_add(guard_ms.max(0))
    }

    pub(crate) fn self_fence_deadline_ms(&self, guard_ms: i64) -> i64 {
        self.lease_until_ms.saturating_sub(guard_ms.max(0))
    }

    pub(crate) fn is_live_at_ms(&self, now_ms: i64, guard_ms: i64) -> bool {
        now_ms < self.live_until_with_guard_ms(guard_ms)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DatabaseDrainState {
    pub keyspace: String,
    pub db_id: u64,
    pub epoch: u64,
    pub node_id: String,
    pub generation: u64,
    pub observed_at_ms: i64,
    pub active_old_epoch_ops: u64,
    pub drained: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DatabaseDropClaim {
    keyspace: String,
    db_id: u64,
    epoch: u64,
    node_id: String,
    generation: u64,
    claimed_at_ms: i64,
    lease_until_ms: i64,
}

#[allow(dead_code)] // Wired by the Phase 1C drop coordinator.
impl DatabaseDrainState {
    pub(crate) fn is_drained(&self) -> bool {
        self.drained && self.active_old_epoch_ops == 0
    }
}

pub(crate) struct DroppedDatabaseMetadata {
    pub db_id: u64,
    pub fencing_epoch: u64,
    pub dropping_guard: crate::sql::session::db_connections::DroppingGuard,
}

impl DatabaseLifecycle {
    fn active(db_id: u64, created_at_ms: i64) -> Self {
        Self {
            db_id,
            epoch: 1,
            state: DatabaseLifecycleState::Active,
            created_at_ms,
            fence_ts_ms: None,
            drop_started_at_ms: None,
            drop_completed_at_ms: None,
            purge_job_id: None,
        }
    }
}

fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn serialize_database_lifecycle(lifecycle: &DatabaseLifecycle) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(64);
    out.push(DATABASE_LIFECYCLE_FORMAT_V1);
    bincode::serialize_into(&mut out, lifecycle)
        .context("Failed to serialize database lifecycle")?;
    Ok(out)
}

fn deserialize_database_lifecycle(data: &[u8]) -> Result<DatabaseLifecycle> {
    match data.split_first() {
        Some((&DATABASE_LIFECYCLE_FORMAT_V1, rest)) => {
            bincode::deserialize(rest).context("Failed to deserialize database lifecycle")
        }
        Some((version, _)) => Err(anyhow!(
            "unknown database lifecycle format version {version}"
        )),
        None => Err(anyhow!("empty database lifecycle value")),
    }
}

fn serialize_database_node_lease(lease: &DatabaseNodeLease) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(96);
    out.push(DATABASE_NODE_LEASE_FORMAT_V1);
    bincode::serialize_into(&mut out, lease).context("Failed to serialize database node lease")?;
    Ok(out)
}

fn deserialize_database_node_lease(data: &[u8]) -> Result<DatabaseNodeLease> {
    match data.split_first() {
        Some((&DATABASE_NODE_LEASE_FORMAT_V1, rest)) => {
            bincode::deserialize(rest).context("Failed to deserialize database node lease")
        }
        Some((version, _)) => Err(anyhow!(
            "unknown database node lease format version {version}"
        )),
        None => Err(anyhow!("empty database node lease value")),
    }
}

fn serialize_database_drain_state(state: &DatabaseDrainState) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(96);
    out.push(DATABASE_DRAIN_STATE_FORMAT_V1);
    bincode::serialize_into(&mut out, state).context("Failed to serialize database drain state")?;
    Ok(out)
}

fn deserialize_database_drain_state(data: &[u8]) -> Result<DatabaseDrainState> {
    match data.split_first() {
        Some((&DATABASE_DRAIN_STATE_FORMAT_V1, rest)) => {
            bincode::deserialize(rest).context("Failed to deserialize database drain state")
        }
        Some((version, _)) => Err(anyhow!(
            "unknown database drain state format version {version}"
        )),
        None => Err(anyhow!("empty database drain state value")),
    }
}

fn serialize_database_drop_claim(claim: &DatabaseDropClaim) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.push(DATABASE_DROP_CLAIM_FORMAT_V1);
    bincode::serialize_into(&mut out, claim).context("Failed to serialize database drop claim")?;
    Ok(out)
}

fn deserialize_database_drop_claim(data: &[u8]) -> Result<DatabaseDropClaim> {
    match data.split_first() {
        Some((&DATABASE_DROP_CLAIM_FORMAT_V1, rest)) => {
            bincode::deserialize(rest).context("Failed to deserialize database drop claim")
        }
        Some((&version, _)) => Err(anyhow!(
            "unknown database drop claim format version {version}"
        )),
        None => Err(anyhow!("empty database drop claim value")),
    }
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

    async fn get_database_lifecycle(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Option<DatabaseLifecycle>> {
        let key = self.key(&encode_database_lifecycle_key(db_id));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(deserialize_database_lifecycle(&data)?)),
            None => Ok(None),
        }
    }

    async fn put_database_lifecycle(
        &self,
        txn: &mut Transaction,
        lifecycle: &DatabaseLifecycle,
    ) -> Result<()> {
        let key = self.key(&encode_database_lifecycle_key(lifecycle.db_id));
        let data = serialize_database_lifecycle(lifecycle)?;
        txn_put(txn, key, data).await
    }

    async fn get_database_lifecycle_for_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Option<DatabaseLifecycle>> {
        let key = self.key(&encode_database_lifecycle_key(db_id));
        match tikv_op!(txn.get_for_update(key).await)? {
            Some(data) => Ok(Some(deserialize_database_lifecycle(&data)?)),
            None => Ok(None),
        }
    }

    #[allow(dead_code)] // Wired by the Phase 1C drop coordinator.
    pub(crate) async fn publish_database_node_lease(
        &self,
        txn: &mut Transaction,
        lease: &DatabaseNodeLease,
    ) -> Result<()> {
        let key = self.key(&encode_database_node_lease_key(&lease.node_id));
        let data = serialize_database_node_lease(lease)?;
        txn_put(txn, key, data).await
    }

    #[allow(dead_code)] // Wired by the Phase 1C drop coordinator.
    pub(crate) async fn delete_database_node_lease(
        &self,
        txn: &mut Transaction,
        node_id: &str,
    ) -> Result<()> {
        let key = self.key(&encode_database_node_lease_key(node_id));
        txn_delete(txn, key).await
    }

    #[allow(dead_code)] // Wired by the Phase 1C drop coordinator.
    pub(crate) async fn list_database_node_leases(
        &self,
        txn: &mut Transaction,
    ) -> Result<Vec<DatabaseNodeLease>> {
        let prefix = encode_database_node_lease_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut leases = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if key.starts_with(&prefix) {
                leases.push(deserialize_database_node_lease(pair.value())?);
            }
        }
        Ok(leases)
    }

    #[allow(dead_code)] // Wired by the Phase 1C drop coordinator.
    pub(crate) async fn put_database_drain_state(
        &self,
        txn: &mut Transaction,
        state: &DatabaseDrainState,
    ) -> Result<()> {
        let key = self.key(&encode_database_drain_state_key(
            &state.keyspace,
            state.db_id,
            state.epoch,
            &state.node_id,
        ));
        let data = serialize_database_drain_state(state)?;
        txn_put(txn, key, data).await
    }

    #[allow(dead_code)] // Wired by the Phase 1C drop coordinator.
    pub(crate) async fn list_database_drain_states(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        epoch: u64,
    ) -> Result<Vec<DatabaseDrainState>> {
        let prefix = encode_database_drain_state_prefix(keyspace, db_id, epoch);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut states = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if key.starts_with(&prefix) {
                states.push(deserialize_database_drain_state(pair.value())?);
            }
        }
        Ok(states)
    }

    pub(crate) async fn claim_database_drop_with_lease(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        epoch: u64,
        node_id: &str,
        generation: u64,
        now_ms: i64,
        lease_ms: i64,
    ) -> Result<bool> {
        let key = self.key(&encode_database_drop_claim_key(keyspace, db_id, epoch));
        if let Some(data) = tikv_op!(txn.get_for_update(key.clone()).await)? {
            let existing = deserialize_database_drop_claim(&data)?;
            let same_owner = existing.node_id == node_id && existing.generation == generation;
            if !same_owner && existing.lease_until_ms > now_ms {
                return Ok(false);
            }
        }

        let claim = DatabaseDropClaim {
            keyspace: keyspace.to_string(),
            db_id,
            epoch,
            node_id: node_id.to_string(),
            generation,
            claimed_at_ms: now_ms,
            lease_until_ms: now_ms.saturating_add(lease_ms.max(1)),
        };
        let data = serialize_database_drop_claim(&claim)?;
        txn_put(txn, key, data).await?;
        Ok(true)
    }

    pub(crate) async fn release_database_drop_claim_if_owner(
        &self,
        txn: &mut Transaction,
        keyspace: &str,
        db_id: u64,
        epoch: u64,
        node_id: &str,
        generation: u64,
    ) -> Result<()> {
        let key = self.key(&encode_database_drop_claim_key(keyspace, db_id, epoch));
        let Some(data) = tikv_op!(txn.get_for_update(key.clone()).await)? else {
            return Ok(());
        };
        let existing = deserialize_database_drop_claim(&data)?;
        if existing.node_id == node_id && existing.generation == generation {
            txn_delete(txn, key).await?;
        }
        Ok(())
    }

    async fn database_active_in_txn(&self, txn: &mut Transaction, db_id: u64) -> Result<bool> {
        let Some(def) = self.get_database_by_id(txn, db_id).await? else {
            return Ok(false);
        };
        Ok(self
            .get_database_lifecycle(txn, db_id)
            .await?
            .unwrap_or_else(|| DatabaseLifecycle::active(db_id, def.created_at))
            .state
            .is_active())
    }

    pub async fn database_active(&self, db_id: u64) -> Result<bool> {
        #[cfg(test)]
        if self.client.is_none() {
            return Ok(true);
        }

        let mut txn = self.begin_optimistic().await?;
        let result = self.database_active_in_txn(&mut txn, db_id).await;
        txn.rollback().await.ok();
        result
    }

    pub(crate) async fn database_fencing_epoch(&self, db_id: u64) -> Result<Option<u64>> {
        let mut txn = self.begin_optimistic().await?;
        let result = self
            .get_database_lifecycle(&mut txn, db_id)
            .await?
            .filter(|lifecycle| lifecycle.state == DatabaseLifecycleState::Fencing)
            .map(|lifecycle| lifecycle.epoch);
        txn.rollback().await.ok();
        Ok(result)
    }

    /// Look up a database ID in its own short-lived transaction.
    pub async fn lookup_database_id(&self, db_name: &str) -> Result<Option<u64>> {
        let mut txn = self.begin_optimistic().await?;
        let result = async {
            let Some(db_id) = self.get_database_id(&mut txn, db_name).await? else {
                return Ok(None);
            };
            if self.database_active_in_txn(&mut txn, db_id).await? {
                Ok(Some(db_id))
            } else {
                Ok(None)
            }
        }
        .await;
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
                    self.put_database_lifecycle(
                        &mut txn,
                        &DatabaseLifecycle::active(db_id, def.created_at),
                    )
                    .await?;

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

    /// Lock the database metadata/lifecycle rows before committing background writes.
    ///
    /// DROP DATABASE first moves the lifecycle row out of ACTIVE, then removes
    /// the database metadata row before destroying the database key range.
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
    /// fencing/dropped database as `Ok(false)` instead of an error.
    ///
    /// Use this when a background re-enqueue must distinguish "the DB was
    /// dropped, so produce no further work" (return `false` -> caller suppresses
    /// the enqueue) from a genuine TiKV failure (return `Err` -> caller retries).
    /// It takes `get_for_update` on both the database row and lifecycle row, so
    /// the read conflicts with DROP DATABASE fencing just like the assert form.
    pub async fn database_alive_for_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<bool> {
        let key = self.key(&encode_database_id_key(db_id));
        let Some(data) = tikv_op!(txn.get_for_update(key).await)? else {
            return Ok(false);
        };
        let def: DatabaseDef =
            bincode::deserialize(&data).context("Failed to deserialize database definition")?;
        Ok(self
            .get_database_lifecycle_for_update(txn, db_id)
            .await?
            .unwrap_or_else(|| DatabaseLifecycle::active(db_id, def.created_at))
            .state
            .is_active())
    }

    async fn mark_database_fencing_for_update(
        &self,
        txn: &mut Transaction,
        def: &DatabaseDef,
    ) -> Result<DatabaseLifecycle> {
        let now = now_epoch_ms();
        let mut lifecycle = self
            .get_database_lifecycle_for_update(txn, def.id)
            .await?
            .unwrap_or_else(|| DatabaseLifecycle::active(def.id, def.created_at));
        if !lifecycle.state.is_active() {
            return Err(anyhow!(
                "database \"{}\" is already being dropped or has been dropped",
                def.name
            ));
        }
        lifecycle.epoch = lifecycle
            .epoch
            .checked_add(1)
            .ok_or_else(|| anyhow!("database lifecycle epoch overflow for db_id {}", def.id))?;
        lifecycle.state = DatabaseLifecycleState::Fencing;
        lifecycle.fence_ts_ms = Some(now);
        lifecycle.drop_started_at_ms = Some(now);
        self.put_database_lifecycle(txn, &lifecycle).await?;
        Ok(lifecycle)
    }

    pub(crate) async fn mark_database_dropped_if_fencing_epoch(
        &self,
        db_id: u64,
        fencing_epoch: u64,
    ) -> Result<bool> {
        let mut txn = self.begin().await?;
        let Some(mut lifecycle) = self
            .get_database_lifecycle_for_update(&mut txn, db_id)
            .await?
        else {
            txn.rollback().await.ok();
            return Ok(false);
        };
        if lifecycle.state != DatabaseLifecycleState::Fencing || lifecycle.epoch != fencing_epoch {
            txn.rollback().await.ok();
            return Ok(false);
        }
        lifecycle.state = DatabaseLifecycleState::Dropped;
        lifecycle.drop_completed_at_ms = Some(now_epoch_ms());
        self.put_database_lifecycle(&mut txn, &lifecycle).await?;
        tikv_op!(txn.commit().await)?;
        Ok(true)
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
            let active = self
                .get_database_lifecycle(txn, def.id)
                .await?
                .unwrap_or_else(|| DatabaseLifecycle::active(def.id, def.created_at))
                .state
                .is_active();
            if active {
                dbs.push(def);
            }
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
        let mut raw_count = 0usize;
        for pair in pairs {
            raw_count += 1;
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let def: DatabaseDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize database")?;
            let active = self
                .get_database_lifecycle(txn, def.id)
                .await?
                .unwrap_or_else(|| DatabaseLifecycle::active(def.id, def.created_at))
                .state
                .is_active();
            if active {
                dbs.push(def);
            }
            last_key = Some(key.to_vec());
        }

        let next_cursor = if raw_count == limit { last_key } else { None };
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
        self.put_database_lifecycle(txn, &DatabaseLifecycle::active(db_id, def.created_at))
            .await?;

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
    ) -> Result<Option<DroppedDatabaseMetadata>> {
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

        let def = self
            .get_database_by_id(txn, db_id)
            .await?
            .ok_or_else(|| anyhow!("database \"{}\" metadata is inconsistent", db_name))?;

        // PostgreSQL 55006: atomically check no other sessions are connected
        // AND mark the database as "dropping" to block new connections.
        // The Mutex in the registry ensures no window between check and mark.
        let dropping_guard = crate::sql::session::db_connections::db_connection_registry()
            .try_mark_dropping(self.keyspace().unwrap_or("default"), db_id)
            .map_err(|count| crate::sql::error::SqlError::ObjectInUse {
                message: format!(
                    "database \"{}\" is being accessed by {} other session(s) or operation(s)",
                    db_name, count
                ),
            })?;

        let lifecycle = self.mark_database_fencing_for_update(txn, &def).await?;

        txn_delete(txn, name_key).await?;

        let id_key = self.key(&encode_database_id_key(db_id));
        txn_delete(txn, id_key).await?;

        Ok(Some(DroppedDatabaseMetadata {
            db_id,
            fencing_epoch: lifecycle.epoch,
            dropping_guard,
        }))
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
        if !self.database_alive_for_update(txn, db_id).await? {
            return Err(anyhow!("database \"{}\" is being dropped", old_name));
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
        if !self.database_alive_for_update(txn, db_id).await? {
            return Err(anyhow!("database \"{}\" is being dropped", db_name));
        }

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

    #[test]
    fn database_lifecycle_codec_is_versioned() {
        let lifecycle = DatabaseLifecycle::active(42, 1234);
        let data = serialize_database_lifecycle(&lifecycle).expect("serialize lifecycle");
        assert_eq!(data.first().copied(), Some(DATABASE_LIFECYCLE_FORMAT_V1));
        let decoded = deserialize_database_lifecycle(&data).expect("decode lifecycle");
        assert_eq!(decoded, lifecycle);

        let mut unknown = data;
        unknown[0] = DATABASE_LIFECYCLE_FORMAT_V1 + 1;
        let err = deserialize_database_lifecycle(&unknown).expect_err("unknown version rejected");
        assert!(
            err.to_string()
                .contains("unknown database lifecycle format"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn database_node_lease_codec_and_guard_math_are_versioned() {
        let lease = DatabaseNodeLease {
            node_id: "node-a".to_string(),
            generation: 9,
            lease_until_ms: 10_000,
            published_at_ms: 9_000,
            accepts_sql: true,
        };
        let data = serialize_database_node_lease(&lease).expect("serialize node lease");
        assert_eq!(data.first().copied(), Some(DATABASE_NODE_LEASE_FORMAT_V1));
        let decoded = deserialize_database_node_lease(&data).expect("decode node lease");
        assert_eq!(decoded, lease);

        assert_eq!(lease.self_fence_deadline_ms(250), 9_750);
        assert_eq!(lease.live_until_with_guard_ms(250), 10_250);
        assert!(lease.is_live_at_ms(10_249, 250));
        assert!(!lease.is_live_at_ms(10_250, 250));

        let mut unknown = data;
        unknown[0] = DATABASE_NODE_LEASE_FORMAT_V1 + 1;
        let err = deserialize_database_node_lease(&unknown).expect_err("unknown version rejected");
        assert!(
            err.to_string()
                .contains("unknown database node lease format"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn database_drain_state_codec_and_completion_rule_are_versioned() {
        let mut state = DatabaseDrainState {
            keyspace: "tenant-a".to_string(),
            db_id: 42,
            epoch: 3,
            node_id: "node-a".to_string(),
            generation: 9,
            observed_at_ms: 10_100,
            active_old_epoch_ops: 1,
            drained: true,
        };
        assert!(
            !state.is_drained(),
            "active old-epoch work means the node is not drained yet"
        );

        state.active_old_epoch_ops = 0;
        assert!(state.is_drained());

        let data = serialize_database_drain_state(&state).expect("serialize drain state");
        assert_eq!(data.first().copied(), Some(DATABASE_DRAIN_STATE_FORMAT_V1));
        let decoded = deserialize_database_drain_state(&data).expect("decode drain state");
        assert_eq!(decoded, state);

        let mut unknown = data;
        unknown[0] = DATABASE_DRAIN_STATE_FORMAT_V1 + 1;
        let err = deserialize_database_drain_state(&unknown).expect_err("unknown version rejected");
        assert!(
            err.to_string()
                .contains("unknown database drain state format"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn database_drop_claim_codec_and_claim_rules_are_versioned() {
        let claim = DatabaseDropClaim {
            keyspace: "tenant-a".to_string(),
            db_id: 42,
            epoch: 3,
            node_id: "node-a".to_string(),
            generation: 9,
            claimed_at_ms: 10_000,
            lease_until_ms: 40_000,
        };
        let data = serialize_database_drop_claim(&claim).expect("serialize drop claim");
        assert_eq!(data.first().copied(), Some(DATABASE_DROP_CLAIM_FORMAT_V1));
        let decoded = deserialize_database_drop_claim(&data).expect("decode drop claim");
        assert_eq!(decoded, claim);

        let mut unknown = data;
        unknown[0] = DATABASE_DROP_CLAIM_FORMAT_V1 + 1;
        let err = deserialize_database_drop_claim(&unknown).expect_err("unknown version rejected");
        assert!(
            err.to_string().contains("unknown database drop claim"),
            "unexpected error: {err}"
        );
    }

    // ── Liveness fence behavior (#2: dropped-DB enqueue suppression) ─────────
    //
    // TiKV-backed; run with a reachable PD cluster (CI integration-tests job).
    // Drives all database liveness branches: a live DB row with ACTIVE or
    // legacy-missing lifecycle yields Ok(true); a FENCING lifecycle yields
    // Ok(false) even while the DB row still exists; after the metadata row is
    // deleted it also yields Ok(false) — NOT an Err — so background re-enqueue
    // call sites can suppress work for a dropped DB.

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

        // FENCING lifecycle while the DB metadata row still exists → Ok(false).
        {
            let mut txn = store.begin().await.expect("begin");
            let mut lifecycle = DatabaseLifecycle::active(db_id, 1234);
            lifecycle.state = DatabaseLifecycleState::Fencing;
            lifecycle.epoch = 2;
            lifecycle.fence_ts_ms = Some(1235);
            lifecycle.drop_started_at_ms = Some(1235);
            store
                .put_database_lifecycle(&mut txn, &lifecycle)
                .await
                .expect("put fencing lifecycle");
            txn.commit().await.expect("commit");
        }
        {
            let mut txn = store.begin().await.expect("begin");
            let alive = store
                .database_alive_for_update(&mut txn, db_id)
                .await
                .expect("fencing check must not error");
            assert!(!alive, "FENCING DB lifecycle must report alive == false");
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
