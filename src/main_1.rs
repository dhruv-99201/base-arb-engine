mod chain;
mod config;
mod dex;
mod error;
mod events;
mod market;
mod telemetry;

use crate::chain::{BaseChainSource, ChainEventSource};
use crate::config::Config;
use crate::dex::DexAdapter;
use crate::error::EngineResult;
use crate::events::decoder::now_us;
use crate::market::{BlockState, DexKind, MarketState};
use futures::StreamExt;

#[tokio::main]
async fn main() -> EngineResult<()> {
    let config = Config::load()?;
    telemetry::init_tracing(&config.log_level);

    tracing::info!(
        execution_mode = ?config.execution_mode,
        can_execute_trades = config.execution_mode.can_execute_trades(),
        "starting base-arb-engine (Day 1: market data + state foundation)"
    );

    let chain_source = BaseChainSource::new(config.base_rpc_url.clone(), config.base_ws_url.clone());

    // --- Verify connectivity ---
    let chain_id = chain_source.chain_id().await?;
    if chain_id != config.base_chain_id {
        tracing::warn!(
            configured = config.base_chain_id,
            observed = chain_id,
            "configured BASE_CHAIN_ID does not match chain ID reported by RPC endpoint"
        );
    } else {
        tracing::info!(chain_id, "chain ID verified");
    }

    let latest_block = chain_source.latest_block_number().await?;
    tracing::info!(latest_block, "retrieved latest Base block");

    let state = MarketState::new_shared();
    {
        let mut guard = state.write().await;
        guard.update_latest_block(BlockState {
            number: latest_block,
            timestamp: None,
            hash: None,
        });
    }

    // --- Optional: register configured pools ---
    let aerodrome_adapter = dex::AerodromeAdapter::new();
    let uniswap_v3_adapter = dex::UniswapV3Adapter::new();

    let mut watched_addresses = Vec::new();
    if let Some(addr) = &config.aerodrome_pool_address {
        match addr.parse::<alloy::primitives::Address>() {
            Ok(a) => {
                tracing::info!(pool = %a, dex = "aerodrome", "watching configured pool");
                watched_addresses.push(a);
            }
            Err(e) => tracing::error!(error = %e, "invalid AERODROME_POOL_ADDRESS, ignoring"),
        }
    } else {
        tracing::info!(
            "AERODROME_POOL_ADDRESS not configured - generic ingestion layer will run without a \
             known Aerodrome pool. Set it to a verified pool address to process real events."
        );
    }
    if let Some(addr) = &config.uniswap_v3_pool_address {
        match addr.parse::<alloy::primitives::Address>() {
            Ok(a) => {
                tracing::info!(pool = %a, dex = "uniswap_v3", "watching configured pool");
                watched_addresses.push(a);
            }
            Err(e) => tracing::error!(error = %e, "invalid UNISWAP_V3_POOL_ADDRESS, ignoring"),
        }
    } else {
        tracing::info!(
            "UNISWAP_V3_POOL_ADDRESS not configured - generic ingestion layer will run without a \
             known Uniswap V3 pool. Set it to a verified pool address to process real events."
        );
    }

    if config.base_ws_url.is_none() {
        tracing::warn!(
            "BASE_WS_URL not configured - live block/log subscription is unavailable this run. \
             Chain ID and latest block were retrieved via HTTP above; set BASE_WS_URL to enable \
             streaming ingestion."
        );
        tracing::info!("Day 1 startup checks complete (HTTP-only mode). Shutting down.");
        return Ok(());
    }

    // --- Block stream ---
    let mut block_stream = chain_source.subscribe_blocks().await?;
    let block_state = state.clone();
    tokio::spawn(async move {
        while let Some(block) = block_stream.next().await {
            let mut guard = block_state.write().await;
            let number = block.number;
            guard.update_latest_block(block);
            tracing::debug!(block = number, "new block");
        }
    });

    // --- Log stream (only if we have pools to watch) ---
    if !watched_addresses.is_empty() {
        let mut log_stream = chain_source.subscribe_logs(watched_addresses).await?;
        let log_state = state.clone();
        let dex_by_address: std::collections::HashMap<alloy::primitives::Address, DexKind> = {
            let mut m = std::collections::HashMap::new();
            if let Some(addr) = &config.aerodrome_pool_address {
                if let Ok(a) = addr.parse() {
                    m.insert(a, DexKind::Aerodrome);
                }
            }
            if let Some(addr) = &config.uniswap_v3_pool_address {
                if let Ok(a) = addr.parse() {
                    m.insert(a, DexKind::UniswapV3);
                }
            }
            m
        };

        tokio::spawn(async move {
            while let Some(log) = log_stream.next().await {
                let received_at_us = now_us();
                let dex = dex_by_address.get(&log.inner.address).copied();
                let decoded = match dex {
                    Some(DexKind::Aerodrome) => {
                        aerodrome_adapter.decode_event(&log, chain_id, received_at_us)
                    }
                    Some(DexKind::UniswapV3) => {
                        uniswap_v3_adapter.decode_event(&log, chain_id, received_at_us)
                    }
                    None => continue, // shouldn't happen given the filter, but never guess
                };

                match decoded {
                    Ok(event) => {
                        let mut guard = log_state.write().await;
                        if guard.apply_event(event.clone()) {
                            telemetry::log_event_received(&event);
                        } else {
                            telemetry::log_event_duplicate(&event);
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "failed to decode event, skipping");
                    }
                }
            }
        });
    }

    tracing::info!("ingestion running - press Ctrl+C to shut down");
    tokio::signal::ctrl_c()
        .await
        .map_err(|e| crate::error::EngineError::Other(anyhow::anyhow!(e)))?;
    tracing::info!("shutdown signal received, exiting cleanly");

    Ok(())
}
