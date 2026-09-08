//! Normalized chain-event models and deterministic event identity.

use crate::market::models::DexKind;
use alloy::primitives::{Address, B256};
use serde::{Deserialize, Serialize};

/// Deterministic, chain-derived identity for an event, used for
/// deduplication (see `events::dedup`). Two logs with the same
/// `(chain_id, tx_hash, log_index)` are always the same event, regardless of
/// how many times a provider redelivers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId {
    pub chain_id: u64,
    pub tx_hash: B256,
    pub log_index: u64,
}

impl std::fmt::Display for EventId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.chain_id, self.tx_hash, self.log_index)
    }
}

/// Normalized swap event. Amounts are signed net flow into the pool, in the
/// token's native integer units (no decimal scaling applied here - that is a
/// display-layer concern, not a storage-layer one).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwapEvent {
    pub pool_address: Address,
    pub dex: DexKind,
    /// Positive = pool received token0, negative = pool paid out token0.
    pub amount0: i128,
    /// Positive = pool received token1, negative = pool paid out token1.
    pub amount1: i128,
    pub sender: Option<Address>,
    pub recipient: Option<Address>,
}

/// The kind of chain-level event this represents. Day 1 focuses on Swap;
/// other variants are recorded so the decoder doesn't have to discard
/// information it does receive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    Swap(SwapEvent),
    Sync,
    Mint,
    Burn,
    Unknown(String),
}

/// A fully normalized market event with full provenance and ingestion
/// telemetry, ready to be folded into `MarketState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketEvent {
    pub id: EventId,
    pub block_number: u64,
    pub block_timestamp: Option<u64>,
    pub tx_hash: B256,
    pub log_index: u64,
    pub pool_address: Address,
    pub dex: DexKind,
    pub kind: EventKind,

    /// Ingestion telemetry. All `*_at_us` fields are microseconds since the
    /// Unix epoch; `processing_latency_us` is a duration.
    pub received_at_us: u64,
    pub processing_started_at_us: u64,
    pub processing_finished_at_us: u64,
    pub processing_latency_us: u64,
}

impl MarketEvent {
    pub fn new(
        chain_id: u64,
        block_number: u64,
        block_timestamp: Option<u64>,
        tx_hash: B256,
        log_index: u64,
        pool_address: Address,
        dex: DexKind,
        kind: EventKind,
        received_at_us: u64,
        processing_started_at_us: u64,
        processing_finished_at_us: u64,
    ) -> Self {
        let processing_latency_us =
            processing_finished_at_us.saturating_sub(processing_started_at_us);
        MarketEvent {
            id: EventId {
                chain_id,
                tx_hash,
                log_index,
            },
            block_number,
            block_timestamp,
            tx_hash,
            log_index,
            pool_address,
            dex,
            kind,
            received_at_us,
            processing_started_at_us,
            processing_finished_at_us,
            processing_latency_us,
        }
    }
}
