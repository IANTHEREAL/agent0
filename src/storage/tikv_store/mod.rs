use super::encoding::*;
use super::kv_stats;
use crate::extensions::InstalledExtension;
use crate::model::{
    DataType, DatabaseDef, FunctionDef, MatViewDef, MigrationRecord, RlsPolicy, Row,
    SequenceBacking, SequenceDef, SequenceState, TableSchema, TriggerDef, UserTypeDef, Value,
    ViewDef,
};
use crate::storage::backpressure::tikv_op;
use crate::txn::{txn_delete, txn_put};
use anyhow::{anyhow, Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tikv_client::{
    BoundRange, CheckLevel, Config, Key, Transaction, TransactionClient, TransactionOptions,
};
use tracing::{debug, info};

// Submodules
mod collations;
pub mod cron;
mod database;
pub(crate) mod ddl_journal;
mod extensions;
mod functions;
pub(crate) mod indexes;
mod migrations;
mod policies;
mod procedures;
mod schemas;
mod sequences;
mod statistics;
mod tables;
mod text_search;
mod triggers;
mod types;
mod views;
pub mod worker;
pub use cron::CronRunClaimStatus;
pub use ddl_journal::{DdlJournalEntry, DdlOperation};

// Import helper functions for tests
#[cfg(test)]
use sequences::{nextval_standalone, setval_standalone};

const AUTOCOMMIT_MAX_RETRIES: usize = 10;

/// Maximum scan limit for TiKV operations.
const SCAN_LIMIT: u32 = u32::MAX;
const BATCH_GET_CHUNK_SIZE: usize = 256;
/// Batch size for paginated table scans / deletes to avoid exceeding gRPC message size limits.
const TABLE_SCAN_BATCH_SIZE: u32 = 1024;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommentTarget {
    Extension {
        name: String,
    },
    Function {
        full_name: String,
    },
    Table {
        full_name: String,
    },
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
    client: Option<Arc<TransactionClient>>,
    keyspace: Option<String>,
}

impl TikvStore {
    /// Returns a reference to the TiKV TransactionClient.
    /// Panics if called on a test stub (client is None).
    fn client(&self) -> &TransactionClient {
        self.client
            .as_ref()
            .expect("TikvStore: no client (test stub used in production code path?)")
    }

    /// Returns a shared reference to the TransactionClient, if available.
    /// Used by the embedded filesystem to create its own transactions.
    pub fn transaction_client(&self) -> Option<Arc<TransactionClient>> {
        self.client.clone()
    }

    /// Returns the keyspace this store is scoped to.
    pub fn keyspace(&self) -> Option<&str> {
        self.keyspace.as_deref()
    }

    pub async fn new_with_keyspace(
        pd_endpoints: Vec<String>,
        keyspace: Option<String>,
    ) -> Result<Self> {
        info!("Connecting to TiKV at {:?}", pd_endpoints);
        let mut config = match &keyspace {
            Some(ks) => {
                info!("Using TiKV Keyspace: {}", ks);
                Config::default().with_keyspace(ks)
            }
            None => Config::default(),
        };

        // Enable TLS for PD/TiKV connection if cert files are provided
        if let (Ok(ca), Ok(cert), Ok(key)) = (
            std::env::var("TIKV_CA_PATH"),
            std::env::var("TIKV_CERT_PATH"),
            std::env::var("TIKV_KEY_PATH"),
        ) {
            info!("TiKV TLS enabled: ca={}, cert={}, key={}", ca, cert, key);
            config = config.with_security(ca, cert, key);
        }
        let client = TransactionClient::new_with_config(pd_endpoints, config)
            .await
            .context("Failed to connect to TiKV")?;
        info!("Connected to TiKV. Keyspace: {:?}", keyspace);
        let store = Self {
            client: Some(Arc::new(client)),
            keyspace: keyspace.clone(),
        };

        store.check_format_version().await?;
        store.bootstrap_default_database("admin").await?;
        store.ensure_view_relation_bindings_migration().await?;
        store.ensure_no_pk_fk_cascade_migration().await?;

        Ok(store)
    }

    /// Create a TikvStore for the system worker keyspace.
    ///
    /// Unlike `new_with_keyspace()`, this does NOT bootstrap databases — it's raw KV access only.
    /// Used by the unified worker engine for cross-tenant task management.
    pub async fn new_system(pd_endpoints: Vec<String>, keyspace: &str) -> Result<Self> {
        info!(
            "Connecting to TiKV at {:?} for system keyspace",
            pd_endpoints
        );
        let mut config = Config::default().with_keyspace(keyspace);

        // Enable TLS for PD/TiKV connection if cert files are provided
        if let (Ok(ca), Ok(cert), Ok(key)) = (
            std::env::var("TIKV_CA_PATH"),
            std::env::var("TIKV_CERT_PATH"),
            std::env::var("TIKV_KEY_PATH"),
        ) {
            info!("TiKV TLS enabled: ca={}, cert={}, key={}", ca, cert, key);
            config = config.with_security(ca, cert, key);
        }

        let client = TransactionClient::new_with_config(pd_endpoints, config)
            .await
            .context("Failed to connect to TiKV")?;
        let store = Self {
            client: Some(Arc::new(client)),
            keyspace: Some(keyspace.to_string()),
        };

        store.check_format_version().await?;
        info!("Initialized system store for keyspace: {}", keyspace);

        Ok(store)
    }

    #[cfg(test)]
    pub(crate) fn new_stub() -> Arc<Self> {
        use std::sync::OnceLock;
        static STUB: OnceLock<Arc<TikvStore>> = OnceLock::new();
        STUB.get_or_init(|| {
            Arc::new(Self {
                client: None,
                keyspace: None,
            })
        })
        .clone()
    }

    fn key(&self, key: &[u8]) -> Vec<u8> {
        key.to_vec()
    }

    pub async fn begin(&self) -> Result<Transaction> {
        let options = TransactionOptions::new_pessimistic().drop_check(CheckLevel::Warn);
        tikv_op!(self.client().begin_with_options(options).await).map_err(|e| anyhow!(e))
    }

    pub async fn begin_optimistic(&self) -> Result<Transaction> {
        let options = TransactionOptions::new_optimistic().drop_check(CheckLevel::Warn);
        tikv_op!(self.client().begin_with_options(options).await).map_err(|e| anyhow!(e))
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
            let current = tikv_op!(txn.get(key.clone()).await)?;
            let (new_value, result) = compute(current)?;

            // SAFETY: Direct txn.put() intentionally bypasses savepoint undo tracking
            // to match PostgreSQL's non-transactional sequence semantics. Size guard
            // is applied inline via check_value_size() before each put.
            match new_value {
                Some(val) => {
                    crate::txn::check_value_size(&key, &val)?;
                    #[allow(clippy::disallowed_methods)]
                    tikv_op!(txn.put(key.clone(), val).await).map_err(|e| anyhow!(e))?
                }
                None => tikv_op!(txn.delete(key.clone()).await).map_err(|e| anyhow!(e))?,
            }

            match tikv_op!(txn.commit().await) {
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

        match tikv_op!(txn.get(key.clone()).await)? {
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
                let has_v1_tables = tikv_op!(txn.get(self.key(&encode_next_table_id_key())).await)?
                    .is_some()
                    || self
                        .prefix_has_any(&mut txn, encode_schema_prefix())
                        .await?;

                if has_v1_tables {
                    txn.rollback().await.ok();
                    return Err(anyhow!(
                        "Storage format v1 detected (no format marker, but v1 table keys exist). \
                         This build requires storage format v2. Please re-initialize the keyspace."
                    ));
                }

                txn_put(&mut txn, key, STORAGE_FORMAT_VERSION.to_be_bytes().to_vec()).await?;
                tikv_op!(txn.commit().await)?;
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

        tikv_op!(txn.commit().await)?;
        info!("Bootstrapped default database 'postgres' with ID {}", db_id);
        Ok(())
    }

    pub(crate) async fn prefix_has_any(
        &self,
        txn: &mut Transaction,
        prefix: Vec<u8>,
    ) -> Result<bool> {
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let mut pairs = tikv_op!(txn.scan(range, 1).await)?;
        Ok(pairs.next().is_some())
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
        let key = self.key(&encode_comment_column_key_v2(
            db_id,
            table_full_name,
            column_name,
        ));
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
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

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

            records.push(CommentRecord {
                target,
                description,
            });
        }

        Ok(records)
    }

    async fn next_oid_u32(
        &self,
        txn: &mut Transaction,
        key: Vec<u8>,
        first_value: u32,
        entity_name: &str,
    ) -> Result<u32> {
        let current = tikv_op!(txn.get(key.clone()).await)?;
        let next_val = match current {
            Some(data) => {
                let oid = u32::from_be_bytes(
                    data.try_into()
                        .map_err(|_| anyhow!("Invalid {} OID format", entity_name))?,
                );
                oid.checked_add(1)
                    .ok_or_else(|| anyhow!("{} OID overflow", entity_name))?
            }
            None => first_value,
        };
        txn_put(txn, key, next_val.to_be_bytes().to_vec()).await?;
        Ok(next_val)
    }

    pub async fn next_schema_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        self.next_oid_u32(
            txn,
            self.key(&encode_next_schema_oid_key_v2(db_id)),
            20000,
            "schema",
        )
        .await
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
                    id.checked_add(1)
                        .ok_or_else(|| anyhow!("Table ID overflow"))?
                }
                None => 1,
            };
            Ok((Some(next_val.to_be_bytes().to_vec()), next_val))
        })
        .await
    }

    pub async fn next_type_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        self.next_oid_u32(
            txn,
            self.key(&encode_next_type_oid_key_v2(db_id)),
            20000,
            "type",
        )
        .await
    }

    pub async fn next_function_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        self.next_oid_u32(
            txn,
            self.key(&encode_next_function_oid_key_v2(db_id)),
            1,
            "function",
        )
        .await
    }

    pub async fn next_trigger_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        self.next_oid_u32(
            txn,
            self.key(&encode_next_trigger_oid_key_v2(db_id)),
            1,
            "trigger",
        )
        .await
    }

    pub async fn next_view_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        self.next_oid_u32(
            txn,
            self.key(&encode_next_view_oid_key_v2(db_id)),
            1,
            "view",
        )
        .await
    }

    pub async fn next_policy_oid(&self, txn: &mut Transaction, db_id: u64) -> Result<u32> {
        self.next_oid_u32(
            txn,
            self.key(&encode_next_policy_oid_key_v2(db_id)),
            1,
            "policy",
        )
        .await
    }
}

#[cfg(test)]
mod sequence_tests {
    use super::*;

    #[test]
    fn test_standalone_nextval_rejects_zero_increment() {
        let mut state = SequenceState {
            last_value: 1,
            is_called: true,
        };

        let err = nextval_standalone("public.s", 0, 1, 10, false, &mut state)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid INCREMENT 0"));
    }

    #[test]
    fn test_standalone_nextval_supports_negative_increment_and_wrap() {
        let mut state = SequenceState {
            last_value: 1,
            is_called: true,
        };

        let wrapped = nextval_standalone("public.s", -1, 1, 5, true, &mut state).unwrap();
        assert_eq!(wrapped, 5);
        assert_eq!(state.last_value, 5);
    }

    #[test]
    fn test_standalone_setval_rejects_out_of_bounds() {
        let mut state = SequenceState {
            last_value: 1,
            is_called: true,
        };

        let err = setval_standalone("public.s", 1, 5, &mut state, 0, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("out of bounds"));
    }

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

    #[test]
    fn test_standalone_non_cycle_errors_on_min_bound_for_desc_sequence() {
        let mut state = SequenceState {
            last_value: 1,
            is_called: true,
        };

        let err = nextval_standalone("public.s", -1, 1, 10, false, &mut state)
            .unwrap_err()
            .to_string();
        assert!(err.contains("reached minimum value"));
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
            vec![
                (old_key_a.clone(), trigger_a),
                (old_key_nested.clone(), trigger_nested),
            ],
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
            vec![
                (old_key_a.clone(), trigger_a),
                (old_key_nested.clone(), trigger_nested.clone()),
            ],
        )
        .unwrap();

        let old_keys: HashSet<Vec<u8>> = vec![old_key_a, old_key_nested.clone()]
            .into_iter()
            .collect();
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

#[cfg(test)]
mod util_tests {
    use super::*;

    #[test]
    fn scan_limit_to_u32_handles_none_zero_and_overflow() {
        assert_eq!(scan_limit_to_u32(None), u32::MAX);
        assert_eq!(scan_limit_to_u32(Some(0)), 0);
        assert_eq!(scan_limit_to_u32(Some(42)), 42);
        assert_eq!(scan_limit_to_u32(Some((u32::MAX as usize) + 10)), u32::MAX);
    }

    #[test]
    fn column_comment_table_prefix_v2_shape_is_stable() {
        let db_id = 42;
        let table = "public.orders";
        let prefix = column_comment_table_prefix_v2(db_id, table);

        let mut expected = encode_comment_prefix_v2(db_id);
        expected.push(b'c');
        expected.push(0);
        expected.extend_from_slice(table.as_bytes());
        expected.push(0);

        assert_eq!(prefix, expected);
    }
}
