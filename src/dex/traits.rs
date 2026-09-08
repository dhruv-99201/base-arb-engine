//! Common DEX adapter interface.
//!
//! Day 1 scope: `decode_event` is fully implemented per adapter.
//! `get_pool_state` is best-effort/partial. `quote` explicitly returns
//! `NotImplemented` until the Day 2 pricing engine exists - it must never
//! fabricate a price. `build_swap_calldata` exists as a boundary for later
//! days but is never wired to any execution path today, and returns
//! `UnsafeOperation` if called, by design.

use crate::error::{EngineError, EngineResult};
use crate::events::model::MarketEvent;
use crate::market::models::{Pool, PoolState};
use alloy::primitives::{Address, Bytes, U256};
use alloy::rpc::types::Log as RpcLog;
use async_trait::async_trait;

#[async_trait]
pub trait DexAdapter: Send + Sync {
    fn name(&self) -> &'static str;

    /// Fetch on-chain state for `pool` via `rpc_url`. Day 1 implementations
    /// may be partial (e.g. Uniswap V3's full tick-bitmap hydration is not
    /// required yet) but must never invent values they didn't actually read.
    async fn get_pool_state(&self, rpc_url: &str, pool: &Pool) -> EngineResult<PoolState>;

    /// Decode a raw log known to originate from one of this adapter's pools
    /// into a normalized `MarketEvent`.
    fn decode_event(
        &self,
        log: &RpcLog,
        chain_id: u64,
        received_at_us: u64,
    ) -> EngineResult<MarketEvent>;

    /// Quote `amount_in` of one side of `pool` for the other side.
    /// Day 1: always `NotImplemented` - no pricing engine exists yet, and we
    /// do not fake quotes.
    async fn quote(
        &self,
        _pool: &PoolState,
        _amount_in: U256,
        _zero_for_one: bool,
    ) -> EngineResult<U256> {
        Err(EngineError::NotImplemented(format!(
            "{}: quote() requires the Day 2+ pricing engine",
            self.name()
        )))
    }

    /// Build calldata for a swap against this pool. Day 1: always refuses.
    /// There is no path from this function to a signed/submitted
    /// transaction anywhere in the codebase today.
    fn build_swap_calldata(
        &self,
        _pool: &Pool,
        _amount_in: U256,
        _min_amount_out: U256,
        _recipient: Address,
    ) -> EngineResult<Bytes> {
        Err(EngineError::UnsafeOperation(format!(
            "{}: build_swap_calldata() is not enabled - Day 1 has no execution path",
            self.name()
        )))
    }
}
