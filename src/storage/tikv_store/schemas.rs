use super::*;
use crate::sql::error::SqlError;

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
            return Err(SqlError::DuplicateSchema(schema.to_string()).into());
        }

        let key = self.key(&encode_schema_def_key_v2(db_id, schema));
        if txn.get(key.clone()).await?.is_some() {
            if if_not_exists {
                return Ok(false);
            }
            return Err(SqlError::DuplicateSchema(schema.to_string()).into());
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
            return Err(SqlError::DependentObjectsStillExist {
                message: format!("cannot drop schema '{}'", schema),
            }
            .into());
        }

        let key = self.key(&encode_schema_def_key_v2(db_id, schema));
        if txn.get(key.clone()).await?.is_none() {
            if if_exists {
                return Ok(false);
            }
            return Err(SqlError::InvalidSchemaName(schema.to_string()).into());
        }

        // Check for dependent objects — match PostgreSQL error format
        let schema_prefix = format!("{}.", schema);
        let mut deps: Vec<String> = Vec::new();

        // Tables (use schema-qualified name like PG: "table app.users depends on schema app")
        for table in self.list_tables(txn, db_id).await? {
            if table.starts_with(&schema_prefix) {
                deps.push(format!("table {} depends on schema {}", table, schema));
            }
        }
        // Views
        for view in self.list_views(txn, db_id).await? {
            if view.schema == schema {
                deps.push(format!("view {} depends on schema {}", view.name, schema));
            }
        }
        // Materialized views
        for mv in self.list_materialized_views(txn, db_id).await? {
            if mv.schema == schema {
                deps.push(format!(
                    "materialized view {} depends on schema {}",
                    mv.name, schema
                ));
            }
        }
        // Functions
        for func in self.list_functions(txn, db_id).await? {
            if func.schema == schema {
                deps.push(format!(
                    "function {}() depends on schema {}",
                    func.name, schema
                ));
            }
        }
        // Procedures
        for proc_name in self.list_procedures(txn, db_id).await? {
            if proc_name.starts_with(&schema_prefix) {
                let name = proc_name.strip_prefix(&schema_prefix).unwrap_or(&proc_name);
                deps.push(format!("procedure {}() depends on schema {}", name, schema));
            }
        }
        // Sequences
        for seq in self.list_sequences(txn, db_id).await? {
            if seq.schema == schema {
                deps.push(format!(
                    "sequence {}.{} depends on schema {}",
                    schema, seq.name, schema
                ));
            }
        }
        // Types
        for udt in self.list_types(txn, db_id).await? {
            let udt_schema = udt
                .name
                .rsplit_once('.')
                .map(|(s, _)| s)
                .unwrap_or("public");
            if udt_schema == schema {
                let bare_name = udt
                    .name
                    .rsplit_once('.')
                    .map(|(_, n)| n)
                    .unwrap_or(&udt.name);
                deps.push(format!("type {} depends on schema {}", bare_name, schema));
            }
        }

        if !deps.is_empty() {
            let detail = deps.join("\n");
            return Err(SqlError::DependentObjectsStillExist {
                message: format!(
                    "cannot drop schema {} because other objects depend on it\nDETAIL:  {}\nHINT:  Use DROP ... CASCADE to drop the dependent objects too.",
                    schema, detail
                ),
            }
            .into());
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
            return Err(SqlError::DependentObjectsStillExist {
                message: format!("cannot drop schema '{}'", schema),
            }
            .into());
        }

        let key = self.key(&encode_schema_def_key_v2(db_id, schema));
        if txn.get(key.clone()).await?.is_none() {
            if if_exists {
                return Ok(false);
            }
            return Err(SqlError::InvalidSchemaName(schema.to_string()).into());
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

#[cfg(test)]
mod tests {
    use super::TikvStore;

    #[test]
    fn builtin_schema_detection_matches_contract() {
        assert!(TikvStore::is_builtin_schema("public"));
        assert!(TikvStore::is_builtin_schema("pg_catalog"));
        assert!(TikvStore::is_builtin_schema("information_schema"));
        assert!(TikvStore::is_builtin_schema("extensions"));

        assert!(!TikvStore::is_builtin_schema("app"));
        assert!(!TikvStore::is_builtin_schema("public_v2"));
    }
}
