//! Resource limits for Parquet imports.
//!
//! Per-tenant concurrency limit (4) prevents resource exhaustion.
// TODO(#2335): migrate to parking_lot — phase 2/3
#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Semaphore;

// -- Per-tenant concurrency -------------------------------------------------

const MAX_CONCURRENT_PARQUET_IMPORTS_PER_TENANT: usize = 4;

struct TenantParquetLimiters {
    by_tenant: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl TenantParquetLimiters {
    fn semaphore(&self, tenant: &str) -> Arc<Semaphore> {
        let mut guard = self.by_tenant.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = guard.get(tenant) {
            return existing.clone();
        }
        let sem = Arc::new(Semaphore::new(MAX_CONCURRENT_PARQUET_IMPORTS_PER_TENANT));
        guard.insert(tenant.to_string(), sem.clone());
        sem
    }
}

static PARQUET_LIMITERS: OnceLock<TenantParquetLimiters> = OnceLock::new();

fn limiters() -> &'static TenantParquetLimiters {
    PARQUET_LIMITERS.get_or_init(|| TenantParquetLimiters {
        by_tenant: Mutex::new(HashMap::new()),
    })
}

/// Remove the cached Parquet semaphore for a keyspace.
///
/// Called when a tenant is evicted from the connection pool to prevent
/// unbounded accumulation of stale entries.
pub(crate) fn evict_parquet_limiter(keyspace: &str) {
    if let Some(l) = PARQUET_LIMITERS.get() {
        let mut guard = l.by_tenant.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(keyspace);
    }
}

pub(crate) fn acquire_import_permit(
    tenant: &str,
) -> anyhow::Result<tokio::sync::OwnedSemaphorePermit> {
    let semaphore = limiters().semaphore(tenant);
    semaphore.try_acquire_owned().map_err(|_| {
        anyhow::anyhow!(
            "Too many concurrent Parquet imports for tenant '{}' (max {})",
            tenant,
            MAX_CONCURRENT_PARQUET_IMPORTS_PER_TENANT
        )
    })
}

// -- Test support -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tenant_limiter_isolation() {
        let sem_a = limiters().semaphore("tenant_iso_a");
        let sem_b = limiters().semaphore("tenant_iso_b");
        assert!(!Arc::ptr_eq(&sem_a, &sem_b));
        let sem_a2 = limiters().semaphore("tenant_iso_a");
        assert!(Arc::ptr_eq(&sem_a, &sem_a2));
    }

    #[tokio::test]
    async fn test_tenant_limiter_capacity() {
        let tenant = "test_capacity_tenant_unique";
        let mut permits = Vec::new();
        for _ in 0..MAX_CONCURRENT_PARQUET_IMPORTS_PER_TENANT {
            permits.push(acquire_import_permit(tenant).unwrap());
        }
        let err = acquire_import_permit(tenant).unwrap_err();
        assert!(err
            .to_string()
            .contains("Too many concurrent Parquet imports"));
        permits.pop();
        let _permit = acquire_import_permit(tenant).unwrap();
    }
}
