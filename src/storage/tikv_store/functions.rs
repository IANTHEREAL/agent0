use super::*;

impl TikvStore {
    pub async fn create_function(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        mut def: FunctionDef,
    ) -> Result<()> {
        if def.oid == 0 {
            def.oid = self.next_function_oid(txn, db_id).await?;
        }

        let full_name = format!("{}.{}", def.schema, def.name);
        let key = self.key(&encode_function_key_v2(db_id, &full_name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Function '{}' already exists", full_name));
        }
        let data = serialize_function_def(&def)?;
        txn_put(txn, key, data).await?;
        self.invalidate_function_cache(db_id, &full_name).await;
        Ok(())
    }

    pub async fn replace_function(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        mut def: FunctionDef,
    ) -> Result<()> {
        let full_name = format!("{}.{}", def.schema, def.name);
        let key = self.key(&encode_function_key_v2(db_id, &full_name));

        if def.oid == 0 {
            if let Some(existing) = txn.get(key.clone()).await? {
                let existing: FunctionDef = deserialize_function_def(&existing)?;
                if existing.oid != 0 {
                    def.oid = existing.oid;
                }
            }
        }
        if def.oid == 0 {
            def.oid = self.next_function_oid(txn, db_id).await?;
        }

        let data = serialize_function_def(&def)?;
        txn_put(txn, key, data).await?;
        self.invalidate_function_cache(db_id, &full_name).await;
        Ok(())
    }

    pub async fn get_function(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
    ) -> Result<Option<FunctionDef>> {
        // 1. Check cache
        {
            let cache = self.cache.read().await;
            if let Some(db_cache) = cache.per_db.get(&db_id) {
                if let Some((cached_at, func_opt)) = db_cache.functions.get(full_name) {
                    if cached_at.elapsed() < SCHEMA_CACHE_TTL {
                        return Ok(func_opt.clone());
                    }
                }
            }
        }

        // 2. Cache miss - TiKV lookup
        let key = self.key(&encode_function_key_v2(db_id, full_name));
        let result = match txn.get(key).await? {
            Some(data) => {
                let mut def: FunctionDef = deserialize_function_def(&data)?;
                if def.oid == 0 {
                    def.oid = self.next_function_oid(txn, db_id).await?;
                    let data = serialize_function_def(&def)?;
                    txn_put(
                        txn,
                        self.key(&encode_function_key_v2(db_id, full_name)),
                        data,
                    )
                    .await?;
                }
                Some(def)
            }
            None => None,
        };

        // 3. Populate cache
        {
            let mut cache = self.cache.write().await;
            cache
                .per_db
                .entry(db_id)
                .or_insert_with(PerDatabaseSchemaCache::new)
                .functions
                .insert(full_name.to_string(), (Instant::now(), result.clone()));
        }

        Ok(result)
    }

    pub async fn list_functions(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<FunctionDef>> {
        let prefix = encode_function_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut funcs = Vec::new();
        for pair in pairs {
            let mut def: FunctionDef = deserialize_function_def(pair.value())?;
            let mut needs_update = false;
            if def.oid == 0 {
                def.oid = self.next_function_oid(txn, db_id).await?;
                needs_update = true;
            }
            if needs_update {
                let data = serialize_function_def(&def)?;
                let full_name = format!("{}.{}", def.schema, def.name);
                txn_put(
                    txn,
                    self.key(&encode_function_key_v2(db_id, &full_name)),
                    data,
                )
                .await?;
            }
            funcs.push(def);
        }
        Ok(funcs)
    }

    pub async fn drop_function(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
        cascade: bool,
    ) -> Result<bool> {
        let key = self.key(&encode_function_key_v2(db_id, full_name));
        if txn.get(key.clone()).await?.is_some() {
            let dependent_triggers: Vec<TriggerDef> = self
                .list_triggers(txn, db_id)
                .await?
                .into_iter()
                .filter(|t| t.function == full_name)
                .collect();

            if !dependent_triggers.is_empty() && !cascade {
                let func_name = full_name.rsplit('.').next().unwrap_or(full_name);
                let func_sig = format!("{}()", func_name);
                let trigger = &dependent_triggers[0];
                let table_name = trigger.table.rsplit('.').next().unwrap_or(&trigger.table);
                return Err(anyhow!(
                    "cannot drop function {} because other objects depend on it\nDETAIL:  trigger {} on table {} depends on function {}\nHINT:  Use DROP ... CASCADE to drop the dependent objects too.",
                    func_sig,
                    trigger.name,
                    table_name,
                    func_sig
                ));
            }

            if cascade {
                let mut affected_tables = HashSet::new();
                for trigger in &dependent_triggers {
                    affected_tables.insert(trigger.table.as_str());
                    let _ = self
                        .drop_trigger(txn, db_id, &trigger.table, &trigger.name)
                        .await?;
                }
                for table in affected_tables {
                    self.invalidate_trigger_cache(db_id, table).await;
                }
            }

            txn_delete(txn, key).await?;
            let comment_key = self.key(&encode_comment_function_key_v2(db_id, full_name));
            txn_delete(txn, comment_key).await?;
            self.invalidate_function_cache(db_id, full_name).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
