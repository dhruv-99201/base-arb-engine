mod chain;
mod cli;
mod config;
mod dex;
mod discovery_pipeline;
mod error;
mod events;
mod market;
mod pools;
mod pricing;
mod telemetry;

use crate::chain::{BaseChainSource, ChainEventSource};
use crate::config::Config;
use crate::dex::DexAdapter;
use crate::discovery_pipeline::DiscoveryPipeline;
use crate::error::EngineResult;
use crate::events::decoder::now_us;
use crate::market::{BlockState, DexKind, MarketState};
use futures::StreamExt;

#[tokio::main]
async fn main() -> EngineResult<()> {
    // --- discover-test / inspect-tx subcommands: read-only diagnostics,
    // handled before the normal startup path so `--help` never requires a
    // valid .env and argument errors never touch the network. ---
    let raw_args: Vec<String> = std::env::args().collect();
    if raw_args.len() > 1 && raw_args[1] == "discover-test" {
        return match cli::parse_discover_test_args(&raw_args[2..]) {
            Ok(cli::DiscoverTestCommand::Help) => {
                println!("{}", cli::DISCOVER_TEST_USAGE);
                Ok(())
            }
            Ok(cli::DiscoverTestCommand::Run { from_block, to_block }) => {
                let config = Config::load()?;
                telemetry::init_tracing(&config.log_level);
                cli::run_discover_test(&config, from_block, to_block).await
            }
            Err(msg) => {
                eprintln!("error: {msg}\n");
                eprintln!("{}", cli::DISCOVER_TEST_USAGE);
                std::process::exit(2)
            }
        };
    }
    if raw_args.len() > 1 && raw_args[1] == "inspect-tx" {
        return match cli::parse_inspect_tx_args(&raw_args[2..]) {
            Ok(cli::InspectTxCommand::Help) => {
                println!("{}", cli::INSPECT_TX_USAGE);
                Ok(())
            }
            Ok(cli::InspectTxCommand::Run { tx_hash }) => {
                let config = Config::load()?;
                telemetry::init_tracing(&config.log_level);
                cli::run_inspect_tx(&config, tx_hash).await
            }
            Err(msg) => {
                eprintln!("error: {msg}\n");
                eprintln!("{}", cli::INSPECT_TX_USAGE);
                std::process::exit(2)
            }
        };
    }

    let config = Config::load()?;
    telemetry::init_tracing(&config.log_level);

    tracing::info!(
        execution_mode = ?config.execution_mode,
        can_execute_trades = config.execution_mode.can_execute_trades(),
        "starting base-arb-engine (Day 2: pool discovery + market-state indexing)"
    );

    let chain_source = BaseChainSource::new(
        config.base_rpc_url.clone(),
        config.base_ws_url.clone(),
        config.http_poll_interval,
    );

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
            source = "http_poll",
            "BASE_WS_URL not configured - entering HTTP fallback mode. Block ingestion will \
             poll the configured BASE_RPC_URL periodically instead of streaming over WebSocket. \
             Log/event ingestion for configured pools is unavailable in this mode (it requires \
             WebSocket)."
        );
    } else {
        tracing::info!(source = "websocket", "WebSocket endpoint configured - using streaming ingestion");
    }

    // --- Block stream (WebSocket push, or HTTP-poll fallback - selected
    // internally by BaseChainSource::mode(); see chain::base module docs) ---
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

    // --- Day 2: pool discovery + hydration + known-pool swap scanning.
    // Always HTTP (`eth_getLogs` polling), independent of whether the block
    // stream above is WebSocket or HTTP-poll - see discovery_pipeline
    // module docs. Runs on the same cadence as HTTP_POLL_INTERVAL_SECS. ---
    {
        let mut pipeline = DiscoveryPipeline::new(&config, chain_id);
        let pipeline_chain_source = chain_source.clone();
        let pipeline_state = state.clone();
        let poll_interval = config.http_poll_interval;

        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(poll_interval);
            loop {
                ticker.tick().await;
                let latest = match pipeline_chain_source.latest_block_number().await {
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!(source = "http_poll", error = %e, "failed to fetch latest block for discovery/swap scan, will retry next interval");
                        continue;
                    }
                };
                pipeline.run_once(latest, &pipeline_state).await;
            }
        });
    }

    // --- Log stream: WebSocket-only. Only attempted when WS is configured
    // AND there are pools to watch - HTTP-poll mode has no log ingestion
    // path today (latest-block polling only, per Day 1 scope). ---
    if chain_source.mode() == chain::ChainSourceMode::WebSocket && !watched_addresses.is_empty() {
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
                    // Day 1's WS pool-address config (AERODROME_POOL_ADDRESS /
                    // UNISWAP_V3_POOL_ADDRESS) never populates a Slipstream
                    // entry in dex_by_address, so this is unreachable in
                    // practice - but the match must still be exhaustive.
                    Some(DexKind::AerodromeSlipstream) | None => continue,
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
    } else if chain_source.mode() == chain::ChainSourceMode::HttpPoll && !watched_addresses.is_empty() {
        tracing::warn!(
            source = "http_poll",
            "pool address(es) are configured but log/event ingestion is unavailable in HTTP \
             fallback mode - only latest-block polling is active. Configure BASE_WS_URL to \
             enable event ingestion for the configured pool(s)."
        );
    }

    tracing::info!("ingestion running - press Ctrl+C to shut down");
    tokio::signal::ctrl_c()
        .await
        .map_err(|e| crate::error::EngineError::Other(anyhow::anyhow!(e)))?;
    tracing::info!("shutdown signal received, exiting cleanly");

    Ok(())
}
