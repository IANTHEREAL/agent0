use super::*;
use crate::storage::backpressure::tikv_op;

impl TikvStore {
    pub async fn create_trigger(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        mut def: TriggerDef,
    ) -> Result<()> {
        if def.oid == 0 {
            def.oid = self.next_trigger_oid(txn, db_id).await?;
        }

        let key = self.key(&encode_trigger_key_v2(db_id, &def.table, def.name.as_str()));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            return Err(anyhow!(
                "Trigger '{}' already exists on '{}'",
                def.name,
                def.table
            ));
        }
        let data = bincode::serialize(&def).context("Failed to serialize trigger definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn list_triggers(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<TriggerDef>> {
        let prefix = encode_trigger_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut triggers = Vec::new();
        for pair in pairs {
            let mut def: TriggerDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize trigger")?;
            if def.oid == 0 {
                def.oid = self.next_trigger_oid(txn, db_id).await?;
                let data = bincode::serialize(&def).context("Failed to serialize trigger")?;
                txn_put(
                    txn,
                    self.key(&encode_trigger_key_v2(db_id, &def.table, def.name.as_str())),
                    data,
                )
                .await?;
            }
            triggers.push(def);
        }
        Ok(triggers)
    }

    pub async fn list_triggers_for_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
    ) -> Result<Vec<TriggerDef>> {
        let prefix = encode_trigger_table_prefix_v2(db_id, table_full_name);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut triggers = Vec::new();
        for pair in pairs {
            let def: TriggerDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize trigger")?;
            if def.table != table_full_name {
                continue;
            }
            triggers.push(def);
        }

        Ok(triggers)
    }

    pub async fn drop_trigger(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        trigger_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_trigger_key_v2(db_id, table_full_name, trigger_name));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            txn_delete(txn, key).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn plan_trigger_rename_ops(
        db_id: u64,
        old_table: &str,
        new_table: &str,
        triggers: Vec<(Vec<u8>, TriggerDef)>,
    ) -> Result<(Vec<(Vec<u8>, Vec<u8>)>, Vec<Vec<u8>>)> {
        let mut puts = Vec::new();
        let mut old_keys = Vec::new();
        let mut new_keys = HashSet::new();

        for (old_key, mut def) in triggers {
            if def.table != old_table {
                continue;
            }

            def.table = new_table.to_string();
            if let Some((schema, _)) = new_table.split_once('.') {
                def.schema = schema.to_string();
            }

            let new_key = encode_trigger_key_v2(db_id, new_table, def.name.as_str());
            let data = bincode::serialize(&def).context("Failed to serialize trigger")?;
            new_keys.insert(new_key.clone());
            puts.push((new_key, data));
            old_keys.push(old_key);
        }

        let deletes = old_keys
            .into_iter()
            .filter(|old_key| !new_keys.contains(old_key))
            .collect();

        Ok((puts, deletes))
    }

    pub(crate) fn validate_trigger_rename_puts(
        old_table: &str,
        new_table: &str,
        puts: &[(Vec<u8>, Vec<u8>)],
        old_keys: &HashSet<Vec<u8>>,
        existing_triggers: &HashMap<Vec<u8>, TriggerDef>,
    ) -> Result<()> {
        for (key, data) in puts {
            if old_keys.contains(key) {
                continue;
            }

            if let Some(existing) = existing_triggers.get(key) {
                let new_def: TriggerDef =
                    bincode::deserialize(data).context("Failed to deserialize trigger")?;
                return Err(anyhow!(
                    "Renaming table '{}' to '{}' would overwrite trigger '{}' on '{}' (key collision with trigger '{}' on '{}')",
                    old_table,
                    new_table,
                    existing.name,
                    existing.table,
                    new_def.name,
                    new_def.table
                ));
            }
        }
        Ok(())
    }

    pub(crate) async fn rename_table_triggers(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        old_table: &str,
        new_table: &str,
    ) -> Result<()> {
        // If orphan triggers exist under the target name (e.g. from a previous bug),
        // remove them so the renamed table doesn't "inherit" unrelated triggers.
        let new_prefix = encode_trigger_table_prefix_v2(db_id, new_table);
        let mut new_end = new_prefix.clone();
        new_end.push(0xFF);
        let range: BoundRange = (new_prefix..new_end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;
        let mut existing_triggers = HashMap::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            let def: TriggerDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize trigger")?;
            if def.table == new_table {
                txn_delete(txn, self.key(key)).await?;
                continue;
            }
            existing_triggers.insert(self.key(key), def);
        }

        let old_prefix = encode_trigger_table_prefix_v2(db_id, old_table);
        let mut old_end = old_prefix.clone();
        old_end.push(0xFF);
        let range: BoundRange = (old_prefix.clone()..old_end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut triggers = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&old_prefix) {
                continue;
            }
            let def: TriggerDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize trigger")?;
            if def.table != old_table {
                continue;
            }
            triggers.push((self.key(key), def));
        }

        let old_keys: HashSet<Vec<u8>> = triggers.iter().map(|(key, _)| key.clone()).collect();
        let (puts, deletes) = Self::plan_trigger_rename_ops(db_id, old_table, new_table, triggers)?;

        Self::validate_trigger_rename_puts(
            old_table,
            new_table,
            &puts,
            &old_keys,
            &existing_triggers,
        )?;

        for (key, data) in puts {
            txn_put(txn, key, data).await?;
        }

        for key in deletes {
            txn_delete(txn, key).await?;
        }
        Ok(())
    }

    pub(crate) async fn rename_table_comments(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        old_table: &str,
        new_table: &str,
    ) -> Result<()> {
        // Clean up any orphan comments under the target name.
        txn_delete(
            txn,
            self.key(&encode_comment_table_key_v2(db_id, new_table)),
        )
        .await?;
        self.delete_column_comments_for_table(txn, db_id, new_table)
            .await?;

        // Move table comment, if present.
        let old_table_key = self.key(&encode_comment_table_key_v2(db_id, old_table));
        if let Some(value) = tikv_op!(txn.get(old_table_key.clone()).await)? {
            let new_table_key = self.key(&encode_comment_table_key_v2(db_id, new_table));
            txn_put(txn, new_table_key, value).await?;
            txn_delete(txn, old_table_key).await?;
        }

        // Move column comments, if present.
        let old_prefix = column_comment_table_prefix_v2(db_id, old_table);
        let mut end = old_prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (old_prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&old_prefix) {
                continue;
            }
            let column_name = std::str::from_utf8(&key[old_prefix.len()..])
                .context("comment key: invalid column name")?;
            let new_key = self.key(&encode_comment_column_key_v2(db_id, new_table, column_name));
            txn_put(txn, new_key, pair.value().to_vec()).await?;
            txn_delete(txn, self.key(key)).await?;
        }
        Ok(())
    }

    pub(crate) async fn rename_column_comment(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        old_column: &str,
        new_column: &str,
    ) -> Result<()> {
        let old_key = self.key(&encode_comment_column_key_v2(
            db_id,
            table_full_name,
            old_column,
        ));
        let Some(value) = tikv_op!(txn.get(old_key.clone()).await)? else {
            return Ok(());
        };
        let new_key = self.key(&encode_comment_column_key_v2(
            db_id,
            table_full_name,
            new_column,
        ));
        txn_put(txn, new_key, value).await?;
        txn_delete(txn, old_key).await?;
        Ok(())
    }

    pub(crate) async fn delete_column_comments_for_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
    ) -> Result<()> {
        let prefix = column_comment_table_prefix_v2(db_id, table_full_name);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;
        for pair in pairs {
            txn_delete(txn, pair.into_key().into()).await?;
        }
        Ok(())
    }

    pub(crate) async fn rewrite_sequences_owned_by_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        old_table: &str,
        new_table: &str,
    ) -> Result<()> {
        let sequences = self.list_sequences(txn, db_id).await?;
        for mut def in sequences {
            let mut changed = false;
            if let Some((owned_table, _)) = def.owned_by.as_mut() {
                if owned_table == old_table {
                    *owned_table = new_table.to_string();
                    changed = true;
                }
            }
            if changed {
                self.update_sequence_def(txn, db_id, &def).await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn rewrite_sequences_owned_by_column(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        old_column: &str,
        new_column: &str,
    ) -> Result<()> {
        let sequences = self.list_sequences(txn, db_id).await?;
        for mut def in sequences {
            let mut changed = false;
            if let Some((owned_table, owned_column)) = def.owned_by.as_mut() {
                if owned_table == table_full_name && owned_column == old_column {
                    *owned_column = new_column.to_string();
                    changed = true;
                }
            }
            if changed {
                self.update_sequence_def(txn, db_id, &def).await?;
            }
        }
        Ok(())
    }
}
