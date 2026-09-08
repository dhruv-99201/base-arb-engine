//! Exact `floor(a*b/denominator)` and `ceil(a*b/denominator)`, matching
//! Uniswap V3's `FullMath.sol` (`mulDiv`/`mulDivRoundingUp`) bit-for-bit in
//! result, verified against the real source at
//! `github.com/Uniswap/v3-core/blob/main/contracts/libraries/FullMath.sol`.
//!
//! Solidity only has 256-bit words, so `FullMath.sol` uses an intricate
//! "phantom overflow" trick (mulmod + Chinese Remainder Theorem + a
//! Newton-Raphson modular inverse) to compute a 512-bit intermediate
//! product using only 256-bit operations. Rust doesn't have that
//! constraint: `alloy_primitives::U512` is a real 512-bit integer type, so
//! we can compute `a*b` directly without truncation and divide once - this
//! is mathematically identical to the Solidity result (both compute exact
//! `floor`/`ceil` of the true rational value) with far less surface area
//! for a porting bug than reimplementing the assembly trick would have.

use crate::error::{EngineError, EngineResult};
use alloy::primitives::{U256, U512};

/// `floor(a * b / denominator)`, with the full 512-bit intermediate product
/// (no truncation before the division). Errors if `denominator == 0` or if
/// the result doesn't fit in `U256` (mirrors Solidity's `require` reverts
/// in `FullMath.mulDiv`).
pub fn mul_div(a: U256, b: U256, denominator: U256) -> EngineResult<U256> {
    if denominator.is_zero() {
        return Err(EngineError::Arithmetic("mul_div: division by zero".into()));
    }
    let product = U512::from(a) * U512::from(b);
    let denominator_wide = U512::from(denominator);
    let result = product / denominator_wide;
    narrow_to_u256(result, "mul_div")
}

/// `ceil(a * b / denominator)`. Same overflow/zero-denominator behavior as
/// [`mul_div`].
pub fn mul_div_rounding_up(a: U256, b: U256, denominator: U256) -> EngineResult<U256> {
    if denominator.is_zero() {
        return Err(EngineError::Arithmetic(
            "mul_div_rounding_up: division by zero".into(),
        ));
    }
    let product = U512::from(a) * U512::from(b);
    let denominator_wide = U512::from(denominator);
    let quotient = product / denominator_wide;
    let remainder = product % denominator_wide;

    let result = if remainder.is_zero() {
        quotient
    } else {
        quotient + U512::from(1u8)
    };

    narrow_to_u256(result, "mul_div_rounding_up")
}

/// Narrow a `U512` down to `U256`, erroring (not panicking) if it doesn't
/// fit.
///
/// NOTE: `U256::try_from(u512_value)` looks like the obvious spelling here
/// but does NOT compile - `ruint`/`alloy_primitives` only implement
/// `TryFrom` between a `Uint` and native integer types (`u64`, `u128`,
/// etc), not generically between two arbitrary `Uint<BITS, LIMBS>` widths
/// (confirmed by an actual compiler error, not assumed). The generic
/// Uint-to-Uint conversion only exists via the internal `UintTryTo`
/// "workaround" trait (ruint's own doc comment on it ironically says "use
/// TryFrom instead", which doesn't work here). Rather than depend on that
/// trait's import path, this does the overflow check explicitly and then
/// uses the already-proven-working `Uint::to::<T>()` (used elsewhere in
/// this codebase, e.g. for `fee.to::<u32>()`), which is safe here because
/// the check above guarantees it won't hit its internal panic path.
fn narrow_to_u256(value: U512, context: &str) -> EngineResult<U256> {
    let max_u256_as_u512 = U512::from(U256::MAX);
    if value > max_u256_as_u512 {
        return Err(EngineError::Arithmetic(format!(
            "{context}: result overflows U256"
        )));
    }
    Ok(value.to::<U256>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mul_div_basic_exact_division() {
        // 10 * 10 / 5 = 20, exact.
        let result = mul_div(U256::from(10u64), U256::from(10u64), U256::from(5u64)).unwrap();
        assert_eq!(result, U256::from(20u64));
    }

    #[test]
    fn mul_div_floors_inexact_division() {
        // 7 * 3 / 2 = 21 / 2 = 10.5 -> floors to 10.
        let result = mul_div(U256::from(7u64), U256::from(3u64), U256::from(2u64)).unwrap();
        assert_eq!(result, U256::from(10u64));
    }

    #[test]
    fn mul_div_rounding_up_ceils_inexact_division() {
        // 7 * 3 / 2 = 21 / 2 = 10.5 -> ceils to 11.
        let result =
            mul_div_rounding_up(U256::from(7u64), U256::from(3u64), U256::from(2u64)).unwrap();
        assert_eq!(result, U256::from(11u64));
    }

    #[test]
    fn mul_div_rounding_up_matches_floor_on_exact_division() {
        let floor = mul_div(U256::from(100u64), U256::from(4u64), U256::from(8u64)).unwrap();
        let ceil =
            mul_div_rounding_up(U256::from(100u64), U256::from(4u64), U256::from(8u64)).unwrap();
        assert_eq!(floor, ceil, "exact division must round the same either way");
        assert_eq!(floor, U256::from(50u64));
    }

    #[test]
    fn mul_div_zero_denominator_is_rejected() {
        let err = mul_div(U256::from(1u64), U256::from(1u64), U256::ZERO).unwrap_err();
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    #[test]
    fn mul_div_rounding_up_zero_denominator_is_rejected() {
        let err = mul_div_rounding_up(U256::from(1u64), U256::from(1u64), U256::ZERO).unwrap_err();
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    #[test]
    fn mul_div_handles_intermediate_overflow_of_u256() {
        // a * b here vastly exceeds U256::MAX, but the final quotient fits -
        // this is exactly the "phantom overflow" case FullMath exists for.
        // U256::MAX * U256::MAX / U256::MAX == U256::MAX.
        let max = U256::MAX;
        let result = mul_div(max, max, max).unwrap();
        assert_eq!(result, max);
    }

    #[test]
    fn mul_div_zero_numerator_is_zero() {
        let result = mul_div(U256::ZERO, U256::from(12345u64), U256::from(7u64)).unwrap();
        assert_eq!(result, U256::ZERO);
        let result_up =
            mul_div_rounding_up(U256::ZERO, U256::from(12345u64), U256::from(7u64)).unwrap();
        assert_eq!(result_up, U256::ZERO);
    }

    #[test]
    fn mul_div_result_overflowing_u256_is_rejected() {
        // U256::MAX * 2 / 1 overflows U256 - must error, not wrap/panic.
        let err = mul_div(U256::MAX, U256::from(2u64), U256::from(1u64)).unwrap_err();
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }
}
