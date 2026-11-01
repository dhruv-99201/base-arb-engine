//! M9: historical discrete trade-size sweep for the existing cross-DEX scanner.
//!
//! This module reuses the M8 pinned pool readers and M6 detector. It evaluates
//! each caller-supplied input size against the same pool snapshot and exports
//! every route outcome to CSV. An optional, caller-supplied all-in cost (in the
//! starting token's smallest units) is subtracted as a scenario assumption; the
//! engine does not estimate gas, token prices, or execution costs itself.
//!
//! Read-only: no signer, transaction builder, or execution path is used.

use crate::arbitrage::{detect_cross_dex_opportunities_at_block, RouteEvaluationFailure};
use crate::cli::{build_scan_candidates, PinnedPoolRead};
use crate::config::Config;
use crate::dex::{AerodromeAdapter, UniswapV3Adapter};
use crate::error::{EngineError, EngineResult};
use crate::market::models::{DexKind, Pool, PoolKind, Token};
use alloy::primitives::{Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

const DEFAULT_WORD_RADIUS: i32 = 1;
const MAX_SWEEP_SIZES: usize = 50;

pub const M9_SIZE_SWEEP_USAGE: &str = "\
Usage: cargo run -- scan-arb-sizes --aerodrome-pool <ADDRESS> [--aerodrome-pool <ADDRESS> ...] --uniswap-v3-pool <ADDRESS> [--uniswap-v3-pool <ADDRESS> ...] --block <U64> --token-in <ADDRESS> --size-in <RAW_INTEGER> --size-in <RAW_INTEGER> [--size-in <RAW_INTEGER> ...] --csv-out <PATH> [--estimated-cost-raw <RAW_INTEGER>] [--word-radius <I32>]

M9 read-only historical trade-size sweep. Loads the supplied Aerodrome Classic
and Uniswap V3 pools once at ONE pinned block, then evaluates every valid
cross-DEX route at each supplied input size. Every route outcome, including
losses and quote failures, is printed and written to CSV.

All size and cost values are base-10 integers in the starting token's
smallest units. No floating point is used in financial calculations.

--estimated-cost-raw is OPTIONAL and is supplied by the operator. It is a
single all-in cost assumption in the starting token's smallest units, applied
to EVERY route and EVERY size. The engine does NOT estimate gas, convert ETH
to the starting token, fetch token/USD prices, or validate this assumption.
When supplied, estimated_net_profit_raw = gross_profit_raw - estimated_cost_raw.
Without it, only gross profit is reported and no net estimate is claimed.

Pool reads are sequential and pinned to the requested block. Failed pool
loads are isolated. Uniswap V3 tick hydration is limited by --word-radius;
large sizes may fail to quote if they need ticks outside the hydrated range.
An archive-capable RPC may be required for historical blocks.

This command is research output, not a live trading signal. It does not sign
or submit transactions and does not prove a route is executable.

Options:
  --aerodrome-pool <ADDRESS>   Aerodrome Classic volatile pool; repeatable.
  --uniswap-v3-pool <ADDRESS>  Uniswap V3 pool; repeatable.
  --block <U64>                Historical block number; required.
  --token-in <ADDRESS>         Starting token address; required.
  --size-in <RAW_INTEGER>      Positive input size in token-in smallest units;
                               repeatable, 2..=50 distinct values required.
  --estimated-cost-raw <N>     Optional operator-supplied all-in cost in
                               token-in smallest units; zero is allowed.
  --csv-out <PATH>             New CSV file path; required and must not exist.
  --word-radius <I32>          V3 tick-bitmap word radius (default: 1).
  --help, -h                   Show this help and exit.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M9SizeSweepArgs {
    pub aerodrome_pools: Vec<Address>,
    pub uniswap_v3_pools: Vec<Address>,
    pub block: u64,
    pub token_in: Address,
    pub sizes: Vec<U256>,
    pub estimated_cost_raw: Option<U256>,
    pub csv_out: PathBuf,
    pub word_radius: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum M9SizeSweepCommand {
    Help,
    Run(M9SizeSweepArgs),
}

fn parse_u256_decimal(raw: &str, flag: &str) -> Result<U256, String> {
    if raw.is_empty() {
        return Err(format!("{flag} requires a non-empty base-10 integer"));
    }
    U256::from_str_radix(raw, 10)
        .map_err(|_| format!("invalid {flag} value: '{raw}' (expected a base-10 integer)"))
}

/// Parse the M9 command entirely before configuration loading or RPC access.
pub fn parse_m9_size_sweep_args(args: &[String]) -> Result<M9SizeSweepCommand, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(M9SizeSweepCommand::Help);
    }

    let mut aerodrome_pools = Vec::new();
    let mut uniswap_v3_pools = Vec::new();
    let mut seen_pools = Vec::new();
    let mut sizes = Vec::new();
    let mut block = None;
    let mut token_in = None;
    let mut estimated_cost_raw = None;
    let mut csv_out = None;
    let mut word_radius = DEFAULT_WORD_RADIUS;

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        let raw = args
            .get(i + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;

        match flag {
            "--aerodrome-pool" | "--uniswap-v3-pool" => {
                let address = raw
                    .parse::<Address>()
                    .map_err(|_| format!("invalid {flag} value: '{raw}'"))?;
                if seen_pools.contains(&address) {
                    return Err(format!("duplicate pool address: {address}"));
                }
                seen_pools.push(address);
                if flag == "--aerodrome-pool" {
                    aerodrome_pools.push(address);
                } else {
                    uniswap_v3_pools.push(address);
                }
            }
            "--block" => {
                if block.is_some() {
                    return Err("--block may only be supplied once".into());
                }
                block = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid --block value: '{raw}'"))?,
                );
            }
            "--token-in" => {
                if token_in.is_some() {
                    return Err("--token-in may only be supplied once".into());
                }
                token_in = Some(
                    raw.parse::<Address>()
                        .map_err(|_| format!("invalid --token-in value: '{raw}'"))?,
                );
            }
            "--size-in" => {
                let size = parse_u256_decimal(raw, "--size-in")?;
                if size.is_zero() {
                    return Err("--size-in must be greater than zero".into());
                }
                if sizes.contains(&size) {
                    return Err(format!("duplicate --size-in value: {size}"));
                }
                sizes.push(size);
            }
            "--estimated-cost-raw" => {
                if estimated_cost_raw.is_some() {
                    return Err("--estimated-cost-raw may only be supplied once".into());
                }
                let cost = parse_u256_decimal(raw, "--estimated-cost-raw")?;
                I256::try_from(cost).map_err(|_| {
                    "--estimated-cost-raw is too large for signed net-profit arithmetic".to_string()
                })?;
                estimated_cost_raw = Some(cost);
            }
            "--csv-out" => {
                if csv_out.is_some() {
                    return Err("--csv-out may only be supplied once".into());
                }
                if raw.trim().is_empty() {
                    return Err("--csv-out path must not be empty".into());
                }
                csv_out = Some(PathBuf::from(raw));
            }
            "--word-radius" => {
                let parsed = raw.parse::<i32>().map_err(|_| {
                    format!(
                        "invalid --word-radius value: '{raw}' (expected a non-negative integer)"
                    )
                })?;
                if parsed < 0 {
                    return Err(format!("--word-radius must be non-negative, got {parsed}"));
                }
                word_radius = parsed;
            }
            other => return Err(format!("unrecognized argument: '{other}'")),
        }
        i += 2;
    }

    if aerodrome_pools.is_empty() {
        return Err("at least one --aerodrome-pool is required".into());
    }
    if uniswap_v3_pools.is_empty() {
        return Err("at least one --uniswap-v3-pool is required".into());
    }
    if sizes.len() < 2 {
        return Err("at least two distinct --size-in values are required for a sweep".into());
    }
    if sizes.len() > MAX_SWEEP_SIZES {
        return Err(format!(
            "at most {MAX_SWEEP_SIZES} --size-in values are allowed per sweep"
        ));
    }

    Ok(M9SizeSweepCommand::Run(M9SizeSweepArgs {
        aerodrome_pools,
        uniswap_v3_pools,
        block: block.ok_or_else(|| "missing required --block <U64>".to_string())?,
        token_in: token_in.ok_or_else(|| "missing required --token-in <ADDRESS>".to_string())?,
        sizes,
        estimated_cost_raw,
        csv_out: csv_out.ok_or_else(|| "missing required --csv-out <PATH>".to_string())?,
        word_radius,
    }))
}

fn placeholder_token() -> Token {
    Token {
        address: Address::ZERO,
        symbol: "UNKNOWN".into(),
        decimals: 0,
    }
}

fn scan_placeholder_aerodrome_pool(address: Address) -> Pool {
    Pool {
        address,
        dex: DexKind::Aerodrome,
        token0: placeholder_token(),
        token1: placeholder_token(),
        kind: PoolKind::Aerodrome {
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
            stable: false,
            fee_bps: None,
        },
    }
}

fn scan_placeholder_v3_pool(address: Address) -> Pool {
    Pool {
        address,
        dex: DexKind::UniswapV3,
        token0: placeholder_token(),
        token1: placeholder_token(),
        kind: PoolKind::ConcentratedLiquidity {
            fee_tier: 0,
            tick_spacing: 1,
            sqrt_price_x96: U256::ZERO,
            current_tick: 0,
            liquidity: 0,
            initialized_ticks: Default::default(),
        },
    }
}

fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

fn append_csv_row(csv: &mut String, fields: &[String]) {
    csv.push_str(
        &fields
            .iter()
            .map(|field| csv_escape(field))
            .collect::<Vec<_>>()
            .join(","),
    );
    csv.push('\n');
}

fn failure_details(failure: &RouteEvaluationFailure) -> (&'static str, String) {
    match failure {
        RouteEvaluationFailure::InvalidRouteCandidateMapping { error } => {
            ("InvalidRouteCandidateMapping", error.to_string())
        }
        RouteEvaluationFailure::Leg1QuoteFailed { error } => ("Leg1QuoteFailed", error.to_string()),
        RouteEvaluationFailure::Leg2QuoteFailed { error, .. } => {
            ("Leg2QuoteFailed", error.to_string())
        }
        RouteEvaluationFailure::OpportunityEvaluationFailed { error, .. } => {
            ("OpportunityEvaluationFailed", error.to_string())
        }
    }
}

#[derive(Debug)]
struct BestObserved {
    metric: I256,
    size: U256,
    route_index: usize,
}

fn checked_estimated_net_profit(gross_profit: I256, estimated_cost: I256) -> Option<I256> {
    gross_profit.checked_sub(estimated_cost)
}

fn update_best(best: &mut Option<BestObserved>, metric: I256, size: U256, route_index: usize) {
    let replace = match best {
        Some(current) => metric > current.metric,
        None => true,
    };
    if replace {
        *best = Some(BestObserved {
            metric,
            size,
            route_index,
        });
    }
}

/// Load each supplied pool once, then evaluate all requested sizes against
/// exactly that same pinned snapshot. CSV output is created exclusively (it
/// never overwrites an existing file).
pub async fn run_m9_size_sweep(config: &Config, args: M9SizeSweepArgs) -> EngineResult<()> {
    if args.csv_out.exists() {
        return Err(EngineError::Config(format!(
            "CSV output already exists: {}; choose a new --csv-out path",
            args.csv_out.display()
        )));
    }

    let url = config
        .base_rpc_url
        .parse()
        .map_err(|e| EngineError::Config(format!("invalid BASE_RPC_URL: {e}")))?;
    let provider = ProviderBuilder::new().connect_http(url);
    let chain_id = provider
        .get_chain_id()
        .await
        .map_err(|e| EngineError::Chain(format!("get_chain_id failed: {e}")))?;
    if chain_id != config.base_chain_id {
        return Err(EngineError::Chain(format!(
            "chain ID mismatch: BASE_CHAIN_ID is {}, RPC endpoint reports {chain_id}",
            config.base_chain_id
        )));
    }

    let aerodrome_adapter =
        AerodromeAdapter::with_factory_address(config.aerodrome_factory_address);
    let uniswap_v3_adapter = UniswapV3Adapter::new();
    let mut reads: Vec<(DexKind, Address, EngineResult<PinnedPoolRead>)> =
        Vec::with_capacity(args.aerodrome_pools.len() + args.uniswap_v3_pools.len());

    for address in &args.aerodrome_pools {
        let read = aerodrome_adapter
            .get_pool_state_at_block(
                &config.base_rpc_url,
                &scan_placeholder_aerodrome_pool(*address),
                args.block,
            )
            .await
            .map(PinnedPoolRead::Aerodrome);
        reads.push((DexKind::Aerodrome, *address, read));
    }
    for address in &args.uniswap_v3_pools {
        let read = uniswap_v3_adapter
            .get_pool_state_and_ticks_at_block(
                &config.base_rpc_url,
                &scan_placeholder_v3_pool(*address),
                args.word_radius,
                Some(args.block),
            )
            .await
            .map(|(state, ticks)| PinnedPoolRead::UniswapV3(state, ticks));
        reads.push((DexKind::UniswapV3, *address, read));
    }

    let (loads, candidates) = build_scan_candidates(args.block, reads);
    let cost_signed = args
        .estimated_cost_raw
        .map(I256::try_from)
        .transpose()
        .map_err(|_| EngineError::Arithmetic("estimated cost does not fit I256".into()))?;

    let mut report = String::new();
    report.push_str("M9 historical size-sweep report\n");
    report.push_str(&format!(
        "chain_id={chain_id}\nblock={}\ntoken_in={}\nsize_points={}\nv3_word_radius={}\n",
        args.block,
        args.token_in,
        args.sizes.len(),
        args.word_radius
    ));
    match args.estimated_cost_raw {
        Some(cost) => report.push_str(&format!(
            "estimated_cost_raw={cost}\ncost_model=operator_supplied_constant_applied_to_every_route_and_size\n"
        )),
        None => report.push_str(
            "estimated_cost_raw=NOT_PROVIDED\ncost_model=none; only gross profit is reported\n",
        ),
    }
    report.push_str(&format!(
        "aerodrome_pools_requested={}\naerodrome_pools_loaded={}\naerodrome_pools_failed={}\nuniswap_v3_pools_requested={}\nuniswap_v3_pools_loaded={}\nuniswap_v3_pools_failed={}\n",
        loads.iter().filter(|load| load.dex == DexKind::Aerodrome).count(),
        loads.iter().filter(|load| load.dex == DexKind::Aerodrome && load.result.is_ok()).count(),
        loads.iter().filter(|load| load.dex == DexKind::Aerodrome && load.result.is_err()).count(),
        loads.iter().filter(|load| load.dex == DexKind::UniswapV3).count(),
        loads.iter().filter(|load| load.dex == DexKind::UniswapV3 && load.result.is_ok()).count(),
        loads.iter().filter(|load| load.dex == DexKind::UniswapV3 && load.result.is_err()).count(),
    ));
    for (index, load) in loads.iter().enumerate() {
        match &load.result {
            Ok((token0, token1)) => report.push_str(&format!(
                "\n[pool {}] dex={} pool={} status=LOADED token0={} token1={}\n",
                index + 1,
                load.dex,
                load.address,
                token0,
                token1
            )),
            Err(error) => report.push_str(&format!(
                "\n[pool {}] dex={} pool={} status=FAILED error={}\n",
                index + 1,
                load.dex,
                load.address,
                error
            )),
        }
    }

    let mut csv = String::from(
        "block,token_in,size_in_raw,route_index,start_token,intermediate_token,leg1_dex,leg1_pool,leg2_dex,leg2_pool,status,intermediate_amount_raw,final_amount_raw,gross_profit_raw,estimated_cost_raw,estimated_net_profit_raw,net_profit_status,failure_kind,failure_detail\n",
    );
    let mut best: Option<BestObserved> = None;
    let mut total_routes = 0usize;
    let mut successful_routes = 0usize;
    let mut failed_routes = 0usize;
    let mut detector_failed = false;

    for (size_index, size) in args.sizes.iter().enumerate() {
        report.push_str(&format!(
            "\n[size {}] amount_in_raw={}\n",
            size_index + 1,
            size
        ));
        let detection =
            detect_cross_dex_opportunities_at_block(args.block, args.token_in, *size, &candidates);

        let outcomes = match detection {
            Ok(outcomes) => outcomes,
            Err(error) => {
                detector_failed = true;
                report.push_str(&format!("route_detection=FAILED error={error}\n"));
                append_csv_row(
                    &mut csv,
                    &vec![
                        args.block.to_string(),
                        args.token_in.to_string(),
                        size.to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        "DETECTION_FAILED".into(),
                        String::new(),
                        String::new(),
                        String::new(),
                        args.estimated_cost_raw
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                        String::new(),
                        "NOT_AVAILABLE".into(),
                        "DetectorError".into(),
                        error.to_string(),
                    ],
                );
                continue;
            }
        };

        report.push_str(&format!("routes_evaluated={}\n", outcomes.len()));
        if outcomes.is_empty() {
            report.push_str("no route matched token_in for this size\n");
        }

        for (route_index, outcome) in outcomes.iter().enumerate() {
            total_routes += 1;
            let route = outcome.route();
            let leg1_dex = route.leg1().dex().to_string();
            let leg1_pool = route.leg1().pool_address().to_string();
            let leg2_dex = route.leg2().dex().to_string();
            let leg2_pool = route.leg2().pool_address().to_string();
            let start_token = route.starting_token().to_string();
            let intermediate_token = route.intermediate_token().to_string();
            let cost_field = args
                .estimated_cost_raw
                .map(|v| v.to_string())
                .unwrap_or_default();

            if let Some(opportunity) = outcome.opportunity() {
                successful_routes += 1;
                let gross = opportunity.gross_profit();
                let gross_field = gross.to_string();
                let intermediate_amount = opportunity.intermediate_amount().to_string();
                let final_amount = opportunity.final_amount().to_string();
                let (net_field, net_status, metric) = match cost_signed {
                    Some(cost) => match checked_estimated_net_profit(gross, cost) {
                        Some(net) => (net.to_string(), "CALCULATED_FROM_OPERATOR_COST", Some(net)),
                        None => (String::new(), "ARITHMETIC_OVERFLOW", None),
                    },
                    None => (String::new(), "NOT_PROVIDED", Some(gross)),
                };
                if let Some(metric) = metric {
                    update_best(&mut best, metric, *size, route_index + 1);
                }
                report.push_str(&format!(
                    "[route {}] status=OK start_token={} intermediate_token={} leg1={} pool={} leg2={} pool={} intermediate_amount_raw={} final_amount_raw={} gross_profit_raw={} estimated_cost_raw={} estimated_net_profit_raw={} net_profit_status={}\n",
                    route_index + 1,
                    start_token,
                    intermediate_token,
                    leg1_dex,
                    leg1_pool,
                    leg2_dex,
                    leg2_pool,
                    intermediate_amount,
                    final_amount,
                    gross_field,
                    cost_field,
                    net_field,
                    net_status
                ));
                append_csv_row(
                    &mut csv,
                    &vec![
                        args.block.to_string(),
                        args.token_in.to_string(),
                        size.to_string(),
                        (route_index + 1).to_string(),
                        start_token,
                        intermediate_token,
                        leg1_dex,
                        leg1_pool,
                        leg2_dex,
                        leg2_pool,
                        "OK".into(),
                        intermediate_amount,
                        final_amount,
                        gross_field,
                        cost_field,
                        net_field,
                        net_status.into(),
                        String::new(),
                        String::new(),
                    ],
                );
            } else if let Some(failure) = outcome.failure() {
                failed_routes += 1;
                let (failure_kind, failure_detail) = failure_details(failure);
                report.push_str(&format!(
                    "[route {}] status=FAILED kind={} start_token={} intermediate_token={} leg1={} pool={} leg2={} pool={} error={}\n",
                    route_index + 1,
                    failure_kind,
                    start_token,
                    intermediate_token,
                    leg1_dex,
                    leg1_pool,
                    leg2_dex,
                    leg2_pool,
                    failure_detail
                ));
                append_csv_row(
                    &mut csv,
                    &vec![
                        args.block.to_string(),
                        args.token_in.to_string(),
                        size.to_string(),
                        (route_index + 1).to_string(),
                        start_token,
                        intermediate_token,
                        leg1_dex,
                        leg1_pool,
                        leg2_dex,
                        leg2_pool,
                        "FAILED".into(),
                        String::new(),
                        String::new(),
                        String::new(),
                        cost_field,
                        String::new(),
                        "NOT_AVAILABLE".into(),
                        failure_kind.into(),
                        failure_detail,
                    ],
                );
            }
        }
    }

    report.push_str(&format!(
        "total_routes_evaluated={total_routes}\nsuccessful_route_quotes={successful_routes}\nfailed_route_quotes={failed_routes}\n"
    ));
    match best {
        Some(best) => report.push_str(&format!(
            "best_observed_size_raw={}\nbest_observed_route_index={}\nbest_observed_metric_raw={}\nbest_metric_basis={}\n",
            best.size,
            best.route_index,
            best.metric,
            if args.estimated_cost_raw.is_some() { "estimated_net_profit" } else { "gross_profit" }
        )),
        None => report.push_str("best_observed_candidate=NONE\n"),
    }
    report.push_str(
        "\nNOTES\n- All pool states were loaded once at the same requested historical block.\n- The sweep tests only the discrete sizes supplied by the operator; it does not prove a continuous or global optimum.\n- estimated_cost_raw is a caller-provided constant applied equally to all routes and sizes; it is not a gas estimate produced by this engine.\n- No gas oracle, ETH/token conversion, token/USD pricing, slippage tolerance, settlement cost model, or transaction simulation is included.\n- Quote failures are retained; they are not treated as zero profit.\n- Historical quotes are not live trading signals. No private key is read and no transaction is signed or submitted.\n",
    );

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.csv_out)
        .map_err(|error| {
            EngineError::Other(anyhow::anyhow!(
                "could not create CSV output {}: {error}",
                args.csv_out.display()
            ))
        })?;
    file.write_all(csv.as_bytes()).map_err(|error| {
        EngineError::Other(anyhow::anyhow!(
            "could not write CSV output {}: {error}",
            args.csv_out.display()
        ))
    })?;

    println!("{report}");
    println!("CSV written: {}", args.csv_out.display());

    if detector_failed {
        return Err(EngineError::State(
            "one or more size evaluations failed at detector level; see report and CSV".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const AERO: &str = "0xcDAC0d6c6C59727a65F871236188350531885C43";
    const V3: &str = "0xd0b53D9277642d899DF5C87A3966A349A798F224";
    const WETH: &str = "0x4200000000000000000000000000000000000006";

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn valid_args() -> Vec<String> {
        args(&[
            "--aerodrome-pool",
            AERO,
            "--uniswap-v3-pool",
            V3,
            "--block",
            "26000000",
            "--token-in",
            WETH,
            "--size-in",
            "100000000000000000",
            "--size-in",
            "500000000000000000",
            "--csv-out",
            "m9-test.csv",
        ])
    }

    #[test]
    fn help_short_circuits_even_with_invalid_arguments() {
        assert_eq!(
            parse_m9_size_sweep_args(&args(&["--bogus", "x", "--help"])),
            Ok(M9SizeSweepCommand::Help)
        );
        assert_eq!(
            parse_m9_size_sweep_args(&args(&["-h"])),
            Ok(M9SizeSweepCommand::Help)
        );
    }

    #[test]
    fn parses_repeatable_pools_and_sizes() {
        let parsed = parse_m9_size_sweep_args(&valid_args()).unwrap();
        match parsed {
            M9SizeSweepCommand::Run(parsed) => {
                assert_eq!(
                    parsed.aerodrome_pools,
                    vec![AERO.parse::<Address>().unwrap()]
                );
                assert_eq!(
                    parsed.uniswap_v3_pools,
                    vec![V3.parse::<Address>().unwrap()]
                );
                assert_eq!(parsed.block, 26_000_000);
                assert_eq!(parsed.token_in, WETH.parse::<Address>().unwrap());
                assert_eq!(parsed.sizes.len(), 2);
                assert_eq!(parsed.word_radius, DEFAULT_WORD_RADIUS);
                assert_eq!(parsed.csv_out, PathBuf::from("m9-test.csv"));
                assert_eq!(parsed.estimated_cost_raw, None);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parses_optional_operator_supplied_cost_in_start_token_units() {
        let mut values = valid_args();
        values.extend(args(&["--estimated-cost-raw", "1234"]));
        match parse_m9_size_sweep_args(&values).unwrap() {
            M9SizeSweepCommand::Run(parsed) => {
                assert_eq!(parsed.estimated_cost_raw, Some(U256::from(1234)));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn rejects_missing_pool_or_size_inputs() {
        assert!(parse_m9_size_sweep_args(&[])
            .unwrap_err()
            .contains("--aerodrome-pool"));
        let mut values = valid_args();
        let last_size_flag = values
            .iter()
            .rposition(|value| value == "--size-in")
            .unwrap();
        values.drain(last_size_flag..last_size_flag + 2);
        assert!(parse_m9_size_sweep_args(&values)
            .unwrap_err()
            .contains("two distinct"));
    }

    #[test]
    fn rejects_duplicate_pool_addresses_across_dexes() {
        let values = args(&[
            "--aerodrome-pool",
            AERO,
            "--uniswap-v3-pool",
            AERO,
            "--block",
            "1",
            "--token-in",
            WETH,
            "--size-in",
            "1",
            "--size-in",
            "2",
            "--csv-out",
            "m9-test.csv",
        ]);
        assert!(parse_m9_size_sweep_args(&values)
            .unwrap_err()
            .contains("duplicate pool"));
    }

    #[test]
    fn rejects_duplicate_or_zero_sizes() {
        let mut duplicate = valid_args();
        duplicate.extend(args(&["--size-in", "100000000000000000"]));
        assert!(parse_m9_size_sweep_args(&duplicate)
            .unwrap_err()
            .contains("duplicate --size-in"));

        let mut zero = valid_args();
        zero.extend(args(&["--size-in", "0"]));
        assert!(parse_m9_size_sweep_args(&zero)
            .unwrap_err()
            .contains("greater than zero"));
    }

    #[test]
    fn rejects_bad_cost_and_cost_that_does_not_fit_signed_math() {
        let mut bad = valid_args();
        bad.extend(args(&["--estimated-cost-raw", "1.5"]));
        assert!(parse_m9_size_sweep_args(&bad)
            .unwrap_err()
            .contains("base-10 integer"));

        let mut too_large = valid_args();
        too_large.extend(args(&["--estimated-cost-raw", &U256::MAX.to_string()]));
        assert!(parse_m9_size_sweep_args(&too_large)
            .unwrap_err()
            .contains("too large"));
    }

    #[test]
    fn rejects_negative_word_radius_and_missing_csv_path() {
        let mut negative = valid_args();
        negative.extend(args(&["--word-radius", "-1"]));
        assert!(parse_m9_size_sweep_args(&negative)
            .unwrap_err()
            .contains("non-negative"));

        let mut no_csv = valid_args();
        no_csv.retain(|value| value != "--csv-out" && value != "m9-test.csv");
        assert!(parse_m9_size_sweep_args(&no_csv)
            .unwrap_err()
            .contains("--csv-out"));
    }

    #[test]
    fn estimated_net_profit_subtracts_operator_cost_without_unsigned_underflow() {
        assert_eq!(
            checked_estimated_net_profit(
                I256::try_from(20i64).unwrap(),
                I256::try_from(7i64).unwrap()
            ),
            Some(I256::try_from(13i64).unwrap())
        );
        assert_eq!(
            checked_estimated_net_profit(
                I256::try_from(-2i64).unwrap(),
                I256::try_from(7i64).unwrap()
            ),
            Some(I256::try_from(-9i64).unwrap())
        );
    }

    #[test]
    fn csv_escaping_quotes_commas_and_newlines() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_escape("a\nb"), "\"a\nb\"");
    }
}
