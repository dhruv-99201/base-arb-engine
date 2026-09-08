# base-arb-engine: fix PoolCreated/Swap event-signature bug + add discover-test diagnostics + inspect-tx
# Run from the repository root: C:\Users\Dell\Downloads\base-arb-engine
#   powershell -ExecutionPolicy Bypass -File apply_fix_event_signatures.ps1
Write-Host 'Applying event-signature fix + inspect-tx command...'

# Ensure all target directories exist first.
New-Item -ItemType Directory -Force -Path 'src' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\dex\discovery' | Out-Null
New-Item -ItemType Directory -Force -Path 'src\events' | Out-Null

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

'@
Set-Content -Path 'src\main.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/main.rs'

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
use crate::error::{EngineError, EngineResult};
use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};

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

pub const INSPECT_TX_USAGE: &str = "\
Usage: cargo run -- inspect-tx --tx <TX_HASH>

Read-only diagnostic command. Fetches the transaction receipt for <TX_HASH>
via eth_getTransactionReceipt (using BASE_RPC_URL) and prints every log it
contains: emitting address, all topics (including topic0 - compare this
directly against a discovery adapter's printed topic0 from `discover-test`
to see whether they match), and the block/tx it came from.

Useful for distinguishing 'the RPC never returned this log because our
eth_getLogs filter excluded it' from 'we have the right log but our decoder
rejects it' - this command bypasses eth_getLogs/filtering entirely and asks
for the receipt directly, so every log the transaction actually emitted is
shown regardless of any topic0/address filter.

Does not modify any blockchain state, requires no private key, and never
signs or submits a transaction.

Options:
  --tx <TX_HASH>   Transaction hash to inspect (0x-prefixed, 64 hex chars). Required.
  --help, -h       Show this help and exit.

Example:
  cargo run -- inspect-tx --tx 0x4104093239f998c41dab2b15864a1baa92198e62be57aa251fb724a320a76de6
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoverTestCommand {
    Help,
    Run { from_block: u64, to_block: u64 },
}/// Parse `discover-test` subcommand arguments (everything after
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectTxCommand {
    Help,
    Run { tx_hash: B256 },
}

/// Parse `inspect-tx` subcommand arguments. Pure function - no I/O, fully
/// unit-testable, same style as `parse_discover_test_args`.
pub fn parse_inspect_tx_args(args: &[String]) -> Result<InspectTxCommand, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(InspectTxCommand::Help);
    }

    let mut tx_hash: Option<B256> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--tx" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--tx requires a value".to_string())?;
                tx_hash = Some(
                    raw.parse::<B256>()
                        .map_err(|_| format!("invalid --tx value: '{raw}' (expected a 0x-prefixed 32-byte hash)"))?,
                );
                i += 2;
            }
            other => return Err(format!("unrecognized argument: '{other}'")),
        }
    }

    let tx_hash = tx_hash.ok_or_else(|| "missing required --tx <TX_HASH>".to_string())?;
    Ok(InspectTxCommand::Run { tx_hash })
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
        let topic0 = adapter.event_topic0();
        println!("\n--- {dex_name} (factory {factory}) ---");
        println!("  topic0={topic0:#x}");

        let logs = log_poller
            .fetch_logs(from_block, to_block, vec![factory], adapter.event_topic0())
            .await?;
        println!("  logs_returned={}", logs.len());

        for log in &logs {
            // Raw evidence first, regardless of decode outcome - this is
            // what distinguishes "RPC returned the log but our decoder
            // rejected it" from "RPC never returned it in the first place".
            println!(
                "  [raw] address={} topics={:?} tx_hash={:?}",
                log.inner.address,
                log.inner.topics(),
                log.transaction_hash
            );

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

/// Execute `inspect-tx`: fetch the transaction receipt directly
/// (`eth_getTransactionReceipt`) and print every log it contains, with no
/// address/topic filtering at all - this is the ground truth for "what did
/// this transaction actually emit", independent of whether our discovery
/// adapters' `eth_getLogs` filters would have matched it. Read-only.
pub async fn run_inspect_tx(config: &Config, tx_hash: B256) -> EngineResult<()> {
    let url = config
        .base_rpc_url
        .parse()
        .map_err(|e| EngineError::Config(format!("invalid BASE_RPC_URL: {e}")))?;
    let provider = ProviderBuilder::new().connect_http(url);

    println!("inspect-tx: {tx_hash}");
    println!("  BASE_RPC_URL: {}", config.base_rpc_url);

    let receipt = provider
        .get_transaction_receipt(tx_hash)
        .await
        .map_err(|e| EngineError::Chain(format!("get_transaction_receipt failed: {e}")))?;

    let Some(receipt) = receipt else {
        println!(
            "\nNo receipt found for this hash via BASE_RPC_URL. Either the transaction doesn't \
             exist on this chain/endpoint, or it hasn't been indexed by this particular RPC \
             provider yet."
        );
        return Ok(());
    };

    println!("\nreceipt found:");
    println!("  status={}", receipt.status());
    println!("  block_number={:?}", receipt.block_number);
    println!("  block_hash={:?}", receipt.block_hash);
    println!("  transaction_index={:?}", receipt.transaction_index);

    let logs = receipt.logs();
    println!("  logs_in_receipt={}", logs.len());

    for (i, log) in logs.iter().enumerate() {
        println!("\n  --- log[{i}] ---");
        println!("    address={}", log.inner.address);
        println!("    topics={:?}", log.inner.topics());
        println!("    data={}", log.inner.data.data);
        println!("    log_index={:?}", log.log_index);
        println!("    removed={}", log.removed);
        if let Some(topic0) = log.inner.topics().first() {
            println!(
                "    topic0={topic0:#x}  (compare against discover-test's printed topic0 for a match)"
            );
        } else {
            println!("    topic0=<none - anonymous event or no topics>");
        }
    }

    if logs.is_empty() {
        println!("\nThis transaction emitted no logs at all.");
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

    #[test]
    fn inspect_tx_parses_valid_tx_hash() {
        let hash = "0x4104093239f998c41dab2b15864a1baa92198e62be57aa251fb724a320a76de6";
        let result = parse_inspect_tx_args(&args(&["--tx", hash]));
        assert_eq!(
            result,
            Ok(InspectTxCommand::Run {
                tx_hash: hash.parse().unwrap()
            })
        );
    }

    #[test]
    fn inspect_tx_help_flag_works() {
        assert_eq!(
            parse_inspect_tx_args(&args(&["--help"])),
            Ok(InspectTxCommand::Help)
        );
        assert_eq!(
            parse_inspect_tx_args(&args(&["-h"])),
            Ok(InspectTxCommand::Help)
        );
    }

    #[test]
    fn inspect_tx_missing_tx_flag_is_rejected() {
        let err = parse_inspect_tx_args(&args(&[])).unwrap_err();
        assert!(err.contains("--tx"));
    }

    #[test]
    fn inspect_tx_malformed_hash_is_rejected() {
        let err = parse_inspect_tx_args(&args(&["--tx", "not-a-hash"])).unwrap_err();
        assert!(err.contains("invalid --tx value"));
    }

    #[test]
    fn inspect_tx_short_hash_is_rejected() {
        // 38 hex chars instead of the required 64 - exactly the class of
        // bug this task's own fixture data caught in an earlier turn.
        let err = parse_inspect_tx_args(&args(&["--tx", "0x1234"])).unwrap_err();
        assert!(err.contains("invalid --tx value"));
    }

    #[test]
    fn inspect_tx_dangling_flag_without_value_is_rejected() {
        let err = parse_inspect_tx_args(&args(&["--tx"])).unwrap_err();
        assert!(err.contains("--tx requires a value"));
    }

    #[test]
    fn inspect_tx_unrecognized_argument_is_rejected() {
        let err = parse_inspect_tx_args(&args(&["--wat", "1"])).unwrap_err();
        assert!(err.contains("unrecognized argument"));
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

# ---- src/events/decoder.rs ----
$content = @'
//! Decodes raw chain logs into normalized `MarketEvent`s.
//!
//! Decoding is protocol-specific (Aerodrome's Solidly-style Swap event vs
//! Uniswap V3's concentrated-liquidity Swap event have different shapes),
//! but the *output* is always the same normalized `MarketEvent`. Malformed
//! logs are rejected with a clear error rather than silently dropped or
//! guessed at.

use crate::error::{EngineError, EngineResult};
use crate::events::model::{EventKind, MarketEvent, SwapEvent};
use crate::market::models::DexKind;
use alloy::primitives::Log as PrimitiveLog;
use alloy::rpc::types::Log as RpcLog;
use alloy::sol_types::SolEvent;

// IMPORTANT: `alloy::sol!` computes each event's on-chain signature hash
// (topic0) from the literal event name declared here - NOT from the Rust
// item/module name. `AerodromeSwap`/`UniswapV3Swap` (the names used prior
// to this fix) therefore hashed to `keccak256("AerodromeSwap(...)")` /
// `keccak256("UniswapV3Swap(...)")`, which never matches any real on-chain
// log (the actual Solidity event is just `Swap` in both cases). Each event
// is wrapped in its own private module here, both literally named `Swap`,
// so the *signature* is correct while the Rust bindings stay distinct
// (`aerodrome_swap_event::Swap` vs `uniswap_v3_swap_event::Swap`) without a
// name collision in this file. See also `dex::discovery::*`, which had the
// exact same bug for `PoolCreated` and is fixed the same way (those events
// each live in their own file/module already, so no wrapper was needed
// there - just the identifier itself was corrected).
mod aerodrome_swap_event {
    use alloy::sol;
    sol! {
        /// Aerodrome (Solidly-fork) pool Swap event.
        event Swap(
            address indexed sender,
            address indexed to,
            uint256 amount0In,
            uint256 amount1In,
            uint256 amount0Out,
            uint256 amount1Out
        );
    }
}

mod uniswap_v3_swap_event {
    use alloy::sol;
    sol! {
        /// Uniswap V3 pool Swap event.
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick
        );
    }
}

use aerodrome_swap_event::Swap as AerodromeSwap;
use uniswap_v3_swap_event::Swap as UniswapV3Swap;

/// Current wall-clock time in microseconds since the Unix epoch.
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// keccak256 topic0 for `AerodromeSwap`. Exposed so the Day 2 log-polling
/// pipeline can filter `eth_getLogs` queries to just this event without
/// duplicating the event definition.
pub fn aerodrome_swap_topic0() -> alloy::primitives::B256 {
    AerodromeSwap::SIGNATURE_HASH
}

/// keccak256 topic0 for `UniswapV3Swap`. Also used for Aerodrome Slipstream
/// (`CLPool`) swap scanning, which reuses this event shape - see
/// `dex::aerodrome_slipstream` module docs.
pub fn uniswap_v3_swap_topic0() -> alloy::primitives::B256 {
    UniswapV3Swap::SIGNATURE_HASH
}

/// Decode a raw RPC log from a known Aerodrome pool into a `MarketEvent`.
///
/// `received_at_us` should be captured by the caller at the moment the log
/// was received from the transport, before any decoding work happens, so
/// `processing_latency_us` reflects actual decode+normalize cost.
pub fn decode_aerodrome_log(
    log: &RpcLog,
    chain_id: u64,
    received_at_us: u64,
) -> EngineResult<MarketEvent> {
    let processing_started_at_us = now_us();

    let block_number = log
        .block_number
        .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;
    let log_index = log
        .log_index
        .ok_or_else(|| EngineError::MalformedEvent("log missing log_index".into()))?;
    let pool_address = log.inner.address;

    let decoded_log = AerodromeSwap::decode_log(&log.inner).map_err(|e| {
        EngineError::Decode(format!("failed to decode AerodromeSwap log: {e}"))
    })?;
    let decoded = decoded_log.data;

    // Solidly-style pools report gross in/out per side rather than a single
    // signed net amount; normalize to net-into-pool the same way Uniswap V3
    // does, so downstream code has one shape to reason about.
    let amount0_in = i128::try_from(decoded.amount0In)
        .map_err(|_| EngineError::MalformedEvent("amount0In overflows i128".into()))?;
    let amount0_out = i128::try_from(decoded.amount0Out)
        .map_err(|_| EngineError::MalformedEvent("amount0Out overflows i128".into()))?;
    let amount1_in = i128::try_from(decoded.amount1In)
        .map_err(|_| EngineError::MalformedEvent("amount1In overflows i128".into()))?;
    let amount1_out = i128::try_from(decoded.amount1Out)
        .map_err(|_| EngineError::MalformedEvent("amount1Out overflows i128".into()))?;

    let amount0 = amount0_in
        .checked_sub(amount0_out)
        .ok_or_else(|| EngineError::Arithmetic("amount0 net overflow".into()))?;
    let amount1 = amount1_in
        .checked_sub(amount1_out)
        .ok_or_else(|| EngineError::Arithmetic("amount1 net overflow".into()))?;

    let swap = SwapEvent {
        pool_address,
        dex: DexKind::Aerodrome,
        amount0,
        amount1,
        sender: Some(decoded.sender),
        recipient: Some(decoded.to),
    };

    let processing_finished_at_us = now_us();

    Ok(MarketEvent::new(
        chain_id,
        block_number,
        None,
        tx_hash,
        log_index,
        pool_address,
        DexKind::Aerodrome,
        EventKind::Swap(swap),
        received_at_us,
        processing_started_at_us,
        processing_finished_at_us,
    ))
}

/// Decode a raw RPC log from a known Uniswap V3 pool into a `MarketEvent`.
pub fn decode_uniswap_v3_log(
    log: &RpcLog,
    chain_id: u64,
    received_at_us: u64,
) -> EngineResult<MarketEvent> {
    let processing_started_at_us = now_us();

    let block_number = log
        .block_number
        .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;
    let log_index = log
        .log_index
        .ok_or_else(|| EngineError::MalformedEvent("log missing log_index".into()))?;
    let pool_address = log.inner.address;

    let decoded_log = UniswapV3Swap::decode_log(&log.inner).map_err(|e| {
        EngineError::Decode(format!("failed to decode UniswapV3Swap log: {e}"))
    })?;
    let decoded = decoded_log.data;

    let amount0 = i128::try_from(decoded.amount0)
        .map_err(|_| EngineError::MalformedEvent("amount0 overflows i128".into()))?;
    let amount1 = i128::try_from(decoded.amount1)
        .map_err(|_| EngineError::MalformedEvent("amount1 overflows i128".into()))?;

    let swap = SwapEvent {
        pool_address,
        dex: DexKind::UniswapV3,
        amount0,
        amount1,
        sender: Some(decoded.sender),
        recipient: Some(decoded.recipient),
    };

    let processing_finished_at_us = now_us();

    Ok(MarketEvent::new(
        chain_id,
        block_number,
        None,
        tx_hash,
        log_index,
        pool_address,
        DexKind::UniswapV3,
        EventKind::Swap(swap),
        received_at_us,
        processing_started_at_us,
        processing_finished_at_us,
    ))
}

/// Decode a raw RPC log from a known Aerodrome Slipstream (`CLPool`) pool.
///
/// Reuses the `UniswapV3Swap` event shape: Slipstream's `Swap` event is
/// documented as "adapted from Uniswap V3's core contracts", and shares the
/// same `(sender, recipient, amount0, amount1, sqrtPriceX96, liquidity,
/// tick)` signature in every source reviewed for this implementation. If
/// real Slipstream swap logs fail to decode against this shape, that
/// assumption is the first thing to re-verify (see
/// `dex::aerodrome_slipstream` module docs for the same caveat on the
/// discovery event).
pub fn decode_aerodrome_slipstream_log(
    log: &RpcLog,
    chain_id: u64,
    received_at_us: u64,
) -> EngineResult<MarketEvent> {
    let processing_started_at_us = now_us();

    let block_number = log
        .block_number
        .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;
    let log_index = log
        .log_index
        .ok_or_else(|| EngineError::MalformedEvent("log missing log_index".into()))?;
    let pool_address = log.inner.address;

    let decoded_log = UniswapV3Swap::decode_log(&log.inner).map_err(|e| {
        EngineError::Decode(format!("failed to decode Slipstream Swap log: {e}"))
    })?;
    let decoded = decoded_log.data;

    let amount0 = i128::try_from(decoded.amount0)
        .map_err(|_| EngineError::MalformedEvent("amount0 overflows i128".into()))?;
    let amount1 = i128::try_from(decoded.amount1)
        .map_err(|_| EngineError::MalformedEvent("amount1 overflows i128".into()))?;

    let swap = SwapEvent {
        pool_address,
        dex: DexKind::AerodromeSlipstream,
        amount0,
        amount1,
        sender: Some(decoded.sender),
        recipient: Some(decoded.recipient),
    };

    let processing_finished_at_us = now_us();

    Ok(MarketEvent::new(
        chain_id,
        block_number,
        None,
        tx_hash,
        log_index,
        pool_address,
        DexKind::AerodromeSlipstream,
        EventKind::Swap(swap),
        received_at_us,
        processing_started_at_us,
        processing_finished_at_us,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, Address, Bytes, B256};
    use alloy::rpc::types::Log as RpcLog;
    use alloy::sol_types::SolEvent;

    fn build_log(topics: Vec<B256>, data: Bytes, addr: Address) -> RpcLog {
        let inner = PrimitiveLog::new_unchecked(addr, topics, data);
        RpcLog {
            inner,
            block_hash: None,
            block_number: Some(12_345_678),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(0xAB)),
            transaction_index: Some(0),
            log_index: Some(3),
            removed: false,
        }
    }

    #[test]
    fn valid_uniswap_v3_swap_decodes() {
        let pool = address!("4200000000000000000000000000000000000006");
        let sender = address!("1111111111111111111111111111111111111111");
        let recipient = address!("2222222222222222222222222222222222222222");

        let event = UniswapV3Swap {
            sender,
            recipient,
            amount0: alloy::primitives::I256::try_from(1_000_000_i64).unwrap(),
            amount1: alloy::primitives::I256::try_from(-2_000_000_i64).unwrap(),
            sqrtPriceX96: alloy::primitives::U160::from(79_228_162_514_264_337_593_543_950_336u128),
            liquidity: 123_456_789_u128,
            tick: alloy::primitives::aliases::I24::try_from(-1234i32).unwrap(),
        };
        let encoded = event.encode_log_data();
        let log = build_log(encoded.topics().to_vec(), encoded.data.clone(), pool);

        let market_event = decode_uniswap_v3_log(&log, 8453, now_us()).expect("should decode");
        match market_event.kind {
            EventKind::Swap(swap) => {
                assert_eq!(swap.amount0, 1_000_000);
                assert_eq!(swap.amount1, -2_000_000);
                assert_eq!(swap.dex, DexKind::UniswapV3);
                assert_eq!(swap.sender, Some(sender));
                assert_eq!(swap.recipient, Some(recipient));
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn malformed_log_is_rejected() {
        let pool = address!("4200000000000000000000000000000000000006");
        // Wrong topic0 (event signature) - decoder must reject, not guess.
        let bogus_topic = B256::repeat_byte(0x11);
        let log = build_log(vec![bogus_topic], Bytes::new(), pool);

        let result = decode_uniswap_v3_log(&log, 8453, now_us());
        assert!(result.is_err(), "malformed/mismatched log must be rejected");
    }

    #[test]
    fn log_missing_block_number_is_rejected() {
        let pool = address!("4200000000000000000000000000000000000006");
        let event = UniswapV3Swap {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            amount0: alloy::primitives::I256::ZERO,
            amount1: alloy::primitives::I256::ZERO,
            sqrtPriceX96: alloy::primitives::U160::ZERO,
            liquidity: 0,
            tick: alloy::primitives::aliases::I24::ZERO,
        };
        let encoded = event.encode_log_data();
        let mut log = build_log(encoded.topics().to_vec(), encoded.data.clone(), pool);
        log.block_number = None;

        let result = decode_uniswap_v3_log(&log, 8453, now_us());
        assert!(matches!(result, Err(EngineError::MalformedEvent(_))));
    }
}

'@
Set-Content -Path 'src\events\decoder.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/events/decoder.rs'

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

'@
Set-Content -Path 'src\dex\discovery\uniswap_v3.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/uniswap_v3.rs'

# ---- src/dex/discovery/aerodrome_classic.rs ----
$content = @'
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

'@
Set-Content -Path 'src\dex\discovery\aerodrome_classic.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/aerodrome_classic.rs'

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

'@
Set-Content -Path 'src\dex\discovery\aerodrome_slipstream.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/dex/discovery/aerodrome_slipstream.rs'

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
or submits anything - a plain read-only check. Each adapter's exact
`topic0` and every raw log's address/topics are printed alongside the
decoded results, so a mismatch between "what we're filtering for" and
"what's actually on-chain" is visible directly.

### `inspect-tx`: ground-truth log inspection for one transaction

If `discover-test` isn't finding a `PoolCreated` event you know exists in a
given transaction, `inspect-tx` bypasses `eth_getLogs` filtering entirely
and asks for that transaction's receipt directly - showing every log it
actually emitted, regardless of address/topic filters:

```bash
cargo run -- inspect-tx --help
cargo run -- inspect-tx --tx <TX_HASH>
```

Compare the printed `topic0` for each log against `discover-test`'s printed
topic0 for the relevant DEX - if they don't match, the event
name/signature used to compute the filter is wrong; if they match but
`discover-test` still shows nothing, the address filter or block range is
the problem instead. Also read-only: no private key, no signing, no state
changes.

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
Write-Host 'Then: cargo test'
Write-Host 'Then: cargo run -- discover-test --from-block 50335298 --to-block 50335298'
Write-Host 'Then (if needed): cargo run -- inspect-tx --tx 0x4104093239f998c41dab2b15864a1baa92198e62be57aa251fb724a320a76de6'