//! Aerodrome Classic **volatile** pool exact-input quote. Stable pools are
//! explicitly out of scope (different curve entirely - see Day 2/3 specs).
//!
//! Fee-on-input, then constant-product:
//! ```text
//! fee = floor(amount_in * fee_bps / 10000)
//! amount_in_after_fee = amount_in - fee
//! amount_out = (amount_in_after_fee * reserve_out) / (reserve_in + amount_in_after_fee)
//! ```
//! No decimal normalization - both reserves and the input/output amounts
//! stay in native token units throughout, matching the real pool
//! contract's integer arithmetic exactly (and Day 1's financial-code rule
//! against floating point).

use crate::error::{EngineError, EngineResult};
use crate::market::models::{Pool, PoolKind};
use crate::pricing::full_math::mul_div;
use alloy::primitives::U256;

/// Quote an exact-input swap against a hydrated `Pool` whose `kind` is
/// `PoolKind::Aerodrome` (**volatile** only - stable-curve pools are
/// explicitly out of scope, same as [`quote_exact_input_aerodrome_volatile`]
/// itself). Wires the already-tested math above to the real pool model's
/// `reserve0`/`reserve1`/`fee_bps` - the integration piece Day 1/2 never
/// built (they only ever read raw reserves, never quoted against them).
///
/// `fee_bps` is `Option<U256>` on the model (see
/// `PoolKind::Aerodrome::fee_bps` docs): `None` means the pool's real fee
/// has not been hydrated from the Aerodrome factory yet, and this function
/// rejects that explicitly (`EngineError::State`) rather than assuming
/// zero - a placeholder pool must never be quoted as if it were a genuine
/// zero-fee pool. `Some(U256::ZERO)` (a real, hydrated, protocol-permitted
/// zero fee) is quoted normally.
pub fn quote_pool_exact_input(
    pool: &Pool,
    amount_in: U256,
    zero_for_one: bool,
) -> EngineResult<U256> {
    match &pool.kind {
        PoolKind::Aerodrome {
            reserve0,
            reserve1,
            stable,
            fee_bps,
        } => {
            if *stable {
                return Err(EngineError::NotImplemented(
                    "quote_pool_exact_input: Aerodrome stable-curve pricing is out of scope - \
                     only volatile (x*y=k) pools are quoted"
                        .into(),
                ));
            }
            let fee_bps = fee_bps.ok_or_else(|| {
                EngineError::State(
                    "quote_pool_exact_input: Aerodrome pool fee has not been hydrated from the \
                     real factory yet - refusing to assume a zero fee (see \
                     PoolKind::Aerodrome::fee_bps docs)"
                        .into(),
                )
            })?;
            let (reserve_in, reserve_out) = if zero_for_one {
                (*reserve0, *reserve1)
            } else {
                (*reserve1, *reserve0)
            };
            quote_exact_input_aerodrome_volatile(amount_in, reserve_in, reserve_out, fee_bps)
        }
        PoolKind::ConcentratedLiquidity { .. } => Err(EngineError::NotImplemented(
            "quote_pool_exact_input: pool is not an Aerodrome classic pool".into(),
        )),
    }
}

/// `floor(amount_in * fee_bps / 10000)`, then `amount_in - fee`.
pub fn quote_exact_input_aerodrome_volatile(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: U256,
) -> EngineResult<U256> {
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err(EngineError::Arithmetic(
            "quote_exact_input_aerodrome_volatile: pool has zero reserves".into(),
        ));
    }
    if amount_in.is_zero() {
        return Ok(U256::ZERO);
    }

    let fee = mul_div(amount_in, fee_bps, U256::from(10_000u64))?;
    let amount_in_after_fee = amount_in
        .checked_sub(fee)
        .ok_or_else(|| EngineError::Arithmetic("fee exceeds amount_in".into()))?;

    let denominator = reserve_in
        .checked_add(amount_in_after_fee)
        .ok_or_else(|| EngineError::Arithmetic("reserve_in + amount_in_after_fee overflows".into()))?;

    mul_div(amount_in_after_fee, reserve_out, denominator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_quote_matches_hand_computed_value() {
        // reserve_in=1_000_000, reserve_out=2_000_000, amount_in=10_000, fee_bps=30 (0.30%)
        // fee = floor(10_000 * 30 / 10_000) = 30
        // amount_after_fee = 9_970
        // amount_out = floor(9_970 * 2_000_000 / (1_000_000 + 9_970))
        //            = floor(19_940_000_000 / 1_009_970) = 19_743 (hand-verified integer division)
        let result = quote_exact_input_aerodrome_volatile(
            U256::from(10_000u64),
            U256::from(1_000_000u64),
            U256::from(2_000_000u64),
            U256::from(30u64),
        )
        .unwrap();
        assert_eq!(result, U256::from(19_743u64));
    }

    #[test]
    fn zero_amount_in_gives_zero_out() {
        let result = quote_exact_input_aerodrome_volatile(
            U256::ZERO,
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            U256::from(30u64),
        )
        .unwrap();
        assert_eq!(result, U256::ZERO);
    }

    #[test]
    fn zero_reserve_is_rejected() {
        let err = quote_exact_input_aerodrome_volatile(
            U256::from(1_000u64),
            U256::ZERO,
            U256::from(1_000_000u64),
            U256::from(30u64),
        )
        .unwrap_err();
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    #[test]
    fn zero_fee_matches_pure_constant_product() {
        // With fee_bps=0: amount_out = amount_in * reserve_out / (reserve_in + amount_in)
        let amount_in = U256::from(1_000u64);
        let reserve_in = U256::from(500_000u64);
        let reserve_out = U256::from(1_000_000u64);
        let result =
            quote_exact_input_aerodrome_volatile(amount_in, reserve_in, reserve_out, U256::ZERO)
                .unwrap();
        let expected = amount_in * reserve_out / (reserve_in + amount_in);
        assert_eq!(result, expected);
    }

    #[test]
    fn output_never_exceeds_reserve_out() {
        // Even a very large input can never drain more than reserve_out
        // (asymptotic approach to reserve_out, never reaching or exceeding
        // it, per the constant-product curve).
        let huge_in = U256::from(1u128) << 100usize;
        let reserve_in = U256::from(1_000_000u64);
        let reserve_out = U256::from(2_000_000u64);
        let result =
            quote_exact_input_aerodrome_volatile(huge_in, reserve_in, reserve_out, U256::from(30u64))
                .unwrap();
        assert!(result < reserve_out);
    }

    #[test]
    fn output_increases_monotonically_with_input() {
        let reserve_in = U256::from(1_000_000u64);
        let reserve_out = U256::from(2_000_000u64);
        let fee_bps = U256::from(30u64);
        let mut prev = U256::ZERO;
        for amount_in in [100u64, 1_000, 10_000, 100_000, 1_000_000] {
            let out = quote_exact_input_aerodrome_volatile(
                U256::from(amount_in),
                reserve_in,
                reserve_out,
                fee_bps,
            )
            .unwrap();
            assert!(out > prev, "output must strictly increase with input");
            prev = out;
        }
    }

    #[test]
    fn higher_fee_gives_lower_output_for_same_input() {
        let amount_in = U256::from(100_000u64);
        let reserve_in = U256::from(1_000_000u64);
        let reserve_out = U256::from(2_000_000u64);
        let low_fee =
            quote_exact_input_aerodrome_volatile(amount_in, reserve_in, reserve_out, U256::from(5u64))
                .unwrap();
        let high_fee = quote_exact_input_aerodrome_volatile(
            amount_in,
            reserve_in,
            reserve_out,
            U256::from(100u64),
        )
        .unwrap();
        assert!(high_fee < low_fee);
    }

    #[test]
    fn no_floating_point_used_result_is_exact_integer_floor() {
        // Regression-style check that rounding is a floor (never rounds up)
        // by constructing a case with a known non-exact division.
        let result = quote_exact_input_aerodrome_volatile(
            U256::from(7u64),
            U256::from(3u64),
            U256::from(11u64),
            U256::ZERO,
        )
        .unwrap();
        // amount_out = 7*11/(3+7) = 77/10 = 7.7 -> floors to 7
        assert_eq!(result, U256::from(7u64));
    }

    fn aerodrome_pool(fee_bps: Option<U256>, stable: bool) -> Pool {
        use crate::market::models::{DexKind, Token};
        use alloy::primitives::address;

        Pool {
            address: address!("0000000000000000000000000000000000000001"),
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
                reserve0: U256::from(1_000_000u64),
                reserve1: U256::from(2_000_000u64),
                stable,
                fee_bps,
            },
        }
    }

    #[test]
    fn quote_pool_exact_input_matches_direct_call_zero_for_one() {
        let pool = aerodrome_pool(Some(U256::from(30u64)), false);

        let via_wrapper = quote_pool_exact_input(&pool, U256::from(10_000u64), true).unwrap();
        let direct = quote_exact_input_aerodrome_volatile(
            U256::from(10_000u64),
            U256::from(1_000_000u64),
            U256::from(2_000_000u64),
            U256::from(30u64),
        )
        .unwrap();
        assert_eq!(via_wrapper, direct);
    }

    #[test]
    fn quote_pool_exact_input_swaps_reserves_for_one_for_zero() {
        let pool = aerodrome_pool(Some(U256::from(30u64)), false);

        let via_wrapper = quote_pool_exact_input(&pool, U256::from(10_000u64), false).unwrap();
        let direct = quote_exact_input_aerodrome_volatile(
            U256::from(10_000u64),
            U256::from(2_000_000u64),
            U256::from(1_000_000u64),
            U256::from(30u64),
        )
        .unwrap();
        assert_eq!(via_wrapper, direct);
    }

    #[test]
    fn quote_pool_exact_input_rejects_stable_pools() {
        let pool = aerodrome_pool(Some(U256::from(4u64)), true);
        let err = quote_pool_exact_input(&pool, U256::from(10_000u64), true).unwrap_err();
        assert!(matches!(err, EngineError::NotImplemented(_)));
    }

    #[test]
    fn quote_pool_exact_input_rejects_unhydrated_none_fee() {
        // The critical placeholder-safety regression test: a discovered
        // but not-yet-hydrated pool (fee_bps: None) must be rejected
        // explicitly, never silently quoted as a zero-fee pool.
        let pool = aerodrome_pool(None, false);
        let err = quote_pool_exact_input(&pool, U256::from(10_000u64), true)
            .expect_err("None fee_bps must be rejected, not treated as zero");
        assert!(
            matches!(err, EngineError::State(_)),
            "expected State (unhydrated), got {err:?}"
        );
    }

    #[test]
    fn quote_pool_exact_input_accepts_genuine_zero_fee() {
        // Some(U256::ZERO) is a real, hydrated, protocol-permitted zero
        // fee - must be quoted normally, not confused with None.
        let pool = aerodrome_pool(Some(U256::ZERO), false);
        let result = quote_pool_exact_input(&pool, U256::from(10_000u64), true).unwrap();
        let direct = quote_exact_input_aerodrome_volatile(
            U256::from(10_000u64),
            U256::from(1_000_000u64),
            U256::from(2_000_000u64),
            U256::ZERO,
        )
        .unwrap();
        assert_eq!(result, direct);
    }

    #[test]
    fn quote_pool_exact_input_rejects_non_aerodrome_pool_kind() {
        use crate::market::models::{DexKind, Token};
        use alloy::primitives::address;

        let pool = Pool {
            address: address!("0000000000000000000000000000000000000003"),
            dex: DexKind::UniswapV3,
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
            kind: PoolKind::ConcentratedLiquidity {
                fee_tier: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::from(1u128) << 96usize,
                current_tick: 0,
                liquidity: 1_000_000,
                initialized_ticks: Default::default(),
            },
        };

        let err = quote_pool_exact_input(&pool, U256::from(10_000u64), true).unwrap_err();
        assert!(matches!(err, EngineError::NotImplemented(_)));
    }
}
