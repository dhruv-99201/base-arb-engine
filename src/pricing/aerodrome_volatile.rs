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
use crate::pricing::full_math::mul_div;
use alloy::primitives::U256;

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
}
