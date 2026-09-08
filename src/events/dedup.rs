//! Deterministic event deduplication.
//!
//! Reconnects, provider-side redelivery, and overlapping historical backfill
//! + live subscription windows can all cause the same log to arrive more
//! than once. This module gives the ingestion pipeline a single, cheap check:
//! first arrival processes, every subsequent arrival is safely ignored.

use crate::events::model::EventId;
use std::collections::HashSet;

#[derive(Debug, Default)]
pub struct Deduplicator {
    seen: HashSet<EventId>,
}

impl Deduplicator {
    pub fn new() -> Self {
        Deduplicator {
            seen: HashSet::new(),
        }
    }

    /// Records `id` as seen. Returns `true` if this is the first time this
    /// id has been observed (i.e. the caller should process the event), and
    /// `false` if it is a duplicate (i.e. the caller should ignore it).
    pub fn record(&mut self, id: EventId) -> bool {
        self.seen.insert(id)
    }

    pub fn is_duplicate(&self, id: &EventId) -> bool {
        self.seen.contains(id)
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;

    fn sample_id(log_index: u64) -> EventId {
        EventId {
            chain_id: 8453,
            tx_hash: B256::repeat_byte(0xAB),
            log_index,
        }
    }

    #[test]
    fn first_arrival_is_processed() {
        let mut dedup = Deduplicator::new();
        let id = sample_id(0);
        assert!(!dedup.is_duplicate(&id));
        assert!(dedup.record(id), "first arrival should be accepted");
    }

    #[test]
    fn duplicate_arrival_is_ignored() {
        let mut dedup = Deduplicator::new();
        let id = sample_id(1);
        assert!(dedup.record(id), "first arrival accepted");
        assert!(dedup.is_duplicate(&id));
        assert!(!dedup.record(id), "second arrival must be rejected");
        assert_eq!(dedup.len(), 1, "duplicate must not create a second entry");
    }

    #[test]
    fn distinct_log_index_is_a_distinct_event() {
        let mut dedup = Deduplicator::new();
        assert!(dedup.record(sample_id(0)));
        assert!(dedup.record(sample_id(1)));
        assert_eq!(dedup.len(), 2);
    }

    #[test]
    fn event_id_is_stable_and_deterministic() {
        let a = sample_id(5);
        let b = sample_id(5);
        assert_eq!(a, b, "identical (chain_id, tx_hash, log_index) must be equal");
        assert_eq!(a.to_string(), b.to_string());
    }
}
