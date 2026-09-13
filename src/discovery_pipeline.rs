//! Orchestrates Day 2's HTTP-only pipeline: scan factories for newly
//! created pools, hydrate them (token metadata + on-chain state), then scan
//! already-known pools for swap events and fold them into `MarketState`.
//!
//! Deliberately transport-independent at the boundary that matters: this
//! module talks to `HttpLogPoller` directly (HTTP `eth_getLogs`), but
//! everything downstream of "raw `RpcLog`" - decoding, dedup (via
//! `MarketState::apply_event`), state application - is the exact same code
//! Day 1's WebSocket path uses. A future low-latency/WS feed for discovery
//! and swaps would plug in beside `HttpLogPoller`, not replace this
//! decode/apply logic.

use crate::chain::log_poller::{HttpLogPoller, LogPollCheckpoint};
use crate::config::{Config, LogStartBlock};
use crate::dex::discovery::{
    AerodromeClassicDiscovery, AerodromeSlipstreamDiscovery, DiscoveryParams, PoolDiscoveryAdapter,
    UniswapV3Discovery,
};
use crate::dex::traits::DexAdapter;
use crate::dex::{AerodromeAdapter, AerodromeSlipstreamAdapter, UniswapV3Adapter};
use crate::events::decoder::{aerodrome_swap_topic0, now_us, uniswap_v3_swap_topic0};
use crate::market::models::{DexKind, Pool, PoolKind, Token};
use crate::market::SharedMarketState;
use crate::pools::models::{DiscoverySource, PoolEligibility, PoolStatus};
use crate::pools::{PoolRegistry, TokenMetadataCache};
use alloy::primitives::{Address, U256};
use std::collections::HashMap;

pub struct DiscoveryPipeline {
    rpc_url: String,
    chain_id: u64,
    log_start_block: LogStartBlock,
    log_poller: HttpLogPoller,

    discovery_adapters: Vec<Box<dyn PoolDiscoveryAdapter>>,
    dex_adapters: HashMap<DexKind, Box<dyn DexAdapter>>,

    pub registry: PoolRegistry,
    token_cache: TokenMetadataCache,

    discovery_checkpoint: LogPollCheckpoint,
    swap_checkpoint: LogPollCheckpoint,
}

impl DiscoveryPipeline {
    pub fn new(config: &Config, chain_id: u64) -> Self {
        let mut discovery_adapters: Vec<Box<dyn PoolDiscoveryAdapter>> = vec![
            Box::new(UniswapV3Discovery::new(config.uniswap_v3_factory_address)),
            Box::new(AerodromeClassicDiscovery::new(
                config.aerodrome_factory_address,
            )),
        ];
        if config.aerodrome_slipstream_factory_addresses.is_empty() {
            tracing::info!(
                "AERODROME_SLIPSTREAM_FACTORY_ADDRESSES is empty - Slipstream pool discovery \
                 is disabled."
            );
        } else {
            for addr in &config.aerodrome_slipstream_factory_addresses {
                discovery_adapters.push(Box::new(AerodromeSlipstreamDiscovery::new(*addr)));
            }
        }

        let mut dex_adapters: HashMap<DexKind, Box<dyn DexAdapter>> = HashMap::new();
        dex_adapters.insert(DexKind::UniswapV3, Box::new(UniswapV3Adapter::new()));
        dex_adapters.insert(DexKind::Aerodrome, Box::new(AerodromeAdapter::new()));
        dex_adapters.insert(
            DexKind::AerodromeSlipstream,
            Box::new(AerodromeSlipstreamAdapter::new()),
        );

        DiscoveryPipeline {
            rpc_url: config.base_rpc_url.clone(),
            chain_id,
            log_start_block: config.log_start_block,
            log_poller: HttpLogPoller::new(
                config.base_rpc_url.clone(),
                config.log_poll_max_block_range,
            ),
            discovery_adapters,
            dex_adapters,
            registry: PoolRegistry::new(),
            token_cache: TokenMetadataCache::new(),
            discovery_checkpoint: LogPollCheckpoint::new(),
            swap_checkpoint: LogPollCheckpoint::new(),
        }
    }

    /// One full pipeline pass: discover -> hydrate -> scan swaps. Safe to
    /// call repeatedly on a timer; every step is checkpointed and
    /// idempotent (redelivered logs/duplicate pools are no-ops, not
    /// errors).
    pub async fn run_once(&mut self, latest_block: u64, market_state: &SharedMarketState) {
        self.scan_discovery(latest_block).await;
        self.hydrate_pending_pools().await;
        self.scan_swaps(latest_block, market_state).await;
    }

    async fn scan_discovery(&mut self, latest_block: u64) {
        let Some((from, to)) = self
            .discovery_checkpoint
            .next_range(latest_block, self.log_start_block)
        else {
            return;
        };

        tracing::info!(
            source = "http_poll",
            scan = "discovery",
            range_from = from,
            range_to = to,
            "scanning for new pools"
        );

        for i in 0..self.discovery_adapters.len() {
            let factory_address = self.discovery_adapters[i].factory_address();
            let topic0 = self.discovery_adapters[i].event_topic0();

            let logs = match self
                .log_poller
                .fetch_logs(from, to, vec![factory_address], topic0)
                .await
            {
                Ok(logs) => logs,
                Err(err) => {
                    tracing::error!(
                        source = "http_poll",
                        scan = "discovery",
                        error = %err,
                        "discovery scan failed for this factory"
                    );
                    continue;
                }
            };

            tracing::info!(
                source = "http_poll",
                scan = "discovery",
                dex = self.discovery_adapters[i].dex().name(),
                logs_returned = logs.len(),
                "discovery scan complete for factory"
            );

            for log in &logs {
                // Defensive reorg guard - see module/README notes: this
                // skips a retracted log rather than applying it as a real
                // discovery event. It does NOT retroactively undo any state
                // from a previous poll; full reorg reconciliation is not
                // implemented.
                if log.removed {
                    tracing::warn!(
                        source = "http_poll",
                        "skipping removed=true log (reorg guard, not full reconciliation)"
                    );
                    continue;
                }

                let discovered = match self.discovery_adapters[i].decode_pool_created(log) {
                    Ok(d) => d,
                    Err(err) => {
                        tracing::error!(source = "http_poll", error = %err, "failed to decode PoolCreated log, skipping");
                        continue;
                    }
                };

                let placeholder_pool = Pool {
                    address: discovered.pool_address,
                    dex: discovered.dex,
                    token0: Token {
                        address: discovered.token0_address,
                        symbol: String::new(),
                        decimals: 0,
                    },
                    token1: Token {
                        address: discovered.token1_address,
                        symbol: String::new(),
                        decimals: 0,
                    },
                    kind: placeholder_pool_kind(&discovered.params),
                };

                let inserted = self.registry.insert_discovered(
                    placeholder_pool,
                    DiscoverySource::FactoryEvent {
                        factory_address: discovered.factory_address,
                        block_number: discovered.block_number,
                        tx_hash: discovered.tx_hash,
                    },
                    discovered.block_number,
                );

                if inserted {
                    tracing::info!(
                        source = "http_poll",
                        event = "pool_discovered",
                        dex = discovered.dex.name(),
                        pool = %discovered.pool_address,
                        token0 = %discovered.token0_address,
                        token1 = %discovered.token1_address,
                        block = discovered.block_number,
                        "pool discovered"
                    );
                }
            }
        }

        self.discovery_checkpoint.advance(to);
    }

    async fn hydrate_pending_pools(&mut self) {
        let pending: Vec<Address> = self
            .registry
            .iter()
            .filter(|(_, record)| record.status == PoolStatus::Discovered)
            .map(|(addr, _)| *addr)
            .collect();

        for address in pending {
            self.registry.set_status(&address, PoolStatus::Hydrating);

            let (dex, token0_addr, token1_addr) = {
                let record = self.registry.get(&address).expect("just looked up");
                (
                    record.pool.dex,
                    record.pool.token0.address,
                    record.pool.token1.address,
                )
            };

            let token0 = self
                .token_cache
                .get_or_fetch(&self.rpc_url, token0_addr)
                .await;
            let token1 = self
                .token_cache
                .get_or_fetch(&self.rpc_url, token1_addr)
                .await;

            let (token0, token1) = match (token0, token1) {
                (Ok(t0), Ok(t1)) => (t0, t1),
                _ => {
                    tracing::warn!(
                        source = "http_poll",
                        pool = %address,
                        "token metadata hydration failed (decimals unavailable) - marking pool inactive"
                    );
                    self.registry.set_status(&address, PoolStatus::Inactive);
                    continue;
                }
            };

            let Some(adapter) = self.dex_adapters.get(&dex) else {
                self.registry.set_status(&address, PoolStatus::Inactive);
                continue;
            };

            let skeleton_kind = self
                .registry
                .get(&address)
                .map(|r| r.pool.kind.clone())
                .unwrap_or(PoolKind::Aerodrome {
                    reserve0: U256::ZERO,
                    reserve1: U256::ZERO,
                    stable: false,
                    fee_bps: None, // not yet hydrated - see PoolKind::Aerodrome::fee_bps docs
                });

            let skeleton = Pool {
                address,
                dex,
                token0,
                token1,
                kind: skeleton_kind,
            };

            match adapter.get_pool_state(&self.rpc_url, &skeleton).await {
                Ok(pool_state) => {
                    if let Some(record) = self.registry.get_mut(&address) {
                        let liquidity_available = has_liquidity(&pool_state.pool.kind);
                        record.pool = pool_state.pool;
                        record.status = PoolStatus::Active;
                        record.last_updated_block = pool_state.freshness.last_updated_block;
                        record.last_updated_timestamp =
                            pool_state.freshness.last_updated_timestamp;
                        record.eligibility = PoolEligibility {
                            verified_protocol: true,
                            token_metadata_available: true,
                            pool_type_supported: true,
                            liquidity_available,
                            state_readable: true,
                        };
                        tracing::info!(
                            source = "http_poll",
                            event = "pool_hydrated",
                            dex = dex.name(),
                            pool = %address,
                            eligibility = ?record.eligibility.status(),
                            "pool hydrated"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(source = "http_poll", pool = %address, error = %err, "state hydration failed - marking pool inactive");
                    self.registry.set_status(&address, PoolStatus::Inactive);
                }
            }
        }
    }

    async fn scan_swaps(&mut self, latest_block: u64, market_state: &SharedMarketState) {
        let Some((from, to)) = self
            .swap_checkpoint
            .next_range(latest_block, self.log_start_block)
        else {
            return;
        };

        let active_by_dex: HashMap<DexKind, Vec<Address>> = {
            let mut map: HashMap<DexKind, Vec<Address>> = HashMap::new();
            for (addr, record) in self.registry.iter() {
                if record.status == PoolStatus::Active {
                    map.entry(record.pool.dex).or_default().push(*addr);
                }
            }
            map
        };

        if active_by_dex.is_empty() {
            self.swap_checkpoint.advance(to);
            return;
        }

        tracing::info!(
            source = "http_poll",
            scan = "swaps",
            range_from = from,
            range_to = to,
            pools_watched = active_by_dex.values().map(|v| v.len()).sum::<usize>(),
            "scanning known pools for swap events"
        );

        for (dex, addresses) in &active_by_dex {
            let topic0 = match dex {
                DexKind::Aerodrome => aerodrome_swap_topic0(),
                DexKind::UniswapV3 | DexKind::AerodromeSlipstream => uniswap_v3_swap_topic0(),
            };

            let logs = match self
                .log_poller
                .fetch_logs(from, to, addresses.clone(), topic0)
                .await
            {
                Ok(logs) => logs,
                Err(err) => {
                    tracing::error!(
                        source = "http_poll",
                        scan = "swaps",
                        dex = dex.name(),
                        error = %err,
                        "swap scan failed"
                    );
                    continue;
                }
            };

            tracing::info!(
                source = "http_poll",
                scan = "swaps",
                dex = dex.name(),
                logs_returned = logs.len(),
                "swap scan complete"
            );

            let Some(adapter) = self.dex_adapters.get(dex) else {
                continue;
            };

            for log in &logs {
                if log.removed {
                    tracing::warn!(
                        source = "http_poll",
                        "skipping removed=true swap log (reorg guard, not full reconciliation)"
                    );
                    continue;
                }

                let received_at_us = now_us();
                match adapter.decode_event(log, self.chain_id, received_at_us) {
                    Ok(event) => {
                        let mut guard = market_state.write().await;
                        if guard.apply_event(event.clone()) {
                            crate::telemetry::log_event_received(&event);
                        } else {
                            crate::telemetry::log_event_duplicate(&event);
                        }
                    }
                    Err(err) => {
                        tracing::error!(source = "http_poll", error = %err, "failed to decode swap log, skipping");
                    }
                }
            }
        }

        self.swap_checkpoint.advance(to);
    }
}

fn placeholder_pool_kind(params: &DiscoveryParams) -> PoolKind {
    match params {
        DiscoveryParams::AerodromeClassic { stable } => PoolKind::Aerodrome {
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
            stable: *stable,
            fee_bps: None, // not yet hydrated - see PoolKind::Aerodrome::fee_bps docs
        },
        DiscoveryParams::ConcentratedLiquidity { tick_spacing, .. } => {
            PoolKind::ConcentratedLiquidity {
                fee_tier: 0,
                tick_spacing: *tick_spacing,
                sqrt_price_x96: U256::ZERO,
                current_tick: 0,
                liquidity: 0,
                initialized_ticks: Default::default(),
            }
        }
    }
}

fn has_liquidity(kind: &PoolKind) -> bool {
    match kind {
        PoolKind::Aerodrome {
            reserve0, reserve1, ..
        } => !reserve0.is_zero() && !reserve1.is_zero(),
        PoolKind::ConcentratedLiquidity { liquidity, .. } => *liquidity > 0,
    }
}
