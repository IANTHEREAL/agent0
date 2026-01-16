use super::encoding::*;
use crate::txn::{txn_delete, txn_put};
use crate::types::{
    DataType, Row, SequenceBacking, SequenceDef, SequenceState, TableSchema, UserTypeDef, Value,
};
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tikv_client::{
    BoundRange, CheckLevel, Config, Transaction, TransactionClient, TransactionOptions,
};
use tracing::{debug, info};

/// Maximum scan limit for TiKV operations.
const SCAN_LIMIT: u32 = u32::MAX;

fn nextval_standalone(
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
            return Err(anyhow!(
                "nextval: reached maximum value of sequence \"{}\" ({})",
                full_name,
                max_value
            ));
        }
    } else if candidate < min_value {
        if is_cycled {
            max_value
        } else {
            return Err(anyhow!(
                "nextval: reached minimum value of sequence \"{}\" ({})",
                full_name,
                min_value
            ));
        }
    } else {
        candidate
    };

    state.last_value = wrapped;
    Ok(wrapped)
}

fn setval_standalone(
    full_name: &str,
    min_value: i64,
    max_value: i64,
    state: &mut SequenceState,
    value: i64,
    is_called: bool,
) -> Result<i64> {
    if value < min_value || value > max_value {
        return Err(anyhow!(
            "setval: value {} is out of bounds for sequence \"{}\"",
            value,
            full_name
        ));
    }

    state.last_value = value;
    state.is_called = is_called;
    Ok(value)
}

pub struct TikvStore {
    client: Arc<TransactionClient>,
}

impl TikvStore {
    #[allow(dead_code)]
    pub async fn new(pd_endpoints: Vec<String>) -> Result<Self> {
        Self::new_with_keyspace(pd_endpoints, None).await
    }

    pub async fn new_with_keyspace(
        pd_endpoints: Vec<String>,
        keyspace: Option<String>,
    ) -> Result<Self> {
        info!("Connecting to TiKV at {:?}", pd_endpoints);
        let config = match &keyspace {
            Some(ks) => {
                info!("Using TiKV Keyspace: {}", ks);
                Config::default().with_keyspace(ks)
            }
            None => Config::default(),
        };
        let client = TransactionClient::new_with_config(pd_endpoints, config)
            .await
            .context("Failed to connect to TiKV")?;
        info!("Connected to TiKV. Keyspace: {:?}", keyspace);
        Ok(Self {
            client: Arc::new(client),
        })
    }

    fn key(&self, key: &[u8]) -> Vec<u8> {
        key.to_vec()
    }

    pub async fn begin(&self) -> Result<Transaction> {
        let options = TransactionOptions::new_pessimistic().drop_check(CheckLevel::Warn);
        self.client
            .begin_with_options(options)
            .await
            .map_err(|e| anyhow!(e))
    }

    #[allow(dead_code)]
    pub async fn begin_optimistic(&self) -> Result<Transaction> {
        let options = TransactionOptions::new_optimistic().drop_check(CheckLevel::Warn);
        self.client
            .begin_with_options(options)
            .await
            .map_err(|e| anyhow!(e))
    }

    pub async fn lock_rows(
        &self,
        txn: &mut Transaction,
        table_name: &str,
        rows: &[Row],
    ) -> Result<()> {
        let schema = self
            .get_schema(txn, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let keys: Vec<Vec<u8>> = rows
            .iter()
            .map(|row| {
                let pk_values = schema.get_pk_values(row);
                let row_key = encode_pk_values(&pk_values);
                self.key(&encode_data_key(schema.table_id, &row_key))
            })
            .collect();
        txn.lock_keys(keys).await.map_err(|e| anyhow!(e))
    }

    /// Check if a table exists (using txn)
    pub async fn table_exists(&self, txn: &mut Transaction, table_name: &str) -> Result<bool> {
        let key = self.key(&encode_schema_key(table_name));
        let exists = txn.get(key).await?.is_some();
        Ok(exists)
    }

    fn is_builtin_schema(schema: &str) -> bool {
        matches!(schema, "public" | "pg_catalog" | "information_schema")
    }

    pub async fn schema_exists(&self, txn: &mut Transaction, schema: &str) -> Result<bool> {
        if Self::is_builtin_schema(schema) {
            return Ok(true);
        }
        let key = self.key(&encode_schema_def_key(schema));
        Ok(txn.get(key).await?.is_some())
    }

    pub async fn create_schema(
        &self,
        txn: &mut Transaction,
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

        let key = self.key(&encode_schema_def_key(schema));
        if txn.get(key.clone()).await?.is_some() {
            if if_not_exists {
                return Ok(false);
            }
            return Err(anyhow!("Schema '{}' already exists", schema));
        }
        txn_put(txn, key, Vec::new()).await?;
        Ok(true)
    }

    async fn prefix_has_any(&self, txn: &mut Transaction, prefix: Vec<u8>) -> Result<bool> {
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let mut pairs = txn.scan(range, 1).await?;
        Ok(pairs.next().is_some())
    }

    pub async fn drop_schema_restrict(
        &self,
        txn: &mut Transaction,
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

        let key = self.key(&encode_schema_def_key(schema));
        if txn.get(key.clone()).await?.is_none() {
            if if_exists {
                return Ok(false);
            }
            return Err(anyhow!("Schema '{}' does not exist", schema));
        }

        let mut table_prefix = encode_schema_prefix();
        table_prefix.extend_from_slice(schema.as_bytes());
        table_prefix.push(b'.');
        if self.prefix_has_any(txn, table_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut view_prefix = encode_view_prefix();
        view_prefix.extend_from_slice(schema.as_bytes());
        view_prefix.push(b'.');
        if self.prefix_has_any(txn, view_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut matview_prefix = encode_matview_prefix();
        matview_prefix.extend_from_slice(schema.as_bytes());
        matview_prefix.push(b'.');
        if self.prefix_has_any(txn, matview_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut procedure_prefix = encode_procedure_prefix();
        procedure_prefix.extend_from_slice(schema.as_bytes());
        procedure_prefix.push(b'.');
        if self.prefix_has_any(txn, procedure_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut type_prefix = encode_type_prefix();
        type_prefix.extend_from_slice(schema.as_bytes());
        type_prefix.push(b'.');
        if self.prefix_has_any(txn, type_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut sequence_prefix = encode_sequence_prefix();
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

    pub async fn list_schemas(&self, txn: &mut Transaction) -> Result<Vec<String>> {
        let prefix = encode_schema_def_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut schemas = vec![
            "public".to_string(),
            "information_schema".to_string(),
            "pg_catalog".to_string(),
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

    /// Get the next table ID (auto-increment)
    pub async fn next_table_id(&self, txn: &mut Transaction) -> Result<u64> {
        self.increment_sys_key(txn, encode_next_table_id_key())
            .await
    }

    pub async fn next_type_oid(&self, txn: &mut Transaction) -> Result<u32> {
        const FIRST_USER_TYPE_OID: u32 = 20000;

        let key = self.key(&encode_next_type_oid_key());
        let current = txn.get(key.clone()).await?;
        let next_val = match current {
            Some(data) => {
                let oid = u32::from_be_bytes(
                    data.try_into()
                        .map_err(|_| anyhow!("Invalid type OID format"))?,
                );
                oid.checked_add(1)
                    .ok_or_else(|| anyhow!("Type OID overflow"))?
            }
            None => FIRST_USER_TYPE_OID,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    pub async fn next_sequence_value(&self, txn: &mut Transaction, table_id: u64) -> Result<i32> {
        let mut raw_key = b"_sys_seq_".to_vec();
        raw_key.extend_from_slice(&table_id.to_be_bytes());
        let val = self.increment_sys_key(txn, raw_key).await?;
        Ok(val as i32)
    }

    pub async fn set_sequence_value(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        value: u64,
    ) -> Result<()> {
        let mut raw_key = b"_sys_seq_".to_vec();
        raw_key.extend_from_slice(&table_id.to_be_bytes());
        let key = self.key(&raw_key);
        txn_put(txn, key, value.to_be_bytes().to_vec()).await?;
        Ok(())
    }

    async fn increment_sys_key(&self, txn: &mut Transaction, raw_key: Vec<u8>) -> Result<u64> {
        let key = self.key(&raw_key);
        let current = txn.get(key.clone()).await?;
        let next_val = match current {
            Some(data) => {
                let id =
                    u64::from_be_bytes(data.try_into().map_err(|_| anyhow!("Invalid ID format"))?);
                id + 1
            }
            None => 1,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    /// Create a new table schema
    pub async fn create_table(&self, txn: &mut Transaction, schema: TableSchema) -> Result<()> {
        let schema_key = self.key(&encode_schema_key(&schema.name));
        if txn.get(schema_key.clone()).await?.is_some() {
            return Err(anyhow!("Table '{}' already exists", schema.name));
        }
        let schema_data = serialize_schema(&schema)?;
        txn_put(txn, schema_key, schema_data).await?;
        info!(
            "Created table '{}' with ID {}",
            schema.name, schema.table_id
        );
        Ok(())
    }

    /// Get a table schema by name
    pub async fn get_schema(
        &self,
        txn: &mut Transaction,
        table_name: &str,
    ) -> Result<Option<TableSchema>> {
        let key = self.key(&encode_schema_key(table_name));
        let val = txn.get(key).await?;
        match val {
            Some(data) => Ok(Some(deserialize_schema(&data)?)),
            None => Ok(None),
        }
    }

    /// Drop a table
    pub async fn drop_table(&self, txn: &mut Transaction, table_name: &str) -> Result<bool> {
        let schema_opt = self.get_schema(txn, table_name).await?;
        if let Some(schema) = schema_opt {
            let schema_key = self.key(&encode_schema_key(table_name));
            txn_delete(txn, schema_key).await?;
            let (raw_start, raw_end) = encode_table_data_range(schema.table_id);
            let start = self.key(&raw_start);
            let end = self.key(&raw_end);
            let range: BoundRange = (start..end).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                let key: Vec<u8> = pair.into_key().into();
                txn_delete(txn, key).await?;
            }

            info!("Dropped table '{}'", table_name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Insert a row into a table
    pub async fn insert(&self, txn: &mut Transaction, table_name: &str, row: Row) -> Result<()> {
        let schema = self
            .get_schema(txn, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if row.values.len() != schema.columns.len() {
            return Err(anyhow!("Column count mismatch"));
        }
        let pk_values = schema.get_pk_values(&row);
        let row_key = encode_pk_values(&pk_values);
        let data_key = self.key(&encode_data_key(schema.table_id, &row_key));
        let row_data = serialize_row(&row)?;
        if txn.get(data_key.clone()).await?.is_some() {
            return Err(anyhow!("Duplicate primary key: {:?}", pk_values));
        }
        txn_put(txn, data_key, row_data).await?;
        debug!("Inserted row into '{}'", table_name);
        Ok(())
    }

    /// Upsert a row into a table
    pub async fn upsert(&self, txn: &mut Transaction, table_name: &str, row: Row) -> Result<()> {
        let schema = self
            .get_schema(txn, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let pk_values = schema.get_pk_values(&row);
        let row_key = encode_pk_values(&pk_values);
        let data_key = self.key(&encode_data_key(schema.table_id, &row_key));
        let row_data = serialize_row(&row)?;
        txn_put(txn, data_key, row_data).await?;
        debug!("Upserted row into '{}'", table_name);
        Ok(())
    }

    /// Scan all rows from a table
    pub async fn scan(&self, txn: &mut Transaction, table_name: &str) -> Result<Vec<Row>> {
        let schema = self
            .get_schema(txn, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let (raw_start, raw_end) = encode_table_data_range(schema.table_id);
        let start = self.key(&raw_start);
        let end = self.key(&raw_end);
        let range: BoundRange = (start..end).into();
        let pairs: Vec<_> = txn.scan(range, SCAN_LIMIT).await?.collect();
        let mut rows = Vec::new();
        for pair in pairs {
            let row = deserialize_row(pair.value())?;
            rows.push(row);
        }
        debug!("Scanned {} rows from '{}'", rows.len(), table_name);
        Ok(rows)
    }

    /// Delete rows matching a simple condition
    pub async fn delete_by_pk(
        &self,
        txn: &mut Transaction,
        table_name: &str,
        pk_values: &[Value],
    ) -> Result<u64> {
        let schema = self
            .get_schema(txn, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let row_key = encode_pk_values(pk_values);
        let data_key = self.key(&encode_data_key(schema.table_id, &row_key));
        let existed = txn.get(data_key.clone()).await?.is_some();
        if existed {
            txn_delete(txn, data_key).await?;
            Ok(1)
        } else {
            Ok(0)
        }
    }

    #[allow(dead_code)]
    pub async fn get_by_pk(
        &self,
        txn: &mut Transaction,
        table_name: &str,
        pk_values: &[Value],
    ) -> Result<Option<Row>> {
        let schema = self
            .get_schema(txn, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let row_key = encode_pk_values(pk_values);
        let data_key = self.key(&encode_data_key(schema.table_id, &row_key));
        match txn.get(data_key).await? {
            Some(data) => Ok(Some(deserialize_row(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn list_tables(&self, txn: &mut Transaction) -> Result<Vec<String>> {
        let prefix = encode_schema_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut tables = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if key.starts_with(&prefix) {
                let name = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
                tables.push(name);
            }
        }
        Ok(tables)
    }

    pub async fn create_type(&self, txn: &mut Transaction, def: UserTypeDef) -> Result<()> {
        let full_name = format!("{}.{}", def.schema, def.name);
        let key = self.key(&encode_type_key(&full_name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Type '{}' already exists", full_name));
        }
        let data = bincode::serialize(&def).context("Failed to serialize type definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn get_type(
        &self,
        txn: &mut Transaction,
        full_name: &str,
    ) -> Result<Option<UserTypeDef>> {
        let key = self.key(&encode_type_key(full_name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize type definition")?,
            )),
            None => Ok(None),
        }
    }

    pub async fn list_types(&self, txn: &mut Transaction) -> Result<Vec<UserTypeDef>> {
        let prefix = encode_type_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut types = Vec::new();
        for pair in pairs {
            let def: UserTypeDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize type")?;
            types.push(def);
        }
        Ok(types)
    }

    pub async fn drop_type(&self, txn: &mut Transaction, full_name: &str) -> Result<bool> {
        let key = self.key(&encode_type_key(full_name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn create_sequence(&self, txn: &mut Transaction, def: SequenceDef) -> Result<()> {
        let full_name = def.full_name();
        let key = self.key(&encode_sequence_key(&full_name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Sequence '{}' already exists", full_name));
        }
        let data = bincode::serialize(&def).context("Failed to serialize sequence definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn get_sequence(
        &self,
        txn: &mut Transaction,
        full_name: &str,
    ) -> Result<Option<SequenceDef>> {
        let key = self.key(&encode_sequence_key(full_name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize sequence definition")?,
            )),
            None => Ok(None),
        }
    }

    pub async fn list_sequences(&self, txn: &mut Transaction) -> Result<Vec<SequenceDef>> {
        let prefix = encode_sequence_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut sequences = Vec::new();
        for pair in pairs {
            let def: SequenceDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize sequence")?;
            sequences.push(def);
        }
        Ok(sequences)
    }

    pub async fn drop_sequence(&self, txn: &mut Transaction, full_name: &str) -> Result<bool> {
        let key = self.key(&encode_sequence_key(full_name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn nextval_sequence(&self, txn: &mut Transaction, full_name: &str) -> Result<i64> {
        let mut def = self
            .get_sequence(txn, full_name)
            .await?
            .ok_or_else(|| anyhow!("Sequence '{}' does not exist", full_name))?;

        match &mut def.backing {
            SequenceBacking::TableId(table_id) => {
                Ok(self.next_sequence_value(txn, *table_id).await? as i64)
            }
            SequenceBacking::Standalone(state) => {
                let next = nextval_standalone(
                    full_name,
                    def.increment,
                    def.min_value,
                    def.max_value,
                    def.is_cycled,
                    state,
                )?;

                let key = self.key(&encode_sequence_key(full_name));
                let data =
                    bincode::serialize(&def).context("Failed to serialize sequence definition")?;
                txn_put(txn, key, data).await?;
                Ok(next)
            }
        }
    }

    pub async fn setval_sequence(
        &self,
        txn: &mut Transaction,
        full_name: &str,
        value: i64,
        is_called: bool,
    ) -> Result<i64> {
        let mut def = self
            .get_sequence(txn, full_name)
            .await?
            .ok_or_else(|| anyhow!("Sequence '{}' does not exist", full_name))?;

        match &mut def.backing {
            SequenceBacking::TableId(table_id) => {
                if value < 1 {
                    return Err(anyhow!(
                        "setval: value {} is out of bounds for sequence \"{}\"",
                        value,
                        full_name
                    ));
                }
                let value_u64: u64 = value.try_into().map_err(|_| {
                    anyhow!(
                        "setval: value {} is too large for sequence \"{}\"",
                        value,
                        full_name
                    )
                })?;
                let stored = if is_called {
                    value_u64
                } else {
                    value_u64.checked_sub(1).ok_or_else(|| {
                        anyhow!(
                            "setval: value {} is out of bounds for sequence \"{}\"",
                            value,
                            full_name
                        )
                    })?
                };
                self.set_sequence_value(txn, *table_id, stored).await?;
                Ok(value)
            }
            SequenceBacking::Standalone(state) => {
                setval_standalone(
                    full_name,
                    def.min_value,
                    def.max_value,
                    state,
                    value,
                    is_called,
                )?;

                let key = self.key(&encode_sequence_key(full_name));
                let data =
                    bincode::serialize(&def).context("Failed to serialize sequence definition")?;
                txn_put(txn, key, data).await?;
                Ok(value)
            }
        }
    }

    /// Truncate a table
    pub async fn truncate_table(&self, txn: &mut Transaction, table_name: &str) -> Result<bool> {
        let schema_opt = self.get_schema(txn, table_name).await?;
        if let Some(schema) = schema_opt {
            let (raw_start, raw_end) = encode_table_data_range(schema.table_id);
            let start = self.key(&raw_start);
            let end = self.key(&raw_end);
            let range: BoundRange = (start..end).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                let key: Vec<u8> = pair.into_key().into();
                txn_delete(txn, key).await?;
            }
            info!("Truncated table '{}'", table_name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Update table schema
    pub async fn update_schema(&self, txn: &mut Transaction, schema: TableSchema) -> Result<()> {
        let schema_key = self.key(&encode_schema_key(&schema.name));
        let schema_data = serialize_schema(&schema)?;
        txn_put(txn, schema_key, schema_data).await?;
        Ok(())
    }

    /// Rename a table by moving its schema metadata key.
    ///
    /// This operation does **not** rewrite row or index keys because those are
    /// keyed by `table_id`, not by table name.
    ///
    /// Callers are responsible for updating cross-table references (e.g.
    /// `foreign_keys[*].ref_table`) if needed.
    pub async fn rename_table_schema(
        &self,
        txn: &mut Transaction,
        old_table: &str,
        new_table: &str,
    ) -> Result<()> {
        if old_table == new_table {
            return Ok(());
        }

        let old_key = self.key(&encode_schema_key(old_table));
        let new_key = self.key(&encode_schema_key(new_table));

        if txn.get(new_key.clone()).await?.is_some() {
            return Err(anyhow!("Table '{}' already exists", new_table));
        }

        let schema_bytes = txn
            .get(old_key.clone())
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", old_table))?;

        let mut schema = deserialize_schema(&schema_bytes)?;
        schema.name = new_table.to_string();
        schema.version = schema
            .version
            .checked_add(1)
            .ok_or_else(|| anyhow!("Schema version overflow"))?;

        txn_put(txn, new_key, serialize_schema(&schema)?).await?;
        txn_delete(txn, old_key).await?;
        Ok(())
    }

    /// Create an index entry
    pub async fn create_index_entry(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        pk_values: &[Value],
        unique: bool,
    ) -> Result<()> {
        if unique {
            let idx_key = self.key(&encode_index_key(table_id, index_id, values, None));
            if txn.get(idx_key.clone()).await?.is_some() {
                return Err(anyhow!("Duplicate entry for unique index"));
            }
            let idx_val = encode_pk_values(pk_values);
            txn_put(txn, idx_key, idx_val).await?;
        } else {
            let idx_key = self.key(&encode_index_key(
                table_id,
                index_id,
                values,
                Some(pk_values),
            ));
            txn_put(txn, idx_key, vec![]).await?;
        }
        Ok(())
    }

    /// Delete an index entry
    pub async fn delete_index_entry(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        pk_values: &[Value],
        unique: bool,
    ) -> Result<()> {
        if unique {
            let idx_key = self.key(&encode_index_key(table_id, index_id, values, None));
            txn_delete(txn, idx_key).await?;
        } else {
            let idx_key = self.key(&encode_index_key(
                table_id,
                index_id,
                values,
                Some(pk_values),
            ));
            txn_delete(txn, idx_key).await?;
        }
        Ok(())
    }

    /// Scan index to get PKs
    pub async fn scan_index(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        unique: bool,
        pk_types: &[DataType],
    ) -> Result<Vec<Vec<Value>>> {
        if pk_types.is_empty() {
            return Err(anyhow!("PK types required for index scan"));
        }

        if unique {
            let idx_key = self.key(&encode_index_key(table_id, index_id, values, None));
            if let Some(val) = txn.get(idx_key).await? {
                let pk = decode_pk_from_index_suffix(&val, pk_types)?;
                Ok(vec![pk])
            } else {
                Ok(vec![])
            }
        } else {
            let prefix = encode_index_key(table_id, index_id, values, None);

            let mut start_raw = prefix.clone();
            start_raw.push(0x01);
            let start_key = self.key(&start_raw);

            let mut end_raw = prefix;
            end_raw.push(0x02);
            let end_key = self.key(&end_raw);

            let range: BoundRange = (start_key.clone()..end_key).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;

            let mut pks = Vec::new();
            for pair in pairs {
                let full_key: &[u8] = pair.key().as_ref().into();
                if full_key.len() <= start_key.len() {
                    continue;
                }
                let pk_bytes = &full_key[start_key.len()..];
                let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
                pks.push(pk);
            }
            Ok(pks)
        }
    }

    /// Batch get rows by PKs
    pub async fn batch_get_rows(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        pks: Vec<Vec<Value>>,
        _schema: &TableSchema,
    ) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for pk in &pks {
            let row_key = encode_pk_values(pk);
            let data_key = self.key(&encode_data_key(table_id, &row_key));
            if let Some(val) = txn.get(data_key).await? {
                let row = deserialize_row(&val)?;
                rows.push(row);
            }
        }
        Ok(rows)
    }

    pub async fn create_view(&self, txn: &mut Transaction, name: &str, query: &str) -> Result<()> {
        let key = self.key(&encode_view_key(name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("View '{}' already exists", name));
        }
        txn_put(txn, key, query.as_bytes().to_vec()).await?;
        info!("Created view '{}'", name);
        Ok(())
    }

    pub async fn get_view(&self, txn: &mut Transaction, name: &str) -> Result<Option<String>> {
        let key = self.key(&encode_view_key(name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(String::from_utf8(data)?)),
            None => Ok(None),
        }
    }

    pub async fn drop_view(&self, txn: &mut Transaction, name: &str) -> Result<bool> {
        let key = self.key(&encode_view_key(name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            info!("Dropped view '{}'", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn list_views(&self, txn: &mut Transaction) -> Result<Vec<String>> {
        let prefix = encode_view_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut views = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if key.starts_with(&prefix) {
                let name = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
                views.push(name);
            }
        }
        Ok(views)
    }

    pub async fn create_materialized_view(
        &self,
        txn: &mut Transaction,
        name: &str,
        query: &str,
    ) -> Result<()> {
        let key = self.key(&encode_matview_key(name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Materialized view '{}' already exists", name));
        }
        txn_put(txn, key, query.as_bytes().to_vec()).await?;
        info!("Created materialized view '{}'", name);
        Ok(())
    }

    pub async fn get_materialized_view(
        &self,
        txn: &mut Transaction,
        name: &str,
    ) -> Result<Option<String>> {
        let key = self.key(&encode_matview_key(name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(String::from_utf8(data)?)),
            None => Ok(None),
        }
    }

    pub async fn drop_materialized_view(&self, txn: &mut Transaction, name: &str) -> Result<bool> {
        let key = self.key(&encode_matview_key(name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            info!("Dropped materialized view '{}'", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[allow(dead_code)]
    pub async fn list_materialized_views(&self, txn: &mut Transaction) -> Result<Vec<String>> {
        let prefix = encode_matview_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut matviews = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if key.starts_with(&prefix) {
                let name = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
                matviews.push(name);
            }
        }
        Ok(matviews)
    }

    pub async fn create_procedure(
        &self,
        txn: &mut Transaction,
        name: &str,
        definition: &str,
    ) -> Result<()> {
        let key = self.key(&encode_procedure_key(name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Procedure '{}' already exists", name));
        }
        txn_put(txn, key, definition.as_bytes().to_vec()).await?;
        info!("Created procedure '{}'", name);
        Ok(())
    }

    pub async fn get_procedure(&self, txn: &mut Transaction, name: &str) -> Result<Option<String>> {
        let key = self.key(&encode_procedure_key(name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(String::from_utf8(data)?)),
            None => Ok(None),
        }
    }

    pub async fn drop_procedure(&self, txn: &mut Transaction, name: &str) -> Result<bool> {
        let key = self.key(&encode_procedure_key(name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            info!("Dropped procedure '{}'", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[allow(dead_code)]
    pub async fn replace_procedure(
        &self,
        txn: &mut Transaction,
        name: &str,
        definition: &str,
    ) -> Result<()> {
        let key = self.key(&encode_procedure_key(name));
        txn_put(txn, key, definition.as_bytes().to_vec()).await?;
        info!("Replaced procedure '{}'", name);
        Ok(())
    }
}

#[cfg(test)]
mod sequence_tests {
    use super::*;

    #[test]
    fn test_standalone_nextval_is_called_semantics() {
        let mut state = SequenceState {
            last_value: 1,
            is_called: false,
        };

        let first = nextval_standalone("public.s", 1, 1, i64::MAX, false, &mut state).unwrap();
        assert_eq!(first, 1);
        assert_eq!(state.last_value, 1);
        assert!(state.is_called);

        let second = nextval_standalone("public.s", 1, 1, i64::MAX, false, &mut state).unwrap();
        assert_eq!(second, 2);
        assert_eq!(state.last_value, 2);
        assert!(state.is_called);
    }

    #[test]
    fn test_standalone_setval_is_called_false_behavior() {
        let mut state = SequenceState {
            last_value: 1,
            is_called: true,
        };

        setval_standalone("public.s", 1, i64::MAX, &mut state, 20, false).unwrap();
        assert_eq!(state.last_value, 20);
        assert!(!state.is_called);

        let first = nextval_standalone("public.s", 1, 1, i64::MAX, false, &mut state).unwrap();
        assert_eq!(first, 20);
        assert_eq!(state.last_value, 20);
        assert!(state.is_called);

        let second = nextval_standalone("public.s", 1, 1, i64::MAX, false, &mut state).unwrap();
        assert_eq!(second, 21);
        assert_eq!(state.last_value, 21);
    }

    #[test]
    fn test_standalone_cycle_wraps() {
        let mut state = SequenceState {
            last_value: 2,
            is_called: true,
        };

        let wrapped = nextval_standalone("public.s", 1, 1, 2, true, &mut state).unwrap();
        assert_eq!(wrapped, 1);
        assert_eq!(state.last_value, 1);
    }

    #[test]
    fn test_standalone_non_cycle_errors_on_bound() {
        let mut state = SequenceState {
            last_value: 2,
            is_called: true,
        };

        let err = nextval_standalone("public.s", 1, 1, 2, false, &mut state)
            .unwrap_err()
            .to_string();
        assert!(err.contains("reached maximum value"));
    }
}
