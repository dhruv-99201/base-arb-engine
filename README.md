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
  - Aerodrome Slipstream (`CLFactory.PoolCreated`) - addresses and event
    shape verified from source (see "Known limitations" below); real
    historical decoding **not yet confirmed** - run `discover-test`
    against a verified Slipstream block to check. Enabled by default
    across three verified factory generations (Initial, Gauge Caps, Gauges
    V3) - see `AERODROME_SLIPSTREAM_FACTORY_ADDRESSES` in `.env.example`.
    Pools created under earlier generations are never migrated, so all
    three stay relevant, not just the newest.
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
  `AERODROME_SLIPSTREAM_FACTORY_ADDRESSES` (comma-separated list, defaults
  to three verified factory generations), `LOG_POLL_MAX_BLOCK_RANGE`,
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

- **Aerodrome Slipstream (`CLFactory`) factory addresses on Base**: three
  generations verified against the official `aerodrome-finance/slipstream`
  GitHub README ("Deployments" section) and cross-checked against
  `CLFactory.sol`'s own source (each factory chains to its predecessor via
  an immutable `legacyCLFactory` reference, confirming old pools are never
  migrated and all three generations stay live/relevant). Two of the three
  were independently corroborated by a third-party MEV/router codebase's
  hardcoded fork-detection constants. A specific real historical
  `PoolCreated` transaction for these factories was not located to test
  against - run `discover-test` yourself against a block range you've
  confirmed via BaseScan's "Events" tab on one of the three factory
  addresses to verify decoding end-to-end.
- **Aerodrome Slipstream `PoolCreated` indexed/non-indexed parameter
  split** (`token0`/`token1`/`tickSpacing` indexed, `pool` not) is
  consistent with the confirmed `emit PoolCreated(token0, token1,
  tickSpacing, pool)` call in `CLFactory.sol`, the pattern both Uniswap
  V3's and Aerodrome classic's analogous events follow, and `CLFactory`'s
  own `getPool[token0][token1][tickSpacing]` lookup mapping - not
  confirmed against `ICLFactory.sol`'s literal interface text or against a
  real decoded historical log yet.
- **Aerodrome Slipstream `Swap` event shape** is assumed identical to
  Uniswap V3's (`sender, recipient, amount0, amount1, sqrtPriceX96,
  liquidity, tick`), based on Slipstream's documented lineage ("adapted
  from Uniswap V3's core contracts") - not independently confirmed from
  `CLPool`'s full event declarations.
- The HTTP `eth_getLogs` retry/range-reduction logic (`reduce_range_on_failure`,
  chunking) is tested as pure logic (no network in this sandbox) - the
  actual RPC-calling code path (`HttpLogPoller::fetch_range`) is
  unverified against a real provider's oversized-range error response.
