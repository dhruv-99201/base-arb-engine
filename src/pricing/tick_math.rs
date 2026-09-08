//! Exact Uniswap V3 tick <-> sqrtPriceX96 conversion, ported from the real
//! `TickMath.sol` source at
//! `github.com/Uniswap/v3-core/blob/main/contracts/libraries/TickMath.sol`
//! (every magic constant below was read directly from that source, not
//! recalled from memory).
//!
//! Two structural differences from the Solidity original, both verified
//! against real `ruint`/`alloy_primitives` source rather than assumed:
//!
//! 1. `getTickAtSqrtRatio`'s most-significant-bit search (8 inline-assembly
//!    binary-search blocks in Solidity, needed there purely for gas
//!    efficiency) is replaced with `U256::bit_len()` - semantically
//!    identical, and Rust has no equivalent gas-efficiency constraint to
//!    justify porting the hand-unrolled version.
//! 2. Every place the algorithm right-shifts a value that can be negative
//!    uses `Signed::asr()` (arithmetic/sign-extending shift), never the
//!    `>>` operator - checked directly against `alloy_primitives`'s
//!    `Signed` source, where `Shr`/`>>` performs a **logical** shift on the
//!    raw bit pattern instead (`wrapping_shr` = `self.0 >> rhs`, no sign
//!    extension), unlike Solidity's `int256 >>` which is arithmetic. Using
//!    plain `>>` here would have silently produced wrong tick values for
//!    every negative-tick (price < 1) input - `asr()` is required for
//!    correctness, not a style choice.

use crate::error::{EngineError, EngineResult};
use alloy::primitives::{I256, U256};

pub const MIN_TICK: i32 = -887272;
pub const MAX_TICK: i32 = 887272;

/// `TickMath.MIN_SQRT_RATIO` = `getSqrtRatioAtTick(MIN_TICK)`.
pub fn min_sqrt_ratio() -> U256 {
    U256::from(4295128739u64)
}

/// `TickMath.MAX_SQRT_RATIO` = `getSqrtRatioAtTick(MAX_TICK)`.
pub fn max_sqrt_ratio() -> U256 {
    // 1461446703485210103287273052203988822378723970342
    U256::from_str_radix("1461446703485210103287273052203988822378723970342", 10)
        .expect("MAX_SQRT_RATIO literal is valid")
}

/// The 19 magic Q128.128 multiplication constants from `TickMath.sol`,
/// indexed by which bit of `abs(tick)` they correspond to (bit 1 through
/// bit 19; bit 0's constant is handled separately as the starting value).
/// Each fits in `u128` (all are exactly 32 hex digits in the source).
const RATIO_CONSTANTS: [u128; 19] = [
    0xfff97272373d413259a46990580e213a, // bit 1
    0xfff2e50f5f656932ef12357cf3c7fdcc, // bit 2
    0xffe5caca7e10e4e61c3624eaa0941cd0, // bit 3
    0xffcb9843d60f6159c9db58835c926644, // bit 4
    0xff973b41fa98c081472e6896dfb254c0, // bit 5
    0xff2ea16466c96a3843ec78b326b52861, // bit 6
    0xfe5dee046a99a2a811c461f1969c3053, // bit 7
    0xfcbe86c7900a88aedcffc83b479aa3a4, // bit 8
    0xf987a7253ac413176f2b074cf7815e54, // bit 9
    0xf3392b0822b70005940c7a398e4b70f3, // bit 10
    0xe7159475a2c29b7443b29c7fa6e889d9, // bit 11
    0xd097f3bdfd2022b8845ad8f792aa5825, // bit 12
    0xa9f746462d870fdf8a65dc1f90e061e5, // bit 13
    0x70d869a156d2a1b890bb3df62baf32f7, // bit 14
    0x31be135f97d08fd981231505542fcfa6, // bit 15
    0x09aa508b5b7a84e1c677de54f3e99bc9, // bit 16
    0x5d6af8dedb81196699c329225ee604,   // bit 17 (31 hex digits, still < 2^128)
    0x2216e584f5fa1ea926041bedfe98,     // bit 18
    0x48a170391f7dc42444e8fa2,          // bit 19
];

/// `TickMath.getSqrtRatioAtTick`: exact sqrtPriceX96 for a given tick.
/// Returns the Q64.96 fixed-point square-root price as a `U256` (the value
/// always fits in 160 bits for valid ticks, matching `PoolKind::
/// ConcentratedLiquidity.sqrt_price_x96`'s existing `U256` field type).
pub fn get_sqrt_ratio_at_tick(tick: i32) -> EngineResult<U256> {
    if tick < MIN_TICK || tick > MAX_TICK {
        return Err(EngineError::Arithmetic(format!(
            "get_sqrt_ratio_at_tick: tick {tick} out of bounds [{MIN_TICK}, {MAX_TICK}]"
        )));
    }

    let abs_tick: u32 = tick.unsigned_abs();

    let mut ratio: U256 = if abs_tick & 0x1 != 0 {
        U256::from(0xfffcb933bd6fad37aa2d162d1a594001u128)
    } else {
        U256::from(1u128) << 128usize
    };

    for (i, constant) in RATIO_CONSTANTS.iter().enumerate() {
        let bit = 0x2u32 << i; // 0x2, 0x4, 0x8, ..., 0x80000
        if abs_tick & bit != 0 {
            ratio = (ratio * U256::from(*constant)) >> 128usize;
        }
    }

    if tick > 0 {
        ratio = U256::MAX / ratio;
    }

    // Divide by 1<<32, rounding up, to go from Q128.128 to Q128.96.
    let shifted = ratio >> 32usize;
    let remainder = ratio & ((U256::from(1u64) << 32usize) - U256::from(1u64));
    let sqrt_price_x96 = if remainder.is_zero() {
        shifted
    } else {
        shifted + U256::from(1u64)
    };

    Ok(sqrt_price_x96)
}

/// `TickMath.getTickAtSqrtRatio`: exact tick for a given sqrtPriceX96.
pub fn get_tick_at_sqrt_ratio(sqrt_price_x96: U256) -> EngineResult<i32> {
    let min_ratio = min_sqrt_ratio();
    let max_ratio = max_sqrt_ratio();
    if sqrt_price_x96 < min_ratio || sqrt_price_x96 >= max_ratio {
        return Err(EngineError::Arithmetic(format!(
            "get_tick_at_sqrt_ratio: sqrtPriceX96 {sqrt_price_x96} out of bounds [{min_ratio}, {max_ratio})"
        )));
    }

    let ratio: U256 = sqrt_price_x96 << 32usize;

    // Most-significant-bit index. `ratio` is nonzero here because
    // sqrt_price_x96 >= MIN_SQRT_RATIO > 0. See module docs for why this
    // replaces Solidity's 8-block assembly binary search.
    let msb: i32 = (ratio.bit_len() - 1) as i32;

    let mut r: U256 = if msb >= 128 {
        ratio >> (msb - 127) as usize
    } else {
        ratio << (127 - msb) as usize
    };

    let mut log_2: I256 = i256_from_i128((msb - 128) as i128) << 64u32;

    // 14 iterations, shift amounts 63 down to 50 - matches TickMath.sol's
    // unrolled assembly loop exactly.
    for shift in (50..=63).rev() {
        r = (r * r) >> 127usize;
        let f: U256 = r >> 128usize;
        let f_usize: usize = f.to::<usize>();
        log_2 |= i256_from_i128(f_usize as i128) << (shift as u32);
        r >>= f_usize;
    }

    let log_sqrt10001: I256 = log_2 * i256_from_decimal("255738958999603826347141");

    let tick_low_const = i256_from_decimal("3402992956809132418596140100660247210");
    let tick_hi_const = i256_from_decimal("291339464771989622907027621153398088495");

    // Both of these subtractions/additions can leave a negative value, and
    // both are followed by a right shift - `asr()` is required here, not
    // `>>` (see module docs).
    let tick_low_wide = (log_sqrt10001 - tick_low_const).asr(128);
    let tick_hi_wide = (log_sqrt10001 + tick_hi_const).asr(128);

    let tick_low = i32_from_i256(tick_low_wide)?;
    let tick_hi = i32_from_i256(tick_hi_wide)?;

    let tick = if tick_low == tick_hi {
        tick_low
    } else if get_sqrt_ratio_at_tick(tick_hi)? <= sqrt_price_x96 {
        tick_hi
    } else {
        tick_low
    };

    Ok(tick)
}

fn i32_from_i256(value: I256) -> EngineResult<i32> {
    let as_i128: i128 = value
        .try_into()
        .map_err(|_| EngineError::Arithmetic("tick value does not fit in i128".to_string()))?;
    i32::try_from(as_i128)
        .map_err(|_| EngineError::Arithmetic("tick value does not fit in i32".to_string()))
}

/// Construct an `I256` from an `i128`. Routes through the confirmed
/// `TryFrom<i128> for Signed<BITS, LIMBS>` conversion rather than any
/// narrower-width native-int `TryFrom` impl, whose availability wasn't
/// independently confirmed.
fn i256_from_i128(value: i128) -> I256 {
    I256::try_from(value).expect("i128 always fits in I256")
}

/// Construct a non-negative `I256` from a decimal string. Used for the
/// `TickMath.sol` magic constants that exceed `i128::MAX` as literals
/// (`291339464771989622907027621153398088495` in particular) - parsed as
/// `U256` first (confirmed `FromStr`/`from_str_radix` support, already used
/// elsewhere in this module for `MAX_SQRT_RATIO`), then converted via the
/// confirmed `TryFrom<Uint<BITS, LIMBS>> for Signed<BITS, LIMBS>`.
fn i256_from_decimal(s: &str) -> I256 {
    let magnitude =
        U256::from_str_radix(s, 10).unwrap_or_else(|_| panic!("'{s}' is not a valid decimal U256"));
    I256::try_from(magnitude)
        .unwrap_or_else(|_| panic!("'{s}' does not fit in I256's positive range"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_zero_is_exactly_q96_one() {
        // sqrtRatio at tick 0 must be exactly 2^96 (price == 1.0), a
        // well-known, independently-verifiable invariant of the algorithm
        // that doesn't require any external reference data.
        let sqrt_price = get_sqrt_ratio_at_tick(0).unwrap();
        assert_eq!(sqrt_price, U256::from(1u128) << 96usize);
    }

    #[test]
    fn min_and_max_tick_produce_min_and_max_sqrt_ratio() {
        let at_min = get_sqrt_ratio_at_tick(MIN_TICK).unwrap();
        let at_max = get_sqrt_ratio_at_tick(MAX_TICK).unwrap();
        assert_eq!(at_min, min_sqrt_ratio());
        assert_eq!(at_max, max_sqrt_ratio());
    }

    #[test]
    fn out_of_bounds_ticks_are_rejected() {
        assert!(get_sqrt_ratio_at_tick(MIN_TICK - 1).is_err());
        assert!(get_sqrt_ratio_at_tick(MAX_TICK + 1).is_err());
    }

    #[test]
    fn out_of_bounds_sqrt_ratios_are_rejected() {
        assert!(get_tick_at_sqrt_ratio(min_sqrt_ratio() - U256::from(1u64)).is_err());
        assert!(get_tick_at_sqrt_ratio(max_sqrt_ratio()).is_err()); // exclusive upper bound
    }

    #[test]
    fn positive_tick_gives_larger_sqrt_price_than_negative_tick() {
        let positive = get_sqrt_ratio_at_tick(1000).unwrap();
        let zero = get_sqrt_ratio_at_tick(0).unwrap();
        let negative = get_sqrt_ratio_at_tick(-1000).unwrap();
        assert!(positive > zero);
        assert!(zero > negative);
    }

    #[test]
    fn sqrt_ratio_is_monotonically_increasing_with_tick() {
        let mut prev = get_sqrt_ratio_at_tick(MIN_TICK).unwrap();
        for tick in [-500000, -1000, -1, 0, 1, 1000, 500000, MAX_TICK] {
            let cur = get_sqrt_ratio_at_tick(tick).unwrap();
            assert!(cur > prev, "sqrt ratio must strictly increase with tick");
            prev = cur;
        }
    }

    #[test]
    fn round_trip_tick_to_sqrt_to_tick_is_consistent() {
        // get_tick_at_sqrt_ratio(get_sqrt_ratio_at_tick(t)) must recover a
        // tick whose own sqrt ratio is <= the original (Uniswap's own
        // documented invariant, arising from the rounding direction chosen
        // in getSqrtRatioAtTick).
        //
        // MAX_TICK is deliberately excluded here: get_sqrt_ratio_at_tick
        // (MAX_TICK) == MAX_SQRT_RATIO exactly, and get_tick_at_sqrt_ratio's
        // valid input range is the *half-open* [MIN_SQRT_RATIO,
        // MAX_SQRT_RATIO) - matching real TickMath.sol's own bounds check
        // (`sqrtPriceX96 < MAX_SQRT_RATIO`, strictly). There is intentionally
        // no tick "above" MAX_TICK to return, so MAX_SQRT_RATIO itself is
        // not a valid round-trip input - see the dedicated test below.
        for tick in [MIN_TICK, -887271, -100000, -1, 0, 1, 100000, 887271, MAX_TICK - 1] {
            let sqrt_price = get_sqrt_ratio_at_tick(tick).unwrap();
            let recovered_tick = get_tick_at_sqrt_ratio(sqrt_price).unwrap();
            assert!(
                recovered_tick <= tick,
                "recovered tick {recovered_tick} must be <= original tick {tick}"
            );
            let recovered_sqrt_price = get_sqrt_ratio_at_tick(recovered_tick).unwrap();
            assert!(recovered_sqrt_price <= sqrt_price);
        }
    }

    #[test]
    fn max_sqrt_ratio_is_intentionally_not_a_valid_round_trip_input() {
        // get_sqrt_ratio_at_tick(MAX_TICK) == MAX_SQRT_RATIO exactly, but
        // get_tick_at_sqrt_ratio's valid range excludes MAX_SQRT_RATIO
        // itself (see the note on the test above) - this must error, not
        // panic or silently return a wrong tick.
        let sqrt_price_at_max_tick = get_sqrt_ratio_at_tick(MAX_TICK).unwrap();
        assert_eq!(sqrt_price_at_max_tick, max_sqrt_ratio());
        assert!(get_tick_at_sqrt_ratio(sqrt_price_at_max_tick).is_err());
    }

    #[test]
    fn negative_tick_round_trip_specifically_exercises_asr() {
        // Regression test for the logical-vs-arithmetic-shift bug this
        // module's docs call out: a plain `>>` instead of `asr()` on a
        // negative log_sqrt10001 would corrupt exactly this kind of
        // negative-tick, price-below-one case.
        let sqrt_price = get_sqrt_ratio_at_tick(-200000).unwrap();
        let tick = get_tick_at_sqrt_ratio(sqrt_price).unwrap();
        assert!(tick <= -200000 && tick > -200100, "tick was {tick}");
    }
}
