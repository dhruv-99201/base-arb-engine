//! Bridge from the existing Aerodrome/Uniswap V3 pricing engines to
//! `arbitrage::LegQuote`. Thin wrappers only: no swap math duplicated
//! here, no RPC, no provider, no rehydration, no new trading policy. Each
//! function just calls the existing pricing engine and reshapes its
//! result into a `LegQuote`; every error it returns is exactly the
//! underlying pricing function's own error, unmodified.

use crate::arbitrage::opportunity::LegQuote;
use crate::error::EngineResult;
use crate::market::models::{DexKind, PoolState};
use crate::pricing::aerodrome_volatile::quote_pool_exact_input;
use crate::pricing::v3_quote::{quote_exact_input, HydratedTicks, HydratedV3State};
use alloy::primitives::U256;

/// Quote one Aerodrome leg via the existing
/// `pricing::aerodrome_volatile::quote_pool_exact_input`. That function
/// already rejects a non-Aerodrome `pool.kind`, a stable pool, and an
/// un-hydrated `fee_bps` - this wrapper adds no validation of its own,
/// just propagates whatever it returns.
pub fn quote_aerodrome_leg(
    pool_state: &PoolState,
    amount_in: U256,
    zero_for_one: bool,
) -> EngineResult<LegQuote> {
    let amount_out = quote_pool_exact_input(&pool_state.pool, amount_in, zero_for_one)?;

    let (token_in, token_out) = if zero_for_one {
        (pool_state.pool.token0.address, pool_state.pool.token1.address)
    } else {
        (pool_state.pool.token1.address, pool_state.pool.token0.address)
    };

    Ok(LegQuote {
        dex: pool_state.pool.dex,
        pool_address: pool_state.pool.address,
        token_in,
        token_out,
        amount_in,
        amount_out,
        block: pool_state.freshness.last_updated_block,
    })
}

/// Quote one Uniswap V3 leg via the existing
/// `HydratedV3State::from_pool_state` + `quote_exact_input`. That seam
/// already rejects a non-`ConcentratedLiquidity` `pool.kind` and an
/// inconsistent/incomplete hydration range - this wrapper adds no
/// validation of its own. `hydrated` must already correspond to the same
/// pinned state as `pool_state`; this function never rehydrates anything.
pub fn quote_uniswap_v3_leg(
    pool_state: &PoolState,
    hydrated: &HydratedTicks,
    amount_in: U256,
    zero_for_one: bool,
) -> EngineResult<LegQuote> {
    let state = HydratedV3State::from_pool_state(pool_state, hydrated)?;
    let result = quote_exact_input(&state, amount_in, zero_for_one)?;

    let (token_in, token_out) = if zero_for_one {
        (pool_state.pool.token0.address, pool_state.pool.token1.address)
    } else {
        (pool_state.pool.token1.address, pool_state.pool.token0.address)
    };

    Ok(LegQuote {
        dex: DexKind::UniswapV3,
        pool_address: pool_state.pool.address,
        token_in,
        token_out,
        amount_in,
        amount_out: result.amount_out,
        block: pool_state.freshness.last_updated_block,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrage::opportunity::Opportunity;
    use crate::error::EngineError;
    use crate::market::models::{Pool, PoolKind, Token};
    use alloy::primitives::address;
    use std::collections::BTreeMap;

    // Real Base WETH/USDC addresses and real pool addresses already used
    // by this project's golden fixtures (pricing::aerodrome_volatile,
    // pricing::v3_quote) - reused here, not re-derived.
    fn weth() -> alloy::primitives::Address {
        address!("4200000000000000000000000000000000000006")
    }
    fn usdc() -> alloy::primitives::Address {
        address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913")
    }

    /// Real Aerodrome Classic volatile WETH/USDC pool, state at block
    /// 26000000 - the same fixture already validated in
    /// `pricing::aerodrome_volatile`'s own golden test.
    fn aerodrome_fixture_pool_state() -> PoolState {
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
        PoolState::new(pool, 26_000_000, None)
    }

    /// Real Uniswap V3 WETH/USDC pool, state + the one real initialized
    /// tick at the SAME block 26000000 - the same fixture already
    /// validated in `pricing::v3_quote`'s own golden tests.
    fn v3_fixture_pool_state_and_ticks() -> (PoolState, HydratedTicks) {
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
        let pool_state = PoolState::new(pool, 26_000_000, None);

        let mut ticks = BTreeMap::new();
        ticks.insert(-197_070, -1_776_514_264_016_992i128);
        let hydrated = HydratedTicks::for_test(ticks, -199_680, -192_010);

        (pool_state, hydrated)
    }

    /// Items 1, 8, 9: forward direction against the real, externally
    /// validated golden amount_out (see
    /// `pricing::aerodrome_volatile::real_fixture_aerodrome_weth_usdc_block_26000000`).
    #[test]
    fn aerodrome_weth_to_usdc_matches_golden_fixture() {
        let pool_state = aerodrome_fixture_pool_state();
        let amount_in = U256::from(1_000_000_000_000_000_000u128);

        let leg = quote_aerodrome_leg(&pool_state, amount_in, true).unwrap();

        assert_eq!(leg.dex, DexKind::Aerodrome);
        assert_eq!(leg.pool_address, pool_state.pool.address);
        assert_eq!(leg.token_in, weth());
        assert_eq!(leg.token_out, usdc());
        assert_eq!(leg.amount_in, amount_in);
        assert_eq!(leg.amount_out, U256::from(2_753_396_596u64));
        assert_eq!(leg.block, 26_000_000);
    }

    /// Items 2, 8, 9: reverse direction. No independent external golden
    /// value exists for this direction/amount, so correctness is proven
    /// by exact equality against a direct call to the same underlying
    /// pricing function - proving the wrapper alters nothing.
    #[test]
    fn aerodrome_usdc_to_weth_matches_direct_pricing_call() {
        let pool_state = aerodrome_fixture_pool_state();
        let amount_in = U256::from(10_000_000_000u64);

        let leg = quote_aerodrome_leg(&pool_state, amount_in, false).unwrap();
        let direct = quote_pool_exact_input(&pool_state.pool, amount_in, false).unwrap();

        assert_eq!(leg.token_in, usdc());
        assert_eq!(leg.token_out, weth());
        assert_eq!(leg.amount_in, amount_in);
        assert_eq!(leg.amount_out, direct);
    }

    /// Items 3, 8, 9: forward direction against the real, externally
    /// validated golden amount_out (see
    /// `pricing::v3_quote::real_fixture_weth_to_usdc_no_cross`).
    #[test]
    fn v3_weth_to_usdc_matches_golden_fixture() {
        let (pool_state, hydrated) = v3_fixture_pool_state_and_ticks();
        let amount_in = U256::from(1_000_000_000_000_000u64);

        let leg = quote_uniswap_v3_leg(&pool_state, &hydrated, amount_in, true).unwrap();

        assert_eq!(leg.dex, DexKind::UniswapV3);
        assert_eq!(leg.pool_address, pool_state.pool.address);
        assert_eq!(leg.token_in, weth());
        assert_eq!(leg.token_out, usdc());
        assert_eq!(leg.amount_in, amount_in);
        assert_eq!(leg.amount_out, U256::from(2_764_652u64));
        assert_eq!(leg.block, 26_000_000);
    }

    /// Items 4, 8, 9: reverse direction, proven by exact equality against
    /// a direct call to `HydratedV3State::from_pool_state` +
    /// `quote_exact_input` with identical arguments.
    #[test]
    fn v3_usdc_to_weth_matches_direct_pricing_call() {
        let (pool_state, hydrated) = v3_fixture_pool_state_and_ticks();
        let amount_in = U256::from(10_000_000_000u64);

        let leg = quote_uniswap_v3_leg(&pool_state, &hydrated, amount_in, false).unwrap();

        let state = HydratedV3State::from_pool_state(&pool_state, &hydrated).unwrap();
        let direct = quote_exact_input(&state, amount_in, false).unwrap();

        assert_eq!(leg.token_in, usdc());
        assert_eq!(leg.token_out, weth());
        assert_eq!(leg.amount_in, amount_in);
        assert_eq!(leg.amount_out, direct.amount_out);
    }

    /// Item 5: a ConcentratedLiquidity pool is not an Aerodrome pool -
    /// rejected by the existing `quote_pool_exact_input`, propagated
    /// unmodified.
    #[test]
    fn aerodrome_wrapper_rejects_concentrated_liquidity_pool() {
        let pool = Pool {
            address: address!("0000000000000000000000000000000000000009"),
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
                sqrt_price_x96: U256::from(1u128) << 96usize,
                current_tick: 0,
                liquidity: 1_000_000,
                initialized_ticks: Default::default(),
            },
        };
        let pool_state = PoolState::new(pool, 26_000_000, None);

        let err = quote_aerodrome_leg(&pool_state, U256::from(1_000u64), true).unwrap_err();
        assert!(matches!(err, EngineError::NotImplemented(_)), "got {err:?}");
    }

    /// Item 6: an Aerodrome pool is not a ConcentratedLiquidity pool -
    /// rejected by the existing `HydratedV3State::from_pool_state`,
    /// propagated unmodified.
    #[test]
    fn v3_wrapper_rejects_aerodrome_pool() {
        let pool = Pool {
            address: address!("0000000000000000000000000000000000000008"),
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
                stable: false,
                fee_bps: Some(U256::from(30u64)),
            },
        };
        let pool_state = PoolState::new(pool, 26_000_000, None);
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);

        let err =
            quote_uniswap_v3_leg(&pool_state, &hydrated, U256::from(1_000u64), true).unwrap_err();
        assert!(matches!(err, EngineError::Dex { .. }), "got {err:?}");
    }

    /// Item 7: `LegQuote.block` must come from
    /// `PoolState.freshness.last_updated_block` - proven with a block
    /// number distinct from the golden fixture's 26000000, so this can't
    /// pass by coincidence.
    #[test]
    fn leg_quote_block_comes_from_pool_state_freshness() {
        let fixture = aerodrome_fixture_pool_state();
        let pool_state = PoolState::new(fixture.pool, 999, None);

        let leg = quote_aerodrome_leg(&pool_state, U256::from(1_000u64), true).unwrap();
        assert_eq!(leg.block, 999);
    }

    /// Item 10: end-to-end seam. Real Aerodrome pricing -> LegQuote ->
    /// real Uniswap V3 pricing -> LegQuote -> `Opportunity::evaluate`.
    /// Both fixtures are pinned to the same real block (26000000), and
    /// leg1's real amount_out is passed, unmodified, as leg2's amount_in -
    /// proving the dataflow/arithmetic seam, not a new golden value. Not
    /// asserted to be profitable (it isn't required to be).
    #[test]
    fn end_to_end_aerodrome_then_v3_seam_through_opportunity_evaluate() {
        let aerodrome_state = aerodrome_fixture_pool_state();
        let (v3_state, hydrated) = v3_fixture_pool_state_and_ticks();

        let leg1 = quote_aerodrome_leg(
            &aerodrome_state,
            U256::from(1_000_000_000_000_000_000u128),
            true, // WETH -> USDC
        )
        .unwrap();
        assert_eq!(leg1.amount_out, U256::from(2_753_396_596u64));

        // leg1's real output becomes leg2's input, unmodified.
        let leg2 = quote_uniswap_v3_leg(
            &v3_state,
            &hydrated,
            leg1.amount_out,
            false, // USDC -> WETH
        )
        .unwrap();
        assert_eq!(leg2.amount_in, leg1.amount_out);

        let opp = Opportunity::evaluate(leg1, leg2).unwrap();
        assert_eq!(opp.input_token(), weth());
        assert_eq!(opp.intermediate_token(), usdc());
        assert_eq!(opp.final_token(), weth());
        assert_eq!(opp.block(), 26_000_000);
    }
}
