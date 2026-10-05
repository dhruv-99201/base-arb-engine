//! Two-leg cross-DEX arbitrage opportunity evaluation: pure, network-free
//! validation and profit arithmetic over two already-computed leg quotes
//! (token A -> leg1 -> token B -> leg2 -> token A). No RPC, no gas, no
//! profitability threshold, no execution - this is raw round-trip
//! arithmetic only. Does not call any DEX pricing engine itself.

use crate::error::{EngineError, EngineResult};
use crate::market::models::DexKind;
use alloy::primitives::{Address, I256, U256};

/// One already-computed leg of a two-leg route: which DEX/pool it executed
/// against, the token pair, the quoted amounts, and the pinned block.
/// Reuses existing types (`DexKind`, `Address`) rather than new ones; a
/// plain data carrier, not validated on its own - [`Opportunity::evaluate`]
/// does the validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegQuote {
    pub dex: DexKind,
    pub pool_address: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
    /// Pinned block this quote was computed at - same concept as
    /// `market::models::Freshness::last_updated_block`.
    pub block: u64,
}

/// A validated two-leg round trip. Fields are private: [`Self::evaluate`]
/// is the only way to construct one, so every route/state invariant it
/// checks always holds for any `Opportunity` that exists - there is no
/// public constructor that can bypass them. Every other field (tokens,
/// amounts, block) is derivable from `leg1`/`leg2` and exposed via
/// accessors rather than duplicated as separate stored fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opportunity {
    leg1: LegQuote,
    leg2: LegQuote,
    /// `final_amount - input_amount`, signed so a loss is a real negative
    /// value (never unsigned subtraction, never a panic or a clamp).
    gross_profit: I256,
}

impl Opportunity {
    /// Validate and combine two leg quotes. DEX-generic: never matches on
    /// `DexKind`, so Aerodrome->UniswapV3 and UniswapV3->Aerodrome run
    /// through this exact same code. Rejects rather than adjusting:
    /// - `leg1.amount_in == 0` -> [`EngineError::Arithmetic`]
    /// - `leg1.block != leg2.block` (never combine quotes from different
    ///   blocks) -> [`EngineError::State`]
    /// - `leg1.token_out != leg2.token_in` -> [`EngineError::State`]
    /// - `leg1.amount_out != leg2.amount_in` (the legs don't chain) ->
    ///   [`EngineError::State`]
    /// - `leg2.token_out != leg1.token_in` (not a round trip) ->
    ///   [`EngineError::State`]
    pub fn evaluate(leg1: LegQuote, leg2: LegQuote) -> EngineResult<Self> {
        if leg1.amount_in.is_zero() {
            return Err(EngineError::Arithmetic(
                "Opportunity::evaluate: leg1 input amount is zero".into(),
            ));
        }
        if leg1.block != leg2.block {
            return Err(EngineError::State(format!(
                "Opportunity::evaluate: block mismatch (leg1={}, leg2={})",
                leg1.block, leg2.block
            )));
        }
        if leg1.token_out != leg2.token_in {
            return Err(EngineError::State(
                "Opportunity::evaluate: leg1 output token != leg2 input token".into(),
            ));
        }
        if leg1.amount_out != leg2.amount_in {
            return Err(EngineError::State(
                "Opportunity::evaluate: leg1 output amount != leg2 input amount".into(),
            ));
        }
        if leg2.token_out != leg1.token_in {
            return Err(EngineError::State(
                "Opportunity::evaluate: leg2 output token != leg1 input token (not a round trip)"
                    .into(),
            ));
        }

        // Signed subtraction via checked I256 arithmetic, never unsigned -
        // same U256 -> I256 pattern already used in
        // pricing::v3_quote::quote_exact_input.
        let input = I256::try_from(leg1.amount_in).map_err(|_| {
            EngineError::Arithmetic("Opportunity::evaluate: input_amount overflows I256".into())
        })?;
        let output = I256::try_from(leg2.amount_out).map_err(|_| {
            EngineError::Arithmetic("Opportunity::evaluate: final_amount overflows I256".into())
        })?;
        let gross_profit = output.checked_sub(input).ok_or_else(|| {
            EngineError::Arithmetic(
                "Opportunity::evaluate: final_amount - input_amount overflows I256".into(),
            )
        })?;

        Ok(Opportunity {
            leg1,
            leg2,
            gross_profit,
        })
    }

    pub fn leg1(&self) -> LegQuote {
        self.leg1
    }

    pub fn leg2(&self) -> LegQuote {
        self.leg2
    }

    pub fn input_token(&self) -> Address {
        self.leg1.token_in
    }

    pub fn intermediate_token(&self) -> Address {
        self.leg1.token_out
    }

    pub fn final_token(&self) -> Address {
        self.leg2.token_out
    }

    pub fn input_amount(&self) -> U256 {
        self.leg1.amount_in
    }

    pub fn intermediate_amount(&self) -> U256 {
        self.leg1.amount_out
    }

    pub fn final_amount(&self) -> U256 {
        self.leg2.amount_out
    }

    pub fn gross_profit(&self) -> I256 {
        self.gross_profit
    }

    pub fn block(&self) -> u64 {
        self.leg1.block
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    const TOKEN_A: Address = address!("0000000000000000000000000000000000000001");
    const TOKEN_B: Address = address!("0000000000000000000000000000000000000002");
    const TOKEN_C: Address = address!("0000000000000000000000000000000000000003");
    const POOL_1: Address = address!("000000000000000000000000000000000000a001");
    const POOL_2: Address = address!("000000000000000000000000000000000000a002");

    fn leg(
        dex: DexKind,
        pool_address: Address,
        token_in: Address,
        token_out: Address,
        amount_in: u64,
        amount_out: u64,
        block: u64,
    ) -> LegQuote {
        LegQuote {
            dex,
            pool_address,
            token_in,
            token_out,
            amount_in: U256::from(amount_in),
            amount_out: U256::from(amount_out),
            block,
        }
    }

    #[test]
    fn profitable_round_trip() {
        let leg1 = leg(DexKind::Aerodrome, POOL_1, TOKEN_A, TOKEN_B, 1000, 1000, 100);
        let leg2 = leg(DexKind::UniswapV3, POOL_2, TOKEN_B, TOKEN_A, 1000, 1050, 100);
        let opp = Opportunity::evaluate(leg1, leg2).unwrap();
        assert_eq!(opp.gross_profit(), I256::try_from(50i64).unwrap());
    }

    #[test]
    fn break_even_round_trip() {
        let leg1 = leg(DexKind::Aerodrome, POOL_1, TOKEN_A, TOKEN_B, 1000, 1000, 100);
        let leg2 = leg(DexKind::UniswapV3, POOL_2, TOKEN_B, TOKEN_A, 1000, 1000, 100);
        let opp = Opportunity::evaluate(leg1, leg2).unwrap();
        assert_eq!(opp.gross_profit(), I256::ZERO);
    }

    /// Must be a real negative value - never a panic, never a clamp to
    /// zero, never an `Err`.
    #[test]
    fn losing_round_trip() {
        let leg1 = leg(DexKind::Aerodrome, POOL_1, TOKEN_A, TOKEN_B, 1000, 1000, 100);
        let leg2 = leg(DexKind::UniswapV3, POOL_2, TOKEN_B, TOKEN_A, 1000, 950, 100);
        let opp = Opportunity::evaluate(leg1, leg2).unwrap();
        assert_eq!(opp.gross_profit(), I256::try_from(-50i64).unwrap());
    }

    #[test]
    fn block_mismatch_is_rejected() {
        let leg1 = leg(DexKind::Aerodrome, POOL_1, TOKEN_A, TOKEN_B, 1000, 1000, 100);
        let leg2 = leg(DexKind::UniswapV3, POOL_2, TOKEN_B, TOKEN_A, 1000, 1050, 101);
        let err = Opportunity::evaluate(leg1, leg2).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    #[test]
    fn zero_input_is_rejected() {
        let leg1 = leg(DexKind::Aerodrome, POOL_1, TOKEN_A, TOKEN_B, 0, 0, 100);
        let leg2 = leg(DexKind::UniswapV3, POOL_2, TOKEN_B, TOKEN_A, 0, 0, 100);
        let err = Opportunity::evaluate(leg1, leg2).unwrap_err();
        assert!(matches!(err, EngineError::Arithmetic(_)), "got {err:?}");
    }

    #[test]
    fn wrong_intermediate_token_is_rejected() {
        let leg1 = leg(DexKind::Aerodrome, POOL_1, TOKEN_A, TOKEN_B, 1000, 1000, 100);
        // leg2 claims to start from TOKEN_C, not TOKEN_B (what leg1 produced).
        let leg2 = leg(DexKind::UniswapV3, POOL_2, TOKEN_C, TOKEN_A, 1000, 1050, 100);
        let err = Opportunity::evaluate(leg1, leg2).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    #[test]
    fn wrong_final_token_is_rejected() {
        let leg1 = leg(DexKind::Aerodrome, POOL_1, TOKEN_A, TOKEN_B, 1000, 1000, 100);
        // leg2 ends in TOKEN_C, not back in TOKEN_A - not a round trip.
        let leg2 = leg(DexKind::UniswapV3, POOL_2, TOKEN_B, TOKEN_C, 1000, 1050, 100);
        let err = Opportunity::evaluate(leg1, leg2).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    /// Aerodrome -> Uniswap V3 ordering, using the real Base WETH/USDC pool
    /// addresses from this project's golden fixtures as realistic
    /// identifiers only - no RPC call is made.
    #[test]
    fn dex_ordering_aerodrome_then_uniswap_v3() {
        let weth = address!("4200000000000000000000000000000000000006");
        let usdc = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let aerodrome_pool = address!("cDAC0d6c6C59727a65F871236188350531885C43");
        let uniswap_pool = address!("d0b53D9277642d899DF5C87A3966A349A798F224");

        let leg1 = leg(DexKind::Aerodrome, aerodrome_pool, weth, usdc, 1_000_000_000_000_000_000, 2_753_396_596, 26_000_000);
        let leg2 = leg(DexKind::UniswapV3, uniswap_pool, usdc, weth, 2_753_396_596, 1_000_500_000_000_000_000, 26_000_000);

        let opp = Opportunity::evaluate(leg1, leg2).unwrap();
        assert_eq!(opp.leg1().dex, DexKind::Aerodrome);
        assert_eq!(opp.leg2().dex, DexKind::UniswapV3);
        assert_eq!(opp.input_token(), weth);
        assert_eq!(opp.final_token(), weth);
        assert_eq!(opp.gross_profit(), I256::try_from(500_000_000_000_000i64).unwrap());
    }

    /// Exact DEX-order reversal of the test above - same `evaluate` code
    /// path, proving there is no per-DEX branching.
    #[test]
    fn dex_ordering_uniswap_v3_then_aerodrome() {
        let weth = address!("4200000000000000000000000000000000000006");
        let usdc = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let aerodrome_pool = address!("cDAC0d6c6C59727a65F871236188350531885C43");
        let uniswap_pool = address!("d0b53D9277642d899DF5C87A3966A349A798F224");

        let leg1 = leg(DexKind::UniswapV3, uniswap_pool, weth, usdc, 1_000_000_000_000_000_000, 2_764_652_000, 26_000_000);
        let leg2 = leg(DexKind::Aerodrome, aerodrome_pool, usdc, weth, 2_764_652_000, 999_000_000_000_000_000, 26_000_000);

        let opp = Opportunity::evaluate(leg1, leg2).unwrap();
        assert_eq!(opp.leg1().dex, DexKind::UniswapV3);
        assert_eq!(opp.leg2().dex, DexKind::Aerodrome);
        assert_eq!(opp.gross_profit(), I256::try_from(-1_000_000_000_000_000i64).unwrap());
    }

    /// Amounts at 2^60 are well beyond f64's 53-bit exact integer range -
    /// an f64 round trip couldn't even distinguish these two values.
    /// Getting exactly +1 proves bit-exact integer arithmetic, not a float.
    #[test]
    fn exact_integer_arithmetic_above_two_pow_53() {
        let base: u128 = 1u128 << 60;
        let leg1 = leg(DexKind::Aerodrome, POOL_1, TOKEN_A, TOKEN_B, 0, 0, 100);
        let leg1 = LegQuote {
            amount_in: U256::from(base),
            amount_out: U256::from(base),
            ..leg1
        };
        let leg2 = leg(DexKind::UniswapV3, POOL_2, TOKEN_B, TOKEN_A, 0, 0, 100);
        let leg2 = LegQuote {
            amount_in: U256::from(base),
            amount_out: U256::from(base + 1),
            ..leg2
        };

        let opp = Opportunity::evaluate(leg1, leg2).unwrap();
        assert_eq!(opp.gross_profit(), I256::try_from(1i64).unwrap());
    }
}
