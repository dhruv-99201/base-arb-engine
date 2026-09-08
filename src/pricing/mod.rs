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
