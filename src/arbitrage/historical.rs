//! M6A: historical pool-state input, pinned to an explicit Base block.
//!
//! The existing RPC-based reader
//! (`dex::uniswap_v3::UniswapV3Adapter::get_pool_state_and_ticks_at_block`)
//! already knows how to fetch a `PoolState` (and, for V3, a `HydratedTicks`)
//! pinned to one block - this module does not duplicate that. What it adds
//! is a small, deterministic, network-free VALIDATION step before a
//! `PoolState` is handed to
//! `arbitrage::quote::quote_aerodrome_leg`/`quote_uniswap_v3_leg`. Neither
//! bridge function is modified; both already take exactly `&PoolState`
//! (and `&HydratedTicks`), which is exactly what these types expose.
//!
//! **What the constructors actually prove, and what they do not:**
//! [`HistoricalAerodromeInput::new`] / [`HistoricalUniswapV3Input::new`]
//! check that `pool_state.freshness.last_updated_block` equals the `block`
//! the caller supplies, and that `pool_state.pool.dex`/`pool_state.pool.kind`
//! are the ones this type claims to represent. That is a
//! self-consistency check on the `PoolState` value itself. It is NOT, and
//! cannot be, independent proof that the pool's actual data (reserves,
//! price, liquidity, ticks) was really read from the chain at that block -
//! `last_updated_block` is just a `u64` field; nothing here can verify how
//! it was set upstream, and a caller could construct a `PoolState` with
//! any block number and any data attached to it. Whatever real guarantee
//! exists comes entirely from how `pool_state` was produced before
//! reaching this module - e.g.
//! `get_pool_state_and_ticks_at_block(..., Some(block))` pins its own RPC
//! reads to that block. This module's job is narrower: given a
//! `PoolState` that claims a block and a DEX, confirm that claim is
//! internally consistent with what the caller is asking for, and reject
//! it when it is not.
//!
//! **Known, unavoidable limitation (see [`HistoricalUniswapV3Input`]):**
//! `pricing::v3_quote::HydratedTicks` carries no block number of its own.
//! This module can check a `PoolState`'s recorded block; it has no data to
//! check a `HydratedTicks` value against at all, so it cannot verify that
//! one passed alongside a `PoolState` truly came from the same block. That
//! guarantee only exists if both were obtained from one call to
//! `get_pool_state_and_ticks_at_block`. This is stated explicitly rather
//! than silently assumed.

use crate::error::{EngineError, EngineResult};
use crate::market::models::{DexKind, PoolKind, PoolState};
use crate::pricing::v3_quote::HydratedTicks;

/// Aerodrome Classic volatile pool state whose `pool.dex`, `pool.kind`,
/// and `freshness.last_updated_block` have been checked, once at
/// construction, against what the caller requires - see the module docs
/// for exactly what that does and does not guarantee. The only way to
/// obtain one is [`Self::new`] - there is no public field to bypass the
/// check.
#[derive(Debug, Clone)]
pub struct HistoricalAerodromeInput {
    block: u64,
    pool_state: PoolState,
}

impl HistoricalAerodromeInput {
    /// Rejects:
    /// - `pool_state.pool.dex != DexKind::Aerodrome` ->
    ///   [`EngineError::NotImplemented`]
    /// - `pool_state.pool.kind` is not `PoolKind::Aerodrome { stable: false, .. }`
    ///   (including a *stable*-curve Aerodrome pool, out of scope exactly
    ///   like `quote_pool_exact_input` itself) -> [`EngineError::NotImplemented`]
    /// - `pool_state.freshness.last_updated_block != block` ->
    ///   [`EngineError::State`]
    pub fn new(block: u64, pool_state: PoolState) -> EngineResult<Self> {
        if pool_state.pool.dex != DexKind::Aerodrome {
            return Err(EngineError::NotImplemented(format!(
                "HistoricalAerodromeInput::new: pool {} has dex={:?}, not DexKind::Aerodrome",
                pool_state.pool.address, pool_state.pool.dex
            )));
        }
        match &pool_state.pool.kind {
            PoolKind::Aerodrome { stable: true, .. } => {
                return Err(EngineError::NotImplemented(format!(
                    "HistoricalAerodromeInput::new: pool {} is a stable-curve Aerodrome pool - \
                     only volatile (x*y=k) pools are supported, matching \
                     quote_pool_exact_input's own scope",
                    pool_state.pool.address
                )));
            }
            PoolKind::Aerodrome { stable: false, .. } => {}
            PoolKind::ConcentratedLiquidity { .. } => {
                return Err(EngineError::NotImplemented(format!(
                    "HistoricalAerodromeInput::new: pool {} is not a volatile Aerodrome pool \
                     (kind is PoolKind::ConcentratedLiquidity, not PoolKind::Aerodrome)",
                    pool_state.pool.address
                )));
            }
        }
        if pool_state.freshness.last_updated_block != block {
            return Err(EngineError::State(format!(
                "HistoricalAerodromeInput::new: pool {} state is pinned to block {}, not the \
                 requested historical block {block}",
                pool_state.pool.address, pool_state.freshness.last_updated_block
            )));
        }
        Ok(Self { block, pool_state })
    }

    pub fn block(&self) -> u64 {
        self.block
    }

    /// Directly usable as `quote_aerodrome_leg`'s `pool_state` argument.
    pub fn pool_state(&self) -> &PoolState {
        &self.pool_state
    }
}

/// Uniswap V3 pool state + hydrated ticks whose `pool.dex`, `pool.kind`,
/// and `freshness.last_updated_block` have been checked, once at
/// construction, against what the caller requires - with one caveat that
/// cannot be engineered away, see below and the module docs.
///
/// `HydratedTicks` has no block field (see `pricing::v3_quote` - it is
/// deliberately minimal: initialized ticks plus the hydrated range, see
/// its own "Coupling safety" docs). That means [`Self::new`] can check
/// `pool_state`'s own recorded block; it has no data at all to check
/// `hydrated` against, so it cannot verify `hydrated` was actually read at
/// that same block. The only real guarantee comes from obtaining both
/// values from a single call to
/// `UniswapV3Adapter::get_pool_state_and_ticks_at_block(..., Some(block))`,
/// which pins every read (`slot0`, `liquidity`, `fee`, `tickSpacing`,
/// `tickBitmap`, `ticks`) to the same `BlockId`.
/// [`Self::from_pinned_read`] takes exactly that function's return shape
/// to make the correct call the natural one - but even it cannot detect a
/// caller who assembles the tuple from two unrelated calls.
#[derive(Debug, Clone)]
pub struct HistoricalUniswapV3Input {
    block: u64,
    pool_state: PoolState,
    hydrated: HydratedTicks,
}

impl HistoricalUniswapV3Input {
    /// Rejects:
    /// - `pool_state.pool.dex != DexKind::UniswapV3` -> [`EngineError::Dex`].
    ///   Checked separately from `pool.kind` below: Aerodrome Slipstream
    ///   pools also use `PoolKind::ConcentratedLiquidity` (same
    ///   representation, different protocol - see `DexKind::AerodromeSlipstream`'s
    ///   own docs), so the `kind` check alone cannot tell a genuine
    ///   Uniswap V3 pool from a Slipstream one.
    /// - `pool_state.pool.kind` is not `PoolKind::ConcentratedLiquidity` ->
    ///   [`EngineError::Dex`] (matching the existing convention
    ///   `HydratedV3State::from_pool_state` itself already uses for this)
    /// - `pool_state.freshness.last_updated_block != block` ->
    ///   [`EngineError::State`]
    pub fn new(block: u64, pool_state: PoolState, hydrated: HydratedTicks) -> EngineResult<Self> {
        if pool_state.pool.dex != DexKind::UniswapV3 {
            return Err(EngineError::Dex {
                dex: pool_state.pool.dex.name().into(),
                reason: format!(
                    "HistoricalUniswapV3Input::new: pool {} has dex={:?}, not DexKind::UniswapV3 \
                     (Aerodrome Slipstream also uses PoolKind::ConcentratedLiquidity, so the \
                     kind alone does not prove this is a Uniswap V3 pool)",
                    pool_state.pool.address, pool_state.pool.dex
                ),
            });
        }
        if !matches!(pool_state.pool.kind, PoolKind::ConcentratedLiquidity { .. }) {
            return Err(EngineError::Dex {
                dex: pool_state.pool.dex.name().into(),
                reason: format!(
                    "HistoricalUniswapV3Input::new: pool {} is not a ConcentratedLiquidity pool",
                    pool_state.pool.address
                ),
            });
        }
        if pool_state.freshness.last_updated_block != block {
            return Err(EngineError::State(format!(
                "HistoricalUniswapV3Input::new: pool {} state is pinned to block {}, not the \
                 requested historical block {block}",
                pool_state.pool.address, pool_state.freshness.last_updated_block
            )));
        }
        Ok(Self {
            block,
            pool_state,
            hydrated,
        })
    }

    /// Convenience constructor matching
    /// `get_pool_state_and_ticks_at_block`'s own `(PoolState,
    /// HydratedTicks)` return shape exactly - the one call in this
    /// codebase that genuinely reads both pinned to the same block.
    /// Performs the identical validation as [`Self::new`]; exists only to
    /// make the correct call shape the natural one, not to add a new
    /// guarantee.
    pub fn from_pinned_read(
        block: u64,
        pinned_read: (PoolState, HydratedTicks),
    ) -> EngineResult<Self> {
        let (pool_state, hydrated) = pinned_read;
        Self::new(block, pool_state, hydrated)
    }

    pub fn block(&self) -> u64 {
        self.block
    }

    /// Directly usable as `quote_uniswap_v3_leg`'s `pool_state` argument.
    pub fn pool_state(&self) -> &PoolState {
        &self.pool_state
    }

    /// Directly usable as `quote_uniswap_v3_leg`'s `hydrated` argument.
    pub fn hydrated(&self) -> &HydratedTicks {
        &self.hydrated
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrage::quote::{quote_aerodrome_leg, quote_uniswap_v3_leg};
    use crate::market::models::{DexKind, Pool, Token};
    use alloy::primitives::{address, U256};
    use std::collections::BTreeMap;

    // Real Base WETH/USDC pools + state at block 26000000 - the same
    // golden fixtures already validated in pricing::aerodrome_volatile,
    // pricing::v3_quote, and arbitrage::quote's own tests. Reused here,
    // not re-derived.
    const FIXTURE_BLOCK: u64 = 26_000_000;

    fn weth() -> alloy::primitives::Address {
        address!("4200000000000000000000000000000000000006")
    }
    fn usdc() -> alloy::primitives::Address {
        address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913")
    }

    fn aerodrome_fixture_pool_state(block: u64) -> PoolState {
        let pool = Pool {
            address: address!("cDAC0d6c6C59727a65F871236188350531885C43"),
            dex: DexKind::Aerodrome,
            token0: Token {
                address: weth(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: usdc(),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::Aerodrome {
                reserve0: U256::from_str_radix("3607642796485591444113", 10).unwrap(),
                reserve1: U256::from(9_965_914_277_780u64),
                stable: false,
                fee_bps: Some(U256::from(30u64)),
            },
        };
        PoolState::new(pool, block, None)
    }

    fn v3_fixture_pool_state_and_ticks(block: u64) -> (PoolState, HydratedTicks) {
        let pool = Pool {
            address: address!("d0b53D9277642d899DF5C87A3966A349A798F224"),
            dex: DexKind::UniswapV3,
            token0: Token {
                address: weth(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: usdc(),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::ConcentratedLiquidity {
                fee_tier: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::from_str_radix("4166855781027983759429743", 10).unwrap(),
                current_tick: -197_069,
                liquidity: 2_241_227_707_498_366_949u128,
                initialized_ticks: Default::default(),
            },
        };
        let pool_state = PoolState::new(pool, block, None);

        let mut ticks = BTreeMap::new();
        ticks.insert(-197_070, -1_776_514_264_016_992i128);
        let hydrated = HydratedTicks::for_test(ticks, -199_680, -192_010);

        (pool_state, hydrated)
    }

    #[test]
    fn aerodrome_input_accepts_matching_block() {
        let pool_state = aerodrome_fixture_pool_state(FIXTURE_BLOCK);
        let input = HistoricalAerodromeInput::new(FIXTURE_BLOCK, pool_state).unwrap();
        assert_eq!(input.block(), FIXTURE_BLOCK);
    }

    #[test]
    fn aerodrome_input_rejects_block_mismatch() {
        let pool_state = aerodrome_fixture_pool_state(FIXTURE_BLOCK);
        let err = HistoricalAerodromeInput::new(FIXTURE_BLOCK + 1, pool_state).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// `pool.dex` genuinely is `DexKind::Aerodrome` here - only `pool.kind`
    /// is wrong (`ConcentratedLiquidity`, not `Aerodrome`). Exercises the
    /// `kind` check specifically, not the dex check (see
    /// `aerodrome_input_rejects_mismatched_dex` for that one).
    #[test]
    fn aerodrome_input_rejects_wrong_pool_kind() {
        let pool = Pool {
            address: address!("d0b53D9277642d899DF5C87A3966A349A798F224"),
            dex: DexKind::Aerodrome,
            token0: Token {
                address: weth(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: usdc(),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::ConcentratedLiquidity {
                fee_tier: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::from(1u128) << 96usize,
                current_tick: 0,
                liquidity: 1_000_000,
                initialized_ticks: Default::default(),
            },
        };
        let pool_state = PoolState::new(pool, FIXTURE_BLOCK, None);

        let err = HistoricalAerodromeInput::new(FIXTURE_BLOCK, pool_state).unwrap_err();
        assert!(matches!(err, EngineError::NotImplemented(_)), "got {err:?}");
    }

    #[test]
    fn aerodrome_input_rejects_stable_pool() {
        let pool = Pool {
            address: address!("cDAC0d6c6C59727a65F871236188350531885C43"),
            dex: DexKind::Aerodrome,
            token0: Token {
                address: weth(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: usdc(),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::Aerodrome {
                reserve0: U256::from(1_000_000u64),
                reserve1: U256::from(2_000_000u64),
                stable: true, // the rejected case
                fee_bps: Some(U256::from(4u64)),
            },
        };
        let pool_state = PoolState::new(pool, FIXTURE_BLOCK, None);

        let err = HistoricalAerodromeInput::new(FIXTURE_BLOCK, pool_state).unwrap_err();
        assert!(matches!(err, EngineError::NotImplemented(_)), "got {err:?}");
    }

    /// `pool.kind` genuinely is `PoolKind::Aerodrome { stable: false, .. }`
    /// here - only `pool.dex` is wrong. Proves the dex check is real and
    /// not redundant with the kind check.
    #[test]
    fn aerodrome_input_rejects_mismatched_dex() {
        let pool = Pool {
            address: address!("cDAC0d6c6C59727a65F871236188350531885C43"),
            dex: DexKind::UniswapV3, // mismatched: kind below says Aerodrome
            token0: Token {
                address: weth(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: usdc(),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::Aerodrome {
                reserve0: U256::from(1_000_000u64),
                reserve1: U256::from(2_000_000u64),
                stable: false,
                fee_bps: Some(U256::from(30u64)),
            },
        };
        let pool_state = PoolState::new(pool, FIXTURE_BLOCK, None);

        let err = HistoricalAerodromeInput::new(FIXTURE_BLOCK, pool_state).unwrap_err();
        assert!(matches!(err, EngineError::NotImplemented(_)), "got {err:?}");
    }

    #[test]
    fn v3_input_accepts_matching_block_via_new() {
        let (pool_state, hydrated) = v3_fixture_pool_state_and_ticks(FIXTURE_BLOCK);
        let input = HistoricalUniswapV3Input::new(FIXTURE_BLOCK, pool_state, hydrated).unwrap();
        assert_eq!(input.block(), FIXTURE_BLOCK);
    }

    #[test]
    fn v3_input_accepts_matching_block_via_from_pinned_read() {
        let pinned_read = v3_fixture_pool_state_and_ticks(FIXTURE_BLOCK);
        let input = HistoricalUniswapV3Input::from_pinned_read(FIXTURE_BLOCK, pinned_read).unwrap();
        assert_eq!(input.block(), FIXTURE_BLOCK);
    }

    #[test]
    fn v3_input_rejects_block_mismatch() {
        let (pool_state, hydrated) = v3_fixture_pool_state_and_ticks(FIXTURE_BLOCK);
        let err =
            HistoricalUniswapV3Input::new(FIXTURE_BLOCK + 1, pool_state, hydrated).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// `pool.dex` genuinely is `DexKind::UniswapV3` here - only
    /// `pool.kind` is wrong (`Aerodrome`, not `ConcentratedLiquidity`).
    /// Exercises the `kind` check specifically, not the dex check (see
    /// `v3_input_rejects_aerodrome_slipstream_pool` for that one).
    #[test]
    fn v3_input_rejects_wrong_pool_kind() {
        let pool = Pool {
            address: address!("cDAC0d6c6C59727a65F871236188350531885C43"),
            dex: DexKind::UniswapV3,
            token0: Token {
                address: weth(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: usdc(),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::Aerodrome {
                reserve0: U256::from(1_000_000u64),
                reserve1: U256::from(2_000_000u64),
                stable: false,
                fee_bps: Some(U256::from(30u64)),
            },
        };
        let pool_state = PoolState::new(pool, FIXTURE_BLOCK, None);
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);

        let err = HistoricalUniswapV3Input::new(FIXTURE_BLOCK, pool_state, hydrated).unwrap_err();
        assert!(matches!(err, EngineError::Dex { .. }), "got {err:?}");
    }

    /// `pool.kind` genuinely is `PoolKind::ConcentratedLiquidity` here -
    /// Aerodrome Slipstream shares that exact representation with Uniswap
    /// V3. Only `pool.dex` (AerodromeSlipstream, not UniswapV3) identifies
    /// it as the wrong protocol - proves the dex check is load-bearing,
    /// not redundant with the kind check.
    #[test]
    fn v3_input_rejects_aerodrome_slipstream_pool() {
        let pool = Pool {
            address: address!("0000000000000000000000000000000000000006"),
            dex: DexKind::AerodromeSlipstream,
            token0: Token {
                address: weth(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: usdc(),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::ConcentratedLiquidity {
                fee_tier: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::from(1u128) << 96usize,
                current_tick: 0,
                liquidity: 1_000_000,
                initialized_ticks: Default::default(),
            },
        };
        let pool_state = PoolState::new(pool, FIXTURE_BLOCK, None);
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);

        let err = HistoricalUniswapV3Input::new(FIXTURE_BLOCK, pool_state, hydrated).unwrap_err();
        assert!(matches!(err, EngineError::Dex { .. }), "got {err:?}");
    }

    /// Proves requirement 6: a validated `HistoricalAerodromeInput` /
    /// `HistoricalUniswapV3Input` is directly usable by the EXISTING
    /// M5A quote bridge with no changes to either, and no pricing math
    /// re-implemented here.
    #[test]
    fn validated_inputs_feed_the_existing_quote_bridge_unchanged() {
        let aerodrome_input = HistoricalAerodromeInput::new(
            FIXTURE_BLOCK,
            aerodrome_fixture_pool_state(FIXTURE_BLOCK),
        )
        .unwrap();
        let leg1 = quote_aerodrome_leg(
            aerodrome_input.pool_state(),
            U256::from(1_000_000_000_000_000_000u128),
            true,
        )
        .unwrap();
        assert_eq!(leg1.amount_out, U256::from(2_753_396_596u64));

        let v3_input = HistoricalUniswapV3Input::from_pinned_read(
            FIXTURE_BLOCK,
            v3_fixture_pool_state_and_ticks(FIXTURE_BLOCK),
        )
        .unwrap();
        let leg2 = quote_uniswap_v3_leg(
            v3_input.pool_state(),
            v3_input.hydrated(),
            leg1.amount_out,
            false,
        )
        .unwrap();
        assert_eq!(leg2.amount_in, leg1.amount_out);
        assert_eq!(leg2.block, FIXTURE_BLOCK);
    }
}
