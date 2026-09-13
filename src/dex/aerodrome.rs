//! Aerodrome adapter.
//!
//! Aerodrome pools are Solidly-style: a `stable` (curve-like) formula and a
//! `volatile` (x*y=k) formula are two different pool types that share an
//! interface, not one model with a flag that changes math elsewhere. This
//! adapter reads the raw on-chain reserves + the `stable` flag, AND (Day 3)
//! the pool's real per-pool fee via the factory's `getFee(pool, stable)` -
//! it does not implement swap math itself (that's
//! `pricing::aerodrome_volatile`).
//!
//! Pool addresses are never invented. If `pool.address` isn't a verified,
//! configured address, callers are responsible for not calling this adapter
//! against it - see `Config::aerodrome_pool_address` and the README.

use crate::dex::traits::DexAdapter;
use crate::error::{EngineError, EngineResult};
use crate::events::decoder;
use crate::events::model::MarketEvent;
use crate::market::models::{Pool, PoolKind, PoolState};
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::Log as RpcLog;
use alloy::sol;
use async_trait::async_trait;
use std::str::FromStr;

sol! {
    #[sol(rpc)]
    interface IAerodromePool {
        function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
        function stable() external view returns (bool);
    }
}

sol! {
    #[sol(rpc)]
    interface IAerodromeFactory {
        /// Real Aerodrome `PoolFactory.getFee(pool, stable)` - returns the
        /// pool's fee in basis points (same `amount * fee / 10_000`
        /// convention `pricing::aerodrome_volatile::
        /// quote_exact_input_aerodrome_volatile` already assumes). Never
        /// hardcoded here: Aerodrome allows per-pool fee overrides, so the
        /// only correct source is this live call.
        function getFee(address pool, bool stable) external view returns (uint256);
    }
}

pub struct AerodromeAdapter {
    factory_address: Address,
}

impl AerodromeAdapter {
    /// Uses the well-known, verified Base Aerodrome `PoolFactory` address
    /// (`config::DEFAULT_AERODROME_FACTORY` - the same constant
    /// `Config::load()` defaults to, so this adapter's fee lookups target
    /// the same factory the rest of the engine assumes unless overridden).
    pub fn new() -> Self {
        let factory_address = Address::from_str(crate::config::DEFAULT_AERODROME_FACTORY)
            .expect("DEFAULT_AERODROME_FACTORY must be a valid address");
        AerodromeAdapter { factory_address }
    }

    /// Explicit override - e.g. a non-Base deployment, or a forked/staging
    /// factory. Never silently falls back to a different address than
    /// what's asked for.
    pub fn with_factory_address(factory_address: Address) -> Self {
        AerodromeAdapter { factory_address }
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

        // Day 3: real per-pool fee from the factory - never a hardcoded
        // constant (Aerodrome supports per-pool fee overrides).
        let factory = IAerodromeFactory::new(self.factory_address, provider.clone());
        let fee_bps = factory
            .getFee(pool.address, stable)
            .call()
            .await
            .map_err(|e| EngineError::Dex {
                dex: self.name().into(),
                reason: format!("factory getFee() failed: {e}"),
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
            // A successful real factory call always yields a known fee -
            // Some(_), never the ambiguous "unhydrated" None state (that
            // state only exists for pools this adapter hasn't read yet;
            // see PoolKind::Aerodrome::fee_bps docs).
            fee_bps: Some(fee_bps),
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
                fee_bps: Some(U256::from(30u64)),
            },
        };

        match pool.kind {
            PoolKind::Aerodrome { stable, fee_bps, .. } => {
                assert!(!stable);
                assert_eq!(fee_bps, Some(U256::from(30u64)));
            }
            _ => panic!("expected Aerodrome pool kind"),
        }
    }

    #[test]
    fn unhydrated_pool_kind_uses_none_not_zero() {
        // A freshly-discovered, not-yet-read pool must use None, never
        // Some(U256::ZERO) - the two are not interchangeable (see
        // PoolKind::Aerodrome::fee_bps docs).
        let pool = Pool {
            address: address!("0000000000000000000000000000000000000004"),
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
                reserve0: U256::ZERO,
                reserve1: U256::ZERO,
                stable: false,
                fee_bps: None,
            },
        };

        match pool.kind {
            PoolKind::Aerodrome { fee_bps, .. } => assert_eq!(fee_bps, None),
            _ => panic!("expected Aerodrome pool kind"),
        }
    }

    #[test]
    fn new_uses_the_well_known_verified_base_factory() {
        let adapter = AerodromeAdapter::new();
        let expected = Address::from_str(crate::config::DEFAULT_AERODROME_FACTORY).unwrap();
        assert_eq!(adapter.factory_address, expected);
    }

    #[test]
    fn with_factory_address_overrides_the_default() {
        let custom = address!("1111111111111111111111111111111111111111");
        let adapter = AerodromeAdapter::with_factory_address(custom);
        assert_eq!(adapter.factory_address, custom);
    }

    #[test]
    fn default_impl_matches_new() {
        let via_default = AerodromeAdapter::default();
        let via_new = AerodromeAdapter::new();
        assert_eq!(via_default.factory_address, via_new.factory_address);
    }
}
