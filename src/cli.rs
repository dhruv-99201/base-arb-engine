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
use crate::arbitrage::{
    detect_cross_dex_opportunities_at_block, HistoricalAerodromeInput, HistoricalQuoteInput,
    HistoricalUniswapV3Input, RouteEvaluationFailure, RouteOutcome,
};
use crate::dex::{AerodromeAdapter, UniswapV3Adapter};
use crate::error::{EngineError, EngineResult};
use crate::market::models::{DexKind, Pool, PoolKind, Token};
use crate::pricing::v3_quote::{quote_exact_input, HydratedTicks, HydratedV3State, V3QuoteResult};
use alloy::primitives::{Address, B256, U256};
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

pub const INSPECT_V3_USAGE: &str = "\
Usage: cargo run -- inspect-v3 --pool <ADDRESS> --block <U64> --amount-in <DECIMAL> [--zero-for-one]

Read-only validation command. Fetches Uniswap V3 pool state (slot0,
liquidity, fee, tickSpacing, token0, token1) and a real hydrated
tick-bitmap range, both pinned to the exact requested historical block via
UniswapV3Adapter::get_pool_state_and_ticks_at_block, builds a
HydratedV3State from that pinned snapshot, and runs a single exact-input
quote through quote_exact_input (the real tick-crossing swap loop).
Prints a deterministic report of every input and result field.

Does not modify any blockchain state, requires no private key, and never
signs or submits a transaction. No router/quoter dependency. Uses
BASE_RPC_URL from your environment/.env exactly like the rest of this
program - a historical read at --block requires an archive-capable RPC
endpoint.

Options:
  --pool <ADDRESS>        Uniswap V3 pool address. Required.
  --block <U64>           Historical block number every read is pinned to. Required.
  --amount-in <DECIMAL>   Exact input amount, as a base-10 integer in the
                          input token's native (wei-like) units. No
                          floating point. Required.
  --zero-for-one          If present, quote token0 -> token1 (price
                          decreases). If absent, quote token1 -> token0
                          (price increases).
  --help, -h              Show this help and exit.

Example:
  cargo run -- inspect-v3 --pool 0x1234567890123456789012345678901234567890 --block 12345678 --amount-in 1000000000000000000 --zero-for-one
";

/// Tick-bitmap word radius `inspect-v3` hydrates around the pool's current
/// tick. There is no project-wide default for this yet (no config field,
/// no other production call site) - `1` is the smallest non-zero radius
/// already exercised by `dex::uniswap_v3::UniswapV3Adapter`'s own test
/// (`get_pool_state_and_ticks_at_block_no_longer_short_circuits_some_block`),
/// reused here rather than inventing a new arbitrary value.
const INSPECT_V3_WORD_RADIUS: i32 = 1;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InspectV3Command {
    Help,
    Run {
        pool: Address,
        block: u64,
        amount_in: U256,
        zero_for_one: bool,
    },
}

/// Parse `inspect-v3` subcommand arguments. Pure function - no I/O, fully
/// unit-testable, same manual style as `parse_discover_test_args`/
/// `parse_inspect_tx_args`. `--zero-for-one` is a bare flag (present =
/// true, absent = false), never takes a value.
pub fn parse_inspect_v3_args(args: &[String]) -> Result<InspectV3Command, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(InspectV3Command::Help);
    }

    let mut pool: Option<Address> = None;
    let mut block: Option<u64> = None;
    let mut amount_in: Option<U256> = None;
    let mut zero_for_one = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pool" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--pool requires a value".to_string())?;
                pool = Some(
                    raw.parse::<Address>()
                        .map_err(|_| format!("invalid --pool value: '{raw}'"))?,
                );
                i += 2;
            }
            "--block" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--block requires a value".to_string())?;
                block = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid --block value: '{raw}'"))?,
                );
                i += 2;
            }
            "--amount-in" => {
                let raw = args
                    .get(i + 1)
                    .ok_or_else(|| "--amount-in requires a value".to_string())?;
                amount_in = Some(U256::from_str_radix(raw, 10).map_err(|_| {
                    format!(
                        "invalid --amount-in value: '{raw}' (expected a non-negative base-10 \
                         integer, no floating point)"
                    )
                })?);
                i += 2;
            }
            "--zero-for-one" => {
                zero_for_one = true;
                i += 1;
            }
            other => return Err(format!("unrecognized argument: '{other}'")),
        }
    }

    let pool = pool.ok_or_else(|| "missing required --pool <ADDRESS>".to_string())?;
    let block = block.ok_or_else(|| "missing required --block <U64>".to_string())?;
    let amount_in =
        amount_in.ok_or_else(|| "missing required --amount-in <DECIMAL>".to_string())?;

    Ok(InspectV3Command::Run {
        pool,
        block,
        amount_in,
        zero_for_one,
    })
}

/// Format one `inspect-v3` result. Pure/deterministic: the same inputs
/// always produce the same output string, and every field the spec
/// requires is present. Takes the already-extracted `ConcentratedLiquidity`
/// scalar fields (and the fetched token0/token1 addresses) directly
/// (rather than a whole `PoolState`) so this function itself never needs
/// to match on `PoolKind` or fabricate a fallback for the Aerodrome case -
/// `run_inspect_v3` does that matching once, as a real `EngineResult`
/// error path, before ever calling this.
#[allow(clippy::too_many_arguments)]
pub fn format_inspect_v3_report(
    pool: Address,
    block: u64,
    token0: Address,
    token1: Address,
    fee_tier: u32,
    tick_spacing: i32,
    sqrt_price_x96: U256,
    current_tick: i32,
    liquidity: u128,
    hydrated: &HydratedTicks,
    amount_in: U256,
    zero_for_one: bool,
    result: &V3QuoteResult,
) -> String {
    let direction = if zero_for_one {
        "zero_for_one (token0 -> token1)"
    } else {
        "one_for_zero (token1 -> token0)"
    };

    format!(
        "inspect-v3 report\n\
         pool={pool}\n\
         block={block}\n\
         token0={token0}\n\
         token1={token1}\n\
         fee={fee_tier}\n\
         tick_spacing={tick_spacing}\n\
         sqrt_price_x96={sqrt_price_x96}\n\
         current_tick={current_tick}\n\
         liquidity={liquidity}\n\
         hydrated_tick_lower_bound={}\n\
         hydrated_tick_upper_bound={}\n\
         initialized_tick_count={}\n\
         amount_in={amount_in}\n\
         direction={direction}\n\
         amount_out={}\n\
         fee_paid={}\n\
         ending_sqrt_price_x96={}\n\
         ending_tick={}\n\
         ticks_crossed={}",
        hydrated.hydrated_tick_lo(),
        hydrated.hydrated_tick_hi(),
        hydrated.initialized_tick_count(),
        result.amount_out,
        result.fee_paid,
        result.ending_sqrt_price_x96,
        result.ending_tick,
        result.ticks_crossed,
    )
}

/// Execute `inspect-v3`: fetch a block-pinned Uniswap V3 pool snapshot
/// (state + token0/token1 + hydrated ticks), build a `HydratedV3State`
/// from it, run a single exact-input quote through the real
/// tick-crossing swap loop (`quote_exact_input`), and print a
/// deterministic report. Read-only - no signer, no transaction
/// submission, no router/quoter dependency, no cross-DEX logic, no
/// arbitrage execution. Never silently falls back to "latest" if the
/// requested block is unavailable - `at_block: Some(block)` is passed
/// straight through, and any RPC/adapter failure surfaces as a real
/// `EngineError` rather than being swallowed or reinterpreted as a quote
/// failure.
pub async fn run_inspect_v3(
    config: &Config,
    pool: Address,
    block: u64,
    amount_in: U256,
    zero_for_one: bool,
) -> EngineResult<()> {
    let adapter = UniswapV3Adapter::new();

    // Placeholder pool identity: `get_pool_state_and_ticks_at_block` (via
    // `get_pool_state_impl`) only ever reads `.address` from this struct
    // for the state/ticks reads - `.kind` is fully overwritten from the
    // real on-chain reads, and `.token0.address`/`.token1.address` are
    // now also overwritten from real `token0()`/`token1()` reads (see
    // `UniswapV3Adapter::get_pool_state_and_ticks_at_block`). Only
    // `symbol`/`decimals` stay as placeholders, since this command never
    // calls `symbol()`/`decimals()` - the report only needs the addresses.
    let placeholder_pool = Pool {
        address: pool,
        dex: DexKind::UniswapV3,
        token0: Token {
            address: Address::ZERO,
            symbol: "UNKNOWN".into(),
            decimals: 18,
        },
        token1: Token {
            address: Address::ZERO,
            symbol: "UNKNOWN".into(),
            decimals: 18,
        },
        kind: PoolKind::ConcentratedLiquidity {
            fee_tier: 0,
            tick_spacing: 1,
            sqrt_price_x96: U256::ZERO,
            current_tick: 0,
            liquidity: 0,
            initialized_ticks: Default::default(),
        },
    };

    println!("inspect-v3: pool={pool} block={block}");

    let (pool_state, hydrated) = adapter
        .get_pool_state_and_ticks_at_block(
            &config.base_rpc_url,
            &placeholder_pool,
            INSPECT_V3_WORD_RADIUS,
            Some(block),
        )
        .await?;

    let token0 = pool_state.pool.token0.address;
    let token1 = pool_state.pool.token1.address;

    let (fee_tier, tick_spacing, sqrt_price_x96, current_tick, liquidity) =
        match &pool_state.pool.kind {
            PoolKind::ConcentratedLiquidity {
                fee_tier,
                tick_spacing,
                sqrt_price_x96,
                current_tick,
                liquidity,
                ..
            } => (
                *fee_tier,
                *tick_spacing,
                *sqrt_price_x96,
                *current_tick,
                *liquidity,
            ),
            PoolKind::Aerodrome { .. } => {
                return Err(EngineError::Dex {
                    dex: "uniswap_v3".into(),
                    reason: "inspect-v3: pool_state.kind resolved to Aerodrome after \
                              get_pool_state_and_ticks_at_block - wrong adapter for this pool"
                        .into(),
                });
            }
        };

    let state = HydratedV3State::from_pool_state(&pool_state, &hydrated)?;
    let result = quote_exact_input(&state, amount_in, zero_for_one)?;

    println!(
        "\n{}",
        format_inspect_v3_report(
            pool,
            block,
            token0,
            token1,
            fee_tier,
            tick_spacing,
            sqrt_price_x96,
            current_tick,
            liquidity,
            &hydrated,
            amount_in,
            zero_for_one,
            &result,
        )
    );

    Ok(())
}

pub const INSPECT_ARB_USAGE: &str = "\
Usage: cargo run -- inspect-arb --aerodrome-pool <ADDRESS> --uniswap-v3-pool <ADDRESS> --block <U64> --token-in <ADDRESS> --amount-in <DECIMAL_INTEGER> [--word-radius <I32>]

Read-only historical cross-DEX inspection. Reads one Aerodrome Classic
pool and one Uniswap V3 pool (with hydrated ticks), both pinned to the
exact same historical block number, builds the validated historical
inputs, and runs the existing cross-DEX opportunity detector on them.
Prints EVERY route outcome (successes, losses, zero-profit routes and
quote failures); nothing is sorted or filtered by profit.

The printed gross_profit_raw is a raw quote difference in the smallest
unit of the starting token. It is NOT net profit: no gas cost and no
execution or settlement cost is subtracted, it is a historical quote
evaluation (not a live or executable signal), and no transaction is ever
signed or submitted. No USD conversion is attempted.

Uses BASE_RPC_URL from your environment/.env. A historical read at --block
requires an archive-capable RPC endpoint when that block is outside the
provider's retained state window; there is no fallback to latest state.
Reads are pinned by block number, not by block hash.

Options:
  --aerodrome-pool <ADDRESS>   Aerodrome Classic (volatile) pool address. Required.
  --uniswap-v3-pool <ADDRESS>  Uniswap V3 pool address. Required.
  --block <U64>                Historical block every read is pinned to. Required.
  --token-in <ADDRESS>         Starting token; must be one of the pools' tokens. Required.
  --amount-in <DECIMAL_INTEGER>
                               Positive base-10 integer in the starting
                               token's smallest units. No floating point. Required.
  --word-radius <I32>          Non-negative V3 tick-bitmap word radius
                               (default: 1, same as inspect-v3).
  --help, -h                   Show this help and exit.

Example:
  cargo run -- inspect-arb --aerodrome-pool 0xcDAC0d6c6C59727a65F871236188350531885C43 --uniswap-v3-pool 0xd0b53D9277642d899DF5C87A3966A349A798F224 --block 26000000 --token-in 0x4200000000000000000000000000000000000006 --amount-in 1000000000000000
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectArbCommand {
    Help,
    Run {
        aerodrome_pool: Address,
        uniswap_v3_pool: Address,
        block: u64,
        token_in: Address,
        amount_in: U256,
        word_radius: i32,
    },
}

/// Parse `inspect-arb` arguments (everything after `inspect-arb`). Pure
/// function - no I/O, same manual style as `parse_inspect_v3_args`.
/// `--help`/`-h` wins over everything else. Every value is validated here,
/// before any RPC call.
pub fn parse_inspect_arb_args(args: &[String]) -> Result<InspectArbCommand, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(InspectArbCommand::Help);
    }

    fn value<'a>(args: &'a [String], i: usize, flag: &str) -> Result<&'a String, String> {
        args.get(i + 1)
            .ok_or_else(|| format!("{flag} requires a value"))
    }

    let mut aerodrome_pool: Option<Address> = None;
    let mut uniswap_v3_pool: Option<Address> = None;
    let mut block: Option<u64> = None;
    let mut token_in: Option<Address> = None;
    let mut amount_in: Option<U256> = None;
    let mut word_radius = INSPECT_V3_WORD_RADIUS;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--aerodrome-pool" => {
                let raw = value(args, i, "--aerodrome-pool")?;
                aerodrome_pool = Some(
                    raw.parse::<Address>()
                        .map_err(|_| format!("invalid --aerodrome-pool value: '{raw}'"))?,
                );
            }
            "--uniswap-v3-pool" => {
                let raw = value(args, i, "--uniswap-v3-pool")?;
                uniswap_v3_pool = Some(
                    raw.parse::<Address>()
                        .map_err(|_| format!("invalid --uniswap-v3-pool value: '{raw}'"))?,
                );
            }
            "--block" => {
                let raw = value(args, i, "--block")?;
                block = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid --block value: '{raw}'"))?,
                );
            }
            "--token-in" => {
                let raw = value(args, i, "--token-in")?;
                token_in = Some(
                    raw.parse::<Address>()
                        .map_err(|_| format!("invalid --token-in value: '{raw}'"))?,
                );
            }
            "--amount-in" => {
                let raw = value(args, i, "--amount-in")?;
                // `U256::from_str_radix("", 10)` yields 0 rather than an
                // error, so an empty value must be rejected explicitly.
                let parsed = if raw.is_empty() {
                    None
                } else {
                    U256::from_str_radix(raw, 10).ok()
                }
                .ok_or_else(|| {
                    format!(
                        "invalid --amount-in value: '{raw}' (expected a positive base-10 \
                         integer, no floating point)"
                    )
                })?;
                if parsed.is_zero() {
                    return Err("--amount-in must be greater than zero".to_string());
                }
                amount_in = Some(parsed);
            }
            "--word-radius" => {
                let raw = value(args, i, "--word-radius")?;
                let parsed = raw.parse::<i32>().map_err(|_| {
                    format!("invalid --word-radius value: '{raw}' (expected a non-negative integer)")
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

    Ok(InspectArbCommand::Run {
        aerodrome_pool: aerodrome_pool
            .ok_or_else(|| "missing required --aerodrome-pool <ADDRESS>".to_string())?,
        uniswap_v3_pool: uniswap_v3_pool
            .ok_or_else(|| "missing required --uniswap-v3-pool <ADDRESS>".to_string())?,
        block: block.ok_or_else(|| "missing required --block <U64>".to_string())?,
        token_in: token_in.ok_or_else(|| "missing required --token-in <ADDRESS>".to_string())?,
        amount_in: amount_in
            .ok_or_else(|| "missing required --amount-in <DECIMAL_INTEGER>".to_string())?,
        word_radius,
    })
}

/// Abort unless the RPC-reported chain ID is the configured one.
pub fn check_chain_id(observed: u64, configured: u64) -> EngineResult<()> {
    if observed != configured {
        return Err(EngineError::Chain(format!(
            "inspect-arb: RPC reports chain id {observed} but BASE_CHAIN_ID is {configured}; \
             refusing to inspect an unintended chain"
        )));
    }
    Ok(())
}

/// Both pools' on-chain token pairs must be the same UNORDERED pair, and
/// `token_in` must be one of them. Pure; never reorders or repairs.
pub fn validate_pair_and_direction(
    aerodrome_pool: Address,
    aerodrome_tokens: (Address, Address),
    uniswap_v3_pool: Address,
    uniswap_v3_tokens: (Address, Address),
    token_in: Address,
) -> EngineResult<()> {
    let (a0, a1) = aerodrome_tokens;
    let (v0, v1) = uniswap_v3_tokens;
    let same_pair = (a0 == v0 && a1 == v1) || (a0 == v1 && a1 == v0);
    if !same_pair {
        return Err(EngineError::State(format!(
            "inspect-arb: pools do not share the same token pair: aerodrome pool \
             {aerodrome_pool} has ({a0}, {a1}), uniswap_v3 pool {uniswap_v3_pool} has ({v0}, {v1})"
        )));
    }
    if token_in != a0 && token_in != a1 {
        return Err(EngineError::State(format!(
            "inspect-arb: --token-in {token_in} is not one of the pools' tokens ({a0}, {a1})"
        )));
    }
    Ok(())
}

/// Request/snapshot facts printed at the top of the `inspect-arb` report.
#[derive(Debug, Clone, Copy)]
pub struct InspectArbHeader {
    pub chain_id: u64,
    pub block: u64,
    pub aerodrome_pool: Address,
    pub aerodrome_tokens: (Address, Address),
    pub uniswap_v3_pool: Address,
    pub uniswap_v3_tokens: (Address, Address),
    pub token_in: Address,
    pub amount_in: U256,
    pub word_radius: i32,
}

/// Deterministic `inspect-arb` report. Pure: same inputs, same string.
/// Prints every outcome in the order given - no sorting, no filtering.
pub fn format_inspect_arb_report(header: &InspectArbHeader, outcomes: &[RouteOutcome]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "inspect-arb report\n\
         chain_id={}\n\
         block={}\n\
         aerodrome_pool={}\n\
         aerodrome_token0={}\n\
         aerodrome_token1={}\n\
         uniswap_v3_pool={}\n\
         uniswap_v3_token0={}\n\
         uniswap_v3_token1={}\n\
         token_in={}\n\
         amount_in_raw={}\n\
         v3_word_radius={}\n\
         routes_evaluated={}\n",
        header.chain_id,
        header.block,
        header.aerodrome_pool,
        header.aerodrome_tokens.0,
        header.aerodrome_tokens.1,
        header.uniswap_v3_pool,
        header.uniswap_v3_tokens.0,
        header.uniswap_v3_tokens.1,
        header.token_in,
        header.amount_in,
        header.word_radius,
        outcomes.len(),
    ));

    if outcomes.is_empty() {
        out.push_str("no route matched the requested token direction; nothing was evaluated\n");
    }

    for (idx, outcome) in outcomes.iter().enumerate() {
        let route = outcome.route();
        out.push_str(&format!(
            "\n[route {}]\n\
             start_token={}\n\
             intermediate_token={}\n\
             leg1={} pool={}\n\
             leg2={} pool={}\n\
             route_block={}\n",
            idx + 1,
            route.starting_token(),
            route.intermediate_token(),
            route.leg1().dex(),
            route.leg1().pool_address(),
            route.leg2().dex(),
            route.leg2().pool_address(),
            route.block(),
        ));
        match outcome.result() {
            Ok(opp) => out.push_str(&format!(
                "status=OK\n\
                 input_amount_raw={}\n\
                 intermediate_amount_raw={}\n\
                 final_amount_raw={}\n\
                 gross_profit_raw={}\n\
                 gross_profit_token={}\n",
                opp.input_amount(),
                opp.intermediate_amount(),
                opp.final_amount(),
                opp.gross_profit(),
                opp.input_token(),
            )),
            Err(RouteEvaluationFailure::InvalidRouteCandidateMapping { error }) => out.push_str(
                &format!("status=FAILED kind=InvalidRouteCandidateMapping\nerror={error}\n"),
            ),
            Err(RouteEvaluationFailure::Leg1QuoteFailed { error }) => {
                out.push_str(&format!("status=FAILED kind=Leg1QuoteFailed\nerror={error}\n"))
            }
            Err(RouteEvaluationFailure::Leg2QuoteFailed { leg1, error }) => out.push_str(&format!(
                "status=FAILED kind=Leg2QuoteFailed\n\
                 leg1_amount_in_raw={}\n\
                 leg1_amount_out_raw={}\n\
                 error={error}\n",
                leg1.amount_in, leg1.amount_out,
            )),
            Err(RouteEvaluationFailure::OpportunityEvaluationFailed { leg1, leg2, error }) => {
                out.push_str(&format!(
                    "status=FAILED kind=OpportunityEvaluationFailed\n\
                     leg1_amount_in_raw={}\n\
                     leg1_amount_out_raw={}\n\
                     leg2_amount_in_raw={}\n\
                     leg2_amount_out_raw={}\n\
                     error={error}\n",
                    leg1.amount_in, leg1.amount_out, leg2.amount_in, leg2.amount_out,
                ))
            }
        }
    }

    out.push_str(
        "\nNOTES\n\
         - gross_profit_raw is a raw quote difference in the smallest unit of the starting token (gross_profit_token); it is NOT net profit.\n\
         - No gas cost is subtracted.\n\
         - No execution or settlement cost is subtracted.\n\
         - This is a historical quote evaluation at the pinned block, not a live opportunity or an executable trading signal.\n\
         - No transaction is signed or submitted.\n",
    );
    out
}

/// Execute `inspect-arb`: read-only, block-pinned Aerodrome + Uniswap V3
/// snapshot at one historical block, fed through the existing validated
/// historical inputs and `detect_cross_dex_opportunities_at_block`. No
/// signer, no transaction, no fallback to latest state; every RPC failure
/// is propagated.
pub async fn run_inspect_arb(
    config: &Config,
    aerodrome_pool: Address,
    uniswap_v3_pool: Address,
    block: u64,
    token_in: Address,
    amount_in: U256,
    word_radius: i32,
) -> EngineResult<()> {
    let url = config
        .base_rpc_url
        .parse()
        .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
    let provider = ProviderBuilder::new().connect_http(url);
    let chain_id = provider
        .get_chain_id()
        .await
        .map_err(|e| EngineError::Chain(format!("get_chain_id failed: {e}")))?;
    check_chain_id(chain_id, config.base_chain_id)?;

    let aerodrome_adapter =
        AerodromeAdapter::with_factory_address(config.aerodrome_factory_address);
    let uniswap_adapter = UniswapV3Adapter::new();

    // Placeholder identities: only `.address` is used to read; both token
    // addresses are filled from real pinned `token0()`/`token1()` reads.
    // `decimals: 0` is an explicit "unset" sentinel - this command never
    // reads or uses decimals and all amounts stay raw integers.
    let unknown_token = || Token {
        address: Address::ZERO,
        symbol: "UNKNOWN".into(),
        decimals: 0,
    };
    let aerodrome_placeholder = Pool {
        address: aerodrome_pool,
        dex: DexKind::Aerodrome,
        token0: unknown_token(),
        token1: unknown_token(),
        kind: PoolKind::Aerodrome {
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
            stable: false,
            fee_bps: None,
        },
    };
    let uniswap_placeholder = Pool {
        address: uniswap_v3_pool,
        dex: DexKind::UniswapV3,
        token0: unknown_token(),
        token1: unknown_token(),
        kind: PoolKind::ConcentratedLiquidity {
            fee_tier: 0,
            tick_spacing: 1,
            sqrt_price_x96: U256::ZERO,
            current_tick: 0,
            liquidity: 0,
            initialized_ticks: Default::default(),
        },
    };

    let aerodrome_state = aerodrome_adapter
        .get_pool_state_at_block(&config.base_rpc_url, &aerodrome_placeholder, block)
        .await?;
    let (uniswap_state, hydrated) = uniswap_adapter
        .get_pool_state_and_ticks_at_block(
            &config.base_rpc_url,
            &uniswap_placeholder,
            word_radius,
            Some(block),
        )
        .await?;

    let aerodrome_tokens = (
        aerodrome_state.pool.token0.address,
        aerodrome_state.pool.token1.address,
    );
    let uniswap_tokens = (
        uniswap_state.pool.token0.address,
        uniswap_state.pool.token1.address,
    );

    let aerodrome_input = HistoricalAerodromeInput::new(block, aerodrome_state)?;
    let uniswap_input = HistoricalUniswapV3Input::from_pinned_read(block, (uniswap_state, hydrated))?;

    validate_pair_and_direction(
        aerodrome_pool,
        aerodrome_tokens,
        uniswap_v3_pool,
        uniswap_tokens,
        token_in,
    )?;

    let candidates = vec![
        HistoricalQuoteInput::Aerodrome(aerodrome_input),
        HistoricalQuoteInput::UniswapV3(uniswap_input),
    ];
    let outcomes = detect_cross_dex_opportunities_at_block(block, token_in, amount_in, &candidates)?;

    let header = InspectArbHeader {
        chain_id,
        block,
        aerodrome_pool,
        aerodrome_tokens,
        uniswap_v3_pool,
        uniswap_v3_tokens: uniswap_tokens,
        token_in,
        amount_in,
        word_radius,
    };
    println!("{}", format_inspect_arb_report(&header, &outcomes));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

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

    // --- inspect-v3 argument parsing ---

    const SAMPLE_POOL: &str = "0x1234567890123456789012345678901234567890";

    #[test]
    fn inspect_v3_parses_valid_complete_command() {
        let result = parse_inspect_v3_args(&args(&[
            "--pool",
            SAMPLE_POOL,
            "--block",
            "12345678",
            "--amount-in",
            "1000000000000000000",
            "--zero-for-one",
        ]));
        assert_eq!(
            result,
            Ok(InspectV3Command::Run {
                pool: SAMPLE_POOL.parse().unwrap(),
                block: 12_345_678,
                amount_in: U256::from(1_000_000_000_000_000_000u128),
                zero_for_one: true,
            })
        );
    }

    #[test]
    fn inspect_v3_help_flag_works() {
        assert_eq!(
            parse_inspect_v3_args(&args(&["--help"])),
            Ok(InspectV3Command::Help)
        );
        assert_eq!(
            parse_inspect_v3_args(&args(&["-h"])),
            Ok(InspectV3Command::Help)
        );
    }

    #[test]
    fn inspect_v3_help_short_circuits_with_bogus_args() {
        assert_eq!(
            parse_inspect_v3_args(&args(&["--totally-bogus", "value", "--help"])),
            Ok(InspectV3Command::Help)
        );
    }

    #[test]
    fn inspect_v3_missing_pool_is_rejected() {
        let err = parse_inspect_v3_args(&args(&["--block", "1", "--amount-in", "1"])).unwrap_err();
        assert!(err.contains("--pool"));
    }

    #[test]
    fn inspect_v3_malformed_pool_is_rejected() {
        let err = parse_inspect_v3_args(&args(&[
            "--pool",
            "not-an-address",
            "--block",
            "1",
            "--amount-in",
            "1",
        ]))
        .unwrap_err();
        assert!(err.contains("invalid --pool value"));
    }

    #[test]
    fn inspect_v3_missing_block_is_rejected() {
        let err =
            parse_inspect_v3_args(&args(&["--pool", SAMPLE_POOL, "--amount-in", "1"])).unwrap_err();
        assert!(err.contains("--block"));
    }

    #[test]
    fn inspect_v3_malformed_block_is_rejected() {
        let err = parse_inspect_v3_args(&args(&[
            "--pool",
            SAMPLE_POOL,
            "--block",
            "not-a-number",
            "--amount-in",
            "1",
        ]))
        .unwrap_err();
        assert!(err.contains("invalid --block value"));
    }

    #[test]
    fn inspect_v3_missing_amount_in_is_rejected() {
        let err = parse_inspect_v3_args(&args(&["--pool", SAMPLE_POOL, "--block", "1"])).unwrap_err();
        assert!(err.contains("--amount-in"));
    }

    #[test]
    fn inspect_v3_malformed_amount_in_is_rejected() {
        // Floating-point input must be rejected - U256::from_str_radix has
        // no notion of a decimal point.
        let err = parse_inspect_v3_args(&args(&[
            "--pool",
            SAMPLE_POOL,
            "--block",
            "1",
            "--amount-in",
            "1.5",
        ]))
        .unwrap_err();
        assert!(err.contains("invalid --amount-in value"));
    }

    #[test]
    fn inspect_v3_dangling_flag_without_value_is_rejected() {
        let err = parse_inspect_v3_args(&args(&["--pool"])).unwrap_err();
        assert!(err.contains("--pool requires a value"));
    }

    #[test]
    fn inspect_v3_unrecognized_argument_is_rejected() {
        let err = parse_inspect_v3_args(&args(&["--wat", "1"])).unwrap_err();
        assert!(err.contains("unrecognized argument"));
    }

    #[test]
    fn inspect_v3_zero_for_one_present_is_true() {
        let result = parse_inspect_v3_args(&args(&[
            "--pool",
            SAMPLE_POOL,
            "--block",
            "1",
            "--amount-in",
            "1",
            "--zero-for-one",
        ]));
        match result {
            Ok(InspectV3Command::Run { zero_for_one, .. }) => assert!(zero_for_one),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn inspect_v3_zero_for_one_absent_is_false() {
        let result =
            parse_inspect_v3_args(&args(&["--pool", SAMPLE_POOL, "--block", "1", "--amount-in", "1"]));
        match result {
            Ok(InspectV3Command::Run { zero_for_one, .. }) => assert!(!zero_for_one),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    // --- inspect-v3 report formatting ---

    fn sample_inspect_v3_result() -> V3QuoteResult {
        V3QuoteResult {
            amount_in: U256::from(1_000_000_000_000_000_000u128),
            amount_out: U256::from(2_500_000_000u128),
            fee_paid: U256::from(3_000_000_000_000_000u128),
            ending_sqrt_price_x96: U256::from(79_228_162_514_264_337_593_543_950_336u128),
            ending_tick: 42,
            liquidity_after: 10_000_000_000_000_000_000_000u128,
            ticks_crossed: 1,
        }
    }

    #[test]
    fn inspect_v3_report_formatting_is_deterministic() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let result = sample_inspect_v3_result();
        let pool: Address = SAMPLE_POOL.parse().unwrap();
        let token0: Address = "0x4200000000000000000000000000000000000006"
            .parse()
            .unwrap();
        let token1: Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();

        let a = format_inspect_v3_report(
            pool,
            12_345_678,
            token0,
            token1,
            3000,
            60,
            U256::from(79_228_162_514_264_337_593_543_950_336u128),
            30,
            10u128.pow(24),
            &hydrated,
            U256::from(1_000_000_000_000_000_000u128),
            true,
            &result,
        );
        let b = format_inspect_v3_report(
            pool,
            12_345_678,
            token0,
            token1,
            3000,
            60,
            U256::from(79_228_162_514_264_337_593_543_950_336u128),
            30,
            10u128.pow(24),
            &hydrated,
            U256::from(1_000_000_000_000_000_000u128),
            true,
            &result,
        );
        assert_eq!(
            a, b,
            "formatting the same inputs twice must produce identical output"
        );
    }

    #[test]
    fn inspect_v3_report_contains_all_required_fields() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let result = sample_inspect_v3_result();
        let pool: Address = SAMPLE_POOL.parse().unwrap();
        let token0: Address = "0x4200000000000000000000000000000000000006"
            .parse()
            .unwrap();
        let token1: Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();

        let report = format_inspect_v3_report(
            pool,
            12_345_678,
            token0,
            token1,
            3000,
            60,
            U256::from(79_228_162_514_264_337_593_543_950_336u128),
            30,
            10u128.pow(24),
            &hydrated,
            U256::from(1_000_000_000_000_000_000u128),
            true,
            &result,
        );

        assert!(report.contains(&format!("pool={pool}")));
        assert!(report.contains("block=12345678"));
        assert!(report.contains(&format!("token0={token0}")));
        assert!(report.contains(&format!("token1={token1}")));
        assert!(report.contains("fee=3000"));
        assert!(report.contains("tick_spacing=60"));
        assert!(report.contains("sqrt_price_x96=79228162514264337593543950336"));
        assert!(report.contains("current_tick=30"));
        assert!(report.contains(&format!("liquidity={}", 10u128.pow(24))));
        assert!(report.contains("hydrated_tick_lower_bound=-100000"));
        assert!(report.contains("hydrated_tick_upper_bound=100000"));
        assert!(report.contains("initialized_tick_count=0"));
        assert!(report.contains("amount_in=1000000000000000000"));
        assert!(report.contains("direction=zero_for_one"));
        assert!(report.contains("amount_out=2500000000"));
        assert!(report.contains("fee_paid=3000000000000000"));
        assert!(report.contains("ending_sqrt_price_x96=79228162514264337593543950336"));
        assert!(report.contains("ending_tick=42"));
        assert!(report.contains("ticks_crossed=1"));
    }

    #[test]
    fn inspect_v3_report_direction_reflects_zero_for_one_false() {
        let hydrated = HydratedTicks::for_test(BTreeMap::new(), -100_000, 100_000);
        let result = sample_inspect_v3_result();
        let pool: Address = SAMPLE_POOL.parse().unwrap();
        let token0: Address = "0x4200000000000000000000000000000000000006"
            .parse()
            .unwrap();
        let token1: Address = "0x0000000000000000000000000000000000000002"
            .parse()
            .unwrap();

        let report = format_inspect_v3_report(
            pool, 1, token0, token1, 3000, 60, U256::from(1u64), 0, 0, &hydrated,
            U256::from(1u64), false, &result,
        );
        assert!(report.contains("direction=one_for_zero"));
    }

    // --- inspect-arb ---

    const ARB_AERO: &str = "0xcDAC0d6c6C59727a65F871236188350531885C43";
    const ARB_V3: &str = "0xd0b53D9277642d899DF5C87A3966A349A798F224";
    const ARB_WETH: &str = "0x4200000000000000000000000000000000000006";
    const ARB_USDC: &str = "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913";

    fn arb_base_args() -> Vec<&'static str> {
        vec![
            "--aerodrome-pool",
            ARB_AERO,
            "--uniswap-v3-pool",
            ARB_V3,
            "--block",
            "26000000",
            "--token-in",
            ARB_WETH,
            "--amount-in",
            "1000000000000000",
        ]
    }

    fn arb_args_with(extra: &[&'static str]) -> Vec<String> {
        let mut v = arb_base_args();
        v.extend_from_slice(extra);
        args(&v)
    }

    fn arb_args_without(flag: &str) -> Vec<String> {
        let base = arb_base_args();
        let mut out = Vec::new();
        let mut i = 0;
        while i < base.len() {
            if base[i] == flag {
                i += 2;
            } else {
                out.push(base[i].to_string());
                i += 1;
            }
        }
        out
    }

    fn arb_args_replacing(flag: &str, value: &str) -> Vec<String> {
        let mut out = arb_args_without(flag);
        out.push(flag.to_string());
        out.push(value.to_string());
        out
    }

    #[test]
    fn inspect_arb_parses_valid_command_with_default_radius() {
        let parsed = parse_inspect_arb_args(&arb_args_with(&[])).unwrap();
        assert_eq!(
            parsed,
            InspectArbCommand::Run {
                aerodrome_pool: ARB_AERO.parse().unwrap(),
                uniswap_v3_pool: ARB_V3.parse().unwrap(),
                block: 26_000_000,
                token_in: ARB_WETH.parse().unwrap(),
                amount_in: U256::from(1_000_000_000_000_000u64),
                word_radius: INSPECT_V3_WORD_RADIUS,
            }
        );
    }

    #[test]
    fn inspect_arb_parses_explicit_word_radius() {
        match parse_inspect_arb_args(&arb_args_with(&["--word-radius", "3"])).unwrap() {
            InspectArbCommand::Run { word_radius, .. } => assert_eq!(word_radius, 3),
            other => panic!("expected Run, got {other:?}"),
        }
        match parse_inspect_arb_args(&arb_args_with(&["--word-radius", "0"])).unwrap() {
            InspectArbCommand::Run { word_radius, .. } => assert_eq!(word_radius, 0),
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn inspect_arb_help_short_circuits_even_with_bogus_args() {
        assert_eq!(
            parse_inspect_arb_args(&args(&["--help"])),
            Ok(InspectArbCommand::Help)
        );
        assert_eq!(
            parse_inspect_arb_args(&args(&["-h"])),
            Ok(InspectArbCommand::Help)
        );
        assert_eq!(
            parse_inspect_arb_args(&args(&["--bogus", "x", "--amount-in", "0", "--help"])),
            Ok(InspectArbCommand::Help)
        );
    }

    #[test]
    fn inspect_arb_each_missing_required_flag_is_rejected() {
        for flag in [
            "--aerodrome-pool",
            "--uniswap-v3-pool",
            "--block",
            "--token-in",
            "--amount-in",
        ] {
            let err = parse_inspect_arb_args(&arb_args_without(flag)).unwrap_err();
            assert!(err.contains(flag), "flag {flag}: {err}");
            assert!(err.contains("missing required"), "flag {flag}: {err}");
        }
    }

    #[test]
    fn inspect_arb_invalid_addresses_are_rejected() {
        for flag in ["--aerodrome-pool", "--uniswap-v3-pool", "--token-in"] {
            let err =
                parse_inspect_arb_args(&arb_args_replacing(flag, "0xnotanaddress")).unwrap_err();
            assert!(err.contains("invalid") && err.contains(flag), "{flag}: {err}");
        }
    }

    #[test]
    fn inspect_arb_invalid_block_is_rejected() {
        for bad in ["abc", "-1", "1.5", "", "18446744073709551616"] {
            let err = parse_inspect_arb_args(&arb_args_replacing("--block", bad)).unwrap_err();
            assert!(err.contains("invalid --block"), "{bad}: {err}");
        }
    }

    #[test]
    fn inspect_arb_invalid_amount_is_rejected() {
        for bad in ["abc", "-5", "1.5", "1e18", "", "0x10"] {
            let err = parse_inspect_arb_args(&arb_args_replacing("--amount-in", bad)).unwrap_err();
            assert!(err.contains("invalid --amount-in"), "{bad}: {err}");
        }
    }

    #[test]
    fn inspect_arb_zero_amount_is_rejected() {
        for zero in ["0", "000"] {
            let err = parse_inspect_arb_args(&arb_args_replacing("--amount-in", zero)).unwrap_err();
            assert!(err.contains("greater than zero"), "{zero}: {err}");
        }
    }

    #[test]
    fn inspect_arb_negative_or_invalid_word_radius_is_rejected() {
        let err = parse_inspect_arb_args(&arb_args_with(&["--word-radius", "-1"])).unwrap_err();
        assert!(err.contains("non-negative"), "{err}");
        let err = parse_inspect_arb_args(&arb_args_with(&["--word-radius", "x"])).unwrap_err();
        assert!(err.contains("invalid --word-radius"), "{err}");
    }

    #[test]
    fn inspect_arb_unknown_and_dangling_arguments_are_rejected() {
        let err = parse_inspect_arb_args(&arb_args_with(&["--wat", "1"])).unwrap_err();
        assert!(err.contains("unrecognized argument"), "{err}");
        let err = parse_inspect_arb_args(&args(&["--aerodrome-pool"])).unwrap_err();
        assert!(err.contains("requires a value"), "{err}");
    }

    #[test]
    fn inspect_arb_check_chain_id() {
        assert!(check_chain_id(8453, 8453).is_ok());
        let err = check_chain_id(1, 8453).unwrap_err();
        assert!(matches!(err, EngineError::Chain(_)), "got {err:?}");
    }

    #[test]
    fn inspect_arb_pair_validation() {
        let weth: Address = ARB_WETH.parse().unwrap();
        let usdc: Address = ARB_USDC.parse().unwrap();
        let other: Address = "0x0000000000000000000000000000000000000abc".parse().unwrap();
        let a: Address = ARB_AERO.parse().unwrap();
        let v: Address = ARB_V3.parse().unwrap();

        // Same pair, same order and swapped order both accepted.
        assert!(validate_pair_and_direction(a, (weth, usdc), v, (weth, usdc), weth).is_ok());
        assert!(validate_pair_and_direction(a, (weth, usdc), v, (usdc, weth), usdc).is_ok());

        // Different pair rejected, message names both pools.
        let err = validate_pair_and_direction(a, (weth, usdc), v, (weth, other), weth).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains(&a.to_string()) && msg.contains(&v.to_string()), "{msg}");

        // token_in not in the pair rejected.
        let err = validate_pair_and_direction(a, (weth, usdc), v, (weth, usdc), other).unwrap_err();
        assert!(matches!(err, EngineError::State(_)), "got {err:?}");
    }

    // --- inspect-arb report formatting (outcomes come from the real
    // detector, not hand-built; RouteOutcome fields are private) ---

    mod arb_report {
        use super::*;
        use crate::arbitrage::HistoricalQuoteInput;
        use crate::market::models::PoolState;

        const BLOCK: u64 = 26_000_000;

        fn weth() -> Address {
            ARB_WETH.parse().unwrap()
        }
        fn usdc() -> Address {
            ARB_USDC.parse().unwrap()
        }

        fn token(address: Address, symbol: &str, decimals: u8) -> Token {
            Token {
                address,
                symbol: symbol.into(),
                decimals,
            }
        }

        fn aerodrome(pool_address: Address, fee_bps: Option<U256>) -> HistoricalQuoteInput {
            let pool = Pool {
                address: pool_address,
                dex: DexKind::Aerodrome,
                token0: token(weth(), "WETH", 18),
                token1: token(usdc(), "USDC", 6),
                kind: PoolKind::Aerodrome {
                    reserve0: U256::from_str_radix("3607642796485591444113", 10).unwrap(),
                    reserve1: U256::from(9_965_914_277_780u64),
                    stable: false,
                    fee_bps,
                },
            };
            HistoricalQuoteInput::Aerodrome(
                HistoricalAerodromeInput::new(BLOCK, PoolState::new(pool, BLOCK, None)).unwrap(),
            )
        }

        fn v3() -> HistoricalQuoteInput {
            let pool = Pool {
                address: ARB_V3.parse().unwrap(),
                dex: DexKind::UniswapV3,
                token0: token(weth(), "WETH", 18),
                token1: token(usdc(), "USDC", 6),
                kind: PoolKind::ConcentratedLiquidity {
                    fee_tier: 500,
                    tick_spacing: 10,
                    sqrt_price_x96: U256::from_str_radix("4166855781027983759429743", 10).unwrap(),
                    current_tick: -197_069,
                    liquidity: 2_241_227_707_498_366_949u128,
                    initialized_ticks: Default::default(),
                },
            };
            let mut ticks = BTreeMap::new();
            ticks.insert(-197_070, -1_776_514_264_016_992i128);
            let hydrated = HydratedTicks::for_test(ticks, -199_680, -192_010);
            HistoricalQuoteInput::UniswapV3(
                HistoricalUniswapV3Input::new(BLOCK, PoolState::new(pool, BLOCK, None), hydrated)
                    .unwrap(),
            )
        }

        fn header(amount_in: U256) -> InspectArbHeader {
            InspectArbHeader {
                chain_id: 8453,
                block: BLOCK,
                aerodrome_pool: ARB_AERO.parse().unwrap(),
                aerodrome_tokens: (weth(), usdc()),
                uniswap_v3_pool: ARB_V3.parse().unwrap(),
                uniswap_v3_tokens: (weth(), usdc()),
                token_in: weth(),
                amount_in,
                word_radius: 1,
            }
        }

        fn mixed_outcomes(amount_in: U256) -> Vec<RouteOutcome> {
            let bad_pool: Address = "0x0000000000000000000000000000000000000091".parse().unwrap();
            let candidates = vec![
                aerodrome(ARB_AERO.parse().unwrap(), Some(U256::from(30u64))),
                aerodrome(bad_pool, None), // unhydrated fee -> real quote failure
                v3(),
            ];
            detect_cross_dex_opportunities_at_block(BLOCK, weth(), amount_in, &candidates).unwrap()
        }

        #[test]
        fn report_is_deterministic_and_prints_every_outcome() {
            let amount = U256::from(1_000_000_000_000_000u64);
            let outcomes = mixed_outcomes(amount);
            assert_eq!(outcomes.len(), 4);
            let a = format_inspect_arb_report(&header(amount), &outcomes);
            let b = format_inspect_arb_report(&header(amount), &outcomes);
            assert_eq!(a, b);
            assert!(a.contains("routes_evaluated=4"));
            for n in 1..=4 {
                assert!(a.contains(&format!("[route {n}]")), "missing route {n}");
            }
            assert_eq!(a.matches("status=OK").count(), 2);
            assert_eq!(a.matches("status=FAILED").count(), 2);
        }

        #[test]
        fn report_has_header_fields_and_labelled_raw_amounts() {
            let amount = U256::from(1_000_000_000_000_000u64);
            let outcomes = mixed_outcomes(amount);
            let r = format_inspect_arb_report(&header(amount), &outcomes);
            for needle in [
                "chain_id=8453",
                "block=26000000",
                &format!("aerodrome_pool={}", ARB_AERO.parse::<Address>().unwrap()),
                &format!("uniswap_v3_pool={}", ARB_V3.parse::<Address>().unwrap()),
                &format!("aerodrome_token0={}", weth()),
                &format!("aerodrome_token1={}", usdc()),
                &format!("uniswap_v3_token0={}", weth()),
                &format!("uniswap_v3_token1={}", usdc()),
                &format!("token_in={}", weth()),
                "amount_in_raw=1000000000000000",
                "v3_word_radius=1",
                &format!("start_token={}", weth()),
                &format!("intermediate_token={}", usdc()),
                "leg1=aerodrome pool=",
                "leg2=uniswap_v3 pool=",
                "route_block=26000000",
                "input_amount_raw=",
                "intermediate_amount_raw=",
                "final_amount_raw=",
                "gross_profit_raw=",
                &format!("gross_profit_token={}", weth()),
            ] {
                assert!(r.contains(needle), "missing {needle:?} in:\n{r}");
            }
        }

        #[test]
        fn report_preserves_exact_signed_gross_profit_including_negative() {
            let amount = U256::from(1_000_000_000_000_000u64);
            let outcomes = mixed_outcomes(amount);
            let r = format_inspect_arb_report(&header(amount), &outcomes);
            let mut saw_negative = false;
            for outcome in &outcomes {
                if let Some(opp) = outcome.opportunity() {
                    let line = format!("gross_profit_raw={}", opp.gross_profit());
                    assert!(r.contains(&line), "missing {line}");
                    if opp.gross_profit().is_negative() {
                        assert!(line.starts_with("gross_profit_raw=-"));
                        saw_negative = true;
                    }
                }
            }
            assert!(saw_negative, "fixture should contain a negative gross profit");
        }

        #[test]
        fn report_shows_route_failure_with_underlying_error() {
            let amount = U256::from(1_000_000_000_000_000u64);
            let outcomes = mixed_outcomes(amount);
            let r = format_inspect_arb_report(&header(amount), &outcomes);
            assert!(r.contains("status=FAILED kind=Leg"), "{r}");
            assert!(r.contains("error="), "{r}");
            // The failing routes must not suppress the successful ones.
            assert!(r.contains("status=OK"));
        }

        #[test]
        fn report_includes_the_no_gas_no_execution_disclaimer() {
            let amount = U256::from(1_000_000_000_000_000u64);
            let r = format_inspect_arb_report(&header(amount), &mixed_outcomes(amount));
            for needle in [
                "NOT net profit",
                "No gas cost is subtracted.",
                "No execution or settlement cost is subtracted.",
                "not a live opportunity or an executable trading signal",
                "No transaction is signed or submitted.",
            ] {
                assert!(r.contains(needle), "missing {needle:?}");
            }
        }

        #[test]
        fn report_with_no_matching_route_does_not_fabricate_one() {
            let amount = U256::from(1_000u64);
            let other: Address = "0x0000000000000000000000000000000000000abc".parse().unwrap();
            let candidates = vec![aerodrome(ARB_AERO.parse().unwrap(), Some(U256::from(30u64))), v3()];
            let outcomes =
                detect_cross_dex_opportunities_at_block(BLOCK, other, amount, &candidates).unwrap();
            assert!(outcomes.is_empty());
            let r = format_inspect_arb_report(&header(amount), &outcomes);
            assert!(r.contains("routes_evaluated=0"));
            assert!(r.contains("nothing was evaluated"));
            assert!(!r.contains("[route 1]"));
            assert!(!r.contains("status=OK"));
        }
    }
}
