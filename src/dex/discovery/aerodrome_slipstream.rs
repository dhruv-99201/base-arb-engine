//! Aerodrome Slipstream (concentrated-liquidity) `CLFactory` discovery.
//!
//! IMPORTANT - PARTIALLY VERIFIED:
//! The event *emission* is confirmed directly from `CLFactory.sol`'s real
//! source (github.com/aerodrome-finance/slipstream, `createPool`):
//! `emit PoolCreated(token0, token1, tickSpacing, pool);`
//!
//! However, the exact *indexed* flags for each parameter could not be
//! independently confirmed from `ICLFactory.sol`'s interface declaration
//! (not fetched). This adapter assumes `token0`, `token1`, and
//! `tickSpacing` are indexed and `pool` is not - matching both Uniswap V3's
//! analogous `PoolCreated` event and Aerodrome's own classic
//! `PoolFactory.PoolCreated` (both of which put exactly the first three
//! logical fields in topics). This is a well-justified inference, not a
//! confirmed fact - if `decode_pool_created` starts failing against real
//! Slipstream pool-creation logs, this is the first place to check (topic0
//! itself is unaffected by this uncertainty since the field order/types are
//! confirmed - only the topics/data split could be wrong).
//!
//! The factory address is NOT defaulted anywhere in this codebase (see
//! `config.rs`) for the same reason - use only after verifying it yourself.

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
        int24 indexed tickSpacing,
        address pool
    );
}

pub struct AerodromeSlipstreamDiscovery {
    factory_address: Address,
}

impl AerodromeSlipstreamDiscovery {
    pub fn new(factory_address: Address) -> Self {
        AerodromeSlipstreamDiscovery { factory_address }
    }
}

impl PoolDiscoveryAdapter for AerodromeSlipstreamDiscovery {
    fn dex(&self) -> DexKind {
        DexKind::AerodromeSlipstream
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
            EngineError::Decode(format!(
                "failed to decode PoolCreated log: {e}"
            ))
        })?;
        let decoded = decoded_log.data;

        Ok(DiscoveredPool {
            pool_address: decoded.pool,
            token0_address: decoded.token0,
            token1_address: decoded.token1,
            dex: DexKind::AerodromeSlipstream,
            params: DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: decoded.tickSpacing.as_i32(),
                // Slipstream's CLFactory.PoolCreated event carries no fee
                // parameter - see module docs.
                fee: None,
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
            block_number: Some(600_000),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(0x9A)),
            transaction_index: Some(0),
            log_index: Some(0),
            removed: false,
        }
    }

    #[test]
    fn valid_slipstream_pool_created_decodes() {
        let factory = Address::from_slice(&[0xC1; 20]);
        let token0 = address!("4200000000000000000000000000000000000006");
        let token1 = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let pool = address!("2222222222222222222222222222222222222222");

        let event = PoolCreated {
            token0,
            token1,
            tickSpacing: alloy::primitives::aliases::I24::try_from(100i32).unwrap(),
            pool,
        };
        let encoded = event.encode_log_data();
        let log = build_log(encoded.topics().to_vec(), encoded.data.clone(), factory);

        let adapter = AerodromeSlipstreamDiscovery::new(factory);
        let discovered = adapter.decode_pool_created(&log).expect("should decode");

        assert_eq!(discovered.pool_address, pool);
        assert_eq!(discovered.dex, DexKind::AerodromeSlipstream);
        match discovered.params {
            DiscoveryParams::ConcentratedLiquidity { tick_spacing, fee } => {
                assert_eq!(tick_spacing, 100);
                assert_eq!(fee, None);
            }
            other => panic!("expected ConcentratedLiquidity params, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_or_malformed_pool_created_is_handled_safely() {
        let factory = Address::from_slice(&[0xC1; 20]);
        let bogus_topic = B256::repeat_byte(0x33);
        let log = build_log(vec![bogus_topic], Bytes::new(), factory);

        let adapter = AerodromeSlipstreamDiscovery::new(factory);
        // Must return a clean Err, never panic and never fabricate a pool.
        assert!(adapter.decode_pool_created(&log).is_err());
    }
}
