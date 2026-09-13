//! Exact, no-floating-point pricing math for the Day 3 opportunity scanner.
//!
//! **Status: partial.** `full_math`, `tick_math`, `tick_bitmap`,
//! `sqrt_price_math`, `swap_math`, and `aerodrome_volatile` were already
//! implemented and tested (see the earlier Day 3 status report). This
//! update adds `v3_quote`: the full multi-tick exact-input quote loop
//! wiring the above together over REAL hydrated tick data - see that
//! module's docs, especially the mandatory "explicit incomplete-state
//! rejection" contract. `aerodrome_volatile` gained a
//! `quote_pool_exact_input` integration wrapper over the real
//! `Pool`/`PoolKind` model (including `fee_bps`, now sourced from the real
//! Aerodrome factory `getFee()` - see `dex::aerodrome`).
//!
//! `v3_quote::quote_exact_input` explicitly terminates (rather than
//! looping) when a trade would need to walk past the global `MIN_TICK`/
//! `MAX_TICK` bound with input still unconsumed, and its `V3QuoteResult`
//! carries `amount_in`/`fee_paid`/`liquidity_after` alongside the
//! original `amount_out`/`ending_sqrt_price_x96`/`ending_tick`/
//! `ticks_crossed` - see that module's docs.
//!
//! Still missing / explicitly out of scope for this update (see the Day 3
//! income-validation spec): the cross-DEX opportunity engine, the
//! trade-size ladder, the economic model, the journal, live dry-run mode,
//! REVM simulation, flash loans, the executor, Aerodrome stable-curve
//! pricing, and Aerodrome Slipstream pricing. Also still missing: real
//! on-chain golden comparison tests - this environment has no live Base
//! RPC access, so nothing here has been exercised against a real pool
//! (see `dex::uniswap_v3::UniswapV3Adapter::hydrate_initialized_ticks`'s
//! docs for exactly what's unverified there), and a working pinned-block
//! read path - see `dex::uniswap_v3::UniswapV3Adapter::
//! get_pool_state_and_ticks_at_block`'s docs for what's implemented there
//! (the interface) versus not (the actual block-pinned RPC calls).

pub mod aerodrome_volatile;
pub mod full_math;
pub mod sqrt_price_math;
pub mod swap_math;
pub mod tick_bitmap;
pub mod tick_math;
pub mod v3_quote;
