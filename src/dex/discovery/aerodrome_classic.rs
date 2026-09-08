//! Aerodrome classic (Solidly-style) factory discovery.
//!
//! Event signature confirmed directly against the verified `PoolFactory`
//! source on BaseScan (address 0x420DD381b31aEf6683db6B902084cB0FFECe40Da,
//! labeled "Aerodrome: Pool Factory"):
//! `event PoolCreated(address indexed token0, address indexed token1, bool
//! indexed stable, address pool, uint256);` - the trailing `uint256` is
//! unnamed in the source (it's `allPools.length - 1`, the pool's index) and
//! is not needed here, so it's decoded but discarded.

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
        bool indexed stable,
        address pool,
        uint256 poolIndex
    );
}

pub struct AerodromeClassicDiscovery {
    factory_address: Address,
}

impl AerodromeClassicDiscovery {
    pub fn new(factory_address: Address) -> Self {
        AerodromeClassicDiscovery { factory_address }
    }
}

impl PoolDiscoveryAdapter for AerodromeClassicDiscovery {
    fn dex(&self) -> DexKind {
        DexKind::Aerodrome
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
            dex: DexKind::Aerodrome,
            params: DiscoveryParams::AerodromeClassic {
                stable: decoded.stable,
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
    use alloy::primitives::{address, Bytes, Log as PrimitiveLog, U256};

    fn build_log(topics: Vec<B256>, data: Bytes, addr: Address) -> RpcLog {
        let inner = PrimitiveLog::new_unchecked(addr, topics, data);
        RpcLog {
            inner,
            block_hash: None,
            block_number: Some(500_000),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(0xEF)),
            transaction_index: Some(0),
            log_index: Some(2),
            removed: false,
        }
    }

    #[test]
    fn valid_classic_pool_created_decodes() {
        let factory = address!("420DD381b31aEf6683db6B902084cB0FFECe40Da");
        let token0 = address!("4200000000000000000000000000000000000006");
        let token1 = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
        let pool = address!("1111111111111111111111111111111111111111");

        let event = PoolCreated {
            token0,
            token1,
            stable: false,
            pool,
            poolIndex: U256::from(42u64),
        };
        let encoded = event.encode_log_data();
        let log = build_log(encoded.topics().to_vec(), encoded.data.clone(), factory);

        let adapter = AerodromeClassicDiscovery::new(factory);
        let discovered = adapter.decode_pool_created(&log).expect("should decode");

        assert_eq!(discovered.pool_address, pool);
        assert_eq!(discovered.dex, DexKind::Aerodrome);
        match discovered.params {
            DiscoveryParams::AerodromeClassic { stable } => assert!(!stable),
            other => panic!("expected AerodromeClassic params, got {other:?}"),
        }
    }

    #[test]
    fn malformed_classic_pool_created_is_rejected() {
        let factory = address!("420DD381b31aEf6683db6B902084cB0FFECe40Da");
        let bogus_topic = B256::repeat_byte(0x22);
        let log = build_log(vec![bogus_topic], Bytes::new(), factory);

        let adapter = AerodromeClassicDiscovery::new(factory);
        assert!(adapter.decode_pool_created(&log).is_err());
    }
}
