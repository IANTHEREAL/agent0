//! Asynchronous AFTER-trigger queue primitives.
//!
//! This module is intentionally storage-agnostic: it defines the event format,
//! ID generation, and key encoding. The actual enqueueing and processing logic
//! lives in `trigger_worker`.

use crate::types::Row;
use serde::{Deserialize, Serialize};
use std::env;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const TRIGGER_QUEUE_PREFIX: &[u8] = b"_sys_tq_";
const TRIGGER_DLQ_PREFIX: &[u8] = b"_sys_tq_dlq_";

// 42-bit millisecond timestamp (fits ~139 years) + 10-bit node id + 12-bit sequence.
const NODE_BITS: u64 = 10;
const SEQ_BITS: u64 = 12;
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;
const NODE_MASK: u64 = (1 << NODE_BITS) - 1;

static NODE_ID: OnceLock<u16> = OnceLock::new();
static ID_STATE: AtomicU64 = AtomicU64::new(0); // (last_ts_ms << SEQ_BITS) | seq

fn now_ms_u64() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn now_ms_i64() -> i64 {
    i64::try_from(now_ms_u64()).unwrap_or(i64::MAX)
}

fn node_id() -> u64 {
    let v = *NODE_ID.get_or_init(|| {
        if let Ok(s) = env::var("PGTIKV_TRIGGER_NODE_ID") {
            if let Ok(v) = s.parse::<u16>() {
                if (v as u64) <= NODE_MASK {
                    return v;
                }
            }
        }

        // Best-effort fallback: derive a node id that is *very likely* to differ across
        // processes/nodes without requiring explicit configuration.
        //
        // For real multi-node deployments, set `PGTIKV_TRIGGER_NODE_ID` (unique per node)
        // to avoid any possibility of collision.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV-1a offset basis
        if let Ok(host) = env::var("HOSTNAME") {
            for b in host.as_bytes() {
                h ^= u64::from(*b);
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        let pid = std::process::id();
        for b in pid.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let r = rand::random::<u64>();
        for b in r.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }

        (h & NODE_MASK) as u16
    });
    (v as u64) & NODE_MASK
}

/// Generate a roughly time-ordered, cluster-safe trigger event ID.
///
/// Layout (MSB → LSB):
/// - 42 bits: milliseconds since unix epoch
/// - 10 bits: node id (`PGTIKV_TRIGGER_NODE_ID`, 0..=1023)
/// - 12 bits: per-millisecond sequence (0..=4095)
pub(crate) fn generate_event_id() -> u64 {
    let node = node_id();

    // Snowflake-style monotonic ID generator.
    // If the clock moves backwards, we stick to the last seen timestamp.
    let mut now_ms = now_ms_u64();
    loop {
        let state = ID_STATE.load(Ordering::Relaxed);
        let last_ts = state >> SEQ_BITS;
        let last_seq = state & SEQ_MASK;

        if now_ms < last_ts {
            now_ms = last_ts;
        }

        if now_ms == last_ts {
            if last_seq == SEQ_MASK {
                // Sequence overflow within the same millisecond: spin until time advances.
                now_ms = now_ms_u64();
                continue;
            }
            let seq = last_seq + 1;
            let new_state = (last_ts << SEQ_BITS) | seq;
            if ID_STATE
                .compare_exchange(state, new_state, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return (last_ts << (NODE_BITS + SEQ_BITS)) | (node << SEQ_BITS) | seq;
            }
            continue;
        }

        // now_ms > last_ts
        let new_state = (now_ms << SEQ_BITS) | 0;
        if ID_STATE
            .compare_exchange(state, new_state, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return (now_ms << (NODE_BITS + SEQ_BITS)) | (node << SEQ_BITS);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum EventStatus {
    Pending,
    Processing,
    Done,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) enum TriggerOp {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct TriggerEvent {
    pub id: u64,
    pub trigger_name: String,
    pub db_id: u64,
    pub table_name: String,
    pub operation: TriggerOp,
    pub old_row: Option<Row>,
    pub new_row: Option<Row>,
    pub created_at_ms: i64,
    pub status: EventStatus,
    pub retry_count: u8,
    pub error_msg: Option<String>,
    pub worker_id: Option<String>,
    pub claimed_at_ms: Option<i64>,
}

impl TriggerEvent {
    pub(crate) fn new_pending(
        trigger_name: String,
        db_id: u64,
        table_name: String,
        operation: TriggerOp,
        old_row: Option<Row>,
        new_row: Option<Row>,
    ) -> Self {
        Self {
            id: generate_event_id(),
            trigger_name,
            db_id,
            table_name,
            operation,
            old_row,
            new_row,
            created_at_ms: now_ms_i64(),
            status: EventStatus::Pending,
            retry_count: 0,
            error_msg: None,
            worker_id: None,
            claimed_at_ms: None,
        }
    }
}

pub(crate) fn encode_trigger_queue_key(id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(TRIGGER_QUEUE_PREFIX.len() + 8);
    key.extend_from_slice(TRIGGER_QUEUE_PREFIX);
    key.extend_from_slice(&id.to_be_bytes());
    key
}

pub(crate) fn encode_trigger_queue_prefix() -> Vec<u8> {
    TRIGGER_QUEUE_PREFIX.to_vec()
}

pub(crate) fn encode_trigger_dlq_key(id: u64) -> Vec<u8> {
    let mut key = Vec::with_capacity(TRIGGER_DLQ_PREFIX.len() + 8);
    key.extend_from_slice(TRIGGER_DLQ_PREFIX);
    key.extend_from_slice(&id.to_be_bytes());
    key
}

pub(crate) fn encode_trigger_dlq_prefix() -> Vec<u8> {
    TRIGGER_DLQ_PREFIX.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn trigger_queue_key_orders_by_id() {
        let id1 = generate_event_id();
        let id2 = generate_event_id();
        assert!(id2 > id1);
        assert!(encode_trigger_queue_key(id1) < encode_trigger_queue_key(id2));
    }

    #[tokio::test]
    async fn event_id_increases_over_time() {
        let id1 = generate_event_id();
        tokio::time::sleep(Duration::from_millis(2)).await;
        let id2 = generate_event_id();
        assert!(id2 > id1);
    }

    #[test]
    fn trigger_event_bincode_roundtrip() {
        let ev = TriggerEvent::new_pending(
            "t".to_string(),
            1,
            "public.tbl".to_string(),
            TriggerOp::Insert,
            None,
            None,
        );
        let bytes = bincode::serialize(&ev).unwrap();
        let decoded: TriggerEvent = bincode::deserialize(&bytes).unwrap();
        assert_eq!(decoded.id, ev.id);
        assert_eq!(decoded.trigger_name, "t");
        assert_eq!(decoded.table_name, "public.tbl");
        assert_eq!(decoded.operation, TriggerOp::Insert);
        assert_eq!(decoded.status, EventStatus::Pending);
    }
}
