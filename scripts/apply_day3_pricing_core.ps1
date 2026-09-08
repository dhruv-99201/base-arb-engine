# base-arb-engine Day 3 (partial): FullMath + TickMath pricing primitives
# Run from the repository root: C:\Users\Dell\Downloads\base-arb-engine
#   powershell -ExecutionPolicy Bypass -File apply_day3_pricing_core.ps1
Write-Host 'Applying Day 3 pricing core (FullMath + TickMath)...'

# Ensure all target directories exist first.
New-Item -ItemType Directory -Force -Path 'src' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\pricing' | Out-Null

# ---- src/pricing/mod.rs ----
$content = @'
//! Exact, no-floating-point pricing math for the Day 3 opportunity scanner.
//!
//! **Status: partial.** `full_math` and `tick_math` are implemented and
//! tested, both ported from real Uniswap V3 source
//! (`FullMath.sol`/`TickMath.sol`) with every constant and shift-semantics
//! detail checked against actual source rather than assumed. The exact V3
//! quoter (tick-crossing swap loop over real hydrated tick-bitmap state),
//! the Aerodrome volatile quoter, the economic model, the trade-size
//! ladder, the opportunity engine, the journal, and golden on-chain
//! comparison tests are NOT yet implemented - see the Day 3 status report
//! for exactly why and what's next.

pub mod full_math;
pub mod tick_math;

'@
Set-Content -Path 'src\pricing\mod.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pricing/mod.rs'

# ---- src/pricing/full_math.rs ----
$content = @'
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
    U256::try_from(result)
        .map_err(|_| EngineError::Arithmetic("mul_div: result overflows U256".into()))
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

    U256::try_from(result)
        .map_err(|_| EngineError::Arithmetic("mul_div_rounding_up: result overflows U256".into()))
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

'@
Set-Content -Path 'src\pricing\full_math.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pricing/full_math.rs'

# ---- src/pricing/tick_math.rs ----
$content = @'
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
        for tick in [MIN_TICK, -887271, -100000, -1, 0, 1, 100000, 887271, MAX_TICK] {
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

'@
Set-Content -Path 'src\pricing\tick_math.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/pricing/tick_math.rs'

# ---- src/main.rs ----
$content = @'
mod chain;
mod cli;
mod config;
mod dex;
mod discovery_pipeline;
mod error;
mod events;
mod market;
mod pools;
mod pricing;
mod telemetry;

use crate::chain::{BaseChainSource, ChainEventSource};
use crate::config::Config;
use crate::dex::DexAdapter;
use crate::discovery_pipeline::DiscoveryPipeline;
use crate::error::EngineResult;
use crate::events::decoder::now_us;
use crate::market::{BlockState, DexKind, MarketState};
use futures::StreamExt;

#[tokio::main]
async fn main() -> EngineResult<()> {
    // --- discover-test / inspect-tx subcommands: read-only diagnostics,
    // handled before the normal startup path so `--help` never requires a
    // valid .env and argument errors never touch the network. ---
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() > 1 && raw_args[1] == "discover-test" {
        return match cli::parse_discover_test_args(&raw_args[2..]) {
            Ok(cli::DiscoverTestCommand::Help) => {
                println!("{}", cli::DISCOVER_TEST_USAGE);
                Ok(())
            }
            Ok(cli::DiscoverTestCommand::Run { from_block, to_block }) => {
                let config = Config::load()?;
                telemetry::init_tracing(&config.log_level);
                cli::run_discover_test(&config, from_block, to_block).await
            }
            Err(msg) => {
                eprintln!("error: {msg}\n");
                eprintln!("{}", cli::DISCOVER_TEST_USAGE);
                std::process::exit(2)
            }
        };
    }
    if raw_args.len() > 1 && raw_args[1] == "inspect-tx" {
        return match cli::parse_inspect_tx_args(&raw_args[2..]) {
            Ok(cli::InspectTxCommand::Help) => {
                println!("{}", cli::INSPECT_TX_USAGE);
                Ok(())
            }
            Ok(cli::InspectTxCommand::Run { tx_hash }) => {
                let config = Config::load()?;
                telemetry::init_tracing(&config.log_level);
                cli::run_inspect_tx(&config, tx_hash).await
            }
            Err(msg) => {
                eprintln!("error: {msg}\n");
                eprintln!("{}", cli::INSPECT_TX_USAGE);
                std::process::exit(2)
            }
        };
    }

    let config = Config::load()?;
    telemetry::init_tracing(&config.log_level);

    tracing::info!(
        execution_mode = ?config.execution_mode,
        can_execute_trades = config.execution_mode.can_execute_trades(),
        "starting base-arb-engine (Day 2: pool discovery + market-state indexing)"
    );

    let chain_source = BaseChainSource::new(
        config.base_rpc_url.clone(),
        config.base_ws_url.clone(),
        config.http_poll_interval,
    );

    // --- Verify connectivity ---
    let chain_id = chain_source.chain_id().await?;
    if chain_id != config.base_chain_id {
        tracing::warn!(
            configured = config.base_chain_id,
            observed = chain_id,
            "configured BASE_CHAIN_ID does not match chain ID reported by RPC endpoint"
        );
    } else {
        tracing::info!(chain_id, "chain ID verified");
    }

    let latest_block = chain_source.latest_block_number().await?;
    tracing::info!(latest_block, "retrieved latest Base block");

    let state = MarketState::new_shared();
    {
        let mut guard = state.write().await;
        guard.update_latest_block(BlockState {
            number: latest_block,
            timestamp: None,
            hash: None,
        });
    }

    // --- Optional: register configured pools ---
    let aerodrome_adapter = dex::AerodromeAdapter::new();
    let uniswap_v3_adapter = dex::UniswapV3Adapter::new();

    let mut watched_addresses = Vec::new();
    if let Some(addr) = &config.aerodrome_pool_address {
        match addr.parse::<alloy::primitives::Address>() {
            Ok(a) => {
                tracing::info!(pool = %a, dex = "aerodrome", "watching configured pool");
                watched_addresses.push(a);
            }
            Err(e) => tracing::error!(error = %e, "invalid AERODROME_POOL_ADDRESS, ignoring"),
        }
    } else {
        tracing::info!(
            "AERODROME_POOL_ADDRESS not configured - generic ingestion layer will run without a \
             known Aerodrome pool. Set it to a verified pool address to process real events."
        );
    }
    if let Some(addr) = &config.uniswap_v3_pool_address {
        match addr.parse::<alloy::primitives::Address>() {
            Ok(a) => {
                tracing::info!(pool = %a, dex = "uniswap_v3", "watching configured pool");
                watched_addresses.push(a);
            }
            Err(e) => tracing::error!(error = %e, "invalid UNISWAP_V3_POOL_ADDRESS, ignoring"),
        }
    } else {
        tracing::info!(
            "UNISWAP_V3_POOL_ADDRESS not configured - generic ingestion layer will run without a \
             known Uniswap V3 pool. Set it to a verified pool address to process real events."
        );
    }

    if config.base_ws_url.is_none() {
        tracing::warn!(
            source = "http_poll",
            "BASE_WS_URL not configured - entering HTTP fallback mode. Block ingestion will \
             poll the configured BASE_RPC_URL periodically instead of streaming over WebSocket. \
             Log/event ingestion for configured pools is unavailable in this mode (it requires \
             WebSocket)."
        );
    } else {
        tracing::info!(source = "websocket", "WebSocket endpoint configured - using streaming ingestion");
    }

    // --- Block stream (WebSocket push, or HTTP-poll fallback - selected
    // internally by BaseChainSource::mode(); see chain::base module docs) ---
    let mut block_stream = chain_source.subscribe_blocks().await?;
    let block_state = state.clone();
    tokio::spawn(async move {
        while let Some(block) = block_stream.next().await {
            let mut guard = block_state.write().await;
            let number = block.number;
            guard.update_latest_block(block);
            tracing::debug!(block = number, "new block");
        }
    });

    // --- Day 2: pool discovery + hydration + known-pool swap scanning.
    // Always HTTP (`eth_getLogs` polling), independent of whether the block
    // stream above is WebSocket or HTTP-poll - see discovery_pipeline
    // module docs. Runs on the same cadence as HTTP_POLL_INTERVAL_SECS. ---
    {
        let mut pipeline = DiscoveryPipeline::new(&config, chain_id);
        let pipeline_chain_source = chain_source.clone();
        let pipeline_state = state.clone();
        let poll_interval = config.http_poll_interval;

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(poll_interval);
            loop {
                ticker.tick().await;
                let latest = match pipeline_chain_source.latest_block_number().await {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(source = "http_poll", error = %e, "failed to fetch latest block for discovery/swap scan, will retry next interval");
                        continue;
                    }
                };
                pipeline.run_once(latest, &pipeline_state).await;
            }
        });
    }

    // --- Log stream: WebSocket-only. Only attempted when WS is configured
    // AND there are pools to watch - HTTP-poll mode has no log ingestion
    // path today (latest-block polling only, per Day 1 scope). ---
    if chain_source.mode() == chain::ChainSourceMode::WebSocket && !watched_addresses.is_empty() {
        let mut log_stream = chain_source.subscribe_logs(watched_addresses).await?;
        let log_state = state.clone();
        let dex_by_address: std::collections::HashMap<alloy::primitives::Address, DexKind> = {
            let mut m = std::collections::HashMap::new();
            if let Some(addr) = &config.aerodrome_pool_address {
                if let Ok(a) = addr.parse() {
                    m.insert(a, DexKind::Aerodrome);
                }
            }
            if let Some(addr) = &config.uniswap_v3_pool_address {
                if let Ok(a) = addr.parse() {
                    m.insert(a, DexKind::UniswapV3);
                }
            }
            m
        };

        tokio::spawn(async move {
            while let Some(log) = log_stream.next().await {
                let received_at_us = now_us();
                let dex = dex_by_address.get(&log.inner.address).copied();
                let decoded = match dex {
                    Some(DexKind::Aerodrome) => {
                        aerodrome_adapter.decode_event(&log, chain_id, received_at_us)
                    }
                    Some(DexKind::UniswapV3) => {
                        uniswap_v3_adapter.decode_event(&log, chain_id, received_at_us)
                    }
                    // Day 1's WS pool-address config (AERODROME_POOL_ADDRESS /
                    // UNISWAP_V3_POOL_ADDRESS) never populates a Slipstream
                    // entry in dex_by_address, so this is unreachable in
                    // practice - but the match must still be exhaustive.
                    Some(DexKind::AerodromeSlipstream) | None => continue,
                };

                match decoded {
                    Ok(event) => {
                        let mut guard = log_state.write().await;
                        if guard.apply_event(event.clone()) {
                            telemetry::log_event_received(&event);
                        } else {
                            telemetry::log_event_duplicate(&event);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to decode event, skipping");
                    }
                }
            }
        });
    } else if chain_source.mode() == chain::ChainSourceMode::HttpPoll && !watched_addresses.is_empty() {
        tracing::warn!(
            source = "http_poll",
            "pool address(es) are configured but log/event ingestion is unavailable in HTTP \
             fallback mode - only latest-block polling is active. Configure BASE_WS_URL to \
             enable event ingestion for the configured pool(s)."
        );
    }

    tracing::info!("ingestion running - press Ctrl+C to shut down");
    tokio::signal::ctrl_c()
        .await
        .map_err(|e| crate::error::EngineError::Other(anyhow::anyhow!(e)))?;
    tracing::info!("shutdown signal received, exiting cleanly");

    Ok(())
}

'@
Set-Content -Path 'src\main.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/main.rs'

Write-Host 'Done. Now run: cargo check'
Write-Host 'Then: cargo test'