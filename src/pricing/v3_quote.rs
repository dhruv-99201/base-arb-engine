//! Uniswap V3 (and structurally-identical Aerodrome Slipstream) full
//! multi-tick exact-input quote: the tick-crossing swap loop that
//! `swap_math::compute_swap_step` computes one segment of, wired over REAL
//! hydrated tick data (`PoolKind::ConcentratedLiquidity::initialized_ticks`)
//! rather than a single-tick, no-crossing approximation. Day 1/2 never
//! fetched `tickBitmap()`/`ticks()` from chain, so `initialized_ticks` was
//! always empty in practice - this module is the first thing to actually
//! consume real tick data once
//! `dex::uniswap_v3::UniswapV3Adapter::hydrate_initialized_ticks` (Day 3)
//! populates it.
//!
//! **Explicit incomplete-state rejection (required, not optional).** A
//! sparse `initialized_ticks: BTreeMap<i32, i128>` alone cannot distinguish
//! "no initialized tick exists here" from "we never hydrated this region" -
//! both look like an empty range to `tick_bitmap`'s search. Every caller
//! MUST supply the inclusive tick range that was actually confirmed
//! hydrated (`hydrated_tick_lo`/`hydrated_tick_hi`, from real `tickBitmap()`
//! word reads - see `HydratedTicks` in `dex::uniswap_v3`, never guessed or
//! defaulted to "the whole range"). If the swap would need to search past
//! that boundary, [`quote_exact_input`] returns `EngineError::NotImplemented`
//! rather than silently treating unhydrated ticks as uninitialized.
//!
//! **Global tick-range termination (required, not optional).** Even with
//! fully-hydrated data, a large enough exact-input amount can, in
//! principle, walk the price all the way to `MIN_TICK`/`MAX_TICK` - the
//! absolute edges of the representable price range - without fully
//! consuming the input (e.g. against a real but finite pool of liquidity).
//! `tick_bitmap::next_initialized_tick_within_one_word`'s "not found"
//! fallback returns a word-boundary tick that can itself lie outside
//! `[MIN_TICK, MAX_TICK]`; naively clamping it and re-searching from the
//! same clamped boundary on every iteration, forever, is a real bug this
//! module explicitly guards against: once a step's target tick is at or
//! beyond the global bound AND the trade still has unconsumed input left
//! afterward, the loop terminates immediately with an explicit
//! `EngineError::Arithmetic` rather than repeatedly re-targeting the same
//! boundary price. A hard iteration cap (independent of tick crossings)
//! backs this up as defense-in-depth. See
//! `min_tick_boundary_with_unconsumed_input_terminates_with_error` /
//! `max_tick_boundary_with_unconsumed_input_terminates_with_error` below.
//!
//! The tick-crossing algorithm itself (search the current word for the
//! next initialized tick, swap up to it, cross and apply `liquidityNet` if
//! initialized, repeat) is standard Uniswap V3 `swap()` loop structure,
//! reimplemented here in Rust against this codebase's own already-tested
//! primitives (`compute_swap_step`, `next_initialized_tick_within_one_word`,
//! `get_sqrt_ratio_at_tick`) - not a line-for-line port of any Solidity or
//! third-party source.

use crate::error::{EngineError, EngineResult};
use crate::pricing::swap_math::compute_swap_step;
use crate::pricing::tick_bitmap::next_initialized_tick_within_one_word;
use crate::pricing::tick_math::{get_sqrt_ratio_at_tick, get_tick_at_sqrt_ratio, MAX_TICK, MIN_TICK};
use alloy::primitives::{I256, U256};
use std::collections::BTreeMap;

/// Hard cap on tick-CROSSING loop iterations (i.e. iterations where an
/// initialized tick is actually crossed). Real pools essentially never
/// require more than a few dozen crossings for a sanely-sized trade; this
/// exists purely to fail loudly with an `Arithmetic` error instead of
/// looping forever if a caller's hydrated data or accounting has a bug -
/// deliberately far above any realistic single-quote crossing count.
const MAX_TICK_CROSSINGS: u32 = 512;

/// Hard cap on TOTAL loop iterations, crossing or not. Distinct from
/// `MAX_TICK_CROSSINGS`: a step that lands on an uninitialized word
/// boundary advances the search position without counting as a crossing,
/// so a crossings-only cap cannot catch a loop that (due to a bug) keeps
/// re-searching without ever crossing anything. This is the primary
/// backstop for "the quote must never infinite-loop" - the explicit
/// global-tick-bound check below is what should normally terminate such a
/// case immediately, but this cap guarantees termination even if some
/// unforeseen path slips past that check.
const MAX_LOOP_ITERATIONS: u32 = 4096;

/// Real, hydrated Uniswap-V3-shaped pool state needed for a full
/// tick-crossing quote. Deliberately distinct from
/// `PoolKind::ConcentratedLiquidity` because it also carries the
/// confirmed-hydrated tick range, which the stored pool model does not
/// track - see the module docs on why that range is mandatory here.
#[derive(Debug, Clone)]
pub struct HydratedV3State<'a> {
    pub sqrt_price_x96: U256,
    pub current_tick: i32,
    pub liquidity: u128,
    pub tick_spacing: i32,
    /// Pool fee in hundredths of a basis point (1e-6), e.g. `3000` for
    /// 0.30% - same convention as `swap_math::compute_swap_step`.
    pub fee_pips: u32,
    pub initialized_ticks: &'a BTreeMap<i32, i128>,
    /// Inclusive lower bound of the tick range actually confirmed via real
    /// `tickBitmap()` reads. Ticks below this are UNKNOWN, not "empty".
    pub hydrated_tick_lo: i32,
    /// Inclusive upper bound of the tick range actually confirmed via real
    /// `tickBitmap()` reads.
    pub hydrated_tick_hi: i32,
}

/// Result of a full exact-input quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct V3QuoteResult {
    /// The input amount that was quoted. Always equal to the `amount_in`
    /// argument passed to [`quote_exact_input`] - exact-input quotes either
    /// fully consume it (`Ok`) or the call fails outright (`Err`); there is
    /// no partial-fill `Ok` result. Carried on the result anyway so callers
    /// building a trade record don't need to keep the original argument
    /// around separately.
    pub amount_in: U256,
    pub amount_out: U256,
    /// Total of every `SwapStep::fee_amount` accumulated across every step
    /// of the loop (one step per tick-boundary segment) - NOT re-derived
    /// from `amount_in`/`fee_pips` after the fact, since that would silently
    /// diverge from the real per-step fee accounting the moment more than
    /// one step is involved.
    pub fee_paid: U256,
    pub ending_sqrt_price_x96: U256,
    pub ending_tick: i32,
    /// Active liquidity after applying every crossed tick's `liquidityNet`.
    /// Equal to the input `liquidity` when `ticks_crossed == 0`.
    pub liquidity_after: u128,
    pub ticks_crossed: u32,
}

/// `LiquidityMath.addDelta`: apply a signed liquidityNet delta to the
/// active liquidity, rejecting anything that would over/underflow `u128`
/// rather than wrapping - an underflow here means the hydrated tick data is
/// inconsistent with the pool's reported active liquidity (or the position
/// accounting is wrong), which must surface as an error, never a silently
/// wrapped/garbage liquidity value.
fn add_liquidity_delta(liquidity: u128, delta: i128) -> EngineResult<u128> {
    if delta < 0 {
        liquidity.checked_sub(delta.unsigned_abs()).ok_or_else(|| {
            EngineError::Arithmetic(
                "add_liquidity_delta: liquidityNet crossing underflows active liquidity - \
                 hydrated tick data is inconsistent with reported pool liquidity"
                    .into(),
            )
        })
    } else {
        liquidity.checked_add(delta as u128).ok_or_else(|| {
            EngineError::Arithmetic(
                "add_liquidity_delta: liquidityNet crossing overflows u128 liquidity".into(),
            )
        })
    }
}

/// Full exact-input quote across as many ticks as the trade needs to
/// cross, in EITHER direction (`zero_for_one` picks which: `true` =
/// token0 in / token1 out, price decreases; `false` = token1 in / token0
/// out, price increases). Requires REAL hydrated tick data - see the
/// module docs on `hydrated_tick_lo`/`hydrated_tick_hi`, and on why hitting
/// the global `MIN_TICK`/`MAX_TICK` bound with input still unconsumed is an
/// explicit error rather than a loop.
pub fn quote_exact_input(
    state: &HydratedV3State,
    amount_in: U256,
    zero_for_one: bool,
) -> EngineResult<V3QuoteResult> {
    if amount_in.is_zero() {
        return Ok(V3QuoteResult {
            amount_in,
            amount_out: U256::ZERO,
            fee_paid: U256::ZERO,
            ending_sqrt_price_x96: state.sqrt_price_x96,
            ending_tick: state.current_tick,
            liquidity_after: state.liquidity,
            ticks_crossed: 0,
        });
    }

    if state.hydrated_tick_lo > state.hydrated_tick_hi {
        return Err(EngineError::Arithmetic(
            "quote_exact_input: hydrated_tick_lo must be <= hydrated_tick_hi".into(),
        ));
    }
    if state.current_tick < state.hydrated_tick_lo || state.current_tick > state.hydrated_tick_hi {
        return Err(EngineError::NotImplemented(
            "quote_exact_input: pool's current tick lies outside the confirmed-hydrated tick \
             range - refusing to quote against incomplete tick data"
                .into(),
        ));
    }

    let mut remaining = amount_in;
    let mut sqrt_price = state.sqrt_price_x96;
    let mut liquidity = state.liquidity;
    let mut tick = state.current_tick;
    let mut amount_out = U256::ZERO;
    let mut fee_paid = U256::ZERO;
    let mut crossings: u32 = 0;
    let mut iterations: u32 = 0;

    while !remaining.is_zero() {
        iterations += 1;
        if iterations > MAX_LOOP_ITERATIONS {
            return Err(EngineError::Arithmetic(format!(
                "quote_exact_input: exceeded {MAX_LOOP_ITERATIONS} loop iterations without \
                 completing the trade - refusing to loop further"
            )));
        }
        if crossings > MAX_TICK_CROSSINGS {
            return Err(EngineError::Arithmetic(format!(
                "quote_exact_input: exceeded {MAX_TICK_CROSSINGS} tick crossings - refusing to \
                 loop further"
            )));
        }

        // `lte`: search at-or-below the current tick when moving down
        // (zero_for_one), strictly above when moving up - same convention
        // as the real V3 swap loop's own bitmap search.
        let (next_tick_raw, initialized) = next_initialized_tick_within_one_word(
            state.initialized_ticks,
            tick,
            state.tick_spacing,
            zero_for_one,
        );

        // Explicit incomplete-state rejection: the word this search landed
        // in (whether or not it found an initialized tick) must be fully
        // within the confirmed-hydrated range, or the answer can't be
        // trusted - "uninitialized" might only mean "we never read that
        // word's bitmap", not "the real pool has no liquidity change
        // there". See module docs.
        if next_tick_raw < state.hydrated_tick_lo || next_tick_raw > state.hydrated_tick_hi {
            return Err(EngineError::NotImplemented(format!(
                "quote_exact_input: swap requires tick data at/beyond {next_tick_raw}, outside \
                 the confirmed-hydrated range [{}, {}] - refusing to fabricate a price past real \
                 hydrated state",
                state.hydrated_tick_lo, state.hydrated_tick_hi
            )));
        }

        // Explicit GLOBAL tick-range termination: the raw search result
        // itself (before clamping) can lie beyond MIN_TICK/MAX_TICK - the
        // absolute edge of every representable price. Clamping it and
        // continuing to search from the same clamped boundary, forever, is
        // exactly the zero-progress infinite loop this module must never
        // produce - see module docs.
        let hit_global_bound = next_tick_raw <= MIN_TICK || next_tick_raw >= MAX_TICK;
        let next_tick = next_tick_raw.clamp(MIN_TICK, MAX_TICK);
        let sqrt_price_target = get_sqrt_ratio_at_tick(next_tick)?;

        let remaining_signed = I256::try_from(remaining).map_err(|_| {
            EngineError::Arithmetic("quote_exact_input: remaining amount overflows I256".into())
        })?;
        let step = compute_swap_step(
            sqrt_price,
            sqrt_price_target,
            liquidity,
            remaining_signed,
            state.fee_pips,
        )?;

        let consumed = step.amount_in.checked_add(step.fee_amount).ok_or_else(|| {
            EngineError::Arithmetic("quote_exact_input: amount_in + fee_amount overflow".into())
        })?;
        remaining = remaining.checked_sub(consumed).ok_or_else(|| {
            EngineError::Arithmetic(
                "quote_exact_input: step consumed more than the remaining amount".into(),
            )
        })?;
        amount_out = amount_out.checked_add(step.amount_out).ok_or_else(|| {
            EngineError::Arithmetic("quote_exact_input: amount_out overflow".into())
        })?;
        fee_paid = fee_paid.checked_add(step.fee_amount).ok_or_else(|| {
            EngineError::Arithmetic("quote_exact_input: fee_paid overflow".into())
        })?;

        sqrt_price = step.sqrt_ratio_next_x96;

        if sqrt_price == sqrt_price_target {
            // Reached (or crossed) next_tick.
            if initialized {
                let liquidity_net = *state.initialized_ticks.get(&next_tick).unwrap_or(&0);
                // Crossing downward (zero_for_one) applies the negated
                // liquidityNet - same sign convention as real V3: a tick's
                // liquidityNet is defined for crossing it left-to-right
                // (upward).
                let signed_delta = if zero_for_one { -liquidity_net } else { liquidity_net };
                liquidity = add_liquidity_delta(liquidity, signed_delta)?;
                crossings += 1;
            }
            // Never let the tracked tick walk past the global bound, even
            // though the "-1 on crossing down" convention would otherwise
            // push it to MIN_TICK - 1.
            tick = if zero_for_one {
                (next_tick - 1).max(MIN_TICK)
            } else {
                next_tick.min(MAX_TICK)
            };

            if hit_global_bound && !remaining.is_zero() {
                let bound_name = if zero_for_one { "MIN_TICK" } else { "MAX_TICK" };
                return Err(EngineError::Arithmetic(format!(
                    "quote_exact_input: reached the global {bound_name} tick bound with \
                     {remaining} of the input still unconsumed - trade exceeds the \
                     representable price range for this pool's liquidity"
                )));
            }
        } else {
            // Remaining amount was fully consumed before reaching
            // next_tick - recover the exact resulting tick from the final
            // price rather than leaving a stale tick value.
            tick = get_tick_at_sqrt_ratio(sqrt_price)?;
        }
    }

    Ok(V3QuoteResult {
        amount_in,
        amount_out,
        fee_paid,
        ending_sqrt_price_x96: sqrt_price,
        ending_tick: tick,
        liquidity_after: liquidity,
        ticks_crossed: crossings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state<'a>(
        sqrt_price_x96: U256,
        current_tick: i32,
        liquidity: u128,
        tick_spacing: i32,
        fee_pips: u32,
        initialized_ticks: &'a BTreeMap<i32, i128>,
        hydrated_tick_lo: i32,
        hydrated_tick_hi: i32,
    ) -> HydratedV3State<'a> {
        HydratedV3State {
            sqrt_price_x96,
            current_tick,
            liquidity,
            tick_spacing,
            fee_pips,
            initialized_ticks,
            hydrated_tick_lo,
            hydrated_tick_hi,
        }
    }

    /// Independently computed via a pure-Python reimplementation of this
    /// exact call graph (full_math -> sqrt_price_math -> swap_math ->
    /// tick_bitmap -> this loop), cross-checked first against this
    /// codebase's own already-passing `swap_math`/`sqrt_price_math`
    /// reference-vector tests (bit-for-bit match) before being used to
    /// derive the new values below - not invented, and not run through
    /// `cargo test` in this environment (no working toolchain here - see
    /// the Day 3 status report).
    #[test]
    fn no_crossing_partial_fill_zero_for_one() {
        let ticks = BTreeMap::new();
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &ticks, -100_000, 100_000);
        let result = quote_exact_input(&s, U256::from(1_000_000_000_000_000u64), true).unwrap();
        assert_eq!(result.amount_in, U256::from(1_000_000_000_000_000u64));
        assert_eq!(result.amount_out, U256::from(1_007_019_512_097_567u64));
        assert_eq!(result.fee_paid, U256::from(3_000_000_000_000u64));
        assert_eq!(
            result.ending_sqrt_price_x96,
            U256::from_str_radix("79625275346740443236829321878", 10).unwrap()
        );
        assert_eq!(result.liquidity_after, 10u128.pow(24));
        assert_eq!(result.ticks_crossed, 0);
    }

    #[test]
    fn no_crossing_partial_fill_one_for_zero() {
        let ticks = BTreeMap::new();
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &ticks, -100_000, 100_000);
        let result = quote_exact_input(&s, U256::from(1_000_000_000_000_000u64), false).unwrap();
        assert_eq!(result.amount_out, U256::from(987_080_176_775_774u64));
        assert_eq!(result.fee_paid, U256::from(3_000_000_000_000u64));
        assert_eq!(
            result.ending_sqrt_price_x96,
            U256::from_str_radix("79625275505515226823052100708", 10).unwrap()
        );
        assert_eq!(result.ticks_crossed, 0);
    }

    /// A concentrated-liquidity position opened over `[-60, 60]` with
    /// `L = 1e23` on top of `1e22` base liquidity elsewhere - the standard
    /// V3 invariant `liquidityNet(lower) = +L`, `liquidityNet(upper) = -L`.
    /// A large zero_for_one swap crosses the lower tick.
    #[test]
    fn crossing_initialized_tick_zero_for_one_applies_liquidity_net() {
        let mut ticks = BTreeMap::new();
        ticks.insert(-60, 100_000_000_000_000_000_000_000i128); // +L
        ticks.insert(60, -100_000_000_000_000_000_000_000i128); // -L
        let sqrt_30 = get_sqrt_ratio_at_tick(30).unwrap();
        let liquidity = 110_000_000_000_000_000_000_000u128; // base(1e22) + L(1e23)
        let s = state(sqrt_30, 30, liquidity, 60, 3000, &ticks, -1_000_000, 1_000_000);

        let result = quote_exact_input(&s, U256::from(5_000_000_000_000_000_000_000u128), true)
            .unwrap();

        assert_eq!(result.ticks_crossed, 1, "must cross exactly the -60 tick");
        assert_eq!(
            result.amount_out,
            U256::from_str_radix("3577454755470381132569", 10).unwrap()
        );
        assert_eq!(
            result.fee_paid,
            U256::from_str_radix("15000000000000000001", 10).unwrap()
        );
        assert_eq!(
            result.ending_sqrt_price_x96,
            U256::from_str_radix("54565990694647229715418592523", 10).unwrap()
        );
        // Liquidity after crossing -60 downward: base(1e22) - (-L) applied
        // as -liquidityNet(-60), i.e. drops from 1.1e23 back down to 1e22.
        assert_eq!(result.liquidity_after, 10_000_000_000_000_000_000_000u128);
        // Not asserted exactly (depends on get_tick_at_sqrt_ratio's
        // rounding on the final partial step) - just sanity-bounded.
        assert!(result.ending_tick <= -60 && result.ending_tick > -15360);
    }

    /// Mirror of the test above in the opposite direction: same symmetric
    /// position, swap starting from tick -30 upward across tick 60. By the
    /// position's symmetry, `amount_out`/`fee_paid` must match exactly.
    #[test]
    fn crossing_initialized_tick_one_for_zero_applies_liquidity_net() {
        let mut ticks = BTreeMap::new();
        ticks.insert(-60, 100_000_000_000_000_000_000_000i128);
        ticks.insert(60, -100_000_000_000_000_000_000_000i128);
        let sqrt_neg30 = get_sqrt_ratio_at_tick(-30).unwrap();
        let liquidity = 110_000_000_000_000_000_000_000u128;
        let s = state(sqrt_neg30, -30, liquidity, 60, 3000, &ticks, -1_000_000, 1_000_000);

        let result = quote_exact_input(&s, U256::from(5_000_000_000_000_000_000_000u128), false)
            .unwrap();

        assert_eq!(result.ticks_crossed, 1, "must cross exactly the +60 tick");
        assert_eq!(
            result.amount_out,
            U256::from_str_radix("3577454755470381132569", 10).unwrap(),
            "symmetric position must give the same output as the mirrored zero_for_one swap"
        );
        assert_eq!(
            result.fee_paid,
            U256::from_str_radix("15000000000000000001", 10).unwrap()
        );
        assert_eq!(
            result.ending_sqrt_price_x96,
            U256::from_str_radix("115036887546191785667034132993", 10).unwrap()
        );
        assert_eq!(result.liquidity_after, 10_000_000_000_000_000_000_000u128);
    }

    /// The exact same trade as `crossing_initialized_tick_zero_for_one_...`
    /// above, but with the hydrated range narrowed so it covers the -60
    /// crossing but NOT the word the swap would need to search next -
    /// demonstrating the crossing itself still works (data was available
    /// for it) while the swap correctly refuses to continue past real
    /// hydrated state instead of assuming empty/uninitialized ticks.
    #[test]
    fn rejects_when_swap_needs_ticks_beyond_hydrated_range() {
        let mut ticks = BTreeMap::new();
        ticks.insert(-60, 100_000_000_000_000_000_000_000i128);
        ticks.insert(60, -100_000_000_000_000_000_000_000i128);
        let sqrt_30 = get_sqrt_ratio_at_tick(30).unwrap();
        let liquidity = 110_000_000_000_000_000_000_000u128;
        // Narrow: covers current_tick (30) and the -60 crossing, but not
        // the next word the search needs after crossing it.
        let s = state(sqrt_30, 30, liquidity, 60, 3000, &ticks, -15_000, 1_000);

        let err = quote_exact_input(&s, U256::from(5_000_000_000_000_000_000_000u128), true)
            .expect_err("must reject rather than fabricate a price past hydrated data");
        assert!(
            matches!(err, EngineError::NotImplemented(_)),
            "expected NotImplemented (incomplete state), got {err:?}"
        );
    }

    #[test]
    fn current_tick_outside_hydrated_range_is_rejected_immediately() {
        let ticks = BTreeMap::new();
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        // hydrated range does not even contain current_tick.
        let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &ticks, 200, 300);
        let err = quote_exact_input(&s, U256::from(1_000u64), true)
            .expect_err("current tick outside hydrated range must be rejected");
        assert!(matches!(err, EngineError::NotImplemented(_)));
    }

    #[test]
    fn zero_amount_in_is_a_no_op_even_with_no_hydrated_data() {
        let ticks = BTreeMap::new();
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        // Deliberately-invalid hydrated range (doesn't even contain
        // current_tick) - must not matter, since there's nothing to quote.
        let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &ticks, 500, 600);
        let result = quote_exact_input(&s, U256::ZERO, true).unwrap();
        assert_eq!(result.amount_in, U256::ZERO);
        assert_eq!(result.amount_out, U256::ZERO);
        assert_eq!(result.fee_paid, U256::ZERO);
        assert_eq!(result.ending_sqrt_price_x96, sqrt_100);
        assert_eq!(result.ending_tick, 100);
        assert_eq!(result.liquidity_after, 10u128.pow(24));
        assert_eq!(result.ticks_crossed, 0);
    }

    /// Crossing a tick whose `liquidityNet` magnitude exceeds the pool's
    /// currently-active liquidity must error, not silently wrap/underflow -
    /// this indicates hydrated data inconsistent with reported liquidity,
    /// which must never be swallowed into a garbage result.
    #[test]
    fn crossing_tick_with_liquidity_net_exceeding_active_liquidity_errors() {
        let mut ticks = BTreeMap::new();
        ticks.insert(-60, 1_000i128); // far more than the tiny liquidity below
        let sqrt_30 = get_sqrt_ratio_at_tick(30).unwrap();
        let s = state(sqrt_30, 30, 10u128, 60, 3000, &ticks, -1_000_000, 1_000_000);

        let err = quote_exact_input(&s, U256::from(5_000_000_000_000_000_000_000u128), true)
            .expect_err("liquidityNet crossing must not underflow silently");
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    #[test]
    fn hydrated_lo_greater_than_hi_is_rejected() {
        let ticks = BTreeMap::new();
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &ticks, 100, -100);
        let err = quote_exact_input(&s, U256::from(1_000u64), true).unwrap_err();
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    #[test]
    fn output_increases_monotonically_with_input_when_uncrossed() {
        let ticks = BTreeMap::new();
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let mut prev = U256::ZERO;
        for amt in [1_000u64, 1_000_000, 1_000_000_000, 1_000_000_000_000] {
            let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &ticks, -100_000, 100_000);
            let result = quote_exact_input(&s, U256::from(amt), true).unwrap();
            assert!(result.amount_out > prev, "output must strictly increase with input");
            prev = result.amount_out;
        }
    }

    /// A pool whose current tick sits exactly at a tick-bitmap word
    /// boundary produces one genuinely zero-progress step (the search
    /// finds "the current tick itself" as the nearest boundary, so that
    /// step's `compute_swap_step` consumes nothing) - but the loop's
    /// search POSITION still advances (`tick` moves), so it resolves
    /// within a couple of iterations rather than hanging. This is the same
    /// behavior the real V3 swap loop has in this situation - not a bug,
    /// but exactly the kind of "zero consumption this step" case that must
    /// never be confused with the true infinite-loop hazard (repeatedly
    /// re-targeting the SAME clamped boundary - see the two tests below).
    #[test]
    fn zero_progress_single_step_at_word_boundary_still_terminates() {
        let ticks = BTreeMap::new();
        // tick 0 is exactly a word boundary for tick_spacing 60 (256*60 =
        // 15360, and 0 is a multiple of that).
        let sqrt_0 = get_sqrt_ratio_at_tick(0).unwrap();
        let s = state(sqrt_0, 0, 10u128.pow(24), 60, 3000, &ticks, -100_000, 100_000);
        let result = quote_exact_input(&s, U256::from(1_000_000_000_000_000u64), true).unwrap();
        assert_eq!(result.amount_out, U256::from(996_999_999_005_991u64));
        assert_eq!(result.fee_paid, U256::from(3_000_000_000_000u64));
        assert_eq!(result.ticks_crossed, 0);
    }

    /// The critical regression test for the boundary-termination fix: an
    /// astronomically large zero_for_one trade against real but finite
    /// liquidity, far enough from MIN_TICK that reaching it takes only one
    /// step, but not far enough input to be satisfied once there. Before
    /// the fix, `next_tick_raw` (a word-start tick below MIN_TICK) would be
    /// clamped to MIN_TICK every iteration while `tick` walked past
    /// MIN_TICK unboundedly - a de facto infinite loop, since crossings
    /// (which the old iteration cap was keyed on) never advances in this
    /// path. Must now fail fast with an explicit error instead.
    #[test]
    fn min_tick_boundary_with_unconsumed_input_terminates_with_error() {
        let ticks = BTreeMap::new();
        let current_tick = MIN_TICK + 5000;
        let sqrt_p = get_sqrt_ratio_at_tick(current_tick).unwrap();
        let s = state(sqrt_p, current_tick, 10u128.pow(18), 60, 3000, &ticks, -10_000_000, 10_000_000);

        let huge_amount = U256::from(10u128.pow(38));
        let err = quote_exact_input(&s, huge_amount, true)
            .expect_err("must terminate with an explicit error, not loop forever");
        assert!(
            matches!(err, EngineError::Arithmetic(_)),
            "expected Arithmetic (global bound reached), got {err:?}"
        );
    }

    /// Mirror of the MIN_TICK test in the opposite direction (one_for_zero
    /// walking up to MAX_TICK).
    #[test]
    fn max_tick_boundary_with_unconsumed_input_terminates_with_error() {
        let ticks = BTreeMap::new();
        let current_tick = MAX_TICK - 5000;
        let sqrt_p = get_sqrt_ratio_at_tick(current_tick).unwrap();
        let s = state(sqrt_p, current_tick, 10u128.pow(18), 60, 3000, &ticks, -10_000_000, 10_000_000);

        let huge_amount = U256::from(10u128.pow(38));
        let err = quote_exact_input(&s, huge_amount, false)
            .expect_err("must terminate with an explicit error, not loop forever");
        assert!(
            matches!(err, EngineError::Arithmetic(_)),
            "expected Arithmetic (global bound reached), got {err:?}"
        );
    }
}
