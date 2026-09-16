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
//! **Pinned-block reads.** `get_pool_state`/`hydrate_initialized_ticks` (no
//! block argument) remain best-effort "latest" reads, each its own separate
//! RPC round-trip - not an atomic snapshot, unchanged from Day 1/2/3.
//! [`UniswapV3Adapter::get_pool_state_and_ticks_at_block`] is the pinned
//! path: when given `at_block: Some(block_number)`, every one of
//! `slot0`/`liquidity`/`fee`/`tickSpacing`/`tickBitmap`/`ticks` is called
//! with the SAME `alloy::rpc::types::BlockId` via `CallBuilder::block(...)`
//! (confirmed real, exact API for `alloy = "2.4.1"`, per `Cargo.lock` - see
//! the block-pinned-reads audit), so the resulting `PoolState` +
//! `HydratedTicks` reflect one consistent block rather than however many
//! independent "latest" reads. `at_block: None` is unchanged best-effort
//! behavior.
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
//! `cargo check`/`cargo test` now pass. The `.block(BlockId)` calls added
//! for pinned reads are backed by directly reading the real, exact pinned
//! source (`alloy-contract`/`alloy-provider`/`alloy-eips` 2.4.1, downloaded
//! from crates.io and inspected - not memory, not a different version) -
//! see the block-pinned-reads audit - but "the API is confirmed to exist
//! and have this shape" is not the same claim as "this file compiles";
//! neither this addition nor anything else in this file has been compiled
//! in this environment. Only the pure, RPC-free
//! bit-extraction helper (`set_bit_positions`) is covered by tests that
//! have been independently hand-verified.

use crate::dex::traits::DexAdapter;
use crate::error::{EngineError, EngineResult};
use crate::events::decoder;
use crate::events::model::MarketEvent;
use crate::market::models::{Pool, PoolKind, PoolState};
use crate::pricing::v3_quote::HydratedTicks;
use alloy::primitives::U256;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{BlockId, Log as RpcLog};
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
    /// `tickBitmap()`/`ticks()` calls against `pool.address`, against
    /// whatever "latest" resolves to at call time (best-effort, NOT a
    /// pinned snapshot - see the module docs). Use
    /// [`Self::get_pool_state_and_ticks_at_block`] for pinned reads.
    pub async fn hydrate_initialized_ticks(
        &self,
        rpc_url: &str,
        pool: &Pool,
        tick_spacing: i32,
        current_tick: i32,
        word_radius: i32,
    ) -> EngineResult<HydratedTicks> {
        self.hydrate_initialized_ticks_impl(rpc_url, pool, tick_spacing, current_tick, word_radius, None)
            .await
    }

    /// Shared implementation behind [`Self::hydrate_initialized_ticks`]
    /// (`at_block: None`) and the pinned path in
    /// [`Self::get_pool_state_and_ticks_at_block`] (`at_block: Some(_)`).
    /// When `at_block` is `Some(n)`, EVERY `tickBitmap()`/`ticks()` call
    /// below is pinned to the SAME `BlockId::number(n)` - never a mix of
    /// pinned and latest calls within one invocation.
    async fn hydrate_initialized_ticks_impl(
        &self,
        rpc_url: &str,
        pool: &Pool,
        tick_spacing: i32,
        current_tick: i32,
        word_radius: i32,
        at_block: Option<u64>,
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

        // The single BlockId every call below is pinned to, when pinning
        // was requested - constructed once so there is exactly one value
        // to keep in sync, not one per call site.
        let block_id: Option<BlockId> = at_block.map(BlockId::number);

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

            let mut bitmap_call = contract.tickBitmap(word_pos_i16);
            if let Some(b) = block_id {
                bitmap_call = bitmap_call.block(b);
            }
            let bitmap = bitmap_call.call().await.map_err(|e| EngineError::Dex {
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

                let mut ticks_call = contract.ticks(tick_arg);
                if let Some(b) = block_id {
                    ticks_call = ticks_call.block(b);
                }
                let tick_info = ticks_call.call().await.map_err(|e| EngineError::Dex {
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

        Ok(HydratedTicks::new(initialized_ticks, hydrated_tick_lo, hydrated_tick_hi))
    }

    /// The pinned-state interface: reads `slot0`, `liquidity`, `fee`,
    /// `tickSpacing`, `tickBitmap`, and `ticks` all as of the SAME block,
    /// so the resulting `PoolState` + `HydratedTicks` form one
    /// internally-consistent snapshot rather than up to
    /// `4 + (2 * set-bit-count) + 1` independent "latest" reads that could
    /// each land on a different block under concurrent chain activity.
    ///
    /// `at_block: None` runs the existing best-effort behavior
    /// (`get_pool_state` then `hydrate_initialized_ticks`, both against
    /// whatever "latest" resolves to at each individual call) - this is
    /// explicitly NOT a pinned snapshot, and this function does not pretend
    /// otherwise.
    ///
    /// `at_block: Some(block_number)` pins every one of
    /// `slot0`/`liquidity`/`fee`/`tickSpacing`/`tickBitmap`/`ticks` to
    /// `BlockId::number(block_number)` via `CallBuilder::block(...)` - the
    /// real, exact API confirmed against this project's pinned
    /// `alloy = "2.4.1"` source (see the block-pinned-reads audit; not
    /// guessed, not from a different alloy version). Whether a historical
    /// read at that block actually succeeds still depends on the
    /// configured RPC endpoint being archive-capable - this function
    /// cannot make a non-archive endpoint return historical state, and
    /// does not claim to.
    pub async fn get_pool_state_and_ticks_at_block(
        &self,
        rpc_url: &str,
        pool: &Pool,
        word_radius: i32,
        at_block: Option<u64>,
    ) -> EngineResult<(PoolState, HydratedTicks)> {
        let pool_state = self.get_pool_state_impl(rpc_url, pool, at_block).await?;
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
            .hydrate_initialized_ticks_impl(
                rpc_url,
                &pool_state.pool,
                tick_spacing,
                current_tick,
                word_radius,
                at_block,
            )
            .await?;

        Ok((pool_state, hydrated_ticks))
    }

    /// Shared implementation behind the trait's `get_pool_state`
    /// (`at_block: None`) and the pinned path in
    /// [`Self::get_pool_state_and_ticks_at_block`] (`at_block: Some(_)`).
    /// When `at_block` is `Some(n)`, EVERY `slot0`/`liquidity`/`fee`/
    /// `tickSpacing` call below is pinned to the SAME `BlockId::number(n)`
    /// - never a mix of pinned and latest calls within one invocation -
    /// and `n` itself is used directly as the resulting `PoolState`'s
    /// `last_updated_block`, without an extra `get_block_number()` call
    /// (which would just be a fifth, unrelated "what's current now" read).
    async fn get_pool_state_impl(
        &self,
        rpc_url: &str,
        pool: &Pool,
        at_block: Option<u64>,
    ) -> EngineResult<PoolState> {
        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let contract = IUniswapV3Pool::new(pool.address, provider.clone());

        // The single BlockId every call below is pinned to, when pinning
        // was requested - constructed once so there is exactly one value
        // to keep in sync, not one per call site.
        let block_id: Option<BlockId> = at_block.map(BlockId::number);

        let mut slot0_call = contract.slot0();
        if let Some(b) = block_id {
            slot0_call = slot0_call.block(b);
        }
        let slot0 = slot0_call.call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("slot0() failed: {e}"),
        })?;

        let mut liquidity_call = contract.liquidity();
        if let Some(b) = block_id {
            liquidity_call = liquidity_call.block(b);
        }
        let liquidity = liquidity_call.call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("liquidity() failed: {e}"),
        })?;

        let mut fee_call = contract.fee();
        if let Some(b) = block_id {
            fee_call = fee_call.block(b);
        }
        let fee = fee_call.call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("fee() failed: {e}"),
        })?;

        let mut tick_spacing_call = contract.tickSpacing();
        if let Some(b) = block_id {
            tick_spacing_call = tick_spacing_call.block(b);
        }
        let tick_spacing = tick_spacing_call.call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("tickSpacing() failed: {e}"),
        })?;

        // Freshness: when a block was explicitly requested, that IS the
        // block every read above was pinned to - use it directly rather
        // than making a fifth, independent "what's the current tip" call,
        // which would neither reflect the pinned reads nor be consistent
        // with them. Only fall back to a live `get_block_number()` call in
        // the unpinned (`None`) path, where "the current tip" is the
        // closest available approximation of when these best-effort reads
        // happened.
        let block_number = match at_block {
            Some(n) => n,
            None => provider
                .get_block_number()
                .await
                .map_err(|e| EngineError::Chain(format!("get_block_number failed: {e}")))?,
        };

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
        self.get_pool_state_impl(rpc_url, pool, None).await
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

    /// The block-id mapping logic itself (`at_block.map(BlockId::number)`,
    /// used identically in both `get_pool_state_impl` and
    /// `hydrate_initialized_ticks_impl`) - pure value mapping, no RPC
    /// involved, fully testable without any live network access.
    #[test]
    fn at_block_maps_to_block_id_number_correctly() {
        let mapped: Option<BlockId> = Some(12_345_678u64).map(BlockId::number);
        assert_eq!(mapped, Some(BlockId::number(12_345_678)));

        let none_case: Option<BlockId> = None.map(BlockId::number);
        assert_eq!(none_case, None);
    }

    /// The pinned-state interface no longer short-circuits `Some(block)`
    /// to `NotImplemented` - it now actually attempts to reach the RPC
    /// endpoint and pin every read there (see `get_pool_state_impl`/
    /// `hydrate_initialized_ticks_impl`). There is no live RPC server in
    /// this test environment (nothing listens on `localhost:8545` here),
    /// so this can only confirm the failure is a REAL attempted-call
    /// failure, not the old short-circuit - it is NOT, and must not be
    /// read as, a claim that pinning actually works against a real
    /// archive node. See the block-pinned-reads audit for what remains
    /// unverified.
    #[tokio::test]
    async fn get_pool_state_and_ticks_at_block_no_longer_short_circuits_some_block() {
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

        assert!(
            !matches!(result, Err(EngineError::NotImplemented(_))),
            "Some(block) must no longer short-circuit to NotImplemented - got {result:?}"
        );
        // No RPC server is actually listening in this environment, so this
        // must fail via a real attempted call (a transport/Dex error), not
        // succeed - there is no live RPC access here to succeed against.
        assert!(result.is_err());
    }
}
