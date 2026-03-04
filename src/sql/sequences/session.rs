use anyhow::{anyhow, Result};
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct SequenceSession {
    per_seq: HashMap<String, i64>,
    last_value: Option<i64>,
    last_nextval_seq: Option<String>,
}

impl SequenceSession {
    pub fn new() -> Self {
        Self {
            per_seq: HashMap::new(),
            last_value: None,
            last_nextval_seq: None,
        }
    }

    pub fn record_nextval(&mut self, seq_name: String, val: i64) {
        self.per_seq.insert(seq_name.clone(), val);
        self.last_value = Some(val);
        self.last_nextval_seq = Some(seq_name);
    }

    pub fn record_setval(&mut self, seq_name: String, val: i64, is_called: bool) {
        if is_called {
            self.per_seq.insert(seq_name.clone(), val);
            if self.last_nextval_seq.as_deref() == Some(&seq_name) {
                self.last_value = Some(val);
            }
        }
    }

    pub fn currval(&self, seq_name: &str) -> Result<i64> {
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
        self.last_value
            .ok_or_else(|| anyhow!("lastval is not yet defined in this session"))
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
}
