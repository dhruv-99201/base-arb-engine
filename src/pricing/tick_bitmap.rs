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
