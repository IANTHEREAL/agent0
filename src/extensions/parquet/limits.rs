//! Resource limits for Parquet imports.
//!
//! Global memory budget (512 MB) prevents OOM from concurrent decompression.
//! Per-tenant concurrency limit (4) prevents resource exhaustion.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::Semaphore;

// -- Global memory budget ---------------------------------------------------

static PARQUET_MEMORY_USED: AtomicUsize = AtomicUsize::new(0);
const MAX_PARQUET_MEMORY_BYTES: usize = 512 * 1024 * 1024; // 512 MB

#[derive(Debug)]
pub(crate) struct MemoryGuard {
    bytes: usize,
}

impl Drop for MemoryGuard {
    fn drop(&mut self) {
        PARQUET_MEMORY_USED.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

pub(crate) fn try_acquire_memory(bytes: usize) -> anyhow::Result<MemoryGuard> {
    loop {
        let current = PARQUET_MEMORY_USED.load(Ordering::Relaxed);
        let new = current
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("parquet: memory budget arithmetic overflow"))?;
        if new > MAX_PARQUET_MEMORY_BYTES {
            return Err(anyhow::anyhow!(
                "parquet: memory budget exceeded (requested {} bytes, {} of {} in use)",
                bytes,
                current,
                MAX_PARQUET_MEMORY_BYTES
            ));
        }
        if PARQUET_MEMORY_USED
            .compare_exchange_weak(current, new, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(MemoryGuard { bytes });
        }
    }
}

// -- Per-tenant concurrency -------------------------------------------------

const MAX_CONCURRENT_PARQUET_IMPORTS_PER_TENANT: usize = 4;

struct TenantParquetLimiters {
    by_tenant: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl TenantParquetLimiters {
    fn semaphore(&self, tenant: &str) -> Arc<Semaphore> {
        let mut guard = self
            .by_tenant
            .lock()
            .expect("parquet tenant semaphore lock");
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

// -- Helpers ----------------------------------------------------------------

/// Estimate memory needed for a row group based on its compressed size.
/// Uses 2x heuristic for decompression ratio.
pub(crate) fn estimate_row_group_memory(compressed_size: i64) -> usize {
    let clamped = compressed_size.max(0) as usize;
    clamped.saturating_mul(2)
}

// -- Test support -----------------------------------------------------------

#[cfg(test)]
pub(crate) fn reset_memory_budget() {
    PARQUET_MEMORY_USED.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All memory budget tests run sequentially in a single test because
    /// they share the global PARQUET_MEMORY_USED atomic counter.
    #[test]
    fn test_memory_budget() {
        // -- acquire and release --
        reset_memory_budget();
        let bytes = 1024;
        {
            let guard = try_acquire_memory(bytes).unwrap();
            assert_eq!(guard.bytes, bytes);
            assert_eq!(PARQUET_MEMORY_USED.load(Ordering::Relaxed), bytes);
        }
        assert_eq!(PARQUET_MEMORY_USED.load(Ordering::Relaxed), 0);

        // -- budget exceeded (single over-max allocation) --
        let err = try_acquire_memory(MAX_PARQUET_MEMORY_BYTES + 1).unwrap_err();
        assert!(err.to_string().contains("memory budget exceeded"));
        assert_eq!(PARQUET_MEMORY_USED.load(Ordering::Relaxed), 0);

        // -- budget exceeded (two halves then overflow) --
        let half = MAX_PARQUET_MEMORY_BYTES / 2;
        let g1 = try_acquire_memory(half).unwrap();
        let g2 = try_acquire_memory(half).unwrap();
        let err = try_acquire_memory(1).unwrap_err();
        assert!(err.to_string().contains("memory budget exceeded"));
        drop(g1);
        drop(g2);
        assert_eq!(PARQUET_MEMORY_USED.load(Ordering::Relaxed), 0);

        // -- multiple concurrent guards --
        let g1 = try_acquire_memory(100).unwrap();
        let g2 = try_acquire_memory(200).unwrap();
        assert_eq!(PARQUET_MEMORY_USED.load(Ordering::Relaxed), 300);
        drop(g1);
        assert_eq!(PARQUET_MEMORY_USED.load(Ordering::Relaxed), 200);
        drop(g2);
        assert_eq!(PARQUET_MEMORY_USED.load(Ordering::Relaxed), 0);
    }

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
