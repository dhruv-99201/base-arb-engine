//! Single-step swap math: `compute_swap_step`, the core of the tick-crossing
//! swap loop. For one segment of a swap (between two adjacent initialized
//! ticks, or between the current price and a target price), computes how
//! much input is consumed, how much output is produced, the resulting sqrt
//! price, and the fee charged. Port of Uniswap V3's
//! `SwapMath.sol::computeSwapStep`.
//!
//! Adapted from the real, published `wp-evm-amm-math` crate
//! (docs.rs/crate/wp-evm-amm-math, "Direct port of Uniswap V3
//! SwapMath.sol"), built on the exact same `alloy_primitives::{I256,
//! U256}` this codebase uses - used as a cross-checked reference rather
//! than re-deriving the port from Solidity from scratch. Only the error
//! type was changed to this codebase's `EngineError`.

use crate::error::{EngineError, EngineResult};
use crate::pricing::full_math::{mul_div, mul_div_rounding_up};
use crate::pricing::sqrt_price_math::{
    get_amount_0_delta, get_amount_1_delta, get_next_sqrt_price_from_input,
    get_next_sqrt_price_from_output,
};
use alloy::primitives::{I256, U256};

/// Result of a single swap step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapStep {
    /// New sqrt price after this step. Either equals `sqrt_ratio_target_x96`
    /// (if the step crossed the target) or some price strictly between the
    /// starting price and the target.
    pub sqrt_ratio_next_x96: U256,
    /// Token amount in (excluding fee) consumed by this step.
    pub amount_in: U256,
    /// Token amount out produced by this step.
    pub amount_out: U256,
    /// Fee charged for this step, in the input token.
    pub fee_amount: U256,
}

const FEE_DENOMINATOR: u32 = 1_000_000;

/// `SwapMath.computeSwapStep`.
///
/// Consumes at most `amount_remaining` of the input token (exact-in, when
/// `amount_remaining >= 0`), or produces at most `|amount_remaining|` of
/// the output token (exact-out, when `amount_remaining < 0`) - the V3 sign
/// convention.
///
/// `fee_pips` is the fee in hundredths of a basis point (1e-6), e.g. `3000`
/// for 0.30%. Must be strictly less than `1_000_000` (100%).
pub fn compute_swap_step(
    sqrt_ratio_current_x96: U256,
    sqrt_ratio_target_x96: U256,
    liquidity: u128,
    amount_remaining: I256,
    fee_pips: u32,
) -> EngineResult<SwapStep> {
    if fee_pips >= FEE_DENOMINATOR {
        return Err(EngineError::Arithmetic(format!(
            "compute_swap_step: fee_pips {fee_pips} must be < {FEE_DENOMINATOR}"
        )));
    }

    let zero_for_one = sqrt_ratio_current_x96 >= sqrt_ratio_target_x96;
    let exact_in = !amount_remaining.is_negative();

    let mut amount_in = U256::ZERO;
    let mut amount_out = U256::ZERO;
    let sqrt_ratio_next_x96: U256;

    if exact_in {
        let amount_remaining_u: U256 = amount_remaining.into_raw();
        let amount_remaining_less_fee = mul_div(
            amount_remaining_u,
            U256::from(FEE_DENOMINATOR - fee_pips),
            U256::from(FEE_DENOMINATOR),
        )?;

        amount_in = if zero_for_one {
            get_amount_0_delta(sqrt_ratio_target_x96, sqrt_ratio_current_x96, liquidity, true)?
        } else {
            get_amount_1_delta(sqrt_ratio_current_x96, sqrt_ratio_target_x96, liquidity, true)?
        };

        sqrt_ratio_next_x96 = if amount_remaining_less_fee >= amount_in {
            sqrt_ratio_target_x96
        } else {
            get_next_sqrt_price_from_input(
                sqrt_ratio_current_x96,
                liquidity,
                amount_remaining_less_fee,
                zero_for_one,
            )?
        };
    } else {
        let amount_remaining_abs: U256 = amount_remaining.unsigned_abs();

        amount_out = if zero_for_one {
            get_amount_1_delta(sqrt_ratio_target_x96, sqrt_ratio_current_x96, liquidity, false)?
        } else {
            get_amount_0_delta(sqrt_ratio_current_x96, sqrt_ratio_target_x96, liquidity, false)?
        };

        sqrt_ratio_next_x96 = if amount_remaining_abs >= amount_out {
            sqrt_ratio_target_x96
        } else {
            get_next_sqrt_price_from_output(
                sqrt_ratio_current_x96,
                liquidity,
                amount_remaining_abs,
                zero_for_one,
            )?
        };
    }

    let max_reached = sqrt_ratio_target_x96 == sqrt_ratio_next_x96;

    if zero_for_one {
        amount_in = if max_reached && exact_in {
            amount_in
        } else {
            get_amount_0_delta(sqrt_ratio_next_x96, sqrt_ratio_current_x96, liquidity, true)?
        };
        amount_out = if max_reached && !exact_in {
            amount_out
        } else {
            get_amount_1_delta(sqrt_ratio_next_x96, sqrt_ratio_current_x96, liquidity, false)?
        };
    } else {
        amount_in = if max_reached && exact_in {
            amount_in
        } else {
            get_amount_1_delta(sqrt_ratio_current_x96, sqrt_ratio_next_x96, liquidity, true)?
        };
        amount_out = if max_reached && !exact_in {
            amount_out
        } else {
            get_amount_0_delta(sqrt_ratio_current_x96, sqrt_ratio_next_x96, liquidity, false)?
        };
    }

    // Cap output at the requested amount in exact-out mode - prevents
    // rounding on the `round_up=false` delta calls from ever exceeding what
    // was asked for. Mirrors Solidity exactly.
    if !exact_in {
        let amount_remaining_abs: U256 = amount_remaining.unsigned_abs();
        if amount_out > amount_remaining_abs {
            amount_out = amount_remaining_abs;
        }
    }

    let fee_amount = if exact_in && sqrt_ratio_next_x96 != sqrt_ratio_target_x96 {
        // Partial step on exact-in: the leftover input becomes fee.
        let amount_remaining_u: U256 = amount_remaining.into_raw();
        amount_remaining_u.checked_sub(amount_in).ok_or_else(|| {
            EngineError::Arithmetic("compute_swap_step: fee underflow".into())
        })?
    } else {
        // Reached the target (or exact-out): fee is computed from amount_in.
        mul_div_rounding_up(
            amount_in,
            U256::from(fee_pips),
            U256::from(FEE_DENOMINATOR - fee_pips),
        )?
    };

    Ok(SwapStep {
        sqrt_ratio_next_x96,
        amount_in,
        amount_out,
        fee_amount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::tick_math::get_sqrt_ratio_at_tick;

    fn price_1() -> U256 {
        U256::from_str_radix("79228162514264337593543950336", 10).unwrap()
    }

    /// Reference vector from `wp-evm-amm-math`'s own test suite (claiming
    /// cross-validation against an independent oracle) - not a
    /// self-invented expected value.
    #[test]
    fn reference_vector_exact_in_partial() {
        let sc = price_1();
        let st = U256::from_str_radix("78228162514264337593543950336", 10).unwrap();
        let liq: u128 = 1_000_000_000_000_000_000_000_000;
        let amt = I256::try_from(U256::from(1_000_000_000_000_000_u64)).unwrap();
        let step = compute_swap_step(sc, st, liq, amt, 3000).unwrap();
        assert_eq!(
            step.sqrt_ratio_next_x96,
            U256::from_str_radix("79228162435273859645575912270", 10).unwrap()
        );
        assert_eq!(step.amount_in, U256::from(997_000_000_000_000_u64));
        assert_eq!(step.amount_out, U256::from(996_999_999_005_991_u64));
        assert_eq!(step.fee_amount, U256::from(3_000_000_000_000_u64));
        assert_eq!(
            step.amount_in + step.fee_amount,
            U256::from(1_000_000_000_000_000_u64)
        );
    }

    #[test]
    fn reference_vector_exact_in_capped_at_target() {
        let sc = price_1();
        let st = U256::from_str_radix("79728162514264337593543950336", 10).unwrap();
        let liq: u128 = 2_000_000_000_000_000_000;
        let amt = I256::try_from(U256::from(10u128.pow(20))).unwrap();
        let step = compute_swap_step(sc, st, liq, amt, 600).unwrap();
        assert_eq!(step.sqrt_ratio_next_x96, st, "should snap to target");
        assert_eq!(step.amount_in, U256::from(12_621_774_483_536_189_u64));
        assert_eq!(step.amount_out, U256::from(12_542_619_426_618_390_u64));
        assert_eq!(step.fee_amount, U256::from(7_577_611_256_876_u64));
    }

    #[test]
    fn rejects_invalid_fee_pips() {
        let p = price_1();
        let err = compute_swap_step(p, p, 1, I256::ZERO, FEE_DENOMINATOR)
            .expect_err("fee_pips == 1e6 must be rejected");
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    #[test]
    fn exact_in_capped_at_target_when_input_exceeds() {
        let p_current = price_1();
        let p_target = get_sqrt_ratio_at_tick(-100).unwrap();
        let liq: u128 = 2_000_000_000_000_000_000;
        let huge_in = I256::try_from(U256::from(10u128.pow(20))).unwrap();
        let step = compute_swap_step(p_current, p_target, liq, huge_in, 600).unwrap();
        assert_eq!(step.sqrt_ratio_next_x96, p_target, "should snap to target");
        assert!(step.amount_in > U256::ZERO);
        assert!(step.amount_out > U256::ZERO);
        assert!(step.fee_amount > U256::ZERO);
    }

    #[test]
    fn exact_in_partial_when_input_insufficient() {
        let p_current = price_1();
        let p_target = get_sqrt_ratio_at_tick(-1000).unwrap();
        let liq: u128 = 2_000_000_000_000_000_000_000_000u128;
        let small_in = I256::try_from(U256::from(1_000u64)).unwrap();
        let step = compute_swap_step(p_current, p_target, liq, small_in, 3000).unwrap();
        assert_ne!(step.sqrt_ratio_next_x96, p_target, "should not reach target");
        assert!(step.sqrt_ratio_next_x96 < p_current);
        assert_eq!(step.amount_in + step.fee_amount, U256::from(1_000u64));
    }

    #[test]
    fn exact_out_capped_at_target_when_output_exceeds() {
        let p_current = price_1();
        let p_target = get_sqrt_ratio_at_tick(-100).unwrap();
        let liq: u128 = 2_000_000_000_000_000_000;
        let huge_out_neg = I256::try_from(U256::from(10u128.pow(20)))
            .unwrap()
            .checked_neg()
            .unwrap();
        let step = compute_swap_step(p_current, p_target, liq, huge_out_neg, 600).unwrap();
        assert_eq!(step.sqrt_ratio_next_x96, p_target, "should snap to target");
    }

    #[test]
    fn exact_out_output_capped_at_request_magnitude() {
        let p_current = price_1();
        let p_target = get_sqrt_ratio_at_tick(-1000).unwrap();
        let liq: u128 = 2_000_000_000_000_000_000_000_000u128;
        let req_out = U256::from(1_000u64);
        let neg = I256::try_from(req_out).unwrap().checked_neg().unwrap();
        let step = compute_swap_step(p_current, p_target, liq, neg, 3000).unwrap();
        assert!(step.amount_out <= req_out, "output must not exceed request");
    }

    #[test]
    fn zero_for_one_decreases_price() {
        let p_current = price_1();
        let p_target = get_sqrt_ratio_at_tick(-50).unwrap();
        let liq: u128 = 1_000_000_000_000_000_000_000;
        let in_amt = I256::try_from(U256::from(1_000_000u64)).unwrap();
        let step = compute_swap_step(p_current, p_target, liq, in_amt, 3000).unwrap();
        assert!(step.sqrt_ratio_next_x96 < p_current);
        assert!(step.sqrt_ratio_next_x96 >= p_target);
    }

    #[test]
    fn one_for_zero_increases_price() {
        let p_current = price_1();
        let p_target = get_sqrt_ratio_at_tick(50).unwrap();
        let liq: u128 = 1_000_000_000_000_000_000_000;
        let in_amt = I256::try_from(U256::from(1_000_000u64)).unwrap();
        let step = compute_swap_step(p_current, p_target, liq, in_amt, 3000).unwrap();
        assert!(step.sqrt_ratio_next_x96 > p_current);
        assert!(step.sqrt_ratio_next_x96 <= p_target);
    }

    #[test]
    fn fee_proportional_when_capped_at_target() {
        let p_current = price_1();
        let p_target = get_sqrt_ratio_at_tick(-100).unwrap();
        let liq: u128 = 2_000_000_000_000_000_000;
        let huge_in = I256::try_from(U256::from(10u128.pow(20))).unwrap();
        let step = compute_swap_step(p_current, p_target, liq, huge_in, 10_000).unwrap();
        let expected =
            mul_div_rounding_up(step.amount_in, U256::from(10_000u64), U256::from(990_000u64))
                .unwrap();
        assert_eq!(step.fee_amount, expected);
    }

    #[test]
    fn zero_amount_remaining_is_a_no_op() {
        let p = price_1();
        let liq: u128 = 1_000_000_000_000_000_000;
        let step = compute_swap_step(p, p, liq, I256::ZERO, 3000).unwrap();
        assert_eq!(step.sqrt_ratio_next_x96, p);
        assert_eq!(step.amount_in, U256::ZERO);
        assert_eq!(step.amount_out, U256::ZERO);
        assert_eq!(step.fee_amount, U256::ZERO);
    }

    #[test]
    fn small_amount_does_not_panic_or_error() {
        let p_current = price_1();
        let p_target = get_sqrt_ratio_at_tick(-887271).unwrap();
        let liq: u128 = 1_000_000_000_000_000_000_000_000u128;
        let tiny_in = I256::try_from(U256::from(1u64)).unwrap();
        let step = compute_swap_step(p_current, p_target, liq, tiny_in, 3000).unwrap();
        assert!(step.amount_in <= U256::from(1u64));
    }

    #[test]
    fn large_liquidity_and_amount_do_not_overflow() {
        let p_current = price_1();
        let p_target = get_sqrt_ratio_at_tick(887271).unwrap();
        let liq: u128 = u128::MAX / 2;
        let big_in = I256::try_from(U256::from(1u128) << 100usize).unwrap();
        let step = compute_swap_step(p_current, p_target, liq, big_in, 3000);
        assert!(step.is_ok(), "large values must not panic or silently corrupt: {step:?}");
    }
}
