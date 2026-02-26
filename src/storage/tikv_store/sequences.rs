use super::*;
use crate::sql::error::SqlError;

pub(super) fn nextval_standalone(
    full_name: &str,
    increment: i64,
    min_value: i64,
    max_value: i64,
    is_cycled: bool,
    state: &mut SequenceState,
) -> Result<i64> {
    if increment == 0 {
        return Err(anyhow!("Sequence '{}' has invalid INCREMENT 0", full_name));
    }

    if !state.is_called {
        state.is_called = true;
        return Ok(state.last_value);
    }

    let candidate = state
        .last_value
        .checked_add(increment)
        .ok_or_else(|| anyhow!("Sequence '{}' overflow", full_name))?;

    let wrapped = if candidate > max_value {
        if is_cycled {
            min_value
        } else {
            return Err(SqlError::SequenceLimitExceeded {
                message: format!(
                    "nextval: reached maximum value of sequence \"{}\" ({})",
                    full_name, max_value
                ),
            }
            .into());
        }
    } else if candidate < min_value {
        if is_cycled {
            max_value
        } else {
            return Err(SqlError::SequenceLimitExceeded {
                message: format!(
                    "nextval: reached minimum value of sequence \"{}\" ({})",
                    full_name, min_value
                ),
            }
            .into());
        }
    } else {
        candidate
    };

    state.last_value = wrapped;
    Ok(wrapped)
}

pub(super) fn setval_standalone(
    full_name: &str,
    min_value: i64,
    max_value: i64,
    state: &mut SequenceState,
    value: i64,
    is_called: bool,
) -> Result<i64> {
    if value < min_value || value > max_value {
        return Err(SqlError::NumericValueOutOfRange {
            message: format!(
                "setval: value {} is out of bounds for sequence \"{}\"",
                value, full_name
            ),
        }
        .into());
    }

    state.last_value = value;
    state.is_called = is_called;
    Ok(value)
}

impl TikvStore {
    pub async fn next_sequence_oid(&self, _txn: &mut Transaction, db_id: u64) -> Result<u32> {
        const FIRST_SEQUENCE_OID: u32 = 1;

        let key = self.key(&encode_next_sequence_oid_key_v2(db_id));
        self.autocommit_update_key(key, |current| {
            let next_val = match current {
                Some(data) => {
                    let oid = u32::from_be_bytes(
                        data.try_into()
                            .map_err(|_| anyhow!("Invalid sequence OID format"))?,
                    );
                    oid.checked_add(1)
                        .ok_or_else(|| anyhow!("Sequence OID overflow"))?
                }
                None => FIRST_SEQUENCE_OID,
            };
            Ok((Some(next_val.to_be_bytes().to_vec()), next_val))
        })
        .await
    }

    pub async fn next_sequence_value(
        &self,
        _txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
    ) -> Result<i32> {
        let key = self.key(&encode_table_sequence_value_key_v2(db_id, table_id));
        let val = self
            .autocommit_update_key(key, |current| {
                let current_val = match current {
                    Some(data) => u64::from_be_bytes(
                        data.try_into().map_err(|_| anyhow!("Invalid ID format"))?,
                    ),
                    None => 0,
                };
                let next_val = current_val
                    .checked_add(1)
                    .ok_or_else(|| anyhow!("Sequence overflow"))?;
                Ok((Some(next_val.to_be_bytes().to_vec()), next_val))
            })
            .await?;
        Ok(val as i32)
    }

    pub async fn set_sequence_value(
        &self,
        _txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        value: u64,
    ) -> Result<()> {
        let key = self.key(&encode_table_sequence_value_key_v2(db_id, table_id));
        self.autocommit_update_key(key, |_current| Ok((Some(value.to_be_bytes().to_vec()), ())))
            .await
    }

    pub async fn create_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        mut def: SequenceDef,
    ) -> Result<()> {
        if def.oid == 0 {
            def.oid = self.next_sequence_oid(txn, db_id).await?;
        }

        let full_name = def.full_name();
        let key = self.key(&encode_sequence_def_key_v2(db_id, &full_name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(SqlError::DuplicateRelation(full_name.to_string()).into());
        }
        let data = bincode::serialize(&def).context("Failed to serialize sequence definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Persist an updated sequence definition (metadata only; does not touch sequence state).
    pub async fn update_sequence_def(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        def: &SequenceDef,
    ) -> Result<()> {
        let key = self.key(&encode_sequence_def_key_v2(db_id, &def.full_name()));
        let data = bincode::serialize(def).context("Failed to serialize sequence definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Apply legacy-OID migration and zero-value defaults to a sequence definition in place.
    ///
    /// Steps:
    /// 1. If `def.oid == 0`, attempt an autocommit OID backfill.  On success the backfilled
    ///    definition is written to storage by `autocommit_backfill_sequence_oid`; `start_value`
    ///    and `cache_size` defaults are applied to the in-memory copy and we return early.
    /// 2. If the backfill returned `None` (race lost to another writer) or `def.oid` was already
    ///    non-zero, we fall through to the caller's transaction: allocate an OID if still missing,
    ///    fix the two defaults, and write the updated definition back within `txn`.
    async fn ensure_sequence_defaults(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        def: &mut SequenceDef,
    ) -> Result<()> {
        if def.oid == 0 {
            if let Some(backfilled) = self
                .autocommit_backfill_sequence_oid(db_id, &def.full_name())
                .await?
            {
                *def = backfilled;
                if def.start_value == 0 {
                    def.start_value = def.min_value;
                }
                if def.cache_size == 0 {
                    def.cache_size = 1;
                }
                return Ok(());
            }
        }

        let mut needs_update = false;
        if def.oid == 0 {
            def.oid = self.next_sequence_oid(txn, db_id).await?;
            needs_update = true;
        }
        if def.start_value == 0 {
            def.start_value = def.min_value;
            needs_update = true;
        }
        if def.cache_size == 0 {
            def.cache_size = 1;
            needs_update = true;
        }
        if needs_update {
            let data =
                bincode::serialize(def).context("Failed to serialize sequence definition")?;
            txn_put(
                txn,
                self.key(&encode_sequence_def_key_v2(db_id, &def.full_name())),
                data,
            )
            .await?;
        }
        Ok(())
    }

    pub async fn get_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
    ) -> Result<Option<SequenceDef>> {
        let key = self.key(&encode_sequence_def_key_v2(db_id, full_name));
        match txn.get(key).await? {
            Some(data) => {
                let mut def: SequenceDef = bincode::deserialize(&data)
                    .context("Failed to deserialize sequence definition")?;
                self.ensure_sequence_defaults(txn, db_id, &mut def).await?;
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    /// Ensure a legacy sequence definition has a stable non-zero OID.
    ///
    /// This backfill is performed in its own auto-committed transaction so it survives caller
    /// transaction rollbacks and SAVEPOINT rollbacks. This is required because standalone sequence
    /// state is stored separately under a key derived from the OID.
    async fn autocommit_backfill_sequence_oid(
        &self,
        db_id: u64,
        full_name: &str,
    ) -> Result<Option<SequenceDef>> {
        const FIRST_SEQUENCE_OID: u32 = 1;

        let def_key = self.key(&encode_sequence_def_key_v2(db_id, full_name));
        let oid_key = self.key(&encode_next_sequence_oid_key_v2(db_id));

        for attempt in 0..AUTOCOMMIT_MAX_RETRIES {
            let mut txn = self.begin_optimistic().await?;
            let Some(data) = txn.get(def_key.clone()).await? else {
                let _ = txn.rollback().await;
                return Ok(None);
            };

            let mut def: SequenceDef =
                bincode::deserialize(&data).context("Failed to deserialize sequence definition")?;

            if def.oid != 0 {
                let _ = txn.rollback().await;
                return Ok(Some(def));
            }

            let current = txn.get(oid_key.clone()).await?;
            let next_val = match current {
                Some(data) => {
                    let oid = u32::from_be_bytes(
                        data.try_into()
                            .map_err(|_| anyhow!("Invalid sequence OID format"))?,
                    );
                    oid.checked_add(1)
                        .ok_or_else(|| anyhow!("Sequence OID overflow"))?
                }
                None => FIRST_SEQUENCE_OID,
            };
            txn.put(oid_key.clone(), next_val.to_be_bytes().to_vec())
                .await
                .map_err(|e| anyhow!(e))?;

            def.oid = next_val;
            let data =
                bincode::serialize(&def).context("Failed to serialize sequence definition")?;
            txn.put(def_key.clone(), data)
                .await
                .map_err(|e| anyhow!(e))?;

            match txn.commit().await {
                Ok(_) => return Ok(Some(def)),
                Err(e) => {
                    let _ = txn.rollback().await;
                    debug!(
                        "autocommit sequence OID backfill failed (attempt {} of {}): {}",
                        attempt + 1,
                        AUTOCOMMIT_MAX_RETRIES,
                        e
                    );
                }
            }
        }

        Err(anyhow!(
            "autocommit sequence OID backfill failed after {} attempts",
            AUTOCOMMIT_MAX_RETRIES
        ))
    }

    pub async fn list_sequences(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<SequenceDef>> {
        let prefix = encode_sequence_def_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut sequences = Vec::new();
        for pair in pairs {
            let mut def: SequenceDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize sequence")?;
            self.ensure_sequence_defaults(txn, db_id, &mut def).await?;
            sequences.push(def);
        }
        Ok(sequences)
    }

    pub async fn drop_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_sequence_def_key_v2(db_id, full_name));
        let Some(data) = txn.get(key.clone()).await? else {
            return Ok(false);
        };

        let def: SequenceDef =
            bincode::deserialize(&data).context("Failed to deserialize sequence definition")?;
        txn_delete(txn, key).await?;

        // Standalone sequences persist state under `sys_seq_{oid}`. Drop that state along with the
        // definition so that a later recreate (with a new OID) doesn't accumulate orphan keys.
        if matches!(def.backing, SequenceBacking::Standalone(_)) && def.oid != 0 {
            let state_key = self.key(&encode_sequence_value_key_v2(db_id, def.oid));
            if txn.get(state_key.clone()).await?.is_some() {
                txn_delete(txn, state_key).await?;
            }
        }

        Ok(true)
    }

    async fn maybe_migrate_implicit_sequence_to_standalone(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        def: &mut SequenceDef,
    ) -> Result<()> {
        let SequenceBacking::TableId(table_id) = def.backing.clone() else {
            return Ok(());
        };

        // Only implicit sequences created from SERIAL/IDENTITY are expected to use TableId backing.
        // Migrate these to standalone state so each sequence advances independently.
        if def.owned_by.is_none() {
            return Ok(());
        }

        // Mirror the current per-table allocator value, if any, so we don't re-issue values
        // that may have been consumed under the old shared-counter behavior.
        let key = self.key(&encode_table_sequence_value_key_v2(db_id, table_id));
        let current = {
            let mut read_txn = self.begin_optimistic().await?;
            let current = read_txn.get(key).await?;
            let _ = read_txn.rollback().await;
            current
        };
        let current_u64 = match current {
            Some(data) => u64::from_be_bytes(
                data.try_into()
                    .map_err(|_| anyhow!("Invalid sequence value format"))?,
            ),
            None => 0,
        };

        if let Some((owned_table, owned_col)) = def.owned_by.as_ref() {
            if let Some(schema) = self.get_schema(txn, db_id, owned_table).await? {
                if let Some(col) = schema.columns.iter().find(|c| c.name == *owned_col) {
                    def.max_value = match col.data_type {
                        DataType::Int64 => i64::MAX,
                        _ => i32::MAX as i64,
                    };
                }
            }
        }

        let last_value = if current_u64 == 0 {
            def.start_value
        } else {
            current_u64.try_into().map_err(|_| {
                anyhow!(
                    "Sequence value {} is too large for i64 (table_id={})",
                    current_u64,
                    table_id
                )
            })?
        };
        let is_called = current_u64 != 0;

        def.backing = SequenceBacking::Standalone(SequenceState {
            last_value,
            is_called,
        });
        Ok(())
    }

    pub async fn nextval_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
    ) -> Result<i64> {
        let mut def = self
            .get_sequence(txn, db_id, full_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(full_name.to_string()))?;

        self.maybe_migrate_implicit_sequence_to_standalone(txn, db_id, &mut def)
            .await?;

        match &def.backing {
            SequenceBacking::TableId(table_id) => {
                Ok(self.next_sequence_value(txn, db_id, *table_id).await? as i64)
            }
            SequenceBacking::Standalone(embedded_state) => {
                let state_key = self.key(&encode_sequence_value_key_v2(db_id, def.oid));
                let embedded_state = embedded_state.clone();
                let full_name = full_name.to_string();
                let increment = def.increment;
                let min_value = def.min_value;
                let max_value = def.max_value;
                let is_cycled = def.is_cycled;

                self.autocommit_update_key(state_key, |current| {
                    let mut state: SequenceState = match current {
                        Some(data) => bincode::deserialize(&data)
                            .context("Failed to deserialize sequence state")?,
                        None => embedded_state.clone(),
                    };

                    let next = nextval_standalone(
                        &full_name, increment, min_value, max_value, is_cycled, &mut state,
                    )?;

                    let data =
                        bincode::serialize(&state).context("Failed to serialize sequence state")?;
                    Ok((Some(data), next))
                })
                .await
            }
        }
    }

    pub async fn setval_sequence(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
        value: i64,
        is_called: bool,
    ) -> Result<i64> {
        let mut def = self
            .get_sequence(txn, db_id, full_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(full_name.to_string()))?;

        self.maybe_migrate_implicit_sequence_to_standalone(txn, db_id, &mut def)
            .await?;

        match &def.backing {
            SequenceBacking::TableId(table_id) => {
                if value < 1 {
                    return Err(SqlError::NumericValueOutOfRange {
                        message: format!(
                            "setval: value {} is out of bounds for sequence \"{}\"",
                            value, full_name
                        ),
                    }
                    .into());
                }
                let value_u64: u64 =
                    value
                        .try_into()
                        .map_err(|_| SqlError::NumericValueOutOfRange {
                            message: format!(
                                "setval: value {} is too large for sequence \"{}\"",
                                value, full_name
                            ),
                        })?;
                let stored = if is_called {
                    value_u64
                } else {
                    value_u64
                        .checked_sub(1)
                        .ok_or_else(|| SqlError::NumericValueOutOfRange {
                            message: format!(
                                "setval: value {} is out of bounds for sequence \"{}\"",
                                value, full_name
                            ),
                        })?
                };
                self.set_sequence_value(txn, db_id, *table_id, stored)
                    .await?;
                Ok(value)
            }
            SequenceBacking::Standalone(embedded_state) => {
                let state_key = self.key(&encode_sequence_value_key_v2(db_id, def.oid));
                let embedded_state = embedded_state.clone();
                let full_name = full_name.to_string();
                let min_value = def.min_value;
                let max_value = def.max_value;
                let value_copy = value;
                let is_called_copy = is_called;

                self.autocommit_update_key(state_key, |current| {
                    let mut state: SequenceState = match current {
                        Some(data) => bincode::deserialize(&data)
                            .context("Failed to deserialize sequence state")?,
                        None => embedded_state.clone(),
                    };

                    setval_standalone(
                        &full_name,
                        min_value,
                        max_value,
                        &mut state,
                        value_copy,
                        is_called_copy,
                    )?;

                    let data =
                        bincode::serialize(&state).context("Failed to serialize sequence state")?;
                    Ok((Some(data), value_copy))
                })
                .await
            }
        }
    }
}
