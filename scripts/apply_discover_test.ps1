# base-arb-engine discover-test apply script
# Run from the repository root: C:\Users\Dell\Downloads\base-arb-engine
#   powershell -ExecutionPolicy Bypass -File apply_discover_test.ps1
Write-Host 'Applying discover-test command...'

# Ensure all target directories exist first.
New-Item -ItemType Directory -Force -Path 'src' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\dex\discovery' | Out-Null

# ---- src/cli.rs ----
$content = @'
//! `discover-test` CLI subcommand: read-only verification of pool discovery
//! against a caller-supplied historical block range.
//!
//! Reuses the exact same `HttpLogPoller` + discovery adapters + decoders
//! the live pipeline uses (`discovery_pipeline`/`dex::discovery`) - this
//! command exists to prove the decode path works against real, verified
//! Base data, not to duplicate or bypass it. It never touches
//! `PoolRegistry` or `MarketState`, never requires a private key, and never
//! signs or submits anything - it only reads logs and prints what it
//! decoded.

use crate::chain::log_poller::HttpLogPoller;
use crate::config::Config;
use crate::dex::discovery::{
    AerodromeClassicDiscovery, AerodromeSlipstreamDiscovery, DiscoveredPool, DiscoveryParams,
    PoolDiscoveryAdapter, UniswapV3Discovery,
};
use crate::error::EngineResult;
use alloy::primitives::Address;

pub const DISCOVER_TEST_USAGE: &str = "\
Usage: cargo run -- discover-test --from-block <BLOCK> --to-block <BLOCK>

Read-only verification command. Scans the configured Uniswap V3 and
Aerodrome (classic) factory addresses - and Aerodrome Slipstream, if
AERODROME_SLIPSTREAM_FACTORY_ADDRESS is set - for PoolCreated events in
the inclusive range [--from-block, --to-block], decodes them with the
exact same adapters the live discovery pipeline uses, and prints the
results.

Does not modify any blockchain state, requires no private key, and never
signs or submits a transaction. Uses BASE_RPC_URL from your environment/
.env exactly like the rest of this program.

Options:
  --from-block <BLOCK>   First block to scan (inclusive). Required.
  --to-block <BLOCK>     Last block to scan (inclusive). Required.
  --help, -h             Show this help and exit.

Example (supply a block range you have independently verified contains a
real PoolCreated event, e.g. via BaseScan's \"Events\" tab on the factory
address - this command never invents block numbers or transaction hashes):

  cargo run -- discover-test --from-block 12345678 --to-block 12345778
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverTestCommand {
    Help,
    Run { from_block: u64, to_block: u64 },
}

/// Parse `discover-test` subcommand arguments (everything after
/// `discover-test` itself). Pure function - no I/O, fully unit-testable.
pub fn parse_discover_test_args(args: &[String]) -> Result<DiscoverTestCommand, String> {
    // --help/-h wins over everything else, including otherwise-invalid args -
    // a user asking for help shouldn't first have to fix an unrelated typo.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(DiscoverTestCommand::Help);
    }

    let mut from_block: Option<u64> = None;
    let mut to_block: Option<u64> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--from-block" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--from-block requires a value".to_string())?;
                from_block = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid --from-block value: '{raw}'"))?,
                );
                i += 2;
            }
            "--to-block" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--to-block requires a value".to_string())?;
                to_block = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid --to-block value: '{raw}'"))?,
                );
                i += 2;
            }
            other => return Err(format!("unrecognized argument: '{other}'")),
        }
    }

    let from_block =
        from_block.ok_or_else(|| "missing required --from-block <BLOCK>".to_string())?;
    let to_block = to_block.ok_or_else(|| "missing required --to-block <BLOCK>".to_string())?;

    if from_block > to_block {
        return Err(format!(
            "--from-block ({from_block}) must be <= --to-block ({to_block})"
        ));
    }

    Ok(DiscoverTestCommand::Run { from_block, to_block })
}

/// Format one decoded `PoolCreated` event for printing. Pure/deterministic:
/// the same input always produces the same output string, and every field
/// the spec requires is present (DEX, factory, block, tx hash, pool,
/// token0, token1, fee/tickSpacing where applicable).
pub fn format_pool_created_report(
    dex_name: &str,
    factory: Address,
    discovered: &DiscoveredPool,
) -> String {
    let params_line = match &discovered.params {
        DiscoveryParams::AerodromeClassic { stable } => format!("stable={stable}"),
        DiscoveryParams::ConcentratedLiquidity { tick_spacing, fee } => match fee {
            Some(f) => format!("fee={f} tick_spacing={tick_spacing}"),
            None => format!("tick_spacing={tick_spacing} fee=unavailable-from-factory-event"),
        },
    };

    format!(
        "  dex={dex_name}\n  factory={factory}\n  block={}\n  tx_hash={}\n  pool={}\n  token0={}\n  token1={}\n  {params_line}",
        discovered.block_number,
        discovered.tx_hash,
        discovered.pool_address,
        discovered.token0_address,
        discovered.token1_address,
    )
}

/// Execute `discover-test`: scan the requested range against every
/// configured discovery adapter and print results. Read-only - no
/// `PoolRegistry`, no `MarketState`, no signer, nothing mutated. Uses
/// `HttpLogPoller` exactly as configured (`LOG_POLL_MAX_BLOCK_RANGE`
/// chunking/retry still applies), so behavior matches the live pipeline.
pub async fn run_discover_test(
    config: &Config,
    from_block: u64,
    to_block: u64,
) -> EngineResult<()> {
    let log_poller =
        HttpLogPoller::new(config.base_rpc_url.clone(), config.log_poll_max_block_range);

    let mut adapters: Vec<Box<dyn PoolDiscoveryAdapter>> = vec![
        Box::new(UniswapV3Discovery::new(config.uniswap_v3_factory_address)),
        Box::new(AerodromeClassicDiscovery::new(
            config.aerodrome_factory_address,
        )),
    ];
    if let Some(addr) = config.aerodrome_slipstream_factory_address {
        adapters.push(Box::new(AerodromeSlipstreamDiscovery::new(addr)));
    }

    println!("discover-test: scanning blocks {from_block}..={to_block}");
    println!("  BASE_RPC_URL: {}", config.base_rpc_url);
    println!("  factories configured: {}", adapters.len());

    let mut total_found = 0usize;
    for adapter in &adapters {
        let dex_name = adapter.dex().name();
        let factory = adapter.factory_address();
        println!("\n--- {dex_name} (factory {factory}) ---");

        let logs = log_poller
            .fetch_logs(from_block, to_block, vec![factory], adapter.event_topic0())
            .await?;
        println!("  logs_returned={}", logs.len());

        for log in &logs {
            // Same defensive reorg guard as the live pipeline - see
            // discovery_pipeline module docs. Never applied as a real
            // discovery event.
            if log.removed {
                println!("  [skipped: removed=true log]");
                continue;
            }

            match adapter.decode_pool_created(log) {
                Ok(discovered) => {
                    println!("{}", format_pool_created_report(dex_name, factory, &discovered));
                    total_found += 1;
                }
                Err(e) => {
                    println!("  [decode error, skipping]: {e}");
                }
            }
        }
    }

    println!(
        "\ndiscover-test complete: {total_found} pool(s) decoded across {} factories, blocks {from_block}..={to_block}",
        adapters.len()
    );
    if total_found == 0 {
        println!(
            "No PoolCreated events decoded in this range. This does not by itself indicate a \
             bug - supply a range you've independently confirmed (e.g. via BaseScan's \"Events\" \
             tab on the factory address) contains a real PoolCreated event, then rerun."
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_valid_args() {
        let result =
            parse_discover_test_args(&args(&["--from-block", "100", "--to-block", "200"]));
        assert_eq!(
            result,
            Ok(DiscoverTestCommand::Run {
                from_block: 100,
                to_block: 200
            })
        );
    }

    #[test]
    fn parses_valid_args_in_reverse_order() {
        let result =
            parse_discover_test_args(&args(&["--to-block", "200", "--from-block", "100"]));
        assert_eq!(
            result,
            Ok(DiscoverTestCommand::Run {
                from_block: 100,
                to_block: 200
            })
        );
    }

    #[test]
    fn help_flag_short_circuits_everything_else() {
        assert_eq!(
            parse_discover_test_args(&args(&["--help"])),
            Ok(DiscoverTestCommand::Help)
        );
        assert_eq!(
            parse_discover_test_args(&args(&["-h"])),
            Ok(DiscoverTestCommand::Help)
        );
        // --help wins even alongside otherwise-invalid args.
        assert_eq!(
            parse_discover_test_args(&args(&["--bogus", "--help"])),
            Ok(DiscoverTestCommand::Help)
        );
    }

    #[test]
    fn missing_from_block_is_rejected() {
        let err = parse_discover_test_args(&args(&["--to-block", "200"])).unwrap_err();
        assert!(err.contains("--from-block"));
    }

    #[test]
    fn missing_to_block_is_rejected() {
        let err = parse_discover_test_args(&args(&["--from-block", "100"])).unwrap_err();
        assert!(err.contains("--to-block"));
    }

    #[test]
    fn non_numeric_block_is_rejected() {
        let err =
            parse_discover_test_args(&args(&["--from-block", "abc", "--to-block", "200"]))
                .unwrap_err();
        assert!(err.contains("--from-block"));
    }

    #[test]
    fn from_block_after_to_block_is_rejected() {
        let err = parse_discover_test_args(&args(&["--from-block", "500", "--to-block", "100"]))
            .unwrap_err();
        assert!(err.contains("must be <="));
    }

    #[test]
    fn unrecognized_argument_is_rejected() {
        let err = parse_discover_test_args(&args(&["--wat", "1"])).unwrap_err();
        assert!(err.contains("unrecognized argument"));
    }

    #[test]
    fn dangling_flag_without_value_is_rejected() {
        let err = parse_discover_test_args(&args(&["--from-block"])).unwrap_err();
        assert!(err.contains("--from-block requires a value"));
    }

    #[test]
    fn equal_from_and_to_block_is_valid() {
        let result =
            parse_discover_test_args(&args(&["--from-block", "100", "--to-block", "100"]));
        assert_eq!(
            result,
            Ok(DiscoverTestCommand::Run {
                from_block: 100,
                to_block: 100
            })
        );
    }

    fn sample_discovered_pool() -> DiscoveredPool {
        DiscoveredPool {
            pool_address: Address::from_slice(&[0x11; 20]),
            token0_address: Address::from_slice(&[0x22; 20]),
            token1_address: Address::from_slice(&[0x33; 20]),
            dex: crate::market::models::DexKind::UniswapV3,
            params: DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: 60,
                fee: Some(3000),
            },
            block_number: 12_345_678,
            tx_hash: alloy::primitives::B256::repeat_byte(0xAB),
            factory_address: Address::from_slice(&[0x44; 20]),
        }
    }

    #[test]
    fn report_formatting_is_deterministic() {
        let discovered = sample_discovered_pool();
        let a = format_pool_created_report("uniswap_v3", discovered.factory_address, &discovered);
        let b = format_pool_created_report("uniswap_v3", discovered.factory_address, &discovered);
        assert_eq!(
            a, b,
            "formatting the same input twice must produce identical output"
        );
    }

    #[test]
    fn report_contains_all_required_fields() {
        let discovered = sample_discovered_pool();
        let report =
            format_pool_created_report("uniswap_v3", discovered.factory_address, &discovered);

        assert!(report.contains("dex=uniswap_v3"));
        assert!(report.contains(&discovered.factory_address.to_string()));
        assert!(report.contains(&discovered.block_number.to_string()));
        assert!(report.contains(&discovered.tx_hash.to_string()));
        assert!(report.contains(&discovered.pool_address.to_string()));
        assert!(report.contains(&discovered.token0_address.to_string()));
        assert!(report.contains(&discovered.token1_address.to_string()));
        assert!(report.contains("fee=3000"));
        assert!(report.contains("tick_spacing=60"));
    }

    #[test]
    fn report_handles_aerodrome_classic_params() {
        let discovered = DiscoveredPool {
            pool_address: Address::from_slice(&[0x55; 20]),
            token0_address: Address::from_slice(&[0x66; 20]),
            token1_address: Address::from_slice(&[0x77; 20]),
            dex: crate::market::models::DexKind::Aerodrome,
            params: DiscoveryParams::AerodromeClassic { stable: true },
            block_number: 1,
            tx_hash: alloy::primitives::B256::repeat_byte(0xCD),
            factory_address: Address::from_slice(&[0x88; 20]),
        };
        let report =
            format_pool_created_report("aerodrome", discovered.factory_address, &discovered);
        assert!(report.contains("stable=true"));
    }

    #[test]
    fn report_handles_slipstream_params_with_no_fee() {
        let discovered = DiscoveredPool {
            pool_address: Address::from_slice(&[0x99; 20]),
            token0_address: Address::from_slice(&[0xAA; 20]),
            token1_address: Address::from_slice(&[0xBB; 20]),
            dex: crate::market::models::DexKind::AerodromeSlipstream,
            params: DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: 200,
                fee: None,
            },
            block_number: 2,
            tx_hash: alloy::primitives::B256::repeat_byte(0xEF),
            factory_address: Address::from_slice(&[0xCC; 20]),
        };
        let report = format_pool_created_report(
            "aerodrome_slipstream",
            discovered.factory_address,
            &discovered,
        );
        assert!(report.contains("tick_spacing=200"));
        assert!(report.contains("fee=unavailable-from-factory-event"));
    }
}

'@
Set-Content -Path 'src\cli.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/cli.rs'

# ---- src/main.rs ----
$content = @'
mod chain;
mod cli;
mod config;
mod dex;
mod discovery_pipeline;
mod error;
mod events;
mod market;
mod pools;
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
    // --- discover-test subcommand: read-only historical verification,
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

'@
Set-Content -Path 'src\main.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/main.rs'

# ---- src/dex/discovery/mod.rs ----
$content = @'
//! Pool discovery: turning factory `PoolCreated`-style events into pools
//! the registry can hydrate. Deliberately separate from `dex::traits::DexAdapter`
//! (which handles state/quoting for pools we already know about) - discovery
//! is a distinct concern with its own event shapes per factory.

pub mod aerodrome_classic;
pub mod aerodrome_slipstream;
pub mod uniswap_v3;

use crate::error::EngineResult;
use crate::market::models::DexKind;
use alloy::primitives::{Address, B256};
use alloy::rpc::types::Log as RpcLog;

pub use aerodrome_classic::AerodromeClassicDiscovery;
pub use aerodrome_slipstream::AerodromeSlipstreamDiscovery;
pub use uniswap_v3::UniswapV3Discovery;

/// Protocol-specific parameters captured at pool-creation time, before any
/// on-chain hydration. Kept separate from `market::models::PoolKind` since
/// that type represents *current* state (reserves/sqrtPriceX96/etc), not
/// creation-time parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryParams {
    AerodromeClassic { stable: bool },
    /// `fee` is `None` for factories whose `PoolCreated` event doesn't
    /// carry a fee (e.g. Aerodrome Slipstream's `CLFactory` - fee there
    /// routes through a separate `getSwapFee(pool)` call, not this event).
    ConcentratedLiquidity { tick_spacing: i32, fee: Option<u32> },
}

/// A pool observed via a factory event, not yet hydrated.
#[derive(Debug, Clone)]
pub struct DiscoveredPool {
    pub pool_address: Address,
    pub token0_address: Address,
    pub token1_address: Address,
    pub dex: DexKind,
    pub params: DiscoveryParams,
    pub block_number: u64,
    pub tx_hash: B256,
    pub factory_address: Address,
}

/// Implemented once per factory/event-shape. Adapters only decode - they
/// never fetch state (that stays in `dex::traits::DexAdapter::get_pool_state`,
/// called afterward during hydration).
pub trait PoolDiscoveryAdapter: Send + Sync {
    fn dex(&self) -> DexKind;
    fn factory_address(&self) -> Address;
    /// keccak256 topic0 of this factory's pool-creation event.
    fn event_topic0(&self) -> B256;
    fn decode_pool_created(&self, log: &RpcLog) -> EngineResult<DiscoveredPool>;
}

'@
Set-Content -Path 'src\dex\discovery\mod.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/mod.rs'

# ---- src/dex/discovery/uniswap_v3.rs ----
$content = @'
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
    event UniswapV3PoolCreated(
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
        UniswapV3PoolCreated::SIGNATURE_HASH
    }

    fn decode_pool_created(&self, log: &RpcLog) -> EngineResult<DiscoveredPool> {
        let block_number = log
            .block_number
            .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
        let tx_hash = log
            .transaction_hash
            .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;

        let decoded_log = UniswapV3PoolCreated::decode_log(&log.inner).map_err(|e| {
            EngineError::Decode(format!("failed to decode UniswapV3PoolCreated log: {e}"))
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

        let event = UniswapV3PoolCreated {
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

'@
Set-Content -Path 'src\dex\discovery\uniswap_v3.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/uniswap_v3.rs'

# ---- src/dex/discovery/aerodrome_slipstream.rs ----
$content = @'
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
    event AerodromeSlipstreamPoolCreated(
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
        AerodromeSlipstreamPoolCreated::SIGNATURE_HASH
    }

    fn decode_pool_created(&self, log: &RpcLog) -> EngineResult<DiscoveredPool> {
        let block_number = log
            .block_number
            .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
        let tx_hash = log
            .transaction_hash
            .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;

        let decoded_log = AerodromeSlipstreamPoolCreated::decode_log(&log.inner).map_err(|e| {
            EngineError::Decode(format!(
                "failed to decode AerodromeSlipstreamPoolCreated log: {e}"
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

        let event = AerodromeSlipstreamPoolCreated {
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

'@
Set-Content -Path 'src\dex\discovery\aerodrome_slipstream.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/aerodrome_slipstream.rs'

# ---- src/discovery_pipeline.rs ----
$content = @'
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
        if let Some(addr) = config.aerodrome_slipstream_factory_address {
            discovery_adapters.push(Box::new(AerodromeSlipstreamDiscovery::new(addr)));
        } else {
            tracing::info!(
                "AERODROME_SLIPSTREAM_FACTORY_ADDRESS not configured - Slipstream pool \
                 discovery is disabled. See README for how to verify the address before \
                 enabling it."
            );
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

'@
Set-Content -Path 'src\discovery_pipeline.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/discovery_pipeline.rs'

# ---- README.md ----
$content = @'
# base-arb-engine

A research/MVP Base L2 arbitrage engine. Eventual goal: detect executable
price discrepancies between Aerodrome and Uniswap V3 on Base, size trades
optimally, simulate locally, and execute atomically via flash-loan-funded
Solidity executor. This repository is being built incrementally, day by day.

## Current scope: Day 1 + Day 2

Day 1 delivered the **market-data and state foundation**. Day 2 adds
**automated pool discovery and real protocol market-state indexing** on top
of it, still entirely over HTTP, still entirely read-only.

### Day 1

- Configurable Base RPC/WS connectivity (Alloy), with chain ID and latest
  block retrieval. WebSocket is **optional**: if `BASE_WS_URL` is configured,
  block/log ingestion streams over WebSocket with reconnect and exponential
  backoff; if not, the engine automatically falls back to periodic HTTP
  polling of `BASE_RPC_URL` for the latest block and keeps running
  indefinitely either way (see "HTTP fallback mode" below).
- A `ChainEventSource` trait so the event feed is swappable later (e.g. a
  lower-latency Base feed) without touching strategy/state code.
- A `DexAdapter` trait with Aerodrome (classic), Aerodrome Slipstream, and
  Uniswap V3 implementations:
  - Aerodrome classic: reserve-based state (`getReserves`, `stable`).
  - Uniswap V3 / Aerodrome Slipstream: **concentrated-liquidity** state
    (`slot0`, `liquidity`, `tickSpacing`) - not a fake two-reserve model.
    These share a `PoolKind::ConcentratedLiquidity` shape (same mechanics)
    but are hydrated via protocol-specific adapters, since their ABIs
    differ (Slipstream's `slot0()` has 6 fields, Uniswap V3's has 7 - see
    `dex::aerodrome_slipstream` module docs).
- Normalized data models (`Token`, `Pool`, `PoolState`, `BlockState`,
  `SwapEvent`, `MarketEvent`, `StateVersion`) using integer/token-native
  units throughout - no floating point anywhere in financial code paths.
- Deterministic event decoding with explicit rejection of malformed logs.
- Deterministic event deduplication keyed on `(chain_id, tx_hash,
  log_index)`.
- An in-memory `MarketState` store: versioned, deterministic updates,
  duplicate-safe, tracks per-pool freshness.
- Structured `tracing` logs including per-event ingestion latency.

### Day 2

- **HTTP `eth_getLogs` polling** (`chain::log_poller::HttpLogPoller`):
  chunked to stay under provider range limits (`LOG_POLL_MAX_BLOCK_RANGE`,
  default 2000 blocks), with automatic range-halving retry if a provider
  rejects a range as too large, and a checkpoint
  (`chain::log_poller::LogPollCheckpoint`) so polling cycles never rescan or
  skip blocks.
- **Automated pool discovery** (`dex::discovery`) from real factory
  `PoolCreated` events:
  - Uniswap V3 (`UniswapV3Factory.PoolCreated`) - standard, well-documented
    event shape.
  - Aerodrome classic (`PoolFactory.PoolCreated`) - event shape confirmed
    directly against the verified contract source on BaseScan.
  - Aerodrome Slipstream (`CLFactory.PoolCreated`) - event *emission*
    confirmed against real `CLFactory.sol` source, but the exact indexed/
    non-indexed parameter split is a well-justified inference, not fully
    confirmed - see `dex::discovery::aerodrome_slipstream` module docs and
    "Known limitations" below. **Disabled by default** - the factory
    address is not hardcoded anywhere in this codebase (unlike Uniswap V3
    and Aerodrome classic, whose addresses are verified defaults); set
    `AERODROME_SLIPSTREAM_FACTORY_ADDRESS` yourself after verifying it to
    enable it.
- **`PoolRegistry`** (`pools::registry`): lifecycle states (`Discovered` ->
  `Hydrating` -> `Active`/`Inactive`/`Blacklisted`), lookup by address, and
  an order-independent token-pair index (`(WETH,USDC)` and `(USDC,WETH)`
  both resolve to the same pools), optionally filtered by DEX.
- **Token metadata hydration** (`pools::token_cache::TokenMetadataCache`):
  on-chain ERC-20 `decimals`/`symbol`/`name` calls, cached per address so a
  token contract is never queried more than once. `decimals` is required;
  `symbol`/`name` are best-effort and never fail hydration.
- **Pool state hydration**: reuses each `DexAdapter::get_pool_state` to pull
  real on-chain reserves/`slot0`/liquidity for every newly discovered pool.
- **Pool eligibility** (`pools::models::PoolEligibility`): a coarse
  `Eligible`/`Ineligible`/`Unknown` signal computed from protocol
  verification, token metadata availability, supported pool type,
  liquidity presence, and state readability. Nothing downstream enforces
  this yet - it's informational, for the future opportunity engine to use.
- **Known-pool swap scanning**: once a pool is `Active`, its address is
  included in periodic `eth_getLogs` swap scans, decoded through the exact
  same `DexAdapter::decode_event` / `MarketState::apply_event` path Day 1's
  WebSocket log stream uses - transport-independent by construction.
- **Defensive reorg guard**: any log with `removed=true` is logged and
  skipped rather than applied as a real event. This is **not** full reorg
  reconciliation (no retroactive state rollback if a re-poll reveals a
  changed block) - see "Known limitations".
- New config: `UNISWAP_V3_FACTORY_ADDRESS`, `AERODROME_FACTORY_ADDRESS`,
  `AERODROME_SLIPSTREAM_FACTORY_ADDRESS`, `LOG_POLL_MAX_BLOCK_RANGE`,
  `LOG_START_BLOCK` (`latest` by default - no historical backfill unless
  you explicitly set a block number).

**Day 2 remains read-only.** See "Safety" below - nothing has changed
there.

## Architecture

```text
Base Chain
    |
    +-------------------------------+
    v                                v
Chain Event Source (blocks)     HttpLogPoller (Day 2: eth_getLogs,
    |                            chunked + checkpointed)
    v                                |
BlockState -> MarketState             +--> Discovery scan (factory logs)
                                       |        |
                                       |        v
                                       |    dex::discovery adapters
                                       |    (decode PoolCreated)
                                       |        |
                                       |        v
                                       |    PoolRegistry (insert, Discovered)
                                       |        |
                                       |        v
                                       |    TokenMetadataCache + DexAdapter
                                       |    ::get_pool_state (hydrate)
                                       |        |
                                       |        v
                                       |    PoolRegistry (Active) + eligibility
                                       |
                                       +--> Swap scan (known Active pools)
                                                |
                                                v
                                       Event Decoder (events::decoder -
                                       same code Day 1's WS path uses)
                                                |
                                                v
                                       Normalized Market State
                                       (market::state::MarketState -
                                        versioned, deduplicated,
                                        freshness-tracked)
                                                |
                                                v
                                       Opportunity Engine    <-- DAY 3+
                                                |
                                                v
                                       REVM Simulator         <-- LATER
                                                |
                                                v
                                       Transaction Builder     <-- LATER
                                                |
                                                v
                                       ArbExecutor.sol          <-- LATER
```

## Setup

Requirements:
- Rust (current stable toolchain; this crate targets modern Alloy, which
  requires a recent `rustc` - see "Known limitations" below if you hit an
  MSRV error).
- A Base RPC endpoint (HTTP) and, for streaming ingestion, a Base WebSocket
  endpoint. Any provider works - nothing is hard-coded.

```bash
cp .env.example .env
# edit .env: set BASE_RPC_URL and (optionally) BASE_WS_URL
```

## Run

```bash
cargo run
```

The engine runs indefinitely (until Ctrl+C) regardless of whether
`BASE_WS_URL` is set:

- **WebSocket mode** (`BASE_WS_URL` set): block and log ingestion stream
  over WebSocket, with reconnect and exponential backoff on disconnect. Log
  lines are tagged `source=websocket`.
- **HTTP fallback mode** (`BASE_WS_URL` unset or empty): block ingestion
  polls `BASE_RPC_URL` every `HTTP_POLL_INTERVAL_SECS` (default 5s) instead.
  This is the mode to use if your WebSocket endpoint isn't available - for
  example, `wss://mainnet.base.org` rejects some clients with HTTP 405.
  Log/event ingestion for configured pools requires WebSocket and is
  unavailable in this mode (only latest-block polling runs). Log lines are
  tagged `source=http_poll`.

To watch a specific, verified pool for swap events (WebSocket mode only),
set `AERODROME_POOL_ADDRESS` and/or `UNISWAP_V3_POOL_ADDRESS` in `.env`.
This is independent of Day 2's automated discovery, which runs regardless
of WebSocket mode (see above) and finds pools on its own.

Expected log lines once Day 2's pipeline is running (exact numbers/blocks
will differ):

```text
INFO ... source=http_poll scan=discovery range_from=... range_to=... "scanning for new pools"
INFO ... source=http_poll event=pool_discovered dex=uniswap_v3 pool=0x... "pool discovered"
INFO ... source=http_poll event=pool_hydrated dex=uniswap_v3 pool=0x... eligibility=Eligible "pool hydrated"
INFO ... source=http_poll scan=swaps range_from=... range_to=... pools_watched=N "scanning known pools for swap events"
INFO ... event=event_received dex=uniswap_v3 pool=0x... processing_latency_us=... "event_received"
```

New pool creation on Base isn't guaranteed within any given observation
window. To verify discovery works at all without waiting, set
`LOG_START_BLOCK` to a historical block you know contains a real
`PoolCreated` event for one of the configured factories (check BaseScan's
"Events" tab on the factory address) and restart - this is the "controlled
backfill" path, not automatic full-history scanning.

## Verifying pool discovery against real historical data: `discover-test`

The live pipeline only discovers pools *created* during the blocks it
happens to be running for - if nothing new was created on Base while it was
up, `logs_returned=0` is expected and correct, not a bug. To verify the
discovery/decode path actually works, use the read-only `discover-test`
subcommand against a block range you've independently confirmed contains a
real `PoolCreated` event:

```bash
cargo run -- discover-test --help
cargo run -- discover-test --from-block <BLOCK> --to-block <BLOCK>
```

**How to get a verified historical range** (this command never invents
block numbers or transaction hashes - you supply them):

1. Open the relevant factory address on BaseScan:
   - Uniswap V3: `0x33128a8fC17869897dcE68Ed026d694621f6FDfD`
   - Aerodrome classic: `0x420DD381b31aEf6683db6B902084cB0FFECe40Da`
2. Go to its "Events" tab and find any `PoolCreated` transaction.
3. Note that transaction's block number, and use a small window around it,
   e.g. `--from-block <N-5> --to-block <N+5>`.
4. Run the command above with that range.

It uses `BASE_RPC_URL` from your `.env`, the same verified factory
addresses and the same `PoolCreated` decoders the live pipeline uses, and
`eth_getLogs` via the existing chunked/checkpointed `HttpLogPoller` (no
polling-architecture changes). For each match it prints DEX, factory,
block number, transaction hash, pool address, token0, token1, and
fee/tickSpacing (where the factory event carries one). It never touches
`PoolRegistry` or `MarketState`, never needs a private key, and never signs
or submits anything - a plain read-only check.

## Test

```bash
cargo test
```

## Safety

- **No private key is required or read anywhere in this codebase.**
- **There is no code path from this program to a submitted transaction.**
  `DexAdapter::build_swap_calldata` exists as a future boundary but always
  returns an error today.
- `ExecutionMode` (`DRY_RUN` / `SIMULATION` / `LIVE`) is defined for future
  days, but `ExecutionMode::can_execute_trades()` is hard-coded to `false`
  regardless of mode - `LIVE` has no working trade path in Day 1.
- No pool address is ever invented. If you don't set
  `AERODROME_POOL_ADDRESS` / `UNISWAP_V3_POOL_ADDRESS`, the program still
  runs (chain connectivity + generic ingestion), it just has nothing
  DEX-specific to watch.

## Not implemented yet

- Optimal trade sizing
- Full Uniswap V3 / Slipstream swap-simulation pricing math (tick-bitmap
  walking)
- Arbitrage opportunity detection
- REVM local simulation
- Balancer V2 flash loans
- `ArbExecutor.sol`
- Transaction signing
- Live trade execution
- Multi-hop graph arbitrage
- Full reorg reconciliation (Day 2 only skips `removed=true` logs
  defensively - it does not retroactively roll back state)
- Aerodrome Slipstream fee resolution (routes through
  `CLFactory.getSwapFee(pool)`, not hydrated - `fee_tier` is a `0`
  placeholder for Slipstream pools, never a real value)

## Known limitations / blockers

This was authored in a sandboxed build environment pinned to an old
`rustc` (1.75) that cannot resolve the modern crate graph at all (`edition2024`
requirements from transitive deps), so I could not run `cargo check` /
`cargo test` locally end-to-end for Day 2 either. Day 1 + the WebSocket-
optional change were fully verified on the operator's own machine across
several iterations; Day 2 has not yet been. Everything here was written by
diffing against real, current source (`alloy-rs/core` v1.6.0, `alloy-rs/
alloy` v2.4.1, the verified `PoolFactory`/`CLFactory`/`CLPool` contract
source on BaseScan and GitHub) rather than guessing, but "diffed against
source" is not the same as "compiled" - run `cargo check` / `cargo test` /
`cargo run` in your own environment and report back anything that doesn't
match.

Specific things flagged as uncertain rather than confirmed, called out
inline in the relevant module docs too:

- **Aerodrome Slipstream (`CLFactory`) factory address on Base** could not
  be independently confirmed (BaseScan's "SlipStream Pool Factory" label
  resolves to a `CLPool` implementation contract's ABI, not the factory's).
  No default is hardcoded; Slipstream discovery is disabled unless you set
  `AERODROME_SLIPSTREAM_FACTORY_ADDRESS` yourself. Aerodrome's
  `FactoryRegistry` (`0x5C3F18F06CC09CA1910767A34a20F771039E37C0` on Base,
  verified) is the documented way to look up the live factory address if
  you want to enable this.
- **Aerodrome Slipstream `PoolCreated` indexed/non-indexed parameter
  split** is inferred from the confirmed `emit PoolCreated(token0, token1,
  tickSpacing, pool)` call in `CLFactory.sol` plus the pattern both
  Uniswap V3's and Aerodrome classic's analogous events follow (first
  three logical fields indexed) - not confirmed against `ICLFactory.sol`'s
  actual interface declaration.
- **Aerodrome Slipstream `Swap` event shape** is assumed identical to
  Uniswap V3's (`sender, recipient, amount0, amount1, sqrtPriceX96,
  liquidity, tick`), based on Slipstream's documented lineage ("adapted
  from Uniswap V3's core contracts") - not independently confirmed from
  `CLPool`'s full event declarations.
- The HTTP `eth_getLogs` retry/range-reduction logic (`reduce_range_on_failure`,
  chunking) is tested as pure logic (no network in this sandbox) - the
  actual RPC-calling code path (`HttpLogPoller::fetch_range`) is
  unverified against a real provider's oversized-range error response.

'@
Set-Content -Path 'README.md' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote README.md'

Write-Host 'Done. Now run: cargo check'
Write-Host 'Then try: cargo run -- discover-test --help'