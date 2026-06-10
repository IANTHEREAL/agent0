//! Buffers whose statement-memory charge is structurally coupled to the
//! allocation they cover (#2555 / #2612).
//!
//! Review rounds on #2612 kept finding desyncs between buffers and their
//! hand-maintained charge counters: rows moved out but still charged; a
//! drained page charged alongside its successor; `Vec::clear()` keeping the
//! slot allocation after its charge was released; a full Vec's doubling
//! reallocation admitted only *after* it happened; the ordered-aggregate
//! buffer's slot charge surviving its own deallocation. The shared root was
//! bookkeeping split across two fields with manually paired grow/shrink at
//! every transition.
//!
//! [`ChargedBuf`] removes the class: the items and their charge have one
//! owner, every transition is a method that updates both, growth is admitted
//! **before** it is allocated (explicit doubling policy + `reserve_exact`,
//! never a prediction of std's private growth), and dropping the buffer
//! releases the charge together with the allocation. Operators holding
//! charged items across method boundaries must use this type; raw
//! `try_grow`/`try_shrink` pairs are acceptable only when the grow and its
//! matching release are both visible within a single function body.
//!
//! Invariant (locked in by the tests below): at every method boundary —
//! including after a failed push — `charged_bytes` covers the payload of
//! items not yet taken plus the slot capacity of the live allocation, and
//! `reset()` frees the allocation (capacity returns to zero) together with
//! the charge.

use anyhow::Result;

use crate::model::Row;
use crate::pool::{try_grow_statement_memory_scope, try_shrink_statement_memory_scope};
use crate::sql::memory::{estimate_row_size, estimate_values_payload_size};

/// An element type a [`ChargedBuf`] can account for.
pub(crate) trait ChargedEntry {
    /// Estimated heap payload of this entry (slot bytes are accounted by the
    /// buffer separately; including struct size here merely over-counts,
    /// which is safe).
    fn charged_size(&self) -> usize;
    /// An empty placeholder left behind when an entry is taken by move.
    fn hollow() -> Self;
}

impl ChargedEntry for Row {
    fn charged_size(&self) -> usize {
        estimate_row_size(self)
    }
    fn hollow() -> Self {
        Row::new(Vec::new())
    }
}

/// A primary key (one `Vec<Value>` per row) admitted by index scans. The match
/// set produced by an index lookup is O(matches) and was previously held in a
/// raw `Vec<Vec<Value>>` outside any quota; routing it through a `ChargedBuf`
/// bounds the scan input the same way `ChargedRowBuffer` bounds materialized
/// rows (#2612 gate-review follow-up).
impl ChargedEntry for Vec<crate::model::Value> {
    fn charged_size(&self) -> usize {
        estimate_values_payload_size(self)
    }
    fn hollow() -> Self {
        Vec::new()
    }
}

#[derive(Debug)]
pub(crate) struct ChargedBuf<T: ChargedEntry> {
    items: Vec<T>,
    position: usize,
    charged_bytes: usize,
}

pub(crate) type ChargedRowBuffer = ChargedBuf<Row>;

/// A charged buffer of primary keys, used by index scans to bound the
/// `pk_queue` match set against the tenant memory quota.
pub(crate) type ChargedPkBuffer = ChargedBuf<Vec<crate::model::Value>>;

fn slot_bytes<T>(capacity: usize) -> usize {
    capacity.saturating_mul(std::mem::size_of::<T>())
}

impl<T: ChargedEntry> Default for ChargedBuf<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: ChargedEntry> ChargedBuf<T> {
    pub(crate) fn new() -> Self {
        Self {
            items: Vec::new(),
            position: 0,
            charged_bytes: 0,
        }
    }

    /// Append one item, admitting its payload **and** any slot growth before
    /// anything is allocated: when the Vec is full, the next capacity is
    /// chosen explicitly (doubling) and reserved with `reserve_exact` only
    /// after the charge succeeds, so a full accumulating buffer can never
    /// create a state-sized backing allocation ahead of quota admission. On
    /// failure nothing was allocated and nothing is charged.
    pub(crate) fn push(&mut self, component: &'static str, item: T) -> Result<()> {
        let payload = item.charged_size();
        let needs_growth = self.items.len() == self.items.capacity();
        let target_capacity = if needs_growth {
            std::cmp::max(4, self.items.capacity().saturating_mul(2))
        } else {
            self.items.capacity()
        };
        let grown_slots =
            slot_bytes::<T>(target_capacity).saturating_sub(slot_bytes::<T>(self.items.capacity()));
        let total = payload.saturating_add(grown_slots);
        try_grow_statement_memory_scope(component, total)?;
        if needs_growth {
            self.items.reserve_exact(target_capacity - self.items.len());
        }
        self.items.push(item);
        self.charged_bytes = self.charged_bytes.saturating_add(total);
        Ok(())
    }

    /// Replace the buffer with a freshly fetched batch: the old page's charge
    /// and allocation are released first (callers only replace after
    /// draining), then the new batch is admitted measured from its real
    /// items. On quota failure the buffer is left empty and uncharged.
    pub(crate) fn adopt(&mut self, component: &'static str, items: Vec<T>) -> Result<()> {
        self.reset();
        let page_bytes = items
            .iter()
            .map(ChargedEntry::charged_size)
            .sum::<usize>()
            .saturating_add(slot_bytes::<T>(items.capacity()));
        try_grow_statement_memory_scope(component, page_bytes)?;
        self.items = items;
        self.position = 0;
        self.charged_bytes = page_bytes;
        Ok(())
    }

    /// Adopt items whose payload charge of `precharged_bytes` was already
    /// made by the producer (e.g. `cop_select`), taking ownership of that
    /// charge. The slot capacity of the adopted Vec is NOT covered by
    /// producer charges, so it is admitted here — every entry path must
    /// uphold the payload-plus-slots invariant, or O(items) slot memory
    /// rides unaccounted.
    pub(crate) fn adopt_precharged(
        &mut self,
        component: &'static str,
        items: Vec<T>,
        precharged_bytes: usize,
    ) -> Result<()> {
        self.reset();
        let slots = slot_bytes::<T>(items.capacity());
        if let Err(e) = try_grow_statement_memory_scope(component, slots) {
            // Adoption failed: the items are dropped here, so the producer's
            // charge over their payload is released with them — otherwise
            // the scope stays over-charged by the whole batch on this error
            // path.
            drop(items);
            try_shrink_statement_memory_scope(precharged_bytes);
            return Err(e.into());
        }
        self.items = items;
        self.position = 0;
        self.charged_bytes = precharged_bytes.saturating_add(slots);
        Ok(())
    }

    /// Move the next item out, releasing its payload share of the charge with
    /// the transfer (the consumer charges its own retention; keeping moved
    /// payload charged here would double-charge the live result set). The
    /// release is clamped so the total released never exceeds what was
    /// charged. Slot capacity stays charged until `reset()`.
    pub(crate) fn take_next(&mut self) -> Option<T> {
        if self.position >= self.items.len() {
            return None;
        }
        let item = std::mem::replace(&mut self.items[self.position], T::hollow());
        self.position += 1;
        let payload = item.charged_size().min(self.charged_bytes);
        try_shrink_statement_memory_scope(payload);
        self.charged_bytes -= payload;
        Some(item)
    }

    /// In-place access for reordering (e.g. ORDER BY sorts). Callers must not
    /// change element sizes — the charge is not re-measured.
    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.items
    }

    /// Release the remaining charge together with the allocation. The Vec is
    /// replaced, not cleared: `clear()` would keep the slot capacity alive
    /// after its charge was released, and on reuse the capacity would never
    /// be recharged because it no longer grows.
    pub(crate) fn reset(&mut self) {
        try_shrink_statement_memory_scope(self.charged_bytes);
        self.charged_bytes = 0;
        self.items = Vec::new();
        self.position = 0;
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.items.len()
    }

    #[cfg(test)]
    fn charged_bytes(&self) -> usize {
        self.charged_bytes
    }

    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.items.capacity()
    }
}

impl<T: ChargedEntry> Drop for ChargedBuf<T> {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Value;
    use crate::pool::{run_with_statement_memory_scope, TenantMemoryAccountant};

    fn row(i: i32, payload: usize) -> Row {
        Row::new(vec![Value::Int32(i), Value::Text("x".repeat(payload))])
    }

    #[test]
    fn push_take_reset_keep_charge_and_rows_in_lockstep() {
        let mut buf = ChargedRowBuffer::new();
        assert!(buf.take_next().is_none());

        let mut pushed = 0usize;
        for i in 0..10 {
            let r = row(i, 64);
            pushed += estimate_row_size(&r);
            buf.push("test.rows", r).unwrap();
        }
        assert!(
            buf.charged_bytes() >= pushed,
            "charge {} must cover payload {} plus slots",
            buf.charged_bytes(),
            pushed
        );
        assert!(buf.charged_bytes() >= slot_bytes::<Row>(buf.capacity()));

        for i in 0..10 {
            let r = buf.take_next().unwrap();
            assert_eq!(r.values[0], Value::Int32(i));
        }
        assert!(buf.take_next().is_none());
        // Payload shares were released with each transfer; only slot capacity
        // (and estimate slack) remains charged.
        assert!(buf.charged_bytes() <= slot_bytes::<Row>(buf.capacity()) * 2);

        buf.reset();
        assert_eq!(buf.charged_bytes(), 0);
        assert_eq!(
            buf.capacity(),
            0,
            "reset must free the allocation, not clear() it: a kept capacity \
             would be uncharged and never recharged on reuse"
        );
    }

    #[test]
    fn adopt_swaps_pages_without_holding_both_charges() {
        let mut buf = ChargedRowBuffer::new();
        buf.adopt("test.page", (0..8).map(|i| row(i, 128)).collect())
            .unwrap();
        let first_charge = buf.charged_bytes();
        assert!(first_charge > 0);
        while buf.take_next().is_some() {}

        buf.adopt("test.page", vec![row(99, 16)]).unwrap();
        assert!(
            buf.charged_bytes() < first_charge,
            "after adopt only the new page may remain charged"
        );
        assert_eq!(buf.take_next().unwrap().values[0], Value::Int32(99));
        buf.reset();
        assert_eq!(buf.charged_bytes(), 0);
        assert_eq!(buf.capacity(), 0);
    }

    #[tokio::test]
    async fn adopt_precharged_admits_slot_capacity_and_clamps_release() {
        let accountant = TenantMemoryAccountant::unlimited("ks_precharged".to_string());
        run_with_statement_memory_scope(Some(accountant.clone()), 0, async {
            let rows: Vec<Row> = (0..4).map(|i| row(i, 256)).collect();
            let slots = slot_bytes::<Row>(rows.capacity());
            // Simulate a producer that already charged 100 payload bytes.
            try_grow_statement_memory_scope("test.producer", 100).unwrap();

            let mut buf = ChargedRowBuffer::new();
            buf.adopt_precharged("test.cop", rows, 100).unwrap();
            assert_eq!(buf.charged_bytes(), 100 + slots);
            assert_eq!(
                accountant.used_bytes(),
                100 + slots,
                "slot capacity must be admitted even on the precharged path"
            );

            // Producer charged less than the payload estimate; releases must
            // clamp instead of underflowing or over-releasing.
            while buf.take_next().is_some() {}
            assert_eq!(accountant.used_bytes(), buf.charged_bytes());
            buf.reset();
            assert_eq!(buf.charged_bytes(), 0);
            assert_eq!(accountant.used_bytes(), 0);
        })
        .await;
        assert_eq!(accountant.used_bytes(), 0);
    }

    #[tokio::test]
    async fn scope_accounting_reconciles_to_zero() {
        let accountant = TenantMemoryAccountant::unlimited("ks_charged_rows".to_string());
        run_with_statement_memory_scope(Some(accountant.clone()), 0, async {
            let mut buf = ChargedRowBuffer::new();
            for i in 0..6 {
                buf.push("test.rows", row(i, 512)).unwrap();
            }
            assert_eq!(accountant.used_bytes(), buf.charged_bytes());

            let taken = buf.take_next().unwrap();
            assert_eq!(
                accountant.used_bytes(),
                buf.charged_bytes(),
                "moving a row out must release its share from the scope"
            );
            drop(taken);

            buf.adopt("test.page", vec![row(42, 64)]).unwrap();
            assert_eq!(accountant.used_bytes(), buf.charged_bytes());

            drop(buf);
            assert_eq!(
                accountant.used_bytes(),
                0,
                "dropping the buffer must release everything it still charged"
            );
        })
        .await;
        assert_eq!(accountant.used_bytes(), 0);
    }

    #[tokio::test]
    async fn quota_failure_allocates_nothing_and_leaves_buffer_consistent() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_quota".to_string(), 1024);
        run_with_statement_memory_scope(Some(accountant.clone()), 0, async {
            let mut buf = ChargedRowBuffer::new();
            // A page far larger than the quota must be rejected wholesale.
            let err = buf.adopt("test.page", (0..16).map(|i| row(i, 4096)).collect());
            assert!(err.is_err());
            assert_eq!(buf.charged_bytes(), 0);
            assert_eq!(buf.len(), 0);
            assert_eq!(accountant.used_bytes(), 0);

            // Failed precharged adoption must release the producer's charge
            // along with the dropped items, not strand it in the scope.
            try_grow_statement_memory_scope("test.producer", 512).unwrap();
            let mut wide = Vec::with_capacity(4096);
            wide.push(row(0, 16));
            assert!(buf.adopt_precharged("test.cop", wide, 512).is_err());
            assert_eq!(buf.charged_bytes(), 0);
            assert_eq!(
                accountant.used_bytes(),
                0,
                "producer charge must be released when adoption fails"
            );

            // A failed push allocates nothing: admission precedes both the
            // slot growth and the item insertion.
            buf.push("test.rows", row(0, 64)).unwrap();
            let len_before = buf.len();
            let cap_before = buf.capacity();
            let charged_before = buf.charged_bytes();
            assert!(buf.push("test.rows", row(1, 8192)).is_err());
            assert_eq!(buf.len(), len_before);
            assert_eq!(
                buf.capacity(),
                cap_before,
                "a failed push must not have grown the allocation"
            );
            assert_eq!(buf.charged_bytes(), charged_before);
            assert_eq!(accountant.used_bytes(), charged_before);
            // The invariant holds for further pushes after the failure.
            buf.push("test.rows", row(2, 64)).unwrap();
            assert!(buf.charged_bytes() >= slot_bytes::<Row>(buf.capacity()));
            assert_eq!(accountant.used_bytes(), buf.charged_bytes());
        })
        .await;
        assert_eq!(accountant.used_bytes(), 0);
    }

    fn pk(i: i32) -> Vec<Value> {
        vec![Value::Int32(i), Value::Text("k".repeat(32))]
    }

    #[tokio::test]
    async fn charged_pk_buffer_bounds_and_reconciles() {
        let accountant = TenantMemoryAccountant::unlimited("ks_pk".to_string());
        run_with_statement_memory_scope(Some(accountant.clone()), 0, async {
            let mut buf = ChargedPkBuffer::new();
            let mut payload = 0usize;
            for i in 0..8 {
                let k = pk(i);
                payload += estimate_values_payload_size(&k);
                buf.push("test.pk", k).unwrap();
            }
            assert_eq!(accountant.used_bytes(), buf.charged_bytes());
            assert!(
                buf.charged_bytes() >= payload,
                "charge must cover PK payload"
            );

            for i in 0..8 {
                let k = buf.take_next().unwrap();
                assert_eq!(k[0], Value::Int32(i));
                assert_eq!(
                    accountant.used_bytes(),
                    buf.charged_bytes(),
                    "taking a PK out must release its payload share"
                );
            }
            assert!(buf.take_next().is_none());

            drop(buf);
            assert_eq!(accountant.used_bytes(), 0, "drop must reconcile to zero");
        })
        .await;
        assert_eq!(accountant.used_bytes(), 0);
    }

    #[tokio::test]
    async fn charged_pk_buffer_aborts_over_quota_allocating_nothing() {
        let accountant = TenantMemoryAccountant::new_with_quota("ks_pk_quota".to_string(), 256);
        run_with_statement_memory_scope(Some(accountant.clone()), 0, async {
            let mut buf = ChargedPkBuffer::new();
            // A PK far larger than the quota is rejected with 53200; nothing
            // is allocated or charged — the index-scan OOM becomes a query error.
            let huge = vec![Value::Text("x".repeat(4096))];
            let err = buf.push("test.pk", huge);
            assert!(err.is_err());
            assert_eq!(buf.charged_bytes(), 0);
            assert_eq!(buf.len(), 0);
            assert_eq!(accountant.used_bytes(), 0);
        })
        .await;
        assert_eq!(accountant.used_bytes(), 0);
    }
}
