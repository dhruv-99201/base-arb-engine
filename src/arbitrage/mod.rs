//! Pure, network-free cross-DEX arbitrage opportunity evaluation.
//!
//! **Status: raw round-trip arithmetic only.** This module answers exactly
//! one question: given two already-computed leg quotes (one per DEX), do
//! they form a valid two-leg round trip, and if so what is the gross
//! profit? It does not call any DEX pricing engine itself, does not make
//! RPC calls, does not estimate gas, does not apply a profitability
//! threshold, and does not execute anything - see `opportunity` module
//! docs for the exact boundary.

pub mod opportunity;
pub mod quote;

pub use opportunity::{LegQuote, Opportunity};
pub use quote::{quote_aerodrome_leg, quote_uniswap_v3_leg};
