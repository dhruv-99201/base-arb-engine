# base-arb-engine Day 3 continued: TickBitmap + SqrtPriceMath + SwapMath + Aerodrome volatile quote
# Run from the repository root: C:\Users\Dell\Downloads\base-arb-engine
#   powershell -ExecutionPolicy Bypass -File apply_day3_swapmath.ps1
Write-Host 'Applying Day 3 continuation (TickBitmap/SqrtPriceMath/SwapMath/Aerodrome volatile)...'

# Ensure all target directories exist first.
New-Item -ItemType Directory -Force -Path 'src\pricing' | Out-Null

# ---- src/pricing/mod.rs ----
$content = @'
//! Exact, no-floating-point pricing math for the Day 3 opportunity scanner.
//!
//! **Status: partial.** `full_math`, `tick_math`, `tick_bitmap`,
//! `sqrt_price_math`, `swap_math`, and `aerodrome_volatile` are implemented
//! and tested - the last three adapted from a real, published Rust port
//! (`wp-evm-amm-math`) of Uniswap V3's `SqrtPriceMath.sol`/`SwapMath.sol`,
//! built on the exact same `alloy_primitives` types this codebase uses.
//! Still missing: the full tick-crossing quote loop wiring these together
//! over REAL hydrated tick data (Day 1/2 never fetch `tickBitmap()`/
//! `ticks()` from chain - only `slot0`/`liquidity`), real on-chain golden
//! comparison tests (no live RPC access from this environment), the
//! Aerodrome fee lookup, the cross-DEX opportunity engine, the trade-size
//! ladder, the economic model, the journal, and live dry-run mode - see
//! the Day 3 status report for exactly why and what's next.

pub mod aerodrome_volatile;
pub mod full_math;
pub mod sqrt_price_math;
pub mod swap_math;
pub mod tick_bitmap;
pub mod tick_math;

'@
Set-Content -Path 'src\pricing\mod.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pricing/mod.rs'

# ---- src/pricing/tick_bitmap.rs ----
$content = @'
//! Uniswap V3-compatible initialized-tick lookup, ported from the real
//! `TickBitmap.sol`/`BitMath.sol` source at
//! `github.com/Uniswap/v3-core/blob/main/contracts/libraries/`.
//!
//! Two implementations, deliberately kept separate:
//!
//! - [`next_initialized_tick_within_one_word`] - the **production** lookup,
//!   operating directly over the sparse `BTreeMap<i32, i128>` already used
//!   by `PoolKind::ConcentratedLiquidity.initialized_ticks` (tick ->
//!   liquidityNet). No packed bitmap is stored in memory; word boundaries
//!   are computed arithmetically and the search is bounded to the same
//!   [word_start, word_end] range the real bitmap would confine it to.
//! - [`ReferenceBitmap`] - a literal, densely-packed port of the real
//!   Solidity algorithm (one `U256` per word, flipped bit-by-bit), used
//!   ONLY in this module's differential tests to prove the sparse lookup
//!   above produces identical `(next, initialized)` results across a wide
//!   range of scenarios. Never used by production code.
//!
//! `BitMath.mostSignificantBit`/`leastSignificantBit` are replaced with
//! `U256::bit_len() - 1` / `U256::trailing_zeros()` respectively (both
//! confirmed against real `ruint` source) rather than porting Solidity's
//! binary-search bit-shift blocks - same simplification rationale as
//! `tick_math`'s msb computation.

use alloy::primitives::U256;
use std::collections::{BTreeMap, HashMap};

/// Compress a real tick into "tick spacing units", rounding towards
/// negative infinity - matches `TickBitmap.sol`'s `compressed = tick /
/// tickSpacing; if (tick < 0 && tick % tickSpacing != 0) compressed--;`
/// exactly. Rust's `div_euclid` for a positive divisor already rounds
/// toward negative infinity (floor division), which is the same
/// compensation Solidity's manual `if` performs on top of its
/// truncating-toward-zero `/`.
fn compress(tick: i32, tick_spacing: i32) -> i32 {
    tick.div_euclid(tick_spacing)
}

/// `(wordPos, bitPos)` for a compressed tick. `wordPos = compressed >> 8`
/// (arithmetic/sign-extending shift on a native `i32`, which is exactly
/// floor-division by 256 for negative values too). `bitPos =
/// compressed.rem_euclid(256)` - equivalent to Solidity's `uint8(compressed
/// % 256)`, which for a negative `compressed` relies on reinterpreting a
/// small negative two's-complement remainder as unsigned; `rem_euclid`
/// computes the same non-negative result directly and more clearly.
fn position(compressed: i32) -> (i32, u8) {
    (compressed >> 8, compressed.rem_euclid(256) as u8)
}

/// The production lookup: search for the next initialized tick within the
/// same 256-tick word as `tick` (or the adjacent word when `lte` is
/// false and `tick` sits at a word boundary), matching
/// `TickBitmap.nextInitializedTickWithinOneWord`'s word-bounded search
/// exactly, but reading from a sparse `BTreeMap` of known-initialized ticks
/// instead of a packed bitmap.
///
/// `lte`: search at-or-below `tick` (inclusive of `tick` itself) if `true`;
/// search strictly above `tick` (exclusive) if `false` - same semantics as
/// the real function.
pub fn next_initialized_tick_within_one_word(
    initialized_ticks: &BTreeMap<i32, i128>,
    tick: i32,
    tick_spacing: i32,
    lte: bool,
) -> (i32, bool) {
    let compressed = compress(tick, tick_spacing);

    if lte {
        let (word_pos, _bit_pos) = position(compressed);
        let word_start_tick = (word_pos * 256) * tick_spacing;
        let search_end_tick = compressed * tick_spacing;

        match initialized_ticks
            .range(word_start_tick..=search_end_tick)
            .next_back()
        {
            Some((&found_tick, _)) => (found_tick, true),
            None => (word_start_tick, false),
        }
    } else {
        let next_compressed = compressed + 1;
        let (word_pos, _bit_pos) = position(next_compressed);
        let word_end_tick = (word_pos * 256 + 255) * tick_spacing;
        let search_start_tick = next_compressed * tick_spacing;

        match initialized_ticks
            .range(search_start_tick..=word_end_tick)
            .next()
        {
            Some((&found_tick, _)) => (found_tick, true),
            None => (word_end_tick, false),
        }
    }
}

/// A literal, densely-packed port of `TickBitmap.sol`, used only to
/// differentially test [`next_initialized_tick_within_one_word`] above.
/// One `U256` per 256-tick word, exactly like the real contract's `mapping
/// (int16 => uint256)`.
#[derive(Debug, Default)]
pub struct ReferenceBitmap {
    words: HashMap<i32, U256>,
}

impl ReferenceBitmap {
    pub fn new() -> Self {
        ReferenceBitmap {
            words: HashMap::new(),
        }
    }

    /// `TickBitmap.flipTick`. Panics if `tick` isn't a multiple of
    /// `tick_spacing`, matching the real `require(tick % tickSpacing ==
    /// 0)`.
    pub fn flip_tick(&mut self, tick: i32, tick_spacing: i32) {
        assert_eq!(
            tick.rem_euclid(tick_spacing),
            0,
            "tick must be a multiple of tick_spacing"
        );
        let compressed = compress(tick, tick_spacing);
        let (word_pos, bit_pos) = position(compressed);
        let word = self.words.entry(word_pos).or_insert(U256::ZERO);
        *word ^= U256::from(1u8) << (bit_pos as usize);
    }

    /// `TickBitmap.nextInitializedTickWithinOneWord`, ported directly
    /// (mask construction, `BitMath.mostSignificantBit`/
    /// `leastSignificantBit` substitutions as described in the module
    /// docs).
    pub fn next_initialized_tick_within_one_word(
        &self,
        tick: i32,
        tick_spacing: i32,
        lte: bool,
    ) -> (i32, bool) {
        let compressed = compress(tick, tick_spacing);

        if lte {
            let (word_pos, bit_pos) = position(compressed);
            // "all the 1s at or to the right of bitPos" - special-cased at
            // bit_pos==255 to avoid needing to rely on wrapping-shift
            // semantics at the shift-width boundary (Solidity's version
            // relies on `1<<256` wrapping to 0 under pre-0.8 unchecked
            // arithmetic; we just special-case the equivalent result
            // directly instead of reproducing that reliance).
            let mask = if bit_pos == 255 {
                U256::MAX
            } else {
                (U256::from(1u8) << (bit_pos as usize + 1)) - U256::from(1u8)
            };
            let word = self.words.get(&word_pos).copied().unwrap_or(U256::ZERO);
            let masked = word & mask;
            let initialized = !masked.is_zero();

            let next = if initialized {
                let msb = (masked.bit_len() - 1) as i32;
                (compressed - (bit_pos as i32 - msb)) * tick_spacing
            } else {
                (compressed - bit_pos as i32) * tick_spacing
            };
            (next, initialized)
        } else {
            let next_compressed = compressed + 1;
            let (word_pos, bit_pos) = position(next_compressed);
            // "all the 1s at or to the left of bitPos".
            let mask = !((U256::from(1u8) << (bit_pos as usize)) - U256::from(1u8));
            let word = self.words.get(&word_pos).copied().unwrap_or(U256::ZERO);
            let masked = word & mask;
            let initialized = !masked.is_zero();

            let next = if initialized {
                let lsb = masked.trailing_zeros() as i32;
                (next_compressed + (lsb - bit_pos as i32)) * tick_spacing
            } else {
                (next_compressed + (255 - bit_pos as i32)) * tick_spacing
            };
            (next, initialized)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build both a sparse `BTreeMap` and a `ReferenceBitmap` from the same
    /// set of initialized ticks, so every test below exercises both
    /// implementations identically.
    fn build(ticks: &[i32], tick_spacing: i32) -> (BTreeMap<i32, i128>, ReferenceBitmap) {
        let mut sparse = BTreeMap::new();
        let mut dense = ReferenceBitmap::new();
        for &t in ticks {
            sparse.insert(t, 0i128);
            dense.flip_tick(t, tick_spacing);
        }
        (sparse, dense)
    }

    fn assert_matches(sparse: &BTreeMap<i32, i128>, dense: &ReferenceBitmap, tick: i32, spacing: i32, lte: bool) {
        let sparse_result = next_initialized_tick_within_one_word(sparse, tick, spacing, lte);
        let dense_result = dense.next_initialized_tick_within_one_word(tick, spacing, lte);
        assert_eq!(
            sparse_result, dense_result,
            "mismatch at tick={tick} spacing={spacing} lte={lte}: sparse={sparse_result:?} dense={dense_result:?}"
        );
    }

    #[test]
    fn positive_ticks_spacing_1() {
        let (sparse, dense) = build(&[10, 50, 100, 200], 1);
        for tick in [0, 5, 10, 11, 49, 50, 99, 100, 150, 199, 200, 201, 255] {
            assert_matches(&sparse, &dense, tick, 1, true);
            assert_matches(&sparse, &dense, tick, 1, false);
        }
    }

    #[test]
    fn negative_ticks_spacing_1() {
        let (sparse, dense) = build(&[-200, -100, -50, -10], 1);
        for tick in [-256, -201, -200, -150, -100, -99, -51, -50, -11, -10, -9, 0] {
            assert_matches(&sparse, &dense, tick, 1, true);
            assert_matches(&sparse, &dense, tick, 1, false);
        }
    }

    #[test]
    fn negative_non_multiples_of_tick_spacing() {
        let spacing = 60;
        let (sparse, dense) = build(&[-120, -60, 0, 60], spacing);
        // Query at ticks that are NOT exact multiples of spacing - exactly
        // the case `compress()`'s negative-rounding compensation exists
        // for.
        for tick in [-121, -119, -61, -59, -1, 1, 59, 61, -180, -175] {
            assert_matches(&sparse, &dense, tick, spacing, true);
            assert_matches(&sparse, &dense, tick, spacing, false);
        }
    }

    #[test]
    fn current_tick_itself_is_included_when_searching_downward() {
        let (sparse, dense) = build(&[100], 1);
        let (next, initialized) =
            next_initialized_tick_within_one_word(&sparse, 100, 1, true);
        assert!(initialized);
        assert_eq!(next, 100, "lte search must include the tick itself");
        assert_matches(&sparse, &dense, 100, 1, true);
    }

    #[test]
    fn current_tick_itself_is_excluded_when_searching_upward() {
        let (sparse, dense) = build(&[100], 1);
        let (next, initialized) =
            next_initialized_tick_within_one_word(&sparse, 100, 1, false);
        // Must NOT report tick 100 itself as the answer even though it's
        // initialized - "gt" search is strictly exclusive.
        assert_ne!(next, 100);
        assert!(!initialized, "no other initialized tick above 100 in this word");
        assert_matches(&sparse, &dense, 100, 1, false);
    }

    #[test]
    fn word_boundaries_spacing_1() {
        // Compressed word boundaries are at multiples of 256 for spacing=1.
        let (sparse, dense) = build(&[0, 255, 256, 511, 512], 1);
        for tick in [-1, 0, 1, 254, 255, 256, 257, 510, 511, 512, 513] {
            assert_matches(&sparse, &dense, tick, 1, true);
            assert_matches(&sparse, &dense, tick, 1, false);
        }
    }

    #[test]
    fn empty_words_report_uninitialized_at_word_edge() {
        let (sparse, dense) = build(&[], 1);
        let (next_lte, init_lte) = next_initialized_tick_within_one_word(&sparse, 500, 1, true);
        assert!(!init_lte);
        assert_eq!(next_lte, (500i32 >> 8) * 256); // word start
        assert_matches(&sparse, &dense, 500, 1, true);

        let (next_gt, init_gt) = next_initialized_tick_within_one_word(&sparse, 500, 1, false);
        assert!(!init_gt);
        assert_matches(&sparse, &dense, 500, 1, false);
        let _ = next_gt;
    }

    #[test]
    fn multiple_words_spanning_search() {
        let (sparse, dense) = build(&[-1000, -300, -1, 0, 1, 300, 1000], 1);
        for tick in [-1100, -1000, -500, -300, -1, 0, 1, 300, 900, 1000, 1100] {
            assert_matches(&sparse, &dense, tick, 1, true);
            assert_matches(&sparse, &dense, tick, 1, false);
        }
    }

    #[test]
    fn spacing_10() {
        let spacing = 10;
        let (sparse, dense) = build(&[-500, -100, 0, 100, 500, 12340], spacing);
        for tick in [-600, -500, -101, -100, -99, 0, 99, 100, 101, 12330, 12340, 12350] {
            assert_matches(&sparse, &dense, tick, spacing, true);
            assert_matches(&sparse, &dense, tick, spacing, false);
        }
    }

    #[test]
    fn spacing_60() {
        let spacing = 60;
        let (sparse, dense) = build(&[-6000, -60, 0, 60, 6000], spacing);
        for tick in [-6060, -6000, -61, -60, -59, 0, 59, 60, 61, 5999, 6000, 6060] {
            assert_matches(&sparse, &dense, tick, spacing, true);
            assert_matches(&sparse, &dense, tick, spacing, false);
        }
    }

    #[test]
    fn spacing_200() {
        let spacing = 200;
        let (sparse, dense) = build(&[-20000, -200, 0, 200, 20000], spacing);
        for tick in [-20200, -20000, -201, -200, -199, 0, 199, 200, 201, 19999, 20000, 20200] {
            assert_matches(&sparse, &dense, tick, spacing, true);
            assert_matches(&sparse, &dense, tick, spacing, false);
        }
    }

    #[test]
    fn no_initialized_tick_anywhere_nearby() {
        let (sparse, dense) = build(&[1_000_000], 1);
        for tick in [-500, 0, 500] {
            assert_matches(&sparse, &dense, tick, 1, true);
            assert_matches(&sparse, &dense, tick, 1, false);
        }
    }

    #[test]
    fn flip_tick_rejects_non_multiple_of_spacing() {
        let mut bitmap = ReferenceBitmap::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            bitmap.flip_tick(7, 10);
        }));
        assert!(result.is_err(), "flipping a non-aligned tick must panic");
    }
}

'@
Set-Content -Path 'src\pricing\tick_bitmap.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pricing/tick_bitmap.rs'

# ---- src/pricing/sqrt_price_math.rs ----
$content = @'
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

'@
Set-Content -Path 'src\pricing\sqrt_price_math.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pricing/sqrt_price_math.rs'

# ---- src/pricing/swap_math.rs ----
$content = @'
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

'@
Set-Content -Path 'src\pricing\swap_math.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pricing/swap_math.rs'

# ---- src/pricing/aerodrome_volatile.rs ----
$content = @'
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

'@
Set-Content -Path 'src\pricing\aerodrome_volatile.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pricing/aerodrome_volatile.rs'

Write-Host 'Done. Now run: cargo check'
Write-Host 'Then: cargo test'