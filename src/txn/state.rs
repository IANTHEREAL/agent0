use super::savepoints::{PreparedRollback, SavepointManager};
use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

/// Session-scoped savepoint state.
///
/// This wraps [`SavepointManager`] with a mutex for interior mutability and an
/// atomic "active" fast-path flag so that the hot write path can quickly skip
/// undo logging when no savepoints are present.
#[derive(Debug)]
pub(crate) struct SavepointState {
    active: AtomicBool,
    manager: Mutex<SavepointManager>,
}

impl SavepointState {
    pub(crate) fn new() -> Self {
        Self {
            active: AtomicBool::new(false),
            manager: Mutex::new(SavepointManager::new()),
        }
    }

    #[inline]
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) async fn reset(&self) -> Result<()> {
        let mut manager = self.manager.lock().await;
        manager.reset();
        self.active.store(false, Ordering::Release);
        Ok(())
    }

    pub(crate) async fn create(&self, name: String) -> Result<()> {
        let mut manager = self.manager.lock().await;
        manager.create(name);
        self.active.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) async fn release(&self, name: &str) -> Result<()> {
        let mut manager = self.manager.lock().await;
        let res = manager.release(name);
        self.active
            .store(manager.has_savepoints(), Ordering::Release);
        res
    }

    pub(crate) async fn prepare_rollback_to(&self, name: &str) -> Result<PreparedRollback> {
        let mut manager = self.manager.lock().await;
        let prepared = manager.prepare_rollback_to(name)?;
        self.active
            .store(manager.has_savepoints(), Ordering::Release);
        Ok(prepared)
    }

    #[inline]
    pub(crate) async fn should_record_key(&self, key: &[u8]) -> Result<bool> {
        if !self.is_active() {
            return Ok(false);
        }
        let manager = self.manager.lock().await;
        Ok(manager.should_record_key(key))
    }

    #[inline]
    pub(crate) async fn record_prev_value(
        &self,
        key: Vec<u8>,
        prev: Option<Vec<u8>>,
    ) -> Result<()> {
        if !self.is_active() {
            return Ok(());
        }
        let mut manager = self.manager.lock().await;
        manager.record_prev_value(key, prev);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SavepointState;

    #[tokio::test]
    async fn active_flag_tracks_create_release_and_reset() {
        let state = SavepointState::new();
        assert!(!state.is_active());

        state.create("sp1".to_string()).await.unwrap();
        assert!(state.is_active());

        state.release("sp1").await.unwrap();
        assert!(!state.is_active());

        state.create("sp2".to_string()).await.unwrap();
        assert!(state.is_active());
        state.reset().await.unwrap();
        assert!(!state.is_active());
    }

    #[tokio::test]
    async fn should_record_key_changes_after_first_record_in_savepoint() {
        let state = SavepointState::new();
        state.create("sp".to_string()).await.unwrap();

        assert!(state.should_record_key(b"k1").await.unwrap());
        state
            .record_prev_value(b"k1".to_vec(), Some(vec![1]))
            .await
            .unwrap();
        assert!(!state.should_record_key(b"k1").await.unwrap());
        assert!(state.should_record_key(b"k2").await.unwrap());
    }

    #[tokio::test]
    async fn rollback_to_keeps_target_savepoint_active_and_clears_target_undo() {
        let state = SavepointState::new();
        state.create("a".to_string()).await.unwrap();
        state.record_prev_value(b"k1".to_vec(), None).await.unwrap();
        state.create("b".to_string()).await.unwrap();
        state
            .record_prev_value(b"k2".to_vec(), Some(vec![2]))
            .await
            .unwrap();

        let prepared = state.prepare_rollback_to("a").await.unwrap();
        assert_eq!(prepared.target_undo.len(), 1);
        assert_eq!(prepared.popped.len(), 1);
        assert!(state.is_active());
        assert!(state.should_record_key(b"k1").await.unwrap());
    }

    #[tokio::test]
    async fn record_and_should_record_noop_when_inactive() {
        let state = SavepointState::new();
        assert!(!state.should_record_key(b"k").await.unwrap());
        state
            .record_prev_value(b"k".to_vec(), Some(vec![7]))
            .await
            .unwrap();
        assert!(!state.should_record_key(b"k").await.unwrap());
    }
}
