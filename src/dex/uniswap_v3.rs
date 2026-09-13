//! Uniswap V3 adapter.
//!
//! Uniswap V3 is concentrated liquidity, NOT a two-reserve constant-product
//! pool. This adapter reads `slot0` (sqrtPriceX96, tick) and `liquidity`
//! directly - it does not synthesize fake reserves.
//!
//! Day 3 adds real tick-bitmap hydration
//! (`hydrate_initialized_ticks`, via `tickBitmap()`/`ticks()`) so
//! `pricing::v3_quote::quote_exact_input` has REAL data to walk instead of
//! an empty `initialized_ticks` map. This is opt-in and NOT called from
//! `get_pool_state` (which stays exactly as before, still Day 1/2 shaped) -
//! wiring it into the default poll path is out of scope for this update.
//!
//! **Pinned-block honesty.** `get_pool_state` and `hydrate_initialized_ticks`
//! each make several SEPARATE RPC round-trips (`slot0`, `liquidity`, `fee`,
//! `tickSpacing`, then one `tickBitmap`/`ticks` pair per set bit). Every one
//! of those calls defaults to "latest" with no block pinning, so nothing
//! about them is an atomic snapshot: the chain can advance between any two
//! of those calls, and a caller combining their results is combining
//! reads from what may be different blocks. Nothing in this file claims
//! otherwise. [`get_pool_state_and_ticks_at_block`] is the interface for
//! requesting all six reads (`slot0`, `liquidity`, `fee`, `tickSpacing`,
//! `tickBitmap`, `ticks`) as of one explicit, caller-supplied block number -
//! see its docs for exactly what is and is not implemented.
//!
//! **Unverified in this environment; partially confirmed by the developer.**
//! This sandbox has no live Base RPC access and no working Rust toolchain
//! (the `edition2024` blocker - see the Day 3 status report), so nothing
//! in this file has been compiled or run against a real pool from here.
//! The developer's own local `cargo check` did report one real type error
//! in an earlier version of this file: `tickBitmap(int16)`'s generated
//! sol! binding takes a native Rust `i16`, not the
//! `alloy::primitives::aliases::I16` wrapper this file originally used
//! (unlike `int24`, which has no native Rust type and does need the
//! `aliases::I24` wrapper - confirmed elsewhere in this codebase, see
//! `events::decoder`, `dex::discovery::aerodrome_slipstream`,
//! `dex::discovery::uniswap_v3`). That is now fixed here via a checked
//! `word_pos.try_into::<i16>()`. The developer reported that error as the
//! ONLY one `cargo check` found, which is the closest thing to
//! confirmation this file has - but that was reported, not something
//! verified by this environment, and it does not amount to a claim that
//! `cargo check`/`cargo test` now pass. Only the pure, RPC-free
//! bit-extraction helper (`set_bit_positions`) is covered by tests that
//! have been independently hand-verified.

use crate::dex::traits::DexAdapter;
use crate::error::{EngineError, EngineResult};
use crate::events::decoder;
use crate::events::model::MarketEvent;
use crate::market::models::{Pool, PoolKind, PoolState};
use alloy::primitives::U256;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use async_trait::async_trait;
use std::collections::BTreeMap;

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
        /// Real `tickBitmap(int16)` - one packed 256-bit word of
        /// initialized-tick flags per `wordPosition`. Day 3 addition.
        function tickBitmap(int16 wordPosition) external view returns (uint256);
        /// Real `ticks(int24)` - per-tick state, of which only
        /// `liquidityNet` and `initialized` are used here. Day 3 addition.
        function ticks(int24 tick) external view returns (
            uint128 liquidityGross,
            int128 liquidityNet,
            uint256 feeGrowthOutside0X128,
            uint256 feeGrowthOutside1X128,
            int56 tickCumulativeOutside,
            uint160 secondsPerLiquidityOutsideX128,
            uint32 secondsOutside,
            bool initialized
        );
    }
}

/// Real, hydrated tick data for a bounded range of tick-bitmap words around
/// a pool's current tick - the confirmed-scanned range
/// `pricing::v3_quote::quote_exact_input` requires to safely reject
/// incomplete state (see that module's docs on `hydrated_tick_lo`/`hi`).
/// Every entry in `initialized_ticks` comes from a real `ticks()` call on a
/// bit this adapter actually observed set in a real `tickBitmap()` read -
/// never fabricated or interpolated.
#[derive(Debug, Clone)]
pub struct HydratedTicks {
    pub initialized_ticks: BTreeMap<i32, i128>,
    /// Inclusive lower bound of the tick range actually confirmed hydrated.
    pub hydrated_tick_lo: i32,
    /// Inclusive upper bound of the tick range actually confirmed hydrated.
    pub hydrated_tick_hi: i32,
}

/// Which bit positions (0..256) are set in a `tickBitmap()` word, in
/// ascending order. Pure and RPC-free - the only part of tick hydration
/// this sandbox can actually exercise with tests (no live Base RPC access
/// here). Built only from operations already used elsewhere in this
/// codebase (`&`, `>>=`, `U256::from`, `is_zero()` - see
/// `pricing::tick_math`/`pricing::tick_bitmap`), not any unconfirmed API.
fn set_bit_positions(word: U256) -> Vec<u8> {
    let mut positions = Vec::new();
    let mut remaining = word;
    let mut bit: u16 = 0;
    while !remaining.is_zero() && bit < 256 {
        if (remaining & U256::from(1u8)) == U256::from(1u8) {
            positions.push(bit as u8);
        }
        remaining >>= 1usize;
        bit += 1;
    }
    positions
}

pub struct UniswapV3Adapter;

impl UniswapV3Adapter {
    pub fn new() -> Self {
        UniswapV3Adapter
    }

    /// Hydrate REAL initialized-tick data for `word_radius` tick-bitmap
    /// words on either side of `current_tick`, via actual
    /// `tickBitmap()`/`ticks()` calls against `pool.address`. **Not a
    /// pinned snapshot** - see the module docs. Each `tickBitmap`/`ticks`
    /// pair is its own RPC round-trip against whatever "latest" resolves to
    /// at call time; use [`get_pool_state_and_ticks_at_block`] if callers
    /// need every read to agree on one block (currently unimplemented for
    /// `Some(block)` - see that function's docs).
    pub async fn hydrate_initialized_ticks(
        &self,
        rpc_url: &str,
        pool: &Pool,
        tick_spacing: i32,
        current_tick: i32,
        word_radius: i32,
    ) -> EngineResult<HydratedTicks> {
        if tick_spacing <= 0 {
            return Err(EngineError::Config(format!(
                "hydrate_initialized_ticks: tick_spacing must be positive, got {tick_spacing}"
            )));
        }
        if word_radius < 0 {
            return Err(EngineError::Config(format!(
                "hydrate_initialized_ticks: word_radius must be >= 0, got {word_radius}"
            )));
        }

        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let contract = IUniswapV3Pool::new(pool.address, provider);

        // Same compress()/word-position arithmetic as
        // `pricing::tick_bitmap`'s private helpers, inlined here since
        // that module doesn't export them.
        let compressed_center = current_tick.div_euclid(tick_spacing);
        let word_pos_center = compressed_center >> 8;

        let mut initialized_ticks = BTreeMap::new();

        for offset in -word_radius..=word_radius {
            let word_pos = word_pos_center + offset;
            let word_pos_i16: i16 = word_pos.try_into().map_err(|_| {
                EngineError::Arithmetic(format!(
                    "hydrate_initialized_ticks: word position {word_pos} does not fit in int16"
                ))
            })?;

            let bitmap = contract
                .tickBitmap(word_pos_i16)
                .call()
                .await
                .map_err(|e| EngineError::Dex {
                    dex: self.name().into(),
                    reason: format!("tickBitmap({word_pos}) failed: {e}"),
                })?;

            for bit in set_bit_positions(bitmap) {
                let compressed = word_pos * 256 + bit as i32;
                let tick = compressed * tick_spacing;

                let tick_arg = alloy::primitives::aliases::I24::try_from(tick).map_err(|_| {
                    EngineError::Arithmetic(format!(
                        "hydrate_initialized_ticks: tick {tick} does not fit in int24"
                    ))
                })?;

                let tick_info =
                    contract.ticks(tick_arg).call().await.map_err(|e| EngineError::Dex {
                        dex: self.name().into(),
                        reason: format!("ticks({tick}) failed: {e}"),
                    })?;

                if tick_info.initialized {
                    initialized_ticks.insert(tick, tick_info.liquidityNet);
                }
            }
        }

        let hydrated_tick_lo = (word_pos_center - word_radius) * 256 * tick_spacing;
        let hydrated_tick_hi = ((word_pos_center + word_radius) * 256 + 255) * tick_spacing;

        Ok(HydratedTicks {
            initialized_ticks,
            hydrated_tick_lo,
            hydrated_tick_hi,
        })
    }

    /// The pinned-state interface: conceptually, read `slot0`, `liquidity`,
    /// `fee`, `tickSpacing`, `tickBitmap`, and `ticks` all as of the SAME
    /// block, so the resulting `PoolState` + `HydratedTicks` form one
    /// internally-consistent snapshot rather than up to `3 + (2 *
    /// set-bit-count)` independent "latest" reads that could each land on
    /// a different block under concurrent chain activity.
    ///
    /// `at_block: None` runs the existing best-effort behavior
    /// (`get_pool_state` then `hydrate_initialized_ticks`, both against
    /// whatever "latest" resolves to at each individual call) - this is
    /// explicitly NOT a pinned snapshot, and this function does not pretend
    /// otherwise.
    ///
    /// `at_block: Some(block_number)` is the actual pinned path this
    /// interface exists for, and it is **deliberately unimplemented** in
    /// this patch: pinning every one of alloy's generated contract-call
    /// builders to a specific historical block requires a `.block(...)`
    /// (or equivalent `BlockId`) call per RPC method, and this codebase has
    /// no prior, confirmed usage of that API in this pinned alloy version
    /// (`alloy = "2.4.2"`, per `Cargo.lock`) to copy from - every other
    /// call site in this repo reads whatever "latest" resolves to. Rather
    /// than guess at that syntax and risk silently producing calls that
    /// don't actually pin anything (which would be strictly worse than
    /// this explicit rejection - it would look pinned without being
    /// pinned), this returns `EngineError::NotImplemented` naming the
    /// requested block, so a caller can never mistake "not implemented"
    /// for "silently ignored".
    pub async fn get_pool_state_and_ticks_at_block(
        &self,
        rpc_url: &str,
        pool: &Pool,
        word_radius: i32,
        at_block: Option<u64>,
    ) -> EngineResult<(PoolState, HydratedTicks)> {
        if let Some(block_number) = at_block {
            return Err(EngineError::NotImplemented(format!(
                "get_pool_state_and_ticks_at_block: pinning reads to block {block_number} is \
                 not implemented - slot0/liquidity/fee/tickSpacing/tickBitmap/ticks would each \
                 need a confirmed block-pinning call this codebase has no precedent for (no \
                 working Rust toolchain in this environment to verify one); only at_block=None \
                 (best-effort latest, NOT a pinned snapshot - see this function's docs) is \
                 functional today"
            )));
        }

        let pool_state = self.get_pool_state(rpc_url, pool).await?;
        let (tick_spacing, current_tick) = match &pool_state.pool.kind {
            PoolKind::ConcentratedLiquidity {
                tick_spacing,
                current_tick,
                ..
            } => (*tick_spacing, *current_tick),
            PoolKind::Aerodrome { .. } => {
                return Err(EngineError::Dex {
                    dex: self.name().into(),
                    reason: "get_pool_state_and_ticks_at_block: pool.kind resolved to Aerodrome, \
                              not ConcentratedLiquidity - wrong adapter for this pool"
                        .into(),
                })
            }
        };

        let hydrated_ticks = self
            .hydrate_initialized_ticks(rpc_url, &pool_state.pool, tick_spacing, current_tick, word_radius)
            .await?;

        Ok((pool_state, hydrated_ticks))
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
        //
        // NOTE: slot0/liquidity/fee/tickSpacing above are four SEPARATE
        // RPC calls, each against "latest" at the moment it's made - see
        // the module docs on pinned-block honesty. This is unchanged from
        // the Day 1/2 behavior; it is not claimed to be atomic.
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

    #[test]
    fn set_bit_positions_empty_word_gives_empty_vec() {
        assert_eq!(set_bit_positions(U256::ZERO), Vec::<u8>::new());
    }

    #[test]
    fn set_bit_positions_single_low_bit() {
        assert_eq!(set_bit_positions(U256::from(1u8)), vec![0u8]);
    }

    #[test]
    fn set_bit_positions_single_high_bit() {
        let word = U256::from(1u8) << 255usize;
        assert_eq!(set_bit_positions(word), vec![255u8]);
    }

    #[test]
    fn set_bit_positions_multiple_bits_ascending_order() {
        let word = U256::from(1u8) | (U256::from(1u8) << 5usize) | (U256::from(1u8) << 200usize);
        assert_eq!(set_bit_positions(word), vec![0u8, 5u8, 200u8]);
    }

    #[test]
    fn set_bit_positions_all_bits_set() {
        let word = U256::MAX;
        let positions = set_bit_positions(word);
        assert_eq!(positions.len(), 256);
        assert_eq!(positions[0], 0u8);
        assert_eq!(positions[255], 255u8);
    }

    // Uses the same `#[tokio::test]` convention already established in
    // this codebase (see `chain::base`'s tests) rather than introducing a
    // new async-test pattern.
    #[tokio::test]
    async fn hydrate_initialized_ticks_rejects_non_positive_tick_spacing() {
        // Pure input-validation path - returns before any RPC call is made.
        let adapter = UniswapV3Adapter::new();
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
                sqrt_price_x96: U256::from(1u128) << 96usize,
                current_tick: 0,
                liquidity: 0,
                initialized_ticks: Default::default(),
            },
        };

        let result = adapter
            .hydrate_initialized_ticks("http://localhost:8545", &pool, 0, 0, 1)
            .await;
        assert!(matches!(result, Err(EngineError::Config(_))));
    }

    /// The pinned-state interface's honesty contract: requesting a
    /// specific block must fail EXPLICITLY (never silently fall back to
    /// "latest" and pretend it was pinned) - this is pure input handling,
    /// exercised without any RPC call.
    #[tokio::test]
    async fn get_pool_state_and_ticks_at_block_rejects_explicit_block_as_not_implemented() {
        let adapter = UniswapV3Adapter::new();
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
                sqrt_price_x96: U256::from(1u128) << 96usize,
                current_tick: 0,
                liquidity: 0,
                initialized_ticks: Default::default(),
            },
        };

        let result = adapter
            .get_pool_state_and_ticks_at_block("http://localhost:8545", &pool, 1, Some(12_345_678))
            .await;
        match result {
            Err(EngineError::NotImplemented(msg)) => {
                assert!(
                    msg.contains("12345678"),
                    "error message should name the requested block: {msg}"
                );
            }
            other => panic!("expected NotImplemented naming the block, got {other:?}"),
        }
    }
}
