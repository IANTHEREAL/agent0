//! Export snapshot subsystem for Backup v2.
//!
//! Provides pinned, bounded-lifetime export snapshots that can be consumed
//! by backend backup workers for consistent logical backup.
//!
//! The export snapshot contract:
//! 1. `begin_export_snapshot` — pin a database-wide snapshot with GC protection
//! 2. `release_export_snapshot` — release the snapshot and remove GC pin
//! 3. `list_export_snapshots` — list all active snapshots (observability)

pub mod lifecycle;
#[allow(dead_code)] // S0 infrastructure: public API wired in S1
pub mod registry;
#[allow(dead_code)] // S1: export scan, wired in S1 handlers
pub(crate) mod scan;

use std::sync::{Arc, OnceLock};

use registry::ExportSnapshotRegistry;

static GLOBAL_REGISTRY: OnceLock<Arc<ExportSnapshotRegistry>> = OnceLock::new();

/// Set the global export snapshot registry (called once at startup).
pub fn set_global_registry(registry: Arc<ExportSnapshotRegistry>) {
    if GLOBAL_REGISTRY.set(registry).is_err() {
        panic!("export snapshot registry already initialized");
    }
}

/// Get the global export snapshot registry, if initialized.
#[allow(dead_code)] // Will be used by S1 handlers
pub fn global_registry() -> Option<&'static Arc<ExportSnapshotRegistry>> {
    GLOBAL_REGISTRY.get()
}
