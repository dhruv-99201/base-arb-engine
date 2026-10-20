//! M6C: deterministic enumeration of two-pool, cross-DEX round-trip route
//! *structure* over already-validated M6A historical inputs.
//!
//! This module answers "which pool pairs could form a round trip, and in
//! which order/direction" - nothing about amounts, prices, or profit. It
//! never calls `arbitrage::multi_quote::quote_historical_candidates_at_block`,
//! either M5A pricing bridge function, or
//! `arbitrage::opportunity::Opportunity::evaluate` - route enumeration is
//! complete before any of those would even be relevant. A route existing
//! here is not evidence it is profitable, or even that it can be filled at
//! any particular size; it only says the two pools share a token pair and
//! are on different supported DEXes.
//!
//! **Provenance limitation, inherited unchanged from M6A/M6B:** a
//! candidate's recorded block is a `PoolState` metadata field, not
//! independent proof the underlying reserve/price/tick data was read from
//! the chain at that block, and `HydratedTicks` still has no block field
//! of its own. This module adds no new guarantee on either point.

use crate::arbitrage::multi_quote::HistoricalQuoteInput;
use crate::error::{EngineError, EngineResult};
use crate::market::models::DexKind;
use alloy::primitives::Address;
use std::collections::HashSet;

/// One leg of an [`EnumeratedRoute`]: which original candidate it came
/// from, that candidate's DEX and pool address, and the direction this
/// leg would trade in. Every field is derived directly from the
/// candidate's own `PoolState` - nothing here is invented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteLeg {
    /// Index into the `candidates` slice passed to
    /// [`enumerate_cross_dex_routes_at_block`] - the connection back to
    /// the caller's original collection. Pool address and DEX alone are
    /// not a substitute for this (see module docs on why).
    candidate_index: usize,
    dex: DexKind,
    pool_address: Address,
    token_in: Address,
    token_out: Address,
}

impl RouteLeg {
    pub fn candidate_index(&self) -> usize {
        self.candidate_index
    }

    pub fn dex(&self) -> DexKind {
        self.dex
    }

    pub fn pool_address(&self) -> Address {
        self.pool_address
    }

    pub fn token_in(&self) -> Address {
        self.token_in
    }

    pub fn token_out(&self) -> Address {
        self.token_out
    }
}

/// A structurally valid two-pool round trip at one requested block.
/// Fields are private - the only way to obtain one is through
/// [`enumerate_cross_dex_routes_at_block`], matching
/// `arbitrage::opportunity::Opportunity`'s own construction discipline.
/// Only `leg1`/`leg2`/`block` are stored; `starting_token()`/
/// `intermediate_token()` are derived from `leg1` rather than duplicated
/// as separate fields that could disagree with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnumeratedRoute {
    block: u64,
    leg1: RouteLeg,
    leg2: RouteLeg,
}

impl EnumeratedRoute {
    pub fn block(&self) -> u64 {
        self.block
    }

    pub fn leg1(&self) -> RouteLeg {
        self.leg1
    }

    pub fn leg2(&self) -> RouteLeg {
        self.leg2
    }

    /// The token the route starts (and, on a closed round trip, ends)
    /// with - `leg1.token_in()`.
    pub fn starting_token(&self) -> Address {
        self.leg1.token_in
    }

    /// The token swapped into after leg 1 and back out of in leg 2 -
    /// `leg1.token_out()`, which by construction always equals
    /// `leg2.token_in()`.
    pub fn intermediate_token(&self) -> Address {
        self.leg1.token_out
    }
}

/// Enumerate every structurally valid two-pool, cross-DEX round trip among
/// `candidates`, pinned to `block`.
///
/// Validates the ENTIRE candidate collection before producing any routes
/// (never silently drops a malformed candidate from consideration):
/// - any `candidate.block() != block` -> [`EngineError::State`]
/// - any candidate pool with identical `token0`/`token1` addresses
///   (direction would have to be guessed) -> [`EngineError::State`]
/// - any pool address appearing more than once in `candidates` ->
///   [`EngineError::State`]
///
/// Once every candidate passes those checks, pairs are considered in
/// original-index order (`i < j` over `0..candidates.len()`). A pair
/// yields routes only if it is cross-DEX (one `DexKind::Aerodrome`, one
/// `DexKind::UniswapV3` - same-DEX pairs, including two Aerodrome
/// Slipstream-shaped pools, never match here since
/// `HistoricalQuoteInput` only ever wraps the two supported types) and
/// shares the same unordered token pair. A non-matching pair simply
/// contributes no routes - that is not an error.
///
/// For each eligible pair, exactly four routes are produced: both
/// canonical-order starting tokens, crossed with both leg-1-DEX
/// orderings (ties within a starting token broken by original candidate
/// index, smaller first) - see the module's own tests for the exact
/// sequence.
pub fn enumerate_cross_dex_routes_at_block(
    block: u64,
    candidates: &[HistoricalQuoteInput],
) -> EngineResult<Vec<EnumeratedRoute>> {
    for candidate in candidates {
        if candidate.block() != block {
            return Err(EngineError::State(format!(
                "enumerate_cross_dex_routes_at_block: pool {} is pinned to block {}, not the \
                 requested block {block}",
                candidate.pool_state().pool.address,
                candidate.block()
            )));
        }
        let pool = &candidate.pool_state().pool;
        if pool.token0.address == pool.token1.address {
            return Err(EngineError::State(format!(
                "enumerate_cross_dex_routes_at_block: pool {} has identical token0/token1 \
                 addresses ({}) - cannot determine a token pair",
                pool.address, pool.token0.address
            )));
        }
    }

    let mut seen_addresses: HashSet<Address> = HashSet::new();
    for candidate in candidates {
        let addr = candidate.pool_state().pool.address;
        if !seen_addresses.insert(addr) {
            return Err(EngineError::State(format!(
                "enumerate_cross_dex_routes_at_block: pool address {addr} appears more than \
                 once in the candidate collection"
            )));
        }
    }

    let mut routes = Vec::new();
    for i in 0..candidates.len() {
        for j in (i + 1)..candidates.len() {
            routes.extend(routes_for_pair(block, candidates, i, j));
        }
    }
    Ok(routes)
}

/// Deterministic tie-break for which of two distinct token addresses is
/// "first" - used only to fix an arbitrary-but-consistent starting-token
/// order, not a claim about token identity or value. Compares each
/// address's own `Display` rendering (already used throughout this
/// crate's error messages) as a string rather than relying on
/// `Address`'s own ordering impl, since this module has no other need to
/// depend on one - any deterministic total order over distinct addresses
/// is equally valid here.
fn canonical_order(a: Address, b: Address) -> (Address, Address) {
    if a.to_string() <= b.to_string() {
        (a, b)
    } else {
        (b, a)
    }
}

/// Produce the 0 or 4 routes for one candidate pair. Returns an empty
/// `Vec` (not an error) for any pair that is same-DEX or does not share
/// the same unordered token pair - that is normal, expected topology, not
/// a malformed input. Caller must pass `i < j` from `candidates`' own
/// index space; every emitted `RouteLeg::candidate_index` is exactly `i`
/// or `j`.
fn routes_for_pair(
    block: u64,
    candidates: &[HistoricalQuoteInput],
    i: usize,
    j: usize,
) -> Vec<EnumeratedRoute> {
    let pool_i = &candidates[i].pool_state().pool;
    let pool_j = &candidates[j].pool_state().pool;

    let is_cross_dex = matches!(
        (pool_i.dex, pool_j.dex),
        (DexKind::Aerodrome, DexKind::UniswapV3) | (DexKind::UniswapV3, DexKind::Aerodrome)
    );
    if !is_cross_dex {
        return Vec::new();
    }

    let set_i = (pool_i.token0.address, pool_i.token1.address);
    let set_j = (pool_j.token0.address, pool_j.token1.address);
    let same_token_pair =
        (set_i.0 == set_j.0 && set_i.1 == set_j.1) || (set_i.0 == set_j.1 && set_i.1 == set_j.0);
    if !same_token_pair {
        return Vec::new();
    }

    let (token_x, token_y) = canonical_order(pool_i.token0.address, pool_i.token1.address);

    let mut routes = Vec::with_capacity(4);
    for &starting in &[token_x, token_y] {
        let other = if starting == token_x {
            token_y
        } else {
            token_x
        };
        // i < j is guaranteed by the caller, so iterating [i, j] in this
        // order already satisfies "the first pool in original
        // candidate-index order leads".
        for &leg1_idx in &[i, j] {
            let leg2_idx = if leg1_idx == i { j } else { i };
            let leg1_pool = &candidates[leg1_idx].pool_state().pool;
            let leg2_pool = &candidates[leg2_idx].pool_state().pool;
            routes.push(EnumeratedRoute {
                block,
                leg1: RouteLeg {
                    candidate_index: leg1_idx,
                    dex: leg1_pool.dex,
                    pool_address: leg1_pool.address,
                    token_in: starting,
                    token_out: other,
                },
                leg2: RouteLeg {
                    candidate_index: leg2_idx,
                    dex: leg2_pool.dex,
                    pool_address: leg2_pool.address,
                    token_in: other,
                    token_out: starting,
                },
            });
        }
    }
    routes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrage::historical::{HistoricalAerodromeInput, HistoricalUniswapV3Input};
    use crate::market::models::{Pool, PoolKind, PoolState, Token};
    use crate::pricing::v3_quote::HydratedTicks;
    use alloy::primitives::{address, U256};
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

    /// Generic synthetic Aerodrome candidate - `pool_address` and the
    /// (token0, token1) pair are caller-chosen so tests can construct
    /// exactly the topology they need.
    fn aerodrome_candidate(
        block: u64,
        pool_address: Address,
        token0: Address,
        token1: Address,
    ) -> HistoricalQuoteInput {
        let pool = Pool {
            address: pool_address,
            dex: DexKind::Aerodrome,
            token0: token(token0, "T0", 18),
            token1: token(token1, "T1", 18),
            kind: PoolKind::Aerodrome {
                reserve0: U256::from(1_000_000u64),
                reserve1: U256::from(2_000_000u64),
                stable: false,
                fee_bps: Some(U256::from(30u64)),
            },
        };
        let pool_state = PoolState::new(pool, block, None);
        HistoricalQuoteInput::Aerodrome(HistoricalAerodromeInput::new(block, pool_state).unwrap())
    }

    /// Generic synthetic Uniswap V3 candidate, same purpose as
    /// `aerodrome_candidate`. Uses the real golden fixture's sqrt price /
    /// tick / liquidity / hydration so `HistoricalUniswapV3Input::new`
    /// always succeeds regardless of which tokens/pool address the test
    /// supplies (route enumeration never reads price/liquidity/tick
    /// fields at all, only token0/token1/dex/address/block).
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
            token1: token(token1, "T1", 18),
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
    fn v3_pool_2() -> Address {
        address!("0000000000000000000000000000000000000071")
    }
    fn aero_pool_2() -> Address {
        address!("0000000000000000000000000000000000000072")
    }

    /// Test 1 + 2 + 3: one Aerodrome + one V3 pool on the real WETH/USDC
    /// pair produce exactly the four required routes, both DEX orderings,
    /// both starting tokens, each closing the cycle correctly.
    #[test]
    fn one_pair_produces_exactly_four_closed_routes() {
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];
        let routes = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();

        assert_eq!(routes.len(), 4);

        let (token_x, token_y) = canonical_order(weth(), usdc());

        // Route 1: start token_x, leg1 = candidate 0 (Aerodrome).
        assert_eq!(routes[0].starting_token(), token_x);
        assert_eq!(routes[0].intermediate_token(), token_y);
        assert_eq!(routes[0].leg1().candidate_index(), 0);
        assert_eq!(routes[0].leg1().dex(), DexKind::Aerodrome);
        assert_eq!(routes[0].leg2().candidate_index(), 1);
        assert_eq!(routes[0].leg2().dex(), DexKind::UniswapV3);

        // Route 2: start token_x, leg1 = candidate 1 (V3).
        assert_eq!(routes[1].starting_token(), token_x);
        assert_eq!(routes[1].leg1().candidate_index(), 1);
        assert_eq!(routes[1].leg1().dex(), DexKind::UniswapV3);
        assert_eq!(routes[1].leg2().candidate_index(), 0);
        assert_eq!(routes[1].leg2().dex(), DexKind::Aerodrome);

        // Route 3: start token_y, leg1 = candidate 0 (Aerodrome).
        assert_eq!(routes[2].starting_token(), token_y);
        assert_eq!(routes[2].intermediate_token(), token_x);
        assert_eq!(routes[2].leg1().candidate_index(), 0);

        // Route 4: start token_y, leg1 = candidate 1 (V3).
        assert_eq!(routes[3].starting_token(), token_y);
        assert_eq!(routes[3].leg1().candidate_index(), 1);

        // Every route: closed cycle, distinct pools/DEXes/candidates,
        // same block, no fabricated data beyond topology.
        for route in &routes {
            assert_eq!(route.block(), FIXTURE_BLOCK);
            assert_eq!(route.leg1().token_out(), route.leg2().token_in());
            assert_eq!(route.leg2().token_out(), route.leg1().token_in());
            assert_ne!(
                route.leg1().candidate_index(),
                route.leg2().candidate_index()
            );
            assert_ne!(route.leg1().pool_address(), route.leg2().pool_address());
            assert_ne!(route.leg1().dex(), route.leg2().dex());
        }
    }

    /// Test 4: matching still works when one pool stores the pair as
    /// (A, B) and the other stores it as (B, A).
    #[test]
    fn matches_regardless_of_token0_token1_storage_order() {
        let candidates = vec![
            // (WETH, USDC)
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            // (USDC, WETH) - reversed storage order, same unordered pair
            v3_candidate(FIXTURE_BLOCK, v3_pool(), usdc(), weth()),
        ];
        let routes = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();
        assert_eq!(routes.len(), 4);
    }

    /// Test 5: pools (A, B) and (B, C) do not form a two-leg round trip -
    /// only a partial token overlap, not the same pair.
    #[test]
    fn partial_token_overlap_produces_no_routes() {
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()), // (WETH, USDC)
            v3_candidate(FIXTURE_BLOCK, v3_pool(), usdc(), dai()),           // (USDC, DAI)
        ];
        let routes = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();
        assert!(routes.is_empty());
    }

    /// Test 6: unrelated token pairs (no shared token at all) produce no
    /// routes.
    #[test]
    fn unrelated_token_pairs_produce_no_routes() {
        let other_a = address!("0000000000000000000000000000000000000081");
        let other_b = address!("0000000000000000000000000000000000000082");
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), other_a, other_b),
        ];
        let routes = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();
        assert!(routes.is_empty());
    }

    /// Test 7: two Aerodrome pools alone, or two V3 pools alone, never
    /// produce routes - cross-DEX only.
    #[test]
    fn same_dex_pairs_produce_no_routes() {
        let two_aerodrome = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool_2(), weth(), usdc()),
        ];
        assert!(
            enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &two_aerodrome)
                .unwrap()
                .is_empty()
        );

        let two_v3 = vec![
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
            v3_candidate(FIXTURE_BLOCK, v3_pool_2(), weth(), usdc()),
        ];
        assert!(enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &two_v3)
            .unwrap()
            .is_empty());
    }

    /// Test 8: a candidate recorded at a different block than requested
    /// causes the whole call to fail, rather than silently excluding it.
    #[test]
    fn block_mismatch_is_rejected() {
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            v3_candidate(FIXTURE_BLOCK + 1, v3_pool(), weth(), usdc()),
        ];
        let err = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// Test 9: a pool with identical token0/token1 addresses is rejected
    /// outright, not silently skipped.
    #[test]
    fn ambiguous_pool_tokens_are_rejected() {
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), weth()),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];
        let err = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// Test 10: the same pool address appearing twice in the candidate
    /// collection is rejected.
    #[test]
    fn duplicate_pool_address_is_rejected() {
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            // Same pool address as above, different dex/kind.
            v3_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
        ];
        let err = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// Test 11: an empty candidate collection succeeds with no routes.
    #[test]
    fn empty_candidate_collection_is_ok_and_empty() {
        let routes = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &[]).unwrap();
        assert!(routes.is_empty());
    }

    /// Test 12: a valid, non-empty collection with no eligible cross-DEX
    /// pair succeeds with no routes (covered concretely by
    /// `unrelated_token_pairs_produce_no_routes` and
    /// `same_dex_pairs_produce_no_routes` above; this asserts the same
    /// property for a single-candidate collection, where no pair exists
    /// at all).
    #[test]
    fn single_candidate_collection_has_no_pairs_and_no_routes() {
        let candidates = vec![aerodrome_candidate(
            FIXTURE_BLOCK,
            aero_pool(),
            weth(),
            usdc(),
        )];
        let routes = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();
        assert!(routes.is_empty());
    }

    /// Test 13: two Aerodrome pools and one V3 pool on the same token
    /// pair yield 2 eligible pairs x 4 routes = 8 routes, with no
    /// duplicate route descriptors.
    #[test]
    fn multiple_compatible_pairs_yield_eight_unique_routes() {
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool_2(), weth(), usdc()),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];
        let routes = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();
        assert_eq!(routes.len(), 8);

        let mut seen = std::collections::HashSet::new();
        for route in &routes {
            let key = (
                route.leg1().candidate_index(),
                route.leg2().candidate_index(),
                route.starting_token(),
            );
            assert!(seen.insert(key), "duplicate route descriptor: {key:?}");
        }
    }

    /// Test 14: enumerating the same candidate collection twice yields
    /// exactly the same route sequence.
    #[test]
    fn enumeration_is_deterministic_across_repeated_calls() {
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];
        let first = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();
        let second = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();
        assert_eq!(first, second);
    }

    /// Test 15: each route leg's candidate_index correctly identifies the
    /// matching DEX, pool address, and token direction in the original
    /// candidate slice.
    #[test]
    fn candidate_index_matches_original_slice_contents() {
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];
        let routes = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();

        for route in &routes {
            for leg in [route.leg1(), route.leg2()] {
                let original_pool = &candidates[leg.candidate_index()].pool_state().pool;
                assert_eq!(leg.dex(), original_pool.dex);
                assert_eq!(leg.pool_address(), original_pool.address);
                let tokens = (original_pool.token0.address, original_pool.token1.address);
                assert!(leg.token_in() == tokens.0 || leg.token_in() == tokens.1);
                assert!(leg.token_out() == tokens.0 || leg.token_out() == tokens.1);
                assert_ne!(leg.token_in(), leg.token_out());
            }
        }
    }

    /// Test 16: every emitted route satisfies the full invariant set at
    /// once, and carries no quote amounts or profitability data (there is
    /// no field on `EnumeratedRoute`/`RouteLeg` that could - this test
    /// documents that by construction, nothing here is computable beyond
    /// what is asserted).
    #[test]
    fn every_route_satisfies_block_and_closure_invariants() {
        let candidates = vec![
            aerodrome_candidate(FIXTURE_BLOCK, aero_pool(), weth(), usdc()),
            v3_candidate(FIXTURE_BLOCK, v3_pool(), weth(), usdc()),
        ];
        let routes = enumerate_cross_dex_routes_at_block(FIXTURE_BLOCK, &candidates).unwrap();
        assert_eq!(routes.len(), 4);

        for route in &routes {
            assert_eq!(route.block(), FIXTURE_BLOCK);
            assert_eq!(route.leg1().token_out(), route.leg2().token_in());
            assert_eq!(route.leg2().token_out(), route.leg1().token_in());
            assert_eq!(route.starting_token(), route.leg1().token_in());
            assert_eq!(route.intermediate_token(), route.leg1().token_out());
            assert_ne!(route.leg1().pool_address(), route.leg2().pool_address());
            assert_ne!(route.leg1().dex(), route.leg2().dex());
            assert_ne!(
                route.leg1().candidate_index(),
                route.leg2().candidate_index()
            );
        }
    }
}
