use super::encoding::*;
use crate::txn::{txn_delete, txn_put};
use crate::types::{
    DataType, FunctionDef, Row, SequenceBacking, SequenceDef, SequenceState, TableSchema,
    TriggerDef, UserTypeDef, Value, ViewDef,
};
use crate::extensions::InstalledExtension;
use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommentTarget {
    Extension { name: String },
    Function { full_name: String },
    Table { full_name: String },
    Column {
        table_full_name: String,
        column_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommentRecord {
    pub(crate) target: CommentTarget,
    pub(crate) description: String,
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
        matches!(schema, "public" | "pg_catalog" | "information_schema" | "extensions")
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
        let oid = self.next_schema_oid(txn).await?;
        txn_put(txn, key, oid.to_be_bytes().to_vec()).await?;
        Ok(true)
    }

    pub async fn list_schema_oids(&self, txn: &mut Transaction) -> Result<HashMap<String, u32>> {
        let prefix = encode_schema_def_prefix();
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
                    let new_oid = self.next_schema_oid(txn).await?;
                    let schema_key = self.key(&encode_schema_def_key(&schema));
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

        let mut function_prefix = encode_function_prefix();
        function_prefix.extend_from_slice(schema.as_bytes());
        function_prefix.push(b'.');
        if self.prefix_has_any(txn, function_prefix).await? {
            return Err(anyhow!(
                "cannot drop schema '{}': schema is not empty",
                schema
            ));
        }

        let mut trigger_prefix = encode_trigger_prefix();
        trigger_prefix.extend_from_slice(schema.as_bytes());
        trigger_prefix.push(b'.');
        if self.prefix_has_any(txn, trigger_prefix).await? {
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

    pub async fn get_extension(
        &self,
        txn: &mut Transaction,
        ext_name: &str,
    ) -> Result<Option<InstalledExtension>> {
        let key = self.key(&encode_extension_key(ext_name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize extension")?,
            )),
            None => Ok(None),
        }
    }

    pub async fn put_extension(&self, txn: &mut Transaction, ext: &InstalledExtension) -> Result<()> {
        let key = self.key(&encode_extension_key(&ext.name));
        let data = bincode::serialize(ext).context("Failed to serialize extension")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn drop_extension(&self, txn: &mut Transaction, ext_name: &str) -> Result<bool> {
        let key = self.key(&encode_extension_key(ext_name));
        let existed = txn.get(key.clone()).await?.is_some();
        if !existed {
            return Ok(false);
        }

        txn_delete(txn, key).await?;
        let cfg_key = self.key(&encode_extension_config_key(ext_name));
        let _ = txn_delete(txn, cfg_key).await;
        Ok(true)
    }

    pub async fn list_extensions(&self, txn: &mut Transaction) -> Result<Vec<InstalledExtension>> {
        let prefix = encode_extension_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut exts = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let ext: InstalledExtension =
                bincode::deserialize(pair.value()).context("Failed to deserialize extension")?;
            exts.push(ext);
        }
        Ok(exts)
    }

    /// Set or clear a comment for an installed extension.
    pub(crate) async fn set_extension_comment(
        &self,
        txn: &mut Transaction,
        ext_name: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let key = self.key(&encode_comment_extension_key(ext_name));
        match comment {
            Some(text) => txn_put(txn, key, text.as_bytes().to_vec()).await,
            None => txn_delete(txn, key).await,
        }
    }

    /// Set or clear a comment for a function (`schema.name`).
    pub(crate) async fn set_function_comment(
        &self,
        txn: &mut Transaction,
        func_full_name: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let key = self.key(&encode_comment_function_key(func_full_name));
        match comment {
            Some(text) => txn_put(txn, key, text.as_bytes().to_vec()).await,
            None => txn_delete(txn, key).await,
        }
    }

    /// Set or clear a comment for a table (`schema.name`).
    pub(crate) async fn set_table_comment(
        &self,
        txn: &mut Transaction,
        table_full_name: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let key = self.key(&encode_comment_table_key(table_full_name));
        match comment {
            Some(text) => txn_put(txn, key, text.as_bytes().to_vec()).await,
            None => txn_delete(txn, key).await,
        }
    }

    /// Set or clear a comment for a table column.
    pub(crate) async fn set_column_comment(
        &self,
        txn: &mut Transaction,
        table_full_name: &str,
        column_name: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let key = self.key(&encode_comment_column_key(table_full_name, column_name));
        match comment {
            Some(text) => txn_put(txn, key, text.as_bytes().to_vec()).await,
            None => txn_delete(txn, key).await,
        }
    }

    /// List all stored comments for the current tenant.
    pub(crate) async fn list_comments(&self, txn: &mut Transaction) -> Result<Vec<CommentRecord>> {
        let prefix = encode_comment_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut records = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let rest = &key[prefix.len()..];
            let Some((&kind, payload)) = rest.split_first() else {
                continue;
            };
            if payload.first() != Some(&0) {
                continue;
            }
            let payload = &payload[1..];

            let target = match kind {
                b'e' => CommentTarget::Extension {
                    name: std::str::from_utf8(payload)
                        .context("comment key: invalid extension name")?
                        .to_string(),
                },
                b'f' => CommentTarget::Function {
                    full_name: std::str::from_utf8(payload)
                        .context("comment key: invalid function name")?
                        .to_string(),
                },
                b't' => CommentTarget::Table {
                    full_name: std::str::from_utf8(payload)
                        .context("comment key: invalid table name")?
                        .to_string(),
                },
                b'c' => {
                    let Some(split) = payload.iter().position(|b| *b == 0) else {
                        continue;
                    };
                    let table = std::str::from_utf8(&payload[..split])
                        .context("comment key: invalid table name")?
                        .to_string();
                    let column = std::str::from_utf8(&payload[split + 1..])
                        .context("comment key: invalid column name")?
                        .to_string();
                    CommentTarget::Column {
                        table_full_name: table,
                        column_name: column,
                    }
                }
                _ => continue,
            };

            let description = std::str::from_utf8(pair.value())
                .context("comment value is not valid UTF-8")?
                .to_string();

            records.push(CommentRecord { target, description });
        }

        Ok(records)
    }

    pub async fn next_schema_oid(&self, txn: &mut Transaction) -> Result<u32> {
        const FIRST_USER_SCHEMA_OID: u32 = 20000;

        let key = self.key(&encode_next_schema_oid_key());
        let current = txn.get(key.clone()).await?;
        let next_val = match current {
            Some(data) => {
                let oid = u32::from_be_bytes(
                    data.try_into()
                        .map_err(|_| anyhow!("Invalid schema OID format"))?,
                );
                oid.checked_add(1)
                    .ok_or_else(|| anyhow!("Schema OID overflow"))?
            }
            None => FIRST_USER_SCHEMA_OID,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
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

    pub async fn next_sequence_oid(&self, txn: &mut Transaction) -> Result<u32> {
        const FIRST_SEQUENCE_OID: u32 = 1;

        let key = self.key(&encode_next_sequence_oid_key());
        let current = txn.get(key.clone()).await?;
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
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    pub async fn next_function_oid(&self, txn: &mut Transaction) -> Result<u32> {
        const FIRST_FUNCTION_OID: u32 = 1;

        let key = self.key(&encode_next_function_oid_key());
        let current = txn.get(key.clone()).await?;
        let next_val = match current {
            Some(data) => {
                let oid = u32::from_be_bytes(
                    data.try_into()
                        .map_err(|_| anyhow!("Invalid function OID format"))?,
                );
                oid.checked_add(1)
                    .ok_or_else(|| anyhow!("Function OID overflow"))?
            }
            None => FIRST_FUNCTION_OID,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    pub async fn next_trigger_oid(&self, txn: &mut Transaction) -> Result<u32> {
        const FIRST_TRIGGER_OID: u32 = 1;

        let key = self.key(&encode_next_trigger_oid_key());
        let current = txn.get(key.clone()).await?;
        let next_val = match current {
            Some(data) => {
                let oid = u32::from_be_bytes(
                    data.try_into()
                        .map_err(|_| anyhow!("Invalid trigger OID format"))?,
                );
                oid.checked_add(1)
                    .ok_or_else(|| anyhow!("Trigger OID overflow"))?
            }
            None => FIRST_TRIGGER_OID,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    pub async fn next_view_oid(&self, txn: &mut Transaction) -> Result<u32> {
        const FIRST_VIEW_OID: u32 = 1;

        let key = self.key(&encode_next_view_oid_key());
        let current = txn.get(key.clone()).await?;
        let next_val = match current {
            Some(data) => {
                let oid = u32::from_be_bytes(
                    data.try_into()
                        .map_err(|_| anyhow!("Invalid view OID format"))?,
                );
                oid.checked_add(1)
                    .ok_or_else(|| anyhow!("View OID overflow"))?
            }
            None => FIRST_VIEW_OID,
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
            let short_table = table_name.rsplit('.').next().unwrap_or(table_name);
            let constraint_name = schema
                .pk_constraint_name
                .clone()
                .unwrap_or_else(|| format!("{}_pkey", short_table));
            let pk_col_names: Vec<String> = schema
                .pk_indices
                .iter()
                .map(|&i| schema.columns[i].name.clone())
                .collect();
            let pk_val_strs: Vec<String> = pk_values
                .iter()
                .map(|v| match v {
                    crate::types::Value::Int32(n) => n.to_string(),
                    crate::types::Value::Int64(n) => n.to_string(),
                    crate::types::Value::Text(s) => s.clone(),
                    crate::types::Value::Uuid(bytes) => uuid::Uuid::from_bytes(*bytes).to_string(),
                    other => format!("{}", other),
                })
                .collect();
            return Err(anyhow!(
                "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                constraint_name,
                pk_col_names.join(", "),
                pk_val_strs.join(", ")
            ));
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

    pub async fn create_sequence(&self, txn: &mut Transaction, mut def: SequenceDef) -> Result<()> {
        if def.oid == 0 {
            def.oid = self.next_sequence_oid(txn).await?;
        }

        let full_name = def.full_name();
        let key = self.key(&encode_sequence_key(&full_name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Sequence '{}' already exists", full_name));
        }
        let data = bincode::serialize(&def).context("Failed to serialize sequence definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Persist an updated sequence definition (metadata only; does not touch sequence state).
    pub async fn update_sequence_def(&self, txn: &mut Transaction, def: &SequenceDef) -> Result<()> {
        let key = self.key(&encode_sequence_key(&def.full_name()));
        let data = bincode::serialize(def).context("Failed to serialize sequence definition")?;
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
            Some(data) => {
                let mut def: SequenceDef = bincode::deserialize(&data)
                    .context("Failed to deserialize sequence definition")?;
                let mut needs_update = false;
                if def.oid == 0 {
                    def.oid = self.next_sequence_oid(txn).await?;
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
                    let data = bincode::serialize(&def)
                        .context("Failed to serialize sequence definition")?;
                    txn_put(txn, self.key(&encode_sequence_key(full_name)), data).await?;
                }
                Ok(Some(def))
            }
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
            let mut def: SequenceDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize sequence")?;
            let mut needs_update = false;
            if def.oid == 0 {
                def.oid = self.next_sequence_oid(txn).await?;
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
                let data = bincode::serialize(&def).context("Failed to serialize sequence")?;
                txn_put(txn, self.key(&encode_sequence_key(&def.full_name())), data).await?;
            }
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

    pub async fn create_function(&self, txn: &mut Transaction, mut def: FunctionDef) -> Result<()> {
        if def.oid == 0 {
            def.oid = self.next_function_oid(txn).await?;
        }

        let full_name = format!("{}.{}", def.schema, def.name);
        let key = self.key(&encode_function_key(&full_name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Function '{}' already exists", full_name));
        }
        let data = serialize_function_def(&def)?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn replace_function(
        &self,
        txn: &mut Transaction,
        mut def: FunctionDef,
    ) -> Result<()> {
        let full_name = format!("{}.{}", def.schema, def.name);
        let key = self.key(&encode_function_key(&full_name));

        if def.oid == 0 {
            if let Some(existing) = txn.get(key.clone()).await? {
                let existing: FunctionDef = deserialize_function_def(&existing)?;
                if existing.oid != 0 {
                    def.oid = existing.oid;
                }
            }
        }
        if def.oid == 0 {
            def.oid = self.next_function_oid(txn).await?;
        }

        let data = serialize_function_def(&def)?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn get_function(
        &self,
        txn: &mut Transaction,
        full_name: &str,
    ) -> Result<Option<FunctionDef>> {
        let key = self.key(&encode_function_key(full_name));
        match txn.get(key).await? {
            Some(data) => {
                let mut def: FunctionDef = deserialize_function_def(&data)?;
                if def.oid == 0 {
                    def.oid = self.next_function_oid(txn).await?;
                    let data = serialize_function_def(&def)?;
                    txn_put(txn, self.key(&encode_function_key(full_name)), data).await?;
                }
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    pub async fn list_functions(&self, txn: &mut Transaction) -> Result<Vec<FunctionDef>> {
        let prefix = encode_function_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut funcs = Vec::new();
        for pair in pairs {
            let mut def: FunctionDef = deserialize_function_def(pair.value())?;
            let mut needs_update = false;
            if def.oid == 0 {
                def.oid = self.next_function_oid(txn).await?;
                needs_update = true;
            }
            if needs_update {
                let data = serialize_function_def(&def)?;
                let full_name = format!("{}.{}", def.schema, def.name);
                txn_put(txn, self.key(&encode_function_key(&full_name)), data).await?;
            }
            funcs.push(def);
        }
        Ok(funcs)
    }

    pub async fn drop_function(&self, txn: &mut Transaction, full_name: &str) -> Result<bool> {
        let key = self.key(&encode_function_key(full_name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn create_trigger(&self, txn: &mut Transaction, mut def: TriggerDef) -> Result<()> {
        if def.oid == 0 {
            def.oid = self.next_trigger_oid(txn).await?;
        }

        let key = self.key(&encode_trigger_key(&def.table, def.name.as_str()));
        if txn.get(key.clone()).await?.is_some() {
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

    pub async fn get_trigger(
        &self,
        txn: &mut Transaction,
        table_full_name: &str,
        trigger_name: &str,
    ) -> Result<Option<TriggerDef>> {
        let key = self.key(&encode_trigger_key(table_full_name, trigger_name));
        match txn.get(key).await? {
            Some(data) => {
                let mut def: TriggerDef = bincode::deserialize(&data)
                    .context("Failed to deserialize trigger definition")?;
                if def.oid == 0 {
                    def.oid = self.next_trigger_oid(txn).await?;
                    let data = bincode::serialize(&def)
                        .context("Failed to serialize trigger definition")?;
                    txn_put(
                        txn,
                        self.key(&encode_trigger_key(table_full_name, trigger_name)),
                        data,
                    )
                    .await?;
                }
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    pub async fn list_triggers(&self, txn: &mut Transaction) -> Result<Vec<TriggerDef>> {
        let prefix = encode_trigger_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut triggers = Vec::new();
        for pair in pairs {
            let mut def: TriggerDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize trigger")?;
            if def.oid == 0 {
                def.oid = self.next_trigger_oid(txn).await?;
                let data = bincode::serialize(&def).context("Failed to serialize trigger")?;
                txn_put(
                    txn,
                    self.key(&encode_trigger_key(&def.table, def.name.as_str())),
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
        table_full_name: &str,
    ) -> Result<Vec<TriggerDef>> {
        let prefix = encode_trigger_table_prefix(table_full_name);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut triggers = Vec::new();
        for pair in pairs {
            let def: TriggerDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize trigger")?;
            triggers.push(def);
        }
        Ok(triggers)
    }

    pub async fn drop_trigger(
        &self,
        txn: &mut Transaction,
        table_full_name: &str,
        trigger_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_trigger_key(table_full_name, trigger_name));
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

    /// Scan index by a prefix of the index values to get PKs.
    ///
    /// This is used for composite indexes when only the leading columns are constrained.
    /// For unique indexes, the PK is stored in the value. For non-unique indexes, the PK is
    /// stored as a suffix in the key after all index values and a separator byte.
    pub async fn scan_index_prefix(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        index_id: u64,
        prefix_values: &[Value],
        unique: bool,
        index_column_types: &[DataType],
        pk_types: &[DataType],
    ) -> Result<Vec<Vec<Value>>> {
        if pk_types.is_empty() {
            return Err(anyhow!("PK types required for index scan"));
        }

        let prefix = encode_index_key(table_id, index_id, prefix_values, None);
        let start_key = self.key(&prefix);

        let mut end_raw = prefix;
        end_raw.push(0xFF);
        let end_key = self.key(&end_raw);

        let range: BoundRange = (start_key..end_key).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut pks = Vec::new();
        if unique {
            for pair in pairs {
                let pk_bytes: &[u8] = pair.value().as_ref();
                let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
                pks.push(pk);
            }
            return Ok(pks);
        }

        let fixed_prefix_len = encode_index_key(table_id, index_id, &[], None).len();
        for pair in pairs {
            let full_key: &[u8] = pair.key().as_ref().into();
            if full_key.len() <= fixed_prefix_len {
                continue;
            }

            let mut offset = fixed_prefix_len;
            for data_type in index_column_types {
                let (_, consumed) = decode_value_memcomparable(&full_key[offset..], data_type)?;
                offset += consumed;
            }

            if full_key.get(offset) != Some(&0x01) {
                return Err(anyhow!("Non-unique index key missing PK separator"));
            }
            offset += 1;

            let pk_bytes = &full_key[offset..];
            let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
            pks.push(pk);
        }

        Ok(pks)
    }

    /// Create GIN-like inverted index entries for a row.
    ///
    /// Each `token_hash` is stored as a separate key that points to `pk_values` via the
    /// key suffix. The value is empty.
    pub async fn create_gin_index_entries(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        index_id: u64,
        token_hashes: &[u64],
        pk_values: &[Value],
    ) -> Result<()> {
        if token_hashes.is_empty() {
            return Ok(());
        }

        let pk_key = encode_pk_values(pk_values);
        for &token_hash in token_hashes {
            let key = self.key(&encode_gin_index_key(table_id, index_id, token_hash, &pk_key));
            txn_put(txn, key, Vec::new()).await?;
        }
        Ok(())
    }

    /// Delete GIN-like inverted index entries for a row.
    pub async fn delete_gin_index_entries(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        index_id: u64,
        token_hashes: &[u64],
        pk_values: &[Value],
    ) -> Result<()> {
        if token_hashes.is_empty() {
            return Ok(());
        }

        let pk_key = encode_pk_values(pk_values);
        for &token_hash in token_hashes {
            let key = self.key(&encode_gin_index_key(table_id, index_id, token_hash, &pk_key));
            txn_delete(txn, key).await?;
        }
        Ok(())
    }

    /// Scan a GIN-like inverted index for rows matching **all** token hashes.
    ///
    /// This returns encoded PK keys (the same bytes used in `t_{table_id}_{pk}` keys)
    /// to allow callers to fetch rows without decoding/re-encoding PK values.
    pub async fn scan_gin_index_intersection(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        index_id: u64,
        token_hashes: &[u64],
    ) -> Result<Vec<Vec<u8>>> {
        if token_hashes.is_empty() {
            return Ok(Vec::new());
        }

        let mut candidates: HashSet<Vec<u8>> = HashSet::new();

        for (i, &token_hash) in token_hashes.iter().enumerate() {
            if i > 0 && candidates.is_empty() {
                break;
            }

            let (start_raw, end_raw) = encode_gin_index_token_range(table_id, index_id, token_hash);
            let start_key = self.key(&start_raw);
            let end_key = self.key(&end_raw);
            let range: BoundRange = (start_key.clone()..end_key).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;

            if i == 0 {
                for pair in pairs {
                    let full_key: &[u8] = pair.key().as_ref().into();
                    if full_key.len() <= start_key.len() {
                        continue;
                    }
                    candidates.insert(full_key[start_key.len()..].to_vec());
                }
            } else {
                let mut next: HashSet<Vec<u8>> = HashSet::with_capacity(candidates.len());
                for pair in pairs {
                    let full_key: &[u8] = pair.key().as_ref().into();
                    if full_key.len() <= start_key.len() {
                        continue;
                    }
                    let pk_bytes = &full_key[start_key.len()..];
                    if candidates.contains(pk_bytes) {
                        next.insert(pk_bytes.to_vec());
                    }
                }
                candidates = next;
            }
        }

        let mut out: Vec<Vec<u8>> = candidates.into_iter().collect();
        out.sort();
        Ok(out)
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

    /// Batch get rows by their encoded PK keys.
    ///
    /// `pk_keys` are the raw bytes produced by `encode_pk_values` for the table's PK.
    pub async fn batch_get_rows_by_pk_keys(
        &self,
        txn: &mut Transaction,
        table_id: u64,
        pk_keys: Vec<Vec<u8>>,
    ) -> Result<Vec<Row>> {
        let mut rows = Vec::new();
        for pk_key in &pk_keys {
            let data_key = self.key(&encode_data_key(table_id, pk_key));
            if let Some(val) = txn.get(data_key).await? {
                rows.push(deserialize_row(&val)?);
            }
        }
        Ok(rows)
    }

    pub async fn create_view(&self, txn: &mut Transaction, name: &str, query: &str) -> Result<()> {
        let key = self.key(&encode_view_key(name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("View '{}' already exists", name));
        }

        let (schema, view_name) = name.split_once('.').unwrap_or(("public", name));
        let oid = self.next_view_oid(txn).await?;
        let def = ViewDef {
            oid,
            schema: schema.to_string(),
            name: view_name.to_string(),
            query: query.to_string(),
        };
        let data = bincode::serialize(&def).context("Failed to serialize view definition")?;
        txn_put(txn, key, data).await?;
        info!("Created view '{}'", name);
        Ok(())
    }

    pub async fn get_view(&self, txn: &mut Transaction, name: &str) -> Result<Option<ViewDef>> {
        let key = self.key(&encode_view_key(name));
        match txn.get(key.clone()).await? {
            Some(data) => match bincode::deserialize::<ViewDef>(&data) {
                Ok(mut def) => {
                    if def.oid == 0 {
                        def.oid = self.next_view_oid(txn).await?;
                        let updated =
                            bincode::serialize(&def).context("Failed to serialize view")?;
                        txn_put(txn, key, updated).await?;
                    }
                    Ok(Some(def))
                }
                Err(_) => {
                    let query = String::from_utf8(data)?;
                    let (schema, view_name) = name.split_once('.').unwrap_or(("public", name));
                    let oid = self.next_view_oid(txn).await?;
                    let def = ViewDef {
                        oid,
                        schema: schema.to_string(),
                        name: view_name.to_string(),
                        query,
                    };
                    let updated = bincode::serialize(&def).context("Failed to serialize view")?;
                    txn_put(txn, key, updated).await?;
                    Ok(Some(def))
                }
            },
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

    pub async fn list_views(&self, txn: &mut Transaction) -> Result<Vec<ViewDef>> {
        let prefix = encode_view_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut views = Vec::new();
        for pair in pairs {
            let key_bytes: &[u8] = pair.key().as_ref().into();
            if !key_bytes.starts_with(&prefix) {
                continue;
            }
            let name = String::from_utf8_lossy(&key_bytes[prefix.len()..]).to_string();
            let key = self.key(&encode_view_key(&name));

            match bincode::deserialize::<ViewDef>(pair.value()) {
                Ok(mut def) => {
                    if def.oid == 0 {
                        def.oid = self.next_view_oid(txn).await?;
                        let updated =
                            bincode::serialize(&def).context("Failed to serialize view")?;
                        txn_put(txn, key, updated).await?;
                    }
                    views.push(def);
                }
                Err(_) => {
                    let query = String::from_utf8_lossy(pair.value()).to_string();
                    let (schema, view_name) = name.split_once('.').unwrap_or(("public", &name));
                    let oid = self.next_view_oid(txn).await?;
                    let def = ViewDef {
                        oid,
                        schema: schema.to_string(),
                        name: view_name.to_string(),
                        query,
                    };
                    let updated = bincode::serialize(&def).context("Failed to serialize view")?;
                    txn_put(txn, key, updated).await?;
                    views.push(def);
                }
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
