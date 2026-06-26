pub mod active_txn_registry;
pub mod config;
pub(crate) mod database_lifecycle;
pub mod engine;
pub mod gc;
pub mod metrics;
pub(crate) mod pd_region_stats;
pub mod types;

use crate::storage::retry::{
    is_retryable_tikv_transient_error, region_error_backoff, REGION_ERROR_MAX_RETRIES,
};
use crate::storage::TikvStore;
use crate::worker::types::{HnswS3DbPrefixCleanupIntent, TaskRegistryEntry};
use anyhow::{Context, Result};
use config::WorkerConfig;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tracing::{debug, info, warn};

pub(crate) use engine::{
    cleanup_hnsw_s3_graph_upload_after_failed_txn, hnsw_s3_graph_version_for_txn,
    put_hnsw_s3_graph_with_intent,
};

/// Canonical error message for a task aborted because its claim lease was lost,
/// stolen, or the engine is shutting down. This is the SAME string emitted by
/// `engine::run_with_guards`' cancellation arm, so the generic/cron path and the
/// specialized long-running paths share one cancellation contract / one error.
pub(crate) const CLAIM_CANCELLED_ERROR: &str = "cancelled by administrator";

/// Lease-cancellation checkpoint threaded into long-running, tenant-writing task
/// bodies (HNSW merge, CREATE INDEX CONCURRENTLY backfill, storage size scan).
///
/// It wraps the lease-renewer's `exec_shutdown` [`CancellationToken`]. The
/// renewer cancels that token the moment our claim lease is lost or stolen
/// (`spawn_claim_lease_renewer`). These specialized paths return BEFORE
/// `run_with_guards` — the only place that otherwise observes the token — so they
/// must check this checkpoint *before every tenant commit / txn rotation / phase
/// write*. On cancellation the in-flight work is abandoned WITHOUT committing and
/// the task is left for the new owner, preserving the at-most-once claim-lease
/// invariant.
///
/// Foreground DDL (synchronous `CREATE INDEX`, `CREATE TABLE`) has no lease and
/// constructs [`LeaseCancel::none`]; its checks are unconditional no-ops, so the
/// SAME commit choke point (`maybe_rotate_backfill_txn`) serves both paths.
#[derive(Clone, Default)]
pub struct LeaseCancel {
    token: Option<pgwire::tokio::CancellationToken>,
    /// Test-only fuse: cancel the token on the Nth `bail_if_cancelled` call.
    /// Lets behavioral tests fire cancellation at a SPECIFIC fence (e.g. the
    /// commit-adjacent one, AFTER the loop-top check has passed) without racing.
    /// `None` in all production constructions, so the production check below is a
    /// plain `is_cancelled` read.
    #[cfg(test)]
    trip_at_check: Option<std::sync::Arc<std::sync::atomic::AtomicI64>>,
}

impl LeaseCancel {
    /// No lease attached (foreground DDL). `bail_if_cancelled` is a no-op.
    pub fn none() -> Self {
        Self {
            token: None,
            #[cfg(test)]
            trip_at_check: None,
        }
    }

    /// Attach a lease-cancellation token (worker task path). A `None` argument
    /// degrades to [`LeaseCancel::none`] so callers can forward an optional
    /// signal without branching.
    pub fn new(token: Option<pgwire::tokio::CancellationToken>) -> Self {
        Self {
            token,
            #[cfg(test)]
            trip_at_check: None,
        }
    }

    /// Test-only constructor that arms a fuse: the token is cancelled exactly on
    /// the `nth` (1-based) `bail_if_cancelled` call. Checks 1..nth-1 pass; the nth
    /// check (and every check after) bails with the canonical claim-cancelled
    /// error. Use `nth = 2` to fire at the FIRST commit-adjacent fence in
    /// `execute_hnsw_merge` (TiKV path): check 1 = loop-top (passes), check 2 =
    /// the commit-adjacent fence immediately before `txn.commit()`.
    #[cfg(test)]
    pub fn new_tripping_at_check(nth: i64) -> Self {
        let token = pgwire::tokio::CancellationToken::new();
        Self {
            token: Some(token),
            trip_at_check: Some(std::sync::Arc::new(std::sync::atomic::AtomicI64::new(nth))),
        }
    }

    /// Return the canonical claim-cancelled error if the lease has been lost,
    /// stolen, or the engine is shutting down. Call this immediately before any
    /// tenant commit / txn rotation / phase write in a long-running task body.
    pub fn bail_if_cancelled(&self) -> Result<()> {
        #[cfg(test)]
        if let (Some(token), Some(counter)) = (&self.token, &self.trip_at_check) {
            // Fire the fuse on the configured check, then stay cancelled.
            if counter.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) <= 1 {
                token.cancel();
            }
        }
        if let Some(token) = &self.token {
            if token.is_cancelled() {
                return Err(anyhow::anyhow!(CLAIM_CANCELLED_ERROR));
            }
        }
        Ok(())
    }
}

const DATABASE_INVENTORY_PAGE_SIZE: usize = 256;

/// Always-on worker system store for `_sys_worker` metadata.
///
/// This handle is required by SQL-serving processes even when worker execution
/// is disabled, because foreground DDL/DML producers must still write durable
/// recovery metadata. Execution gating lives in `WORKER_EXECUTION_ENABLED`,
/// not in the presence of this handle.
static SYSTEM_STORE: OnceLock<Arc<TikvStore>> = OnceLock::new();
static WORKER_EXECUTION_ENABLED: AtomicBool = AtomicBool::new(false);

static WORKER_NOTIFY: OnceLock<Arc<tokio::sync::Notify>> = OnceLock::new();
static WORKER_METRICS: OnceLock<Arc<metrics::WorkerMetrics>> = OnceLock::new();
static ASYNC_TRIGGER_DEPTH_SAMPLER: OnceLock<Mutex<AsyncTriggerDepthSampler>> = OnceLock::new();

const ASYNC_TRIGGER_QUEUE_DEPTH_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);
const ASYNC_TRIGGER_QUEUE_DEPTH_IDLE_RETENTION: Duration = Duration::from_secs(60 * 60);

#[derive(Default)]
struct AsyncTriggerDepthSampler {
    dirty: HashSet<String>,
    last_attempted: HashMap<String, Instant>,
}

impl AsyncTriggerDepthSampler {
    fn mark_dirty(&mut self, keyspace: &str) {
        self.dirty.insert(keyspace.to_string());
    }

    fn due(&self, keyspace: &str, now: Instant) -> bool {
        self.last_attempted
            .get(keyspace)
            .and_then(|last| now.checked_duration_since(*last))
            .is_none_or(|elapsed| elapsed >= ASYNC_TRIGGER_QUEUE_DEPTH_SAMPLE_INTERVAL)
    }

    fn dirty_due_keyspaces(&self, now: Instant) -> Vec<String> {
        self.dirty
            .iter()
            .filter(|keyspace| self.due(keyspace, now))
            .cloned()
            .collect()
    }

    fn prune_idle(&mut self, now: Instant) {
        let dirty = &self.dirty;
        self.last_attempted.retain(|keyspace, last| {
            dirty.contains(keyspace)
                || now
                    .checked_duration_since(*last)
                    .is_none_or(|elapsed| elapsed < ASYNC_TRIGGER_QUEUE_DEPTH_IDLE_RETENTION)
        });
    }

    fn mark_attempted(&mut self, keyspace: &str, now: Instant) {
        self.prune_idle(now);
        self.last_attempted.insert(keyspace.to_string(), now);
    }

    fn mark_sampled(&mut self, keyspace: &str, now: Instant) {
        self.prune_idle(now);
        self.last_attempted.insert(keyspace.to_string(), now);
        self.dirty.remove(keyspace);
    }
}

fn async_trigger_depth_sampler() -> &'static Mutex<AsyncTriggerDepthSampler> {
    ASYNC_TRIGGER_DEPTH_SAMPLER.get_or_init(|| Mutex::new(AsyncTriggerDepthSampler::default()))
}

fn begin_async_trigger_queue_depth_sample(keyspace: &str, now: Instant) -> bool {
    let mut sampler = async_trigger_depth_sampler().lock();
    sampler.prune_idle(now);
    sampler.mark_dirty(keyspace);
    if !sampler.due(keyspace, now) {
        return false;
    }
    sampler.mark_attempted(keyspace, now);
    true
}

fn mark_async_trigger_queue_depth_sampled(keyspace: &str, now: Instant) {
    async_trigger_depth_sampler()
        .lock()
        .mark_sampled(keyspace, now);
}

pub(crate) fn async_trigger_queue_depth_dirty_keyspaces_due() -> Vec<String> {
    let now = Instant::now();
    async_trigger_depth_sampler()
        .lock()
        .dirty_due_keyspaces(now)
}

pub(crate) fn now_epoch_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Set the global system store. Called once during startup.
pub fn set_system_store(store: Arc<TikvStore>) {
    SYSTEM_STORE.set(store).ok(); // Ignore if already set
}

/// Get the global system store. Production startup sets this unconditionally;
/// tests may still observe None when they call producer helpers directly.
pub fn get_system_store() -> Option<&'static Arc<TikvStore>> {
    SYSTEM_STORE.get()
}

/// Get the always-on system store, failing closed if startup did not initialize
/// the required `_sys_worker` handle.
pub fn system_store() -> Result<&'static Arc<TikvStore>> {
    get_system_store().ok_or_else(|| {
        anyhow::anyhow!(
            "worker system store is not initialized; recovery metadata cannot be written"
        )
    })
}

/// Refresh the Prometheus async-trigger queue-depth gauge for one tenant.
///
/// Uses the V2 identity index plus active worker claims only; it never reads
/// command payloads. This keeps Prometheus queue depth independent from the SQL
/// diagnostics table while preserving the same tenant-scoped definition. Calls
/// from enqueue/finalize/tick paths are tenant-rate-limited to avoid turning the
/// gauge into a storage scan on every task transition.
pub(crate) async fn sample_async_trigger_queue_depth(
    system_store: &Arc<TikvStore>,
    keyspace: &str,
) -> Result<()> {
    let now = Instant::now();
    if !begin_async_trigger_queue_depth_sample(keyspace, now) {
        return Ok(());
    }
    let mut txn = system_store.begin().await?;
    let pending = system_store
        .pending_async_trigger_task_ids(&mut txn, keyspace)
        .await?
        .len() as u64;
    let mut processing = 0u64;
    for (key, claim) in system_store.list_worker_claims(&mut txn).await? {
        if claim.task_type == types::TaskType::AsyncTrigger
            && crate::storage::worker_claim_keyspace_matches(&key, keyspace)
        {
            processing = processing.saturating_add(1);
        }
    }
    txn.rollback().await.ok();
    crate::metrics::sample_trigger_queue_depth(keyspace, pending.saturating_add(processing));
    mark_async_trigger_queue_depth_sampled(keyspace, now);
    Ok(())
}

pub async fn request_hnsw_s3_db_prefix_cleanup(
    keyspace: &str,
    db_id: u64,
    drop_txn_start_ts: u64,
    reason: &str,
) -> Result<()> {
    let system_store = system_store()?;
    let intent = HnswS3DbPrefixCleanupIntent::new(
        keyspace.to_string(),
        db_id,
        drop_txn_start_ts,
        reason.to_string(),
    );
    let mut txn = system_store.begin().await?;
    system_store
        .put_hnsw_s3_db_prefix_cleanup_intent(&mut txn, &intent)
        .await?;
    txn.commit().await?;
    Ok(())
}

pub async fn complete_hnsw_s3_db_prefix_cleanup_for_dropped_db(
    keyspace: &str,
    db_id: u64,
) -> Result<u64> {
    let Some(s3) = crate::sql::hnsw::s3::hnsw_s3_client() else {
        return Ok(0);
    };
    let deleted = s3.delete_db_prefix(keyspace, db_id).await?;
    let system_store = system_store()?;
    let mut txn = system_store.begin().await?;
    system_store
        .delete_hnsw_s3_db_prefix_cleanup_intent(&mut txn, keyspace, db_id)
        .await?;
    txn.commit().await?;
    Ok(deleted)
}

pub fn set_worker_execution_enabled(enabled: bool) {
    WORKER_EXECUTION_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn execution_enabled() -> bool {
    WORKER_EXECUTION_ENABLED.load(Ordering::Relaxed)
}

pub fn canonical_registry_keyspace(keyspace: &str) -> String {
    if keyspace == "DEFAULT" {
        "default".to_string()
    } else {
        keyspace.to_string()
    }
}

async fn retry_worker_metadata_operation(
    operation: &'static str,
    attempt: u32,
    err: &anyhow::Error,
) -> bool {
    if attempt < REGION_ERROR_MAX_RETRIES && is_retryable_tikv_transient_error(err) {
        info!(
            attempt = attempt + 1,
            max_attempts = REGION_ERROR_MAX_RETRIES + 1,
            operation,
            "retrying worker metadata operation after transient TiKV error"
        );
        region_error_backoff(attempt).await;
        true
    } else {
        false
    }
}

pub async fn register_database_inventory(
    system_store: &TikvStore,
    tenant_store: &TikvStore,
    keyspace: &str,
) -> Result<usize> {
    let keyspace = canonical_registry_keyspace(keyspace);
    let mut cursor: Option<Vec<u8>> = None;
    let mut registered = 0usize;

    loop {
        let mut tenant_page = None;
        for attempt in 0..=REGION_ERROR_MAX_RETRIES {
            let result = async {
                let mut tenant_txn = tenant_store.begin().await?;
                let page = tenant_store
                    .scan_databases_page(
                        &mut tenant_txn,
                        cursor.as_deref(),
                        DATABASE_INVENTORY_PAGE_SIZE,
                    )
                    .await?;
                tenant_txn.rollback().await.ok();
                Ok(page)
            }
            .await;

            match result {
                Ok(page) => {
                    tenant_page = Some(page);
                    break;
                }
                Err(err) => {
                    if retry_worker_metadata_operation(
                        "register_database_inventory tenant scan",
                        attempt,
                        &err,
                    )
                    .await
                    {
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        let (databases, next_cursor) =
            tenant_page.expect("worker tenant inventory scan retry loop must return");

        if databases.is_empty() {
            break;
        }

        let mut system_added = None;
        for attempt in 0..=REGION_ERROR_MAX_RETRIES {
            let result = async {
                let mut sys_txn = system_store.begin().await?;
                let mut added = 0usize;
                for db in &databases {
                    if system_store
                        .get_worker_registry(&mut sys_txn, &keyspace, db.id)
                        .await?
                        .is_some()
                    {
                        continue;
                    }
                    system_store
                        .put_worker_registry(
                            &mut sys_txn,
                            &TaskRegistryEntry::new(keyspace.clone(), db.id),
                        )
                        .await?;
                    added += 1;
                }
                sys_txn.commit().await?;
                Ok(added)
            }
            .await;

            match result {
                Ok(added) => {
                    system_added = Some(added);
                    break;
                }
                Err(err) => {
                    if retry_worker_metadata_operation(
                        "register_database_inventory system registry",
                        attempt,
                        &err,
                    )
                    .await
                    {
                        continue;
                    }
                    return Err(err);
                }
            }
        }
        let added = system_added.expect("worker system inventory retry loop must return");
        registered += added;

        cursor = next_cursor;
        if cursor.is_none() {
            break;
        }
    }

    Ok(registered)
}

pub async fn ensure_database_inventory_row(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
) -> Result<bool> {
    let keyspace = canonical_registry_keyspace(keyspace);
    let mut sys_txn = system_store.begin().await?;
    let registered = if system_store
        .get_worker_registry(&mut sys_txn, &keyspace, db_id)
        .await?
        .is_none()
    {
        system_store
            .put_worker_registry(
                &mut sys_txn,
                &TaskRegistryEntry::new(keyspace.clone(), db_id),
            )
            .await?;
        true
    } else {
        false
    };
    sys_txn.commit().await?;
    Ok(registered)
}

pub async fn ensure_database_inventory_row_with_retry(
    system_store: &TikvStore,
    keyspace: &str,
    db_id: u64,
    attempts: usize,
) -> Result<bool> {
    let mut last_error = None;
    for attempt in 0..attempts.max(1) {
        match ensure_database_inventory_row(system_store, keyspace, db_id).await {
            Ok(registered) => return Ok(registered),
            Err(e) => {
                last_error = Some(e);
                if attempt + 1 < attempts.max(1) {
                    let backoff_ms = 25u64.saturating_mul(1u64 << attempt.min(5));
                    sleep(Duration::from_millis(backoff_ms)).await;
                }
            }
        }
    }
    Err(last_error.expect("at least one inventory registration attempt ran"))
}

pub fn set_worker_notify(notify: Arc<tokio::sync::Notify>) {
    WORKER_NOTIFY.set(notify).ok();
}

pub fn wake_worker() {
    if let Some(notify) = WORKER_NOTIFY.get() {
        notify.notify_one();
    }
}

pub fn set_worker_metrics(m: Arc<metrics::WorkerMetrics>) {
    WORKER_METRICS.set(m).ok();
}

pub fn get_worker_metrics() -> Option<&'static Arc<metrics::WorkerMetrics>> {
    WORKER_METRICS.get()
}

/// Build an HTTP client for PD API calls, with mutual TLS if configured.
pub(crate) fn build_pd_client() -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(5));

    if let (Ok(ca), Ok(cert_path), Ok(key_path)) = (
        std::env::var("TIKV_CA_PATH"),
        std::env::var("TIKV_CERT_PATH"),
        std::env::var("TIKV_KEY_PATH"),
    ) {
        let tls_config = build_pd_rustls_config(&ca, &cert_path, &key_path)?;
        builder = builder.use_preconfigured_tls(tls_config);
    }

    builder.build().context("failed to build PD HTTP client")
}

/// Build a rustls client config for PD mTLS directly.
///
/// `reqwest::tls::Identity::from_pem` with the rustls backend only accepts
/// PKCS#8 keys. The TiKV client secret can contain PKCS#1 or SEC1 keys, so use
/// `rustls_pemfile::private_key`, which accepts all three PEM key shapes.
fn build_pd_rustls_config(
    ca_path: &str,
    cert_path: &str,
    key_path: &str,
) -> Result<rustls::ClientConfig> {
    use rustls_pki_types::CertificateDer;

    let ca_pem =
        std::fs::read(ca_path).with_context(|| format!("failed to read PD CA cert: {ca_path}"))?;
    let mut ca_reader = std::io::BufReader::new(ca_pem.as_slice());
    let ca_certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut ca_reader)
        .collect::<std::result::Result<_, _>>()
        .context("failed to parse PD CA cert")?;
    if ca_certs.is_empty() {
        anyhow::bail!("no CA certificates found in {ca_path}");
    }
    let mut root_store = rustls::RootCertStore::empty();
    for cert in ca_certs {
        root_store
            .add(cert)
            .context("failed to register PD CA cert in rustls root store")?;
    }

    let cert_pem = std::fs::read(cert_path)
        .with_context(|| format!("failed to read PD client cert: {cert_path}"))?;
    let mut cert_reader = std::io::BufReader::new(cert_pem.as_slice());
    let client_certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<_, _>>()
        .context("failed to parse PD client cert")?;
    if client_certs.is_empty() {
        anyhow::bail!("no client certificates found in {cert_path}");
    }

    let key_pem = std::fs::read(key_path)
        .with_context(|| format!("failed to read PD client key: {key_path}"))?;
    let mut key_reader = std::io::BufReader::new(key_pem.as_slice());
    let key = rustls_pemfile::private_key(&mut key_reader)
        .context("failed to parse PD client key")?
        .ok_or_else(|| anyhow::anyhow!("no private key found in {key_path}"))?;

    rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_client_auth_cert(client_certs, key)
        .context("failed to build rustls ClientConfig for PD HTTP client")
}

/// PD API base URL (HTTPS when TLS is configured, HTTP otherwise).
pub(crate) fn pd_base_url(pd_endpoint: &str) -> String {
    let pd_endpoint = pd_endpoint.trim().trim_end_matches('/');
    if pd_endpoint.starts_with("http://") || pd_endpoint.starts_with("https://") {
        return pd_endpoint.to_string();
    }

    if std::env::var("TIKV_CA_PATH").is_ok() {
        format!("https://{}", pd_endpoint)
    } else {
        format!("http://{}", pd_endpoint)
    }
}

fn pd_keyspaces_base_url(pd_endpoint: &str) -> String {
    format!("{}/pd/api/v2/keyspaces", pd_base_url(pd_endpoint))
}

/// Query PD for the state of a keyspace.
///
/// Returns the state string (e.g. "ENABLED", "DISABLED") or None if the
/// keyspace does not exist or the query fails.
pub async fn check_keyspace_state(pd_endpoints: &[String], keyspace: &str) -> Option<String> {
    let pd_primary = pd_endpoints.first()?;
    let url = format!("{}/{}", pd_keyspaces_base_url(pd_primary), keyspace);

    let client = match build_pd_client() {
        Ok(c) => c,
        Err(e) => {
            warn!("Failed to build PD client for keyspace state check: {}", e);
            return None;
        }
    };

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body: serde_json::Value = resp.json().await.ok()?;
            body.get("state")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
        }
        Ok(resp) => {
            let status = resp.status();
            debug!("PD keyspace query for '{}' returned {}", keyspace, status);
            None
        }
        Err(e) => {
            debug!("PD keyspace query for '{}' failed: {}", keyspace, e);
            None
        }
    }
}

/// Ensure the system keyspace exists in PD before initializing worker store.
///
/// This keeps worker metadata isolated (`_sys_worker`) while removing the need
/// for manual keyspace pre-provisioning in local/dev and CI-like environments.
/// In TLS mode, skip HTTP provisioning and rely on existing pre-created keyspace.
///
/// Visible at crate scope so TiKV-backed behavioral tests that acquire a TENANT
/// store via the pool (whose `with_keyspace` connect requires the keyspace to
/// already exist in PD) can pre-create the tenant keyspace with the SAME
/// canonical PD-API path production uses for the system keyspace.
pub(crate) async fn ensure_system_keyspace(pd_endpoints: &[String], keyspace: &str) -> Result<()> {
    if std::env::var("TIKV_CA_PATH").is_ok() {
        info!(
            "Skipping system keyspace ensure in TLS mode; expecting '{}' to be pre-created",
            keyspace
        );
        return Ok(());
    }

    let pd_primary = pd_endpoints
        .first()
        .ok_or_else(|| anyhow::anyhow!("no PD endpoint configured"))?;
    let base = pd_keyspaces_base_url(pd_primary);
    let keyspace_url = format!("{}/{}", base, keyspace);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("failed to build PD HTTP client")?;

    let mut last_err = String::new();
    for _ in 0..15 {
        // Idempotent create. Some PD versions return 500 with "keyspace already exists"
        // instead of 409, so we always verify with a follow-up GET before retrying.
        let post_resp = client
            .post(&base)
            .json(&serde_json::json!({ "name": keyspace }))
            .send()
            .await;

        match post_resp {
            Ok(resp) => {
                let status = resp.status();
                if !(status.is_success() || status.as_u16() == 409) {
                    let body = resp.text().await.unwrap_or_default();
                    if status.as_u16() == 500 || status.as_u16() == 503 {
                        // Fall through to GET existence check for idempotent success.
                    } else {
                        return Err(anyhow::anyhow!(
                            "failed to create system keyspace '{}': status={}, body={}",
                            keyspace,
                            status,
                            body
                        ));
                    }
                }
            }
            Err(e) => {
                last_err = format!("POST error: {}", e);
                sleep(Duration::from_secs(1)).await;
                continue;
            }
        }

        match client.get(&keyspace_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    "Ensured worker system keyspace '{}' on PD {}",
                    keyspace, pd_primary
                );
                return Ok(());
            }
            Ok(resp) => {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                last_err = format!("GET status={}, body={}", status, body);
            }
            Err(e) => {
                last_err = format!("GET error: {}", e);
            }
        }

        sleep(Duration::from_secs(1)).await;
    }

    Err(anyhow::anyhow!(
        "unable to ensure system keyspace '{}' on PD {} after retries: {}",
        keyspace,
        pd_primary,
        last_err
    ))
}

/// Initialize the always-on system store. This is the SINGLE canonical
/// system-store init used by BOTH production startup and tests/integration —
/// there is no second "test-only" init that could diverge from production.
///
/// Initialization performs the one-shot V1->V2 worker-queue schema migration
/// (idempotent: short-circuits once `_wq_schema_version = 2`). This MUST run
/// here, before the worker engine begins ticking, because the production tick
/// is V2-only: any pre-existing legacy `_worker_queue_` rows (cron next-fire
/// entries, in-flight BgDdl/CREATE INDEX CONCURRENTLY backfills, queued BgSql,
/// pending AsyncTriggers, pending HNSW merges) would otherwise be neither
/// scanned, executed, nor reaped — a durable orphan / lost-recovery defect on
/// any node upgrading from a build that still has live V1 rows.
pub async fn init_gc_registry_store(
    pd_endpoints: Vec<String>,
    config: &WorkerConfig,
) -> Result<Arc<TikvStore>> {
    info!(
        "Initializing GC registry store for keyspace: {}",
        config.system_keyspace
    );

    if let Err(e) = ensure_system_keyspace(&pd_endpoints, &config.system_keyspace).await {
        warn!(
            "Failed to ensure GC registry keyspace '{}': {}. Proceeding with direct init.",
            config.system_keyspace, e
        );
    }

    let store = TikvStore::new_system(pd_endpoints, &config.system_keyspace)
        .await
        .with_context(|| {
            format!(
                "failed to initialize isolated system keyspace '{}'; refusing fallback",
                config.system_keyspace
            )
        })?;
    let migrated = store
        .ensure_worker_queue_schema_v2()
        .await
        .context("failed to migrate worker queue schema to V2")?;
    if migrated > 0 {
        info!(
            "Migrated {} legacy worker queue entries into V2 schema",
            migrated
        );
    }
    let store = Arc::new(store);
    info!("GC registry store initialized successfully");
    Ok(store)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_system_store_signature_has_no_fallback_keyspace() {
        let cfg = WorkerConfig::default();
        let fut = init_gc_registry_store(Vec::new(), &cfg);
        drop(fut);
    }

    #[test]
    fn pd_base_url_preserves_explicit_scheme_and_trims_slash() {
        assert_eq!(
            pd_base_url("https://pd.example:2379/"),
            "https://pd.example:2379"
        );
        assert_eq!(
            pd_base_url("http://pd.example:2379/"),
            "http://pd.example:2379"
        );
    }

    #[test]
    fn pd_keyspaces_base_url_preserves_explicit_scheme() {
        assert_eq!(
            pd_keyspaces_base_url("http://127.0.0.1:2379/"),
            "http://127.0.0.1:2379/pd/api/v2/keyspaces"
        );
        assert_eq!(
            pd_keyspaces_base_url("https://pd.example:2379/"),
            "https://pd.example:2379/pd/api/v2/keyspaces"
        );
    }

    #[test]
    fn async_trigger_depth_sampler_throttles_dirty_keyspace_until_interval_elapsed() {
        let mut sampler = AsyncTriggerDepthSampler::default();
        let now = Instant::now();

        sampler.mark_dirty("tenant_a");
        assert!(sampler.due("tenant_a", now));
        sampler.mark_attempted("tenant_a", now);

        sampler.mark_dirty("tenant_a");
        assert!(
            !sampler.due(
                "tenant_a",
                now + ASYNC_TRIGGER_QUEUE_DEPTH_SAMPLE_INTERVAL / 2
            ),
            "same tenant should not trigger another storage scan inside the sample interval"
        );
        assert!(
            sampler
                .dirty_due_keyspaces(now + ASYNC_TRIGGER_QUEUE_DEPTH_SAMPLE_INTERVAL / 2)
                .is_empty(),
            "dirty keyspace should wait for the next due interval"
        );

        let due = sampler.dirty_due_keyspaces(now + ASYNC_TRIGGER_QUEUE_DEPTH_SAMPLE_INTERVAL);
        assert_eq!(due, vec!["tenant_a".to_string()]);
    }

    #[test]
    fn async_trigger_depth_sampler_keeps_dirty_keyspace_after_failed_attempt() {
        let mut sampler = AsyncTriggerDepthSampler::default();
        let now = Instant::now();

        sampler.mark_dirty("tenant_a");
        sampler.mark_attempted("tenant_a", now);

        assert!(
            sampler.dirty.contains("tenant_a"),
            "failed samples must keep the keyspace dirty for a later retry"
        );
        assert!(
            !sampler.due(
                "tenant_a",
                now + ASYNC_TRIGGER_QUEUE_DEPTH_SAMPLE_INTERVAL / 2
            ),
            "failed samples should still be rate-limited to avoid retry storms"
        );
        assert!(
            sampler.due("tenant_a", now + ASYNC_TRIGGER_QUEUE_DEPTH_SAMPLE_INTERVAL),
            "dirty keyspace should become retryable after the interval"
        );
    }

    #[test]
    fn async_trigger_depth_sampler_prunes_idle_tenant_attempts() {
        let mut sampler = AsyncTriggerDepthSampler::default();
        let now = Instant::now();
        let old = now - ASYNC_TRIGGER_QUEUE_DEPTH_IDLE_RETENTION - Duration::from_secs(1);

        sampler
            .last_attempted
            .insert("idle_tenant".to_string(), old);
        sampler.mark_dirty("active_tenant");
        sampler.mark_attempted("active_tenant", now);
        sampler.prune_idle(now);

        assert!(
            !sampler.last_attempted.contains_key("idle_tenant"),
            "idle tenants should not remain in process-global sampler state forever"
        );
        assert!(
            sampler.last_attempted.contains_key("active_tenant"),
            "dirty tenants must stay rate-limited while waiting for a retry"
        );
    }

    #[test]
    fn pd_client_uses_rustls_config_not_reqwest_identity_from_pem() {
        let source = include_str!("mod.rs");
        let build_fn = source
            .split("pub(crate) fn build_pd_client()")
            .nth(1)
            .and_then(|rest| rest.split("/// Build a rustls client config").next())
            .expect("build_pd_client must exist before pd_base_url");

        assert!(
            build_fn.contains("build_pd_rustls_config"),
            "PD client must use the rustls config helper"
        );
        assert!(
            build_fn.contains("use_preconfigured_tls"),
            "PD client must pass a preconfigured rustls ClientConfig to reqwest"
        );
        assert!(
            !build_fn.contains("Identity::from_pem"),
            "reqwest Identity::from_pem rejects non-PKCS#8 keys with rustls-tls"
        );
    }

    #[tokio::test]
    async fn test_init_system_store_disabled_still_initializes_metadata_store() {
        let cfg = WorkerConfig {
            enabled: false,
            ..Default::default()
        };
        let result = init_gc_registry_store(vec!["127.0.0.1:1".to_string()], &cfg).await;
        assert!(
            result.is_err(),
            "disabled worker execution must still require the metadata store; \
             the unreachable test PD should therefore fail initialization"
        );
    }

    #[tokio::test]
    async fn test_init_system_store_failure_refuses_fallback() {
        let cfg = WorkerConfig {
            enabled: true,
            system_keyspace: "_sys_worker".to_string(),
            ..Default::default()
        };

        let err = match init_gc_registry_store(vec!["127.0.0.1:1".to_string()], &cfg).await {
            Ok(_) => panic!("system store should fail without fallback when unavailable"),
            Err(err) => err,
        };
        let msg = format!("{:#}", err);

        assert!(
            msg.contains("refusing fallback"),
            "error should contain no-fallback guard, got: {msg}"
        );
        assert!(
            msg.contains(&cfg.system_keyspace),
            "error should include target system keyspace, got: {msg}"
        );
    }

    /// Regression test: system store init failure must return Err — never a
    /// silent absence. Recovery metadata producers depend on this handle even
    /// when worker execution is disabled.
    #[tokio::test]
    async fn test_system_store_init_failure_is_never_silent() {
        let cfg = WorkerConfig {
            enabled: false,
            ..Default::default()
        };

        // Unreachable PD guarantees init will fail
        let result = init_gc_registry_store(vec!["127.0.0.1:1".to_string()], &cfg).await;
        assert!(
            result.is_err(),
            "system store init failure must return Err, not an optional absence"
        );
    }

    /// Behavioral regression for the V1->V2 migration on the PRODUCTION init
    /// path. Seeds a legacy `_worker_queue_` row, runs the SAME init function
    /// that `main.rs` calls at startup (`init_gc_registry_store`), and asserts
    /// the legacy row is migrated and visible to the V2-only tick (`scan_due_v2`).
    /// This proves the tested path equals the production path — a source-string
    /// EXISTS check would not.
    #[tokio::test]
    #[ignore = "requires TiKV / PD cluster"]
    async fn production_init_migrates_legacy_worker_queue_rows() {
        use crate::worker::types::{TaskQueueEntry, TaskType};

        let pd_endpoints = std::env::var("PD_ENDPOINTS")
            .unwrap_or_else(|_| "127.0.0.1:2379".to_string())
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
        let system_keyspace = format!(
            "_sys_worker_prodinit_{}_{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        );
        let cfg = WorkerConfig {
            enabled: true,
            system_keyspace,
            ..Default::default()
        };

        // `new_system` connects with `with_keyspace`, which requires the keyspace
        // to already exist in PD (the vendored client does NOT auto-create it).
        // Pre-create it with the canonical PD-API helper that `init_gc_registry_store`
        // uses, so this promoted CI test does not fail at connect.
        ensure_system_keyspace(&pd_endpoints, &cfg.system_keyspace)
            .await
            .expect("pre-create production-init test keyspace in PD");
        // Seed a legacy V1 row directly (pre-upgrade durable state) on a raw
        // store handle that has NOT run the migration yet.
        let raw = std::sync::Arc::new(
            TikvStore::new_system(pd_endpoints.clone(), &cfg.system_keyspace)
                .await
                .expect("raw system store init"),
        );
        let entry = TaskQueueEntry::new(
            "default".to_string(),
            7,
            42,
            TaskType::Cron,
            "SELECT 1".to_string(),
            "admin".to_string(),
            128,
        )
        .with_schedule("* * * * *".to_string());
        let fire_time_ms = crate::worker::now_epoch_ms() - 60_000;
        raw.seed_legacy_worker_queue_entry_for_test(&entry, fire_time_ms)
            .await
            .expect("seed legacy V1 row");

        // Run the EXACT production startup init (the one main.rs calls).
        let store = init_gc_registry_store(pd_endpoints, &cfg)
            .await
            .expect("production init must succeed");

        // The legacy row must now be visible to the V2-only tick.
        let mut txn = store.begin().await.expect("begin");
        let due = store
            .scan_due_v2(&mut txn, i64::MAX, 1000)
            .await
            .expect("scan_due_v2");
        txn.rollback().await.ok();
        assert!(
            due.iter()
                .any(|(_, d)| d.task_id == 42 && d.db_id == 7 && d.task_type == TaskType::Cron),
            "production init must migrate the seeded legacy V1 row into V2"
        );
    }
}
