//! M6B: multi-DEX quote generation over a collection of validated
//! historical pool inputs (M6A), using the existing M5A quote bridge.
//!
//! This module generates one quote outcome per candidate pool for a
//! single requested input token and amount - nothing more. It does not
//! enumerate routes, compose multi-leg paths, call
//! `arbitrage::opportunity::Opportunity::evaluate`, estimate gas, or judge
//! profitability. A successful quote here is evidence of nothing beyond
//! "this pool, quoted on its own, produced this output" - see
//! `arbitrage::opportunity` for round-trip evaluation.
//!
//! No pricing math is duplicated: every quote is produced by the existing
//! `arbitrage::quote::quote_aerodrome_leg`/`quote_uniswap_v3_leg`, which
//! in turn call the existing `pricing::aerodrome_volatile`/
//! `pricing::v3_quote` engines unchanged.
//!
//! **Provenance limitation, inherited unchanged from M6A:** a
//! `HistoricalAerodromeInput`/`HistoricalUniswapV3Input` only proves that
//! its `PoolState`'s own recorded `freshness.last_updated_block` equals
//! the block it was constructed with - not that the underlying reserve/
//! price/liquidity/tick data was genuinely read from the chain at that
//! block. `HydratedTicks` still has no block field of its own. This
//! module adds no new guarantee on either point; it only refuses to mix
//! candidates whose recorded block disagrees with the block requested
//! here (see [`quote_historical_candidates_at_block`]).

use crate::arbitrage::historical::{HistoricalAerodromeInput, HistoricalUniswapV3Input};
use crate::arbitrage::opportunity::LegQuote;
use crate::arbitrage::quote::{quote_aerodrome_leg, quote_uniswap_v3_leg};
use crate::error::{EngineError, EngineResult};
use crate::market::models::{DexKind, PoolState};
use alloy::primitives::{Address, U256};

/// A supported candidate for [`quote_historical_candidates_at_block`].
/// Wraps the existing, already-validated M6A input types directly - there
/// is no variant that accepts a raw, unvalidated `PoolState`, so every
/// candidate that reaches this module has already passed
/// `HistoricalAerodromeInput::new`'s or `HistoricalUniswapV3Input::new`'s
/// own dex/kind/block checks.
#[derive(Debug, Clone)]
pub enum HistoricalQuoteInput {
    Aerodrome(HistoricalAerodromeInput),
    UniswapV3(HistoricalUniswapV3Input),
}

impl HistoricalQuoteInput {
    pub fn pool_state(&self) -> &PoolState {
        match self {
            Self::Aerodrome(input) => input.pool_state(),
            Self::UniswapV3(input) => input.pool_state(),
        }
    }

    pub fn block(&self) -> u64 {
        match self {
            Self::Aerodrome(input) => input.block(),
            Self::UniswapV3(input) => input.block(),
        }
    }
}

/// One candidate's quote outcome: which DEX and pool it came from, and
/// the real pricing engine's own result for it - success or failure,
/// never fabricated.
#[derive(Debug)]
pub struct QuoteOutcome {
    pub dex: DexKind,
    pub pool_address: Address,
    pub result: EngineResult<LegQuote>,
}

/// Quote the same `token_in`/`amount_in` against every candidate in
/// `candidates`, independently.
///
/// Global, request-level rejection (before any candidate is touched):
/// - `amount_in.is_zero()` -> [`EngineError::Arithmetic`]
///
/// Per-candidate rejection (recorded in that candidate's own
/// [`QuoteOutcome::result`] - never aborts the whole request, never
/// suppresses another candidate's success):
/// - `token_in` matches neither the pool's `token0` nor `token1` address
///   -> [`EngineError::State`]
/// - the pool's `token0` and `token1` addresses are identical (direction
///   would have to be guessed) -> [`EngineError::State`]
/// - `candidate.block() != block` (never silently retag a quote with a
///   different block; the pricing engine is not even called for this
///   candidate) -> [`EngineError::State`]
/// - whatever `quote_aerodrome_leg`/`quote_uniswap_v3_leg` itself returns
///   (an unhydrated Aerodrome fee, a V3 current tick outside the hydrated
///   range, etc.) - propagated exactly as received, not reinterpreted
///
/// Output order always matches `candidates`' input order, one outcome per
/// candidate; an empty `candidates` produces `Ok(vec![])`.
pub fn quote_historical_candidates_at_block(
    block: u64,
    token_in: Address,
    amount_in: U256,
    candidates: &[HistoricalQuoteInput],
) -> EngineResult<Vec<QuoteOutcome>> {
    if amount_in.is_zero() {
        return Err(EngineError::Arithmetic(
            "quote_historical_candidates_at_block: amount_in is zero".into(),
        ));
    }

    Ok(candidates
        .iter()
        .map(|candidate| quote_one_candidate(block, token_in, amount_in, candidate))
        .collect())
}

/// Resolve direction, enforce the block, and dispatch to the correct M5A
/// bridge function for exactly one candidate. Never returns `Err` itself -
/// any failure is captured inside the returned `QuoteOutcome::result`, so
/// a caller folding this over many candidates never loses a later
/// candidate's outcome to an earlier candidate's failure.
fn quote_one_candidate(
    block: u64,
    token_in: Address,
    amount_in: U256,
    candidate: &HistoricalQuoteInput,
) -> QuoteOutcome {
    let pool_state = candidate.pool_state();
    let dex = pool_state.pool.dex;
    let pool_address = pool_state.pool.address;
    let token0 = pool_state.pool.token0.address;
    let token1 = pool_state.pool.token1.address;

    let zero_for_one = if token0 == token1 {
        Err(EngineError::State(format!(
            "quote_historical_candidates_at_block: pool {pool_address} has identical \
             token0/token1 addresses ({token0}) - refusing to guess a trade direction"
        )))
    } else if token_in == token0 {
        Ok(true)
    } else if token_in == token1 {
        Ok(false)
    } else {
        Err(EngineError::State(format!(
            "quote_historical_candidates_at_block: requested token_in {token_in} matches \
             neither pool {pool_address}'s token0 ({token0}) nor token1 ({token1})"
        )))
    };
    let zero_for_one = match zero_for_one {
        Ok(z) => z,
        Err(err) => {
            return QuoteOutcome {
                dex,
                pool_address,
                result: Err(err),
            };
        }
    };

    if candidate.block() != block {
        let err = EngineError::State(format!(
            "quote_historical_candidates_at_block: pool {pool_address} is pinned to block {}, \
             not the requested block {block} - refusing to quote it, not retagging it",
            candidate.block()
        ));
        return QuoteOutcome {
            dex,
            pool_address,
            result: Err(err),
        };
    }

    let result = match candidate {
        HistoricalQuoteInput::Aerodrome(input) => {
            quote_aerodrome_leg(input.pool_state(), amount_in, zero_for_one)
        }
        HistoricalQuoteInput::UniswapV3(input) => quote_uniswap_v3_leg(
            input.pool_state(),
            input.hydrated(),
            amount_in,
            zero_for_one,
        ),
    };

    QuoteOutcome {
        dex,
        pool_address,
        result,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::models::{Pool, PoolKind, Token};
    use crate::pricing::v3_quote::HydratedTicks;
    use alloy::primitives::address;
    use std::collections::BTreeMap;

    // Real Base WETH/USDC pools + state at block 26000000 - the same
    // golden fixtures already validated in pricing::aerodrome_volatile,
    // pricing::v3_quote, arbitrage::quote, and arbitrage::historical's own
    // tests. Reused here, not re-derived.
    const FIXTURE_BLOCK: u64 = 26_000_000;

    fn weth() -> Address {
        address!("4200000000000000000000000000000000000006")
    }
    fn usdc() -> Address {
        address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913")
    }

    fn aerodrome_input(block: u64, fee_bps: Option<U256>) -> HistoricalQuoteInput {
        let pool = Pool {
            address: address!("cDAC0d6c6C59727a65F871236188350531885C43"),
            dex: DexKind::Aerodrome,
            token0: Token {
                address: weth(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: usdc(),
                symbol: "USDC".into(),
                decimals: 6,
            },
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

    /// `hydrated_override`: when `None`, uses the real fixture's hydrated
    /// range (covers the real `current_tick`). When `Some`, substitutes a
    /// deliberately insufficient range for provoking
    /// `HydratedV3State::new`'s own existing "current tick outside
    /// hydrated range" rejection (test 8).
    fn v3_input(block: u64, hydrated_override: Option<HydratedTicks>) -> HistoricalQuoteInput {
        let pool = Pool {
            address: address!("d0b53D9277642d899DF5C87A3966A349A798F224"),
            dex: DexKind::UniswapV3,
            token0: Token {
                address: weth(),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: usdc(),
                symbol: "USDC".into(),
                decimals: 6,
            },
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

        let hydrated = hydrated_override.unwrap_or_else(|| {
            let mut ticks = BTreeMap::new();
            ticks.insert(-197_070, -1_776_514_264_016_992i128);
            HydratedTicks::for_test(ticks, -199_680, -192_010)
        });

        HistoricalQuoteInput::UniswapV3(
            HistoricalUniswapV3Input::new(block, pool_state, hydrated).unwrap(),
        )
    }

    /// Test 1 + 4: mixed DEX candidates in one request, both succeed, and
    /// every outcome field (dex, pool address, direction, amount, block)
    /// is correct.
    #[test]
    fn mixed_dex_candidates_both_succeed_with_correct_fields() {
        let candidates = vec![
            aerodrome_input(FIXTURE_BLOCK, Some(U256::from(30u64))),
            v3_input(FIXTURE_BLOCK, None),
        ];
        let amount_in = U256::from(1_000_000_000_000_000u64); // V3's own golden WETH->USDC amount

        let outcomes =
            quote_historical_candidates_at_block(FIXTURE_BLOCK, weth(), amount_in, &candidates)
                .unwrap();

        assert_eq!(outcomes.len(), 2);

        assert_eq!(outcomes[0].dex, DexKind::Aerodrome);
        assert_eq!(
            outcomes[0].pool_address,
            address!("cDAC0d6c6C59727a65F871236188350531885C43")
        );
        let leg0 = outcomes[0].result.as_ref().unwrap();
        assert_eq!(leg0.token_in, weth());
        assert_eq!(leg0.token_out, usdc());
        assert_eq!(leg0.amount_in, amount_in);
        assert_eq!(leg0.block, FIXTURE_BLOCK);

        assert_eq!(outcomes[1].dex, DexKind::UniswapV3);
        assert_eq!(
            outcomes[1].pool_address,
            address!("d0b53D9277642d899DF5C87A3966A349A798F224")
        );
        let leg1 = outcomes[1].result.as_ref().unwrap();
        assert_eq!(leg1.token_in, weth());
        assert_eq!(leg1.token_out, usdc());
        assert_eq!(leg1.amount_in, amount_in);
        assert_eq!(leg1.amount_out, U256::from(2_764_652u64)); // the real golden value
        assert_eq!(leg1.block, FIXTURE_BLOCK);
    }

    /// Test 2: forward (token0 -> token1) direction for both DEX types.
    #[test]
    fn forward_direction_correct_for_both_dex_types() {
        let candidates = vec![
            aerodrome_input(FIXTURE_BLOCK, Some(U256::from(30u64))),
            v3_input(FIXTURE_BLOCK, None),
        ];
        let outcomes = quote_historical_candidates_at_block(
            FIXTURE_BLOCK,
            weth(), // token0 for both fixtures
            U256::from(1_000_000_000_000_000u64),
            &candidates,
        )
        .unwrap();

        for outcome in &outcomes {
            let leg = outcome.result.as_ref().unwrap();
            assert_eq!(leg.token_in, weth());
            assert_eq!(leg.token_out, usdc());
        }
    }

    /// Test 3: reverse (token1 -> token0) direction for both DEX types.
    #[test]
    fn reverse_direction_correct_for_both_dex_types() {
        let candidates = vec![
            aerodrome_input(FIXTURE_BLOCK, Some(U256::from(30u64))),
            v3_input(FIXTURE_BLOCK, None),
        ];
        let outcomes = quote_historical_candidates_at_block(
            FIXTURE_BLOCK,
            usdc(), // token1 for both fixtures
            U256::from(10_000_000_000u64),
            &candidates,
        )
        .unwrap();

        for outcome in &outcomes {
            let leg = outcome.result.as_ref().unwrap();
            assert_eq!(leg.token_in, usdc());
            assert_eq!(leg.token_out, weth());
        }
    }

    /// Test 5: outcome order matches candidate input order, including
    /// across a mix of successes and failures - a failing candidate is
    /// never dropped or reordered.
    #[test]
    fn outcome_order_matches_candidate_order() {
        let v3 = v3_input(FIXTURE_BLOCK, None);
        let aerodrome_unhydrated = aerodrome_input(FIXTURE_BLOCK, None); // fails
        let aerodrome_ok = aerodrome_input(FIXTURE_BLOCK, Some(U256::from(30u64)));
        let candidates = vec![v3, aerodrome_unhydrated, aerodrome_ok];

        let outcomes = quote_historical_candidates_at_block(
            FIXTURE_BLOCK,
            weth(),
            U256::from(1_000_000_000_000_000u64),
            &candidates,
        )
        .unwrap();

        assert_eq!(outcomes.len(), 3);
        assert_eq!(outcomes[0].dex, DexKind::UniswapV3);
        assert!(outcomes[0].result.is_ok());
        assert_eq!(outcomes[1].dex, DexKind::Aerodrome);
        assert!(outcomes[1].result.is_err());
        assert_eq!(outcomes[2].dex, DexKind::Aerodrome);
        assert!(outcomes[2].result.is_ok());
    }

    /// Test 6 (part 1): a token belonging to neither candidate's pool
    /// produces each candidate's own independent `State` error - see
    /// `wrong_token_pair_does_not_suppress_a_valid_candidate` below for
    /// the case where one candidate still succeeds, which is the stronger
    /// form of this requirement.
    #[test]
    fn wrong_token_pair_each_candidate_reports_its_own_error() {
        let unrelated_token = address!("0000000000000000000000000000000000000099");
        let candidates = vec![
            v3_input(FIXTURE_BLOCK, None),
            aerodrome_input(FIXTURE_BLOCK, Some(U256::from(30u64))),
        ];

        let outcomes = quote_historical_candidates_at_block(
            FIXTURE_BLOCK,
            unrelated_token,
            U256::from(1_000_000_000_000_000u64),
            &candidates,
        )
        .unwrap();

        assert_eq!(outcomes.len(), 2);
        let v3_err = outcomes[0].result.as_ref().unwrap_err();
        assert!(matches!(v3_err, EngineError::State(_)), "got {v3_err:?}");
        // The Aerodrome candidate ALSO doesn't contain unrelated_token, so
        // it fails too - both candidates independently report their own
        // State error; neither call aborts the request.
        let aero_err = outcomes[1].result.as_ref().unwrap_err();
        assert!(
            matches!(aero_err, EngineError::State(_)),
            "got {aero_err:?}"
        );
    }

    /// Test 6b: the same wrong-token failure, but paired with a candidate
    /// that DOES succeed, proving failure isolation concretely.
    #[test]
    fn wrong_token_pair_does_not_suppress_a_valid_candidate() {
        let unrelated_token = address!("0000000000000000000000000000000000000099");
        let mismatched_pool = {
            let pool = Pool {
                address: address!("0000000000000000000000000000000000000077"),
                dex: DexKind::Aerodrome,
                token0: Token {
                    address: unrelated_token,
                    symbol: "XXX".into(),
                    decimals: 18,
                },
                token1: Token {
                    address: address!("0000000000000000000000000000000000000078"),
                    symbol: "YYY".into(),
                    decimals: 18,
                },
                kind: PoolKind::Aerodrome {
                    reserve0: U256::from(1_000_000u64),
                    reserve1: U256::from(2_000_000u64),
                    stable: false,
                    fee_bps: Some(U256::from(30u64)),
                },
            };
            let pool_state = PoolState::new(pool, FIXTURE_BLOCK, None);
            HistoricalQuoteInput::Aerodrome(
                HistoricalAerodromeInput::new(FIXTURE_BLOCK, pool_state).unwrap(),
            )
        };
        let candidates = vec![mismatched_pool, v3_input(FIXTURE_BLOCK, None)];

        let outcomes = quote_historical_candidates_at_block(
            FIXTURE_BLOCK,
            weth(), // doesn't belong to candidates[0], does belong to candidates[1]
            U256::from(1_000_000_000_000_000u64),
            &candidates,
        )
        .unwrap();

        assert_eq!(outcomes.len(), 2);
        let err = outcomes[0].result.as_ref().unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
        assert!(outcomes[1].result.is_ok());
    }

    /// Test 7: a candidate recorded at a different block than requested
    /// produces an error for that candidate, not a quote silently tagged
    /// with the requested block.
    #[test]
    fn block_mismatch_fails_that_candidate_without_retagging() {
        let wrong_block_candidate = aerodrome_input(FIXTURE_BLOCK, Some(U256::from(30u64)));
        let candidates = vec![wrong_block_candidate];

        // Request a DIFFERENT block than the candidate was recorded at.
        let outcomes = quote_historical_candidates_at_block(
            FIXTURE_BLOCK + 1,
            weth(),
            U256::from(1_000_000_000_000_000u64),
            &candidates,
        )
        .unwrap();

        assert_eq!(outcomes.len(), 1);
        let err = outcomes[0].result.as_ref().unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// Test 8: an insufficiently hydrated V3 candidate (its hydrated
    /// range deliberately excludes the pool's real current tick, the
    /// exact condition `HydratedV3State::new` itself already rejects)
    /// does not prevent a valid Aerodrome candidate from succeeding.
    #[test]
    fn insufficiently_hydrated_v3_does_not_block_a_valid_aerodrome_candidate() {
        // Does not cover the real current_tick (-197069).
        let bad_hydrated = HydratedTicks::for_test(BTreeMap::new(), 100, 200);
        let candidates = vec![
            v3_input(FIXTURE_BLOCK, Some(bad_hydrated)),
            aerodrome_input(FIXTURE_BLOCK, Some(U256::from(30u64))),
        ];

        let outcomes = quote_historical_candidates_at_block(
            FIXTURE_BLOCK,
            weth(),
            U256::from(1_000_000_000_000_000u64),
            &candidates,
        )
        .unwrap();

        assert_eq!(outcomes.len(), 2);
        let v3_err = outcomes[0].result.as_ref().unwrap_err();
        assert!(
            matches!(v3_err, EngineError::NotImplemented(_)),
            "got {v3_err:?}"
        );
        assert!(outcomes[1].result.is_ok());
    }

    /// Test 9: an Aerodrome candidate with an unhydrated fee
    /// (`fee_bps: None`) produces the real pricing engine's own error - it
    /// is never converted into a zero-fee quote.
    #[test]
    fn unhydrated_aerodrome_fee_is_not_treated_as_zero() {
        let candidates = vec![aerodrome_input(FIXTURE_BLOCK, None)];

        let outcomes = quote_historical_candidates_at_block(
            FIXTURE_BLOCK,
            weth(),
            U256::from(1_000_000_000_000_000u64),
            &candidates,
        )
        .unwrap();

        assert_eq!(outcomes.len(), 1);
        let err = outcomes[0].result.as_ref().unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// Test 10: a zero input amount is rejected at the request level,
    /// before any candidate is touched.
    #[test]
    fn zero_input_amount_is_rejected() {
        let candidates = vec![aerodrome_input(FIXTURE_BLOCK, Some(U256::from(30u64)))];
        let err =
            quote_historical_candidates_at_block(FIXTURE_BLOCK, weth(), U256::ZERO, &candidates)
                .unwrap_err();
        assert!(matches!(err, EngineError::Arithmetic(_)), "got {err:?}");
    }

    /// Test 11: an empty candidate collection produces an empty result
    /// for an otherwise-valid request.
    #[test]
    fn empty_candidate_collection_produces_empty_output() {
        let outcomes =
            quote_historical_candidates_at_block(FIXTURE_BLOCK, weth(), U256::from(1_000u64), &[])
                .unwrap();
        assert!(outcomes.is_empty());
    }

    /// Test 12: a pool whose token0 and token1 addresses are identical
    /// must not be quoted by guessing a direction.
    #[test]
    fn ambiguous_token_direction_is_rejected() {
        let same_token = weth();
        let pool = Pool {
            address: address!("0000000000000000000000000000000000000055"),
            dex: DexKind::Aerodrome,
            token0: Token {
                address: same_token,
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: same_token,
                symbol: "WETH".into(),
                decimals: 18,
            },
            kind: PoolKind::Aerodrome {
                reserve0: U256::from(1_000_000u64),
                reserve1: U256::from(1_000_000u64),
                stable: false,
                fee_bps: Some(U256::from(30u64)),
            },
        };
        let pool_state = PoolState::new(pool, FIXTURE_BLOCK, None);
        let candidate = HistoricalQuoteInput::Aerodrome(
            HistoricalAerodromeInput::new(FIXTURE_BLOCK, pool_state).unwrap(),
        );

        let outcomes = quote_historical_candidates_at_block(
            FIXTURE_BLOCK,
            same_token,
            U256::from(1_000u64),
            &[candidate],
        )
        .unwrap();

        assert_eq!(outcomes.len(), 1);
        let err = outcomes[0].result.as_ref().unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }
}
