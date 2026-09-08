//! Uniswap V3 adapter.
//!
//! Uniswap V3 is concentrated liquidity, NOT a two-reserve constant-product
//! pool. This adapter reads `slot0` (sqrtPriceX96, tick) and `liquidity`
//! directly - it does not synthesize fake reserves. Full tick-bitmap
//! hydration and the swap-simulation math are Day 2+ (they require walking
//! initialized ticks, which needs either many RPC calls or a dedicated
//! indexer - out of scope for the Day 1 foundation).

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
    interface IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );
        function liquidity() external view returns (uint128);
        function fee() external view returns (uint24);
        function tickSpacing() external view returns (int24);
    }
}

pub struct UniswapV3Adapter;

impl UniswapV3Adapter {
    pub fn new() -> Self {
        UniswapV3Adapter
    }
}

impl Default for UniswapV3Adapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DexAdapter for UniswapV3Adapter {
    fn name(&self) -> &'static str {
        "uniswap_v3"
    }

    async fn get_pool_state(&self, rpc_url: &str, pool: &Pool) -> EngineResult<PoolState> {
        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let contract = IUniswapV3Pool::new(pool.address, provider.clone());

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
        let fee = contract.fee().call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("fee() failed: {e}"),
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

        // Field access verified against alloy-core 1.6.0 / alloy 2.4.1
        // source: multi-output Sol functions (slot0) generate a named
        // struct (`.sqrtPriceX96`, `.tick`, ...); single-output functions
        // (fee, tickSpacing, liquidity) return the bare Rust type directly
        // (no `._0` wrapper). Sub-word Sol ints (`uint24`/`int24`) are
        // `ruint` `Uint`/`Signed` wrapper types converted via
        // `.to::<T>()`/`.as_i32()`.
        let mut updated_pool = pool.clone();
        updated_pool.kind = PoolKind::ConcentratedLiquidity {
            fee_tier: fee.to::<u32>(),
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
        decoder::decode_uniswap_v3_log(log, chain_id, received_at_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::models::{DexKind, Token};
    use alloy::primitives::{address, U256};

    #[test]
    fn uniswap_v3_pool_model_can_represent_v3_state() {
        let pool = Pool {
            address: address!("0000000000000000000000000000000000000003"),
            dex: DexKind::UniswapV3,
            token0: Token {
                address: address!("4200000000000000000000000000000000000006"),
                symbol: "WETH".into(),
                decimals: 18,
            },
            token1: Token {
                address: address!("0000000000000000000000000000000000000002"),
                symbol: "USDC".into(),
                decimals: 6,
            },
            kind: PoolKind::ConcentratedLiquidity {
                fee_tier: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::from(79_228_162_514_264_337_593_543_950_336u128),
                current_tick: -1234,
                liquidity: 123_456_789,
                initialized_ticks: Default::default(),
            },
        };

        match pool.kind {
            PoolKind::ConcentratedLiquidity {
                fee_tier,
                current_tick,
                ..
            } => {
                assert_eq!(fee_tier, 500);
                assert_eq!(current_tick, -1234);
            }
            _ => panic!("expected UniswapV3 pool kind - not a fake x*y=k model"),
        }
    }
}
