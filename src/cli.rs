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
Aerodrome (classic) factory addresses, plus the configured Aerodrome
Slipstream factories (AERODROME_SLIPSTREAM_FACTORY_ADDRESSES - defaults to
three verified deployments), for PoolCreated events in
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
    for addr in &config.aerodrome_slipstream_factory_addresses {
        adapters.push(Box::new(AerodromeSlipstreamDiscovery::new(*addr)));
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
