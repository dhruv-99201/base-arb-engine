//! M12: read-only historical WETH/USDC pool catalog.
//!
//! Discovers pool creation events from Aerodrome Classic and Uniswap V3.
//! This is a creation-event catalog only: it does not establish historical
//! liquidity or profitability. M10 performs block-pinned state validation.

use crate::chain::log_poller::HttpLogPoller;
use crate::config::Config;
use crate::dex::discovery::{
    AerodromeClassicDiscovery, DiscoveryParams, PoolDiscoveryAdapter, UniswapV3Discovery,
};
use crate::error::{EngineError, EngineResult};
use crate::market::models::DexKind;
use alloy::primitives::{address, Address};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

const WETH: Address = address!("4200000000000000000000000000000000000006");
const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");

pub const M12_CATALOG_USAGE: &str = "\
Usage: cargo run -- discover-pool-catalog --from-block <BLOCK> --to-block <BLOCK> --csv-out <PATH>

Read-only historical pool discovery for the Base WETH/USDC pair.

Scans the configured Aerodrome Classic and Uniswap V3 factories.
Includes volatile Aerodrome pools only; excludes stable pools and
Aerodrome Slipstream because the current M9 scanner does not support them.

The CSV records pool creation block and factory provenance. It does not
prove liquidity, quote validity, or profitability at any historical block.
M10 must validate pool state at the exact blocks being studied.

Options:
  --from-block <BLOCK>   First block to scan (inclusive). Required.
  --to-block <BLOCK>     Last block to scan (inclusive). Required.
  --csv-out <PATH>       Output CSV path. Required.
  --help, -h             Show this help and exit.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum M12CatalogCommand {
    Help,
    Run {
        from_block: u64,
        to_block: u64,
        csv_out: PathBuf,
    },
}

pub fn parse_m12_catalog_args(args: &[String]) -> Result<M12CatalogCommand, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(M12CatalogCommand::Help);
    }

    let mut from_block = None;
    let mut to_block = None;
    let mut csv_out = None;
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
            "--csv-out" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--csv-out requires a value".to_string())?;
                if raw.trim().is_empty() {
                    return Err("--csv-out cannot be empty".into());
                }
                csv_out = Some(PathBuf::from(raw));
                i += 2;
            }
            other => return Err(format!("unrecognized argument: '{other}'")),
        }
    }

    let from_block =
        from_block.ok_or_else(|| "missing required --from-block <BLOCK>".to_string())?;
    let to_block = to_block.ok_or_else(|| "missing required --to-block <BLOCK>".to_string())?;
    let csv_out = csv_out.ok_or_else(|| "missing required --csv-out <PATH>".to_string())?;

    if from_block > to_block {
        return Err(format!(
            "--from-block ({from_block}) must be <= --to-block ({to_block})"
        ));
    }

    Ok(M12CatalogCommand::Run {
        from_block,
        to_block,
        csv_out,
    })
}

fn is_weth_usdc_pair(token0: Address, token1: Address) -> bool {
    (token0 == WETH && token1 == USDC) || (token0 == USDC && token1 == WETH)
}

fn is_supported_pool(dex: &DexKind, params: &DiscoveryParams) -> bool {
    matches!(
        (dex, params),
        (
            DexKind::Aerodrome,
            DiscoveryParams::AerodromeClassic { stable: false }
        ) | (
            DexKind::UniswapV3,
            DiscoveryParams::ConcentratedLiquidity { .. }
        )
    )
}

/// Scan factory creation events and write a deduplicated WETH/USDC catalog.
/// No pool state is hydrated here; M10 performs historical state reads.
pub async fn run_m12_catalog(
    config: &Config,
    from_block: u64,
    to_block: u64,
    csv_out: PathBuf,
) -> EngineResult<()> {
    let poller = HttpLogPoller::new(config.base_rpc_url.clone(), config.log_poll_max_block_range);

    let adapters: Vec<Box<dyn PoolDiscoveryAdapter>> = vec![
        Box::new(UniswapV3Discovery::new(config.uniswap_v3_factory_address)),
        Box::new(AerodromeClassicDiscovery::new(
            config.aerodrome_factory_address,
        )),
    ];

    let mut seen = HashSet::new();

    let mut csv = String::from(
        "dex,pool_address,token0_address,token1_address,factory_address,\
created_block,tx_hash,stable,fee,tick_spacing\n",
    );

    let mut logs_seen = 0usize;
    let mut pair_events = 0usize;
    let mut decode_errors = 0usize;
    let mut duplicates = 0usize;
    let mut rows_written = 0usize;
    let mut queries_run = 0usize;

    println!("M12 catalog: scanning blocks {from_block}..={to_block}");
    println!("Using strict RPC range fetching; incomplete scans will abort.");

    for adapter in &adapters {
        let dex_name = adapter.dex().name();
        let factory = adapter.factory_address();

        // Aerodrome's third indexed event parameter is the stable flag.
        // Uniswap V3 uses topic3 for the fee, so do not filter it there.
        let topic3 = if dex_name.eq_ignore_ascii_case("Aerodrome") {
            Some(false)
        } else {
            None
        };

        println!("\nScanning {dex_name} factory {factory}");

        // First indexed token order: WETH / USDC.
        let mut logs = poller
            .fetch_logs_strict_indexed_pair(
                from_block,
                to_block,
                vec![factory],
                adapter.event_topic0(),
                WETH,
                USDC,
                topic3,
            )
            .await?;

        queries_run += 1;

        // Second indexed token order: USDC / WETH.
        let mut reverse_logs = poller
            .fetch_logs_strict_indexed_pair(
                from_block,
                to_block,
                vec![factory],
                adapter.event_topic0(),
                USDC,
                WETH,
                topic3,
            )
            .await?;

        queries_run += 1;
        logs.append(&mut reverse_logs);

        logs_seen += logs.len();

        println!("  indexed pair queries: 2");
        println!("  logs returned: {}", logs.len());

        for log in &logs {
            if log.removed {
                continue;
            }

            let discovered = match adapter.decode_pool_created(log) {
                Ok(pool) => pool,
                Err(error) => {
                    decode_errors += 1;
                    println!("  decode error; skipping: {error}");
                    continue;
                }
            };

            // Defensive local validation, even though the RPC query
            // already filters both indexed token addresses.
            if !is_weth_usdc_pair(discovered.token0_address, discovered.token1_address) {
                continue;
            }

            pair_events += 1;

            // Keep the scanner compatibility check as a second defense.
            if !is_supported_pool(&discovered.dex, &discovered.params) {
                continue;
            }

            if !seen.insert(discovered.pool_address) {
                duplicates += 1;
                continue;
            }

            let (stable, fee, tick_spacing) = match &discovered.params {
                DiscoveryParams::AerodromeClassic { stable } => {
                    (stable.to_string(), String::new(), String::new())
                }

                DiscoveryParams::ConcentratedLiquidity { tick_spacing, fee } => (
                    String::new(),
                    fee.map(|value| value.to_string()).unwrap_or_default(),
                    tick_spacing.to_string(),
                ),
            };

            csv.push_str(&format!(
                "{},{},{},{},{},{},{},{},{},{}\n",
                discovered.dex.name(),
                discovered.pool_address,
                discovered.token0_address,
                discovered.token1_address,
                discovered.factory_address,
                discovered.block_number,
                discovered.tx_hash,
                stable,
                fee,
                tick_spacing,
            ));

            rows_written += 1;
        }
    }

    // The CSV is written only after every strict RPC query has succeeded.
    // If an RPC range fails, the function returns before reaching this point.
    fs::write(&csv_out, csv).map_err(|error| {
        EngineError::Chain(format!(
            "failed to write M12 catalog '{}': {error}",
            csv_out.display()
        ))
    })?;

    println!("\nM12 catalog complete");
    println!("  strict indexed queries: {queries_run}");
    println!("  logs returned:          {logs_seen}");
    println!("  WETH/USDC events:       {pair_events}");
    println!("  decode errors:          {decode_errors}");
    println!("  duplicate pools:        {duplicates}");
    println!("  catalog rows written:   {rows_written}");
    println!("  output:                 {}", csv_out.display());

    println!("\nAerodrome stable pools were excluded by the topic3=false RPC filter.");
    println!("Uniswap V3 fee tiers were not restricted.");
    println!("Note: creation records are not proof of historical liquidity or profit.");

    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn parses_valid_catalog_arguments() {
        let parsed = parse_m12_catalog_args(&args(&[
            "--from-block",
            "26000000",
            "--to-block",
            "26100000",
            "--csv-out",
            "m12-pools.csv",
        ]));

        assert_eq!(
            parsed,
            Ok(M12CatalogCommand::Run {
                from_block: 26_000_000,
                to_block: 26_100_000,
                csv_out: PathBuf::from("m12-pools.csv"),
            })
        );
    }

    #[test]
    fn help_short_circuits_invalid_arguments() {
        assert_eq!(
            parse_m12_catalog_args(&args(&["--bad", "--help"])),
            Ok(M12CatalogCommand::Help)
        );
    }

    #[test]
    fn rejects_missing_csv_path() {
        let error = parse_m12_catalog_args(&args(&["--from-block", "100", "--to-block", "200"]))
            .unwrap_err();

        assert!(error.contains("--csv-out"));
    }

    #[test]
    fn rejects_reversed_block_range() {
        let error = parse_m12_catalog_args(&args(&[
            "--from-block",
            "200",
            "--to-block",
            "100",
            "--csv-out",
            "out.csv",
        ]))
        .unwrap_err();

        assert!(error.contains("must be <="));
    }

    #[test]
    fn matches_weth_usdc_in_either_order() {
        assert!(is_weth_usdc_pair(WETH, USDC));
        assert!(is_weth_usdc_pair(USDC, WETH));
        assert!(!is_weth_usdc_pair(WETH, Address::ZERO));
    }

    #[test]
    fn accepts_only_scanner_supported_pool_types() {
        assert!(is_supported_pool(
            &DexKind::Aerodrome,
            &DiscoveryParams::AerodromeClassic { stable: false }
        ));
        assert!(!is_supported_pool(
            &DexKind::Aerodrome,
            &DiscoveryParams::AerodromeClassic { stable: true }
        ));
        assert!(is_supported_pool(
            &DexKind::UniswapV3,
            &DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: 10,
                fee: Some(500),
            }
        ));
        assert!(!is_supported_pool(
            &DexKind::AerodromeSlipstream,
            &DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: 1,
                fee: Some(100),
            }
        ));
    }
}
