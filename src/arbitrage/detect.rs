//! M6D: read-only, two-leg cross-DEX opportunity detection - the
//! integration seam connecting M6A (`HistoricalQuoteInput`), M6B
//! (`quote_historical_candidates_at_block`), M6C
//! (`enumerate_cross_dex_routes_at_block`), and M4
//! (`Opportunity::evaluate`). This module writes no new pricing math, no
//! new validation, and no new route logic - it only sequences existing,
//! already-tested functions and records what happened.
//!
//! For a requested starting token and input amount: enumerate every
//! structurally valid route (M6C), keep only the routes that actually
//! start with the requested token, quote leg 1, feed its real output
//! into leg 2 as both `token_in` and `amount_in`, and pass both real
//! `LegQuote` values to `Opportunity::evaluate`. Every stage's real
//! result - success or failure - is recorded; nothing is fabricated,
//! substituted, or silently dropped.
//!
//! **Not a trading signal.** `Opportunity::gross_profit()` is gross, not
//! net - no gas, slippage, or execution cost is subtracted anywhere in
//! this pipeline, and no profitability threshold is applied. A positive
//! result here is not a claim that the route is executable or reliably
//! profitable. **Provenance limitation, inherited unchanged from
//! M6A/M6B/M6C:** a candidate's recorded block is `PoolState` metadata,
//! not independent proof the underlying data was read from the chain at
//! that block, and `HydratedTicks` still has no block field of its own.
//! This module adds no new guarantee on either point.

use crate::arbitrage::multi_quote::{quote_historical_candidates_at_block, HistoricalQuoteInput};
use crate::arbitrage::opportunity::{LegQuote, Opportunity};
use crate::arbitrage::routes::{enumerate_cross_dex_routes_at_block, EnumeratedRoute};
use crate::error::{EngineError, EngineResult};
use alloy::primitives::{Address, U256};

/// Why evaluating one route did not produce an `Opportunity`. Each
/// variant retains an `EngineError` describing the failure and any real
/// quote data already produced - never collapsed into an opaque string.
#[derive(Debug)]
pub enum RouteEvaluationFailure {
    /// `route`'s `candidate_index()`/`dex()`/`pool_address()` could not be
    /// resolved against the `candidates` slice this function was called
    /// with (an out-of-bounds index, or a resolved candidate whose real
    /// dex/pool address disagrees with what the route claims). This is an
    /// internal route/candidate bookkeeping problem, not a pricing
    /// failure - it should not happen for a `candidates` slice identical
    /// to the one `route` was enumerated from, but is checked rather than
    /// assumed. `error` is an `EngineError::State` describing the mismatch.
    InvalidRouteCandidateMapping { error: EngineError },
    /// Leg 1's quote failed. `error` is the real, unmodified error from
    /// `quote_historical_candidates_at_block` (either its own top-level
    /// error, or the one-candidate outcome's own `result`).
    Leg1QuoteFailed { error: EngineError },
    /// Leg 1 succeeded; leg 2's quote failed. `leg1` is the real,
    /// successfully computed first leg, retained so the caller can see
    /// how far evaluation got before it failed.
    Leg2QuoteFailed { leg1: LegQuote, error: EngineError },
    /// Both legs quoted successfully, but `Opportunity::evaluate` itself
    /// rejected the pair (for example a block or token-chain
    /// inconsistency). Both real quotes are retained.
    OpportunityEvaluationFailed {
        leg1: LegQuote,
        leg2: LegQuote,
        error: EngineError,
    },
}

/// One selected route (an [`EnumeratedRoute`] whose `starting_token()`
/// matched the caller's requested `token_in`) and the real result of
/// evaluating it. Fields are private - the only way to obtain one is
/// through [`detect_cross_dex_opportunities_at_block`], matching
/// `Opportunity`/`EnumeratedRoute`'s own construction discipline. The
/// association with the original route is kept even when evaluation
/// failed.
#[derive(Debug)]
pub struct RouteOutcome {
    route: EnumeratedRoute,
    result: Result<Opportunity, RouteEvaluationFailure>,
}

impl RouteOutcome {
    pub fn route(&self) -> EnumeratedRoute {
        self.route
    }

    pub fn result(&self) -> &Result<Opportunity, RouteEvaluationFailure> {
        &self.result
    }

    pub fn opportunity(&self) -> Option<&Opportunity> {
        self.result.as_ref().ok()
    }

    pub fn failure(&self) -> Option<&RouteEvaluationFailure> {
        self.result.as_ref().err()
    }
}

/// Detect every structurally valid cross-DEX round trip starting with
/// `token_in`, quote both legs for `amount_in` (in `token_in`'s smallest
/// units), and evaluate each through the existing `Opportunity::evaluate`.
///
/// Request-level rejection (before any route work):
/// - `amount_in.is_zero()` -> [`EngineError::Arithmetic`]
///
/// Collection-level rejection, propagated unmodified from
/// [`enumerate_cross_dex_routes_at_block`] (a block mismatch, an
/// ambiguous pool, or a duplicate pool address anywhere in `candidates`
/// fails the WHOLE call - never silently drops the offending candidate):
/// - [`EngineError::State`]
///
/// Everything else is per-route: every route whose `starting_token()`
/// equals `token_in`, in M6C's own enumeration order, produces exactly
/// one [`RouteOutcome`] - success or [`RouteEvaluationFailure`] - and one
/// route's failure never removes another route's outcome from the
/// result. A request with no candidates, or with candidates that produce
/// no route starting with `token_in`, returns `Ok(vec![])`.
pub fn detect_cross_dex_opportunities_at_block(
    block: u64,
    token_in: Address,
    amount_in: U256,
    candidates: &[HistoricalQuoteInput],
) -> EngineResult<Vec<RouteOutcome>> {
    if amount_in.is_zero() {
        return Err(EngineError::Arithmetic(
            "detect_cross_dex_opportunities_at_block: amount_in is zero".into(),
        ));
    }

    let routes = enumerate_cross_dex_routes_at_block(block, candidates)?;

    Ok(routes
        .into_iter()
        .filter(|route| route.starting_token() == token_in)
        .map(|route| evaluate_one_route(block, amount_in, candidates, route))
        .collect())
}

/// Quote both legs of `route` (via the existing
/// `quote_historical_candidates_at_block`, each called with a one-item
/// slice containing only the relevant candidate) and evaluate the result
/// through `Opportunity::evaluate`. Never panics and never returns an
/// `Err` itself - every failure is captured inside the returned
/// `RouteOutcome`, so a caller folding this over many routes never loses
/// a later route's outcome to an earlier route's failure.
fn evaluate_one_route(
    block: u64,
    amount_in: U256,
    candidates: &[HistoricalQuoteInput],
    route: EnumeratedRoute,
) -> RouteOutcome {
    let leg1_idx = route.leg1().candidate_index();
    let leg2_idx = route.leg2().candidate_index();

    let leg1_candidate = match resolve_candidate(candidates, leg1_idx, route.leg1()) {
        Ok(candidate) => candidate,
        Err(failure) => {
            return RouteOutcome {
                route,
                result: Err(failure),
            }
        }
    };
    let leg2_candidate = match resolve_candidate(candidates, leg2_idx, route.leg2()) {
        Ok(candidate) => candidate,
        Err(failure) => {
            return RouteOutcome {
                route,
                result: Err(failure),
            }
        }
    };

    let leg1 = match quote_single_candidate(
        block,
        route.starting_token(),
        amount_in,
        leg1_candidate,
    ) {
        Ok(leg_quote) => leg_quote,
        Err(error) => {
            return RouteOutcome {
                route,
                result: Err(RouteEvaluationFailure::Leg1QuoteFailed { error }),
            }
        }
    };

    let leg2 = match quote_single_candidate(
        block,
        leg1.token_out,
        leg1.amount_out,
        leg2_candidate,
    ) {
        Ok(leg_quote) => leg_quote,
        Err(error) => {
            return RouteOutcome {
                route,
                result: Err(RouteEvaluationFailure::Leg2QuoteFailed { leg1, error }),
            }
        }
    };

    let result = match Opportunity::evaluate(leg1, leg2) {
        Ok(opportunity) => Ok(opportunity),
        Err(error) => Err(RouteEvaluationFailure::OpportunityEvaluationFailed {
            leg1,
            leg2,
            error,
        }),
    };

    RouteOutcome { route, result }
}

/// Safe (never-panicking) lookup of the candidate a route leg claims,
/// plus a consistency check that the resolved candidate's real dex/pool
/// address actually match what the leg descriptor says.
fn resolve_candidate(
    candidates: &[HistoricalQuoteInput],
    index: usize,
    leg: crate::arbitrage::routes::RouteLeg,
) -> Result<&HistoricalQuoteInput, RouteEvaluationFailure> {
    let candidate = candidates.get(index).ok_or_else(|| {
        RouteEvaluationFailure::InvalidRouteCandidateMapping {
            error: EngineError::State(format!(
                "candidate_index {index} is out of bounds for a candidates slice of length {}",
                candidates.len()
            )),
        }
    })?;

    let pool = &candidate.pool_state().pool;
    if pool.dex != leg.dex() || pool.address != leg.pool_address() {
        return Err(RouteEvaluationFailure::InvalidRouteCandidateMapping {
            error: EngineError::State(format!(
                "candidate_index {index} resolves to pool {} (dex {:?}), but the route leg \
                 claims pool {} (dex {:?})",
                pool.address,
                pool.dex,
                leg.pool_address(),
                leg.dex()
            )),
        });
    }

    Ok(candidate)
}

/// Quote exactly one candidate via the existing
/// `quote_historical_candidates_at_block`, called with a one-item slice.
/// Checks both failure levels that function can return - its own
/// top-level `EngineResult`, and the single outcome's own `result` - and
/// defensively handles the (should-never-happen-but-checked) case where
/// it returns zero outcomes for a one-item input.
fn quote_single_candidate(
    block: u64,
    token_in: Address,
    amount_in: U256,
    candidate: &HistoricalQuoteInput,
) -> EngineResult<LegQuote> {
    let one = std::slice::from_ref(candidate);
    let outcomes = quote_historical_candidates_at_block(block, token_in, amount_in, one)?;
    match outcomes.into_iter().next() {
        Some(outcome) => outcome.result,
        None => Err(EngineError::State(
            "quote_single_candidate: quote_historical_candidates_at_block returned no outcome \
             for a one-item candidate slice"
                .into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrage::historical::{HistoricalAerodromeInput, HistoricalUniswapV3Input};
    use crate::market::models::{DexKind, Pool, PoolKind, PoolState, Token};
    use crate::pricing::v3_quote::HydratedTicks;
    use alloy::primitives::{address, I256};
    use std::collections::BTreeMap;

    const FIXTURE_BLOCK: u64 = 26_000_000;

    fn weth() -> Address {
        address!("4200000000000000000000000000000000000006")
    }
    fn usdc() -> Address {
        address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913")
    }
    fn dai() -> Address {
        address!("50c5725949A6F0c72E6C4a641F24049A917DB0Cb")
    }

    fn token(addr: Address, symbol: &str, decimals: u8) -> Token {
        Token {
            address: addr,
            symbol: symbol.into(),
            decimals,
        }
    }

    fn aerodrome_candidate(
        block: u64,
        pool_address: Address,
        token0: Address,
        token1: Address,
        fee_bps: Option<U256>,
    ) -> HistoricalQuoteInput {
        let pool = Pool {
            address: pool_address,
            dex: DexKind::Aerodrome,
            token0: token(token0, "T0", 18),
            token1: token(token1, "T1", 6),
            kind: PoolKind::Aerodrome {
                reserve0: U256::from_str_radix("3607642796485591444113", 10).unwrap(),
                reserve1: U256::from(9_965_914_277_780u64),
                stable: false,
                fee_bps,
            },
        };
        let pool_state = PoolState::new(pool, block, None);
        HistoricalQuoteInput::Aerodrome(HistoricalAerodromeInput::new(block, pool_state).unwrap())
    }

    fn v3_candidate(
        block: u64,
        pool_address: Address,
        token0: Address,
        token1: Address,
    ) -> HistoricalQuoteInput {
        let pool = Pool {
            address: pool_address,
            dex: DexKind::UniswapV3,
            token0: token(token0, "T0", 18),
            token1: token(token1, "T1", 6),
            kind: PoolKind::ConcentratedLiquidity {
                fee_tier: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::from_str_radix("4166855781027983759429743", 10).unwrap(),
                current_tick: -197_069,
                liquidity: 2_241_227_707_498_366_949u128,
                initialized_ticks: Default::default(),
            },
        };
        let pool_state = PoolState::new(pool, block, None);
        let mut ticks = BTreeMap::new();
        ticks.insert(-197_070, -1_776_514_264_016_992i128);
        let hydrated = HydratedTicks::for_test(ticks, -199_680, -192_010);
        HistoricalQuoteInput::UniswapV3(
            HistoricalUniswapV3Input::new(block, pool_state, hydrated).unwrap(),
        )
    }

    fn aero_pool() -> Address {
        address!("cDAC0d6c6C59727a65F871236188350531885C43")
    }
    fn v3_pool() -> Address {
        address!("d0b53D9277642d899DF5C87A3966A349A798F224")
    }

    /// Items 1, 2, 3, 4, 5: a real Aerodrome pool and a real V3 pool, same
    /// pair, same block. Requesting `token_in = WETH` selects exactly the
    /// 2 (of 4 enumerated) routes that start with WETH - one with
    /// Aerodrome as leg 1, one with Uniswap V3 as leg 1 - both DEX
    /// orderings through the identical code path. Both must succeed, with
    /// exact amount chaining and a closed, same-block cycle.
    #[test]
    fn full_pipeline_succeeds_both_dex_orderings_for_requested_direction() {
        // 1e15: the existing V3 WETH->USDC golden-fixture amount, so leg
        // 1 succeeds deterministically regardless of which DEX ends up
        // first.
        let amount_in = U256::from(1_000_000_000_000_000u64);
        let candidates = vec![
            aerodrome_candidate(
                FIXTURE_BLOCK,
                aero_pool(),
                weth(),
                usdc(),
                Some(U256::from(30u64)),
            ),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];

        let outcomes =
            detect_cross_dex_opportunities_at_block(FIXTURE_BLOCK, weth(), amount_in, &candidates)
                .unwrap();

        assert_eq!(outcomes.len(), 2);

        let mut saw_aerodrome_first = false;
        let mut saw_v3_first = false;

        for outcome in &outcomes {
            assert_eq!(outcome.route().starting_token(), weth());
            let opportunity = outcome
                .opportunity()
                .unwrap_or_else(|| panic!("expected success, got {:?}", outcome.failure()));

            assert_eq!(opportunity.input_token(), weth());
            assert_eq!(opportunity.final_token(), weth());
            assert_eq!(opportunity.intermediate_token(), usdc());
            assert_eq!(opportunity.input_amount(), amount_in);
            assert_eq!(opportunity.block(), FIXTURE_BLOCK);
            // Amount chaining (item 4): leg2's real input is exactly
            // leg1's real output.
            assert_eq!(opportunity.leg1().amount_out, opportunity.leg2().amount_in);
            assert_eq!(opportunity.leg1().block, FIXTURE_BLOCK);
            assert_eq!(opportunity.leg2().block, FIXTURE_BLOCK);

            match opportunity.leg1().dex {
                DexKind::Aerodrome => saw_aerodrome_first = true,
                DexKind::UniswapV3 => saw_v3_first = true,
                other => panic!("unexpected dex {other:?}"),
            }
        }
        assert!(saw_aerodrome_first, "expected one route with Aerodrome as leg 1");
        assert!(saw_v3_first, "expected one route with Uniswap V3 as leg 1");
    }

    /// Item 6: each outcome's route still carries the correct candidate
    /// indices and pool references back to the original slice.
    #[test]
    fn route_association_matches_original_candidates() {
        let amount_in = U256::from(1_000_000_000_000_000u64);
        let candidates = vec![
            aerodrome_candidate(
                FIXTURE_BLOCK,
                aero_pool(),
                weth(),
                usdc(),
                Some(U256::from(30u64)),
            ),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];

        let outcomes =
            detect_cross_dex_opportunities_at_block(FIXTURE_BLOCK, weth(), amount_in, &candidates)
                .unwrap();

        for outcome in &outcomes {
            let route = outcome.route();
            for leg in [route.leg1(), route.leg2()] {
                let original = &candidates[leg.candidate_index()].pool_state().pool;
                assert_eq!(leg.dex(), original.dex);
                assert_eq!(leg.pool_address(), original.address);
            }
        }
    }

    /// Item 7: with two Aerodrome pools (one with a genuine fee, one
    /// unhydrated) and one V3 pool on the same pair, the unhydrated pool's
    /// routes fail while the genuinely-fee'd pool's routes still succeed
    /// in the SAME call.
    #[test]
    fn per_route_failure_does_not_suppress_other_route_success() {
        let bad_pool = address!("0000000000000000000000000000000000000091");
        let amount_in = U256::from(1_000_000_000_000_000u64);
        let candidates = vec![
            aerodrome_candidate(
                FIXTURE_BLOCK,
                aero_pool(),
                weth(),
                usdc(),
                Some(U256::from(30u64)),
            ),
            aerodrome_candidate(FIXTURE_BLOCK, bad_pool, weth(), usdc(), None), // unhydrated fee
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];

        let outcomes =
            detect_cross_dex_opportunities_at_block(FIXTURE_BLOCK, weth(), amount_in, &candidates)
                .unwrap();

        // 2 eligible pairs (good-aerodrome/v3, bad-aerodrome/v3), each
        // contributing 2 WETH-starting routes = 4 outcomes.
        assert_eq!(outcomes.len(), 4);

        let (ok_count, failed_count) = outcomes.iter().fold((0, 0), |(ok, bad), outcome| {
            if outcome.opportunity().is_some() {
                (ok + 1, bad)
            } else {
                (ok, bad + 1)
            }
        });
        assert_eq!(ok_count, 2, "the genuinely-fee'd pool's 2 routes must succeed");
        assert_eq!(failed_count, 2, "the unhydrated pool's 2 routes must fail");

        // Every failure must be traceable to the unhydrated-fee pool, not
        // some unrelated problem.
        for outcome in &outcomes {
            if let Some(failure) = outcome.failure() {
                assert!(
                    matches!(
                        failure,
                        RouteEvaluationFailure::Leg1QuoteFailed { .. }
                            | RouteEvaluationFailure::Leg2QuoteFailed { .. }
                    ),
                    "unexpected failure kind: {failure:?}"
                );
            }
        }
    }

    /// Item 8: zero input amount is rejected before any route work.
    #[test]
    fn zero_amount_in_is_rejected_at_request_level() {
        let candidates = vec![
            aerodrome_candidate(
                FIXTURE_BLOCK,
                aero_pool(),
                weth(),
                usdc(),
                Some(U256::from(30u64)),
            ),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];
        let err =
            detect_cross_dex_opportunities_at_block(FIXTURE_BLOCK, weth(), U256::ZERO, &candidates)
                .unwrap_err();
        assert!(matches!(err, EngineError::Arithmetic(_)), "got {err:?}");
    }

    /// Item 9 (a): a candidate's block mismatch is the existing M6C
    /// collection-level failure, propagated as a top-level error here.
    #[test]
    fn block_mismatch_propagates_as_top_level_error() {
        let candidates = vec![
            aerodrome_candidate(
                FIXTURE_BLOCK,
                aero_pool(),
                weth(),
                usdc(),
                Some(U256::from(30u64)),
            ),
            v3_candidate(FIXTURE_BLOCK + 1, v3_pool(), weth(), usdc()),
        ];
        let err = detect_cross_dex_opportunities_at_block(
            FIXTURE_BLOCK,
            weth(),
            U256::from(1_000u64),
            &candidates,
        )
        .unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// Item 9 (b): a duplicate pool address is likewise an M6C
    /// collection-level failure, propagated as a top-level error here.
    #[test]
    fn duplicate_pool_address_propagates_as_top_level_error() {
        let candidates = vec![
            aerodrome_candidate(
                FIXTURE_BLOCK,
                aero_pool(),
                weth(),
                usdc(),
                Some(U256::from(30u64)),
            ),
            v3_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()), // same address
        ];
        let err = detect_cross_dex_opportunities_at_block(
            FIXTURE_BLOCK,
            weth(),
            U256::from(1_000u64),
            &candidates,
        )
        .unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// Item 10 (a): requesting a starting token that matches no route
    /// returns an empty vector, not a fabricated opportunity.
    #[test]
    fn no_matching_starting_token_returns_empty_without_fabrication() {
        let candidates = vec![
            aerodrome_candidate(
                FIXTURE_BLOCK,
                aero_pool(),
                weth(),
                usdc(),
                Some(U256::from(30u64)),
            ),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];
        let outcomes = detect_cross_dex_opportunities_at_block(
            FIXTURE_BLOCK,
            dai(), // belongs to neither pool
            U256::from(1_000u64),
            &candidates,
        )
        .unwrap();
        assert!(outcomes.is_empty());
    }

    /// Item 10 (b): an empty candidate collection returns an empty
    /// vector, not an error and not a fabricated opportunity.
    #[test]
    fn empty_candidates_returns_empty_without_fabrication() {
        let outcomes = detect_cross_dex_opportunities_at_block(
            FIXTURE_BLOCK,
            weth(),
            U256::from(1_000u64),
            &[],
        )
        .unwrap();
        assert!(outcomes.is_empty());
    }

    /// Item 11: the Aerodrome -> Uniswap V3 round trip using these
    /// deterministic fixtures produces a gross loss. The detector must
    /// still return the successfully evaluated Opportunity rather than
    /// filtering it out. The sign is checked against the actual fixture
    /// arithmetic; no external or invented profit amount is assumed.
    #[test]
    fn negative_gross_profit_opportunity_is_not_filtered_out() {
        let amount_in = U256::from(1_000_000_000_000_000u64);
        let candidates = vec![
            aerodrome_candidate(
                FIXTURE_BLOCK,
                aero_pool(),
                weth(),
                usdc(),
                Some(U256::from(30u64)),
            ),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];

        let outcomes =
            detect_cross_dex_opportunities_at_block(FIXTURE_BLOCK, weth(), amount_in, &candidates)
                .unwrap();
        assert_eq!(outcomes.len(), 2);

        let mut saw_aerodrome_first_loss = false;
        for outcome in &outcomes {
            let opportunity = outcome.opportunity().expect("expected a successful outcome");
            let expected_profit = I256::try_from(opportunity.final_amount()).unwrap()
                - I256::try_from(opportunity.input_amount()).unwrap();
            assert_eq!(opportunity.gross_profit(), expected_profit);

            if outcome.route().leg1().dex() == DexKind::Aerodrome {
                assert!(
                    opportunity.gross_profit().is_negative(),
                    "the deterministic Aerodrome -> V3 fixture should remain a loss, got {}",
                    opportunity.gross_profit()
                );
                saw_aerodrome_first_loss = true;
            }
        }

        assert!(
            saw_aerodrome_first_loss,
            "the detector must retain the Aerodrome-first loss-making opportunity"
        );
    }
}
