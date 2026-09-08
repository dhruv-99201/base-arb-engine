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
    /// `fee` is `None` for factories whose `PoolCreated` event doesn't
    /// carry a fee (e.g. Aerodrome Slipstream's `CLFactory` - fee there
    /// routes through a separate `getSwapFee(pool)` call, not this event).
    ConcentratedLiquidity { tick_spacing: i32, fee: Option<u32> },
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
