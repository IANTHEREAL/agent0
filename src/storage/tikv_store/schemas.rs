use super::*;

impl TikvStore {
    pub async fn list_procedures(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<String>> {
        let prefix = encode_procedure_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut procedures = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if key.starts_with(&prefix) {
                let name = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
                procedures.push(name);
            }
        }
        Ok(procedures)
    }

    pub async fn schema_exists(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: &str,
    ) -> Result<bool> {
        if Self::is_builtin_schema(schema) {
            return Ok(true);
        }
        let key = self.key(&encode_schema_def_key_v2(db_id, schema));
        Ok(txn.get(key).await?.is_some())
    }

    fn is_builtin_schema(schema: &str) -> bool {
        matches!(
            schema,
            "public" | "pg_catalog" | "information_schema" | "extensions"
        )
    }

    pub async fn create_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: &str,
        if_not_exists: bool,
    ) -> Result<bool> {
        if schema.is_empty() {
            return Err(anyhow!("schema name must not be empty"));
        }
        if schema.contains('.') {
            return Err(anyhow!("schema name '{}' must not contain '.'", schema));
        }

        if Self::is_builtin_schema(schema) {
            if if_not_exists {
                return Ok(false);
            }
            return Err(anyhow!("Schema '{}' already exists", schema));
        }

        let key = self.key(&encode_schema_def_key_v2(db_id, schema));
        if txn.get(key.clone()).await?.is_some() {
            if if_not_exists {
                return Ok(false);
            }
            return Err(anyhow!("Schema '{}' already exists", schema));
        }
        let oid = self.next_schema_oid(txn, db_id).await?;
        txn_put(txn, key, oid.to_be_bytes().to_vec()).await?;
        Ok(true)
    }

    pub async fn list_schemas(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<String>> {
        let prefix = encode_schema_def_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut schemas = vec![
            "public".to_string(),
            "information_schema".to_string(),
            "pg_catalog".to_string(),
            "extensions".to_string(),
        ];
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if key.starts_with(&prefix) {
                let name = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
                if !name.is_empty() && !Self::is_builtin_schema(&name) {
                    schemas.push(name);
                }
            }
        }
        schemas.sort();
        schemas.dedup();

        Ok(schemas)
    }

    pub async fn list_schema_oids(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<HashMap<String, u32>> {
        let prefix = encode_schema_def_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut oids: HashMap<String, u32> = HashMap::new();
        oids.insert("public".to_string(), 2200);
        oids.insert("information_schema".to_string(), 13222);
        oids.insert("pg_catalog".to_string(), 11);
        oids.insert("extensions".to_string(), 2201);

        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let schema = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
            if schema.is_empty() || Self::is_builtin_schema(&schema) {
                continue;
            }

            let oid = match pair.value().len() {
                0 => {
                    let new_oid = self.next_schema_oid(txn, db_id).await?;
                    let schema_key = self.key(&encode_schema_def_key_v2(db_id, &schema));
                    txn_put(txn, schema_key, new_oid.to_be_bytes().to_vec()).await?;
                    new_oid
                }
                4 => {
                    let bytes: [u8; 4] = pair
                        .value()
                        .as_slice()
                        .try_into()
                        .map_err(|_| anyhow!("Invalid schema OID format"))?;
                    u32::from_be_bytes(bytes)
                }
                _ => return Err(anyhow!("Invalid schema OID format")),
            };

            oids.insert(schema, oid);
        }

        Ok(oids)
    }

    pub async fn drop_schema_restrict(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: &str,
        if_exists: bool,
    ) -> Result<bool> {
        if schema.is_empty() {
            return Err(anyhow!("schema name must not be empty"));
        }
        if schema.contains('.') {
            return Err(anyhow!("schema name '{}' must not contain '.'", schema));
        }
        if Self::is_builtin_schema(schema) {
            return Err(anyhow!("cannot drop schema '{}'", schema));
        }

        let key = self.key(&encode_schema_def_key_v2(db_id, schema));
        if txn.get(key.clone()).await?.is_none() {
            if if_exists {
                return Ok(false);
            }
            return Err(anyhow!("Schema '{}' does not exist", schema));
        }

        let mut table_prefix = encode_schema_prefix_v2(db_id);
        table_prefix.extend_from_slice(schema.as_bytes());
        table_prefix.push(b'.');
        if self.prefix_has_any(txn, table_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut view_prefix = encode_view_prefix_v2(db_id);
        view_prefix.extend_from_slice(schema.as_bytes());
        view_prefix.push(b'.');
        if self.prefix_has_any(txn, view_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut matview_prefix = encode_matview_prefix_v2(db_id);
        matview_prefix.extend_from_slice(schema.as_bytes());
        matview_prefix.push(b'.');
        if self.prefix_has_any(txn, matview_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut procedure_prefix = encode_procedure_prefix_v2(db_id);
        procedure_prefix.extend_from_slice(schema.as_bytes());
        procedure_prefix.push(b'.');
        if self.prefix_has_any(txn, procedure_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut function_prefix = encode_function_prefix_v2(db_id);
        function_prefix.extend_from_slice(schema.as_bytes());
        function_prefix.push(b'.');
        if self.prefix_has_any(txn, function_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut trigger_prefix = encode_trigger_prefix_v2(db_id);
        trigger_prefix.extend_from_slice(schema.as_bytes());
        trigger_prefix.push(b'.');
        if self.prefix_has_any(txn, trigger_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut type_prefix = encode_type_prefix_v2(db_id);
        type_prefix.extend_from_slice(schema.as_bytes());
        type_prefix.push(b'.');
        if self.prefix_has_any(txn, type_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut sequence_prefix = encode_sequence_def_prefix_v2(db_id);
        sequence_prefix.extend_from_slice(schema.as_bytes());
        sequence_prefix.push(b'.');
        if self.prefix_has_any(txn, sequence_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        txn_delete(txn, key).await?;
        Ok(true)
    }

    pub async fn drop_schema_cascade(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: &str,
        if_exists: bool,
    ) -> Result<bool> {
        if schema.is_empty() {
            return Err(anyhow!("schema name must not be empty"));
        }
        if schema.contains('.') {
            return Err(anyhow!("schema name '{}' must not contain '.'", schema));
        }
        if Self::is_builtin_schema(schema) {
            return Err(anyhow!("cannot drop schema '{}'", schema));
        }

        let key = self.key(&encode_schema_def_key_v2(db_id, schema));
        if txn.get(key.clone()).await?.is_none() {
            if if_exists {
                return Ok(false);
            }
            return Err(anyhow!("Schema '{}' does not exist", schema));
        }

        let schema_prefix = format!("{}.", schema);

        for view in self.list_views(txn, db_id).await? {
            if view.schema == schema {
                let _ = self.drop_view(txn, db_id, &view.full_name()).await?;
            }
        }

        for matview in self.list_materialized_views(txn, db_id).await? {
            if matview.schema == schema {
                let _ = self
                    .drop_materialized_view(txn, db_id, &matview.full_name())
                    .await?;
            }
        }

        let sequences = self.list_sequences(txn, db_id).await?;
        for def in &sequences {
            if def.schema == schema {
                let _ = self.drop_sequence(txn, db_id, &def.full_name()).await?;
            }
        }

        let mut owned_sequences: HashMap<String, Vec<String>> = HashMap::new();
        for def in sequences {
            let Some((owned_table, _)) = &def.owned_by else {
                continue;
            };
            owned_sequences
                .entry(owned_table.clone())
                .or_default()
                .push(def.full_name());
        }

        for table in self.list_tables(txn, db_id).await? {
            if !table.starts_with(&schema_prefix) {
                continue;
            }

            for trigger in self.list_triggers_for_table(txn, db_id, &table).await? {
                let _ = self
                    .drop_trigger(txn, db_id, &table, trigger.name.as_str())
                    .await?;
            }

            if let Some(seqs) = owned_sequences.get(&table) {
                for seq in seqs {
                    let _ = self.drop_sequence(txn, db_id, seq).await?;
                }
            }

            let _ = self.drop_table(txn, db_id, &table).await?;
        }

        for trigger in self.list_triggers(txn, db_id).await? {
            if trigger.table.starts_with(&schema_prefix) {
                let _ = self
                    .drop_trigger(txn, db_id, &trigger.table, trigger.name.as_str())
                    .await?;
            }
        }

        for ty in self.list_types(txn, db_id).await? {
            if ty.schema == schema {
                let full_name = format!("{}.{}", ty.schema, ty.name);
                let _ = self.drop_type(txn, db_id, &full_name).await?;
            }
        }

        for func in self.list_functions(txn, db_id).await? {
            if func.schema == schema {
                let full_name = format!("{}.{}", func.schema, func.name);
                let _ = self.drop_function(txn, db_id, &full_name, true).await?;
            }
        }

        for proc_name in self.list_procedures(txn, db_id).await? {
            if proc_name.starts_with(&schema_prefix) {
                let _ = self.drop_procedure(txn, db_id, &proc_name).await?;
            }
        }

        self.drop_schema_restrict(txn, db_id, schema, if_exists)
            .await?;
        Ok(true)
    }
}
