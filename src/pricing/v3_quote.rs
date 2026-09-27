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
//! word reads - see [`HydratedTicks`], never guessed or defaulted to "the
//! whole range"). If the swap would need to search past that boundary,
//! [`quote_exact_input`] returns `EngineError::NotImplemented` rather than
//! silently treating unhydrated ticks as uninitialized.
//!
//! **Coupling safety (C1/C2 fix).** `HydratedTicks` bundles
//! `initialized_ticks` together with the `hydrated_tick_lo`/`hi` range that
//! describes it, with all three fields private - the only way to build one
//! outside tests is [`HydratedTicks::new`], and the only production caller
//! of that is `dex::uniswap_v3::UniswapV3Adapter::hydrate_initialized_ticks`,
//! which computes the range directly from the words it actually scanned.
//! `HydratedV3State` in turn holds a `&HydratedTicks` (not the three fields
//! separately), and is itself only constructible via [`HydratedV3State::new`]
//! or [`HydratedV3State::from_pool_state`], both of which validate
//! `current_tick` falls within the hydrated range before returning `Ok`.
//! This makes it impossible, outside of `#[cfg(test)]`, to construct a
//! quote state whose claimed hydration coverage is disconnected from the
//! tick data it actually carries.
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
use crate::market::models::{PoolKind, PoolState};
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

/// Real, hydrated tick data for a bounded range of tick-bitmap words around
/// a pool's current tick - the confirmed-scanned range [`quote_exact_input`]
/// requires to safely reject incomplete state (see module docs). Every
/// entry in `initialized_ticks` comes from a real `ticks()` call on a bit
/// the adapter actually observed set in a real `tickBitmap()` read - never
/// fabricated or interpolated.
///
/// Fields are private: `initialized_ticks` and the `hydrated_tick_lo`/`hi`
/// range that describes it must always come from the same source, or the
/// range claim is meaningless. The only way to build one is [`Self::new`]
/// (used by `dex::uniswap_v3::UniswapV3Adapter::hydrate_initialized_ticks`,
/// which computes the range from the exact words it scanned) or, in test
/// builds only, [`Self::for_test`].
#[derive(Debug, Clone)]
pub struct HydratedTicks {
    initialized_ticks: BTreeMap<i32, i128>,
    /// Inclusive lower bound of the tick range actually confirmed hydrated.
    hydrated_tick_lo: i32,
    /// Inclusive upper bound of the tick range actually confirmed hydrated.
    hydrated_tick_hi: i32,
}

impl HydratedTicks {
    /// Production constructor. Callers are trusted to compute
    /// `hydrated_tick_lo`/`hydrated_tick_hi` from the SAME scan that
    /// populated `initialized_ticks` - this type cannot verify that on its
    /// own (it has no way to re-derive "which words were read" after the
    /// fact), but bundling the three together in one type at least makes it
    /// impossible for a caller to update one without the others, or to
    /// construct a `HydratedV3State` (see below) that mixes ticks from one
    /// hydration with bounds from another.
    pub fn new(
        initialized_ticks: BTreeMap<i32, i128>,
        hydrated_tick_lo: i32,
        hydrated_tick_hi: i32,
    ) -> Self {
        HydratedTicks {
            initialized_ticks,
            hydrated_tick_lo,
            hydrated_tick_hi,
        }
    }

    /// Test-only escape hatch for building synthetic hydrated states
    /// (including deliberately-inconsistent ones, to exercise rejection
    /// paths) without any real RPC data. Does not exist in non-test
    /// builds.
    #[cfg(test)]
    pub fn for_test(
        initialized_ticks: BTreeMap<i32, i128>,
        hydrated_tick_lo: i32,
        hydrated_tick_hi: i32,
    ) -> Self {
        Self::new(initialized_ticks, hydrated_tick_lo, hydrated_tick_hi)
    }

    /// Inclusive lower bound of the tick range actually confirmed
    /// hydrated. Read-only - does not expose `initialized_ticks` itself,
    /// preserving the "fields stay private" invariant this type exists to
    /// enforce (see the struct docs' "Coupling safety" reference).
    pub fn hydrated_tick_lo(&self) -> i32 {
        self.hydrated_tick_lo
    }

    /// Inclusive upper bound of the tick range actually confirmed
    /// hydrated. See [`Self::hydrated_tick_lo`].
    pub fn hydrated_tick_hi(&self) -> i32 {
        self.hydrated_tick_hi
    }

    /// Number of real initialized ticks currently held (each backed by an
    /// actual `ticks()` call on a bit observed set in a real
    /// `tickBitmap()` read - see the struct docs). Read-only - does not
    /// expose the map itself.
    pub fn initialized_tick_count(&self) -> usize {
        self.initialized_ticks.len()
    }
}

/// Real, hydrated Uniswap-V3-shaped pool state needed for a full
/// tick-crossing quote. Deliberately distinct from
/// `PoolKind::ConcentratedLiquidity` because it also carries the
/// confirmed-hydrated tick range, which the stored pool model does not
/// track - see the module docs on why that range is mandatory here.
///
/// All fields are private. The only ways to construct one are
/// [`Self::new`] and [`Self::from_pool_state`], both of which validate
/// `current_tick` against `hydrated`'s own range before returning `Ok` -
/// see the module docs' "Coupling safety" section for why this, rather than
/// public fields, is the actual fix for C1/C2.
#[derive(Debug, Clone)]
pub struct HydratedV3State<'a> {
    sqrt_price_x96: U256,
    current_tick: i32,
    liquidity: u128,
    tick_spacing: i32,
    /// Pool fee in hundredths of a basis point (1e-6), e.g. `3000` for
    /// 0.30% - same convention as `swap_math::compute_swap_step`.
    fee_pips: u32,
    hydrated: &'a HydratedTicks,
}

impl<'a> HydratedV3State<'a> {
    /// Construct a hydrated V3 quote state, validating that `current_tick`
    /// actually falls within `hydrated`'s own confirmed range and that the
    /// range itself is well-formed (`lo <= hi`). This is the ONLY
    /// non-test-only way to build a `HydratedV3State` - every field is
    /// private specifically so a caller cannot bypass these checks via
    /// struct-literal or functional-update syntax.
    pub fn new(
        sqrt_price_x96: U256,
        current_tick: i32,
        liquidity: u128,
        tick_spacing: i32,
        fee_pips: u32,
        hydrated: &'a HydratedTicks,
    ) -> EngineResult<Self> {
        if tick_spacing <= 0 {
            return Err(EngineError::Config(format!(
                "HydratedV3State::new: tick_spacing must be positive, got {tick_spacing}"
            )));
        }
        if hydrated.hydrated_tick_lo > hydrated.hydrated_tick_hi {
            return Err(EngineError::Arithmetic(
                "HydratedV3State::new: hydrated_tick_lo must be <= hydrated_tick_hi".into(),
            ));
        }
        if current_tick < hydrated.hydrated_tick_lo || current_tick > hydrated.hydrated_tick_hi {
            return Err(EngineError::NotImplemented(
                "HydratedV3State::new: pool's current tick lies outside the confirmed-hydrated \
                 tick range - refusing to quote against incomplete tick data"
                    .into(),
            ));
        }
        Ok(HydratedV3State {
            sqrt_price_x96,
            current_tick,
            liquidity,
            tick_spacing,
            fee_pips,
            hydrated,
        })
    }

    /// Build a `HydratedV3State` directly from a real `PoolState` (must be
    /// `PoolKind::ConcentratedLiquidity`) plus its matching `HydratedTicks` -
    /// the seam a real adapter -> quote pipeline uses. See
    /// `dex::uniswap_v3::UniswapV3Adapter::get_pool_state_and_ticks_at_block`
    /// for where a `PoolState`/`HydratedTicks` pair actually comes from
    /// together in this codebase today (Day 4's job is to feed that pair
    /// into this function).
    pub fn from_pool_state(
        pool_state: &PoolState,
        hydrated: &'a HydratedTicks,
    ) -> EngineResult<Self> {
        match &pool_state.pool.kind {
            PoolKind::ConcentratedLiquidity {
                fee_tier,
                tick_spacing,
                sqrt_price_x96,
                current_tick,
                liquidity,
                ..
            } => Self::new(
                *sqrt_price_x96,
                *current_tick,
                *liquidity,
                *tick_spacing,
                *fee_tier,
                hydrated,
            ),
            PoolKind::Aerodrome { .. } => Err(EngineError::Dex {
                dex: "uniswap_v3".into(),
                reason: "HydratedV3State::from_pool_state: pool.kind is Aerodrome, not \
                         ConcentratedLiquidity - wrong pricing path for this pool"
                    .into(),
            }),
        }
    }
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

    if state.hydrated.hydrated_tick_lo > state.hydrated.hydrated_tick_hi {
        return Err(EngineError::Arithmetic(
            "quote_exact_input: hydrated_tick_lo must be <= hydrated_tick_hi".into(),
        ));
    }
    if state.current_tick < state.hydrated.hydrated_tick_lo
        || state.current_tick > state.hydrated.hydrated_tick_hi
    {
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
            &state.hydrated.initialized_ticks,
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
        if next_tick_raw < state.hydrated.hydrated_tick_lo || next_tick_raw > state.hydrated.hydrated_tick_hi {
            return Err(EngineError::NotImplemented(format!(
                "quote_exact_input: swap requires tick data at/beyond {next_tick_raw}, outside \
                 the confirmed-hydrated range [{}, {}] - refusing to fabricate a price past real \
                 hydrated state",
                state.hydrated.hydrated_tick_lo, state.hydrated.hydrated_tick_hi
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
                let liquidity_net = *state.hydrated.initialized_ticks.get(&next_tick).unwrap_or(&0);
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
        hydrated: &'a HydratedTicks,
    ) -> EngineResult<HydratedV3State<'a>> {
        HydratedV3State::new(sqrt_price_x96, current_tick, liquidity, tick_spacing, fee_pips, hydrated)
    }

    // --- HydratedTicks read-only accessor regression tests ---

    /// `hydrated_tick_lo()`/`hydrated_tick_hi()`/`initialized_tick_count()`
    /// must report exactly what `for_test` (i.e. `new`) was constructed
    /// with - both the hydrated bounds and the real count of initialized
    /// ticks held, without exposing `initialized_ticks` itself.
    #[test]
    fn accessors_report_bounds_and_count_for_populated_ticks() {
        let mut ticks: BTreeMap<i32, i128> = BTreeMap::new();
        ticks.insert(-60, 1_000_000);
        ticks.insert(120, -1_000_000);

        let hydrated = HydratedTicks::for_test(ticks, -1_000_000, 1_000_000);

        assert_eq!(hydrated.hydrated_tick_lo(), -1_000_000);
        assert_eq!(hydrated.hydrated_tick_hi(), 1_000_000);
        assert_eq!(hydrated.initialized_tick_count(), 2);
    }

    /// Same accessors against an empty `initialized_ticks` map - a real,
    /// common case (a pinned block with no initialized ticks in the
    /// hydrated word range) - `initialized_tick_count()` must report 0,
    /// not panic or misreport.
    #[test]
    fn initialized_tick_count_is_zero_for_empty_ticks() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        assert_eq!(hydrated.initialized_tick_count(), 0);
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
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &hydrated).unwrap();
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
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &hydrated).unwrap();
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
        let hydrated = HydratedTicks::for_test(ticks, -1_000_000, 1_000_000);
        let sqrt_30 = get_sqrt_ratio_at_tick(30).unwrap();
        let liquidity = 110_000_000_000_000_000_000_000u128; // base(1e22) + L(1e23)
        let s = state(sqrt_30, 30, liquidity, 60, 3000, &hydrated).unwrap();

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
        let hydrated = HydratedTicks::for_test(ticks, -1_000_000, 1_000_000);
        let sqrt_neg30 = get_sqrt_ratio_at_tick(-30).unwrap();
        let liquidity = 110_000_000_000_000_000_000_000u128;
        let s = state(sqrt_neg30, -30, liquidity, 60, 3000, &hydrated).unwrap();

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
        // Narrow: covers current_tick (30) and the -60 crossing, but not
        // the next word the search needs after crossing it.
        let hydrated = HydratedTicks::for_test(ticks, -15_000, 1_000);
        let sqrt_30 = get_sqrt_ratio_at_tick(30).unwrap();
        let liquidity = 110_000_000_000_000_000_000_000u128;
        let s = state(sqrt_30, 30, liquidity, 60, 3000, &hydrated).unwrap();

        let err = quote_exact_input(&s, U256::from(5_000_000_000_000_000_000_000u128), true)
            .expect_err("must reject rather than fabricate a price past hydrated data");
        assert!(
            matches!(err, EngineError::NotImplemented(_)),
            "expected NotImplemented (incomplete state), got {err:?}"
        );
    }

    /// This rejection now happens at `HydratedV3State::new` construction
    /// time (via the `state()` helper), not inside `quote_exact_input` -
    /// the C1/C2 refactor moved this check earlier rather than removing it
    /// (see also the more direct `new_rejects_current_tick_outside_hydrated_bounds`
    /// below, which calls `HydratedV3State::new` without going through this
    /// helper).
    #[test]
    fn current_tick_outside_hydrated_range_is_rejected_immediately() {
        // hydrated range does not even contain current_tick.
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), 200, 300);
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let err = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &hydrated)
            .expect_err("current tick outside hydrated range must be rejected");
        assert!(matches!(err, EngineError::NotImplemented(_)));
    }

    #[test]
    fn zero_amount_in_is_a_no_op_even_with_no_hydrated_data() {
        // Deliberately-invalid hydrated range (doesn't even contain
        // current_tick) - must not matter, since there's nothing to quote.
        // NOTE: this specific combination now fails at construction (see
        // current_tick_outside_hydrated_range_is_rejected_immediately), so
        // this test uses a range that DOES contain current_tick, and relies
        // on quote_exact_input's own zero-amount short-circuit (which
        // returns before consulting the hydrated range at all) rather than
        // the construction-time check to prove the "no hydration data
        // needed for a zero-amount quote" property.
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), 100, 100);
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &hydrated).unwrap();
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
        let hydrated = HydratedTicks::for_test(ticks, -1_000_000, 1_000_000);
        let sqrt_30 = get_sqrt_ratio_at_tick(30).unwrap();
        let s = state(sqrt_30, 30, 10u128, 60, 3000, &hydrated).unwrap();

        let err = quote_exact_input(&s, U256::from(5_000_000_000_000_000_000_000u128), true)
            .expect_err("liquidityNet crossing must not underflow silently");
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    /// This rejection now happens at `HydratedV3State::new` construction
    /// time (via the `state()` helper), not inside `quote_exact_input` -
    /// see also the more direct `new_rejects_inverted_bounds` below.
    #[test]
    fn hydrated_lo_greater_than_hi_is_rejected() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), 100, -100);
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let err = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &hydrated).unwrap_err();
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    #[test]
    fn output_increases_monotonically_with_input_when_uncrossed() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let sqrt_100 = get_sqrt_ratio_at_tick(100).unwrap();
        let mut prev = U256::ZERO;
        for amt in [1_000u64, 1_000_000, 1_000_000_000, 1_000_000_000_000] {
            let s = state(sqrt_100, 100, 10u128.pow(24), 60, 3000, &hydrated).unwrap();
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
        // tick 0 is exactly a word boundary for tick_spacing 60 (256*60 =
        // 15360, and 0 is a multiple of that).
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let sqrt_0 = get_sqrt_ratio_at_tick(0).unwrap();
        let s = state(sqrt_0, 0, 10u128.pow(24), 60, 3000, &hydrated).unwrap();
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
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -10_000_000, 10_000_000);
        let current_tick = MIN_TICK + 5000;
        let sqrt_p = get_sqrt_ratio_at_tick(current_tick).unwrap();
        let s = state(sqrt_p, current_tick, 10u128.pow(18), 60, 3000, &hydrated).unwrap();

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
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -10_000_000, 10_000_000);
        let current_tick = MAX_TICK - 5000;
        let sqrt_p = get_sqrt_ratio_at_tick(current_tick).unwrap();
        let s = state(sqrt_p, current_tick, 10u128.pow(18), 60, 3000, &hydrated).unwrap();

        let huge_amount = U256::from(10u128.pow(38));
        let err = quote_exact_input(&s, huge_amount, false)
            .expect_err("must terminate with an explicit error, not loop forever");
        assert!(
            matches!(err, EngineError::Arithmetic(_)),
            "expected Arithmetic (global bound reached), got {err:?}"
        );
    }

    // --- C1/C2 coupling-safety tests (HydratedTicks / HydratedV3State) ---

    /// Direct test of the construction-time check, independent of the
    /// `state()` helper - mirrors `current_tick_outside_hydrated_range_is_rejected_immediately`
    /// above but calls `HydratedV3State::new` itself for an unambiguous
    /// regression test tied to the exact function this design audit
    /// targeted.
    #[test]
    fn new_rejects_current_tick_outside_hydrated_bounds() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), 200, 300);
        let err = HydratedV3State::new(
            get_sqrt_ratio_at_tick(100).unwrap(),
            100,
            10u128.pow(24),
            60,
            3000,
            &hydrated,
        )
        .expect_err("current_tick outside the hydrated range must be rejected at construction");
        assert!(matches!(err, EngineError::NotImplemented(_)));
    }

    /// Direct test of the construction-time check, independent of the
    /// `state()` helper - mirrors `hydrated_lo_greater_than_hi_is_rejected`
    /// above but calls `HydratedV3State::new` itself.
    #[test]
    fn new_rejects_inverted_bounds() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), 100, -100);
        let err = HydratedV3State::new(
            get_sqrt_ratio_at_tick(100).unwrap(),
            100,
            10u128.pow(24),
            60,
            3000,
            &hydrated,
        )
        .expect_err("hydrated_tick_lo > hydrated_tick_hi must be rejected at construction");
        assert!(matches!(err, EngineError::Arithmetic(_)));
    }

    /// B's audit item: a zero or negative `tick_spacing` must never reach
    /// `tick_bitmap::compress` (which divides by it) - `HydratedV3State`
    /// must reject it at construction with a clean `EngineError`, not let
    /// it flow through to `quote_exact_input`'s tick-walking loop (a
    /// division-by-zero panic for `tick_spacing == 0`, or silently wrong
    /// tick math for a negative one).
    #[test]
    fn new_rejects_zero_tick_spacing() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let err = HydratedV3State::new(
            get_sqrt_ratio_at_tick(0).unwrap(),
            0,
            10u128.pow(24),
            0,
            3000,
            &hydrated,
        )
        .expect_err("tick_spacing == 0 must be rejected at construction");
        assert!(matches!(err, EngineError::Config(_)));
    }

    /// Mirror of `new_rejects_zero_tick_spacing` for a negative
    /// `tick_spacing`, which is equally invalid but a distinct case from
    /// zero (no division-by-zero, but still nonsensical tick math).
    #[test]
    fn new_rejects_negative_tick_spacing() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let err = HydratedV3State::new(
            get_sqrt_ratio_at_tick(0).unwrap(),
            0,
            10u128.pow(24),
            -60,
            3000,
            &hydrated,
        )
        .expect_err("negative tick_spacing must be rejected at construction");
        assert!(matches!(err, EngineError::Config(_)));
    }

    /// Positive counterpart to `new_rejects_zero_tick_spacing`/
    /// `new_rejects_negative_tick_spacing`: the same validation must NOT
    /// reject a normal, valid `tick_spacing` - 60 is the real Uniswap V3
    /// tick spacing for the 0.30% fee tier, already used throughout this
    /// file's other tests (e.g. `zero_progress_single_step_at_word_boundary_still_terminates`).
    /// Construction must succeed.
    #[test]
    fn new_accepts_valid_positive_tick_spacing() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let result = HydratedV3State::new(
            get_sqrt_ratio_at_tick(0).unwrap(),
            0,
            10u128.pow(24),
            60,
            3000,
            &hydrated,
        );
        assert!(
            result.is_ok(),
            "tick_spacing = 60 (a normal, valid value) must be accepted: {result:?}"
        );
    }

    /// Same check, exercised through `from_pool_state` (the real
    /// adapter -> quote seam) rather than calling `HydratedV3State::new`
    /// directly, so the construction-time guard is confirmed reachable
    /// from a real `PoolState` too, not just the lower-level constructor.
    #[test]
    fn from_pool_state_rejects_zero_tick_spacing() {
        use crate::market::models::{DexKind, Pool, Token};
        use alloy::primitives::address;

        let pool_state = PoolState::new(
            Pool {
                address: address!("0000000000000000000000000000000000000001"),
                dex: DexKind::UniswapV3,
                token0: Token {
                    address: address!("4200000000000000000000000000000000000006"),
                    symbol: "WETH".into(),
                    decimals: 18,
                },
                token1: Token {
                    address: address!("0000000000000000000000000000000000000002"),
                    symbol: "USDC".into(),
                    decimals: 6,
                },
                kind: PoolKind::ConcentratedLiquidity {
                    fee_tier: 3000,
                    tick_spacing: 0,
                    sqrt_price_x96: get_sqrt_ratio_at_tick(0).unwrap(),
                    current_tick: 0,
                    liquidity: 10u128.pow(24),
                    initialized_ticks: Default::default(),
                },
            },
            123,
            None,
        );
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let err = HydratedV3State::from_pool_state(&pool_state, &hydrated)
            .expect_err("tick_spacing == 0 via from_pool_state must be rejected at construction");
        assert!(matches!(err, EngineError::Config(_)));
    }

    #[test]
    fn from_pool_state_rejects_aerodrome_pool_kind() {
        use crate::market::models::{DexKind, Pool, Token};
        use alloy::primitives::address;

        let pool_state = PoolState::new(
            Pool {
                address: address!("0000000000000000000000000000000000000001"),
                dex: DexKind::Aerodrome,
                token0: Token {
                    address: address!("4200000000000000000000000000000000000006"),
                    symbol: "WETH".into(),
                    decimals: 18,
                },
                token1: Token {
                    address: address!("0000000000000000000000000000000000000002"),
                    symbol: "USDC".into(),
                    decimals: 6,
                },
                kind: PoolKind::Aerodrome {
                    reserve0: U256::from(1u64),
                    reserve1: U256::from(1u64),
                    stable: false,
                    fee_bps: None,
                },
            },
            123,
            None,
        );
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);

        let err = HydratedV3State::from_pool_state(&pool_state, &hydrated).unwrap_err();
        assert!(matches!(err, EngineError::Dex { .. }));
    }

    #[test]
    fn from_pool_state_extracts_fields_correctly() {
        use crate::market::models::{DexKind, Pool, Token};
        use alloy::primitives::address;

        let sqrt_p = get_sqrt_ratio_at_tick(100).unwrap();
        let pool_state = PoolState::new(
            Pool {
                address: address!("0000000000000000000000000000000000000003"),
                dex: DexKind::UniswapV3,
                token0: Token {
                    address: address!("4200000000000000000000000000000000000006"),
                    symbol: "WETH".into(),
                    decimals: 18,
                },
                token1: Token {
                    address: address!("0000000000000000000000000000000000000002"),
                    symbol: "USDC".into(),
                    decimals: 6,
                },
                kind: PoolKind::ConcentratedLiquidity {
                    fee_tier: 3000,
                    tick_spacing: 60,
                    sqrt_price_x96: sqrt_p,
                    current_tick: 100,
                    liquidity: 10u128.pow(24),
                    initialized_ticks: Default::default(),
                },
            },
            42,
            None,
        );
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);

        // Private-field access below relies on this test module being a
        // descendant of the defining module (`use super::*;` at the top of
        // `mod tests`) - standard Rust visibility, not a special test-only
        // API surface.
        let s = HydratedV3State::from_pool_state(&pool_state, &hydrated).unwrap();
        assert_eq!(s.sqrt_price_x96, sqrt_p);
        assert_eq!(s.current_tick, 100);
        assert_eq!(s.liquidity, 10u128.pow(24));
        assert_eq!(s.tick_spacing, 60);
        assert_eq!(s.fee_pips, 3000);
    }

    /// The seam test (closes C2): real-*shaped* adapter output
    /// (`HydratedTicks`, built here via the test-only constructor since
    /// there is no live Base RPC access in this environment - a real
    /// `hydrate_initialized_ticks()` call would build the same shape) plus
    /// a real `PoolState`, combined via `HydratedV3State::from_pool_state`,
    /// fed straight into `quote_exact_input` - proving the previously-missing
    /// glue between `dex::uniswap_v3`'s hydration output and the pricing
    /// engine actually exists. Reuses the exact same scenario and
    /// hand-verified numbers as
    /// `crossing_initialized_tick_zero_for_one_applies_liquidity_net` above,
    /// so this test's only new claim is that the SAME result is reachable
    /// through the full construction pipeline, not hand-built structs.
    #[test]
    fn seam_hydrated_ticks_to_pool_state_to_quote_exact_input() {
        use crate::market::models::{DexKind, Pool, Token};
        use alloy::primitives::address;

        let mut ticks = BTreeMap::new();
        ticks.insert(-60, 100_000_000_000_000_000_000_000i128);
        ticks.insert(60, -100_000_000_000_000_000_000_000i128);
        let hydrated = HydratedTicks::for_test(ticks, -1_000_000, 1_000_000);

        let pool_state = PoolState::new(
            Pool {
                address: address!("0000000000000000000000000000000000000003"),
                dex: DexKind::UniswapV3,
                token0: Token {
                    address: address!("4200000000000000000000000000000000000006"),
                    symbol: "WETH".into(),
                    decimals: 18,
                },
                token1: Token {
                    address: address!("0000000000000000000000000000000000000002"),
                    symbol: "USDC".into(),
                    decimals: 6,
                },
                kind: PoolKind::ConcentratedLiquidity {
                    fee_tier: 3000,
                    tick_spacing: 60,
                    sqrt_price_x96: get_sqrt_ratio_at_tick(30).unwrap(),
                    current_tick: 30,
                    liquidity: 110_000_000_000_000_000_000_000u128,
                    // Deliberately left empty: real tick data flows through
                    // the separate `HydratedTicks` value, never through
                    // this field (see `PoolKind::ConcentratedLiquidity`
                    // docs) - `from_pool_state` must not (and does not)
                    // read `initialized_ticks` from here.
                    initialized_ticks: Default::default(),
                },
            },
            999,
            None,
        );

        let s = HydratedV3State::from_pool_state(&pool_state, &hydrated).unwrap();
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
        assert_eq!(result.liquidity_after, 10_000_000_000_000_000_000_000u128);
    }
}
