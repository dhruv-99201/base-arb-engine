//! M10: multi-block historical validation extending the M9 single-block sweep.
//!
//! Each requested block is independently pinned and loaded. Pool load results,
//! route successes, quote failures, and detector failures are all retained in
//! one CSV. No pool state is carried forward between blocks.
//!
//! This is a historical sample study, not a stateful trading backtest: it does
//! not model capital, compounding, inventory, transaction ordering, gas, or
//! execution. It is read-only and never signs or submits transactions.

use crate::arbitrage::{
    detect_cross_dex_opportunities_at_block, HistoricalQuoteInput, HistoricalUniswapV3Input,
    RouteEvaluationFailure, RouteOutcome,
};
use crate::cli::{build_scan_candidates, PinnedPoolRead};
use crate::config::Config;
use crate::dex::{AerodromeAdapter, UniswapV3Adapter};
use crate::error::{EngineError, EngineResult};
use crate::m9::{self, M9SizeSweepArgs, M9SizeSweepCommand};
use crate::market::models::{DexKind, Pool, PoolKind, PoolState, Token};
use crate::pricing::v3_quote::HydratedTicks;
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

// M14: bounded, pool-specific hydration expansion. Only the typed error
// emitted when a swap crosses the verified hydrated boundary can trigger it.
//
// M14_RETRY_POLICY_HELPERS: classification and retry-budget decisions are
// small private pure functions, and the per-pool expansion attempt takes its
// block-pinned reader as a closure, so the policy is unit-testable without
// RPC access. The production reader still pins every read to `block`.
const MAX_TICK_COVERAGE_RETRIES_PER_POOL: u8 = 2;
const MAX_TICK_COVERAGE_RETRY_READS_PER_BLOCK: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailedQuoteLeg {
    Leg1,
    Leg2,
}

/// Which leg's quote failed and why. Only per-leg quote failures can be a
/// tick-coverage failure; mapping and opportunity-evaluation failures never
/// are, whatever error they wrap.
fn failed_quote_leg(failure: &RouteEvaluationFailure) -> Option<(FailedQuoteLeg, &EngineError)> {
    match failure {
        RouteEvaluationFailure::Leg1QuoteFailed { error } => Some((FailedQuoteLeg::Leg1, error)),
        RouteEvaluationFailure::Leg2QuoteFailed { error, .. } => {
            Some((FailedQuoteLeg::Leg2, error))
        }
        RouteEvaluationFailure::InvalidRouteCandidateMapping { .. }
        | RouteEvaluationFailure::OpportunityEvaluationFailed { .. } => None,
    }
}

/// Retry is allowed only for the typed coverage error on a Uniswap V3 leg.
/// The error is matched by type, never by message text.
fn is_tick_coverage_retryable(dex: DexKind, error: &EngineError) -> bool {
    dex == DexKind::UniswapV3 && matches!(error, EngineError::InsufficientV3TickCoverage { .. })
}

fn tick_coverage_retry_target(outcome: &RouteOutcome) -> Option<(usize, Address)> {
    let failure = outcome.failure()?;
    let (failed_leg, error) = failed_quote_leg(failure)?;
    let leg = match failed_leg {
        FailedQuoteLeg::Leg1 => outcome.route().leg1(),
        FailedQuoteLeg::Leg2 => outcome.route().leg2(),
    };

    if !is_tick_coverage_retryable(leg.dex(), error) {
        return None;
    }

    Some((leg.candidate_index(), leg.pool_address()))
}

/// The route leg's candidate must still be the Uniswap V3 pool the route
/// claims before it is eligible for replacement.
fn candidate_is_retry_target(candidate: &HistoricalQuoteInput, pool_address: Address) -> bool {
    let pool = &candidate.pool_state().pool;
    pool.dex == DexKind::UniswapV3 && pool.address == pool_address
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TickRetrySkipReason {
    PerBlockReadLimit,
    PerPoolRetryLimit,
    MissingInitialRadius,
    RadiusOverflow,
}

impl TickRetrySkipReason {
    fn report_reason(self) -> String {
        match self {
            Self::PerBlockReadLimit => {
                format!("per_block_retry_read_limit({MAX_TICK_COVERAGE_RETRY_READS_PER_BLOCK})")
            }
            Self::PerPoolRetryLimit => {
                format!("per_pool_retry_limit({MAX_TICK_COVERAGE_RETRIES_PER_POOL})")
            }
            Self::MissingInitialRadius => "missing_initial_radius".to_string(),
            Self::RadiusOverflow => "radius_overflow".to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TickRetryDecision {
    Expand {
        current_radius: i32,
        next_radius: i32,
    },
    Skip(TickRetrySkipReason),
}

/// Pure retry-budget decision. Checks run in a fixed order: per-block read
/// limit, per-pool retry limit, known initial radius, radius overflow.
fn decide_tick_retry(
    block_retry_reads: usize,
    pool_retry_count: u8,
    current_radius: Option<i32>,
) -> TickRetryDecision {
    if block_retry_reads >= MAX_TICK_COVERAGE_RETRY_READS_PER_BLOCK {
        return TickRetryDecision::Skip(TickRetrySkipReason::PerBlockReadLimit);
    }
    if pool_retry_count >= MAX_TICK_COVERAGE_RETRIES_PER_POOL {
        return TickRetryDecision::Skip(TickRetrySkipReason::PerPoolRetryLimit);
    }
    let Some(current_radius) = current_radius else {
        return TickRetryDecision::Skip(TickRetrySkipReason::MissingInitialRadius);
    };
    let Some(next_radius) = current_radius.checked_add(1) else {
        return TickRetryDecision::Skip(TickRetrySkipReason::RadiusOverflow);
    };
    TickRetryDecision::Expand {
        current_radius,
        next_radius,
    }
}

/// Every attempted read consumes budget up front, so a failed or rejected
/// expansion still counts against both limits.
fn consume_tick_retry_budget(block_retry_reads: &mut usize, pool_retry_count: &mut u8) {
    *block_retry_reads = block_retry_reads.saturating_add(1);
    *pool_retry_count = pool_retry_count.saturating_add(1);
}

struct TickCoverageRetryContext<'a> {
    rpc_url: &'a str,
    adapter: &'a UniswapV3Adapter,
    candidate_word_radii: &'a mut [Option<i32>],
    candidate_retry_counts: &'a mut [u8],
    block_retry_reads: &'a mut usize,
    report: &'a mut String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TickExpansionStep {
    Expanded,
    Skipped,
    Failed,
}

/// One bounded expansion attempt for one candidate. `read` receives the next
/// word radius and must return a state/tick pair read at `block`; it is not
/// called at all when the budget decision is to skip. The candidate and its
/// stored radius change only after the read passes historical-input
/// validation for `block`.
async fn attempt_tick_expansion<R, Fut>(
    block: u64,
    candidate_index: usize,
    pool_address: Address,
    candidates: &mut [HistoricalQuoteInput],
    retry: &mut TickCoverageRetryContext<'_>,
    read: R,
) -> TickExpansionStep
where
    R: FnOnce(i32) -> Fut,
    Fut: Future<Output = EngineResult<(PoolState, HydratedTicks)>>,
{
    let decision = decide_tick_retry(
        *retry.block_retry_reads,
        retry.candidate_retry_counts[candidate_index],
        retry.candidate_word_radii[candidate_index],
    );
    let (current_radius, next_radius) = match decision {
        TickRetryDecision::Skip(reason) => {
            retry.report.push_str(&format!(
                "[tick_retry] block={block} pool={pool_address} status=SKIPPED reason={}\n",
                reason.report_reason()
            ));
            return TickExpansionStep::Skipped;
        }
        TickRetryDecision::Expand {
            current_radius,
            next_radius,
        } => (current_radius, next_radius),
    };

    consume_tick_retry_budget(
        retry.block_retry_reads,
        &mut retry.candidate_retry_counts[candidate_index],
    );

    match read(next_radius).await {
        Ok((state, ticks)) => {
            if state.pool.address != pool_address {
                retry.report.push_str(&format!(
                    "[tick_retry] block={block} pool={pool_address} status=FAILED reason=returned_pool_address_mismatch actual={}\n",
                    state.pool.address
                ));
                return TickExpansionStep::Failed;
            }
            match HistoricalUniswapV3Input::from_pinned_read(block, (state, ticks)) {
                Ok(input) => {
                    candidates[candidate_index] = HistoricalQuoteInput::UniswapV3(input);
                    retry.candidate_word_radii[candidate_index] = Some(next_radius);
                    retry.report.push_str(&format!(
                        "[tick_retry] block={block} pool={pool_address} radius={current_radius}->{next_radius} status=EXPANDED reads_used={}\n",
                        *retry.block_retry_reads
                    ));
                    TickExpansionStep::Expanded
                }
                Err(error) => {
                    retry.report.push_str(&format!(
                        "[tick_retry] block={block} pool={pool_address} radius={next_radius} status=FAILED reason=historical_input_validation error={error}\n"
                    ));
                    TickExpansionStep::Failed
                }
            }
        }
        Err(error) => {
            retry.report.push_str(&format!(
                "[tick_retry] block={block} pool={pool_address} radius={next_radius} status=FAILED error={error}\n"
            ));
            TickExpansionStep::Failed
        }
    }
}

async fn detect_with_bounded_tick_expansion(
    block: u64,
    token_in: Address,
    amount_in: U256,
    candidates: &mut [HistoricalQuoteInput],
    retry: &mut TickCoverageRetryContext<'_>,
) -> EngineResult<Vec<RouteOutcome>> {
    let mut blocked_candidates = BTreeSet::new();

    loop {
        let outcomes =
            detect_cross_dex_opportunities_at_block(block, token_in, amount_in, candidates)?;

        let mut targets = BTreeSet::new();
        for outcome in &outcomes {
            let Some((candidate_index, pool_address)) = tick_coverage_retry_target(outcome) else {
                continue;
            };
            if blocked_candidates.contains(&candidate_index) {
                continue;
            }
            let candidate_matches = candidates
                .get(candidate_index)
                .is_some_and(|candidate| candidate_is_retry_target(candidate, pool_address));
            if candidate_matches {
                targets.insert((candidate_index, pool_address));
            }
        }

        if targets.is_empty() {
            return Ok(outcomes);
        }

        let mut expanded_any = false;
        for (candidate_index, pool_address) in targets {
            let rpc_url = retry.rpc_url;
            let adapter = retry.adapter;
            let step = attempt_tick_expansion(
                block,
                candidate_index,
                pool_address,
                candidates,
                retry,
                |next_radius| async move {
                    let label = format!(
                        "M10 V3 tick-coverage expansion for pool {pool_address} at block {block} (radius {next_radius})"
                    );
                    let placeholder = scan_placeholder_v3_pool(pool_address);
                    with_rpc_timeout(
                        label,
                        Duration::from_secs(POOL_READ_TIMEOUT_SECS),
                        adapter.get_pool_state_and_ticks_at_block(
                            rpc_url,
                            &placeholder,
                            next_radius,
                            Some(block),
                        ),
                    )
                    .await
                },
            )
            .await;

            match step {
                TickExpansionStep::Expanded => expanded_any = true,
                TickExpansionStep::Skipped | TickExpansionStep::Failed => {
                    blocked_candidates.insert(candidate_index);
                }
            }
        }

        if !expanded_any {
            return Ok(outcomes);
        }
    }
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
  --pool-catalog <PATH>       Use an M13 catalog instead of manual pool flags.
M10 read-only multi-block historical validation. Loads the supplied Aerodrome
Classic and Uniswap V3 pools independently at EVERY requested block, then
runs the M9 discrete size sweep against each block's own pinned pool state.
Pool-load results, route outcomes, losses, and quote failures are written to
one CSV. No pool state is carried forward from one block to another.
When --pool-catalog is used, manual --aerodrome-pool and --uniswap-v3-pool
flags must not be supplied. Only cross-DEX pairs containing --token-in and
created by the earliest requested block are selected.

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
V3 tick hydration starts at --word-radius. M10 may perform a bounded number
of additional, block-pinned reads for an affected Uniswap V3 pool when a swap
reaches the confirmed hydration boundary; retry limits are enforced per pool
and per block.

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

        let (loads, mut candidates) = build_scan_candidates(block, reads);
        let mut candidate_word_radii = candidates
            .iter()
            .map(|candidate| {
                if candidate.pool_state().pool.dex == DexKind::UniswapV3 {
                    Some(args.scan.word_radius)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        let mut candidate_retry_counts = vec![0u8; candidates.len()];
        let mut tick_coverage_retry_reads = 0usize;
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
            let outcomes = match detect_with_bounded_tick_expansion(
                block,
                args.scan.token_in,
                *size,
                &mut candidates,
                &mut TickCoverageRetryContext {
                    rpc_url: &config.base_rpc_url,
                    adapter: &uniswap_v3_adapter,
                    candidate_word_radii: &mut candidate_word_radii,
                    candidate_retry_counts: &mut candidate_retry_counts,
                    block_retry_reads: &mut tick_coverage_retry_reads,
                    report: &mut report,
                },
            )
            .await
            {
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
        report.push_str(&format!(
            "[block {block}] tick_coverage_retry_reads={}\n",
            tick_coverage_retry_reads
        ));
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

// M14_RETRY_POLICY_TESTS: deterministic regression tests for the bounded
// tick-coverage retry policy. No RPC: expansion reads are injected closures.
#[cfg(test)]
mod m14_retry_policy_tests {
    use super::*;
    use crate::arbitrage::opportunity::LegQuote;
    use crate::arbitrage::HistoricalAerodromeInput;
    use alloy::primitives::address;
    use std::cell::Cell;
    use std::collections::BTreeMap;

    const BLOCK: u64 = 26_000_000;
    const POOL: Address = address!("d0b53D9277642d899DF5C87A3966A349A798F224");
    const OTHER_POOL: Address = address!("0000000000000000000000000000000000000b02");
    const WETH: Address = address!("4200000000000000000000000000000000000006");
    const USDC: Address = address!("833589fCD6eDb6E08f4c7C32D4f71b54bdA02913");
    const INITIAL_HI: i32 = -192_010;
    const EXPANDED_HI: i32 = -184_330;

    fn coverage_error() -> EngineError {
        EngineError::InsufficientV3TickCoverage {
            required_tick: -190_000,
            hydrated_tick_lo: -199_680,
            hydrated_tick_hi: INITIAL_HI,
        }
    }

    fn v3_state(address: Address, block: u64, dex: DexKind) -> PoolState {
        let kind = if dex == DexKind::Aerodrome {
            PoolKind::Aerodrome {
                reserve0: U256::from(1_000_000u64),
                reserve1: U256::from(2_000_000u64),
                stable: false,
                fee_bps: Some(U256::from(30u64)),
            }
        } else {
            PoolKind::ConcentratedLiquidity {
                fee_tier: 500,
                tick_spacing: 10,
                sqrt_price_x96: U256::from_str_radix("4166855781027983759429743", 10).unwrap(),
                current_tick: -197_069,
                liquidity: 2_241_227_707_498_366_949u128,
                initialized_ticks: Default::default(),
            }
        };
        let token = |address, symbol: &str| Token {
            address,
            symbol: symbol.into(),
            decimals: 0,
        };
        PoolState::new(
            Pool {
                address,
                dex,
                token0: token(WETH, "T0"),
                token1: token(USDC, "T1"),
                kind,
            },
            block,
            None,
        )
    }

    fn hydrated(hi: i32) -> HydratedTicks {
        let mut ticks = BTreeMap::new();
        ticks.insert(-197_070, -1_776_514_264_016_992i128);
        HydratedTicks::for_test(ticks, -199_680, hi)
    }

    fn v3_candidate(address: Address) -> HistoricalQuoteInput {
        HistoricalQuoteInput::UniswapV3(
            HistoricalUniswapV3Input::new(
                BLOCK,
                v3_state(address, BLOCK, DexKind::UniswapV3),
                hydrated(INITIAL_HI),
            )
            .unwrap(),
        )
    }

    fn candidate_hi(candidate: &HistoricalQuoteInput) -> i32 {
        match candidate {
            HistoricalQuoteInput::UniswapV3(input) => input.hydrated().hydrated_tick_hi(),
            other => panic!("expected a Uniswap V3 candidate, got {other:?}"),
        }
    }

    /// Owns the mutable retry state so each test can drive one candidate.
    struct Harness {
        candidates: Vec<HistoricalQuoteInput>,
        radii: Vec<Option<i32>>,
        counts: Vec<u8>,
        block_reads: usize,
        report: String,
        adapter: UniswapV3Adapter,
    }

    impl Harness {
        fn new(radius: Option<i32>) -> Self {
            Self {
                candidates: vec![v3_candidate(POOL)],
                radii: vec![radius],
                counts: vec![0],
                block_reads: 0,
                report: String::new(),
                adapter: UniswapV3Adapter::new(),
            }
        }

        /// One attempt on candidate 0 with an injected reader. Returns the
        /// step result, how many times the reader ran, and the radius asked.
        async fn attempt(
            &mut self,
            result: EngineResult<(PoolState, HydratedTicks)>,
        ) -> (TickExpansionStep, usize, Option<i32>) {
            let calls = Cell::new(0usize);
            let requested = Cell::new(None);
            let mut ctx = TickCoverageRetryContext {
                rpc_url: "unused-no-rpc-in-tests",
                adapter: &self.adapter,
                candidate_word_radii: &mut self.radii,
                candidate_retry_counts: &mut self.counts,
                block_retry_reads: &mut self.block_reads,
                report: &mut self.report,
            };
            let step =
                attempt_tick_expansion(BLOCK, 0, POOL, &mut self.candidates, &mut ctx, |radius| {
                    calls.set(calls.get() + 1);
                    requested.set(Some(radius));
                    std::future::ready(result)
                })
                .await;
            (step, calls.get(), requested.get())
        }
    }

    fn rpc_failure() -> EngineResult<(PoolState, HydratedTicks)> {
        Err(EngineError::Chain("simulated archive rpc failure".into()))
    }

    fn valid_expansion() -> EngineResult<(PoolState, HydratedTicks)> {
        Ok((
            v3_state(POOL, BLOCK, DexKind::UniswapV3),
            hydrated(EXPANDED_HI),
        ))
    }

    // ---- limits are exactly as specified ----

    #[test]
    fn retry_limits_are_unchanged() {
        assert_eq!(MAX_TICK_COVERAGE_RETRIES_PER_POOL, 2);
        assert_eq!(MAX_TICK_COVERAGE_RETRY_READS_PER_BLOCK, 8);
    }

    // ---- classification ----

    #[test]
    fn only_the_typed_tick_coverage_error_on_a_v3_leg_is_retryable() {
        assert!(is_tick_coverage_retryable(
            DexKind::UniswapV3,
            &coverage_error()
        ));
    }

    #[test]
    fn other_error_types_are_not_retryable() {
        let others = [
            EngineError::Arithmetic("overflow".into()),
            EngineError::State("stale".into()),
            EngineError::Chain("rpc down".into()),
            EngineError::NotImplemented("stable pool".into()),
            EngineError::Dex {
                dex: "uniswap_v3".into(),
                reason: "slot0() failed".into(),
            },
            // Message text must never be matched; only the typed variant.
            EngineError::Dex {
                dex: "uniswap_v3".into(),
                reason: "insufficient Uniswap V3 tick coverage".into(),
            },
            EngineError::State("InsufficientV3TickCoverage".into()),
        ];
        for error in &others {
            assert!(
                !is_tick_coverage_retryable(DexKind::UniswapV3, error),
                "must not retry {error:?}"
            );
        }
    }

    #[test]
    fn non_uniswap_v3_legs_are_never_retryable() {
        for dex in [DexKind::Aerodrome, DexKind::AerodromeSlipstream] {
            assert!(
                !is_tick_coverage_retryable(dex, &coverage_error()),
                "{dex:?} must not be retried even with the typed error"
            );
        }
    }

    #[test]
    fn only_per_leg_quote_failures_can_name_a_failed_leg() {
        let leg = LegQuote {
            dex: DexKind::UniswapV3,
            pool_address: POOL,
            token_in: WETH,
            token_out: USDC,
            amount_in: U256::from(1u64),
            amount_out: U256::from(1u64),
            block: BLOCK,
        };

        let leg1 = RouteEvaluationFailure::Leg1QuoteFailed {
            error: coverage_error(),
        };
        let (which, error) = failed_quote_leg(&leg1).unwrap();
        assert_eq!(which, FailedQuoteLeg::Leg1);
        assert!(matches!(
            error,
            EngineError::InsufficientV3TickCoverage { .. }
        ));

        let leg2 = RouteEvaluationFailure::Leg2QuoteFailed {
            leg1: leg,
            error: coverage_error(),
        };
        assert_eq!(failed_quote_leg(&leg2).unwrap().0, FailedQuoteLeg::Leg2);

        // The same typed error wrapped in a non-quote failure is not retryable.
        let mapping = RouteEvaluationFailure::InvalidRouteCandidateMapping {
            error: coverage_error(),
        };
        assert!(failed_quote_leg(&mapping).is_none());
        let opportunity = RouteEvaluationFailure::OpportunityEvaluationFailed {
            leg1: leg,
            leg2: leg,
            error: coverage_error(),
        };
        assert!(failed_quote_leg(&opportunity).is_none());
    }

    #[test]
    fn candidate_must_still_be_the_claimed_uniswap_v3_pool() {
        let v3 = v3_candidate(POOL);
        assert!(candidate_is_retry_target(&v3, POOL));
        assert!(!candidate_is_retry_target(&v3, OTHER_POOL));

        let aerodrome = HistoricalQuoteInput::Aerodrome(
            HistoricalAerodromeInput::new(BLOCK, v3_state(POOL, BLOCK, DexKind::Aerodrome))
                .unwrap(),
        );
        assert!(!candidate_is_retry_target(&aerodrome, POOL));
    }

    // ---- pure budget decisions ----

    #[test]
    fn per_pool_limit_allows_two_attempts_then_skips() {
        for used in 0..MAX_TICK_COVERAGE_RETRIES_PER_POOL {
            assert_eq!(
                decide_tick_retry(0, used, Some(1)),
                TickRetryDecision::Expand {
                    current_radius: 1,
                    next_radius: 2
                },
                "attempt {} must be allowed",
                used + 1
            );
        }
        assert_eq!(
            decide_tick_retry(0, 2, Some(1)),
            TickRetryDecision::Skip(TickRetrySkipReason::PerPoolRetryLimit)
        );
        assert_eq!(
            decide_tick_retry(0, u8::MAX, Some(1)),
            TickRetryDecision::Skip(TickRetrySkipReason::PerPoolRetryLimit)
        );
    }

    #[test]
    fn per_block_limit_allows_eight_reads_then_skips() {
        for used in 0..MAX_TICK_COVERAGE_RETRY_READS_PER_BLOCK {
            assert!(
                matches!(
                    decide_tick_retry(used, 0, Some(1)),
                    TickRetryDecision::Expand { .. }
                ),
                "read {} must be allowed",
                used + 1
            );
        }
        for used in [8usize, 9, usize::MAX] {
            assert_eq!(
                decide_tick_retry(used, 0, Some(1)),
                TickRetryDecision::Skip(TickRetrySkipReason::PerBlockReadLimit)
            );
        }
    }

    #[test]
    fn per_block_limit_is_reported_before_per_pool_limit() {
        assert_eq!(
            decide_tick_retry(8, 2, Some(1)),
            TickRetryDecision::Skip(TickRetrySkipReason::PerBlockReadLimit)
        );
    }

    #[test]
    fn missing_initial_radius_is_skipped_safely() {
        assert_eq!(
            decide_tick_retry(0, 0, None),
            TickRetryDecision::Skip(TickRetrySkipReason::MissingInitialRadius)
        );
        // Limits still win over a missing radius.
        assert_eq!(
            decide_tick_retry(0, 2, None),
            TickRetryDecision::Skip(TickRetrySkipReason::PerPoolRetryLimit)
        );
    }

    #[test]
    fn radius_overflow_is_skipped_safely_without_wrapping() {
        assert_eq!(
            decide_tick_retry(0, 0, Some(i32::MAX)),
            TickRetryDecision::Skip(TickRetrySkipReason::RadiusOverflow)
        );
        assert_eq!(
            decide_tick_retry(0, 0, Some(i32::MAX - 1)),
            TickRetryDecision::Expand {
                current_radius: i32::MAX - 1,
                next_radius: i32::MAX
            }
        );
    }

    #[test]
    fn skip_reasons_render_the_exact_report_strings() {
        assert_eq!(
            TickRetrySkipReason::PerBlockReadLimit.report_reason(),
            "per_block_retry_read_limit(8)"
        );
        assert_eq!(
            TickRetrySkipReason::PerPoolRetryLimit.report_reason(),
            "per_pool_retry_limit(2)"
        );
        assert_eq!(
            TickRetrySkipReason::MissingInitialRadius.report_reason(),
            "missing_initial_radius"
        );
        assert_eq!(
            TickRetrySkipReason::RadiusOverflow.report_reason(),
            "radius_overflow"
        );
    }

    #[test]
    fn consuming_budget_increments_both_counters_and_saturates() {
        let (mut block_reads, mut pool_count) = (0usize, 0u8);
        consume_tick_retry_budget(&mut block_reads, &mut pool_count);
        assert_eq!((block_reads, pool_count), (1, 1));

        let (mut block_reads, mut pool_count) = (usize::MAX, u8::MAX);
        consume_tick_retry_budget(&mut block_reads, &mut pool_count);
        assert_eq!((block_reads, pool_count), (usize::MAX, u8::MAX));
    }

    // ---- the real attempt step, with an injected reader ----

    #[tokio::test]
    async fn failed_rpc_attempts_consume_budget_until_the_pool_limit() {
        let mut h = Harness::new(Some(1));

        let (step, calls, requested) = h.attempt(rpc_failure()).await;
        assert_eq!(step, TickExpansionStep::Failed);
        assert_eq!((calls, requested), (1, Some(2)));
        assert_eq!((h.block_reads, h.counts[0]), (1, 1));
        assert_eq!(h.radii[0], Some(1), "a failed read must not change radius");
        assert_eq!(candidate_hi(&h.candidates[0]), INITIAL_HI);

        let (step, calls, requested) = h.attempt(rpc_failure()).await;
        assert_eq!(step, TickExpansionStep::Failed);
        assert_eq!((calls, requested), (1, Some(2)), "radius is not advanced");
        assert_eq!((h.block_reads, h.counts[0]), (2, 2));

        // Third attempt: budget exhausted, so the reader must not run.
        let (step, calls, _) = h.attempt(valid_expansion()).await;
        assert_eq!(step, TickExpansionStep::Skipped);
        assert_eq!(calls, 0);
        assert_eq!((h.block_reads, h.counts[0]), (2, 2));
        assert_eq!(candidate_hi(&h.candidates[0]), INITIAL_HI);
        assert!(h.report.contains("status=FAILED error="));
        assert!(h
            .report
            .contains("status=SKIPPED reason=per_pool_retry_limit(2)"));
    }

    #[tokio::test]
    async fn per_block_limit_stops_reads_across_pools() {
        let mut h = Harness::new(Some(1));
        h.block_reads = MAX_TICK_COVERAGE_RETRY_READS_PER_BLOCK - 1;

        let (step, calls, _) = h.attempt(rpc_failure()).await;
        assert_eq!((step, calls), (TickExpansionStep::Failed, 1));
        assert_eq!(h.block_reads, MAX_TICK_COVERAGE_RETRY_READS_PER_BLOCK);

        // A fresh pool on the same block is still blocked by the block limit.
        h.counts[0] = 0;
        let (step, calls, _) = h.attempt(valid_expansion()).await;
        assert_eq!((step, calls), (TickExpansionStep::Skipped, 0));
        assert_eq!(h.block_reads, MAX_TICK_COVERAGE_RETRY_READS_PER_BLOCK);
        assert_eq!(h.counts[0], 0, "a skipped attempt consumes nothing");
        assert!(h
            .report
            .contains("status=SKIPPED reason=per_block_retry_read_limit(8)"));
    }

    #[tokio::test]
    async fn missing_or_overflowing_radius_skips_without_reading_or_consuming() {
        for (radius, reason) in [
            (None, "missing_initial_radius"),
            (Some(i32::MAX), "radius_overflow"),
        ] {
            let mut h = Harness::new(radius);
            let (step, calls, _) = h.attempt(valid_expansion()).await;
            assert_eq!((step, calls), (TickExpansionStep::Skipped, 0));
            assert_eq!((h.block_reads, h.counts[0]), (0, 0));
            assert_eq!(h.radii[0], radius);
            assert_eq!(candidate_hi(&h.candidates[0]), INITIAL_HI);
            assert!(h.report.contains(&format!("reason={reason}")), "{reason}");
        }
    }

    #[tokio::test]
    async fn radius_and_candidate_update_only_after_successful_validation() {
        let mut h = Harness::new(Some(1));
        let (step, calls, requested) = h.attempt(valid_expansion()).await;
        assert_eq!(step, TickExpansionStep::Expanded);
        assert_eq!((calls, requested), (1, Some(2)));
        assert_eq!(h.radii[0], Some(2));
        assert_eq!(candidate_hi(&h.candidates[0]), EXPANDED_HI);
        assert_eq!((h.block_reads, h.counts[0]), (1, 1));
        assert!(h
            .report
            .contains("radius=1->2 status=EXPANDED reads_used=1"));
    }

    #[tokio::test]
    async fn state_from_a_different_block_is_rejected_and_changes_nothing() {
        let mut h = Harness::new(Some(1));
        let wrong_block = Ok((
            v3_state(POOL, BLOCK + 1, DexKind::UniswapV3),
            hydrated(EXPANDED_HI),
        ));
        let (step, calls, _) = h.attempt(wrong_block).await;
        assert_eq!((step, calls), (TickExpansionStep::Failed, 1));
        assert_eq!(h.radii[0], Some(1), "radius must not move on rejection");
        assert_eq!(candidate_hi(&h.candidates[0]), INITIAL_HI);
        assert_eq!(
            (h.block_reads, h.counts[0]),
            (1, 1),
            "a rejected expansion still consumes budget"
        );
        assert!(h.report.contains("reason=historical_input_validation"));
    }

    #[tokio::test]
    async fn state_that_fails_validation_for_any_reason_changes_nothing() {
        // Wrong DEX kind for a Uniswap V3 candidate.
        let mut h = Harness::new(Some(1));
        let aerodrome_state = Ok((
            v3_state(POOL, BLOCK, DexKind::Aerodrome),
            hydrated(EXPANDED_HI),
        ));
        let (step, _, _) = h.attempt(aerodrome_state).await;
        assert_eq!(step, TickExpansionStep::Failed);
        assert_eq!(h.radii[0], Some(1));
        assert_eq!(candidate_hi(&h.candidates[0]), INITIAL_HI);
        assert_eq!((h.block_reads, h.counts[0]), (1, 1));
    }

    #[tokio::test]
    async fn returned_pool_address_mismatch_changes_nothing() {
        let mut h = Harness::new(Some(1));
        let other = Ok((
            v3_state(OTHER_POOL, BLOCK, DexKind::UniswapV3),
            hydrated(EXPANDED_HI),
        ));
        let (step, _, _) = h.attempt(other).await;
        assert_eq!(step, TickExpansionStep::Failed);
        assert_eq!(h.radii[0], Some(1));
        assert_eq!(candidate_hi(&h.candidates[0]), INITIAL_HI);
        assert_eq!((h.block_reads, h.counts[0]), (1, 1));
        assert!(h.report.contains("reason=returned_pool_address_mismatch"));
    }

    #[tokio::test]
    async fn successful_expansions_still_stop_at_the_per_pool_limit() {
        let mut h = Harness::new(Some(1));
        for expected_radius in [2, 3] {
            let (step, _, requested) = h.attempt(valid_expansion()).await;
            assert_eq!(step, TickExpansionStep::Expanded);
            assert_eq!(requested, Some(expected_radius));
            assert_eq!(h.radii[0], Some(expected_radius));
        }
        let (step, calls, _) = h.attempt(valid_expansion()).await;
        assert_eq!((step, calls), (TickExpansionStep::Skipped, 0));
        assert_eq!(h.radii[0], Some(3));
        assert_eq!((h.block_reads, h.counts[0]), (2, 2));
    }
}
