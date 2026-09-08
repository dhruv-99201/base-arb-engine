//! Aerodrome Slipstream (`CLPool`) adapter.
//!
//! Concentrated liquidity, structurally close to Uniswap V3 but NOT
//! ABI-identical - confirmed by reading the real `CLPool` source (verified
//! contract on BaseScan). Notably, `CLPool.slot0()` returns a 6-field tuple
//! (no `feeProtocol`, unlike `IUniswapV3Pool.slot0()`'s 7 fields), so Day
//! 1's `UniswapV3Adapter` cannot be reused as-is for Slipstream pools - the
//! ABI mismatch would silently misdecode the return data.
//!
//! Fee is intentionally NOT hydrated here: Slipstream fees are not a simple
//! per-pool constant (they route through `CLFactory.getSwapFee(pool)`,
//! which additionally depends on gauge/fee-module state) - out of scope for
//! Day 2 state hydration. `fee_tier` is stored as `0` as an explicit
//! placeholder, not a real value; the future pricing layer must resolve fee
//! separately before using this for quote math.

use crate::dex::traits::DexAdapter;
use crate::error::{EngineError, EngineResult};
use crate::events::decoder;
use crate::events::model::MarketEvent;
use crate::market::models::{Pool, PoolKind, PoolState};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use async_trait::async_trait;

sol! {
    #[sol(rpc)]
    interface ICLPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function tickSpacing() external view returns (int24);
    }
}

pub struct AerodromeSlipstreamAdapter;

impl AerodromeSlipstreamAdapter {
    pub fn new() -> Self {
        AerodromeSlipstreamAdapter
    }
}

impl Default for AerodromeSlipstreamAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DexAdapter for AerodromeSlipstreamAdapter {
    fn name(&self) -> &'static str {
        "aerodrome_slipstream"
    }

    async fn get_pool_state(&self, rpc_url: &str, pool: &Pool) -> EngineResult<PoolState> {
        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let contract = ICLPool::new(pool.address, provider.clone());

        let slot0 = contract.slot0().call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("slot0() failed: {e}"),
        })?;
        let liquidity = contract
            .liquidity()
            .call()
            .await
            .map_err(|e| EngineError::Dex {
                dex: self.name().into(),
                reason: format!("liquidity() failed: {e}"),
            })?;
        let tick_spacing = contract
            .tickSpacing()
            .call()
            .await
            .map_err(|e| EngineError::Dex {
                dex: self.name().into(),
                reason: format!("tickSpacing() failed: {e}"),
            })?;

        let block_number = provider
            .get_block_number()
            .await
            .map_err(|e| EngineError::Chain(format!("get_block_number failed: {e}")))?;

        let mut updated_pool = pool.clone();
        updated_pool.kind = PoolKind::ConcentratedLiquidity {
            // Placeholder - see module docs. Never treat as a real fee.
            fee_tier: 0,
            tick_spacing: tick_spacing.as_i32(),
            sqrt_price_x96: alloy::primitives::U256::from(slot0.sqrtPriceX96),
            current_tick: slot0.tick.as_i32(),
            liquidity,
            initialized_ticks: Default::default(),
        };

        Ok(PoolState::new(updated_pool, block_number, None))
    }

    fn decode_event(
        &self,
        log: &RpcLog,
        chain_id: u64,
        received_at_us: u64,
    ) -> EngineResult<MarketEvent> {
        decoder::decode_aerodrome_slipstream_log(log, chain_id, received_at_us)
    }
}
