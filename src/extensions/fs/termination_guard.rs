//! Drop-bomb helper for stream types that must be explicitly consumed.
//!
//! `FsWriteStream` callers MUST go through `terminate(outcome)`; dropping the
//! stream without calling `terminate` is a bug. Historically that invariant
//! lived inline in each impl's `Drop`, with a `terminated: bool` flag gating a
//! `debug_assert!`. Extracting it as a pure, backend-free struct lets the
//! invariant itself be exercised by ordinary `cargo test` in PR CI — the
//! impls' own backends still require TiKV/S3/fs9 to instantiate, and their
//! behavioral tests stay in the nightly integration tier.
//!
//! This helper carries only a boolean flag and a static "kind" label used in
//! diagnostic messages. It emits no `warn!` on drop (each impl still emits
//! context-rich warnings alongside its advisory cleanup); it only fires the
//! `debug_assert!` that catches forgot-to-terminate in debug/CI builds.
//!
//! # Field-drop ordering
//! Rust drops struct fields in declaration order after the struct's own
//! `Drop::drop` runs. Embed `TerminationGuard` as the LAST field of the host
//! struct so the host's own `Drop` (advisory cleanup + context-rich warnings)
//! runs first, and the guard's debug_assert fires last. See the guard's Drop
//! for the two-check rationale.

/// Drop-bomb: panics in debug builds if dropped without `mark_terminated()`.
///
/// Consumers wire this up by:
///   1. Embedding `guard: TerminationGuard` as the **last** field of the host
///      struct.
///   2. Calling `self.guard.mark_terminated()` synchronously at the top of
///      the host's consuming method (`terminate(...)`), **before any `.await`**
///      — otherwise a cancelled terminate future falsely trips the bomb.
///   3. Letting the host's own `Drop` run its advisory cleanup; the guard's
///      `Drop` then runs the assert.
pub(crate) struct TerminationGuard {
    terminated: bool,
    kind: &'static str,
}

impl TerminationGuard {
    /// Create an unterminated guard. `kind` is used only in the debug_assert
    /// message so the failing test points at the right host type.
    pub(crate) fn new(kind: &'static str) -> Self {
        Self {
            terminated: false,
            kind,
        }
    }

    /// Declare that the host has been properly consumed. Must be called from
    /// the host's consuming method before any `.await`.
    pub(crate) fn mark_terminated(&mut self) {
        self.terminated = true;
    }

    /// Whether the host has been marked terminated. The host's `Drop` uses
    /// this to decide whether to run advisory cleanup.
    pub(crate) fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl Drop for TerminationGuard {
    fn drop(&mut self) {
        // Two-check structure:
        //   1. If the host terminated normally, everything is fine — return.
        //   2. If we're unwinding a panic, do NOT double-panic (Rust would
        //      abort the process). The outer panic will surface the real bug;
        //      our missed-termination is almost certainly a consequence, not
        //      a cause.
        // Debug builds otherwise hit `debug_assert!(false, ...)` which fails
        // the test or crashes the debug binary. Release builds have no bomb
        // (debug_assert is a no-op there); the host's own Drop emits a
        // context-rich warn and server TTL reclaims.
        if !self.terminated && !std::thread::panicking() {
            debug_assert!(
                false,
                "{} dropped without terminate() — \
                 callers must go through the single consuming entry point",
                self.kind
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A guard that is never marked terminated MUST panic in debug builds.
    /// This is the only CI-level signal that the drop-bomb is wired at all;
    /// a regression that removes `debug_assert!` or flips its condition
    /// makes this test pass-without-panic → `#[should_panic]` catches it.
    #[test]
    #[should_panic(expected = "dropped without terminate")]
    fn guard_unterminated_drop_fires_bomb() {
        let _guard = TerminationGuard::new("TestGuard");
        // Intentional: drop without calling mark_terminated.
    }

    /// A guard that was properly terminated MUST NOT panic on drop.
    #[test]
    fn guard_terminated_drop_is_silent() {
        let mut guard = TerminationGuard::new("TestGuard");
        guard.mark_terminated();
        assert!(guard.is_terminated());
        // Drop runs here — no panic expected.
    }

    /// If the host is already unwinding a panic, the guard MUST stay silent
    /// (otherwise we'd cause a double-panic → process abort). The guard
    /// covers "forgot to terminate"; the outer panic covers its own cause.
    #[test]
    fn guard_drop_during_panic_is_silent() {
        // `std::panic::catch_unwind` suppresses the payload so the test
        // process survives. If the guard double-panicked it would abort
        // the test process regardless of catch_unwind.
        let result = std::panic::catch_unwind(|| {
            let _guard = TerminationGuard::new("TestGuard");
            // Trigger an unrelated panic; guard is dropped as the stack
            // unwinds through this frame.
            panic!("unrelated panic — guard must stay silent");
        });
        assert!(result.is_err(), "outer panic must surface");
    }
}
