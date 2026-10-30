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
use crate::market::models::{DexKind, Pool, PoolKind, PoolState};
use alloy::primitives::{Address, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{BlockId, Log as RpcLog};
use alloy::sol;
use async_trait::async_trait;
use std::str::FromStr;

sol! {
    #[sol(rpc)]
    interface IAerodromePool {
        function token0() external view returns (address);
        function token1() external view returns (address);
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

impl AerodromeAdapter {
    /// Block-pinned Aerodrome pool read (read-only).
    ///
    /// Every on-chain call below - `token0()`, `token1()`, `getReserves()`,
    /// `stable()`, and the factory's `getFee(pool, stable)` - uses ONE
    /// provider and ONE explicit `BlockId::number(block)` via
    /// `CallBuilder::block(...)`, the same convention
    /// `UniswapV3Adapter::get_pool_state_and_ticks_at_block` uses. There
    /// is no `get_block_number()` call and no latest-state call; the
    /// requested `block` itself is stored as
    /// `PoolState.freshness.last_updated_block`.
    ///
    /// Historical reads require an archive-capable RPC provider whenever
    /// `block` is outside the provider's retained state window. Any RPC
    /// failure is propagated as an `EngineError`; this method never falls
    /// back to latest state. The read is pinned by block NUMBER, not by
    /// EIP-1898 block hash, so it is not reorg/hash-anchored.
    ///
    /// The unpinned `DexAdapter::get_pool_state` is unchanged.
    pub async fn get_pool_state_at_block(
        &self,
        rpc_url: &str,
        pool: &Pool,
        block: u64,
    ) -> EngineResult<PoolState> {
        // Cheap, offline precondition: fail before any RPC if this is not
        // an Aerodrome pool identity at all.
        if pool.dex != DexKind::Aerodrome {
            return Err(wrong_dex_error(pool));
        }

        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let block_id = BlockId::number(block);
        let contract = IAerodromePool::new(pool.address, provider.clone());

        let token0 = contract
            .token0()
            .block(block_id)
            .call()
            .await
            .map_err(|e| self.read_error("token0()", block, e))?;
        let token1 = contract
            .token1()
            .block(block_id)
            .call()
            .await
            .map_err(|e| self.read_error("token1()", block, e))?;
        let reserves = contract
            .getReserves()
            .block(block_id)
            .call()
            .await
            .map_err(|e| self.read_error("getReserves()", block, e))?;
        let stable = contract
            .stable()
            .block(block_id)
            .call()
            .await
            .map_err(|e| self.read_error("stable()", block, e))?;

        let factory = IAerodromeFactory::new(self.factory_address, provider.clone());
        let fee_bps = factory
            .getFee(pool.address, stable)
            .block(block_id)
            .call()
            .await
            .map_err(|e| self.read_error("factory getFee()", block, e))?;

        build_pinned_pool_state(
            pool,
            block,
            token0,
            token1,
            reserves.reserve0,
            reserves.reserve1,
            stable,
            fee_bps,
        )
    }

    fn read_error(&self, call: &str, block: u64, e: impl std::fmt::Display) -> EngineError {
        EngineError::Dex {
            dex: self.name().into(),
            reason: format!("{call} at block {block} failed: {e}"),
        }
    }
}

fn wrong_dex_error(pool: &Pool) -> EngineError {
    EngineError::Dex {
        dex: pool.dex.name().into(),
        reason: format!(
            "AerodromeAdapter::get_pool_state_at_block: pool {} has dex={:?}, not \
             DexKind::Aerodrome",
            pool.address, pool.dex
        ),
    }
}

/// Pure, network-free construction of the pinned `PoolState` from values
/// already read on-chain at `block`. Exists separately from the RPC method
/// so every identity/consistency rule is unit-testable offline.
///
/// - Rejects `pool.dex != DexKind::Aerodrome`.
/// - Rejects zero or identical on-chain token addresses.
/// - If `pool` already carries a non-zero token0/token1 address, it must
///   equal the on-chain value; a mismatch is an error. Reserves are never
///   silently reordered and an existing identity is never rewritten.
/// - Zero (placeholder) addresses are filled from the on-chain values.
/// - `symbol`/`decimals` are preserved exactly as supplied.
/// - `fee_bps` is always `Some(fee_bps)` - the real factory value, never
///   assumed zero.
#[allow(clippy::too_many_arguments)]
fn build_pinned_pool_state(
    pool: &Pool,
    block: u64,
    onchain_token0: Address,
    onchain_token1: Address,
    reserve0: U256,
    reserve1: U256,
    stable: bool,
    fee_bps: U256,
) -> EngineResult<PoolState> {
    if pool.dex != DexKind::Aerodrome {
        return Err(wrong_dex_error(pool));
    }
    if onchain_token0 == Address::ZERO || onchain_token1 == Address::ZERO {
        return Err(EngineError::Dex {
            dex: "aerodrome".into(),
            reason: format!(
                "pool {} returned a zero token address at block {block} \
                 (token0={onchain_token0}, token1={onchain_token1})",
                pool.address
            ),
        });
    }
    if onchain_token0 == onchain_token1 {
        return Err(EngineError::Dex {
            dex: "aerodrome".into(),
            reason: format!(
                "pool {} returned identical token0/token1 {onchain_token0} at block {block}",
                pool.address
            ),
        });
    }
    for (label, supplied, onchain) in [
        ("token0", pool.token0.address, onchain_token0),
        ("token1", pool.token1.address, onchain_token1),
    ] {
        if supplied != Address::ZERO && supplied != onchain {
            return Err(EngineError::State(format!(
                "pool {} supplied {label}={supplied} does not match on-chain {label}={onchain} \
                 at block {block}; refusing to rewrite the pool identity or reorder reserves",
                pool.address
            )));
        }
    }

    let mut updated_pool = pool.clone();
    updated_pool.token0.address = onchain_token0;
    updated_pool.token1.address = onchain_token1;
    updated_pool.kind = PoolKind::Aerodrome {
        reserve0,
        reserve1,
        stable,
        fee_bps: Some(fee_bps),
    };

    Ok(PoolState::new(updated_pool, block, None))
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

    fn t(address: Address, symbol: &str, decimals: u8) -> Token {
        Token {
            address,
            symbol: symbol.into(),
            decimals,
        }
    }

    const WETH: Address = address!("4200000000000000000000000000000000000006");
    const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    const POOL: Address = address!("cDAC0d6c6C59727a65F871236188350531885C43");

    fn placeholder(dex: DexKind, t0: Address, t1: Address) -> Pool {
        Pool {
            address: POOL,
            dex,
            token0: t(t0, "T0", 7),
            token1: t(t1, "T1", 9),
            kind: PoolKind::Aerodrome {
                reserve0: U256::ZERO,
                reserve1: U256::ZERO,
                stable: false,
                fee_bps: None,
            },
        }
    }

    fn build(pool: &Pool, t0: Address, t1: Address) -> EngineResult<PoolState> {
        build_pinned_pool_state(
            pool,
            26_000_000,
            t0,
            t1,
            U256::from(111u64),
            U256::from(222u64),
            false,
            U256::from(30u64),
        )
    }

    #[test]
    fn pinned_state_populates_tokens_reserves_stable_fee_and_block() {
        let pool = placeholder(DexKind::Aerodrome, Address::ZERO, Address::ZERO);
        let state = build(&pool, WETH, USDC).unwrap();
        assert_eq!(state.pool.token0.address, WETH);
        assert_eq!(state.pool.token1.address, USDC);
        assert_eq!(state.freshness.last_updated_block, 26_000_000);
        match state.pool.kind {
            PoolKind::Aerodrome {
                reserve0,
                reserve1,
                stable,
                fee_bps,
            } => {
                assert_eq!(reserve0, U256::from(111u64));
                assert_eq!(reserve1, U256::from(222u64));
                assert!(!stable);
                assert_eq!(fee_bps, Some(U256::from(30u64)));
            }
            other => panic!("expected Aerodrome kind, got {other:?}"),
        }
    }

    #[test]
    fn pinned_state_maps_stable_true_and_real_fee() {
        let pool = placeholder(DexKind::Aerodrome, Address::ZERO, Address::ZERO);
        let state = build_pinned_pool_state(
            &pool,
            5,
            WETH,
            USDC,
            U256::from(1u64),
            U256::from(2u64),
            true,
            U256::from(4u64),
        )
        .unwrap();
        match state.pool.kind {
            PoolKind::Aerodrome { stable, fee_bps, .. } => {
                assert!(stable);
                assert_eq!(fee_bps, Some(U256::from(4u64)));
            }
            other => panic!("expected Aerodrome kind, got {other:?}"),
        }
        assert_eq!(state.freshness.last_updated_block, 5);
    }

    #[test]
    fn pinned_state_preserves_symbol_and_decimals() {
        let pool = placeholder(DexKind::Aerodrome, Address::ZERO, Address::ZERO);
        let state = build(&pool, WETH, USDC).unwrap();
        assert_eq!(state.pool.token0.symbol, "T0");
        assert_eq!(state.pool.token0.decimals, 7);
        assert_eq!(state.pool.token1.symbol, "T1");
        assert_eq!(state.pool.token1.decimals, 9);
    }

    #[test]
    fn pinned_state_rejects_wrong_dex() {
        let pool = placeholder(DexKind::UniswapV3, Address::ZERO, Address::ZERO);
        let err = build(&pool, WETH, USDC).unwrap_err();
        assert!(matches!(err, EngineError::Dex { .. }), "got {err:?}");
    }

    #[test]
    fn pinned_state_rejects_zero_onchain_tokens() {
        let pool = placeholder(DexKind::Aerodrome, Address::ZERO, Address::ZERO);
        assert!(matches!(
            build(&pool, Address::ZERO, USDC).unwrap_err(),
            EngineError::Dex { .. }
        ));
        assert!(matches!(
            build(&pool, WETH, Address::ZERO).unwrap_err(),
            EngineError::Dex { .. }
        ));
    }

    #[test]
    fn pinned_state_rejects_identical_onchain_tokens() {
        let pool = placeholder(DexKind::Aerodrome, Address::ZERO, Address::ZERO);
        let err = build(&pool, WETH, WETH).unwrap_err();
        assert!(matches!(err, EngineError::Dex { .. }), "got {err:?}");
    }

    #[test]
    fn pinned_state_rejects_existing_identity_mismatch() {
        // Swapped ordering relative to chain - must be rejected, not reordered.
        let swapped = placeholder(DexKind::Aerodrome, USDC, WETH);
        let err = build(&swapped, WETH, USDC).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");

        // Only token1 supplied and wrong.
        let partial = placeholder(DexKind::Aerodrome, Address::ZERO, WETH);
        let err = build(&partial, WETH, USDC).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    #[test]
    fn pinned_state_accepts_matching_existing_identity() {
        let pool = placeholder(DexKind::Aerodrome, WETH, USDC);
        let state = build(&pool, WETH, USDC).unwrap();
        assert_eq!(state.pool.token0.address, WETH);
        assert_eq!(state.pool.token1.address, USDC);
    }

    #[tokio::test]
    async fn pinned_read_rejects_wrong_dex_before_any_rpc() {
        // Unroutable URL: if this reached the network it would not produce
        // a Dex wrong-dex error.
        let pool = placeholder(DexKind::UniswapV3, Address::ZERO, Address::ZERO);
        let err = AerodromeAdapter::new()
            .get_pool_state_at_block("http://127.0.0.1:1", &pool, 1)
            .await
            .unwrap_err();
        match err {
            EngineError::Dex { reason, .. } => assert!(reason.contains("not"), "{reason}"),
            other => panic!("expected Dex error, got {other:?}"),
        }
    }
}
