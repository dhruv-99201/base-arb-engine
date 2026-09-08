//! Aerodrome adapter.
//!
//! Aerodrome pools are Solidly-style: a `stable` (curve-like) formula and a
//! `volatile` (x*y=k) formula are two different pool types that share an
//! interface, not one model with a flag that changes math elsewhere. This
//! adapter only exposes the raw on-chain reserves + the `stable` flag; it
//! does not implement swap math itself (that's Day 2+).
//!
//! Pool addresses are never invented. If `pool.address` isn't a verified,
//! configured address, callers are responsible for not calling this adapter
//! against it - see `Config::aerodrome_pool_address` and the README.

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
    interface IAerodromePool {
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
        function stable() external view returns (bool);
    }
}

pub struct AerodromeAdapter;

impl AerodromeAdapter {
    pub fn new() -> Self {
        AerodromeAdapter
    }
}

impl Default for AerodromeAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DexAdapter for AerodromeAdapter {
    fn name(&self) -> &'static str {
        "aerodrome"
    }

    async fn get_pool_state(&self, rpc_url: &str, pool: &Pool) -> EngineResult<PoolState> {
        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);

        let contract = IAerodromePool::new(pool.address, provider.clone());

        let reserves = contract
            .getReserves()
            .call()
            .await
            .map_err(|e| EngineError::Dex {
                dex: self.name().into(),
                reason: format!("getReserves() failed: {e}"),
            })?;

        let stable = contract.stable().call().await.map_err(|e| EngineError::Dex {
            dex: self.name().into(),
            reason: format!("stable() failed: {e}"),
        })?;
        let block_number = provider
            .get_block_number()
            .await
            .map_err(|e| EngineError::Chain(format!("get_block_number failed: {e}")))?;

        let mut updated_pool = pool.clone();
        updated_pool.kind = PoolKind::Aerodrome {
            reserve0: reserves.reserve0,
            reserve1: reserves.reserve1,
            stable,
        };

        Ok(PoolState::new(updated_pool, block_number, None))
    }

    fn decode_event(
        &self,
        log: &RpcLog,
        chain_id: u64,
        received_at_us: u64,
    ) -> EngineResult<MarketEvent> {
        decoder::decode_aerodrome_log(log, chain_id, received_at_us)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::models::{DexKind, Token};
    use alloy::primitives::{address, U256};

    #[test]
    fn aerodrome_pool_model_can_be_created() {
        let pool = Pool {
            address: address!("0000000000000000000000000000000000000001"),
            dex: DexKind::Aerodrome,
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
            kind: PoolKind::Aerodrome {
                reserve0: U256::from(1_000_000u64),
                reserve1: U256::from(2_000_000u64),
                stable: false,
            },
        };

        match pool.kind {
            PoolKind::Aerodrome { stable, .. } => assert!(!stable),
            _ => panic!("expected Aerodrome pool kind"),
        }
    }
}
