use super::encoding::*;
use super::kv_stats;
use crate::txn::{txn_delete, txn_put};
use crate::types::{
    DataType, DatabaseDef, DefaultTablePrivilegeGrant, FunctionDef, Row, SequenceBacking,
    SequenceDef, SequenceState, TablePrivilegeGrant, TableSchema, TriggerDef, UserTypeDef, Value,
    ViewDef,
};
use crate::extensions::InstalledExtension;
use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tikv_client::{
    BoundRange, CheckLevel, Config, Key, Transaction, TransactionClient, TransactionOptions,
};
use tokio::sync::RwLock;
use tracing::{debug, info};

const SCHEMA_CACHE_TTL: Duration = Duration::from_secs(60);
const AUTOCOMMIT_MAX_RETRIES: usize = 10;

/// Maximum scan limit for TiKV operations.
const SCAN_LIMIT: u32 = u32::MAX;
const BATCH_GET_CHUNK_SIZE: usize = 256;

fn scan_limit_to_u32(limit: Option<usize>) -> u32 {
    match limit {
        Some(0) => 0,
        Some(n) => u32::try_from(n).unwrap_or(u32::MAX),
        None => SCAN_LIMIT,
    }
}

fn column_comment_table_prefix_v2(db_id: u64, table_full_name: &str) -> Vec<u8> {
    let mut prefix = encode_comment_prefix_v2(db_id);
    prefix.push(b'c');
    prefix.push(0);
    prefix.extend_from_slice(table_full_name.as_bytes());
    prefix.push(0);
    prefix
}

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

struct SchemaCache {
    per_db: HashMap<u64, PerDatabaseSchemaCache>,
}

struct PerDatabaseSchemaCache {
    schemas: Option<(Instant, Vec<String>)>,
    schema_oids: Option<(Instant, HashMap<String, u32>)>,
    tables: Option<(Instant, Vec<String>)>,
    triggers: HashMap<String, (Instant, Vec<TriggerDef>)>,
    functions: HashMap<String, (Instant, Option<FunctionDef>)>,
}

impl SchemaCache {
    fn new() -> Self {
        Self {
            per_db: HashMap::new(),
        }
    }
}

impl PerDatabaseSchemaCache {
    fn new() -> Self {
        Self {
            schemas: None,
            schema_oids: None,
            tables: None,
            triggers: HashMap::new(),
            functions: HashMap::new(),
        }
    }
}

pub struct TikvStore {
    client: Arc<TransactionClient>,
    cache: Arc<RwLock<SchemaCache>>,
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
        let store = Self {
            client: Arc::new(client),
            cache: Arc::new(RwLock::new(SchemaCache::new())),
        };

        store.check_format_version().await?;
        store.bootstrap_default_database("admin").await?;

        Ok(store)
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

    /// Perform a single-key read/modify/write in its own auto-committed transaction.
    ///
    /// This is used to emulate PostgreSQL non-transactional semantics (e.g. sequences),
    /// where updates must survive caller transaction rollbacks and SAVEPOINT rollbacks.
    async fn autocommit_update_key<R>(
        &self,
        key: Vec<u8>,
        mut compute: impl FnMut(Option<Vec<u8>>) -> Result<(Option<Vec<u8>>, R)>,
    ) -> Result<R> {
        for attempt in 0..AUTOCOMMIT_MAX_RETRIES {
            let mut txn = self.begin_optimistic().await?;
            let current = txn.get(key.clone()).await?;
            let (new_value, result) = compute(current)?;

            match new_value {
                Some(val) => txn.put(key.clone(), val).await.map_err(|e| anyhow!(e))?,
                None => txn.delete(key.clone()).await.map_err(|e| anyhow!(e))?,
            }

            match txn.commit().await {
                Ok(_) => return Ok(result),
                Err(e) => {
                    let _ = txn.rollback().await;
                    debug!(
                        "autocommit update failed (attempt {} of {}): {}",
                        attempt + 1,
                        AUTOCOMMIT_MAX_RETRIES,
                        e
                    );
                }
            }
        }

        Err(anyhow!(
            "autocommit update failed after {} attempts",
            AUTOCOMMIT_MAX_RETRIES
        ))
    }

    /// Check or initialize the on-disk storage format version for this keyspace.
    ///
    /// This is a breaking change boundary: storage format v2 introduces per-database key
    /// partitioning. If an existing keyspace contains v1 table metadata keys, we refuse to
    /// initialize v2 and require a re-initialize/migration.
    pub async fn check_format_version(&self) -> Result<()> {
        const STORAGE_FORMAT_VERSION: u32 = 2;

        let mut txn = self.begin().await?;
        let key = self.key(&encode_format_version_key());

        match txn.get(key.clone()).await? {
            Some(data) => {
                let bytes: [u8; 4] = data
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow!("Invalid format version value"))?;
                let version = u32::from_be_bytes(bytes);
                if version != STORAGE_FORMAT_VERSION {
                    txn.rollback().await.ok();
                    return Err(anyhow!(
                        "Incompatible storage format: found v{}, expected v{}. \
                         Please re-initialize the keyspace or migrate data.",
                        version,
                        STORAGE_FORMAT_VERSION
                    ));
                }
                txn.rollback().await.ok();
                Ok(())
            }
            None => {
                // New keyspace (or a legacy v1 keyspace). Refuse to auto-upgrade if we detect
                // v1 table metadata keys.
                let has_v1_tables = txn
                    .get(self.key(&encode_next_table_id_key()))
                    .await?
                    .is_some()
                    || self.prefix_has_any(&mut txn, encode_schema_prefix()).await?;

                if has_v1_tables {
                    txn.rollback().await.ok();
                    return Err(anyhow!(
                        "Storage format v1 detected (no format marker, but v1 table keys exist). \
                         This build requires storage format v2. Please re-initialize the keyspace."
                    ));
                }

                txn_put(
                    &mut txn,
                    key,
                    STORAGE_FORMAT_VERSION.to_be_bytes().to_vec(),
                )
                .await?;
                txn.commit().await?;
                Ok(())
            }
        }
    }

    /// Ensure the default `postgres` database exists (storage format v2).
    pub async fn bootstrap_default_database(&self, owner: &str) -> Result<()> {
        const DEFAULT_DB: &str = "postgres";

        let mut txn = self.begin().await?;
        if self.get_database_id(&mut txn, DEFAULT_DB).await?.is_some() {
            txn.rollback().await.ok();
            return Ok(());
        }

        let db_id = self.next_database_id(&mut txn).await?;
        let def = DatabaseDef::default_postgres(db_id, owner.to_string());

        let name_key = self.key(&encode_database_name_key(DEFAULT_DB));
        txn_put(&mut txn, name_key, db_id.to_be_bytes().to_vec()).await?;

        let id_key = self.key(&encode_database_id_key(db_id));
        let data = bincode::serialize(&def).context("Failed to serialize database definition")?;
        txn_put(&mut txn, id_key, data).await?;

        txn.commit().await?;
        info!("Bootstrapped default database 'postgres' with ID {}", db_id);
        Ok(())
    }

    /// Allocate and persist the next database ID (storage format v2).
    pub async fn next_database_id(&self, txn: &mut Transaction) -> Result<u64> {
        const FIRST_DATABASE_ID: u64 = 1;

        let key = self.key(&encode_next_database_id_key());
        let current = txn.get(key.clone()).await?;
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
        match txn.get(key).await? {
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

    /// Fetch a database definition by ID (storage format v2).
    pub async fn get_database_by_id(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Option<DatabaseDef>> {
        let key = self.key(&encode_database_id_key(db_id));
        match txn.get(key).await? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize database definition")?,
            )),
            None => Ok(None),
        }
    }

    /// List all databases in the current keyspace (storage format v2).
    pub async fn list_databases(&self, txn: &mut Transaction) -> Result<Vec<DatabaseDef>> {
        let prefix = encode_database_id_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

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
        if txn.get(name_key.clone()).await?.is_some() {
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
    ) -> Result<Option<u64>> {
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
        let db_id = match txn.get(name_key.clone()).await? {
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

        txn_delete(txn, name_key).await?;

        let id_key = self.key(&encode_database_id_key(db_id));
        txn_delete(txn, id_key).await?;

        Ok(Some(db_id))
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
        let db_id = match txn.get(old_key.clone()).await? {
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
        if txn.get(new_key.clone()).await?.is_some() {
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
        let db_id = match txn.get(name_key).await? {
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
        self.client
            .unsafe_destroy_range(range)
            .await
            .map_err(|e| anyhow!(e))
    }

    pub async fn lock_rows(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        rows: &[Row],
    ) -> Result<()> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!(
                "cannot lock rows: table '{}' has no primary key",
                table_name
            ));
        }
        let keys: Vec<Vec<u8>> = rows
            .iter()
            .map(|row| {
                let pk_values = schema.get_pk_values(row);
                let row_key = encode_pk_values(&pk_values);
                self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key))
            })
            .collect();
        txn.lock_keys(keys).await.map_err(|e| anyhow!(e))
    }

    pub async fn lock_rows_skip_locked(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        rows: &[Row],
        max_locks: Option<usize>,
    ) -> Result<Vec<usize>> {
        fn is_key_locked_error(err: &tikv_client::Error) -> bool {
            match err {
                tikv_client::Error::PessimisticLockError { inner, .. } => is_key_locked_error(inner.as_ref()),
                tikv_client::Error::ExtractedErrors(errors)
                | tikv_client::Error::MultipleKeyErrors(errors) => errors.iter().any(is_key_locked_error),
                tikv_client::Error::KeyError(key_error) => {
                    key_error.locked.is_some() || key_error.conflict.is_some()
                }
                _ => false,
            }
        }

        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!(
                "cannot lock rows: table '{}' has no primary key",
                table_name
            ));
        }

        let max_locks = max_locks.unwrap_or(usize::MAX);
        let mut locked_indices = Vec::new();

        for (idx, row) in rows.iter().enumerate() {
            if locked_indices.len() >= max_locks {
                break;
            }

            let pk_values = schema.get_pk_values(row);
            let row_key = encode_pk_values(&pk_values);
            let key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));

            match txn.lock_keys(vec![key]).await {
                Ok(()) => locked_indices.push(idx),
                Err(err) => {
                    if is_key_locked_error(&err) {
                        continue;
                    }
                    return Err(anyhow!(err));
                }
            }
        }

        Ok(locked_indices)
    }

    /// Check if a table exists (using txn)
    pub async fn table_exists(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_schema_key_v2(db_id, table_name));
        let exists = txn.get(key).await?.is_some();
        Ok(exists)
    }

    fn is_builtin_schema(schema: &str) -> bool {
        matches!(schema, "public" | "pg_catalog" | "information_schema" | "extensions")
    }

    pub async fn schema_exists(&self, txn: &mut Transaction, db_id: u64, schema: &str) -> Result<bool> {
        if Self::is_builtin_schema(schema) {
            return Ok(true);
        }
        let key = self.key(&encode_schema_def_key_v2(db_id, schema));
        Ok(txn.get(key).await?.is_some())
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
        self.invalidate_schema_cache(db_id).await;
        Ok(true)
    }

    pub async fn invalidate_schema_cache(&self, db_id: u64) {
        let mut cache = self.cache.write().await;
        if let Some(entry) = cache.per_db.get_mut(&db_id) {
            entry.schemas = None;
            entry.schema_oids = None;
        }
    }

    pub async fn invalidate_table_cache(&self, db_id: u64) {
        let mut cache = self.cache.write().await;
        if let Some(entry) = cache.per_db.get_mut(&db_id) {
            entry.tables = None;
        }
    }

    pub async fn invalidate_trigger_cache(&self, db_id: u64, table_full_name: &str) {
        let mut cache = self.cache.write().await;
        if let Some(entry) = cache.per_db.get_mut(&db_id) {
            entry.triggers.remove(table_full_name);
        }
    }

    pub async fn invalidate_function_cache(&self, db_id: u64, full_name: &str) {
        let mut cache = self.cache.write().await;
        if let Some(entry) = cache.per_db.get_mut(&db_id) {
            entry.functions.remove(full_name);
        }
    }

    pub async fn list_schema_oids(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<HashMap<String, u32>> {
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.per_db.get(&db_id) {
                if let Some((ts, oids)) = &entry.schema_oids {
                    if ts.elapsed() < SCHEMA_CACHE_TTL {
                        return Ok(oids.clone());
                    }
                }
            }
        }

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

        {
            let mut cache = self.cache.write().await;
            cache.per_db
                .entry(db_id)
                .or_insert_with(PerDatabaseSchemaCache::new)
                .schema_oids = Some((Instant::now(), oids.clone()));
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
        self.invalidate_schema_cache(db_id).await;
        Ok(true)
    }

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

        // Avoid stale cached table lists during CASCADE cleanup.
        self.invalidate_table_cache(db_id).await;

        for view in self.list_views(txn, db_id).await? {
            if view.schema == schema {
                let _ = self.drop_view(txn, db_id, &view.full_name()).await?;
            }
        }

        for matview in self.list_materialized_views(txn, db_id).await? {
            if matview.starts_with(&schema_prefix) {
                let _ = self.drop_materialized_view(txn, db_id, &matview).await?;
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

        self.drop_schema_restrict(txn, db_id, schema, if_exists).await?;
        Ok(true)
    }

    pub async fn list_schemas(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<String>> {
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.per_db.get(&db_id) {
                if let Some((ts, schemas)) = &entry.schemas {
                    if ts.elapsed() < SCHEMA_CACHE_TTL {
                        info!(
                            "list_schemas: cache HIT ({} schemas, age {}ms)",
                            schemas.len(),
                            ts.elapsed().as_millis()
                        );
                        return Ok(schemas.clone());
                    }
                }
            }
        }
        info!("list_schemas: cache MISS");

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

        {
            let mut cache = self.cache.write().await;
            cache.per_db
                .entry(db_id)
                .or_insert_with(PerDatabaseSchemaCache::new)
                .schemas = Some((Instant::now(), schemas.clone()));
        }

        Ok(schemas)
    }

    pub async fn get_extension(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        ext_name: &str,
    ) -> Result<Option<InstalledExtension>> {
        let key = self.key(&encode_extension_key_v2(db_id, ext_name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize extension")?,
            )),
            None => Ok(None),
        }
    }

    pub async fn put_extension(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        ext: &InstalledExtension,
    ) -> Result<()> {
        let key = self.key(&encode_extension_key_v2(db_id, &ext.name));
        let data = bincode::serialize(ext).context("Failed to serialize extension")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn drop_extension(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        ext_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_extension_key_v2(db_id, ext_name));
        let existed = txn.get(key.clone()).await?.is_some();
        if !existed {
            return Ok(false);
        }

        txn_delete(txn, key).await?;
        let cfg_key = self.key(&encode_extension_config_key_v2(db_id, ext_name));
        let _ = txn_delete(txn, cfg_key).await;
        let comment_key = self.key(&encode_comment_extension_key_v2(db_id, ext_name));
        txn_delete(txn, comment_key).await?;
        Ok(true)
    }

    pub async fn list_extensions(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<InstalledExtension>> {
        let prefix = encode_extension_prefix_v2(db_id);
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
        db_id: u64,
        ext_name: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let key = self.key(&encode_comment_extension_key_v2(db_id, ext_name));
        match comment {
            Some(text) => txn_put(txn, key, text.as_bytes().to_vec()).await,
            None => txn_delete(txn, key).await,
        }
    }

    /// Set or clear a comment for a function (`schema.name`).
    pub(crate) async fn set_function_comment(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        func_full_name: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let key = self.key(&encode_comment_function_key_v2(db_id, func_full_name));
        match comment {
            Some(text) => txn_put(txn, key, text.as_bytes().to_vec()).await,
            None => txn_delete(txn, key).await,
        }
    }

    /// Set or clear a comment for a table (`schema.name`).
    pub(crate) async fn set_table_comment(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let key = self.key(&encode_comment_table_key_v2(db_id, table_full_name));
        match comment {
            Some(text) => txn_put(txn, key, text.as_bytes().to_vec()).await,
            None => txn_delete(txn, key).await,
        }
    }

    /// Set or clear a comment for a table column.
    pub(crate) async fn set_column_comment(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        column_name: &str,
        comment: Option<&str>,
    ) -> Result<()> {
        let key = self.key(&encode_comment_column_key_v2(db_id, table_full_name, column_name));
        match comment {
            Some(text) => txn_put(txn, key, text.as_bytes().to_vec()).await,
            None => txn_delete(txn, key).await,
        }
    }

    /// List all stored comments for the current tenant.
    pub(crate) async fn list_comments(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<CommentRecord>> {
        let prefix = encode_comment_prefix_v2(db_id);
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

    pub async fn next_schema_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        const FIRST_USER_SCHEMA_OID: u32 = 20000;

        let key = self.key(&encode_next_schema_oid_key_v2(db_id));
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
    pub async fn next_table_id(&self, _txn: &mut Transaction, db_id: u64) -> Result<u64> {
        let key = self.key(&encode_next_table_id_key_v2(db_id));
        self.autocommit_update_key(key, |current| {
            let next_val = match current {
                Some(data) => {
                    let id = u64::from_be_bytes(
                        data.try_into().map_err(|_| anyhow!("Invalid ID format"))?,
                    );
                    id.checked_add(1).ok_or_else(|| anyhow!("Table ID overflow"))?
                }
                None => 1,
            };
            Ok((Some(next_val.to_be_bytes().to_vec()), next_val))
        })
        .await
    }

    pub async fn next_type_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        const FIRST_USER_TYPE_OID: u32 = 20000;

        let key = self.key(&encode_next_type_oid_key_v2(db_id));
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

    pub async fn next_function_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        const FIRST_FUNCTION_OID: u32 = 1;

        let key = self.key(&encode_next_function_oid_key_v2(db_id));
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

    pub async fn next_trigger_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        const FIRST_TRIGGER_OID: u32 = 1;

        let key = self.key(&encode_next_trigger_oid_key_v2(db_id));
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

    pub async fn next_view_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        const FIRST_VIEW_OID: u32 = 1;

        let key = self.key(&encode_next_view_oid_key_v2(db_id));
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
        self.autocommit_update_key(key, |_current| {
            Ok((Some(value.to_be_bytes().to_vec()), ()))
        })
        .await
    }

    pub async fn create_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: TableSchema,
    ) -> Result<()> {
        let schema_key = self.key(&encode_schema_key_v2(db_id, &schema.name));
        if txn.get(schema_key.clone()).await?.is_some() {
            let short_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
            return Err(anyhow!("relation \"{}\" already exists", short_name));
        }
        let schema_data = serialize_schema(&schema)?;
        txn_put(txn, schema_key, schema_data).await?;
        info!(
            "Created table '{}' with ID {}",
            schema.name, schema.table_id
        );
        self.invalidate_table_cache(db_id).await;
        Ok(())
    }

    /// Get a table schema by name
    pub async fn get_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
    ) -> Result<Option<TableSchema>> {
        let key = self.key(&encode_schema_key_v2(db_id, table_name));
        let val = txn.get(key).await?;
        match val {
            Some(data) => Ok(Some(deserialize_schema(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn get_table_privileges(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
    ) -> Result<Vec<TablePrivilegeGrant>> {
        let key = self.key(&encode_table_privileges_key_v2(db_id, table_full_name));
        match txn.get(key).await? {
            Some(data) => Ok(bincode::deserialize(&data)?),
            None => Ok(Vec::new()),
        }
    }

    pub async fn grant_table_privilege(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        grant: TablePrivilegeGrant,
    ) -> Result<()> {
        let key = self.key(&encode_table_privileges_key_v2(db_id, table_full_name));
        let mut grants: Vec<TablePrivilegeGrant> = match txn.get(key.clone()).await? {
            Some(data) => bincode::deserialize(&data)?,
            None => Vec::new(),
        };
        grants.retain(|g| !(g.grantee == grant.grantee && g.privilege_type == grant.privilege_type));
        grants.push(grant);
        let data = bincode::serialize(&grants)?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn revoke_table_privilege(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        grantee: &str,
        privilege_type: &str,
    ) -> Result<()> {
        let key = self.key(&encode_table_privileges_key_v2(db_id, table_full_name));
        let Some(data) = txn.get(key.clone()).await? else {
            return Ok(());
        };
        let mut grants: Vec<TablePrivilegeGrant> = bincode::deserialize(&data)?;
        grants.retain(|g| !(g.grantee == grantee && g.privilege_type == privilege_type));
        if grants.is_empty() {
            txn_delete(txn, key).await?;
        } else {
            txn_put(txn, key, bincode::serialize(&grants)?).await?;
        }
        Ok(())
    }

    pub async fn get_default_table_privileges(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        owner: &str,
        schema: Option<&str>,
    ) -> Result<Vec<DefaultTablePrivilegeGrant>> {
        let key = self.key(&encode_default_table_privileges_key_v2(db_id, owner, schema));
        match txn.get(key).await? {
            Some(data) => Ok(bincode::deserialize(&data)?),
            None => Ok(Vec::new()),
        }
    }

    pub async fn grant_default_table_privilege(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        owner: &str,
        schema: Option<&str>,
        grant: DefaultTablePrivilegeGrant,
    ) -> Result<()> {
        let key = self.key(&encode_default_table_privileges_key_v2(db_id, owner, schema));
        let mut grants: Vec<DefaultTablePrivilegeGrant> = match txn.get(key.clone()).await? {
            Some(data) => bincode::deserialize(&data)?,
            None => Vec::new(),
        };
        grants.retain(|g| !(g.grantee == grant.grantee && g.privilege_type == grant.privilege_type));
        grants.push(grant);
        txn_put(txn, key, bincode::serialize(&grants)?).await?;
        Ok(())
    }

    pub async fn revoke_default_table_privilege(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        owner: &str,
        schema: Option<&str>,
        grantee: &str,
        privilege_type: &str,
    ) -> Result<()> {
        let key = self.key(&encode_default_table_privileges_key_v2(db_id, owner, schema));
        let Some(data) = txn.get(key.clone()).await? else {
            return Ok(());
        };
        let mut grants: Vec<DefaultTablePrivilegeGrant> = bincode::deserialize(&data)?;
        grants.retain(|g| !(g.grantee == grantee && g.privilege_type == privilege_type));
        if grants.is_empty() {
            txn_delete(txn, key).await?;
        } else {
            txn_put(txn, key, bincode::serialize(&grants)?).await?;
        }
        Ok(())
    }

    pub async fn delete_default_table_privileges_for_owner(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        owner: &str,
    ) -> Result<()> {
        let prefix = encode_default_table_privileges_key_v2(db_id, owner, None);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (self.key(&prefix)..self.key(&end)).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        for pair in pairs {
            txn_delete(txn, pair.into_key().into()).await?;
        }
        Ok(())
    }

    pub async fn drop_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
    ) -> Result<bool> {
        let schema_opt = self.get_schema(txn, db_id, table_name).await?;
        if let Some(schema) = schema_opt {
            let schema_key = self.key(&encode_schema_key_v2(db_id, table_name));
            txn_delete(txn, schema_key).await?;
            let (raw_start, raw_end) = encode_table_data_range_v2(db_id, schema.table_id);
            let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                txn_delete(txn, pair.into_key().into()).await?;
            }

            let (raw_start, raw_end) = encode_table_index_range_v2(db_id, schema.table_id);
            let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                txn_delete(txn, pair.into_key().into()).await?;
            }

            // Remove the per-table sequence counter (used by TableId-backed sequences),
            // but only if no sequence definition still references this table_id.
            //
            // `ALTER SEQUENCE ... OWNED BY NONE` preserves the sequence while leaving it
            // backed by the historical per-table `_sys_seq_ + table_id` key.
            let table_id = schema.table_id;
            let has_table_id_sequence = self
                .list_sequences(txn, db_id)
                .await?
                .iter()
                .any(|def| matches!(&def.backing, SequenceBacking::TableId(id) if *id == table_id));
            if !has_table_id_sequence {
                let seq_key = self.key(&encode_table_sequence_value_key_v2(db_id, table_id));
                txn_delete(txn, seq_key).await?;
            }

            // Comments are stored under name-keyed keys; ensure they do not resurrect after
            // DROP + recreate with the same name.
            txn_delete(
                txn,
                self.key(&encode_comment_table_key_v2(db_id, table_name)),
            )
            .await?;
            self.delete_column_comments_for_table(txn, db_id, table_name)
                .await?;

            // Table privileges are stored under a dedicated key to support `information_schema.table_privileges`.
            txn_delete(
                txn,
                self.key(&encode_table_privileges_key_v2(db_id, table_name)),
            )
            .await?;

            info!("Dropped table '{}'", table_name);
            self.invalidate_table_cache(db_id).await;
            self.invalidate_trigger_cache(db_id, table_name).await;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Insert a row into a table
    pub async fn insert(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        row: Row,
    ) -> Result<Vec<Value>> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if row.values.len() != schema.columns.len() {
            return Err(anyhow!("Column count mismatch"));
        }
        let pk_values = schema.get_pk_values(&row);
        let row_key = encode_pk_values(&pk_values);
        let data_key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));
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
        Ok(pk_values)
    }

    /// Upsert a row into a table
    pub async fn upsert(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        row: Row,
    ) -> Result<()> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let pk_values = schema.get_pk_values(&row);
        let row_key = encode_pk_values(&pk_values);
        let data_key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));
        let row_data = serialize_row(&row)?;
        txn_put(txn, data_key, row_data).await?;
        debug!("Upserted row into '{}'", table_name);
        Ok(())
    }

    /// Upsert a row into a table with an explicit physical PK value.
    ///
    /// This is required for tables without an explicit primary key, where the
    /// physical row key is not derivable from row values.
    pub async fn upsert_by_pk(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        pk_values: &[Value],
        row: Row,
    ) -> Result<()> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if row.values.len() != schema.columns.len() {
            return Err(anyhow!("Column count mismatch"));
        }
        let row_key = encode_pk_values(pk_values);
        let data_key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));
        let row_data = serialize_row(&row)?;
        txn_put(txn, data_key, row_data).await?;
        debug!("Upserted row into '{}' (explicit PK)", table_name);
        Ok(())
    }

    /// Scan all rows from a table, returning both the row key and row value.
    pub async fn scan_with_keys(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
    ) -> Result<Vec<(Vec<u8>, Row)>> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let (raw_start, raw_end) = encode_table_data_range_v2(db_id, schema.table_id);
        let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
        let pairs: Vec<_> = txn.scan(range, SCAN_LIMIT).await?.collect();
        let mut rows = Vec::with_capacity(pairs.len());
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            let row = deserialize_row(pair.value())?;
            rows.push((key.to_vec(), row));
        }
        Ok(rows)
    }

    /// Scan all rows from a table
    pub async fn scan(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        limit: Option<usize>,
    ) -> Result<Vec<Row>> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let (raw_start, raw_end) = encode_table_data_range_v2(db_id, schema.table_id);
        let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
        let pairs = txn.scan(range, scan_limit_to_u32(limit)).await?;
        let mut rows = Vec::new();
        let mut scanned_pairs = 0usize;
        for pair in pairs {
            scanned_pairs += 1;
            let row = deserialize_row(pair.value())?;
            rows.push(row);
        }
        kv_stats::record_table_scan_pairs(scanned_pairs);
        debug!("Scanned {} rows from '{}'", rows.len(), table_name);
        Ok(rows)
    }

    /// Delete rows matching a simple condition
    pub async fn delete_by_pk(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        pk_values: &[Value],
    ) -> Result<u64> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let row_key = encode_pk_values(pk_values);
        let data_key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));
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
        db_id: u64,
        table_name: &str,
        pk_values: &[Value],
    ) -> Result<Option<Row>> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let row_key = encode_pk_values(pk_values);
        let data_key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));
        match txn.get(data_key).await? {
            Some(data) => Ok(Some(deserialize_row(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn list_tables(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<String>> {
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.per_db.get(&db_id) {
                if let Some((ts, tables)) = &entry.tables {
                    if ts.elapsed() < SCHEMA_CACHE_TTL {
                        info!(
                            "list_tables: cache HIT ({} tables, age {}ms)",
                            tables.len(),
                            ts.elapsed().as_millis()
                        );
                        return Ok(tables.clone());
                    }
                }
            }
        }
        info!("list_tables: cache MISS");

        let prefix = encode_schema_prefix_v2(db_id);
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

        {
            let mut cache = self.cache.write().await;
            cache.per_db
                .entry(db_id)
                .or_insert_with(PerDatabaseSchemaCache::new)
                .tables = Some((Instant::now(), tables.clone()));
        }

        Ok(tables)
    }

    pub async fn create_type(&self, txn: &mut Transaction, db_id: u64, def: UserTypeDef) -> Result<()> {
        let full_name = format!("{}.{}", def.schema, def.name);
        let key = self.key(&encode_type_key_v2(db_id, &full_name));
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
        db_id: u64,
        full_name: &str,
    ) -> Result<Option<UserTypeDef>> {
        let key = self.key(&encode_type_key_v2(db_id, full_name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize type definition")?,
            )),
            None => Ok(None),
        }
    }

    pub async fn list_types(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<UserTypeDef>> {
        let prefix = encode_type_prefix_v2(db_id);
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

    pub async fn drop_type(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_type_key_v2(db_id, full_name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            Ok(true)
        } else {
            Ok(false)
        }
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
            return Err(anyhow!("Sequence '{}' already exists", full_name));
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
                if def.oid == 0 {
                    if let Some(backfilled) =
                        self.autocommit_backfill_sequence_oid(db_id, full_name).await?
                    {
                        let mut def = backfilled;
                        if def.start_value == 0 {
                            def.start_value = def.min_value;
                        }
                        if def.cache_size == 0 {
                            def.cache_size = 1;
                        }
                        return Ok(Some(def));
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
                    let data = bincode::serialize(&def)
                        .context("Failed to serialize sequence definition")?;
                    txn_put(txn, self.key(&encode_sequence_def_key_v2(db_id, full_name)), data).await?;
                }
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

            let mut def: SequenceDef = bincode::deserialize(&data)
                .context("Failed to deserialize sequence definition")?;

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
            let data = bincode::serialize(&def).context("Failed to serialize sequence definition")?;
            txn.put(def_key.clone(), data).await.map_err(|e| anyhow!(e))?;

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
            if def.oid == 0 {
                if let Some(backfilled) =
                    self.autocommit_backfill_sequence_oid(db_id, &def.full_name())
                        .await?
                {
                    let mut def = backfilled;
                    if def.start_value == 0 {
                        def.start_value = def.min_value;
                    }
                    if def.cache_size == 0 {
                        def.cache_size = 1;
                    }
                    sequences.push(def);
                    continue;
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
                let data = bincode::serialize(&def).context("Failed to serialize sequence")?;
                txn_put(txn, self.key(&encode_sequence_def_key_v2(db_id, &def.full_name())), data).await?;
            }
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
                    txn_put(txn, self.key(&encode_function_key_v2(db_id, full_name)), data).await?;
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
                txn_put(txn, self.key(&encode_function_key_v2(db_id, &full_name)), data).await?;
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
        db_id: u64,
        table_full_name: &str,
        trigger_name: &str,
    ) -> Result<Option<TriggerDef>> {
        let key = self.key(&encode_trigger_key_v2(db_id, table_full_name, trigger_name));
        match txn.get(key).await? {
            Some(data) => {
                let mut def: TriggerDef = bincode::deserialize(&data)
                    .context("Failed to deserialize trigger definition")?;
                if def.oid == 0 {
                    def.oid = self.next_trigger_oid(txn, db_id).await?;
                    let data = bincode::serialize(&def)
                        .context("Failed to serialize trigger definition")?;
                    txn_put(
                        txn,
                        self.key(&encode_trigger_key_v2(db_id, table_full_name, trigger_name)),
                        data,
                    )
                    .await?;
                }
                Ok(Some(def))
            }
            None => Ok(None),
        }
    }

    pub async fn list_triggers(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<TriggerDef>> {
        let prefix = encode_trigger_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

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
        // 1. Check cache (read lock)
        {
            let cache = self.cache.read().await;
            if let Some(db_cache) = cache.per_db.get(&db_id) {
                if let Some((cached_at, triggers)) = db_cache.triggers.get(table_full_name) {
                    if cached_at.elapsed() < SCHEMA_CACHE_TTL {
                        return Ok(triggers.clone());
                    }
                }
            }
        }

        // 2. Cache miss or expired — scan TiKV
        let prefix = encode_trigger_table_prefix_v2(db_id, table_full_name);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut triggers = Vec::new();
        for pair in pairs {
            let def: TriggerDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize trigger")?;
            if def.table != table_full_name {
                continue;
            }
            triggers.push(def);
        }

        // 3. Populate cache (write lock)
        {
            let mut cache = self.cache.write().await;
            cache
                .per_db
                .entry(db_id)
                .or_insert_with(PerDatabaseSchemaCache::new)
                .triggers
                .insert(
                    table_full_name.to_string(),
                    (Instant::now(), triggers.clone()),
                );
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
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            Ok(true)
        } else {
            Ok(false)
        }
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
            .ok_or_else(|| anyhow!("Sequence '{}' does not exist", full_name))?;

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
                        &full_name,
                        increment,
                        min_value,
                        max_value,
                        is_cycled,
                        &mut state,
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
            .ok_or_else(|| anyhow!("Sequence '{}' does not exist", full_name))?;

        self.maybe_migrate_implicit_sequence_to_standalone(txn, db_id, &mut def)
            .await?;

        match &def.backing {
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
                self.set_sequence_value(txn, db_id, *table_id, stored).await?;
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

    /// Truncate a table
    pub async fn truncate_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
    ) -> Result<bool> {
        let schema_opt = self.get_schema(txn, db_id, table_name).await?;
        if let Some(schema) = schema_opt {
            let (raw_start, raw_end) = encode_table_data_range_v2(db_id, schema.table_id);
            let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                txn_delete(txn, pair.into_key().into()).await?;
            }

            let (raw_start, raw_end) = encode_table_index_range_v2(db_id, schema.table_id);
            let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                txn_delete(txn, pair.into_key().into()).await?;
            }
            info!("Truncated table '{}'", table_name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Update table schema
    pub async fn update_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: TableSchema,
    ) -> Result<()> {
        let schema_key = self.key(&encode_schema_key_v2(db_id, &schema.name));
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
        db_id: u64,
        old_table: &str,
        new_table: &str,
    ) -> Result<()> {
        if old_table == new_table {
            return Ok(());
        }

        let old_key = self.key(&encode_schema_key_v2(db_id, old_table));
        let new_key = self.key(&encode_schema_key_v2(db_id, new_table));

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

    /// Rewrite name-keyed metadata for a renamed table.
    ///
    /// This updates (moves) trigger keys, table/column comment keys, and rewrites
    /// `SequenceDef.owned_by` string references that point at the renamed table.
    pub async fn rename_table_metadata(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        old_table: &str,
        new_table: &str,
    ) -> Result<()> {
        if old_table == new_table {
            return Ok(());
        }

        self.rename_table_triggers(txn, db_id, old_table, new_table)
            .await?;
        self.rename_table_comments(txn, db_id, old_table, new_table)
            .await?;
        self.rewrite_sequences_owned_by_table(txn, db_id, old_table, new_table)
            .await?;
        Ok(())
    }

    /// Rewrite name-keyed metadata for a renamed column.
    ///
    /// This updates column comment keys and rewrites `SequenceDef.owned_by` column
    /// references for sequences owned by the renamed column.
    pub async fn rename_column_metadata(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        old_column: &str,
        new_column: &str,
    ) -> Result<()> {
        if old_column == new_column {
            return Ok(());
        }

        self.rename_column_comment(txn, db_id, table_full_name, old_column, new_column)
            .await?;
        self.rewrite_sequences_owned_by_column(
            txn,
            db_id,
            table_full_name,
            old_column,
            new_column,
        )
        .await?;
        Ok(())
    }

    fn plan_trigger_rename_ops(
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

    fn validate_trigger_rename_puts(
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

    async fn rename_table_triggers(
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
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
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
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

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

        Self::validate_trigger_rename_puts(old_table, new_table, &puts, &old_keys, &existing_triggers)?;

        for (key, data) in puts {
            txn_put(txn, key, data).await?;
        }

        for key in deletes {
            txn_delete(txn, key).await?;
        }
        Ok(())
    }

    async fn rename_table_comments(
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
        if let Some(value) = txn.get(old_table_key.clone()).await? {
            let new_table_key = self.key(&encode_comment_table_key_v2(db_id, new_table));
            txn_put(txn, new_table_key, value).await?;
            txn_delete(txn, old_table_key).await?;
        }

        // Move column comments, if present.
        let old_prefix = column_comment_table_prefix_v2(db_id, old_table);
        let mut end = old_prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (old_prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
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

    async fn rename_column_comment(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        old_column: &str,
        new_column: &str,
    ) -> Result<()> {
        let old_key = self.key(&encode_comment_column_key_v2(db_id, table_full_name, old_column));
        let Some(value) = txn.get(old_key.clone()).await? else {
            return Ok(());
        };
        let new_key = self.key(&encode_comment_column_key_v2(db_id, table_full_name, new_column));
        txn_put(txn, new_key, value).await?;
        txn_delete(txn, old_key).await?;
        Ok(())
    }

    async fn delete_column_comments_for_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
    ) -> Result<()> {
        let prefix = column_comment_table_prefix_v2(db_id, table_full_name);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        for pair in pairs {
            txn_delete(txn, pair.into_key().into()).await?;
        }
        Ok(())
    }

    async fn rewrite_sequences_owned_by_table(
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

    async fn rewrite_sequences_owned_by_column(
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

    /// Create an index entry
    pub async fn create_index_entry(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        pk_values: &[Value],
        unique: bool,
    ) -> Result<()> {
        if unique {
            let idx_key = self.key(&encode_index_key_v2(db_id, table_id, index_id, values, None));
            if txn.get(idx_key.clone()).await?.is_some() {
                return Err(anyhow!("Duplicate entry for unique index"));
            }
            let idx_val = encode_pk_values(pk_values);
            txn_put(txn, idx_key, idx_val).await?;
        } else {
            let idx_key = self.key(&encode_index_key_v2(
                db_id,
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
        db_id: u64,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        pk_values: &[Value],
        unique: bool,
    ) -> Result<()> {
        if unique {
            let idx_key = self.key(&encode_index_key_v2(db_id, table_id, index_id, values, None));
            txn_delete(txn, idx_key).await?;
        } else {
            let idx_key = self.key(&encode_index_key_v2(
                db_id,
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
        db_id: u64,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        unique: bool,
        pk_types: &[DataType],
        limit: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        if pk_types.is_empty() {
            return Err(anyhow!("PK types required for index scan"));
        }

        if matches!(limit, Some(0)) {
            return Ok(Vec::new());
        }

        if unique {
            let idx_key = self.key(&encode_index_key_v2(db_id, table_id, index_id, values, None));
            if let Some(val) = txn.get(idx_key).await? {
                let pk = decode_pk_from_index_suffix(&val, pk_types)?;
                Ok(vec![pk])
            } else {
                Ok(vec![])
            }
        } else {
            let prefix = encode_index_key_v2(db_id, table_id, index_id, values, None);

            let mut start_raw = prefix.clone();
            start_raw.push(0x01);
            let start_key = self.key(&start_raw);

            let mut end_raw = prefix;
            end_raw.push(0x02);
            let end_key = self.key(&end_raw);

            let range: BoundRange = (start_key.clone()..end_key).into();
            let pairs = txn.scan(range, scan_limit_to_u32(limit)).await?;

            let mut pks = Vec::new();
            let mut scanned_pairs = 0usize;
            for pair in pairs {
                scanned_pairs += 1;
                let full_key: &[u8] = pair.key().as_ref().into();
                if full_key.len() <= start_key.len() {
                    continue;
                }
                let pk_bytes = &full_key[start_key.len()..];
                let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
                pks.push(pk);
            }
            kv_stats::record_index_scan_pairs(scanned_pairs);
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
        db_id: u64,
        table_id: u64,
        index_id: u64,
        prefix_values: &[Value],
        unique: bool,
        index_column_types: &[DataType],
        pk_types: &[DataType],
        limit: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        if pk_types.is_empty() {
            return Err(anyhow!("PK types required for index scan"));
        }

        if matches!(limit, Some(0)) {
            return Ok(Vec::new());
        }

        let prefix = encode_index_key_v2(db_id, table_id, index_id, prefix_values, None);
        let start_key = self.key(&prefix);

        let mut end_raw = prefix;
        end_raw.push(0xFF);
        let end_key = self.key(&end_raw);

        let range: BoundRange = (start_key..end_key).into();
        let pairs = txn.scan(range, scan_limit_to_u32(limit)).await?;

        let mut pks = Vec::new();
        if unique {
            let mut scanned_pairs = 0usize;
            for pair in pairs {
                scanned_pairs += 1;
                let pk_bytes: &[u8] = pair.value().as_ref();
                let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
                pks.push(pk);
            }
            kv_stats::record_index_scan_pairs(scanned_pairs);
            return Ok(pks);
        }

        let fixed_prefix_len = encode_index_key_v2(db_id, table_id, index_id, &[], None).len();
        let mut scanned_pairs = 0usize;
        for pair in pairs {
            scanned_pairs += 1;
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

        kv_stats::record_index_scan_pairs(scanned_pairs);
        Ok(pks)
    }

    /// Create GIN-like inverted index entries for a row.
    ///
    /// Each `token_hash` is stored as a separate key that points to `pk_values` via the
    /// key suffix. The value is empty.
    pub async fn create_gin_index_entries(
        &self,
        txn: &mut Transaction,
        db_id: u64,
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
            let key =
                self.key(&encode_gin_index_key_v2(db_id, table_id, index_id, token_hash, &pk_key));
            txn_put(txn, key, Vec::new()).await?;
        }
        Ok(())
    }

    /// Delete GIN-like inverted index entries for a row.
    pub async fn delete_gin_index_entries(
        &self,
        txn: &mut Transaction,
        db_id: u64,
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
            let key =
                self.key(&encode_gin_index_key_v2(db_id, table_id, index_id, token_hash, &pk_key));
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
        db_id: u64,
        table_id: u64,
        index_id: u64,
        token_hashes: &[u64],
    ) -> Result<Vec<Vec<u8>>> {
        if token_hashes.is_empty() {
            return Ok(Vec::new());
        }

        // Probe posting sizes to scan the smallest posting list first.
        //
        // This avoids materializing a high-cardinality token into `candidates` (memory spikes),
        // and increases the chance we can early-exit before scanning large postings when the
        // intersection becomes empty.
        const GIN_POSTING_PROBE_LIMIT: u32 = 4096;

        struct TokenProbe {
            orig_pos: usize,
            token_hash: u64,
            estimated_postings: usize,
            cached_pk_keys: Option<Vec<Vec<u8>>>,
        }

        fn pk_suffix<'a>(full_key: &'a [u8], prefix_len: usize) -> Option<&'a [u8]> {
            if full_key.len() <= prefix_len {
                None
            } else {
                Some(&full_key[prefix_len..])
            }
        }

        let probe_scan_limit = GIN_POSTING_PROBE_LIMIT.saturating_add(1);
        let mut probes = Vec::with_capacity(token_hashes.len());
        for (orig_pos, &token_hash) in token_hashes.iter().enumerate() {
            let (start_raw, end_raw) =
                encode_gin_index_token_range_v2(db_id, table_id, index_id, token_hash);
            let start_key = self.key(&start_raw);
            let end_key = self.key(&end_raw);
            let prefix_len = start_key.len();

            let range: BoundRange = (start_key.clone()..end_key).into();
            let keys: Vec<_> = txn.scan_keys(range, probe_scan_limit).await?.collect();
            kv_stats::record_gin_scan_keys(keys.len());
            let estimated_postings = keys.len();

            let cached_pk_keys = if estimated_postings < probe_scan_limit as usize {
                let mut pk_keys = Vec::with_capacity(estimated_postings);
                for key in keys {
                    let full_key: &[u8] = key.as_ref().into();
                    let Some(pk_bytes) = pk_suffix(full_key, prefix_len) else {
                        continue;
                    };
                    pk_keys.push(pk_bytes.to_vec());
                }
                Some(pk_keys)
            } else {
                None
            };

            probes.push(TokenProbe {
                orig_pos,
                token_hash,
                estimated_postings,
                cached_pk_keys,
            });
        }

        probes.sort_by_key(|p| (p.estimated_postings, p.orig_pos));

        let mut candidates: HashSet<Vec<u8>> = HashSet::with_capacity(
            probes
                .first()
                .map(|p| p.estimated_postings)
                .unwrap_or_default(),
        );

        for (i, probe) in probes.into_iter().enumerate() {
            if i > 0 && candidates.is_empty() {
                break;
            }

            if let Some(pk_keys) = probe.cached_pk_keys {
                if i == 0 {
                    candidates.extend(pk_keys);
                } else {
                    let mut next: HashSet<Vec<u8>> = HashSet::with_capacity(candidates.len());
                    for pk_key in pk_keys {
                        if candidates.contains(&pk_key) {
                            next.insert(pk_key);
                        }
                    }
                    candidates = next;
                }
                continue;
            }

            let (start_raw, end_raw) =
                encode_gin_index_token_range_v2(db_id, table_id, index_id, probe.token_hash);
            let start_key = self.key(&start_raw);
            let end_key = self.key(&end_raw);
            let prefix_len = start_key.len();

            if i == 0 {
                let range: BoundRange = (start_key..end_key).into();
                let keys = txn.scan_keys(range, SCAN_LIMIT).await?;
                let mut scanned_keys = 0usize;
                for key in keys {
                    scanned_keys += 1;
                    let full_key: &[u8] = key.as_ref().into();
                    let Some(pk_bytes) = pk_suffix(full_key, prefix_len) else {
                        continue;
                    };
                    candidates.insert(pk_bytes.to_vec());
                }
                kv_stats::record_gin_scan_keys(scanned_keys);
            } else {
                let mut next: HashSet<Vec<u8>> = HashSet::with_capacity(candidates.len());
                let range: BoundRange = (start_key..end_key).into();
                let keys = txn.scan_keys(range, SCAN_LIMIT).await?;
                let mut scanned_keys = 0usize;
                for key in keys {
                    scanned_keys += 1;
                    let full_key: &[u8] = key.as_ref().into();
                    let Some(pk_bytes) = pk_suffix(full_key, prefix_len) else {
                        continue;
                    };
                    if candidates.contains(pk_bytes) {
                        next.insert(pk_bytes.to_vec());
                    }
                }
                kv_stats::record_gin_scan_keys(scanned_keys);
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
        db_id: u64,
        table_id: u64,
        pks: Vec<Vec<Value>>,
        _schema: &TableSchema,
    ) -> Result<Vec<Row>> {
        let mut rows = Vec::with_capacity(pks.len());
        let mut data_keys: Vec<Vec<u8>> = Vec::with_capacity(BATCH_GET_CHUNK_SIZE);

        for pk in &pks {
            let row_key = encode_pk_values(pk);
            data_keys.push(self.key(&encode_data_key_v2(db_id, table_id, &row_key)));

            if data_keys.len() >= BATCH_GET_CHUNK_SIZE {
                self.batch_get_rows_by_data_keys(txn, &data_keys, &mut rows)
                    .await?;
                data_keys.clear();
            }
        }

        if !data_keys.is_empty() {
            self.batch_get_rows_by_data_keys(txn, &data_keys, &mut rows)
                .await?;
        }

        Ok(rows)
    }

    /// Batch get rows by their encoded PK keys.
    ///
    /// `pk_keys` are the raw bytes produced by `encode_pk_values` for the table's PK.
    pub async fn batch_get_rows_by_pk_keys(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        pk_keys: Vec<Vec<u8>>,
    ) -> Result<Vec<Row>> {
        let mut rows = Vec::with_capacity(pk_keys.len());
        let mut data_keys: Vec<Vec<u8>> = Vec::with_capacity(BATCH_GET_CHUNK_SIZE);

        for pk_key in &pk_keys {
            data_keys.push(self.key(&encode_data_key_v2(db_id, table_id, pk_key)));

            if data_keys.len() >= BATCH_GET_CHUNK_SIZE {
                self.batch_get_rows_by_data_keys(txn, &data_keys, &mut rows)
                    .await?;
                data_keys.clear();
            }
        }

        if !data_keys.is_empty() {
            self.batch_get_rows_by_data_keys(txn, &data_keys, &mut rows)
                .await?;
        }

        Ok(rows)
    }

    async fn batch_get_rows_by_data_keys(
        &self,
        txn: &mut Transaction,
        data_keys: &[Vec<u8>],
        out: &mut Vec<Row>,
    ) -> Result<()> {
        kv_stats::record_batch_get_keys(data_keys.len());
        let pairs = txn.batch_get(data_keys.iter().cloned()).await?;
        let mut by_key: HashMap<Key, tikv_client::Value> = HashMap::with_capacity(data_keys.len());

        for pair in pairs {
            let tikv_client::KvPair(key, value) = pair;
            by_key.insert(key, value);
        }

        for key in data_keys {
            let key_ref: &Key = key.into();
            if let Some(val) = by_key.get(key_ref) {
                out.push(deserialize_row(val)?);
            }
        }

        Ok(())
    }

    pub async fn create_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        query: &str,
        or_replace: bool,
    ) -> Result<()> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            if !or_replace {
                return Err(anyhow!("View '{}' already exists", name));
            }

            let mut def = self
                .get_view(txn, db_id, name)
                .await?
                .ok_or_else(|| anyhow!("View '{}' does not exist", name))?;
            def.query = query.to_string();
            let data =
                bincode::serialize(&def).context("Failed to serialize view definition")?;
            txn_put(txn, key, data).await?;
            info!("Replaced view '{}'", name);
            return Ok(());
        }

        let (schema, view_name) = name.split_once('.').unwrap_or(("public", name));
        let oid = self.next_view_oid(txn, db_id).await?;
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

    pub async fn get_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<Option<ViewDef>> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        match txn.get(key.clone()).await? {
            Some(data) => match bincode::deserialize::<ViewDef>(&data) {
                Ok(mut def) => {
                    if def.oid == 0 {
                        def.oid = self.next_view_oid(txn, db_id).await?;
                        let updated =
                            bincode::serialize(&def).context("Failed to serialize view")?;
                        txn_put(txn, key, updated).await?;
                    }
                    Ok(Some(def))
                }
                Err(_) => {
                    let query = String::from_utf8(data)?;
                    let (schema, view_name) = name.split_once('.').unwrap_or(("public", name));
                    let oid = self.next_view_oid(txn, db_id).await?;
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

    pub async fn drop_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_view_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            info!("Dropped view '{}'", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn list_views(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<ViewDef>> {
        let prefix = encode_view_prefix_v2(db_id);
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
            let key = self.key(&encode_view_key_v2(db_id, &name));

            match bincode::deserialize::<ViewDef>(pair.value()) {
                Ok(mut def) => {
                    if def.oid == 0 {
                        def.oid = self.next_view_oid(txn, db_id).await?;
                        let updated =
                            bincode::serialize(&def).context("Failed to serialize view")?;
                        txn_put(txn, key, updated).await?;
                    }
                    views.push(def);
                }
                Err(_) => {
                    let query = String::from_utf8_lossy(pair.value()).to_string();
                    let (schema, view_name) = name.split_once('.').unwrap_or(("public", &name));
                    let oid = self.next_view_oid(txn, db_id).await?;
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
        db_id: u64,
        name: &str,
        query: &str,
    ) -> Result<()> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
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
        db_id: u64,
        name: &str,
    ) -> Result<Option<String>> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(String::from_utf8(data)?)),
            None => Ok(None),
        }
    }

    pub async fn drop_materialized_view(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_matview_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            info!("Dropped materialized view '{}'", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    #[allow(dead_code)]
    pub async fn list_materialized_views(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<String>> {
        let prefix = encode_matview_prefix_v2(db_id);
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
        db_id: u64,
        name: &str,
        definition: &str,
    ) -> Result<()> {
        let key = self.key(&encode_procedure_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Procedure '{}' already exists", name));
        }
        txn_put(txn, key, definition.as_bytes().to_vec()).await?;
        info!("Created procedure '{}'", name);
        Ok(())
    }

    pub async fn get_procedure(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<Option<String>> {
        let key = self.key(&encode_procedure_key_v2(db_id, name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(String::from_utf8(data)?)),
            None => Ok(None),
        }
    }

    pub async fn drop_procedure(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_procedure_key_v2(db_id, name));
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
        db_id: u64,
        name: &str,
        definition: &str,
    ) -> Result<()> {
        let key = self.key(&encode_procedure_key_v2(db_id, name));
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

#[cfg(test)]
mod trigger_rename_tests {
    use super::*;

    #[test]
    fn trigger_rename_keeps_reused_old_keys() {
        let db_id = 1;
        let old_table = "public.t";
        let new_table = "public.t/a";

        let trigger_a = TriggerDef {
            oid: 1,
            schema: "public".to_string(),
            name: "a".to_string(),
            table: old_table.to_string(),
            timing: "BEFORE".to_string(),
            events: vec!["INSERT".to_string()],
            function: "public.f".to_string(),
        };
        let trigger_nested = TriggerDef {
            oid: 2,
            schema: "public".to_string(),
            name: "a/a".to_string(),
            table: old_table.to_string(),
            timing: "BEFORE".to_string(),
            events: vec!["INSERT".to_string()],
            function: "public.f".to_string(),
        };

        let old_key_a = encode_trigger_key_v2(db_id, old_table, "a");
        let old_key_nested = encode_trigger_key_v2(db_id, old_table, "a/a");

        let new_key_a = encode_trigger_key_v2(db_id, new_table, "a");
        let new_key_nested = encode_trigger_key_v2(db_id, new_table, "a/a");
        assert_eq!(new_key_a, old_key_nested);

        let (puts, deletes) = TikvStore::plan_trigger_rename_ops(
            db_id,
            old_table,
            new_table,
            vec![(old_key_a.clone(), trigger_a), (old_key_nested.clone(), trigger_nested)],
        )
        .unwrap();

        let put_keys: HashSet<Vec<u8>> = puts.iter().map(|(key, _)| key.clone()).collect();
        assert!(put_keys.contains(&new_key_a));
        assert!(put_keys.contains(&new_key_nested));

        assert!(deletes.contains(&old_key_a));
        assert!(!deletes.contains(&old_key_nested));

        for (key, data) in puts {
            let def: TriggerDef = bincode::deserialize(&data).unwrap();
            assert_eq!(def.table, new_table);
            assert_eq!(def.schema, "public");

            if key == new_key_a {
                assert_eq!(def.name, "a");
            } else if key == new_key_nested {
                assert_eq!(def.name, "a/a");
            } else {
                panic!("unexpected trigger key");
            }
        }
    }

    #[test]
    fn trigger_rename_allows_overwriting_reused_old_keys() {
        let db_id = 1;
        let old_table = "public.t";
        let new_table = "public.t/a";

        let trigger_a = TriggerDef {
            oid: 1,
            schema: "public".to_string(),
            name: "a".to_string(),
            table: old_table.to_string(),
            timing: "BEFORE".to_string(),
            events: vec!["INSERT".to_string()],
            function: "public.f".to_string(),
        };
        let trigger_nested = TriggerDef {
            oid: 2,
            schema: "public".to_string(),
            name: "a/a".to_string(),
            table: old_table.to_string(),
            timing: "BEFORE".to_string(),
            events: vec!["INSERT".to_string()],
            function: "public.f".to_string(),
        };

        let old_key_a = encode_trigger_key_v2(db_id, old_table, "a");
        let old_key_nested = encode_trigger_key_v2(db_id, old_table, "a/a");
        let new_key_a = encode_trigger_key_v2(db_id, new_table, "a");
        assert_eq!(new_key_a, old_key_nested);

        let (puts, _) = TikvStore::plan_trigger_rename_ops(
            db_id,
            old_table,
            new_table,
            vec![(old_key_a.clone(), trigger_a), (old_key_nested.clone(), trigger_nested.clone())],
        )
        .unwrap();

        let old_keys: HashSet<Vec<u8>> = vec![old_key_a, old_key_nested.clone()].into_iter().collect();
        let existing_triggers = HashMap::from([(old_key_nested, trigger_nested)]);

        TikvStore::validate_trigger_rename_puts(
            old_table,
            new_table,
            &puts,
            &old_keys,
            &existing_triggers,
        )
        .unwrap();
    }

    #[test]
    fn trigger_rename_errors_on_unrelated_key_collision() {
        let db_id = 1;
        let old_table = "public.other";
        let new_table = "public.t/a";

        let trigger_a = TriggerDef {
            oid: 1,
            schema: "public".to_string(),
            name: "a".to_string(),
            table: old_table.to_string(),
            timing: "BEFORE".to_string(),
            events: vec!["INSERT".to_string()],
            function: "public.f".to_string(),
        };

        let old_key = encode_trigger_key_v2(db_id, old_table, "a");
        let old_keys: HashSet<Vec<u8>> = vec![old_key.clone()].into_iter().collect();

        let (puts, _) = TikvStore::plan_trigger_rename_ops(
            db_id,
            old_table,
            new_table,
            vec![(old_key, trigger_a)],
        )
        .unwrap();

        let collision_key = encode_trigger_key_v2(db_id, "public.t", "a/a");
        let expected_new_key = encode_trigger_key_v2(db_id, new_table, "a");
        assert_eq!(collision_key, expected_new_key);

        let existing_triggers = HashMap::from([(
            collision_key,
            TriggerDef {
                oid: 99,
                schema: "public".to_string(),
                name: "a/a".to_string(),
                table: "public.t".to_string(),
                timing: "BEFORE".to_string(),
                events: vec!["INSERT".to_string()],
                function: "public.f".to_string(),
            },
        )]);

        let err = TikvStore::validate_trigger_rename_puts(
            old_table,
            new_table,
            &puts,
            &old_keys,
            &existing_triggers,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("would overwrite trigger"));
        assert!(err.contains("public.t"));
    }
}
