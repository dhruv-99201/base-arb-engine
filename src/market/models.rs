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
