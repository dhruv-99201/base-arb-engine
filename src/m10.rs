//! M10: multi-block historical validation extending the M9 single-block sweep.
//!
//! Each requested block is independently pinned and loaded. Pool load results,
//! route successes, quote failures, and detector failures are all retained in
//! one CSV. No pool state is carried forward between blocks.
//!
//! This is a historical sample study, not a stateful trading backtest: it does
//! not model capital, compounding, inventory, transaction ordering, gas, or
//! execution. It is read-only and never signs or submits transactions.

use crate::arbitrage::{detect_cross_dex_opportunities_at_block, RouteEvaluationFailure};
use crate::cli::{build_scan_candidates, PinnedPoolRead};
use crate::config::Config;
use crate::dex::{AerodromeAdapter, UniswapV3Adapter};
use crate::error::{EngineError, EngineResult};
use crate::m9::{self, M9SizeSweepArgs, M9SizeSweepCommand};
use crate::market::models::{DexKind, Pool, PoolKind, Token};
use alloy::primitives::{Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::time::Duration;

const MAX_BLOCKS: usize = 100;
const CHAIN_ID_TIMEOUT_SECS: u64 = 15;
const POOL_READ_TIMEOUT_SECS: u64 = 300;

// M10_PROGRESS_TIMEOUT_PATCH: bound each whole pool read and report useful progress.
async fn with_rpc_timeout<T, F>(label: String, duration: Duration, future: F) -> EngineResult<T>
where
    F: Future<Output = EngineResult<T>>,
{
    tokio::time::timeout(duration, future).await.map_err(|_| {
        EngineError::Chain(format!(
            "{label} timed out after {} seconds",
            duration.as_secs()
        ))
    })?
}

fn flush_csv_buffer(
    file: &mut std::fs::File,
    csv: &mut String,
    output_path: &std::path::Path,
) -> EngineResult<()> {
    if csv.is_empty() {
        return Ok(());
    }
    file.write_all(csv.as_bytes())
        .and_then(|_| file.flush())
        .map_err(|error| {
            EngineError::Other(anyhow::anyhow!(
                "could not write M10 CSV output {}: {error}",
                output_path.display()
            ))
        })?;
    csv.clear();
    Ok(())
}
pub const M10_HISTORY_USAGE: &str = "\
Usage: cargo run -- scan-arb-history --aerodrome-pool <ADDRESS> [repeat] --uniswap-v3-pool <ADDRESS> [repeat] --block <U64> --block <U64> [more blocks] --token-in <ADDRESS> --size-in <RAW_INTEGER> --size-in <RAW_INTEGER> [more sizes] --csv-out <PATH> [--estimated-cost-raw <RAW_INTEGER>] [--word-radius <I32>]

M10 read-only multi-block historical validation. Loads the supplied Aerodrome
Classic and Uniswap V3 pools independently at EVERY requested block, then
runs the M9 discrete size sweep against each block's own pinned pool state.
Pool-load results, route outcomes, losses, and quote failures are written to
one CSV. No pool state is carried forward from one block to another.

Supply 2..=100 distinct --block values and 2..=50 distinct --size-in values.
Blocks are processed sequentially in the order supplied. All size and cost
values are base-10 integers in the starting token's smallest units. No
floating-point pricing is used.

The optional --estimated-cost-raw value is one operator-supplied constant in
the starting token's smallest units, applied equally to every route, size,
and block. It is NOT an engine gas estimate. No gas oracle, ETH/token
conversion, token/USD pricing, settlement model, or transaction simulation
is included.

This is NOT a stateful trading backtest: it does not model capital, inventory,
compounding, or execution between blocks. It is a read-only historical sample
comparison, not a live trading signal. No transaction is signed or submitted.
V3 tick hydration remains limited by --word-radius at each block.

Options:
  --aerodrome-pool <ADDRESS>   Aerodrome Classic volatile pool; repeatable.
  --uniswap-v3-pool <ADDRESS>  Uniswap V3 pool; repeatable.
  --block <U64>                Historical block; repeat 2..=100 distinct values.
  --token-in <ADDRESS>         Starting token address; required.
  --size-in <RAW_INTEGER>      Positive starting-token raw units; repeat 2..=50.
  --estimated-cost-raw <N>     Optional operator-supplied cost per route/sample.
  --csv-out <PATH>             New merged CSV path; must not already exist.
  --word-radius <I32>          V3 tick-bitmap word radius (default: 1).
  --help, -h                   Show this help and exit.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M10HistoryArgs {
    pub blocks: Vec<u64>,
    pub scan: M9SizeSweepArgs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum M10HistoryCommand {
    Help,
    Run(M10HistoryArgs),
}

/// Parse repeated block flags, then delegate shared pool/size/cost validation
/// to the already-tested M9 parser before configuration loading or RPC access.
pub fn parse_m10_history_args(args: &[String]) -> Result<M10HistoryCommand, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(M10HistoryCommand::Help);
    }

    let mut blocks = Vec::new();
    let mut common_args = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--block" {
            let raw = args
                .get(index + 1)
                .ok_or_else(|| "--block requires a value".to_string())?;
            let block = raw
                .parse::<u64>()
                .map_err(|_| format!("invalid --block value: '{raw}' (expected U64)"))?;
            if blocks.contains(&block) {
                return Err(format!("duplicate --block value: {block}"));
            }
            blocks.push(block);
            index += 2;
        } else {
            common_args.push(args[index].clone());
            index += 1;
        }
    }

    if blocks.len() < 2 {
        return Err(
            "at least two distinct --block values are required for multi-block validation".into(),
        );
    }
    if blocks.len() > MAX_BLOCKS {
        return Err(format!(
            "at most {MAX_BLOCKS} --block values are allowed per run"
        ));
    }

    // M9 owns parsing of all shared flags. Its parsed block is replaced for
    // each M10 iteration below.
    common_args.push("--block".into());
    common_args.push(blocks[0].to_string());
    let scan = match m9::parse_m9_size_sweep_args(&common_args)? {
        M9SizeSweepCommand::Run(scan) => scan,
        M9SizeSweepCommand::Help => return Ok(M10HistoryCommand::Help),
    };

    Ok(M10HistoryCommand::Run(M10HistoryArgs { blocks, scan }))
}

fn gross_profitable_block_hit_rate_pct(
    evaluated_blocks: &BTreeSet<u64>,
    profitable_blocks: &BTreeSet<u64>,
) -> Option<f64> {
    if evaluated_blocks.is_empty() {
        return None;
    }

    let profitable_evaluated_blocks = evaluated_blocks.intersection(profitable_blocks).count();

    Some(profitable_evaluated_blocks as f64 / evaluated_blocks.len() as f64 * 100.0)
}

#[derive(Debug, Default)]
struct Aggregate {
    pools_requested: usize,
    pools_loaded: usize,
    pools_failed: usize,
    route_rows: usize,
    successful_quotes: usize,
    failed_quotes: usize,
    detector_failure_rows: usize,
    no_route_samples: usize,
    gross_profitable_quotes: usize,
    net_profitable_quotes: usize,
    gross_profitable_blocks: BTreeSet<u64>,
    blocks_with_successful_quotes: BTreeSet<u64>,
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

fn checked_estimated_net_profit(gross_profit: I256, estimated_cost: I256) -> Option<I256> {
    gross_profit.checked_sub(estimated_cost)
}

fn pool_csv_row(
    block: u64,
    token_in: Address,
    dex: DexKind,
    pool: Address,
    status: &str,
    token_pair: Option<(&str, &str)>,
    error: Option<&str>,
) -> Vec<String> {
    let pool_metadata = token_pair
        .map(|(token0, token1)| format!("token0={token0};token1={token1}"))
        .unwrap_or_default();
    let (failure_kind, failure_detail) = match error {
        Some(detail) => ("PoolLoadError", detail.to_string()),
        None => ("", pool_metadata),
    };
    vec![
        block.to_string(),
        token_in.to_string(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        dex.to_string(),
        pool.to_string(),
        String::new(),
        String::new(),
        status.to_string(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        "NOT_APPLICABLE".into(),
        failure_kind.to_string(),
        failure_detail,
    ]
}

/// Run the M9 discrete size sweep independently at every supplied block.
/// Each block's pool state is fetched once, pinned to that block, and reused
/// for every size in that block. Pool and route failures are retained in CSV.
pub async fn run_m10_history(config: &Config, args: M10HistoryArgs) -> EngineResult<()> {
    if args.scan.csv_out.exists() {
        return Err(EngineError::Config(format!(
            "CSV output already exists: {}; choose a new --csv-out path",
            args.scan.csv_out.display()
        )));
    }

    let url = config
        .base_rpc_url
        .parse()
        .map_err(|error| EngineError::Config(format!("invalid BASE_RPC_URL: {error}")))?;
    let provider = ProviderBuilder::new().connect_http(url);
    let chain_id = with_rpc_timeout(
        "get_chain_id".into(),
        Duration::from_secs(CHAIN_ID_TIMEOUT_SECS),
        async {
            provider
                .get_chain_id()
                .await
                .map_err(|error| EngineError::Chain(format!("get_chain_id failed: {error}")))
        },
    )
    .await?;
    if chain_id != config.base_chain_id {
        return Err(EngineError::Chain(format!(
            "chain ID mismatch: BASE_CHAIN_ID is {}, RPC endpoint reports {chain_id}",
            config.base_chain_id
        )));
    }

    let aerodrome_adapter =
        AerodromeAdapter::with_factory_address(config.aerodrome_factory_address);
    let uniswap_v3_adapter = UniswapV3Adapter::new();
    let cost_signed = args
        .scan
        .estimated_cost_raw
        .map(I256::try_from)
        .transpose()
        .map_err(|_| EngineError::Arithmetic("estimated cost does not fit I256".into()))?;

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.scan.csv_out)
        .map_err(|error| {
            EngineError::Other(anyhow::anyhow!(
                "could not create M10 CSV output {}: {error}",
                args.scan.csv_out.display()
            ))
        })?;
    file.write_all(b"block,token_in,size_in_raw,route_index,start_token,intermediate_token,leg1_dex,leg1_pool,leg2_dex,leg2_pool,status,intermediate_amount_raw,final_amount_raw,gross_profit_raw,estimated_cost_raw,estimated_net_profit_raw,net_profit_status,failure_kind,failure_detail\n")
        .and_then(|_| file.flush())
        .map_err(|error| {
            EngineError::Other(anyhow::anyhow!(
                "could not write M10 CSV header {}: {error}",
                args.scan.csv_out.display()
            ))
        })?;
    let mut csv = String::new();
    let mut report = String::new();
    let mut aggregate = Aggregate::default();
    let mut detector_failed = false;

    report.push_str("M10 multi-block historical validation\n");
    report.push_str(&format!(
        "chain_id={chain_id}\nblocks_requested={}\nsizes_per_block={}\nblock_size_points={}\nv3_word_radius={}\n",
        args.blocks.len(),
        args.scan.sizes.len(),
        args.blocks.len() * args.scan.sizes.len(),
        args.scan.word_radius
    ));
    match args.scan.estimated_cost_raw {
        Some(cost) => report.push_str(&format!(
            "estimated_cost_raw={cost}\ncost_model=operator_supplied_constant_applied_to_every_route_size_and_block\n"
        )),
        None => report.push_str(
            "estimated_cost_raw=NOT_PROVIDED\ncost_model=none; only gross profit is reported\n",
        ),
    }
    println!(
        "[progress] M10 started: blocks={}, pools_per_block={}, sizes_per_block={}, pool_timeout={}s, csv={}",
        args.blocks.len(),
        args.scan.aerodrome_pools.len() + args.scan.uniswap_v3_pools.len(),
        args.scan.sizes.len(),
        POOL_READ_TIMEOUT_SECS,
        args.scan.csv_out.display()
    );

    for (block_index, block) in args.blocks.iter().copied().enumerate() {
        println!(
            "\n[progress] block {}/{} starting: {block}",
            block_index + 1,
            args.blocks.len()
        );
        report.push_str(&format!("\n[block {}] number={block}\n", block_index + 1));
        report.push_str(&format!("\n[block {}] number={block}\n", block_index + 1));
        let mut reads: Vec<(DexKind, Address, EngineResult<PinnedPoolRead>)> =
            Vec::with_capacity(args.scan.aerodrome_pools.len() + args.scan.uniswap_v3_pools.len());

        for address in &args.scan.aerodrome_pools {
            let label = format!("Aerodrome pool {address} at block {block}");
            println!("[progress] loading {label}");
            let read = with_rpc_timeout(
                label.clone(),
                Duration::from_secs(POOL_READ_TIMEOUT_SECS),
                aerodrome_adapter.get_pool_state_at_block(
                    &config.base_rpc_url,
                    &scan_placeholder_aerodrome_pool(*address),
                    block,
                ),
            )
            .await
            .map(PinnedPoolRead::Aerodrome);
            println!(
                "[progress] {label}: {}",
                if read.is_ok() { "loaded" } else { "failed" }
            );
            reads.push((DexKind::Aerodrome, *address, read));
        }
        for address in &args.scan.uniswap_v3_pools {
            let label = format!("Uniswap V3 pool {address} at block {block}");
            println!(
                "[progress] loading {label} (tick radius {})",
                args.scan.word_radius
            );
            let read = with_rpc_timeout(
                label.clone(),
                Duration::from_secs(POOL_READ_TIMEOUT_SECS),
                uniswap_v3_adapter.get_pool_state_and_ticks_at_block(
                    &config.base_rpc_url,
                    &scan_placeholder_v3_pool(*address),
                    args.scan.word_radius,
                    Some(block),
                ),
            )
            .await
            .map(|(state, ticks)| PinnedPoolRead::UniswapV3(state, ticks));
            println!(
                "[progress] {label}: {}",
                if read.is_ok() { "loaded" } else { "failed" }
            );
            reads.push((DexKind::UniswapV3, *address, read));
        }

        let (loads, candidates) = build_scan_candidates(block, reads);
        let requested = loads.len();
        let loaded = loads.iter().filter(|load| load.result.is_ok()).count();
        let failed = requested - loaded;
        aggregate.pools_requested += requested;
        aggregate.pools_loaded += loaded;
        aggregate.pools_failed += failed;
        report.push_str(&format!(
            "pools_requested={requested}\npools_loaded={loaded}\npools_failed={failed}\n"
        ));

        for load in &loads {
            match &load.result {
                Ok((token0, token1)) => {
                    let token0 = token0.to_string();
                    let token1 = token1.to_string();
                    report.push_str(&format!(
                        "[pool] dex={} address={} status=LOADED token0={} token1={}\n",
                        load.dex, load.address, token0, token1
                    ));
                    append_csv_row(
                        &mut csv,
                        &pool_csv_row(
                            block,
                            args.scan.token_in,
                            load.dex,
                            load.address,
                            "POOL_LOADED",
                            Some((token0.as_str(), token1.as_str())),
                            None,
                        ),
                    );
                }
                Err(error) => {
                    let error_detail = error.to_string();
                    report.push_str(&format!(
                        "[pool] dex={} address={} status=FAILED error={}\n",
                        load.dex, load.address, error_detail
                    ));
                    append_csv_row(
                        &mut csv,
                        &pool_csv_row(
                            block,
                            args.scan.token_in,
                            load.dex,
                            load.address,
                            "POOL_LOAD_FAILED",
                            None,
                            Some(&error_detail),
                        ),
                    );
                }
            }
        }
        flush_csv_buffer(&mut file, &mut csv, &args.scan.csv_out)?;
        println!(
            "[progress] block {}/{} pool reads finished: loaded={}, failed={}",
            block_index + 1,
            args.blocks.len(),
            loaded,
            failed
        );

        for (size_index, size) in args.scan.sizes.iter().enumerate() {
            report.push_str(&format!(
                "\n[size {}] amount_in_raw={}\n",
                size_index + 1,
                size
            ));
            let outcomes = match detect_cross_dex_opportunities_at_block(
                block,
                args.scan.token_in,
                *size,
                &candidates,
            ) {
                Ok(outcomes) => outcomes,
                Err(error) => {
                    detector_failed = true;
                    aggregate.detector_failure_rows += 1;
                    report.push_str(&format!("route_detection=FAILED error={error}\n"));
                    append_csv_row(
                        &mut csv,
                        &vec![
                            block.to_string(),
                            args.scan.token_in.to_string(),
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
                            args.scan
                                .estimated_cost_raw
                                .map(|value| value.to_string())
                                .unwrap_or_default(),
                            String::new(),
                            "NOT_AVAILABLE".into(),
                            "DetectorError".into(),
                            error.to_string(),
                        ],
                    );
                    flush_csv_buffer(&mut file, &mut csv, &args.scan.csv_out)?;
                    println!(
                        "[progress] block {}/{} size {}/{} detector failed; failure row saved",
                        block_index + 1,
                        args.blocks.len(),
                        size_index + 1,
                        args.scan.sizes.len()
                    );
                    continue;
                }
            };

            report.push_str(&format!("routes_evaluated={}\n", outcomes.len()));
            if outcomes.is_empty() {
                aggregate.no_route_samples += 1;
                report.push_str("no route matched token_in for this size\n");
                append_csv_row(
                    &mut csv,
                    &vec![
                        block.to_string(),
                        args.scan.token_in.to_string(),
                        size.to_string(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        String::new(),
                        "NO_ROUTE".into(),
                        String::new(),
                        String::new(),
                        String::new(),
                        args.scan
                            .estimated_cost_raw
                            .map(|value| value.to_string())
                            .unwrap_or_default(),
                        String::new(),
                        "NOT_APPLICABLE".into(),
                        String::new(),
                        String::new(),
                    ],
                );
            }

            for (route_index, outcome) in outcomes.iter().enumerate() {
                aggregate.route_rows += 1;
                let route = outcome.route();
                let leg1_dex = route.leg1().dex().to_string();
                let leg1_pool = route.leg1().pool_address().to_string();
                let leg2_dex = route.leg2().dex().to_string();
                let leg2_pool = route.leg2().pool_address().to_string();
                let start_token = route.starting_token().to_string();
                let intermediate_token = route.intermediate_token().to_string();
                let cost_field = args
                    .scan
                    .estimated_cost_raw
                    .map(|value| value.to_string())
                    .unwrap_or_default();

                if let Some(opportunity) = outcome.opportunity() {
                    aggregate.successful_quotes += 1;
                    aggregate.blocks_with_successful_quotes.insert(block);
                    let gross = opportunity.gross_profit();
                    let gross_field = gross.to_string();
                    let intermediate_amount = opportunity.intermediate_amount().to_string();
                    let final_amount = opportunity.final_amount().to_string();
                    if gross > I256::ZERO {
                        aggregate.gross_profitable_quotes += 1;
                        aggregate.gross_profitable_blocks.insert(block);
                    }
                    let (net_field, net_status) = match cost_signed {
                        Some(cost) => match checked_estimated_net_profit(gross, cost) {
                            Some(net) => {
                                if net > I256::ZERO {
                                    aggregate.net_profitable_quotes += 1;
                                }
                                (net.to_string(), "CALCULATED_FROM_OPERATOR_COST")
                            }
                            None => (String::new(), "ARITHMETIC_OVERFLOW"),
                        },
                        None => (String::new(), "NOT_PROVIDED"),
                    };
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
                            block.to_string(),
                            args.scan.token_in.to_string(),
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
                    aggregate.failed_quotes += 1;
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
                            block.to_string(),
                            args.scan.token_in.to_string(),
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
            flush_csv_buffer(&mut file, &mut csv, &args.scan.csv_out)?;
            println!(
                "[progress] block {}/{} size {}/{} finished; routes={}",
                block_index + 1,
                args.blocks.len(),
                size_index + 1,
                args.scan.sizes.len(),
                outcomes.len()
            );
        }
        println!(
            "[progress] block {}/{} complete: {block}",
            block_index + 1,
            args.blocks.len()
        );
    }
    file.flush().map_err(|error| {
        EngineError::Other(anyhow::anyhow!(
            "could not flush M10 CSV output {}: {error}",
            args.scan.csv_out.display()
        ))
    })?;

    report.push_str(&format!(
        "\nblocks_scanned={}\npools_requested={}\npools_loaded={}\npools_failed={}\nroute_rows={}\nsuccessful_route_quotes={}\nfailed_route_quotes={}\nno_route_samples={}\ndetector_failure_rows={}\ngross_profitable_quotes={}\nblocks_with_gross_profitable_quote={}\n",
        args.blocks.len(),
        aggregate.pools_requested,
        aggregate.pools_loaded,
        aggregate.pools_failed,
        aggregate.route_rows,
        aggregate.successful_quotes,
        aggregate.failed_quotes,
        aggregate.no_route_samples,
        aggregate.detector_failure_rows,
        aggregate.gross_profitable_quotes,
        aggregate.gross_profitable_blocks.len()
    ));
    report.push_str(&format!(
        "blocks_with_successful_route_quotes={}\n",
        aggregate.blocks_with_successful_quotes.len()
    ));

    match gross_profitable_block_hit_rate_pct(
        &aggregate.blocks_with_successful_quotes,
        &aggregate.gross_profitable_blocks,
    ) {
        Some(hit_rate) => report.push_str(&format!(
            "gross_profitable_block_hit_rate_pct={hit_rate:.4}\n"
        )),
        None => report.push_str("gross_profitable_block_hit_rate_pct=NOT_AVAILABLE\n"),
    }
    if args.scan.estimated_cost_raw.is_some() {
        report.push_str(&format!(
            "net_profitable_quotes={}\n",
            aggregate.net_profitable_quotes
        ));
        report.push_str("net_profit_basis=operator_supplied_constant_cost\n");
    } else {
        report
            .push_str("net_profitable_quotes=NOT_AVAILABLE\nnet_profit_basis=cost_not_provided\n");
    }
    report.push_str(&format!("csv_written={}\n", args.scan.csv_out.display()));
    report.push_str(
        "\nNOTES\n- Each block is an independent historical pool snapshot; no state is carried between blocks.\n- The sweep tests only the operator-supplied discrete sizes; it does not establish a continuous optimum.\n- The optional cost is a caller-provided constant, not an engine gas estimate.\n- No token/USD conversion, gas oracle, transaction simulation, signing, or submission is included.\n- Pool-load, route-quote, and detector failures are retained in the CSV.\n- Historical quotes are not live trading signals.\n",
    );
    println!("{report}");

    if detector_failed {
        return Err(EngineError::State(
            "one or more block/size detector evaluations failed; see M10 report and CSV".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    fn valid_args() -> Vec<String> {
        strings(&[
            "--aerodrome-pool",
            "0xcDAC0d6c6C59727a65F871236188350531885C43",
            "--uniswap-v3-pool",
            "0xd0b53D9277642d899DF5C87A3966A349A798F224",
            "--block",
            "26000000",
            "--block",
            "26000001",
            "--token-in",
            "0x4200000000000000000000000000000000000006",
            "--size-in",
            "100000000000000000",
            "--size-in",
            "500000000000000000",
            "--csv-out",
            "m10-test.csv",
        ])
    }

    #[test]
    fn parser_accepts_multiple_blocks_and_delegates_common_options_to_m9() {
        match parse_m10_history_args(&valid_args()).unwrap() {
            M10HistoryCommand::Run(parsed) => {
                assert_eq!(parsed.blocks, vec![26_000_000, 26_000_001]);
                assert_eq!(parsed.scan.block, 26_000_000);
                assert_eq!(parsed.scan.sizes.len(), 2);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn parser_rejects_missing_duplicate_or_excessive_blocks() {
        let mut one = valid_args();
        let second = one.iter().position(|value| value == "--block").unwrap() + 2;
        one.drain(second..second + 2);
        assert!(parse_m10_history_args(&one)
            .unwrap_err()
            .contains("at least two"));

        let mut duplicate = valid_args();
        duplicate.extend(strings(&["--block", "26000000"]));
        assert!(parse_m10_history_args(&duplicate)
            .unwrap_err()
            .contains("duplicate --block"));

        let mut too_many = valid_args();
        for block in 27_000_000..27_000_000 + (MAX_BLOCKS as u64) {
            too_many.extend(strings(&["--block", &block.to_string()]));
        }
        assert!(parse_m10_history_args(&too_many)
            .unwrap_err()
            .contains("at most 100"));
    }

    #[test]
    fn parser_help_short_circuits_even_with_invalid_flags() {
        assert_eq!(
            parse_m10_history_args(&strings(&["--unknown", "bad", "--help"])),
            Ok(M10HistoryCommand::Help)
        );
    }

    #[test]
    fn block_hit_rate_is_unavailable_without_successful_quotes() {
        let evaluated = BTreeSet::new();
        let profitable = BTreeSet::new();

        assert_eq!(
            gross_profitable_block_hit_rate_pct(&evaluated, &profitable),
            None
        );
    }

    #[test]
    fn block_hit_rate_uses_only_blocks_with_successful_quotes() {
        let evaluated = [26_000_000_u64, 26_200_000_u64]
            .into_iter()
            .collect::<BTreeSet<_>>();
        let profitable = [26_200_000_u64].into_iter().collect::<BTreeSet<_>>();

        assert_eq!(
            gross_profitable_block_hit_rate_pct(&evaluated, &profitable),
            Some(50.0)
        );
    }

    #[test]
    fn block_hit_rate_is_zero_when_valid_blocks_have_no_wins() {
        let evaluated = [26_000_000_u64, 26_100_000_u64]
            .into_iter()
            .collect::<BTreeSet<_>>();
        let profitable = BTreeSet::new();

        assert_eq!(
            gross_profitable_block_hit_rate_pct(&evaluated, &profitable),
            Some(0.0)
        );
    }
    #[tokio::test]
    async fn rpc_timeout_returns_a_clear_error() {
        let result = with_rpc_timeout(
            "test historical pool".into(),
            Duration::from_millis(1),
            std::future::pending::<EngineResult<()>>(),
        )
        .await;
        let error = result.unwrap_err().to_string();
        assert!(error.contains("timed out"));
        assert!(error.contains("test historical pool"));
    }

    #[test]
    fn csv_pool_failure_rows_preserve_error_details() {
        let mut csv = String::new();
        let row = pool_csv_row(
            26_000_000,
            "0x4200000000000000000000000000000000000006"
                .parse()
                .unwrap(),
            DexKind::Aerodrome,
            "0xcDAC0d6c6C59727a65F871236188350531885C43"
                .parse()
                .unwrap(),
            "POOL_LOAD_FAILED",
            None,
            Some("RPC error, reason: \"historical state unavailable\""),
        );
        append_csv_row(&mut csv, &row);
        assert!(csv.contains("\"RPC error, reason: \"\"historical state unavailable\"\"\""));
        assert_eq!(row.len(), 19);
    }
}
