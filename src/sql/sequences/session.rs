use anyhow::{anyhow, Result};
use std::collections::HashMap;

/// Tracks a sequence name whose pending drop was cancelled by a
/// re-observation (`nextval`/`setval(true)` after `defer_sequence_drop`).
/// This indicates a drop+recreate cycle within the same transaction scope.
/// On rollback the re-observation effects must be undone so that `lastval()`
/// errors (PG parity: the recreated OID no longer exists) and `currval()`
/// returns the pre-drop value.
#[derive(Debug, Clone)]
struct ReobservedDrop {
    name: String,
    /// `per_seq` entry for this name *before* the re-observation overwrote it.
    old_per_seq_val: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct SequenceSession {
    per_seq: HashMap<String, i64>,
    last_value: Option<i64>,
    last_nextval_seq: Option<String>,
    /// Sequence names whose session state should be cleared after the
    /// enclosing storage transaction commits.  Accumulated by
    /// `defer_sequence_drop` during statement execution and applied by
    /// `apply_pending_drops` in the commit path.  Discarded without
    /// effect on rollback via `discard_pending_drops`.
    pending_drops: Vec<String>,
    /// Ordered log of pending drops cancelled by re-observation
    /// (drop+recreate+nextval within the same transaction scope).  Each
    /// cycle pushes a new entry (not deduplicated by name) so that
    /// length-based savepoint truncation correctly undoes multi-cycle
    /// scenarios.  On rollback their effects on `per_seq`/`lastval`
    /// are undone.
    reobserved_drops: Vec<ReobservedDrop>,
    /// Savepoint stack: each entry snapshots `pending_drops` (and the
    /// length of `reobserved_drops`) at savepoint creation time so that
    /// `ROLLBACK TO` can restore state.
    savepoints: Vec<(String, Vec<String>, usize)>,
}

impl SequenceSession {
    pub fn new() -> Self {
        Self {
            per_seq: HashMap::new(),
            last_value: None,
            last_nextval_seq: None,
            pending_drops: Vec::new(),
            reobserved_drops: Vec::new(),
            savepoints: Vec::new(),
        }
    }

    pub fn record_nextval(&mut self, seq_name: String, val: i64) {
        // If cancelling a pending drop, record the re-observation so rollback
        // can undo it (drop+recreate identity invalidation — PG OID parity).
        if self.pending_drops.iter().any(|n| n == &seq_name) {
            self.reobserved_drops.push(ReobservedDrop {
                name: seq_name.clone(),
                old_per_seq_val: self.per_seq.get(&seq_name).copied(),
            });
        }
        self.per_seq.insert(seq_name.clone(), val);
        self.last_value = Some(val);
        // Cancel any pending drop for this name — the session now observes a
        // new sequence identity (drop+recreate same name within one txn).
        self.pending_drops.retain(|n| n != &seq_name);
        self.last_nextval_seq = Some(seq_name);
    }

    pub fn record_setval(&mut self, seq_name: String, val: i64, is_called: bool) {
        if is_called {
            // Track re-observation — same as record_nextval.
            if self.pending_drops.iter().any(|n| n == &seq_name) {
                self.reobserved_drops.push(ReobservedDrop {
                    name: seq_name.clone(),
                    old_per_seq_val: self.per_seq.get(&seq_name).copied(),
                });
            }
            self.per_seq.insert(seq_name.clone(), val);
            // Cancel any pending drop — same rationale as record_nextval.
            self.pending_drops.retain(|n| n != &seq_name);
            if self.last_nextval_seq.as_deref() == Some(&seq_name) {
                self.last_value = Some(val);
            }
        }
    }

    pub fn currval(&self, seq_name: &str) -> Result<i64> {
        // If the sequence has a pending drop, treat it as not yet observed
        // (PG parity: DROP within an explicit txn makes currval error
        // immediately, not just at commit).
        if self.pending_drops.iter().any(|n| n == seq_name) {
            let display_name = seq_name
                .rsplit_once('.')
                .map(|(_, name)| name)
                .unwrap_or(seq_name);
            return Err(anyhow!(
                "currval of sequence \"{}\" is not yet defined in this session",
                display_name
            ));
        }
        self.per_seq.get(seq_name).copied().ok_or_else(|| {
            let display_name = seq_name
                .rsplit_once('.')
                .map(|(_, name)| name)
                .unwrap_or(seq_name);
            anyhow!(
                "currval of sequence \"{}\" is not yet defined in this session",
                display_name
            )
        })
    }

    pub fn lastval(&self) -> Result<i64> {
        // If the sequence that produced the last value has a pending drop,
        // treat lastval as undefined (PG parity: DROP within an explicit
        // txn makes lastval error immediately, not just at commit).
        if let Some(ref name) = self.last_nextval_seq {
            if self.pending_drops.iter().any(|n| n == name) {
                return Err(anyhow!("lastval is not yet defined in this session"));
            }
        }
        self.last_value
            .ok_or_else(|| anyhow!("lastval is not yet defined in this session"))
    }

    /// Clear session state for a dropped sequence so stale values cannot leak
    /// through after a drop/recreate cycle.  PostgreSQL tracks sequences by OID,
    /// so dropping and recreating with the same name produces a new identity;
    /// `lastval()` must not return the old sequence's value.
    fn on_sequence_dropped(&mut self, seq_full_name: &str) {
        self.per_seq.remove(seq_full_name);
        if self.last_nextval_seq.as_deref() == Some(seq_full_name) {
            self.last_value = None;
            self.last_nextval_seq = None;
        }
    }

    /// Record a sequence drop that should take effect only after the
    /// enclosing storage transaction commits.  This avoids clearing
    /// `lastval`/`currval` state prematurely — a subsequent ROLLBACK
    /// must leave session state untouched (PostgreSQL parity).
    pub fn defer_sequence_drop(&mut self, seq_full_name: String) {
        self.pending_drops.push(seq_full_name);
    }

    /// Apply all deferred sequence drops.  Called after the storage
    /// transaction commits successfully.
    pub fn apply_pending_drops(&mut self) {
        for name in std::mem::take(&mut self.pending_drops) {
            self.on_sequence_dropped(&name);
        }
        self.reobserved_drops.clear();
        self.savepoints.clear();
    }

    /// Discard deferred sequence drops without applying them.  Called
    /// on transaction rollback so that session state is preserved.
    /// Also undoes the effects of any drop+recreate re-observations so
    /// that `lastval()` errors and `currval()` returns the pre-drop
    /// value (PG OID-invalidation parity).
    pub fn discard_pending_drops(&mut self) {
        self.pending_drops.clear();
        self.undo_reobserved_drops(0);
        self.reobserved_drops.clear();
        self.savepoints.clear();
    }

    /// Undo re-observation effects for entries at index `from_idx..`.
    /// Restores `per_seq` to the pre-re-observation value and clears
    /// `lastval` if it points to an invalidated identity.
    fn undo_reobserved_drops(&mut self, from_idx: usize) {
        for rd in self.reobserved_drops[from_idx..].iter().rev() {
            match rd.old_per_seq_val {
                Some(v) => {
                    self.per_seq.insert(rd.name.clone(), v);
                }
                None => {
                    self.per_seq.remove(&rd.name);
                }
            }
            if self.last_nextval_seq.as_deref() == Some(&rd.name) {
                self.last_nextval_seq = None;
                self.last_value = None;
            }
        }
    }

    /// Snapshot `pending_drops` and `reobserved_drops` length for a
    /// savepoint so `ROLLBACK TO` can restore state.
    pub fn push_savepoint(&mut self, name: String) {
        self.savepoints.push((
            name,
            self.pending_drops.clone(),
            self.reobserved_drops.len(),
        ));
    }

    /// Restore state to the snapshot captured when savepoint `name` was
    /// created.  Undoes re-observations that happened after the
    /// savepoint, restores `pending_drops`, and keeps the savepoint
    /// active (a second ROLLBACK TO the same name is valid in PG).
    pub fn rollback_to_savepoint(&mut self, name: &str) {
        let Some(idx) = self.savepoints.iter().rposition(|sp| sp.0 == name) else {
            return;
        };
        let (_, snap_pending, snap_reobs_len) = self.savepoints[idx].clone();
        // Undo re-observations added after the savepoint.
        self.undo_reobserved_drops(snap_reobs_len);
        self.reobserved_drops.truncate(snap_reobs_len);
        self.pending_drops = snap_pending.clone();
        // Truncate nested savepoints above the target, keep the target.
        self.savepoints.truncate(idx + 1);
        // Update the target's snapshot to the restored state.
        self.savepoints[idx].1 = snap_pending;
        self.savepoints[idx].2 = snap_reobs_len;
    }

    /// Remove the savepoint snapshot on RELEASE SAVEPOINT.
    pub fn release_savepoint(&mut self, name: &str) {
        let Some(idx) = self.savepoints.iter().rposition(|sp| sp.0 == name) else {
            return;
        };
        self.savepoints.truncate(idx);
    }
}

impl Default for SequenceSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_session_has_no_lastval() {
        let s = SequenceSession::new();
        assert!(s.lastval().is_err());
    }

    #[test]
    fn new_session_has_no_currval() {
        let s = SequenceSession::new();
        assert!(s.currval("public.s1").is_err());
    }

    #[test]
    fn nextval_sets_currval_and_lastval() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        assert_eq!(s.currval("public.s1").unwrap(), 1);
        assert_eq!(s.lastval().unwrap(), 1);
    }

    #[test]
    fn nextval_on_different_seq_updates_lastval() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.record_nextval("public.s2".into(), 10);
        assert_eq!(s.currval("public.s1").unwrap(), 1);
        assert_eq!(s.currval("public.s2").unwrap(), 10);
        assert_eq!(s.lastval().unwrap(), 10);
    }

    #[test]
    fn setval_true_same_seq_updates_currval_and_lastval() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.record_setval("public.s1".into(), 42, true);
        assert_eq!(s.currval("public.s1").unwrap(), 42);
        assert_eq!(s.lastval().unwrap(), 42);
    }

    #[test]
    fn setval_true_different_seq_updates_currval_not_lastval() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.record_setval("public.s2".into(), 42, true);
        assert_eq!(s.currval("public.s2").unwrap(), 42);
        assert_eq!(s.lastval().unwrap(), 1);
    }

    #[test]
    fn setval_false_updates_nothing_in_session() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.record_setval("public.s1".into(), 42, false);
        assert_eq!(s.currval("public.s1").unwrap(), 1);
        assert_eq!(s.lastval().unwrap(), 1);
    }

    #[test]
    fn setval_true_on_untouched_seq_sets_currval() {
        let mut s = SequenceSession::new();
        s.record_setval("public.s1".into(), 99, true);
        assert_eq!(s.currval("public.s1").unwrap(), 99);
        assert!(s.lastval().is_err());
    }

    #[test]
    fn setval_false_on_untouched_seq_leaves_currval_undefined() {
        let mut s = SequenceSession::new();
        s.record_setval("public.s1".into(), 99, false);
        assert!(s.currval("public.s1").is_err());
        assert!(s.lastval().is_err());
    }

    #[test]
    fn multiple_sequences_independent_currval() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 5);
        s.record_nextval("public.s2".into(), 15);
        s.record_setval("public.s1".into(), 100, true);
        assert_eq!(s.currval("public.s1").unwrap(), 100);
        assert_eq!(s.currval("public.s2").unwrap(), 15);
        assert_eq!(s.lastval().unwrap(), 15);
    }

    #[test]
    fn deferred_drop_blocks_reads_immediately_and_clears_on_apply() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        assert_eq!(s.lastval().unwrap(), 1);
        assert_eq!(s.currval("public.s1").unwrap(), 1);

        // Defer — reads must immediately reflect the drop (PG parity:
        // DROP within an explicit txn makes lastval/currval error).
        s.defer_sequence_drop("public.s1".into());
        assert!(s.lastval().is_err());
        assert!(s.currval("public.s1").is_err());

        // Apply — formally clears underlying state.
        s.apply_pending_drops();
        assert!(s.lastval().is_err());
        assert!(s.currval("public.s1").is_err());
    }

    #[test]
    fn deferred_drop_preserves_other_sequences_on_apply() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.record_nextval("public.s2".into(), 10);
        // lastval tracks s2 (most recent)
        s.defer_sequence_drop("public.s1".into());
        s.apply_pending_drops();
        // s2 is unaffected
        assert_eq!(s.currval("public.s2").unwrap(), 10);
        assert_eq!(s.lastval().unwrap(), 10);
        // s1 is gone
        assert!(s.currval("public.s1").is_err());
    }

    #[test]
    fn deferred_drop_discarded_on_rollback() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.defer_sequence_drop("public.s1".into());

        // Discard (rollback) — session state preserved.
        s.discard_pending_drops();
        assert_eq!(s.lastval().unwrap(), 1);
        assert_eq!(s.currval("public.s1").unwrap(), 1);
    }

    #[test]
    fn pending_drop_blocks_lastval_but_not_other_seq() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.record_nextval("public.s2".into(), 10);
        // lastval tracks s2 (most recent)
        s.defer_sequence_drop("public.s1".into());
        // Dropping s1 does not affect lastval (s2 is the sentinel).
        assert_eq!(s.lastval().unwrap(), 10);
        // But currval for the dropped seq errors.
        assert!(s.currval("public.s1").is_err());
        // Unrelated seq is fine.
        assert_eq!(s.currval("public.s2").unwrap(), 10);
    }

    #[test]
    fn pending_drop_of_lastval_seq_blocks_lastval() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.record_nextval("public.s2".into(), 10);
        // Drop s2 (the sentinel sequence) — lastval must error.
        s.defer_sequence_drop("public.s2".into());
        assert!(s.lastval().is_err());
        // s1 currval still works.
        assert_eq!(s.currval("public.s1").unwrap(), 1);
    }

    #[test]
    fn pending_drop_restored_by_discard() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.defer_sequence_drop("public.s1".into());
        assert!(s.lastval().is_err());
        // Discard (rollback) — reads restored.
        s.discard_pending_drops();
        assert_eq!(s.lastval().unwrap(), 1);
        assert_eq!(s.currval("public.s1").unwrap(), 1);
    }

    #[test]
    fn deferred_drop_cancelled_by_nextval_on_same_name() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        // Simulate DROP + CREATE + nextval on same name within one txn.
        s.defer_sequence_drop("public.s1".into());
        // nextval on the recreated (same-name) sequence cancels the pending drop.
        s.record_nextval("public.s1".into(), 1);
        s.apply_pending_drops();
        assert_eq!(s.currval("public.s1").unwrap(), 1);
        assert_eq!(s.lastval().unwrap(), 1);
    }

    #[test]
    fn deferred_drop_cancelled_by_setval_on_same_name() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.defer_sequence_drop("public.s1".into());
        // Recreated sequence observed via setval(is_called=true).
        s.record_setval("public.s1".into(), 100, true);
        s.apply_pending_drops();
        assert_eq!(s.currval("public.s1").unwrap(), 100);
    }

    #[test]
    fn deferred_drop_not_cancelled_by_setval_false() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.defer_sequence_drop("public.s1".into());
        // setval(false) does not observe the sequence — pending drop stays.
        s.record_setval("public.s1".into(), 100, false);
        s.apply_pending_drops();
        assert!(s.currval("public.s1").is_err());
    }

    #[test]
    fn deferred_drop_twice_same_name_both_cleared_by_nextval() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.defer_sequence_drop("public.s1".into());
        // Recreate, nextval, drop again, recreate, nextval.
        s.record_nextval("public.s1".into(), 1);
        s.defer_sequence_drop("public.s1".into());
        s.record_nextval("public.s1".into(), 1);
        // Both pending drops cancelled by the last nextval.
        s.apply_pending_drops();
        assert_eq!(s.currval("public.s1").unwrap(), 1);
        assert_eq!(s.lastval().unwrap(), 1);
    }

    #[test]
    fn setval_true_same_seq_then_different_seq() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        // setval(true) on same seq → updates lastval
        s.record_setval("public.s1".into(), 50, true);
        assert_eq!(s.lastval().unwrap(), 50);
        // setval(true) on different seq → lastval unchanged
        s.record_setval("public.s2".into(), 99, true);
        assert_eq!(s.lastval().unwrap(), 50);
        assert_eq!(s.currval("public.s1").unwrap(), 50);
        assert_eq!(s.currval("public.s2").unwrap(), 99);
    }

    #[test]
    fn rollback_to_savepoint_restores_pending_drops() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        // SAVEPOINT sp1
        s.push_savepoint("sp1".into());
        // DROP SEQUENCE s1 inside savepoint
        s.defer_sequence_drop("public.s1".into());
        // Blocked by pending drop
        assert!(s.lastval().is_err());
        // ROLLBACK TO sp1 — pending_drops restored
        s.rollback_to_savepoint("sp1");
        assert_eq!(s.lastval().unwrap(), 1);
        assert_eq!(s.currval("public.s1").unwrap(), 1);
    }

    #[test]
    fn rollback_to_savepoint_then_commit_applies_no_stale_drop() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.push_savepoint("sp1".into());
        s.defer_sequence_drop("public.s1".into());
        s.rollback_to_savepoint("sp1");
        // COMMIT — no pending drops remain, so apply is a no-op.
        s.apply_pending_drops();
        assert_eq!(s.lastval().unwrap(), 1);
        assert_eq!(s.currval("public.s1").unwrap(), 1);
    }

    #[test]
    fn nested_savepoints_rollback_restores_correct_level() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.push_savepoint("sp1".into());
        // Drop inside sp1
        s.defer_sequence_drop("public.s1".into());
        s.push_savepoint("sp2".into());
        // ROLLBACK TO sp1 — removes sp2, restores pending_drops to sp1 creation
        s.rollback_to_savepoint("sp1");
        assert_eq!(s.lastval().unwrap(), 1);
        // sp2 savepoint is gone
        assert_eq!(s.savepoints.len(), 1);
    }

    #[test]
    fn release_savepoint_removes_snapshot() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.push_savepoint("sp1".into());
        s.defer_sequence_drop("public.s1".into());
        s.push_savepoint("sp2".into());
        // RELEASE sp2
        s.release_savepoint("sp2");
        assert_eq!(s.savepoints.len(), 1);
        // pending_drops still has the deferred drop
        assert!(s.lastval().is_err());
    }

    #[test]
    fn discard_pending_drops_clears_savepoint_stack() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.push_savepoint("sp1".into());
        s.defer_sequence_drop("public.s1".into());
        // Full rollback clears everything
        s.discard_pending_drops();
        assert!(s.savepoints.is_empty());
        assert_eq!(s.lastval().unwrap(), 1);
    }

    // -- P1 fix: drop+recreate+rollback invalidates stale identity ----------

    #[test]
    fn drop_recreate_rollback_clears_lastval() {
        // Simulates: nextval(s,2) → BEGIN → DROP → CREATE → nextval(s,100) → ROLLBACK
        let mut s = SequenceSession::new();
        s.record_nextval("public.s".into(), 1);
        s.record_nextval("public.s".into(), 2);
        assert_eq!(s.lastval().unwrap(), 2);

        // BEGIN; DROP s; CREATE s START 100; nextval(s)
        s.defer_sequence_drop("public.s".into());
        s.record_nextval("public.s".into(), 100); // cancels drop, adds reobserved

        // ROLLBACK
        s.discard_pending_drops();
        // lastval must error — the recreated identity was rolled back (PG OID parity).
        assert!(s.lastval().is_err());
        // currval must return the pre-drop value.
        assert_eq!(s.currval("public.s").unwrap(), 2);
    }

    #[test]
    fn drop_recreate_commit_preserves_lastval() {
        // Same cycle but COMMIT — reobserved_drops are cleared, state stays.
        let mut s = SequenceSession::new();
        s.record_nextval("public.s".into(), 1);
        s.defer_sequence_drop("public.s".into());
        s.record_nextval("public.s".into(), 100);

        s.apply_pending_drops();
        assert_eq!(s.lastval().unwrap(), 100);
        assert_eq!(s.currval("public.s").unwrap(), 100);
    }

    #[test]
    fn drop_recreate_savepoint_rollback_clears_lastval() {
        // nextval(s,1) → BEGIN → nextval(s,2) → SAVEPOINT → DROP → CREATE →
        // nextval(s,100) → ROLLBACK TO → lastval (ERROR) → currval (2)
        let mut s = SequenceSession::new();
        s.record_nextval("public.s".into(), 1);
        s.record_nextval("public.s".into(), 2);

        // SAVEPOINT sp1
        s.push_savepoint("sp1".into());

        // DROP; CREATE START 100; nextval
        s.defer_sequence_drop("public.s".into());
        s.record_nextval("public.s".into(), 100);

        // ROLLBACK TO sp1
        s.rollback_to_savepoint("sp1");
        assert!(s.lastval().is_err());
        assert_eq!(s.currval("public.s").unwrap(), 2);
    }

    #[test]
    fn drop_recreate_rollback_preserves_other_seq_lastval() {
        // If lastval points to a different (non-reobserved) seq, keep it.
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.defer_sequence_drop("public.s1".into());
        s.record_nextval("public.s1".into(), 100); // reobserved
        s.record_nextval("public.s2".into(), 42); // moves lastval to s2

        s.discard_pending_drops();
        // lastval still points to s2 (not invalidated).
        assert_eq!(s.lastval().unwrap(), 42);
        // s1 currval restored to pre-drop value.
        assert_eq!(s.currval("public.s1").unwrap(), 1);
    }

    #[test]
    fn drop_recreate_rollback_no_prior_value() {
        // Sequence was first observed inside the txn, so old_per_seq = None.
        let mut s = SequenceSession::new();
        // No prior nextval for s — first interaction is inside the txn.
        s.defer_sequence_drop("public.s".into());
        s.record_nextval("public.s".into(), 1); // reobserved, old = None

        s.discard_pending_drops();
        assert!(s.lastval().is_err());
        assert!(s.currval("public.s").is_err());
    }

    #[test]
    fn drop_recreate_setval_true_rollback_clears_lastval() {
        // Re-observation via setval(true) should also be undone on rollback.
        let mut s = SequenceSession::new();
        s.record_nextval("public.s".into(), 1);
        s.defer_sequence_drop("public.s".into());
        s.record_setval("public.s".into(), 100, true); // reobserved via setval

        s.discard_pending_drops();
        // setval doesn't set last_nextval_seq, but the prior nextval did.
        // last_nextval_seq = "public.s" which is in reobserved_drops → cleared.
        assert!(s.lastval().is_err());
        assert_eq!(s.currval("public.s").unwrap(), 1);
    }

    // -- P2 fix: savepoints cleared on commit ---------------------------------

    #[test]
    fn apply_pending_drops_clears_savepoints() {
        let mut s = SequenceSession::new();
        s.record_nextval("public.s1".into(), 1);
        s.push_savepoint("sp1".into());
        assert_eq!(s.savepoints.len(), 1);

        s.apply_pending_drops();
        assert!(s.savepoints.is_empty());
    }

    // -- Multi-cycle savepoint rollback (QG Round 8 block) --------------------

    #[test]
    fn multi_cycle_savepoint_rollback_undoes_second_cycle() {
        // Regression: reobserved_drops must not deduplicate by name.
        // Without per-cycle tracking, ROLLBACK TO s2 can't undo the second
        // drop+recreate cycle because no new entry was added.
        let mut s = SequenceSession::new();
        s.record_nextval("public.foo".into(), 1);

        // BEGIN; SAVEPOINT s1
        s.push_savepoint("s1".into());

        // First cycle: DROP foo; CREATE foo START 100; nextval(foo)
        s.defer_sequence_drop("public.foo".into());
        s.record_nextval("public.foo".into(), 100);

        // SAVEPOINT s2
        s.push_savepoint("s2".into());

        // Second cycle: DROP foo; CREATE foo START 200; nextval(foo)
        s.defer_sequence_drop("public.foo".into());
        s.record_nextval("public.foo".into(), 200);

        // ROLLBACK TO s2 — must undo second cycle only
        s.rollback_to_savepoint("s2");
        // currval must return first cycle's value (100), not second's (200)
        assert_eq!(s.currval("public.foo").unwrap(), 100);
        // lastval must error — the identity that produced 200 was rolled back
        assert!(s.lastval().is_err());
    }

    #[test]
    fn multi_cycle_commit_preserves_latest_value() {
        // Two drop+recreate cycles followed by COMMIT — last value survives.
        let mut s = SequenceSession::new();
        s.record_nextval("public.foo".into(), 1);

        s.defer_sequence_drop("public.foo".into());
        s.record_nextval("public.foo".into(), 100);

        s.defer_sequence_drop("public.foo".into());
        s.record_nextval("public.foo".into(), 200);

        s.apply_pending_drops();
        assert_eq!(s.lastval().unwrap(), 200);
        assert_eq!(s.currval("public.foo").unwrap(), 200);
    }

    #[test]
    fn multi_cycle_full_rollback_restores_original() {
        // Two drop+recreate cycles followed by full ROLLBACK.
        let mut s = SequenceSession::new();
        s.record_nextval("public.foo".into(), 1);

        s.defer_sequence_drop("public.foo".into());
        s.record_nextval("public.foo".into(), 100);

        s.defer_sequence_drop("public.foo".into());
        s.record_nextval("public.foo".into(), 200);

        s.discard_pending_drops();
        // lastval must error — both cycle identities were rolled back
        assert!(s.lastval().is_err());
        // currval must return pre-txn value
        assert_eq!(s.currval("public.foo").unwrap(), 1);
    }
}
