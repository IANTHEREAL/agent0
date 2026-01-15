use anyhow::{anyhow, Result};
use std::collections::HashMap;

/// One undo operation: restore `key` to `prev` (`None` means delete).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UndoRecord {
    pub(crate) key: Vec<u8>,
    pub(crate) prev: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub(crate) struct SavepointManager {
    stack: Vec<Savepoint>,
}

#[derive(Debug, Default)]
pub(crate) struct Savepoint {
    pub(crate) name: String,
    pub(crate) undo: HashMap<Vec<u8>, Option<Vec<u8>>>,
}

/// State captured while performing `ROLLBACK TO SAVEPOINT`.
///
/// The rollback is applied to the TiKV transaction asynchronously.
#[derive(Debug)]
pub(crate) struct PreparedRollback {
    pub(crate) popped: Vec<Savepoint>,
    pub(crate) target_undo: Vec<UndoRecord>,
}

impl SavepointManager {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn reset(&mut self) {
        self.stack.clear();
    }

    #[inline]
    pub(crate) fn has_savepoints(&self) -> bool {
        !self.stack.is_empty()
    }

    pub(crate) fn create(&mut self, name: String) {
        self.stack.push(Savepoint {
            name,
            undo: HashMap::new(),
        });
    }

    pub(crate) fn release(&mut self, name: &str) -> Result<()> {
        // PostgreSQL allows duplicate savepoint names: only the most recent
        // identically-named savepoint is accessible until it is released.
        let target_index = self
            .stack
            .iter()
            .rposition(|sp| sp.name == name)
            .ok_or_else(|| anyhow!("savepoint \"{}\" does not exist", name))?;

        // RELEASE destroys the named savepoint and all later savepoints.
        if target_index == 0 {
            self.stack.clear();
            return Ok(());
        }

        let mut released = self.stack.split_off(target_index);
        let parent = self
            .stack
            .last_mut()
            .expect("target_index > 0 implies parent exists");

        // Merge released savepoints into parent in outer→inner order, so the
        // earliest prev value wins when the same key appears multiple times.
        for sp in released.iter_mut() {
            for (key, prev) in sp.undo.drain() {
                parent.undo.entry(key).or_insert(prev);
            }
        }
        Ok(())
    }

    pub(crate) fn should_record_key(&self, key: &[u8]) -> bool {
        let Some(current) = self.stack.last() else {
            return false;
        };
        !current.undo.contains_key(key)
    }

    pub(crate) fn record_prev_value(&mut self, key: Vec<u8>, prev: Option<Vec<u8>>) {
        let Some(current) = self.stack.last_mut() else {
            return;
        };
        current.undo.entry(key).or_insert(prev);
    }

    pub(crate) fn prepare_rollback_to(&mut self, name: &str) -> Result<PreparedRollback> {
        // PostgreSQL allows duplicate savepoint names: rollback targets the
        // most recent identically-named savepoint.
        let target_index = self
            .stack
            .iter()
            .rposition(|sp| sp.name == name)
            .ok_or_else(|| anyhow!("savepoint \"{}\" does not exist", name))?;

        // Pop (destroy) nested savepoints above target.
        let popped = self.stack.split_off(target_index + 1);

        // Drain the target's undo to re-establish the savepoint after undo.
        let target_undo = self
            .stack
            .get_mut(target_index)
            .expect("target_index is within bounds")
            .undo
            .drain()
            .map(|(key, prev)| UndoRecord { key, prev })
            .collect();

        Ok(PreparedRollback {
            popped,
            target_undo,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_to_removes_nested_and_reestablishes_target() {
        let mut m = SavepointManager::new();
        m.create("a".to_string());
        m.record_prev_value(b"k1".to_vec(), None);
        m.create("b".to_string());
        m.record_prev_value(b"k2".to_vec(), Some(vec![2]));

        let prepared = m.prepare_rollback_to("a").unwrap();
        // Nested savepoint removed immediately.
        assert!(m.has_savepoints());
        assert_eq!(m.stack.len(), 1);
        // Target undo drained (re-established).
        assert!(m.stack[0].undo.is_empty());
        assert_eq!(prepared.target_undo.len(), 1);
    }

    #[test]
    fn rollback_to_uses_most_recent_name() {
        let mut m = SavepointManager::new();
        m.create("a".to_string());
        m.record_prev_value(b"k1".to_vec(), Some(vec![1]));
        m.create("a".to_string());
        m.record_prev_value(b"k2".to_vec(), Some(vec![2]));

        let prepared = m.prepare_rollback_to("a").unwrap();
        // Should roll back to the most recent "a" only, so stack keeps both.
        assert_eq!(m.stack.len(), 2);
        assert_eq!(prepared.target_undo.len(), 1);
        assert_eq!(prepared.target_undo[0].key, b"k2".to_vec());
    }

    #[test]
    fn release_destroys_named_and_nested_savepoints() {
        let mut m = SavepointManager::new();
        m.create("a".to_string());
        m.create("b".to_string());
        m.create("c".to_string());

        m.release("b").unwrap();
        assert_eq!(m.stack.len(), 1);
        assert_eq!(m.stack[0].name, "a");
    }

    #[test]
    fn release_merges_undo_outer_first() {
        let mut m = SavepointManager::new();
        m.create("a".to_string());
        m.create("b".to_string());
        // Key first changed in b; prev should be kept if inner savepoint also changes it.
        m.record_prev_value(b"k".to_vec(), Some(vec![0]));
        m.create("c".to_string());
        // Inner savepoint sees later prev.
        m.record_prev_value(b"k".to_vec(), Some(vec![1]));

        m.release("b").unwrap();
        assert_eq!(m.stack.len(), 1);
        let parent = &m.stack[0];
        assert_eq!(parent.undo.get(b"k".as_slice()).cloned().flatten(), Some(vec![0]));
    }

    #[test]
    fn release_outermost_clears_stack() {
        let mut m = SavepointManager::new();
        m.create("a".to_string());
        m.create("b".to_string());
        m.release("a").unwrap();
        assert!(!m.has_savepoints());
    }
}
