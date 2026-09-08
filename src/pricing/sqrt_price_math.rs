//! Sqrt-price math: `get_amount_0_delta`/`get_amount_1_delta` and the
//! `get_next_sqrt_price_from_*` helpers `swap_math::compute_swap_step`
//! needs. Port of Uniswap V3's `SqrtPriceMath.sol`.
//!
//! Adapted from the real, published `wp-evm-amm-math` crate
//! (docs.rs/crate/wp-evm-amm-math), which is itself an explicit "Direct
//! port of Uniswap V3 `SqrtPriceMath.sol`" built on the exact same
//! `alloy_primitives::U256` this codebase uses - used here as a
//! cross-checked reference rather than re-deriving the port from Solidity
//! from scratch. Only the error type was changed (to this codebase's
//! `EngineError`) and `Q96` is constructed via the already-proven
//! `U256::from(1u128) << 96` pattern instead of `U256::from_limbs(...)`
//! (unverified API, avoided out of caution).

use crate::error::{EngineError, EngineResult};
use crate::pricing::full_math::{mul_div, mul_div_rounding_up};
use alloy::primitives::U256;

fn q96() -> U256 {
    U256::from(1u128) << 96usize
}

fn u160_max() -> U256 {
    (U256::from(1u64) << 160usize) - U256::from(1u64)
}

/// Integer division rounding up.
pub(crate) fn div_rounding_up(a: U256, b: U256) -> EngineResult<U256> {
    if b.is_zero() {
        return Err(EngineError::Arithmetic("div_rounding_up: division by zero".into()));
    }
    let q = a / b;
    let r = a % b;
    if r.is_zero() {
        Ok(q)
    } else {
        Ok(q + U256::from(1u64))
    }
}

/// `amount0 = liquidity * (sqrt_upper - sqrt_lower) / (sqrt_lower * sqrt_upper)`.
/// `round_up = true` rounds toward positive infinity.
pub fn get_amount_0_delta(
    sqrt_ratio_a_x96: U256,
    sqrt_ratio_b_x96: U256,
    liquidity: u128,
    round_up: bool,
) -> EngineResult<U256> {
    let (sqrt_lower, sqrt_upper) = if sqrt_ratio_a_x96 <= sqrt_ratio_b_x96 {
        (sqrt_ratio_a_x96, sqrt_ratio_b_x96)
    } else {
        (sqrt_ratio_b_x96, sqrt_ratio_a_x96)
    };

    if sqrt_lower.is_zero() {
        return Err(EngineError::Arithmetic(
            "get_amount_0_delta: sqrt_lower is zero".into(),
        ));
    }

    let numerator = U256::from(liquidity) << 96usize;
    let diff = sqrt_upper - sqrt_lower;

    if round_up {
        let amount = mul_div_rounding_up(numerator, diff, sqrt_upper)?;
        div_rounding_up(amount, sqrt_lower)
    } else {
        let amount = mul_div(numerator, diff, sqrt_upper)?;
        Ok(amount / sqrt_lower)
    }
}

/// `amount1 = liquidity * (sqrt_upper - sqrt_lower)`. `round_up = true`
/// rounds toward positive infinity.
pub fn get_amount_1_delta(
    sqrt_ratio_a_x96: U256,
    sqrt_ratio_b_x96: U256,
    liquidity: u128,
    round_up: bool,
) -> EngineResult<U256> {
    let (sqrt_lower, sqrt_upper) = if sqrt_ratio_a_x96 <= sqrt_ratio_b_x96 {
        (sqrt_ratio_a_x96, sqrt_ratio_b_x96)
    } else {
        (sqrt_ratio_b_x96, sqrt_ratio_a_x96)
    };

    let diff = sqrt_upper - sqrt_lower;

    if round_up {
        mul_div_rounding_up(U256::from(liquidity), diff, q96())
    } else {
        mul_div(U256::from(liquidity), diff, q96())
    }
}

fn get_next_sqrt_price_from_amount_0_rounding_up(
    sqrt_p_x96: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> EngineResult<U256> {
    if amount.is_zero() {
        return Ok(sqrt_p_x96);
    }
    let numerator_1: U256 = U256::from(liquidity) << 96usize;

    if add {
        if let Some(product) = amount.checked_mul(sqrt_p_x96) {
            if let Some(denominator) = numerator_1.checked_add(product) {
                return mul_div_rounding_up(numerator_1, sqrt_p_x96, denominator);
            }
        }
        let term = (numerator_1 / sqrt_p_x96)
            .checked_add(amount)
            .ok_or_else(|| EngineError::Arithmetic("get_next_sqrt_price: overflow".into()))?;
        div_rounding_up(numerator_1, term)
    } else {
        let product = amount
            .checked_mul(sqrt_p_x96)
            .ok_or_else(|| EngineError::Arithmetic("get_next_sqrt_price: price underflow".into()))?;
        if numerator_1 <= product {
            return Err(EngineError::Arithmetic(
                "get_next_sqrt_price: price underflow".into(),
            ));
        }
        let denominator = numerator_1 - product;
        let next = mul_div_rounding_up(numerator_1, sqrt_p_x96, denominator)?;
        if next > u160_max() {
            return Err(EngineError::Arithmetic(
                "get_next_sqrt_price: result out of U160 range".into(),
            ));
        }
        Ok(next)
    }
}

fn get_next_sqrt_price_from_amount_1_rounding_down(
    sqrt_p_x96: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> EngineResult<U256> {
    let liquidity_u256 = U256::from(liquidity);

    if add {
        let quotient = if amount <= u160_max() {
            if liquidity_u256.is_zero() {
                return Err(EngineError::Arithmetic(
                    "get_next_sqrt_price: liquidity is zero".into(),
                ));
            }
            (amount << 96usize) / liquidity_u256
        } else {
            mul_div(amount, q96(), liquidity_u256)?
        };
        let next = sqrt_p_x96
            .checked_add(quotient)
            .ok_or_else(|| EngineError::Arithmetic("get_next_sqrt_price: out of range".into()))?;
        if next > u160_max() {
            return Err(EngineError::Arithmetic(
                "get_next_sqrt_price: result out of U160 range".into(),
            ));
        }
        Ok(next)
    } else {
        let quotient = if amount <= u160_max() {
            div_rounding_up(amount << 96usize, liquidity_u256)?
        } else {
            mul_div_rounding_up(amount, q96(), liquidity_u256)?
        };
        if sqrt_p_x96 <= quotient {
            return Err(EngineError::Arithmetic(
                "get_next_sqrt_price: price underflow".into(),
            ));
        }
        Ok(sqrt_p_x96 - quotient)
    }
}

/// `SqrtPriceMath.getNextSqrtPriceFromInput`: next sqrt price after adding
/// an exact input amount of token0 (`zero_for_one = true`) or token1
/// (`zero_for_one = false`).
pub fn get_next_sqrt_price_from_input(
    sqrt_p_x96: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> EngineResult<U256> {
    if sqrt_p_x96.is_zero() {
        return Err(EngineError::Arithmetic(
            "get_next_sqrt_price_from_input: sqrt price is zero".into(),
        ));
    }
    if liquidity == 0 {
        return Err(EngineError::Arithmetic(
            "get_next_sqrt_price_from_input: liquidity is zero".into(),
        ));
    }
    if zero_for_one {
        get_next_sqrt_price_from_amount_0_rounding_up(sqrt_p_x96, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount_1_rounding_down(sqrt_p_x96, liquidity, amount_in, true)
    }
}

/// `SqrtPriceMath.getNextSqrtPriceFromOutput`: next sqrt price after
/// removing an exact output amount of token1 (`zero_for_one = true`) or
/// token0 (`zero_for_one = false`).
pub fn get_next_sqrt_price_from_output(
    sqrt_p_x96: U256,
    liquidity: u128,
    amount_out: U256,
    zero_for_one: bool,
) -> EngineResult<U256> {
    if sqrt_p_x96.is_zero() {
        return Err(EngineError::Arithmetic(
            "get_next_sqrt_price_from_output: sqrt price is zero".into(),
        ));
    }
    if liquidity == 0 {
        return Err(EngineError::Arithmetic(
            "get_next_sqrt_price_from_output: liquidity is zero".into(),
        ));
    }
    if zero_for_one {
        get_next_sqrt_price_from_amount_1_rounding_down(sqrt_p_x96, liquidity, amount_out, false)
    } else {
        get_next_sqrt_price_from_amount_0_rounding_up(sqrt_p_x96, liquidity, amount_out, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::tick_math::get_sqrt_ratio_at_tick;

    #[test]
    fn amount_0_simple() {
        let sqrt_a = get_sqrt_ratio_at_tick(0).unwrap();
        let sqrt_b = get_sqrt_ratio_at_tick(100).unwrap();
        let a0 = get_amount_0_delta(sqrt_a, sqrt_b, 1_000_000_000_000_000_000, false).unwrap();
        assert!(a0 > U256::ZERO);
    }

    #[test]
    fn amount_1_simple() {
        let sqrt_a = get_sqrt_ratio_at_tick(0).unwrap();
        let sqrt_b = get_sqrt_ratio_at_tick(100).unwrap();
        let a1 = get_amount_1_delta(sqrt_a, sqrt_b, 1_000_000_000_000_000_000, false).unwrap();
        assert!(a1 > U256::ZERO);
    }

    #[test]
    fn amount_0_round_up_geq_round_down() {
        let sqrt_a = get_sqrt_ratio_at_tick(-100).unwrap();
        let sqrt_b = get_sqrt_ratio_at_tick(100).unwrap();
        let down = get_amount_0_delta(sqrt_a, sqrt_b, 999_999_999, false).unwrap();
        let up = get_amount_0_delta(sqrt_a, sqrt_b, 999_999_999, true).unwrap();
        assert!(up >= down);
    }

    #[test]
    fn amount_1_round_up_geq_round_down() {
        let sqrt_a = get_sqrt_ratio_at_tick(-100).unwrap();
        let sqrt_b = get_sqrt_ratio_at_tick(100).unwrap();
        let down = get_amount_1_delta(sqrt_a, sqrt_b, 999_999_999, false).unwrap();
        let up = get_amount_1_delta(sqrt_a, sqrt_b, 999_999_999, true).unwrap();
        assert!(up >= down);
    }

    #[test]
    fn zero_liquidity_gives_zero_delta() {
        let sqrt_a = get_sqrt_ratio_at_tick(0).unwrap();
        let sqrt_b = get_sqrt_ratio_at_tick(100).unwrap();
        assert_eq!(get_amount_0_delta(sqrt_a, sqrt_b, 0, false).unwrap(), U256::ZERO);
        assert_eq!(get_amount_1_delta(sqrt_a, sqrt_b, 0, false).unwrap(), U256::ZERO);
    }

    #[test]
    fn same_sqrt_price_gives_zero_delta() {
        let sqrt = get_sqrt_ratio_at_tick(42).unwrap();
        assert_eq!(get_amount_0_delta(sqrt, sqrt, 1_000_000, false).unwrap(), U256::ZERO);
        assert_eq!(get_amount_1_delta(sqrt, sqrt, 1_000_000, false).unwrap(), U256::ZERO);
    }

    #[test]
    fn reversed_args_auto_sort_to_same_result() {
        let sqrt_a = get_sqrt_ratio_at_tick(0).unwrap();
        let sqrt_b = get_sqrt_ratio_at_tick(100).unwrap();
        let normal = get_amount_0_delta(sqrt_a, sqrt_b, 10_000_000, false).unwrap();
        let reversed = get_amount_0_delta(sqrt_b, sqrt_a, 10_000_000, false).unwrap();
        assert_eq!(normal, reversed);
    }

    fn price_1() -> U256 {
        U256::from_str_radix("79228162514264337593543950336", 10).unwrap()
    }

    fn price_q96_plus_1e27() -> U256 {
        U256::from_str_radix("80228162514264337593543950336", 10).unwrap()
    }

    /// Reference vector from `wp-evm-amm-math`'s own test suite (itself
    /// claiming cross-validation against an independent oracle) - not a
    /// self-invented expected value.
    #[test]
    fn reference_vector_amount_1_delta_rounding() {
        let l: u128 = 12_345_678_901_234_567_890;
        let down = get_amount_1_delta(price_1(), price_q96_plus_1e27(), l, false).unwrap();
        let up = get_amount_1_delta(price_1(), price_q96_plus_1e27(), l, true).unwrap();
        assert_eq!(down, U256::from(155_824_374_937_533_562_u128));
        assert_eq!(up, U256::from(155_824_374_937_533_563_u128));
    }

    #[test]
    fn reference_vector_amount_0_delta_rounding() {
        let l: u128 = 12_345_678_901_234_567_890;
        let down = get_amount_0_delta(price_1(), price_q96_plus_1e27(), l, false).unwrap();
        let up = get_amount_0_delta(price_1(), price_q96_plus_1e27(), l, true).unwrap();
        assert_eq!(down, U256::from(153_882_109_652_449_556_u128));
        assert_eq!(up, U256::from(153_882_109_652_449_557_u128));
    }

    #[test]
    fn reference_vector_next_sqrt_price_from_input() {
        let l: u128 = 1_000_000_000_000_000_000_000_000;
        let amt = U256::from(1_000_000_000_000_000_u64);
        let zfo = get_next_sqrt_price_from_input(price_1(), l, amt, true).unwrap();
        let ofz = get_next_sqrt_price_from_input(price_1(), l, amt, false).unwrap();
        assert_eq!(
            zfo,
            U256::from_str_radix("79228162435036175158507775178", 10).unwrap()
        );
        assert_eq!(
            ofz,
            U256::from_str_radix("79228162593492500107808287929", 10).unwrap()
        );
        assert!(zfo < price_1() && ofz > price_1());
    }

    #[test]
    fn reference_vector_next_sqrt_price_from_output() {
        let l: u128 = 1_000_000_000_000_000_000_000_000;
        let amt = U256::from(1_000_000_000_000_000_u64);
        let next = get_next_sqrt_price_from_output(price_1(), l, amt, true).unwrap();
        assert_eq!(
            next,
            U256::from_str_radix("79228162435036175079279612742", 10).unwrap()
        );
        assert!(next < price_1());
    }

    #[test]
    fn from_input_zero_amount_returns_input_price() {
        let liq: u128 = 1_000_000_000_000_000_000;
        let next = get_next_sqrt_price_from_input(price_1(), liq, U256::ZERO, true).unwrap();
        assert_eq!(next, price_1());
        let next2 = get_next_sqrt_price_from_input(price_1(), liq, U256::ZERO, false).unwrap();
        assert_eq!(next2, price_1());
    }

    #[test]
    fn from_input_rejects_zero_price() {
        let err = get_next_sqrt_price_from_input(U256::ZERO, 1, U256::from(1u64), true)
            .expect_err("should reject zero price");
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    #[test]
    fn from_input_rejects_zero_liquidity() {
        let err = get_next_sqrt_price_from_input(price_1(), 0, U256::from(1u64), true)
            .expect_err("should reject zero liquidity");
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    #[test]
    fn from_input_zero_for_one_decreases_price() {
        let liq: u128 = 1_000_000_000_000_000_000;
        let amount_in = U256::from(100_000_000_000_000_000u64);
        let next = get_next_sqrt_price_from_input(price_1(), liq, amount_in, true).unwrap();
        assert!(next < price_1());
    }

    #[test]
    fn from_input_one_for_zero_increases_price() {
        let liq: u128 = 1_000_000_000_000_000_000;
        let amount_in = U256::from(100_000_000_000_000_000u64);
        let next = get_next_sqrt_price_from_input(price_1(), liq, amount_in, false).unwrap();
        assert!(next > price_1());
    }

    #[test]
    fn from_output_removing_token0_increases_price() {
        let liq: u128 = 1_000_000_000_000_000_000_000_000u128;
        let amount_out = U256::from(1_000_000u64);
        let next = get_next_sqrt_price_from_output(price_1(), liq, amount_out, false).unwrap();
        assert!(next > price_1());
    }

    #[test]
    fn from_output_removing_token1_decreases_price() {
        let liq: u128 = 1_000_000_000_000_000_000_000_000u128;
        let amount_out = U256::from(1_000_000u64);
        let next = get_next_sqrt_price_from_output(price_1(), liq, amount_out, true).unwrap();
        assert!(next < price_1());
    }

    #[test]
    fn from_output_huge_withdrawal_is_rejected() {
        let huge = U256::from(1u64) << 100usize;
        let result = get_next_sqrt_price_from_output(price_1(), 1, huge, false);
        assert!(result.is_err(), "huge output withdrawal should error, got {result:?}");
    }
}
