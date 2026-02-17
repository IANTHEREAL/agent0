//! Trigger subsystem — BEFORE/AFTER trigger execution, caching, queuing, and background worker.

pub(crate) mod before;
pub(crate) mod cache;
pub(crate) mod claim;
pub(crate) mod enqueue;
pub(crate) mod execute;
pub(crate) mod gc;
pub(crate) mod queue;
pub(crate) mod rewrite;
pub(crate) mod worker;

// Re-exports for stable public API (used by pool.rs, dynamic.rs, executor/core/mod.rs)
pub(crate) use before::{apply_before_triggers_with_cache, prefetch_trigger_functions};
pub(crate) use cache::TriggerBodyCache;
