//! In-memory pool registry.
//!
//! Handles pool discovery/lifecycle/lookup. Does NOT handle market state
//! (that's `market::MarketState`) - keeping these separate means the
//! registry can answer "do we know this pool and do we trust it" without
//! needing to know anything about its latest price/reserves.

use crate::market::models::{DexKind, Pool};
use crate::pools::models::{DiscoverySource, PoolRecord, PoolStatus};
use alloy::primitives::Address;
use std::collections::HashMap;

/// Order-independent token-pair key: token addresses sorted so `(A, B)` and
/// `(B, A)` always map to the same key.
fn pair_key(a: Address, b: Address) -> (Address, Address) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

#[derive(Debug, Default)]
pub struct PoolRegistry {
    pools: HashMap<Address, PoolRecord>,
    /// (token0, token1) [order-independent] -> pool addresses.
    pair_index: HashMap<(Address, Address), Vec<Address>>,
}

impl PoolRegistry {
    pub fn new() -> Self {
        PoolRegistry {
            pools: HashMap::new(),
            pair_index: HashMap::new(),
        }
    }

    /// Insert a newly discovered pool. Returns `false` (and does nothing
    /// else) if this pool address is already registered - discovery events
    /// can be redelivered, and re-discovery must never reset an existing
    /// record's lifecycle/eligibility progress back to `Discovered`.
    pub fn insert_discovered(
        &mut self,
        pool: Pool,
        discovery: DiscoverySource,
        block_number: u64,
    ) -> bool {
        if self.pools.contains_key(&pool.address) {
            return false;
        }

        let key = pair_key(pool.token0.address, pool.token1.address);
        self.pair_index.entry(key).or_default().push(pool.address);

        let address = pool.address;
        let record = PoolRecord::new_discovered(pool, discovery, block_number);
        self.pools.insert(address, record);
        true
    }

    pub fn get(&self, address: &Address) -> Option<&PoolRecord> {
        self.pools.get(address)
    }

    pub fn get_mut(&mut self, address: &Address) -> Option<&mut PoolRecord> {
        self.pools.get_mut(address)
    }

    /// Move a pool to a new lifecycle status. No-op (returns `false`) if the
    /// pool isn't registered. Blacklisting is always allowed from any
    /// state; other transitions are the caller's responsibility to sequence
    /// sensibly (this method doesn't enforce a strict state machine beyond
    /// "the pool must exist").
    pub fn set_status(&mut self, address: &Address, status: PoolStatus) -> bool {
        match self.pools.get_mut(address) {
            Some(record) => {
                record.status = status;
                true
            }
            None => false,
        }
    }

    /// All pools for a token pair, regardless of which order the tokens are
    /// given in.
    pub fn find_by_token_pair(&self, token_a: Address, token_b: Address) -> Vec<&PoolRecord> {
        let key = pair_key(token_a, token_b);
        self.pair_index
            .get(&key)
            .map(|addrs| addrs.iter().filter_map(|a| self.pools.get(a)).collect())
            .unwrap_or_default()
    }

    /// Pools for a token pair on a specific DEX only.
    pub fn find_by_token_pair_and_dex(
        &self,
        token_a: Address,
        token_b: Address,
        dex: DexKind,
    ) -> Vec<&PoolRecord> {
        self.find_by_token_pair(token_a, token_b)
            .into_iter()
            .filter(|r| r.pool.dex == dex)
            .collect()
    }

    pub fn len(&self) -> usize {
        self.pools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&Address, &PoolRecord)> {
        self.pools.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::models::{PoolKind, Token};
    use alloy::primitives::{address, U256};

    fn token(addr: Address, symbol: &str) -> Token {
        Token {
            address: addr,
            symbol: symbol.into(),
            decimals: 18,
        }
    }

    fn sample_pool(pool_addr: Address, token0: Address, token1: Address, dex: DexKind) -> Pool {
        Pool {
            address: pool_addr,
            dex,
            token0: token(token0, "T0"),
            token1: token(token1, "T1"),
            kind: PoolKind::Aerodrome {
                reserve0: U256::from(1u64),
                reserve1: U256::from(1u64),
                stable: false,
                fee_bps: Some(U256::from(30u64)),
            },
        }
    }

    fn factory_source(block: u64) -> DiscoverySource {
        DiscoverySource::FactoryEvent {
            factory_address: Address::from_slice(&[0xFA; 20]),
            block_number: block,
            tx_hash: alloy::primitives::B256::repeat_byte(0x01),
        }
    }

    #[test]
    fn new_pool_is_inserted() {
        let mut registry = PoolRegistry::new();
        let pool_addr = Address::from_slice(&[0x01; 20]);
        let t0 = Address::from_slice(&[0x10; 20]);
        let t1 = Address::from_slice(&[0x11; 20]);

        let inserted = registry.insert_discovered(
            sample_pool(pool_addr, t0, t1, DexKind::Aerodrome),
            factory_source(100),
            100,
        );

        assert!(inserted);
        assert_eq!(registry.len(), 1);
        let record = registry.get(&pool_addr).expect("pool should be present");
        assert_eq!(record.status, PoolStatus::Discovered);
    }

    #[test]
    fn duplicate_pool_is_rejected() {
        let mut registry = PoolRegistry::new();
        let pool_addr = Address::from_slice(&[0x02; 20]);
        let t0 = Address::from_slice(&[0x20; 20]);
        let t1 = Address::from_slice(&[0x21; 20]);

        let first = registry.insert_discovered(
            sample_pool(pool_addr, t0, t1, DexKind::Aerodrome),
            factory_source(100),
            100,
        );
        let second = registry.insert_discovered(
            sample_pool(pool_addr, t0, t1, DexKind::Aerodrome),
            factory_source(200), // different block - should still be rejected
            200,
        );

        assert!(first);
        assert!(!second, "duplicate discovery must not overwrite the record");
        assert_eq!(registry.len(), 1);
        // Original discovery block preserved, not overwritten by the "duplicate".
        assert_eq!(registry.get(&pool_addr).unwrap().discovered_at_block, 100);
    }

    #[test]
    fn status_transitions() {
        let mut registry = PoolRegistry::new();
        let pool_addr = Address::from_slice(&[0x03; 20]);
        let t0 = Address::from_slice(&[0x30; 20]);
        let t1 = Address::from_slice(&[0x31; 20]);
        registry.insert_discovered(
            sample_pool(pool_addr, t0, t1, DexKind::Aerodrome),
            factory_source(1),
            1,
        );

        assert!(registry.set_status(&pool_addr, PoolStatus::Hydrating));
        assert_eq!(registry.get(&pool_addr).unwrap().status, PoolStatus::Hydrating);

        assert!(registry.set_status(&pool_addr, PoolStatus::Active));
        assert_eq!(registry.get(&pool_addr).unwrap().status, PoolStatus::Active);

        // Blacklisting is always allowed, from any state.
        assert!(registry.set_status(&pool_addr, PoolStatus::Blacklisted));
        assert_eq!(
            registry.get(&pool_addr).unwrap().status,
            PoolStatus::Blacklisted
        );

        // Unknown address: no-op, returns false.
        let unknown = Address::from_slice(&[0xFF; 20]);
        assert!(!registry.set_status(&unknown, PoolStatus::Active));
    }

    #[test]
    fn lookup_by_address() {
        let mut registry = PoolRegistry::new();
        let pool_addr = Address::from_slice(&[0x04; 20]);
        let t0 = Address::from_slice(&[0x40; 20]);
        let t1 = Address::from_slice(&[0x41; 20]);
        registry.insert_discovered(
            sample_pool(pool_addr, t0, t1, DexKind::UniswapV3),
            factory_source(1),
            1,
        );

        assert!(registry.get(&pool_addr).is_some());
        assert!(registry.get(&Address::from_slice(&[0xAB; 20])).is_none());
    }

    #[test]
    fn lookup_by_token_pair_ignores_input_order() {
        let mut registry = PoolRegistry::new();
        let pool_addr = Address::from_slice(&[0x05; 20]);
        let usdc = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let weth = address!("4200000000000000000000000000000000000006");
        registry.insert_discovered(
            sample_pool(pool_addr, weth, usdc, DexKind::Aerodrome),
            factory_source(1),
            1,
        );

        let via_weth_usdc = registry.find_by_token_pair(weth, usdc);
        let via_usdc_weth = registry.find_by_token_pair(usdc, weth);

        assert_eq!(via_weth_usdc.len(), 1);
        assert_eq!(via_usdc_weth.len(), 1);
        assert_eq!(via_weth_usdc[0].pool.address, pool_addr);
        assert_eq!(via_usdc_weth[0].pool.address, pool_addr);
    }

    #[test]
    fn lookup_by_token_pair_and_dex_filters_correctly() {
        let mut registry = PoolRegistry::new();
        let t0 = Address::from_slice(&[0x60; 20]);
        let t1 = Address::from_slice(&[0x61; 20]);
        let aero_pool = Address::from_slice(&[0x06; 20]);
        let uni_pool = Address::from_slice(&[0x07; 20]);

        registry.insert_discovered(
            sample_pool(aero_pool, t0, t1, DexKind::Aerodrome),
            factory_source(1),
            1,
        );
        registry.insert_discovered(
            sample_pool(uni_pool, t0, t1, DexKind::UniswapV3),
            factory_source(1),
            1,
        );

        let aero_only = registry.find_by_token_pair_and_dex(t0, t1, DexKind::Aerodrome);
        assert_eq!(aero_only.len(), 1);
        assert_eq!(aero_only[0].pool.address, aero_pool);

        let all = registry.find_by_token_pair(t0, t1);
        assert_eq!(all.len(), 2);
    }
}
