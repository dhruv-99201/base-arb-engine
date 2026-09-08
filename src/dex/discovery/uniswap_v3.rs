//! Uniswap V3 factory discovery.
//!
//! Event signature is Uniswap's well-known, extensively documented
//! `UniswapV3Factory.PoolCreated` - the same shape across every chain
//! Uniswap V3 is deployed on. Factory address for Base is verified against
//! Uniswap's official deployments page (see `config.rs`).

use crate::dex::discovery::{DiscoveredPool, DiscoveryParams, PoolDiscoveryAdapter};
use crate::error::{EngineError, EngineResult};
use crate::market::models::DexKind;
use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use alloy::sol_types::SolEvent;

sol! {
    event PoolCreated(
        address indexed token0,
        address indexed token1,
        uint24 indexed fee,
        int24 tickSpacing,
        address pool
    );
}

pub struct UniswapV3Discovery {
    factory_address: Address,
}

impl UniswapV3Discovery {
    pub fn new(factory_address: Address) -> Self {
        UniswapV3Discovery { factory_address }
    }
}

impl PoolDiscoveryAdapter for UniswapV3Discovery {
    fn dex(&self) -> DexKind {
        DexKind::UniswapV3
    }

    fn factory_address(&self) -> Address {
        self.factory_address
    }

    fn event_topic0(&self) -> B256 {
        PoolCreated::SIGNATURE_HASH
    }

    fn decode_pool_created(&self, log: &RpcLog) -> EngineResult<DiscoveredPool> {
        let block_number = log
            .block_number
            .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
        let tx_hash = log
            .transaction_hash
            .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;

        let decoded_log = PoolCreated::decode_log(&log.inner).map_err(|e| {
            EngineError::Decode(format!("failed to decode PoolCreated log: {e}"))
        })?;
        let decoded = decoded_log.data;

        Ok(DiscoveredPool {
            pool_address: decoded.pool,
            token0_address: decoded.token0,
            token1_address: decoded.token1,
            dex: DexKind::UniswapV3,
            params: DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: decoded.tickSpacing.as_i32(),
                fee: Some(decoded.fee.to::<u32>()),
            },
            block_number,
            tx_hash,
            factory_address: self.factory_address,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, Bytes, Log as PrimitiveLog};

    fn build_log(topics: Vec<B256>, data: Bytes, addr: Address) -> RpcLog {
        let inner = PrimitiveLog::new_unchecked(addr, topics, data);
        RpcLog {
            inner,
            block_hash: None,
            block_number: Some(999_000),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(0xCD)),
            transaction_index: Some(0),
            log_index: Some(1),
            removed: false,
        }
    }

    #[test]
    fn valid_pool_created_decodes() {
        let factory = address!("33128a8fC17869897dcE68Ed026d694621f6FDfD");
        let token0 = address!("4200000000000000000000000000000000000006");
        let token1 = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let pool = address!("d0b53D9277642d899DF5C87A3966A349A798F224");

        let event = PoolCreated {
            token0,
            token1,
            fee: alloy::primitives::aliases::U24::try_from(500u32).unwrap(),
            tickSpacing: alloy::primitives::aliases::I24::try_from(10i32).unwrap(),
            pool,
        };
        let encoded = event.encode_log_data();
        let log = build_log(encoded.topics().to_vec(), encoded.data.clone(), factory);

        let adapter = UniswapV3Discovery::new(factory);
        let discovered = adapter.decode_pool_created(&log).expect("should decode");

        assert_eq!(discovered.pool_address, pool);
        assert_eq!(discovered.token0_address, token0);
        assert_eq!(discovered.token1_address, token1);
        assert_eq!(discovered.dex, DexKind::UniswapV3);
        match discovered.params {
            DiscoveryParams::ConcentratedLiquidity { tick_spacing, fee } => {
                assert_eq!(tick_spacing, 10);
                assert_eq!(fee, Some(500));
            }
            other => panic!("expected ConcentratedLiquidity params, got {other:?}"),
        }
    }

    #[test]
    fn malformed_pool_created_is_rejected() {
        let factory = address!("33128a8fC17869897dcE68Ed026d694621f6FDfD");
        let bogus_topic = B256::repeat_byte(0x11);
        let log = build_log(vec![bogus_topic], Bytes::new(), factory);

        let adapter = UniswapV3Discovery::new(factory);
        assert!(adapter.decode_pool_created(&log).is_err());
    }
}
