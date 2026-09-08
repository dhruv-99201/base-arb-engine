# base-arb-engine Day 2 apply script (v5 - fixes malformed test address literal)
# Run from the repository root: C:\Users\Dell\Downloads\base-arb-engine
#   powershell -ExecutionPolicy Bypass -File apply_day2.ps1
Write-Host 'Applying Day 2 changes...'

# Ensure all target directories exist first.
New-Item -ItemType Directory -Force -Path 'src' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\chain' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\dex' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\dex\discovery' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\events' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\market' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\pools' | Out-Null

# ---- src/pools/mod.rs ----
$content = @'
pub mod models;
pub mod registry;
pub mod token_cache;

pub use models::{
    DiscoverySource, EligibilityStatus, PoolEligibility, PoolRecord, PoolStatus,
};
pub use registry::PoolRegistry;
pub use token_cache::TokenMetadataCache;

'@
Set-Content -Path 'src\pools\mod.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pools/mod.rs'

# ---- src/pools/models.rs ----
$content = @'
//! Pool registry data models: lifecycle status, discovery provenance, and
//! eligibility. Separate from `market::MarketState` by design - the
//! registry answers "what pools do we know about and can we trust them",
//! `MarketState` answers "what is the latest observed state of a pool".

use crate::market::models::Pool;
use alloy::primitives::{Address, B256};
use serde::{Deserialize, Serialize};

/// Lifecycle of a discovered pool. Pools only ever move forward through
/// this state machine (never silently reset), except into `Blacklisted`,
/// which can be reached from any state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PoolStatus {
    /// Observed via a factory `PoolCreated` event (or explicit verified
    /// config), but on-chain state has not been fetched yet.
    Discovered,
    /// Hydration (immutable/current state + token metadata) in progress.
    Hydrating,
    /// Hydrated successfully; state can be trusted as of its freshness
    /// metadata.
    Active,
    /// Was `Active` at some point but hydration/updates are currently
    /// failing (e.g. RPC errors) - not necessarily a bad pool, may recover.
    Inactive,
    /// Explicitly excluded - malformed data, unsupported mechanics, or
    /// operator decision. Never reconsidered automatically.
    Blacklisted,
}

/// Provenance: how/where a pool entered the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscoverySource {
    /// A specific factory `PoolCreated`-style event.
    FactoryEvent {
        factory_address: Address,
        block_number: u64,
        tx_hash: B256,
    },
    /// Explicit operator configuration (verified address supplied directly,
    /// bypassing factory-event discovery). Mirrors Day 1's
    /// `AERODROME_POOL_ADDRESS` / `UNISWAP_V3_POOL_ADDRESS` pattern.
    ExplicitConfig,
}

/// Coarse-grained eligibility signal for whether a pool is safe to hand to
/// future strategy/scanning layers. Day 2 only *computes* this; nothing
/// downstream consumes it as a hard gate yet - that's the opportunity
/// engine's job (later days), which will impose stronger criteria on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EligibilityStatus {
    Eligible,
    Ineligible,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolEligibility {
    pub verified_protocol: bool,
    pub token_metadata_available: bool,
    pub pool_type_supported: bool,
    pub liquidity_available: bool,
    pub state_readable: bool,
}

impl PoolEligibility {
    pub fn unknown() -> Self {
        PoolEligibility {
            verified_protocol: false,
            token_metadata_available: false,
            pool_type_supported: false,
            liquidity_available: false,
            state_readable: false,
        }
    }

    /// Overall status derived from the individual signals. All five must be
    /// true for `Eligible`; if none have been evaluated yet (all false, the
    /// `unknown()` default), the status is `Unknown` rather than a
    /// misleadingly confident `Ineligible`.
    pub fn status(&self) -> EligibilityStatus {
        let all_true = self.verified_protocol
            && self.token_metadata_available
            && self.pool_type_supported
            && self.liquidity_available
            && self.state_readable;
        let all_false = !self.verified_protocol
            && !self.token_metadata_available
            && !self.pool_type_supported
            && !self.liquidity_available
            && !self.state_readable;

        if all_true {
            EligibilityStatus::Eligible
        } else if all_false {
            EligibilityStatus::Unknown
        } else {
            EligibilityStatus::Ineligible
        }
    }
}

/// A pool as tracked by the registry: identity/state (`Pool`, reused from
/// `market::models`), lifecycle, provenance, and eligibility. Freshness of
/// the *market data* itself lives on `MarketState`'s `PoolState`, not here -
/// the registry's `last_updated_block` tracks registry-level bookkeeping
/// (when this record was last touched), which is a different concern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRecord {
    pub pool: Pool,
    pub status: PoolStatus,
    pub discovery: DiscoverySource,
    pub eligibility: PoolEligibility,
    pub discovered_at_block: u64,
    pub last_updated_block: u64,
    pub last_updated_timestamp: Option<u64>,
}

impl PoolRecord {
    pub fn new_discovered(pool: Pool, discovery: DiscoverySource, block_number: u64) -> Self {
        PoolRecord {
            pool,
            status: PoolStatus::Discovered,
            discovery,
            eligibility: PoolEligibility::unknown(),
            discovered_at_block: block_number,
            last_updated_block: block_number,
            last_updated_timestamp: None,
        }
    }
}

'@
Set-Content -Path 'src\pools\models.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pools/models.rs'

# ---- src/pools/registry.rs ----
$content = @'
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

'@
Set-Content -Path 'src\pools\registry.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pools/registry.rs'

# ---- src/pools/token_cache.rs ----
$content = @'
//! ERC-20 token metadata hydration, with caching so the same token contract
//! is never queried more than once per process lifetime.
//!
//! Decimals are treated as required (a pool can't be safely reasoned about
//! without them - see Day 1's financial-code rules). `symbol`/`name` are
//! best-effort: a revert or non-standard implementation just leaves them
//! empty rather than failing the whole hydration.

use crate::error::{EngineError, EngineResult};
use crate::market::models::Token;
use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use alloy::sol;
use std::collections::HashMap;

sol! {
    #[sol(rpc)]
    interface IERC20Metadata {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string memory);
        function name() external view returns (string memory);
    }
}

#[derive(Debug, Default)]
pub struct TokenMetadataCache {
    cache: HashMap<Address, Token>,
}

impl TokenMetadataCache {
    pub fn new() -> Self {
        TokenMetadataCache {
            cache: HashMap::new(),
        }
    }

    pub fn get_cached(&self, address: &Address) -> Option<&Token> {
        self.cache.get(address)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Fetch (or return cached) metadata for `address`. `decimals()` must
    /// succeed - everything else is best-effort.
    pub async fn get_or_fetch(&mut self, rpc_url: &str, address: Address) -> EngineResult<Token> {
        if let Some(token) = self.cache.get(&address) {
            return Ok(token.clone());
        }

        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let contract = IERC20Metadata::new(address, provider);

        let decimals = contract.decimals().call().await.map_err(|e| EngineError::Dex {
            dex: "erc20".into(),
            reason: format!("decimals() failed for {address}: {e}"),
        })?;

        // Best-effort: a nonstandard/missing symbol or name must never fail
        // hydration - decimals is the only value later pricing math depends
        // on.
        let symbol = contract
            .symbol()
            .call()
            .await
            .unwrap_or_default();
        let _name = contract.name().call().await.unwrap_or_default();

        let token = Token {
            address,
            symbol,
            decimals,
        };
        self.cache.insert(address, token.clone());
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_starts_empty() {
        let cache = TokenMetadataCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_hit_avoids_refetch() {
        let mut cache = TokenMetadataCache::new();
        let addr = Address::from_slice(&[0x42; 20]);
        let token = Token {
            address: addr,
            symbol: "TEST".into(),
            decimals: 18,
        };
        cache.cache.insert(addr, token.clone());

        assert_eq!(cache.get_cached(&addr), Some(&token));
        assert_eq!(cache.len(), 1);
    }
}

'@
Set-Content -Path 'src\pools\token_cache.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pools/token_cache.rs'

# ---- src/dex/discovery/mod.rs ----
$content = @'
//! Pool discovery: turning factory `PoolCreated`-style events into pools
//! the registry can hydrate. Deliberately separate from `dex::traits::DexAdapter`
//! (which handles state/quoting for pools we already know about) - discovery
//! is a distinct concern with its own event shapes per factory.

pub mod aerodrome_classic;
pub mod aerodrome_slipstream;
pub mod uniswap_v3;

use crate::error::EngineResult;
use crate::market::models::DexKind;
use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log as RpcLog;

pub use aerodrome_classic::AerodromeClassicDiscovery;
pub use aerodrome_slipstream::AerodromeSlipstreamDiscovery;
pub use uniswap_v3::UniswapV3Discovery;

/// Protocol-specific parameters captured at pool-creation time, before any
/// on-chain hydration. Kept separate from `market::models::PoolKind` since
/// that type represents *current* state (reserves/sqrtPriceX96/etc), not
/// creation-time parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryParams {
    AerodromeClassic { stable: bool },
    ConcentratedLiquidity { tick_spacing: i32 },
}

/// A pool observed via a factory event, not yet hydrated.
#[derive(Debug, Clone)]
pub struct DiscoveredPool {
    pub pool_address: Address,
    pub token0_address: Address,
    pub token1_address: Address,
    pub dex: DexKind,
    pub params: DiscoveryParams,
    pub block_number: u64,
    pub tx_hash: B256,
    pub factory_address: Address,
}

/// Implemented once per factory/event-shape. Adapters only decode - they
/// never fetch state (that stays in `dex::traits::DexAdapter::get_pool_state`,
/// called afterward during hydration).
pub trait PoolDiscoveryAdapter: Send + Sync {
    fn dex(&self) -> DexKind;
    fn factory_address(&self) -> Address;
    /// keccak256 topic0 of this factory's pool-creation event.
    fn event_topic0(&self) -> B256;
    fn decode_pool_created(&self, log: &RpcLog) -> EngineResult<DiscoveredPool>;
}

'@
Set-Content -Path 'src\dex\discovery\mod.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/mod.rs'

# ---- src/dex/discovery/uniswap_v3.rs ----
$content = @'
//! Uniswap V3 factory discovery.
//!
//! Event signature is Uniswap's well-known, extensively documented
//! `UniswapV3Factory.PoolCreated` - the same shape across every chain
//! Uniswap V3 is deployed on. Factory address for Base is verified against
//! Uniswap's official deployments page (see `config.rs`).

use crate::dex::discovery::{DiscoveredPool, DiscoveryParams, PoolDiscoveryAdapter};
use crate::error::{EngineError, EngineResult};
use crate::market::models::DexKind;
use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use alloy::sol_types::SolEvent;

sol! {
    event UniswapV3PoolCreated(
        address indexed token0,
        address indexed token1,
        uint24 indexed fee,
        int24 tickSpacing,
        address pool
    );
}

pub struct UniswapV3Discovery {
    factory_address: Address,
}

impl UniswapV3Discovery {
    pub fn new(factory_address: Address) -> Self {
        UniswapV3Discovery { factory_address }
    }
}

impl PoolDiscoveryAdapter for UniswapV3Discovery {
    fn dex(&self) -> DexKind {
        DexKind::UniswapV3
    }

    fn factory_address(&self) -> Address {
        self.factory_address
    }

    fn event_topic0(&self) -> B256 {
        UniswapV3PoolCreated::SIGNATURE_HASH
    }

    fn decode_pool_created(&self, log: &RpcLog) -> EngineResult<DiscoveredPool> {
        let block_number = log
            .block_number
            .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
        let tx_hash = log
            .transaction_hash
            .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;

        let decoded_log = UniswapV3PoolCreated::decode_log(&log.inner).map_err(|e| {
            EngineError::Decode(format!("failed to decode UniswapV3PoolCreated log: {e}"))
        })?;
        let decoded = decoded_log.data;

        Ok(DiscoveredPool {
            pool_address: decoded.pool,
            token0_address: decoded.token0,
            token1_address: decoded.token1,
            dex: DexKind::UniswapV3,
            params: DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: decoded.tickSpacing.as_i32(),
            },
            block_number,
            tx_hash,
            factory_address: self.factory_address,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, Bytes, Log as PrimitiveLog};

    fn build_log(topics: Vec<B256>, data: Bytes, addr: Address) -> RpcLog {
        let inner = PrimitiveLog::new_unchecked(addr, topics, data);
        RpcLog {
            inner,
            block_hash: None,
            block_number: Some(999_000),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(0xCD)),
            transaction_index: Some(0),
            log_index: Some(1),
            removed: false,
        }
    }

    #[test]
    fn valid_pool_created_decodes() {
        let factory = address!("33128a8fC17869897dcE68Ed026d694621f6FDfD");
        let token0 = address!("4200000000000000000000000000000000000006");
        let token1 = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let pool = address!("d0b53D9277642d899DF5C87A3966A349A798F224");

        let event = UniswapV3PoolCreated {
            token0,
            token1,
            fee: alloy::primitives::aliases::U24::try_from(500u32).unwrap(),
            tickSpacing: alloy::primitives::aliases::I24::try_from(10i32).unwrap(),
            pool,
        };
        let encoded = event.encode_log_data();
        let log = build_log(encoded.topics().to_vec(), encoded.data.clone(), factory);

        let adapter = UniswapV3Discovery::new(factory);
        let discovered = adapter.decode_pool_created(&log).expect("should decode");

        assert_eq!(discovered.pool_address, pool);
        assert_eq!(discovered.token0_address, token0);
        assert_eq!(discovered.token1_address, token1);
        assert_eq!(discovered.dex, DexKind::UniswapV3);
        match discovered.params {
            DiscoveryParams::ConcentratedLiquidity { tick_spacing } => {
                assert_eq!(tick_spacing, 10)
            }
            other => panic!("expected ConcentratedLiquidity params, got {other:?}"),
        }
    }

    #[test]
    fn malformed_pool_created_is_rejected() {
        let factory = address!("33128a8fC17869897dcE68Ed026d694621f6FDfD");
        let bogus_topic = B256::repeat_byte(0x11);
        let log = build_log(vec![bogus_topic], Bytes::new(), factory);

        let adapter = UniswapV3Discovery::new(factory);
        assert!(adapter.decode_pool_created(&log).is_err());
    }
}

'@
Set-Content -Path 'src\dex\discovery\uniswap_v3.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/uniswap_v3.rs'

# ---- src/dex/discovery/aerodrome_classic.rs ----
$content = @'
//! Aerodrome classic (Solidly-style) factory discovery.
//!
//! Event signature confirmed directly against the verified `PoolFactory`
//! source on BaseScan (address 0x420DD381b31aEf6683db6B902084cB0FFECe40Da,
//! labeled "Aerodrome: Pool Factory"):
//! `event PoolCreated(address indexed token0, address indexed token1, bool
//! indexed stable, address pool, uint256);` - the trailing `uint256` is
//! unnamed in the source (it's `allPools.length - 1`, the pool's index) and
//! is not needed here, so it's decoded but discarded.

use crate::dex::discovery::{DiscoveredPool, DiscoveryParams, PoolDiscoveryAdapter};
use crate::error::{EngineError, EngineResult};
use crate::market::models::DexKind;
use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use alloy::sol_types::SolEvent;

sol! {
    event AerodromeClassicPoolCreated(
        address indexed token0,
        address indexed token1,
        bool indexed stable,
        address pool,
        uint256 poolIndex
    );
}

pub struct AerodromeClassicDiscovery {
    factory_address: Address,
}

impl AerodromeClassicDiscovery {
    pub fn new(factory_address: Address) -> Self {
        AerodromeClassicDiscovery { factory_address }
    }
}

impl PoolDiscoveryAdapter for AerodromeClassicDiscovery {
    fn dex(&self) -> DexKind {
        DexKind::Aerodrome
    }

    fn factory_address(&self) -> Address {
        self.factory_address
    }

    fn event_topic0(&self) -> B256 {
        AerodromeClassicPoolCreated::SIGNATURE_HASH
    }

    fn decode_pool_created(&self, log: &RpcLog) -> EngineResult<DiscoveredPool> {
        let block_number = log
            .block_number
            .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
        let tx_hash = log
            .transaction_hash
            .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;

        let decoded_log = AerodromeClassicPoolCreated::decode_log(&log.inner).map_err(|e| {
            EngineError::Decode(format!(
                "failed to decode AerodromeClassicPoolCreated log: {e}"
            ))
        })?;
        let decoded = decoded_log.data;

        Ok(DiscoveredPool {
            pool_address: decoded.pool,
            token0_address: decoded.token0,
            token1_address: decoded.token1,
            dex: DexKind::Aerodrome,
            params: DiscoveryParams::AerodromeClassic {
                stable: decoded.stable,
            },
            block_number,
            tx_hash,
            factory_address: self.factory_address,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, Bytes, Log as PrimitiveLog, U256};

    fn build_log(topics: Vec<B256>, data: Bytes, addr: Address) -> RpcLog {
        let inner = PrimitiveLog::new_unchecked(addr, topics, data);
        RpcLog {
            inner,
            block_hash: None,
            block_number: Some(500_000),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(0xEF)),
            transaction_index: Some(0),
            log_index: Some(2),
            removed: false,
        }
    }

    #[test]
    fn valid_classic_pool_created_decodes() {
        let factory = address!("420DD381b31aEf6683db6B902084cB0FFECe40Da");
        let token0 = address!("4200000000000000000000000000000000000006");
        let token1 = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let pool = address!("1111111111111111111111111111111111111111");

        let event = AerodromeClassicPoolCreated {
            token0,
            token1,
            stable: false,
            pool,
            poolIndex: U256::from(42u64),
        };
        let encoded = event.encode_log_data();
        let log = build_log(encoded.topics().to_vec(), encoded.data.clone(), factory);

        let adapter = AerodromeClassicDiscovery::new(factory);
        let discovered = adapter.decode_pool_created(&log).expect("should decode");

        assert_eq!(discovered.pool_address, pool);
        assert_eq!(discovered.dex, DexKind::Aerodrome);
        match discovered.params {
            DiscoveryParams::AerodromeClassic { stable } => assert!(!stable),
            other => panic!("expected AerodromeClassic params, got {other:?}"),
        }
    }

    #[test]
    fn malformed_classic_pool_created_is_rejected() {
        let factory = address!("420DD381b31aEf6683db6B902084cB0FFECe40Da");
        let bogus_topic = B256::repeat_byte(0x22);
        let log = build_log(vec![bogus_topic], Bytes::new(), factory);

        let adapter = AerodromeClassicDiscovery::new(factory);
        assert!(adapter.decode_pool_created(&log).is_err());
    }
}

'@
Set-Content -Path 'src\dex\discovery\aerodrome_classic.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/aerodrome_classic.rs'

# ---- src/dex/discovery/aerodrome_slipstream.rs ----
$content = @'
//! Aerodrome Slipstream (concentrated-liquidity) `CLFactory` discovery.
//!
//! IMPORTANT - PARTIALLY VERIFIED:
//! The event *emission* is confirmed directly from `CLFactory.sol`'s real
//! source (github.com/aerodrome-finance/slipstream, `createPool`):
//! `emit PoolCreated(token0, token1, tickSpacing, pool);`
//!
//! However, the exact *indexed* flags for each parameter could not be
//! independently confirmed from `ICLFactory.sol`'s interface declaration
//! (not fetched). This adapter assumes `token0`, `token1`, and
//! `tickSpacing` are indexed and `pool` is not - matching both Uniswap V3's
//! analogous `PoolCreated` event and Aerodrome's own classic
//! `PoolFactory.PoolCreated` (both of which put exactly the first three
//! logical fields in topics). This is a well-justified inference, not a
//! confirmed fact - if `decode_pool_created` starts failing against real
//! Slipstream pool-creation logs, this is the first place to check (topic0
//! itself is unaffected by this uncertainty since the field order/types are
//! confirmed - only the topics/data split could be wrong).
//!
//! The factory address is NOT defaulted anywhere in this codebase (see
//! `config.rs`) for the same reason - use only after verifying it yourself.

use crate::dex::discovery::{DiscoveredPool, DiscoveryParams, PoolDiscoveryAdapter};
use crate::error::{EngineError, EngineResult};
use crate::market::models::DexKind;
use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use alloy::sol_types::SolEvent;

sol! {
    event AerodromeSlipstreamPoolCreated(
        address indexed token0,
        address indexed token1,
        int24 indexed tickSpacing,
        address pool
    );
}

pub struct AerodromeSlipstreamDiscovery {
    factory_address: Address,
}

impl AerodromeSlipstreamDiscovery {
    pub fn new(factory_address: Address) -> Self {
        AerodromeSlipstreamDiscovery { factory_address }
    }
}

impl PoolDiscoveryAdapter for AerodromeSlipstreamDiscovery {
    fn dex(&self) -> DexKind {
        DexKind::AerodromeSlipstream
    }

    fn factory_address(&self) -> Address {
        self.factory_address
    }

    fn event_topic0(&self) -> B256 {
        AerodromeSlipstreamPoolCreated::SIGNATURE_HASH
    }

    fn decode_pool_created(&self, log: &RpcLog) -> EngineResult<DiscoveredPool> {
        let block_number = log
            .block_number
            .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
        let tx_hash = log
            .transaction_hash
            .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;

        let decoded_log = AerodromeSlipstreamPoolCreated::decode_log(&log.inner).map_err(|e| {
            EngineError::Decode(format!(
                "failed to decode AerodromeSlipstreamPoolCreated log: {e}"
            ))
        })?;
        let decoded = decoded_log.data;

        Ok(DiscoveredPool {
            pool_address: decoded.pool,
            token0_address: decoded.token0,
            token1_address: decoded.token1,
            dex: DexKind::AerodromeSlipstream,
            params: DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: decoded.tickSpacing.as_i32(),
            },
            block_number,
            tx_hash,
            factory_address: self.factory_address,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, Bytes, Log as PrimitiveLog};

    fn build_log(topics: Vec<B256>, data: Bytes, addr: Address) -> RpcLog {
        let inner = PrimitiveLog::new_unchecked(addr, topics, data);
        RpcLog {
            inner,
            block_hash: None,
            block_number: Some(600_000),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(0x9A)),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        }
    }

    #[test]
    fn valid_slipstream_pool_created_decodes() {
        let factory = Address::from_slice(&[0xC1; 20]);
        let token0 = address!("4200000000000000000000000000000000000006");
        let token1 = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let pool = address!("2222222222222222222222222222222222222222");

        let event = AerodromeSlipstreamPoolCreated {
            token0,
            token1,
            tickSpacing: alloy::primitives::aliases::I24::try_from(100i32).unwrap(),
            pool,
        };
        let encoded = event.encode_log_data();
        let log = build_log(encoded.topics().to_vec(), encoded.data.clone(), factory);

        let adapter = AerodromeSlipstreamDiscovery::new(factory);
        let discovered = adapter.decode_pool_created(&log).expect("should decode");

        assert_eq!(discovered.pool_address, pool);
        assert_eq!(discovered.dex, DexKind::AerodromeSlipstream);
        match discovered.params {
            DiscoveryParams::ConcentratedLiquidity { tick_spacing } => {
                assert_eq!(tick_spacing, 100)
            }
            other => panic!("expected ConcentratedLiquidity params, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_or_malformed_pool_created_is_handled_safely() {
        let factory = Address::from_slice(&[0xC1; 20]);
        let bogus_topic = B256::repeat_byte(0x33);
        let log = build_log(vec![bogus_topic], Bytes::new(), factory);

        let adapter = AerodromeSlipstreamDiscovery::new(factory);
        // Must return a clean Err, never panic and never fabricate a pool.
        assert!(adapter.decode_pool_created(&log).is_err());
    }
}

'@
Set-Content -Path 'src\dex\discovery\aerodrome_slipstream.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/aerodrome_slipstream.rs'

# ---- src/dex/aerodrome_slipstream.rs ----
$content = @'
//! Aerodrome Slipstream (`CLPool`) adapter.
//!
//! Concentrated liquidity, structurally close to Uniswap V3 but NOT
//! ABI-identical - confirmed by reading the real `CLPool` source (verified
//! contract on BaseScan). Notably, `CLPool.slot0()` returns a 6-field tuple
//! (no `feeProtocol`, unlike `IUniswapV3Pool.slot0()`'s 7 fields), so Day
//! 1's `UniswapV3Adapter` cannot be reused as-is for Slipstream pools - the
//! ABI mismatch would silently misdecode the return data.
//!
//! Fee is intentionally NOT hydrated here: Slipstream fees are not a simple
//! per-pool constant (they route through `CLFactory.getSwapFee(pool)`,
//! which additionally depends on gauge/fee-module state) - out of scope for
//! Day 2 state hydration. `fee_tier` is stored as `0` as an explicit
//! placeholder, not a real value; the future pricing layer must resolve fee
//! separately before using this for quote math.

use crate::dex::traits::DexAdapter;
use crate::error::{EngineError, EngineResult};
use crate::events::decoder;
use crate::events::model::MarketEvent;
use crate::market::models::{Pool, PoolKind, PoolState};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use async_trait::async_trait;

sol! {
    #[sol(rpc)]
    interface ICLPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function tickSpacing() external view returns (int24);
    }
}

pub struct AerodromeSlipstreamAdapter;

impl AerodromeSlipstreamAdapter {
    pub fn new() -> Self {
        AerodromeSlipstreamAdapter
    }
}

impl Default for AerodromeSlipstreamAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DexAdapter for AerodromeSlipstreamAdapter {
    fn name(&self) -> &'static str {
        "aerodrome_slipstream"
    }

    async fn get_pool_state(&self, rpc_url: &str, pool: &Pool) -> EngineResult<PoolState> {
        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let contract = ICLPool::new(pool.address, provider.clone());

        let slot0 = contract.slot0().call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("slot0() failed: {e}"),
        })?;
        let liquidity = contract
            .liquidity()
            .call()
            .await
            .map_err(|e| EngineError::Dex {
                dex: self.name().into(),
                reason: format!("liquidity() failed: {e}"),
            })?;
        let tick_spacing = contract
            .tickSpacing()
            .call()
            .await
            .map_err(|e| EngineError::Dex {
                dex: self.name().into(),
                reason: format!("tickSpacing() failed: {e}"),
            })?;

        let block_number = provider
            .get_block_number()
            .await
            .map_err(|e| EngineError::Chain(format!("get_block_number failed: {e}")))?;

        let mut updated_pool = pool.clone();
        updated_pool.kind = PoolKind::ConcentratedLiquidity {
            // Placeholder - see module docs. Never treat as a real fee.
            fee_tier: 0,
            tick_spacing: tick_spacing.as_i32(),
            sqrt_price_x96: alloy::primitives::U256::from(slot0.sqrtPriceX96),
            current_tick: slot0.tick.as_i32(),
            liquidity,
            initialized_ticks: Default::default(),
        };

        Ok(PoolState::new(updated_pool, block_number, None))
    }

    fn decode_event(
        &self,
        log: &RpcLog,
        chain_id: u64,
        received_at_us: u64,
    ) -> EngineResult<MarketEvent> {
        decoder::decode_aerodrome_slipstream_log(log, chain_id, received_at_us)
    }
}

'@
Set-Content -Path 'src\dex\aerodrome_slipstream.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/aerodrome_slipstream.rs'

# ---- src/chain/log_poller.rs ----
$content = @'
//! HTTP `eth_getLogs` polling.
//!
//! Reusable by both pool-discovery scanning (factory addresses + PoolCreated
//! topics) and known-pool swap scanning (pool addresses + Swap topics) - see
//! `main.rs`. Transport-independent from the strategy/state layer's point of
//! view: this produces raw `RpcLog`s, the same shape the WebSocket path
//! produces, so decoding/dedup/state application code doesn't know or care
//! which transport a log came from.

use crate::config::LogStartBlock;
use crate::error::{EngineError, EngineResult};
use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, Log as RpcLog};

/// How many times a failing range is allowed to be halved before giving up
/// on that sub-range entirely (logged, not fatal - see `fetch_range`).
const MAX_RANGE_REDUCTIONS: u32 = 5;

/// Tracks how far a given scan (discovery or swaps) has progressed, so
/// polling cycles never redundantly rescan already-processed blocks and
/// never silently skip a gap.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogPollCheckpoint {
    last_scanned_block: Option<u64>,
}

impl LogPollCheckpoint {
    pub fn new() -> Self {
        LogPollCheckpoint {
            last_scanned_block: None,
        }
    }

    pub fn last_scanned_block(&self) -> Option<u64> {
        self.last_scanned_block
    }

    /// Compute the next `(from, to)` range to scan, given the current chain
    /// head. Returns `None` if there is nothing new to scan (already caught
    /// up). On first call (no checkpoint yet), `start` decides where
    /// scanning begins - `Latest` means "start from the current head, no
    /// history", matching the Day 2 safety default.
    pub fn next_range(&self, latest_block: u64, start: LogStartBlock) -> Option<(u64, u64)> {
        let from = match self.last_scanned_block {
            Some(last) => last.saturating_add(1),
            None => match start {
                LogStartBlock::Latest => latest_block,
                LogStartBlock::Block(b) => b,
            },
        };
        if from > latest_block {
            None
        } else {
            Some((from, latest_block))
        }
    }

    /// Record that blocks up to and including `to_block` have been scanned.
    pub fn advance(&mut self, to_block: u64) {
        self.last_scanned_block = Some(to_block);
    }
}

/// Split `[from, to]` (inclusive) into chunks of at most `max_range` blocks
/// each. Empty (`from > to`) ranges produce no chunks.
pub fn compute_chunks(from: u64, to: u64, max_range: u64) -> Vec<(u64, u64)> {
    if from > to || max_range == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = from;
    loop {
        let end = start.saturating_add(max_range - 1).min(to);
        chunks.push((start, end));
        if end >= to {
            break;
        }
        start = end + 1;
    }
    chunks
}

/// Given a range that just failed (e.g. the RPC provider rejected it as too
/// large), return a smaller range to retry: the first half of the original.
/// Returns `None` once the range can't be reduced any further (a single
/// block that still fails - nothing left to do but skip it and log).
pub fn reduce_range_on_failure(from: u64, to: u64) -> Option<(u64, u64)> {
    if from >= to {
        return None;
    }
    let mid = from + (to - from) / 2;
    Some((from, mid))
}

pub struct HttpLogPoller {
    rpc_url: String,
    max_block_range: u64,
}

impl HttpLogPoller {
    pub fn new(rpc_url: String, max_block_range: u64) -> Self {
        HttpLogPoller {
            rpc_url,
            max_block_range,
        }
    }

    /// Fetch all logs in `[from, to]` matching `addresses`/`topic0`,
    /// chunking the range and transparently shrinking any chunk that the
    /// provider rejects (e.g. "range too large") until it succeeds or can't
    /// be shrunk further. Never panics or aborts the whole scan because one
    /// sub-range is troublesome - a failed leaf range is logged and
    /// skipped, not silently dropped without a trace.
    pub async fn fetch_logs(
        &self,
        from: u64,
        to: u64,
        addresses: Vec<Address>,
        topic0: B256,
    ) -> EngineResult<Vec<RpcLog>> {
        let mut all_logs = Vec::new();
        for (chunk_from, chunk_to) in compute_chunks(from, to, self.max_block_range) {
            let mut logs = self
                .fetch_range_with_retry(chunk_from, chunk_to, &addresses, topic0, 0)
                .await;
            all_logs.append(&mut logs);
        }
        Ok(all_logs)
    }

    /// Recursive helper: try `[from, to]`; on failure, halve and retry each
    /// half, up to `MAX_RANGE_REDUCTIONS` deep. Returns whatever logs were
    /// successfully collected - partial results on partial failure, not an
    /// all-or-nothing error, since one bad sub-range shouldn't discard logs
    /// we already successfully fetched from the rest of the range.
    fn fetch_range_with_retry<'a>(
        &'a self,
        from: u64,
        to: u64,
        addresses: &'a [Address],
        topic0: B256,
        depth: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<RpcLog>> + Send + 'a>> {
        Box::pin(async move {
            match self.fetch_range(from, to, addresses, topic0).await {
                Ok(logs) => logs,
                Err(err) => {
                    tracing::warn!(
                        source = "http_poll",
                        from_block = from,
                        to_block = to,
                        error = %err,
                        "eth_getLogs failed for range"
                    );

                    if depth >= MAX_RANGE_REDUCTIONS {
                        tracing::error!(
                            source = "http_poll",
                            from_block = from,
                            to_block = to,
                            "giving up on range after max reductions - these blocks will be skipped"
                        );
                        return Vec::new();
                    }

                    match reduce_range_on_failure(from, to) {
                        Some((reduced_from, reduced_to)) => {
                            let mut logs = self
                                .fetch_range_with_retry(
                                    reduced_from,
                                    reduced_to,
                                    addresses,
                                    topic0,
                                    depth + 1,
                                )
                                .await;
                            let mut rest = self
                                .fetch_range_with_retry(
                                    reduced_to + 1,
                                    to,
                                    addresses,
                                    topic0,
                                    depth + 1,
                                )
                                .await;
                            logs.append(&mut rest);
                            logs
                        }
                        None => {
                            tracing::error!(
                                source = "http_poll",
                                block = from,
                                "single block still fails eth_getLogs - skipping"
                            );
                            Vec::new()
                        }
                    }
                }
            }
        })
    }

    async fn fetch_range(
        &self,
        from: u64,
        to: u64,
        addresses: &[Address],
        topic0: B256,
    ) -> EngineResult<Vec<RpcLog>> {
        let url = self
            .rpc_url
            .parse()
            .map_err(|e| EngineError::Chain(format!("invalid BASE_RPC_URL: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);

        let mut filter = Filter::new().from_block(from).to_block(to);
        if !addresses.is_empty() {
            filter = filter.address(addresses.to_vec());
        }
        filter = filter.event_signature(topic0);

        provider
            .get_logs(&filter)
            .await
            .map_err(|e| EngineError::Chain(format!("get_logs failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_range_produces_no_chunks() {
        assert_eq!(compute_chunks(100, 50, 1000), Vec::new());
        assert_eq!(compute_chunks(100, 100, 0), Vec::new());
    }

    #[test]
    fn range_within_max_produces_one_chunk() {
        assert_eq!(compute_chunks(100, 200, 1000), vec![(100, 200)]);
    }

    #[test]
    fn range_larger_than_max_is_chunked() {
        let chunks = compute_chunks(0, 2499, 1000);
        assert_eq!(chunks, vec![(0, 999), (1000, 1999), (2000, 2499)]);
    }

    #[test]
    fn exact_multiple_range_chunks_cleanly() {
        let chunks = compute_chunks(0, 1999, 1000);
        assert_eq!(chunks, vec![(0, 999), (1000, 1999)]);
    }

    #[test]
    fn checkpoint_advances_and_computes_next_range() {
        let mut checkpoint = LogPollCheckpoint::new();

        // First call, LogStartBlock::Latest: start exactly at the head, no backfill.
        let range = checkpoint.next_range(1000, LogStartBlock::Latest);
        assert_eq!(range, Some((1000, 1000)));

        checkpoint.advance(1000);
        assert_eq!(checkpoint.last_scanned_block(), Some(1000));

        // Nothing new yet.
        assert_eq!(checkpoint.next_range(1000, LogStartBlock::Latest), None);

        // Chain advanced - next range picks up right after the checkpoint.
        let range = checkpoint.next_range(1050, LogStartBlock::Latest);
        assert_eq!(range, Some((1001, 1050)));
    }

    #[test]
    fn checkpoint_honors_explicit_start_block_on_first_scan() {
        let checkpoint = LogPollCheckpoint::new();
        let range = checkpoint.next_range(5000, LogStartBlock::Block(4000));
        assert_eq!(range, Some((4000, 5000)));
    }

    #[test]
    fn large_range_failure_shrinks_and_eventually_bottoms_out() {
        let (from, to) = (0u64, 10_000u64);
        let (from2, to2) = reduce_range_on_failure(from, to).expect("should shrink");
        assert_eq!(from2, 0);
        assert!(to2 < to, "reduced range must be smaller");

        // Keep shrinking until we hit a single-block range.
        let mut cur = (from2, to2);
        let mut iterations = 0;
        while let Some(next) = reduce_range_on_failure(cur.0, cur.1) {
            cur = next;
            iterations += 1;
            assert!(iterations < 100, "shrinking should converge quickly");
        }
        assert_eq!(cur.0, cur.1, "must bottom out at a single block");

        // A single block that still fails cannot be reduced further.
        assert_eq!(reduce_range_on_failure(cur.0, cur.0), None);
    }
}

'@
Set-Content -Path 'src\chain\log_poller.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/chain/log_poller.rs'

# ---- src/discovery_pipeline.rs ----
$content = @'
//! Orchestrates Day 2's HTTP-only pipeline: scan factories for newly
//! created pools, hydrate them (token metadata + on-chain state), then scan
//! already-known pools for swap events and fold them into `MarketState`.
//!
//! Deliberately transport-independent at the boundary that matters: this
//! module talks to `HttpLogPoller` directly (HTTP `eth_getLogs`), but
//! everything downstream of "raw `RpcLog`" - decoding, dedup (via
//! `MarketState::apply_event`), state application - is the exact same code
//! Day 1's WebSocket path uses. A future low-latency/WS feed for discovery
//! and swaps would plug in beside `HttpLogPoller`, not replace this
//! decode/apply logic.

use crate::chain::log_poller::{HttpLogPoller, LogPollCheckpoint};
use crate::config::{Config, LogStartBlock};
use crate::dex::discovery::{
    AerodromeClassicDiscovery, AerodromeSlipstreamDiscovery, DiscoveryParams, PoolDiscoveryAdapter,
    UniswapV3Discovery,
};
use crate::dex::traits::DexAdapter;
use crate::dex::{AerodromeAdapter, AerodromeSlipstreamAdapter, UniswapV3Adapter};
use crate::events::decoder::{aerodrome_swap_topic0, now_us, uniswap_v3_swap_topic0};
use crate::market::models::{DexKind, Pool, PoolKind, Token};
use crate::market::SharedMarketState;
use crate::pools::models::{DiscoverySource, PoolEligibility, PoolStatus};
use crate::pools::{PoolRegistry, TokenMetadataCache};
use alloy::primitives::{Address, U256};
use std::collections::HashMap;

pub struct DiscoveryPipeline {
    rpc_url: String,
    chain_id: u64,
    log_start_block: LogStartBlock,
    log_poller: HttpLogPoller,

    discovery_adapters: Vec<Box<dyn PoolDiscoveryAdapter>>,
    dex_adapters: HashMap<DexKind, Box<dyn DexAdapter>>,

    pub registry: PoolRegistry,
    token_cache: TokenMetadataCache,

    discovery_checkpoint: LogPollCheckpoint,
    swap_checkpoint: LogPollCheckpoint,
}

impl DiscoveryPipeline {
    pub fn new(config: &Config, chain_id: u64) -> Self {
        let mut discovery_adapters: Vec<Box<dyn PoolDiscoveryAdapter>> = vec![
            Box::new(UniswapV3Discovery::new(config.uniswap_v3_factory_address)),
            Box::new(AerodromeClassicDiscovery::new(
                config.aerodrome_factory_address,
            )),
        ];
        if let Some(addr) = config.aerodrome_slipstream_factory_address {
            discovery_adapters.push(Box::new(AerodromeSlipstreamDiscovery::new(addr)));
        } else {
            tracing::info!(
                "AERODROME_SLIPSTREAM_FACTORY_ADDRESS not configured - Slipstream pool \
                 discovery is disabled. See README for how to verify the address before \
                 enabling it."
            );
        }

        let mut dex_adapters: HashMap<DexKind, Box<dyn DexAdapter>> = HashMap::new();
        dex_adapters.insert(DexKind::UniswapV3, Box::new(UniswapV3Adapter::new()));
        dex_adapters.insert(DexKind::Aerodrome, Box::new(AerodromeAdapter::new()));
        dex_adapters.insert(
            DexKind::AerodromeSlipstream,
            Box::new(AerodromeSlipstreamAdapter::new()),
        );

        DiscoveryPipeline {
            rpc_url: config.base_rpc_url.clone(),
            chain_id,
            log_start_block: config.log_start_block,
            log_poller: HttpLogPoller::new(
                config.base_rpc_url.clone(),
                config.log_poll_max_block_range,
            ),
            discovery_adapters,
            dex_adapters,
            registry: PoolRegistry::new(),
            token_cache: TokenMetadataCache::new(),
            discovery_checkpoint: LogPollCheckpoint::new(),
            swap_checkpoint: LogPollCheckpoint::new(),
        }
    }

    /// One full pipeline pass: discover -> hydrate -> scan swaps. Safe to
    /// call repeatedly on a timer; every step is checkpointed and
    /// idempotent (redelivered logs/duplicate pools are no-ops, not
    /// errors).
    pub async fn run_once(&mut self, latest_block: u64, market_state: &SharedMarketState) {
        self.scan_discovery(latest_block).await;
        self.hydrate_pending_pools().await;
        self.scan_swaps(latest_block, market_state).await;
    }

    async fn scan_discovery(&mut self, latest_block: u64) {
        let Some((from, to)) = self
            .discovery_checkpoint
            .next_range(latest_block, self.log_start_block)
        else {
            return;
        };

        tracing::info!(
            source = "http_poll",
            scan = "discovery",
            range_from = from,
            range_to = to,
            "scanning for new pools"
        );

        for i in 0..self.discovery_adapters.len() {
            let factory_address = self.discovery_adapters[i].factory_address();
            let topic0 = self.discovery_adapters[i].event_topic0();

            let logs = match self
                .log_poller
                .fetch_logs(from, to, vec![factory_address], topic0)
                .await
            {
                Ok(logs) => logs,
                Err(err) => {
                    tracing::error!(
                        source = "http_poll",
                        scan = "discovery",
                        error = %err,
                        "discovery scan failed for this factory"
                    );
                    continue;
                }
            };

            tracing::info!(
                source = "http_poll",
                scan = "discovery",
                dex = self.discovery_adapters[i].dex().name(),
                logs_returned = logs.len(),
                "discovery scan complete for factory"
            );

            for log in &logs {
                // Defensive reorg guard - see module/README notes: this
                // skips a retracted log rather than applying it as a real
                // discovery event. It does NOT retroactively undo any state
                // from a previous poll; full reorg reconciliation is not
                // implemented.
                if log.removed {
                    tracing::warn!(
                        source = "http_poll",
                        "skipping removed=true log (reorg guard, not full reconciliation)"
                    );
                    continue;
                }

                let discovered = match self.discovery_adapters[i].decode_pool_created(log) {
                    Ok(d) => d,
                    Err(err) => {
                        tracing::error!(source = "http_poll", error = %err, "failed to decode PoolCreated log, skipping");
                        continue;
                    }
                };

                let placeholder_pool = Pool {
                    address: discovered.pool_address,
                    dex: discovered.dex,
                    token0: Token {
                        address: discovered.token0_address,
                        symbol: String::new(),
                        decimals: 0,
                    },
                    token1: Token {
                        address: discovered.token1_address,
                        symbol: String::new(),
                        decimals: 0,
                    },
                    kind: placeholder_pool_kind(&discovered.params),
                };

                let inserted = self.registry.insert_discovered(
                    placeholder_pool,
                    DiscoverySource::FactoryEvent {
                        factory_address: discovered.factory_address,
                        block_number: discovered.block_number,
                        tx_hash: discovered.tx_hash,
                    },
                    discovered.block_number,
                );

                if inserted {
                    tracing::info!(
                        source = "http_poll",
                        event = "pool_discovered",
                        dex = discovered.dex.name(),
                        pool = %discovered.pool_address,
                        token0 = %discovered.token0_address,
                        token1 = %discovered.token1_address,
                        block = discovered.block_number,
                        "pool discovered"
                    );
                }
            }
        }

        self.discovery_checkpoint.advance(to);
    }

    async fn hydrate_pending_pools(&mut self) {
        let pending: Vec<Address> = self
            .registry
            .iter()
            .filter(|(_, record)| record.status == PoolStatus::Discovered)
            .map(|(addr, _)| *addr)
            .collect();

        for address in pending {
            self.registry.set_status(&address, PoolStatus::Hydrating);

            let (dex, token0_addr, token1_addr) = {
                let record = self.registry.get(&address).expect("just looked up");
                (
                    record.pool.dex,
                    record.pool.token0.address,
                    record.pool.token1.address,
                )
            };

            let token0 = self
                .token_cache
                .get_or_fetch(&self.rpc_url, token0_addr)
                .await;
            let token1 = self
                .token_cache
                .get_or_fetch(&self.rpc_url, token1_addr)
                .await;

            let (token0, token1) = match (token0, token1) {
                (Ok(t0), Ok(t1)) => (t0, t1),
                _ => {
                    tracing::warn!(
                        source = "http_poll",
                        pool = %address,
                        "token metadata hydration failed (decimals unavailable) - marking pool inactive"
                    );
                    self.registry.set_status(&address, PoolStatus::Inactive);
                    continue;
                }
            };

            let Some(adapter) = self.dex_adapters.get(&dex) else {
                self.registry.set_status(&address, PoolStatus::Inactive);
                continue;
            };

            let skeleton_kind = self
                .registry
                .get(&address)
                .map(|r| r.pool.kind.clone())
                .unwrap_or(PoolKind::Aerodrome {
                    reserve0: U256::ZERO,
                    reserve1: U256::ZERO,
                    stable: false,
                });

            let skeleton = Pool {
                address,
                dex,
                token0,
                token1,
                kind: skeleton_kind,
            };

            match adapter.get_pool_state(&self.rpc_url, &skeleton).await {
                Ok(pool_state) => {
                    if let Some(record) = self.registry.get_mut(&address) {
                        let liquidity_available = has_liquidity(&pool_state.pool.kind);
                        record.pool = pool_state.pool;
                        record.status = PoolStatus::Active;
                        record.last_updated_block = pool_state.freshness.last_updated_block;
                        record.last_updated_timestamp =
                            pool_state.freshness.last_updated_timestamp;
                        record.eligibility = PoolEligibility {
                            verified_protocol: true,
                            token_metadata_available: true,
                            pool_type_supported: true,
                            liquidity_available,
                            state_readable: true,
                        };
                        tracing::info!(
                            source = "http_poll",
                            event = "pool_hydrated",
                            dex = dex.name(),
                            pool = %address,
                            eligibility = ?record.eligibility.status(),
                            "pool hydrated"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(source = "http_poll", pool = %address, error = %err, "state hydration failed - marking pool inactive");
                    self.registry.set_status(&address, PoolStatus::Inactive);
                }
            }
        }
    }

    async fn scan_swaps(&mut self, latest_block: u64, market_state: &SharedMarketState) {
        let Some((from, to)) = self
            .swap_checkpoint
            .next_range(latest_block, self.log_start_block)
        else {
            return;
        };

        let active_by_dex: HashMap<DexKind, Vec<Address>> = {
            let mut map: HashMap<DexKind, Vec<Address>> = HashMap::new();
            for (addr, record) in self.registry.iter() {
                if record.status == PoolStatus::Active {
                    map.entry(record.pool.dex).or_default().push(*addr);
                }
            }
            map
        };

        if active_by_dex.is_empty() {
            self.swap_checkpoint.advance(to);
            return;
        }

        tracing::info!(
            source = "http_poll",
            scan = "swaps",
            range_from = from,
            range_to = to,
            pools_watched = active_by_dex.values().map(|v| v.len()).sum::<usize>(),
            "scanning known pools for swap events"
        );

        for (dex, addresses) in &active_by_dex {
            let topic0 = match dex {
                DexKind::Aerodrome => aerodrome_swap_topic0(),
                DexKind::UniswapV3 | DexKind::AerodromeSlipstream => uniswap_v3_swap_topic0(),
            };

            let logs = match self
                .log_poller
                .fetch_logs(from, to, addresses.clone(), topic0)
                .await
            {
                Ok(logs) => logs,
                Err(err) => {
                    tracing::error!(
                        source = "http_poll",
                        scan = "swaps",
                        dex = dex.name(),
                        error = %err,
                        "swap scan failed"
                    );
                    continue;
                }
            };

            tracing::info!(
                source = "http_poll",
                scan = "swaps",
                dex = dex.name(),
                logs_returned = logs.len(),
                "swap scan complete"
            );

            let Some(adapter) = self.dex_adapters.get(dex) else {
                continue;
            };

            for log in &logs {
                if log.removed {
                    tracing::warn!(
                        source = "http_poll",
                        "skipping removed=true swap log (reorg guard, not full reconciliation)"
                    );
                    continue;
                }

                let received_at_us = now_us();
                match adapter.decode_event(log, self.chain_id, received_at_us) {
                    Ok(event) => {
                        let mut guard = market_state.write().await;
                        if guard.apply_event(event.clone()) {
                            crate::telemetry::log_event_received(&event);
                        } else {
                            crate::telemetry::log_event_duplicate(&event);
                        }
                    }
                    Err(err) => {
                        tracing::error!(source = "http_poll", error = %err, "failed to decode swap log, skipping");
                    }
                }
            }
        }

        self.swap_checkpoint.advance(to);
    }
}

fn placeholder_pool_kind(params: &DiscoveryParams) -> PoolKind {
    match params {
        DiscoveryParams::AerodromeClassic { stable } => PoolKind::Aerodrome {
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
            stable: *stable,
        },
        DiscoveryParams::ConcentratedLiquidity { tick_spacing } => {
            PoolKind::ConcentratedLiquidity {
                fee_tier: 0,
                tick_spacing: *tick_spacing,
                sqrt_price_x96: U256::ZERO,
                current_tick: 0,
                liquidity: 0,
                initialized_ticks: Default::default(),
            }
        }
    }
}

fn has_liquidity(kind: &PoolKind) -> bool {
    match kind {
        PoolKind::Aerodrome {
            reserve0, reserve1, ..
        } => !reserve0.is_zero() && !reserve1.is_zero(),
        PoolKind::ConcentratedLiquidity { liquidity, .. } => *liquidity > 0,
    }
}

'@
Set-Content -Path 'src\discovery_pipeline.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/discovery_pipeline.rs'

# ---- src/main.rs ----
$content = @'
mod chain;
mod config;
mod dex;
mod discovery_pipeline;
mod error;
mod events;
mod market;
mod pools;
mod telemetry;

use crate::chain::{BaseChainSource, ChainEventSource};
use crate::config::Config;
use crate::dex::DexAdapter;
use crate::discovery_pipeline::DiscoveryPipeline;
use crate::error::EngineResult;
use crate::events::decoder::now_us;
use crate::market::{BlockState, DexKind, MarketState};
use futures::StreamExt;

#[tokio::main]
async fn main() -> EngineResult<()> {
    let config = Config::load()?;
    telemetry::init_tracing(&config.log_level);

    tracing::info!(
        execution_mode = ?config.execution_mode,
        can_execute_trades = config.execution_mode.can_execute_trades(),
        "starting base-arb-engine (Day 2: pool discovery + market-state indexing)"
    );

    let chain_source = BaseChainSource::new(
        config.base_rpc_url.clone(),
        config.base_ws_url.clone(),
        config.http_poll_interval,
    );

    // --- Verify connectivity ---
    let chain_id = chain_source.chain_id().await?;
    if chain_id != config.base_chain_id {
        tracing::warn!(
            configured = config.base_chain_id,
            observed = chain_id,
            "configured BASE_CHAIN_ID does not match chain ID reported by RPC endpoint"
        );
    } else {
        tracing::info!(chain_id, "chain ID verified");
    }

    let latest_block = chain_source.latest_block_number().await?;
    tracing::info!(latest_block, "retrieved latest Base block");

    let state = MarketState::new_shared();
    {
        let mut guard = state.write().await;
        guard.update_latest_block(BlockState {
            number: latest_block,
            timestamp: None,
            hash: None,
        });
    }

    // --- Optional: register configured pools ---
    let aerodrome_adapter = dex::AerodromeAdapter::new();
    let uniswap_v3_adapter = dex::UniswapV3Adapter::new();

    let mut watched_addresses = Vec::new();
    if let Some(addr) = &config.aerodrome_pool_address {
        match addr.parse::<alloy::primitives::Address>() {
            Ok(a) => {
                tracing::info!(pool = %a, dex = "aerodrome", "watching configured pool");
                watched_addresses.push(a);
            }
            Err(e) => tracing::error!(error = %e, "invalid AERODROME_POOL_ADDRESS, ignoring"),
        }
    } else {
        tracing::info!(
            "AERODROME_POOL_ADDRESS not configured - generic ingestion layer will run without a \
             known Aerodrome pool. Set it to a verified pool address to process real events."
        );
    }
    if let Some(addr) = &config.uniswap_v3_pool_address {
        match addr.parse::<alloy::primitives::Address>() {
            Ok(a) => {
                tracing::info!(pool = %a, dex = "uniswap_v3", "watching configured pool");
                watched_addresses.push(a);
            }
            Err(e) => tracing::error!(error = %e, "invalid UNISWAP_V3_POOL_ADDRESS, ignoring"),
        }
    } else {
        tracing::info!(
            "UNISWAP_V3_POOL_ADDRESS not configured - generic ingestion layer will run without a \
             known Uniswap V3 pool. Set it to a verified pool address to process real events."
        );
    }

    if config.base_ws_url.is_none() {
        tracing::warn!(
            source = "http_poll",
            "BASE_WS_URL not configured - entering HTTP fallback mode. Block ingestion will \
             poll the configured BASE_RPC_URL periodically instead of streaming over WebSocket. \
             Log/event ingestion for configured pools is unavailable in this mode (it requires \
             WebSocket)."
        );
    } else {
        tracing::info!(source = "websocket", "WebSocket endpoint configured - using streaming ingestion");
    }

    // --- Block stream (WebSocket push, or HTTP-poll fallback - selected
    // internally by BaseChainSource::mode(); see chain::base module docs) ---
    let mut block_stream = chain_source.subscribe_blocks().await?;
    let block_state = state.clone();
    tokio::spawn(async move {
        while let Some(block) = block_stream.next().await {
            let mut guard = block_state.write().await;
            let number = block.number;
            guard.update_latest_block(block);
            tracing::debug!(block = number, "new block");
        }
    });

    // --- Day 2: pool discovery + hydration + known-pool swap scanning.
    // Always HTTP (`eth_getLogs` polling), independent of whether the block
    // stream above is WebSocket or HTTP-poll - see discovery_pipeline
    // module docs. Runs on the same cadence as HTTP_POLL_INTERVAL_SECS. ---
    {
        let mut pipeline = DiscoveryPipeline::new(&config, chain_id);
        let pipeline_chain_source = chain_source.clone();
        let pipeline_state = state.clone();
        let poll_interval = config.http_poll_interval;

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(poll_interval);
            loop {
                ticker.tick().await;
                let latest = match pipeline_chain_source.latest_block_number().await {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(source = "http_poll", error = %e, "failed to fetch latest block for discovery/swap scan, will retry next interval");
                        continue;
                    }
                };
                pipeline.run_once(latest, &pipeline_state).await;
            }
        });
    }

    // --- Log stream: WebSocket-only. Only attempted when WS is configured
    // AND there are pools to watch - HTTP-poll mode has no log ingestion
    // path today (latest-block polling only, per Day 1 scope). ---
    if chain_source.mode() == chain::ChainSourceMode::WebSocket && !watched_addresses.is_empty() {
        let mut log_stream = chain_source.subscribe_logs(watched_addresses).await?;
        let log_state = state.clone();
        let dex_by_address: std::collections::HashMap<alloy::primitives::Address, DexKind> = {
            let mut m = std::collections::HashMap::new();
            if let Some(addr) = &config.aerodrome_pool_address {
                if let Ok(a) = addr.parse() {
                    m.insert(a, DexKind::Aerodrome);
                }
            }
            if let Some(addr) = &config.uniswap_v3_pool_address {
                if let Ok(a) = addr.parse() {
                    m.insert(a, DexKind::UniswapV3);
                }
            }
            m
        };

        tokio::spawn(async move {
            while let Some(log) = log_stream.next().await {
                let received_at_us = now_us();
                let dex = dex_by_address.get(&log.inner.address).copied();
                let decoded = match dex {
                    Some(DexKind::Aerodrome) => {
                        aerodrome_adapter.decode_event(&log, chain_id, received_at_us)
                    }
                    Some(DexKind::UniswapV3) => {
                        uniswap_v3_adapter.decode_event(&log, chain_id, received_at_us)
                    }
                    // Day 1's WS pool-address config (AERODROME_POOL_ADDRESS /
                    // UNISWAP_V3_POOL_ADDRESS) never populates a Slipstream
                    // entry in dex_by_address, so this is unreachable in
                    // practice - but the match must still be exhaustive.
                    Some(DexKind::AerodromeSlipstream) | None => continue,
                };

                match decoded {
                    Ok(event) => {
                        let mut guard = log_state.write().await;
                        if guard.apply_event(event.clone()) {
                            telemetry::log_event_received(&event);
                        } else {
                            telemetry::log_event_duplicate(&event);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to decode event, skipping");
                    }
                }
            }
        });
    } else if chain_source.mode() == chain::ChainSourceMode::HttpPoll && !watched_addresses.is_empty() {
        tracing::warn!(
            source = "http_poll",
            "pool address(es) are configured but log/event ingestion is unavailable in HTTP \
             fallback mode - only latest-block polling is active. Configure BASE_WS_URL to \
             enable event ingestion for the configured pool(s)."
        );
    }

    tracing::info!("ingestion running - press Ctrl+C to shut down");
    tokio::signal::ctrl_c()
        .await
        .map_err(|e| crate::error::EngineError::Other(anyhow::anyhow!(e)))?;
    tracing::info!("shutdown signal received, exiting cleanly");

    Ok(())
}

'@
Set-Content -Path 'src\main.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/main.rs'

# ---- src/config.rs ----
$content = @'
use crate::error::{EngineError, EngineResult};
use alloy::primitives::Address;
use std::str::FromStr;

/// Uniswap V3 canonical factory on Base, per Uniswap's official deployments
/// page (developers.uniswap.org/docs/protocols/v3/deployments/v3-base-deployments).
/// Overridable via `UNISWAP_V3_FACTORY_ADDRESS` - never trust this blindly
/// for chains other than Base mainnet (8453).
const DEFAULT_UNISWAP_V3_FACTORY: &str = "0x33128a8fC17869897dcE68Ed026d694621f6FDfD";

/// Aerodrome classic (Solidly-style) PoolFactory on Base. Verified against
/// the contract's own source on BaseScan (labeled "Aerodrome: Pool Factory",
/// address 0x420DD381b31aEf6683db6B902084cB0FFECe40Da, actively creating
/// pools as of this writing). Overridable via `AERODROME_FACTORY_ADDRESS`.
const DEFAULT_AERODROME_FACTORY: &str = "0x420DD381b31aEf6683db6B902084cB0FFECe40Da";

/// Aerodrome Slipstream (concentrated-liquidity) CLFactory: deliberately
/// NOT defaulted here. At the time this was written the exact current
/// deployment address could not be independently confirmed from a primary
/// source (BaseScan's "Aerodrome: SlipStream Pool Factory" label resolves
/// to a `CLPool` implementation contract, not the factory's ABI/events) -
/// see README "Known limitations". Rather than guess, Slipstream discovery
/// is only enabled if the operator sets `AERODROME_SLIPSTREAM_FACTORY_ADDRESS`
/// to a value they've verified themselves (e.g. via Aerodrome's
/// FactoryRegistry.poolFactories() on Base at
/// 0x5C3F18F06CC09CA1910767A34a20F771039E37C0, or official docs).

/// Where the HTTP log poller should begin pool-discovery/swap scanning on
/// first startup (i.e. when no checkpoint exists yet). Never defaults to
/// scanning full chain history - that's a deliberate safety default per the
/// Day 2 spec ("do not automatically scan the entire history of Base").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStartBlock {
    /// Start from the current chain head - the safe default. No historical
    /// backfill.
    Latest,
    /// Start from an explicit, operator-chosen block number (controlled
    /// backfill).
    Block(u64),
}

impl FromStr for LogStartBlock {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("latest") {
            return Ok(LogStartBlock::Latest);
        }
        trimmed
            .parse::<u64>()
            .map(LogStartBlock::Block)
            .map_err(|_| {
                EngineError::Config(format!(
                    "invalid LOG_START_BLOCK '{trimmed}': expected 'latest' or a block number"
                ))
            })
    }
}

/// Execution mode gates what the engine is *allowed* to do at runtime.
///
/// Day 1 only ever runs in `DryRun`. `Simulation` and `Live` are defined now
/// so later days can extend the same config surface, but there is
/// deliberately no code path today that reads `Live` and does anything
/// other than refuse to proceed with trade execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    #[default]
    DryRun,
    Simulation,
    Live,
}

impl FromStr for ExecutionMode {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "DRY_RUN" | "DRYRUN" | "" => Ok(ExecutionMode::DryRun),
            "SIMULATION" | "SIM" => Ok(ExecutionMode::Simulation),
            "LIVE" => Ok(ExecutionMode::Live),
            other => Err(EngineError::Config(format!(
                "invalid EXECUTION_MODE '{other}': expected DRY_RUN | SIMULATION | LIVE"
            ))),
        }
    }
}

impl ExecutionMode {
    /// Day 1 hard rule: there is no trade path at all, regardless of mode.
    /// This function exists so any future execution entry point has a single,
    /// obvious place to check before doing anything irreversible.
    pub fn can_execute_trades(&self) -> bool {
        // Intentionally always false today. Day 1 acceptance criteria requires
        // that LIVE mode has no working trade path. When execution is built
        // (later days), this should still require explicit, separate
        // confirmation beyond just `self == Live`.
        false
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub base_rpc_url: String,
    pub base_ws_url: Option<String>,
    pub base_chain_id: u64,
    pub log_level: String,
    pub execution_mode: ExecutionMode,

    /// Optional, explicit pool addresses. Left unset unless the operator
    /// supplies verified addresses - see README "Not implemented yet" /
    /// DEX adapter docs. We never invent these.
    pub aerodrome_pool_address: Option<String>,
    pub uniswap_v3_pool_address: Option<String>,

    /// How often the HTTP-fallback chain source polls for the latest block
    /// when no WebSocket endpoint is configured (or WebSocket is otherwise
    /// unavailable). Only used in HTTP-poll mode - ignored in WebSocket
    /// mode, which is push-based.
    pub http_poll_interval: std::time::Duration,

    // --- Day 2: pool discovery / log polling ---
    /// Uniswap V3 factory address to watch for `PoolCreated` events.
    pub uniswap_v3_factory_address: Address,
    /// Aerodrome classic (Solidly-style) factory address to watch for
    /// `PoolCreated` events.
    pub aerodrome_factory_address: Address,
    /// Aerodrome Slipstream (concentrated-liquidity) factory address. No
    /// default - see `DEFAULT_AERODROME_FACTORY` docs above. Slipstream
    /// discovery is simply skipped if this is unset.
    pub aerodrome_slipstream_factory_address: Option<Address>,
    /// Maximum block range per `eth_getLogs` call. Chunked to stay under
    /// RPC-provider limits.
    pub log_poll_max_block_range: u64,
    /// Where to start scanning from when no checkpoint exists yet.
    pub log_start_block: LogStartBlock,
}

impl Config {
    /// Load configuration from environment variables (via `.env` if present).
    pub fn load() -> EngineResult<Self> {
        // Loading .env is best-effort: it's fine if it doesn't exist (e.g. in
        // containers where env vars are injected directly).
        let _ = dotenvy::dotenv();
        Self::load_from_env()
    }

    /// Loads config purely from whatever is already in the process
    /// environment, without touching any `.env` file on disk.
    ///
    /// This split exists because `dotenvy::dotenv()` only sets a variable if
    /// it isn't already set - so in tests that `remove_var` a variable to
    /// simulate it being missing, `dotenv()` would silently reload it from a
    /// developer's real local `.env` file (which is expected to contain real
    /// values for `cargo run`) and defeat the test. Tests call this
    /// directly; `main.rs` goes through `load()`.
    fn load_from_env() -> EngineResult<Self> {
        let base_rpc_url = require_env("BASE_RPC_URL")?;
        let base_ws_url = std::env::var("BASE_WS_URL").ok().filter(|s| !s.is_empty());

        let base_chain_id_raw = require_env("BASE_CHAIN_ID")?;
        let base_chain_id: u64 = base_chain_id_raw.parse().map_err(|_| {
            EngineError::Config(format!(
                "BASE_CHAIN_ID must be a positive integer, got '{base_chain_id_raw}'"
            ))
        })?;

        let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());

        let execution_mode = std::env::var("EXECUTION_MODE")
            .unwrap_or_else(|_| "DRY_RUN".to_string())
            .parse()?;

        let aerodrome_pool_address = std::env::var("AERODROME_POOL_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty());
        let uniswap_v3_pool_address = std::env::var("UNISWAP_V3_POOL_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty());

        let http_poll_interval_secs: u64 = std::env::var("HTTP_POLL_INTERVAL_SECS")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<u64>().map_err(|_| {
                    EngineError::Config(format!(
                        "HTTP_POLL_INTERVAL_SECS must be a positive integer, got '{s}'"
                    ))
                })
            })
            .transpose()?
            // Safe development default: frequent enough to be useful for a
            // Day 1 foundation, gentle enough not to hammer a public RPC.
            .unwrap_or(5);

        let uniswap_v3_factory_address = std::env::var("UNISWAP_V3_FACTORY_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_UNISWAP_V3_FACTORY.to_string())
            .parse::<Address>()
            .map_err(|e| {
                EngineError::Config(format!("invalid UNISWAP_V3_FACTORY_ADDRESS: {e}"))
            })?;

        let aerodrome_factory_address = std::env::var("AERODROME_FACTORY_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_AERODROME_FACTORY.to_string())
            .parse::<Address>()
            .map_err(|e| EngineError::Config(format!("invalid AERODROME_FACTORY_ADDRESS: {e}")))?;

        let aerodrome_slipstream_factory_address = std::env::var(
            "AERODROME_SLIPSTREAM_FACTORY_ADDRESS",
        )
        .ok()
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<Address>().map_err(|e| {
                EngineError::Config(format!(
                    "invalid AERODROME_SLIPSTREAM_FACTORY_ADDRESS: {e}"
                ))
            })
        })
        .transpose()?;

        let log_poll_max_block_range: u64 = std::env::var("LOG_POLL_MAX_BLOCK_RANGE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<u64>().map_err(|_| {
                    EngineError::Config(format!(
                        "LOG_POLL_MAX_BLOCK_RANGE must be a positive integer, got '{s}'"
                    ))
                })
            })
            .transpose()?
            // Conservative default: comfortably under most public RPC
            // providers' eth_getLogs range limits (commonly 2000-10000).
            .unwrap_or(2000);

        let log_start_block: LogStartBlock = std::env::var("LOG_START_BLOCK")
            .unwrap_or_else(|_| "latest".to_string())
            .parse()?;

        let cfg = Config {
            base_rpc_url,
            base_ws_url,
            base_chain_id,
            log_level,
            execution_mode,
            aerodrome_pool_address,
            uniswap_v3_pool_address,
            http_poll_interval: std::time::Duration::from_secs(http_poll_interval_secs),
            uniswap_v3_factory_address,
            aerodrome_factory_address,
            aerodrome_slipstream_factory_address,
            log_poll_max_block_range,
            log_start_block,
        };

        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> EngineResult<()> {
        if self.base_rpc_url.trim().is_empty() {
            return Err(EngineError::Config("BASE_RPC_URL is empty".into()));
        }
        if !(self.base_rpc_url.starts_with("http://") || self.base_rpc_url.starts_with("https://"))
        {
            return Err(EngineError::Config(
                "BASE_RPC_URL must start with http:// or https://".into(),
            ));
        }
        if let Some(ws) = &self.base_ws_url {
            if !(ws.starts_with("ws://") || ws.starts_with("wss://")) {
                return Err(EngineError::Config(
                    "BASE_WS_URL must start with ws:// or wss://".into(),
                ));
            }
        }
        if self.base_chain_id == 0 {
            return Err(EngineError::Config("BASE_CHAIN_ID must be nonzero".into()));
        }
        if self.http_poll_interval.as_secs() == 0 {
            return Err(EngineError::Config(
                "HTTP_POLL_INTERVAL_SECS must be greater than zero".into(),
            ));
        }
        if self.log_poll_max_block_range == 0 {
            return Err(EngineError::Config(
                "LOG_POLL_MAX_BLOCK_RANGE must be greater than zero".into(),
            ));
        }

        // Hard safety invariant, independent of ExecutionMode::can_execute_trades:
        // Day 1 refuses to even start if anything smells like a private key was
        // configured for use. We don't scan the whole environment (too broad /
        // fragile), but we never read one ourselves anywhere in this codebase.
        if std::env::var("PRIVATE_KEY").is_ok() {
            tracing::warn!(
                "PRIVATE_KEY is set in the environment but is never read by this program. \
                 Day 1 has no signer and no trade execution path."
            );
        }

        Ok(())
    }
}

fn require_env(key: &str) -> EngineResult<String> {
    std::env::var(key)
        .map_err(|_| EngineError::Config(format!("missing required environment variable: {key}")))
        .and_then(|v| {
            if v.trim().is_empty() {
                Err(EngineError::Config(format!(
                    "environment variable {key} is set but empty"
                )))
            } else {
                Ok(v)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Environment variables are process-global, so serialize tests that touch them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        for key in [
            "BASE_RPC_URL",
            "BASE_WS_URL",
            "BASE_CHAIN_ID",
            "LOG_LEVEL",
            "EXECUTION_MODE",
            "AERODROME_POOL_ADDRESS",
            "UNISWAP_V3_POOL_ADDRESS",
            "PRIVATE_KEY",
            "HTTP_POLL_INTERVAL_SECS",
            "UNISWAP_V3_FACTORY_ADDRESS",
            "AERODROME_FACTORY_ADDRESS",
            "AERODROME_SLIPSTREAM_FACTORY_ADDRESS",
            "LOG_POLL_MAX_BLOCK_RANGE",
            "LOG_START_BLOCK",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn valid_configuration_loads() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_WS_URL", "wss://mainnet.base.org/ws");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("LOG_LEVEL", "debug");

        let cfg = Config::load_from_env().expect("valid config should load");
        assert_eq!(cfg.base_chain_id, 8453);
        assert_eq!(cfg.execution_mode, ExecutionMode::DryRun);
        clear_env();
    }

    #[test]
    fn missing_required_configuration_produces_clear_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_CHAIN_ID", "8453");
        // BASE_RPC_URL intentionally missing.

        let err = Config::load_from_env().expect_err("missing BASE_RPC_URL should fail");
        match err {
            EngineError::Config(msg) => assert!(msg.contains("BASE_RPC_URL")),
            other => panic!("expected Config error, got {other:?}"),
        }
        clear_env();
    }

    #[test]
    fn invalid_chain_id_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "not-a-number");

        let err = Config::load_from_env().expect_err("non-numeric chain id should fail");
        assert!(matches!(err, EngineError::Config(_)));
        clear_env();
    }

    #[test]
    fn live_mode_never_permits_trade_execution() {
        assert!(!ExecutionMode::Live.can_execute_trades());
        assert!(!ExecutionMode::DryRun.can_execute_trades());
        assert!(!ExecutionMode::Simulation.can_execute_trades());
    }

    #[test]
    fn execution_mode_parses_case_insensitively() {
        assert_eq!("dry_run".parse::<ExecutionMode>().unwrap(), ExecutionMode::DryRun);
        assert_eq!("LIVE".parse::<ExecutionMode>().unwrap(), ExecutionMode::Live);
        assert_eq!(
            "simulation".parse::<ExecutionMode>().unwrap(),
            ExecutionMode::Simulation
        );
        assert!("bogus".parse::<ExecutionMode>().is_err());
    }

    #[test]
    fn empty_base_ws_url_is_treated_as_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("BASE_WS_URL", ""); // explicitly empty, not just unset

        let cfg = Config::load_from_env().expect("config without a WS endpoint should still load");
        assert!(
            cfg.base_ws_url.is_none(),
            "empty BASE_WS_URL must be normalized to None so HTTP fallback is selected"
        );
        clear_env();
    }

    #[test]
    fn unset_base_ws_url_is_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        // BASE_WS_URL not set at all.

        let cfg = Config::load_from_env().expect("config without BASE_WS_URL should still load");
        assert!(cfg.base_ws_url.is_none());
        clear_env();
    }

    #[test]
    fn http_poll_interval_defaults_to_a_safe_value_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");

        let cfg = Config::load_from_env().expect("config should load with default poll interval");
        assert_eq!(cfg.http_poll_interval, std::time::Duration::from_secs(5));
        clear_env();
    }

    #[test]
    fn http_poll_interval_is_configurable() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("HTTP_POLL_INTERVAL_SECS", "15");

        let cfg = Config::load_from_env().expect("config with custom poll interval should load");
        assert_eq!(cfg.http_poll_interval, std::time::Duration::from_secs(15));
        clear_env();
    }

    #[test]
    fn zero_http_poll_interval_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("HTTP_POLL_INTERVAL_SECS", "0");

        let err = Config::load_from_env().expect_err("zero poll interval must be rejected");
        assert!(matches!(err, EngineError::Config(_)));
        clear_env();
    }

    #[test]
    fn factory_addresses_default_to_verified_base_deployments() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");

        let cfg = Config::load_from_env().expect("config should load with default factories");
        assert_eq!(
            cfg.uniswap_v3_factory_address,
            DEFAULT_UNISWAP_V3_FACTORY.parse::<Address>().unwrap()
        );
        assert_eq!(
            cfg.aerodrome_factory_address,
            DEFAULT_AERODROME_FACTORY.parse::<Address>().unwrap()
        );
        assert!(
            cfg.aerodrome_slipstream_factory_address.is_none(),
            "Slipstream factory must NOT have a guessed default"
        );
        clear_env();
    }

    #[test]
    fn factory_addresses_are_overridable() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var(
            "AERODROME_SLIPSTREAM_FACTORY_ADDRESS",
            "0xdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaD",
        );

        let cfg = Config::load_from_env().expect("config should load with override");
        assert_eq!(
            cfg.aerodrome_slipstream_factory_address,
            Some(
                "0xdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaD"
                    .parse::<Address>()
                    .unwrap()
            )
        );
        clear_env();
    }

    #[test]
    fn log_start_block_defaults_to_latest() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");

        let cfg = Config::load_from_env().expect("config should load");
        assert_eq!(cfg.log_start_block, LogStartBlock::Latest);
        clear_env();
    }

    #[test]
    fn log_start_block_accepts_explicit_block_number() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("LOG_START_BLOCK", "12345678");

        let cfg = Config::load_from_env().expect("config should load");
        assert_eq!(cfg.log_start_block, LogStartBlock::Block(12345678));
        clear_env();
    }

    #[test]
    fn zero_log_poll_max_block_range_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("LOG_POLL_MAX_BLOCK_RANGE", "0");

        let err = Config::load_from_env().expect_err("zero max range must be rejected");
        assert!(matches!(err, EngineError::Config(_)));
        clear_env();
    }
}

'@
Set-Content -Path 'src\config.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/config.rs'

# ---- src/chain/mod.rs ----
$content = @'
pub mod base;
pub mod log_poller;
pub mod source;

pub use base::{BaseChainSource, ChainSourceMode};
pub use log_poller::{HttpLogPoller, LogPollCheckpoint};
pub use source::ChainEventSource;

'@
Set-Content -Path 'src\chain\mod.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/chain/mod.rs'

# ---- src/dex/mod.rs ----
$content = @'
pub mod aerodrome;
pub mod aerodrome_slipstream;
pub mod discovery;
pub mod traits;
pub mod uniswap_v3;

pub use aerodrome::AerodromeAdapter;
pub use aerodrome_slipstream::AerodromeSlipstreamAdapter;
pub use traits::DexAdapter;
pub use uniswap_v3::UniswapV3Adapter;

'@
Set-Content -Path 'src\dex\mod.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/mod.rs'

# ---- src/dex/uniswap_v3.rs ----
$content = @'
//! Uniswap V3 adapter.
//!
//! Uniswap V3 is concentrated liquidity, NOT a two-reserve constant-product
//! pool. This adapter reads `slot0` (sqrtPriceX96, tick) and `liquidity`
//! directly - it does not synthesize fake reserves. Full tick-bitmap
//! hydration and the swap-simulation math are Day 2+ (they require walking
//! initialized ticks, which needs either many RPC calls or a dedicated
//! indexer - out of scope for the Day 1 foundation).

use crate::dex::traits::DexAdapter;
use crate::error::{EngineError, EngineResult};
use crate::events::decoder;
use crate::events::model::MarketEvent;
use crate::market::models::{Pool, PoolKind, PoolState};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use async_trait::async_trait;

sol! {
    #[sol(rpc)]
    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
    }
}

pub struct UniswapV3Adapter;

impl UniswapV3Adapter {
    pub fn new() -> Self {
        UniswapV3Adapter
    }
}

impl Default for UniswapV3Adapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DexAdapter for UniswapV3Adapter {
    fn name(&self) -> &'static str {
        "uniswap_v3"
    }

    async fn get_pool_state(&self, rpc_url: &str, pool: &Pool) -> EngineResult<PoolState> {
        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let contract = IUniswapV3Pool::new(pool.address, provider.clone());

        let slot0 = contract.slot0().call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("slot0() failed: {e}"),
        })?;
        let liquidity = contract
            .liquidity()
            .call()
            .await
            .map_err(|e| EngineError::Dex {
                dex: self.name().into(),
                reason: format!("liquidity() failed: {e}"),
            })?;
        let fee = contract.fee().call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("fee() failed: {e}"),
        })?;
        let tick_spacing = contract
            .tickSpacing()
            .call()
            .await
            .map_err(|e| EngineError::Dex {
                dex: self.name().into(),
                reason: format!("tickSpacing() failed: {e}"),
            })?;

        let block_number = provider
            .get_block_number()
            .await
            .map_err(|e| EngineError::Chain(format!("get_block_number failed: {e}")))?;

        // Field access verified against alloy-core 1.6.0 / alloy 2.4.1
        // source: multi-output Sol functions (slot0) generate a named
        // struct (`.sqrtPriceX96`, `.tick`, ...); single-output functions
        // (fee, tickSpacing, liquidity) return the bare Rust type directly
        // (no `._0` wrapper). Sub-word Sol ints (`uint24`/`int24`) are
        // `ruint` `Uint`/`Signed` wrapper types converted via
        // `.to::<T>()`/`.as_i32()`.
        let mut updated_pool = pool.clone();
        updated_pool.kind = PoolKind::ConcentratedLiquidity {
            fee_tier: fee.to::<u32>(),
            tick_spacing: tick_spacing.as_i32(),
            sqrt_price_x96: alloy::primitives::U256::from(slot0.sqrtPriceX96),
            current_tick: slot0.tick.as_i32(),
            liquidity,
            initialized_ticks: Default::default(),
        };

        Ok(PoolState::new(updated_pool, block_number, None))
    }

    fn decode_event(
        &self,
        log: &RpcLog,
        chain_id: u64,
        received_at_us: u64,
    ) -> EngineResult<MarketEvent> {
        decoder::decode_uniswap_v3_log(log, chain_id, received_at_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::models::{DexKind, Token};
    use alloy::primitives::{address, U256};

    #[test]
    fn uniswap_v3_pool_model_can_represent_v3_state() {
        let pool = Pool {
            address: address!("0000000000000000000000000000000000000003"),
            dex: DexKind::UniswapV3,
            token0: Token {
                address: address!("4200000000000000000000000000000000000006"),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: address!("0000000000000000000000000000000000000002"),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::ConcentratedLiquidity {
                fee_tier: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::from(79_228_162_514_264_337_593_543_950_336u128),
                current_tick: -1234,
                liquidity: 123_456_789,
                initialized_ticks: Default::default(),
            },
        };

        match pool.kind {
            PoolKind::ConcentratedLiquidity {
                fee_tier,
                current_tick,
                ..
            } => {
                assert_eq!(fee_tier, 500);
                assert_eq!(current_tick, -1234);
            }
            _ => panic!("expected UniswapV3 pool kind - not a fake x*y=k model"),
        }
    }
}

'@
Set-Content -Path 'src\dex\uniswap_v3.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/uniswap_v3.rs'

# ---- src/market/models.rs ----
$content = @'
//! Normalized, protocol-agnostic market state models.
//!
//! Everything here uses integer/token-native units. No `f64`/`f32` anywhere
//! in this module - see the financial-code rules in the Day 1 spec.

use alloy::primitives::{Address, B256, U256};
use serde::{Deserialize, Serialize};

/// Monotonically increasing state version. Every accepted state transition
/// in `MarketState` advances this by exactly one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct StateVersion(pub u64);

impl StateVersion {
    pub fn genesis() -> Self {
        StateVersion(0)
    }

    pub fn next(self) -> Self {
        StateVersion(self.0.saturating_add(1))
    }
}

/// Which DEX a pool/event belongs to. Kept as a small closed enum rather than
/// a string so adapter dispatch is exhaustive-checked by the compiler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DexKind {
    /// Classic Aerodrome (Solidly-style) volatile/stable pools.
    Aerodrome,
    /// Aerodrome Slipstream: concentrated-liquidity pools, a separate AMM
    /// design from classic Aerodrome (different factory, different pool
    /// contract, different math) - never collapsed into `Aerodrome`.
    AerodromeSlipstream,
    UniswapV3,
}

impl DexKind {
    pub fn name(&self) -> &'static str {
        match self {
            DexKind::Aerodrome => "aerodrome",
            DexKind::AerodromeSlipstream => "aerodrome_slipstream",
            DexKind::UniswapV3 => "uniswap_v3",
        }
    }
}

impl std::fmt::Display for DexKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Minimal ERC-20 description. `decimals` is required for any future
/// unit-scaling logic - never inferred, never defaulted to 18.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub address: Address,
    pub symbol: String,
    pub decimals: u8,
}

/// Protocol-specific pool mechanics. Deliberately NOT unified into a single
/// "reserve0/reserve1" shape - Uniswap V3 is concentrated liquidity, not
/// constant-product, and collapsing it into a fake x*y=k model would produce
/// wrong prices later. See Day 1 spec section 6.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PoolKind {
    /// Aerodrome (Solidly-style) pool. `stable` distinguishes the stable
    /// (curve-like) formula from the volatile (x*y=k) formula - these are
    /// different pools with different math, not a toggle on one model.
    Aerodrome {
        reserve0: U256,
        reserve1: U256,
        stable: bool,
    },
    /// Concentrated-liquidity state shape, shared by Uniswap V3 and
    /// Aerodrome Slipstream (structurally identical mechanics - a different
    /// factory/deployment, not different math). `Pool.dex` is what
    /// distinguishes which protocol a given pool actually belongs to.
    /// `initialized_ticks` is a sparse map of tick index -> net liquidity,
    /// populated lazily as ticks are observed; Day 1/2 do not need the full
    /// tick bitmap hydrated.
    ConcentratedLiquidity {
        fee_tier: u32,
        tick_spacing: i32,
        sqrt_price_x96: U256,
        current_tick: i32,
        liquidity: u128,
        #[serde(default)]
        initialized_ticks: std::collections::BTreeMap<i32, i128>,
    },
}

/// A pool identity + its protocol-specific state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pool {
    pub address: Address,
    pub dex: DexKind,
    pub token0: Token,
    pub token1: Token,
    pub kind: PoolKind,
}

/// Freshness / versioning metadata attached to every stored pool state.
/// The Day 2+ opportunity engine will use this to reject stale reads -
/// Day 1 only needs to record it correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Freshness {
    pub last_updated_block: u64,
    pub last_updated_timestamp: Option<u64>,
    pub state_version: StateVersion,
}

/// A pool plus the freshness metadata for its current stored state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolState {
    pub pool: Pool,
    pub freshness: Freshness,
}

impl PoolState {
    pub fn new(pool: Pool, block_number: u64, block_timestamp: Option<u64>) -> Self {
        PoolState {
            pool,
            freshness: Freshness {
                last_updated_block: block_number,
                last_updated_timestamp: block_timestamp,
                state_version: StateVersion::genesis(),
            },
        }
    }
}

/// Snapshot of the chain head as observed by the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockState {
    pub number: u64,
    pub timestamp: Option<u64>,
    pub hash: Option<B256>,
}

'@
Set-Content -Path 'src\market\models.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/market/models.rs'

# ---- src/events/decoder.rs ----
$content = @'
//! Decodes raw chain logs into normalized `MarketEvent`s.
//!
//! Decoding is protocol-specific (Aerodrome's Solidly-style Swap event vs
//! Uniswap V3's concentrated-liquidity Swap event have different shapes),
//! but the *output* is always the same normalized `MarketEvent`. Malformed
//! logs are rejected with a clear error rather than silently dropped or
//! guessed at.

use crate::error::{EngineError, EngineResult};
use crate::events::model::{EventKind, MarketEvent, SwapEvent};
use crate::market::models::DexKind;
use alloy::primitives::Log as PrimitiveLog;
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use alloy::sol_types::SolEvent;

sol! {
    /// Aerodrome (Solidly-fork) pool Swap event.
    event AerodromeSwap(
        address indexed sender,
        address indexed to,
        uint256 amount0In,
        uint256 amount1In,
        uint256 amount0Out,
        uint256 amount1Out
    );

    /// Uniswap V3 pool Swap event.
    event UniswapV3Swap(
        address indexed sender,
        address indexed recipient,
        int256 amount0,
        int256 amount1,
        uint160 sqrtPriceX96,
        uint128 liquidity,
        int24 tick
    );
}

/// Current wall-clock time in microseconds since the Unix epoch.
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// keccak256 topic0 for `AerodromeSwap`. Exposed so the Day 2 log-polling
/// pipeline can filter `eth_getLogs` queries to just this event without
/// duplicating the event definition.
pub fn aerodrome_swap_topic0() -> alloy::primitives::B256 {
    AerodromeSwap::SIGNATURE_HASH
}

/// keccak256 topic0 for `UniswapV3Swap`. Also used for Aerodrome Slipstream
/// (`CLPool`) swap scanning, which reuses this event shape - see
/// `dex::aerodrome_slipstream` module docs.
pub fn uniswap_v3_swap_topic0() -> alloy::primitives::B256 {
    UniswapV3Swap::SIGNATURE_HASH
}

/// Decode a raw RPC log from a known Aerodrome pool into a `MarketEvent`.
///
/// `received_at_us` should be captured by the caller at the moment the log
/// was received from the transport, before any decoding work happens, so
/// `processing_latency_us` reflects actual decode+normalize cost.
pub fn decode_aerodrome_log(
    log: &RpcLog,
    chain_id: u64,
    received_at_us: u64,
) -> EngineResult<MarketEvent> {
    let processing_started_at_us = now_us();

    let block_number = log
        .block_number
        .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;
    let log_index = log
        .log_index
        .ok_or_else(|| EngineError::MalformedEvent("log missing log_index".into()))?;
    let pool_address = log.inner.address;

    let decoded_log = AerodromeSwap::decode_log(&log.inner).map_err(|e| {
        EngineError::Decode(format!("failed to decode AerodromeSwap log: {e}"))
    })?;
    let decoded = decoded_log.data;

    // Solidly-style pools report gross in/out per side rather than a single
    // signed net amount; normalize to net-into-pool the same way Uniswap V3
    // does, so downstream code has one shape to reason about.
    let amount0_in = i128::try_from(decoded.amount0In)
        .map_err(|_| EngineError::MalformedEvent("amount0In overflows i128".into()))?;
    let amount0_out = i128::try_from(decoded.amount0Out)
        .map_err(|_| EngineError::MalformedEvent("amount0Out overflows i128".into()))?;
    let amount1_in = i128::try_from(decoded.amount1In)
        .map_err(|_| EngineError::MalformedEvent("amount1In overflows i128".into()))?;
    let amount1_out = i128::try_from(decoded.amount1Out)
        .map_err(|_| EngineError::MalformedEvent("amount1Out overflows i128".into()))?;

    let amount0 = amount0_in
        .checked_sub(amount0_out)
        .ok_or_else(|| EngineError::Arithmetic("amount0 net overflow".into()))?;
    let amount1 = amount1_in
        .checked_sub(amount1_out)
        .ok_or_else(|| EngineError::Arithmetic("amount1 net overflow".into()))?;

    let swap = SwapEvent {
        pool_address,
        dex: DexKind::Aerodrome,
        amount0,
        amount1,
        sender: Some(decoded.sender),
        recipient: Some(decoded.to),
    };

    let processing_finished_at_us = now_us();

    Ok(MarketEvent::new(
        chain_id,
        block_number,
        None,
        tx_hash,
        log_index,
        pool_address,
        DexKind::Aerodrome,
        EventKind::Swap(swap),
        received_at_us,
        processing_started_at_us,
        processing_finished_at_us,
    ))
}

/// Decode a raw RPC log from a known Uniswap V3 pool into a `MarketEvent`.
pub fn decode_uniswap_v3_log(
    log: &RpcLog,
    chain_id: u64,
    received_at_us: u64,
) -> EngineResult<MarketEvent> {
    let processing_started_at_us = now_us();

    let block_number = log
        .block_number
        .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;
    let log_index = log
        .log_index
        .ok_or_else(|| EngineError::MalformedEvent("log missing log_index".into()))?;
    let pool_address = log.inner.address;

    let decoded_log = UniswapV3Swap::decode_log(&log.inner).map_err(|e| {
        EngineError::Decode(format!("failed to decode UniswapV3Swap log: {e}"))
    })?;
    let decoded = decoded_log.data;

    let amount0 = i128::try_from(decoded.amount0)
        .map_err(|_| EngineError::MalformedEvent("amount0 overflows i128".into()))?;
    let amount1 = i128::try_from(decoded.amount1)
        .map_err(|_| EngineError::MalformedEvent("amount1 overflows i128".into()))?;

    let swap = SwapEvent {
        pool_address,
        dex: DexKind::UniswapV3,
        amount0,
        amount1,
        sender: Some(decoded.sender),
        recipient: Some(decoded.recipient),
    };

    let processing_finished_at_us = now_us();

    Ok(MarketEvent::new(
        chain_id,
        block_number,
        None,
        tx_hash,
        log_index,
        pool_address,
        DexKind::UniswapV3,
        EventKind::Swap(swap),
        received_at_us,
        processing_started_at_us,
        processing_finished_at_us,
    ))
}

/// Decode a raw RPC log from a known Aerodrome Slipstream (`CLPool`) pool.
///
/// Reuses the `UniswapV3Swap` event shape: Slipstream's `Swap` event is
/// documented as "adapted from Uniswap V3's core contracts", and shares the
/// same `(sender, recipient, amount0, amount1, sqrtPriceX96, liquidity,
/// tick)` signature in every source reviewed for this implementation. If
/// real Slipstream swap logs fail to decode against this shape, that
/// assumption is the first thing to re-verify (see
/// `dex::aerodrome_slipstream` module docs for the same caveat on the
/// discovery event).
pub fn decode_aerodrome_slipstream_log(
    log: &RpcLog,
    chain_id: u64,
    received_at_us: u64,
) -> EngineResult<MarketEvent> {
    let processing_started_at_us = now_us();

    let block_number = log
        .block_number
        .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;
    let log_index = log
        .log_index
        .ok_or_else(|| EngineError::MalformedEvent("log missing log_index".into()))?;
    let pool_address = log.inner.address;

    let decoded_log = UniswapV3Swap::decode_log(&log.inner).map_err(|e| {
        EngineError::Decode(format!("failed to decode Slipstream Swap log: {e}"))
    })?;
    let decoded = decoded_log.data;

    let amount0 = i128::try_from(decoded.amount0)
        .map_err(|_| EngineError::MalformedEvent("amount0 overflows i128".into()))?;
    let amount1 = i128::try_from(decoded.amount1)
        .map_err(|_| EngineError::MalformedEvent("amount1 overflows i128".into()))?;

    let swap = SwapEvent {
        pool_address,
        dex: DexKind::AerodromeSlipstream,
        amount0,
        amount1,
        sender: Some(decoded.sender),
        recipient: Some(decoded.recipient),
    };

    let processing_finished_at_us = now_us();

    Ok(MarketEvent::new(
        chain_id,
        block_number,
        None,
        tx_hash,
        log_index,
        pool_address,
        DexKind::AerodromeSlipstream,
        EventKind::Swap(swap),
        received_at_us,
        processing_started_at_us,
        processing_finished_at_us,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, Address, Bytes, B256};
    use alloy::rpc::types::Log as RpcLog;
    use alloy::sol_types::SolEvent;

    fn build_log(topics: Vec<B256>, data: Bytes, addr: Address) -> RpcLog {
        let inner = PrimitiveLog::new_unchecked(addr, topics, data);
        RpcLog {
            inner,
            block_hash: None,
            block_number: Some(12_345_678),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(0xAB)),
            transaction_index: Some(0),
            log_index: Some(3),
            removed: false,
        }
    }

    #[test]
    fn valid_uniswap_v3_swap_decodes() {
        let pool = address!("4200000000000000000000000000000000000006");
        let sender = address!("1111111111111111111111111111111111111111");
        let recipient = address!("2222222222222222222222222222222222222222");

        let event = UniswapV3Swap {
            sender,
            recipient,
            amount0: alloy::primitives::I256::try_from(1_000_000_i64).unwrap(),
            amount1: alloy::primitives::I256::try_from(-2_000_000_i64).unwrap(),
            sqrtPriceX96: alloy::primitives::U160::from(79_228_162_514_264_337_593_543_950_336u128),
            liquidity: 123_456_789_u128,
            tick: alloy::primitives::aliases::I24::try_from(-1234i32).unwrap(),
        };
        let encoded = event.encode_log_data();
        let log = build_log(encoded.topics().to_vec(), encoded.data.clone(), pool);

        let market_event = decode_uniswap_v3_log(&log, 8453, now_us()).expect("should decode");
        match market_event.kind {
            EventKind::Swap(swap) => {
                assert_eq!(swap.amount0, 1_000_000);
                assert_eq!(swap.amount1, -2_000_000);
                assert_eq!(swap.dex, DexKind::UniswapV3);
                assert_eq!(swap.sender, Some(sender));
                assert_eq!(swap.recipient, Some(recipient));
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn malformed_log_is_rejected() {
        let pool = address!("4200000000000000000000000000000000000006");
        // Wrong topic0 (event signature) - decoder must reject, not guess.
        let bogus_topic = B256::repeat_byte(0x11);
        let log = build_log(vec![bogus_topic], Bytes::new(), pool);

        let result = decode_uniswap_v3_log(&log, 8453, now_us());
        assert!(result.is_err(), "malformed/mismatched log must be rejected");
    }

    #[test]
    fn log_missing_block_number_is_rejected() {
        let pool = address!("4200000000000000000000000000000000000006");
        let event = UniswapV3Swap {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            amount0: alloy::primitives::I256::ZERO,
            amount1: alloy::primitives::I256::ZERO,
            sqrtPriceX96: alloy::primitives::U160::ZERO,
            liquidity: 0,
            tick: alloy::primitives::aliases::I24::ZERO,
        };
        let encoded = event.encode_log_data();
        let mut log = build_log(encoded.topics().to_vec(), encoded.data.clone(), pool);
        log.block_number = None;

        let result = decode_uniswap_v3_log(&log, 8453, now_us());
        assert!(matches!(result, Err(EngineError::MalformedEvent(_))));
    }
}

'@
Set-Content -Path 'src\events\decoder.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/events/decoder.rs'

# ---- .env.example ----
$content = @'
# Copy this file to `.env` and fill in real values. NEVER commit `.env`.

# --- Base RPC / connectivity (required) ---
# Any Base-compatible HTTP RPC endpoint. Do not assume a specific provider.
BASE_RPC_URL=https://mainnet.base.org

# WebSocket endpoint. OPTIONAL. If set, block/log ingestion streams over
# WebSocket with reconnect + exponential backoff. If left empty/unset, the
# app automatically falls back to periodic HTTP polling of BASE_RPC_URL for
# the latest block (see HTTP_POLL_INTERVAL_SECS below) and keeps running -
# it does not exit. Log/event ingestion for configured pools requires
# WebSocket; HTTP fallback only polls latest-block state.
#
# mainnet.base.org's default WS endpoint may reject connections (HTTP 405)
# for some clients/providers - if that happens, just leave this blank to run
# in HTTP fallback mode until you have a working WS endpoint (e.g. from a
# paid RPC provider).
BASE_WS_URL=

# Base mainnet chain ID is 8453. Base Sepolia testnet is 84532.
BASE_CHAIN_ID=8453

# How often (seconds) the HTTP-fallback chain source polls for the latest
# block when BASE_WS_URL is not configured. Ignored in WebSocket mode.
# Safe development default: 5.
HTTP_POLL_INTERVAL_SECS=5

# --- Logging ---
# trace | debug | info | warn | error
LOG_LEVEL=info

# --- Execution mode (safety gate) ---
# DRY_RUN (default) | SIMULATION | LIVE
# Day 1: no mode has a working trade path. LIVE exists as a config value only.
EXECUTION_MODE=DRY_RUN

# --- DEX pool addresses (optional, verify before use) ---
# Leave unset unless you have verified the exact deployed pool address you
# want to watch. This program will never invent or guess an address.
AERODROME_POOL_ADDRESS=
UNISWAP_V3_POOL_ADDRESS=

# --- Day 2: automated pool discovery ---
# Factory addresses default to verified Base deployments (see README /
# config.rs comments for how each was confirmed) - only set these if you
# need to override.
# UNISWAP_V3_FACTORY_ADDRESS=0x33128a8fC17869897dcE68Ed026d694621f6FDfD
# AERODROME_FACTORY_ADDRESS=0x420DD381b31aEf6683db6B902084cB0FFECe40Da

# Aerodrome Slipstream (concentrated liquidity) factory. NO default - could
# not be independently confirmed from a primary source at time of writing.
# Verify it yourself (e.g. via Aerodrome's FactoryRegistry.poolFactories()
# at 0x5C3F18F06CC09CA1910767A34a20F771039E37C0 on Base, "Read Contract" on
# BaseScan) before setting this. Slipstream discovery stays disabled while
# this is unset.
AERODROME_SLIPSTREAM_FACTORY_ADDRESS=

# Max blocks per eth_getLogs call (chunked). Safe default below common
# public-RPC-provider limits.
LOG_POLL_MAX_BLOCK_RANGE=2000

# Where to start pool-discovery/swap scanning on first run (no checkpoint
# yet). "latest" = start from the current chain head, no history scan -
# the safe default. Set an explicit block number for a controlled backfill.
LOG_START_BLOCK=latest

# --- NEVER put a real private key in this file or in source. ---
# Day 1/2 have no signer and never read this variable, but it is documented
# here so future days don't accidentally introduce it insecurely.
# PRIVATE_KEY=

'@
Set-Content -Path '.env.example' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote .env.example'

# ---- README.md ----
$content = @'
# base-arb-engine

A research/MVP Base L2 arbitrage engine. Eventual goal: detect executable
price discrepancies between Aerodrome and Uniswap V3 on Base, size trades
optimally, simulate locally, and execute atomically via flash-loan-funded
Solidity executor. This repository is being built incrementally, day by day.

## Current scope: Day 1 + Day 2

Day 1 delivered the **market-data and state foundation**. Day 2 adds
**automated pool discovery and real protocol market-state indexing** on top
of it, still entirely over HTTP, still entirely read-only.

### Day 1

- Configurable Base RPC/WS connectivity (Alloy), with chain ID and latest
  block retrieval. WebSocket is **optional**: if `BASE_WS_URL` is configured,
  block/log ingestion streams over WebSocket with reconnect and exponential
  backoff; if not, the engine automatically falls back to periodic HTTP
  polling of `BASE_RPC_URL` for the latest block and keeps running
  indefinitely either way (see "HTTP fallback mode" below).
- A `ChainEventSource` trait so the event feed is swappable later (e.g. a
  lower-latency Base feed) without touching strategy/state code.
- A `DexAdapter` trait with Aerodrome (classic), Aerodrome Slipstream, and
  Uniswap V3 implementations:
  - Aerodrome classic: reserve-based state (`getReserves`, `stable`).
  - Uniswap V3 / Aerodrome Slipstream: **concentrated-liquidity** state
    (`slot0`, `liquidity`, `tickSpacing`) - not a fake two-reserve model.
    These share a `PoolKind::ConcentratedLiquidity` shape (same mechanics)
    but are hydrated via protocol-specific adapters, since their ABIs
    differ (Slipstream's `slot0()` has 6 fields, Uniswap V3's has 7 - see
    `dex::aerodrome_slipstream` module docs).
- Normalized data models (`Token`, `Pool`, `PoolState`, `BlockState`,
  `SwapEvent`, `MarketEvent`, `StateVersion`) using integer/token-native
  units throughout - no floating point anywhere in financial code paths.
- Deterministic event decoding with explicit rejection of malformed logs.
- Deterministic event deduplication keyed on `(chain_id, tx_hash,
  log_index)`.
- An in-memory `MarketState` store: versioned, deterministic updates,
  duplicate-safe, tracks per-pool freshness.
- Structured `tracing` logs including per-event ingestion latency.

### Day 2

- **HTTP `eth_getLogs` polling** (`chain::log_poller::HttpLogPoller`):
  chunked to stay under provider range limits (`LOG_POLL_MAX_BLOCK_RANGE`,
  default 2000 blocks), with automatic range-halving retry if a provider
  rejects a range as too large, and a checkpoint
  (`chain::log_poller::LogPollCheckpoint`) so polling cycles never rescan or
  skip blocks.
- **Automated pool discovery** (`dex::discovery`) from real factory
  `PoolCreated` events:
  - Uniswap V3 (`UniswapV3Factory.PoolCreated`) - standard, well-documented
    event shape.
  - Aerodrome classic (`PoolFactory.PoolCreated`) - event shape confirmed
    directly against the verified contract source on BaseScan.
  - Aerodrome Slipstream (`CLFactory.PoolCreated`) - event *emission*
    confirmed against real `CLFactory.sol` source, but the exact indexed/
    non-indexed parameter split is a well-justified inference, not fully
    confirmed - see `dex::discovery::aerodrome_slipstream` module docs and
    "Known limitations" below. **Disabled by default** - the factory
    address is not hardcoded anywhere in this codebase (unlike Uniswap V3
    and Aerodrome classic, whose addresses are verified defaults); set
    `AERODROME_SLIPSTREAM_FACTORY_ADDRESS` yourself after verifying it to
    enable it.
- **`PoolRegistry`** (`pools::registry`): lifecycle states (`Discovered` ->
  `Hydrating` -> `Active`/`Inactive`/`Blacklisted`), lookup by address, and
  an order-independent token-pair index (`(WETH,USDC)` and `(USDC,WETH)`
  both resolve to the same pools), optionally filtered by DEX.
- **Token metadata hydration** (`pools::token_cache::TokenMetadataCache`):
  on-chain ERC-20 `decimals`/`symbol`/`name` calls, cached per address so a
  token contract is never queried more than once. `decimals` is required;
  `symbol`/`name` are best-effort and never fail hydration.
- **Pool state hydration**: reuses each `DexAdapter::get_pool_state` to pull
  real on-chain reserves/`slot0`/liquidity for every newly discovered pool.
- **Pool eligibility** (`pools::models::PoolEligibility`): a coarse
  `Eligible`/`Ineligible`/`Unknown` signal computed from protocol
  verification, token metadata availability, supported pool type,
  liquidity presence, and state readability. Nothing downstream enforces
  this yet - it's informational, for the future opportunity engine to use.
- **Known-pool swap scanning**: once a pool is `Active`, its address is
  included in periodic `eth_getLogs` swap scans, decoded through the exact
  same `DexAdapter::decode_event` / `MarketState::apply_event` path Day 1's
  WebSocket log stream uses - transport-independent by construction.
- **Defensive reorg guard**: any log with `removed=true` is logged and
  skipped rather than applied as a real event. This is **not** full reorg
  reconciliation (no retroactive state rollback if a re-poll reveals a
  changed block) - see "Known limitations".
- New config: `UNISWAP_V3_FACTORY_ADDRESS`, `AERODROME_FACTORY_ADDRESS`,
  `AERODROME_SLIPSTREAM_FACTORY_ADDRESS`, `LOG_POLL_MAX_BLOCK_RANGE`,
  `LOG_START_BLOCK` (`latest` by default - no historical backfill unless
  you explicitly set a block number).

**Day 2 remains read-only.** See "Safety" below - nothing has changed
there.

## Architecture

```text
Base Chain
    |
    +-------------------------------+
    v                                v
Chain Event Source (blocks)     HttpLogPoller (Day 2: eth_getLogs,
    |                            chunked + checkpointed)
    v                                |
BlockState -> MarketState             +--> Discovery scan (factory logs)
                                       |        |
                                       |        v
                                       |    dex::discovery adapters
                                       |    (decode PoolCreated)
                                       |        |
                                       |        v
                                       |    PoolRegistry (insert, Discovered)
                                       |        |
                                       |        v
                                       |    TokenMetadataCache + DexAdapter
                                       |    ::get_pool_state (hydrate)
                                       |        |
                                       |        v
                                       |    PoolRegistry (Active) + eligibility
                                       |
                                       +--> Swap scan (known Active pools)
                                                |
                                                v
                                       Event Decoder (events::decoder -
                                       same code Day 1's WS path uses)
                                                |
                                                v
                                       Normalized Market State
                                       (market::state::MarketState -
                                        versioned, deduplicated,
                                        freshness-tracked)
                                                |
                                                v
                                       Opportunity Engine    <-- DAY 3+
                                                |
                                                v
                                       REVM Simulator         <-- LATER
                                                |
                                                v
                                       Transaction Builder     <-- LATER
                                                |
                                                v
                                       ArbExecutor.sol          <-- LATER
```

## Setup

Requirements:
- Rust (current stable toolchain; this crate targets modern Alloy, which
  requires a recent `rustc` - see "Known limitations" below if you hit an
  MSRV error).
- A Base RPC endpoint (HTTP) and, for streaming ingestion, a Base WebSocket
  endpoint. Any provider works - nothing is hard-coded.

```bash
cp .env.example .env
# edit .env: set BASE_RPC_URL and (optionally) BASE_WS_URL
```

## Run

```bash
cargo run
```

The engine runs indefinitely (until Ctrl+C) regardless of whether
`BASE_WS_URL` is set:

- **WebSocket mode** (`BASE_WS_URL` set): block and log ingestion stream
  over WebSocket, with reconnect and exponential backoff on disconnect. Log
  lines are tagged `source=websocket`.
- **HTTP fallback mode** (`BASE_WS_URL` unset or empty): block ingestion
  polls `BASE_RPC_URL` every `HTTP_POLL_INTERVAL_SECS` (default 5s) instead.
  This is the mode to use if your WebSocket endpoint isn't available - for
  example, `wss://mainnet.base.org` rejects some clients with HTTP 405.
  Log/event ingestion for configured pools requires WebSocket and is
  unavailable in this mode (only latest-block polling runs). Log lines are
  tagged `source=http_poll`.

To watch a specific, verified pool for swap events (WebSocket mode only),
set `AERODROME_POOL_ADDRESS` and/or `UNISWAP_V3_POOL_ADDRESS` in `.env`.
This is independent of Day 2's automated discovery, which runs regardless
of WebSocket mode (see above) and finds pools on its own.

Expected log lines once Day 2's pipeline is running (exact numbers/blocks
will differ):

```text
INFO ... source=http_poll scan=discovery range_from=... range_to=... "scanning for new pools"
INFO ... source=http_poll event=pool_discovered dex=uniswap_v3 pool=0x... "pool discovered"
INFO ... source=http_poll event=pool_hydrated dex=uniswap_v3 pool=0x... eligibility=Eligible "pool hydrated"
INFO ... source=http_poll scan=swaps range_from=... range_to=... pools_watched=N "scanning known pools for swap events"
INFO ... event=event_received dex=uniswap_v3 pool=0x... processing_latency_us=... "event_received"
```

New pool creation on Base isn't guaranteed within any given observation
window. To verify discovery works at all without waiting, set
`LOG_START_BLOCK` to a historical block you know contains a real
`PoolCreated` event for one of the configured factories (check BaseScan's
"Events" tab on the factory address) and restart - this is the "controlled
backfill" path, not automatic full-history scanning.

## Test

```bash
cargo test
```

## Safety

- **No private key is required or read anywhere in this codebase.**
- **There is no code path from this program to a submitted transaction.**
  `DexAdapter::build_swap_calldata` exists as a future boundary but always
  returns an error today.
- `ExecutionMode` (`DRY_RUN` / `SIMULATION` / `LIVE`) is defined for future
  days, but `ExecutionMode::can_execute_trades()` is hard-coded to `false`
  regardless of mode - `LIVE` has no working trade path in Day 1.
- No pool address is ever invented. If you don't set
  `AERODROME_POOL_ADDRESS` / `UNISWAP_V3_POOL_ADDRESS`, the program still
  runs (chain connectivity + generic ingestion), it just has nothing
  DEX-specific to watch.

## Not implemented yet

- Optimal trade sizing
- Full Uniswap V3 / Slipstream swap-simulation pricing math (tick-bitmap
  walking)
- Arbitrage opportunity detection
- REVM local simulation
- Balancer V2 flash loans
- `ArbExecutor.sol`
- Transaction signing
- Live trade execution
- Multi-hop graph arbitrage
- Full reorg reconciliation (Day 2 only skips `removed=true` logs
  defensively - it does not retroactively roll back state)
- Aerodrome Slipstream fee resolution (routes through
  `CLFactory.getSwapFee(pool)`, not hydrated - `fee_tier` is a `0`
  placeholder for Slipstream pools, never a real value)

## Known limitations / blockers

This was authored in a sandboxed build environment pinned to an old
`rustc` (1.75) that cannot resolve the modern crate graph at all (`edition2024`
requirements from transitive deps), so I could not run `cargo check` /
`cargo test` locally end-to-end for Day 2 either. Day 1 + the WebSocket-
optional change were fully verified on the operator's own machine across
several iterations; Day 2 has not yet been. Everything here was written by
diffing against real, current source (`alloy-rs/core` v1.6.0, `alloy-rs/
alloy` v2.4.1, the verified `PoolFactory`/`CLFactory`/`CLPool` contract
source on BaseScan and GitHub) rather than guessing, but "diffed against
source" is not the same as "compiled" - run `cargo check` / `cargo test` /
`cargo run` in your own environment and report back anything that doesn't
match.

Specific things flagged as uncertain rather than confirmed, called out
inline in the relevant module docs too:

- **Aerodrome Slipstream (`CLFactory`) factory address on Base** could not
  be independently confirmed (BaseScan's "SlipStream Pool Factory" label
  resolves to a `CLPool` implementation contract's ABI, not the factory's).
  No default is hardcoded; Slipstream discovery is disabled unless you set
  `AERODROME_SLIPSTREAM_FACTORY_ADDRESS` yourself. Aerodrome's
  `FactoryRegistry` (`0x5C3F18F06CC09CA1910767A34a20F771039E37C0` on Base,
  verified) is the documented way to look up the live factory address if
  you want to enable this.
- **Aerodrome Slipstream `PoolCreated` indexed/non-indexed parameter
  split** is inferred from the confirmed `emit PoolCreated(token0, token1,
  tickSpacing, pool)` call in `CLFactory.sol` plus the pattern both
  Uniswap V3's and Aerodrome classic's analogous events follow (first
  three logical fields indexed) - not confirmed against `ICLFactory.sol`'s
  actual interface declaration.
- **Aerodrome Slipstream `Swap` event shape** is assumed identical to
  Uniswap V3's (`sender, recipient, amount0, amount1, sqrtPriceX96,
  liquidity, tick`), based on Slipstream's documented lineage ("adapted
  from Uniswap V3's core contracts") - not independently confirmed from
  `CLPool`'s full event declarations.
- The HTTP `eth_getLogs` retry/range-reduction logic (`reduce_range_on_failure`,
  chunking) is tested as pure logic (no network in this sandbox) - the
  actual RPC-calling code path (`HttpLogPoller::fetch_range`) is
  unverified against a real provider's oversized-range error response.

'@
Set-Content -Path 'README.md' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote README.md'

Write-Host 'Done. Now run: cargo check'
Write-Host 'IMPORTANT: also add AERODROME_SLIPSTREAM_FACTORY_ADDRESS= and LOG_POLL_MAX_BLOCK_RANGE=2000 and LOG_START_BLOCK=latest to your own .env if not already present (see updated .env.example).'