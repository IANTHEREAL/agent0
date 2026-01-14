use super::encoding::*;
use crate::types::{DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Context, Result};
use std::sync::Arc;
use tikv_client::{
    BoundRange, CheckLevel, Config, Transaction, TransactionClient, TransactionOptions,
};
use tracing::{debug, info};

/// Maximum scan limit - use a large but safe value to avoid overflow in TiKV client buffer
/// when it adds headroom for deleted entries. u32::MAX causes overflow when combined with
/// the count of deleted entries in the transaction buffer.
const SCAN_LIMIT: u32 = i32::MAX as u32;

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

    /// Get the next table ID (auto-increment)
    pub async fn next_table_id(&self, txn: &mut Transaction) -> Result<u64> {
        self.increment_sys_key(txn, encode_next_table_id_key())
            .await
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
        txn.put(key, value.to_be_bytes().to_vec()).await?;
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
        txn.put(key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    /// Create a new table schema
    pub async fn create_table(&self, txn: &mut Transaction, schema: TableSchema) -> Result<()> {
        let schema_key = self.key(&encode_schema_key(&schema.name));
        if txn.get(schema_key.clone()).await?.is_some() {
            return Err(anyhow!("Table '{}' already exists", schema.name));
        }
        let schema_data = serialize_schema(&schema)?;
        txn.put(schema_key, schema_data).await?;
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
            txn.delete(schema_key).await?;
            let (raw_start, raw_end) = encode_table_data_range(schema.table_id);
            let start = self.key(&raw_start);
            let end = self.key(&raw_end);
            let range: BoundRange = (start..end).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                txn.delete(pair.key().clone()).await?;
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
        txn.put(data_key, row_data).await?;
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
        txn.put(data_key, row_data).await?;
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
            let row = deserialize_row(&pair.value())?;
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
            txn.delete(data_key).await?;
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
                txn.delete(pair.key().clone()).await?;
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
        txn.put(schema_key, schema_data).await?;
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
            txn.put(idx_key, idx_val).await?;
        } else {
            let idx_key = self.key(&encode_index_key(
                table_id,
                index_id,
                values,
                Some(pk_values),
            ));
            txn.put(idx_key, vec![]).await?;
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
            txn.delete(idx_key).await?;
        } else {
            let idx_key = self.key(&encode_index_key(
                table_id,
                index_id,
                values,
                Some(pk_values),
            ));
            txn.delete(idx_key).await?;
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
        txn.put(key, query.as_bytes().to_vec()).await?;
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
            txn.delete(key).await?;
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
        txn.put(key, query.as_bytes().to_vec()).await?;
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
            txn.delete(key).await?;
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
        txn.put(key, definition.as_bytes().to_vec()).await?;
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
            txn.delete(key).await?;
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
        txn.put(key, definition.as_bytes().to_vec()).await?;
        info!("Replaced procedure '{}'", name);
        Ok(())
    }
}
