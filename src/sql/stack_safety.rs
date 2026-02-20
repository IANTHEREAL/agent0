//! Stack-safety helpers for very deep SQL trees.
//!
//! PostgreSQL-compatible queries can contain deeply nested boolean expressions
//! (e.g. long OR chains). sqlparser-rs and our typed IR both use recursive
//! tree structures, so dropping large trees on a default worker stack can
//! overflow. These helpers provide a single explicit boundary for running
//! such work on a grown stack.

/// Minimum remaining stack before switching to an alternate stack.
///
/// Keep this intentionally high so deep SQL AST/IR operations run on a grown
/// stack even when called from non-recursive top-level frames.
const STACK_RED_ZONE_BYTES: usize = 32 * 1024 * 1024;
/// Target alternate stack size for deep-tree traversal/drop.
const GROWN_STACK_BYTES: usize = 64 * 1024 * 1024;

#[inline]
pub(crate) fn with_grown_stack<R>(f: impl FnOnce() -> R) -> R {
    stacker::maybe_grow(STACK_RED_ZONE_BYTES, GROWN_STACK_BYTES, f)
}

#[inline]
pub(crate) fn drop_on_grown_stack<T>(value: T) {
    with_grown_stack(|| drop(value));
}
