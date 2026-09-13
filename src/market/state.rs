//! Deterministic in-memory market state.
//!
//! `MarketState` is intentionally NOT thread-safe on its own - callers share
//! it behind `SharedMarketState` (`Arc<RwLock<MarketState>>`). Every accepted
//! mutation advances `state_version` by exactly one; duplicate events never
//! mutate anything.

use crate::events::dedup::Deduplicator;
use crate::events::model::MarketEvent;
use crate::market::models::{BlockState, Freshness, Pool, PoolState, StateVersion};
use alloy::primitives::Address;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Shared handle used across the ingestion pipeline.
pub type SharedMarketState = Arc<RwLock<MarketState>>;

/// How many recently-processed events to retain in memory for inspection /
/// debugging. Bounded so a long-running process doesn't grow unbounded.
const RECENT_EVENTS_CAPACITY: usize = 1024;

#[derive(Debug)]
pub struct MarketState {
    pub latest_block: Option<BlockState>,
    pub state_version: StateVersion,
    pools: HashMap<Address, PoolState>,
    recent_events: VecDeque<MarketEvent>,
    dedup: Deduplicator,
}

impl Default for MarketState {
    fn default() -> Self {
        Self::new()
    }
}

impl MarketState {
    pub fn new() -> Self {
        MarketState {
            latest_block: None,
            state_version: StateVersion::genesis(),
            pools: HashMap::new(),
            recent_events: VecDeque::with_capacity(RECENT_EVENTS_CAPACITY),
            dedup: Deduplicator::new(),
        }
    }

    pub fn new_shared() -> SharedMarketState {
        Arc::new(RwLock::new(MarketState::new()))
    }

    /// Register (or overwrite the definition of) a pool the engine tracks.
    /// This does not count as a market-data state transition on its own -
    /// it's configuration, not an observed chain event - so it does not
    /// advance `state_version`.
    pub fn register_pool(&mut self, pool: Pool, block_number: u64, block_timestamp: Option<u64>) {
        self.pools.insert(
            pool.address,
            PoolState::new(pool, block_number, block_timestamp),
        );
    }

    pub fn get_pool(&self, address: &Address) -> Option<&PoolState> {
        self.pools.get(address)
    }

    pub fn pool_count(&self) -> usize {
        self.pools.len()
    }

    /// Record the latest observed chain head. Advances `state_version` only
    /// if the block number is strictly newer than what we already have -
    /// this keeps `latest_block` monotonic even if a stale block arrives out
    /// of order.
    pub fn update_latest_block(&mut self, block: BlockState) {
        let is_newer = match self.latest_block {
            Some(current) => block.number > current.number,
            None => true,
        };
        if is_newer {
            self.latest_block = Some(block);
            self.state_version = self.state_version.next();
        }
    }

    /// Apply a normalized market event to state.
    ///
    /// Returns `Ok(true)` if the event was newly processed, `Ok(false)` if it
    /// was a duplicate and safely ignored. Duplicate events never mutate
    /// state and never advance `state_version`.
    pub fn apply_event(&mut self, event: MarketEvent) -> bool {
        if !self.dedup.record(event.id) {
            return false;
        }

        // Advance pool freshness metadata if we track this pool. Day 1 does
        // not recompute reserves/price from swap deltas - that lands with
        // the Day 2 pricing engine - but freshness must still be accurate so
        // later staleness checks are meaningful.
        if let Some(pool_state) = self.pools.get_mut(&event.pool_address) {
            pool_state.freshness = Freshness {
                last_updated_block: event.block_number,
                last_updated_timestamp: event.block_timestamp,
                state_version: self.state_version.next(),
            };
        }

        self.state_version = self.state_version.next();

        if self.recent_events.len() == RECENT_EVENTS_CAPACITY {
            self.recent_events.pop_front();
        }
        self.recent_events.push_back(event);

        true
    }

    pub fn recent_events(&self) -> impl Iterator<Item = &MarketEvent> {
        self.recent_events.iter()
    }

    pub fn event_count(&self) -> usize {
        self.recent_events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::model::{EventKind, SwapEvent};
    use crate::market::models::{DexKind, PoolKind, Token};
    use alloy::primitives::{address, Address, B256, U256};

    fn sample_pool(addr: Address) -> Pool {
        Pool {
            address: addr,
            dex: DexKind::Aerodrome,
            token0: Token {
                address: address!("4200000000000000000000000000000000000006"),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::Aerodrome {
                reserve0: U256::from(1_000u64),
                reserve1: U256::from(2_000u64),
                stable: false,
                fee_bps: Some(U256::from(30u64)),
            },
        }
    }

    fn sample_event(pool_addr: Address, log_index: u64) -> MarketEvent {
        MarketEvent::new(
            8453,
            100,
            Some(1_700_000_000),
            B256::repeat_byte(0x01),
            log_index,
            pool_addr,
            DexKind::Aerodrome,
            EventKind::Swap(SwapEvent {
                pool_address: pool_addr,
                dex: DexKind::Aerodrome,
                amount0: 10,
                amount1: -20,
                sender: None,
                recipient: None,
            }),
            1,
            2,
            3,
        )
    }

    #[test]
    fn state_updates_correctly() {
        let mut state = MarketState::new();
        let pool_addr = Address::from_slice(&[0xA1; 20]);
        state.register_pool(sample_pool(pool_addr), 1, None);
        assert_eq!(state.pool_count(), 1);

        let applied = state.apply_event(sample_event(pool_addr, 0));
        assert!(applied);
        assert_eq!(state.event_count(), 1);

        let pool_state = state.get_pool(&pool_addr).unwrap();
        assert_eq!(pool_state.freshness.last_updated_block, 100);
    }

    #[test]
    fn state_version_increments_on_accepted_event() {
        let mut state = MarketState::new();
        let pool_addr = Address::from_slice(&[0xA2; 20]);
        state.register_pool(sample_pool(pool_addr), 1, None);

        let before = state.state_version;
        state.apply_event(sample_event(pool_addr, 0));
        assert!(state.state_version > before);
    }

    #[test]
    fn duplicate_event_does_not_modify_state_twice() {
        let mut state = MarketState::new();
        let pool_addr = Address::from_slice(&[0xA3; 20]);
        state.register_pool(sample_pool(pool_addr), 1, None);

        let event = sample_event(pool_addr, 0);
        assert!(state.apply_event(event.clone()));
        let version_after_first = state.state_version;
        let count_after_first = state.event_count();

        assert!(!state.apply_event(event), "duplicate must be rejected");
        assert_eq!(state.state_version, version_after_first);
        assert_eq!(state.event_count(), count_after_first);
    }

    #[test]
    fn latest_block_updates_correctly_and_stays_monotonic() {
        let mut state = MarketState::new();
        state.update_latest_block(BlockState {
            number: 10,
            timestamp: Some(1),
            hash: None,
        });
        assert_eq!(state.latest_block.unwrap().number, 10);
        let version_after_first = state.state_version;

        // Stale block must not regress latest_block or bump state_version.
        state.update_latest_block(BlockState {
            number: 5,
            timestamp: Some(1),
            hash: None,
        });
        assert_eq!(state.latest_block.unwrap().number, 10);
        assert_eq!(state.state_version, version_after_first);

        state.update_latest_block(BlockState {
            number: 11,
            timestamp: Some(2),
            hash: None,
        });
        assert_eq!(state.latest_block.unwrap().number, 11);
        assert!(state.state_version > version_after_first);
    }
}
