//! HTTP `eth_getLogs` polling.
//!
//! Reusable by both pool-discovery scanning (factory addresses + PoolCreated
//! topics) and known-pool swap scanning (pool addresses + Swap topics) - see
//! `main.rs`. Transport-independent from the strategy/state layer's point of
//! view: this produces raw `RpcLog`s, the same shape the WebSocket path
//! produces, so decoding/dedup/state application code doesn't know or care
//! which transport a log came from.

use crate::config::LogStartBlock;
use crate::error::{EngineError, EngineResult};
use alloy::primitives::{Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::{Filter, Log as RpcLog};

/// How many times a failing range is allowed to be halved before giving up
/// on that sub-range entirely (logged, not fatal - see `fetch_range`).
const MAX_RANGE_REDUCTIONS: u32 = 5;

/// Tracks how far a given scan (discovery or swaps) has progressed, so
/// polling cycles never redundantly rescan already-processed blocks and
/// never silently skip a gap.
#[derive(Debug, Clone, Copy, Default)]
pub struct LogPollCheckpoint {
    last_scanned_block: Option<u64>,
}

impl LogPollCheckpoint {
    pub fn new() -> Self {
        LogPollCheckpoint {
            last_scanned_block: None,
        }
    }

    pub fn last_scanned_block(&self) -> Option<u64> {
        self.last_scanned_block
    }

    /// Compute the next `(from, to)` range to scan, given the current chain
    /// head. Returns `None` if there is nothing new to scan (already caught
    /// up). On first call (no checkpoint yet), `start` decides where
    /// scanning begins - `Latest` means "start from the current head, no
    /// history", matching the Day 2 safety default.
    pub fn next_range(&self, latest_block: u64, start: LogStartBlock) -> Option<(u64, u64)> {
        let from = match self.last_scanned_block {
            Some(last) => last.saturating_add(1),
            None => match start {
                LogStartBlock::Latest => latest_block,
                LogStartBlock::Block(b) => b,
            },
        };
        if from > latest_block {
            None
        } else {
            Some((from, latest_block))
        }
    }

    /// Record that blocks up to and including `to_block` have been scanned.
    pub fn advance(&mut self, to_block: u64) {
        self.last_scanned_block = Some(to_block);
    }
}

/// Split `[from, to]` (inclusive) into chunks of at most `max_range` blocks
/// each. Empty (`from > to`) ranges produce no chunks.
pub fn compute_chunks(from: u64, to: u64, max_range: u64) -> Vec<(u64, u64)> {
    if from > to || max_range == 0 {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut start = from;
    loop {
        let end = start.saturating_add(max_range - 1).min(to);
        chunks.push((start, end));
        if end >= to {
            break;
        }
        start = end + 1;
    }
    chunks
}

/// Given a range that just failed (e.g. the RPC provider rejected it as too
/// large), return a smaller range to retry: the first half of the original.
/// Returns `None` once the range can't be reduced any further (a single
/// block that still fails - nothing left to do but skip it and log).
pub fn reduce_range_on_failure(from: u64, to: u64) -> Option<(u64, u64)> {
    if from >= to {
        return None;
    }
    let mid = from + (to - from) / 2;
    Some((from, mid))
}

pub struct HttpLogPoller {
    rpc_url: String,
    max_block_range: u64,
}

impl HttpLogPoller {
    pub fn new(rpc_url: String, max_block_range: u64) -> Self {
        HttpLogPoller {
            rpc_url,
            max_block_range,
        }
    }

    /// Fetch all logs in `[from, to]` matching `addresses`/`topic0`,
    /// chunking the range and transparently shrinking any chunk that the
    /// provider rejects (e.g. "range too large") until it succeeds or can't
    /// be shrunk further. Never panics or aborts the whole scan because one
    /// sub-range is troublesome - a failed leaf range is logged and
    /// skipped, not silently dropped without a trace.
    pub async fn fetch_logs(
        &self,
        from: u64,
        to: u64,
        addresses: Vec<Address>,
        topic0: B256,
    ) -> EngineResult<Vec<RpcLog>> {
        let mut all_logs = Vec::new();
        for (chunk_from, chunk_to) in compute_chunks(from, to, self.max_block_range) {
            let mut logs = self
                .fetch_range_with_retry(chunk_from, chunk_to, &addresses, topic0, 0)
                .await;
            all_logs.append(&mut logs);
        }
        Ok(all_logs)
    }

    /// Recursive helper: try `[from, to]`; on failure, halve and retry each
    /// half, up to `MAX_RANGE_REDUCTIONS` deep. Returns whatever logs were
    /// successfully collected - partial results on partial failure, not an
    /// all-or-nothing error, since one bad sub-range shouldn't discard logs
    /// we already successfully fetched from the rest of the range.
    fn fetch_range_with_retry<'a>(
        &'a self,
        from: u64,
        to: u64,
        addresses: &'a [Address],
        topic0: B256,
        depth: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<RpcLog>> + Send + 'a>> {
        Box::pin(async move {
            match self.fetch_range(from, to, addresses, topic0).await {
                Ok(logs) => logs,
                Err(err) => {
                    tracing::warn!(
                        source = "http_poll",
                        from_block = from,
                        to_block = to,
                        error = %err,
                        "eth_getLogs failed for range"
                    );

                    if depth >= MAX_RANGE_REDUCTIONS {
                        tracing::error!(
                            source = "http_poll",
                            from_block = from,
                            to_block = to,
                            "giving up on range after max reductions - these blocks will be skipped"
                        );
                        return Vec::new();
                    }

                    match reduce_range_on_failure(from, to) {
                        Some((reduced_from, reduced_to)) => {
                            let mut logs = self
                                .fetch_range_with_retry(
                                    reduced_from,
                                    reduced_to,
                                    addresses,
                                    topic0,
                                    depth + 1,
                                )
                                .await;
                            let mut rest = self
                                .fetch_range_with_retry(
                                    reduced_to + 1,
                                    to,
                                    addresses,
                                    topic0,
                                    depth + 1,
                                )
                                .await;
                            logs.append(&mut rest);
                            logs
                        }
                        None => {
                            tracing::error!(
                                source = "http_poll",
                                block = from,
                                "single block still fails eth_getLogs - skipping"
                            );
                            Vec::new()
                        }
                    }
                }
            }
        })
    }

    async fn fetch_range(
        &self,
        from: u64,
        to: u64,
        addresses: &[Address],
        topic0: B256,
    ) -> EngineResult<Vec<RpcLog>> {
        let url = self
            .rpc_url
            .parse()
            .map_err(|e| EngineError::Chain(format!("invalid BASE_RPC_URL: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);

        let mut filter = Filter::new().from_block(from).to_block(to);
        if !addresses.is_empty() {
            filter = filter.address(addresses.to_vec());
        }
        filter = filter.event_signature(topic0);

        provider
            .get_logs(&filter)
            .await
            .map_err(|e| EngineError::Chain(format!("get_logs failed: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_range_produces_no_chunks() {
        assert_eq!(compute_chunks(100, 50, 1000), Vec::new());
        assert_eq!(compute_chunks(100, 100, 0), Vec::new());
    }

    #[test]
    fn range_within_max_produces_one_chunk() {
        assert_eq!(compute_chunks(100, 200, 1000), vec![(100, 200)]);
    }

    #[test]
    fn range_larger_than_max_is_chunked() {
        let chunks = compute_chunks(0, 2499, 1000);
        assert_eq!(chunks, vec![(0, 999), (1000, 1999), (2000, 2499)]);
    }

    #[test]
    fn exact_multiple_range_chunks_cleanly() {
        let chunks = compute_chunks(0, 1999, 1000);
        assert_eq!(chunks, vec![(0, 999), (1000, 1999)]);
    }

    #[test]
    fn checkpoint_advances_and_computes_next_range() {
        let mut checkpoint = LogPollCheckpoint::new();

        // First call, LogStartBlock::Latest: start exactly at the head, no backfill.
        let range = checkpoint.next_range(1000, LogStartBlock::Latest);
        assert_eq!(range, Some((1000, 1000)));

        checkpoint.advance(1000);
        assert_eq!(checkpoint.last_scanned_block(), Some(1000));

        // Nothing new yet.
        assert_eq!(checkpoint.next_range(1000, LogStartBlock::Latest), None);

        // Chain advanced - next range picks up right after the checkpoint.
        let range = checkpoint.next_range(1050, LogStartBlock::Latest);
        assert_eq!(range, Some((1001, 1050)));
    }

    #[test]
    fn checkpoint_honors_explicit_start_block_on_first_scan() {
        let checkpoint = LogPollCheckpoint::new();
        let range = checkpoint.next_range(5000, LogStartBlock::Block(4000));
        assert_eq!(range, Some((4000, 5000)));
    }

    #[test]
    fn large_range_failure_shrinks_and_eventually_bottoms_out() {
        let (from, to) = (0u64, 10_000u64);
        let (from2, to2) = reduce_range_on_failure(from, to).expect("should shrink");
        assert_eq!(from2, 0);
        assert!(to2 < to, "reduced range must be smaller");

        // Keep shrinking until we hit a single-block range.
        let mut cur = (from2, to2);
        let mut iterations = 0;
        while let Some(next) = reduce_range_on_failure(cur.0, cur.1) {
            cur = next;
            iterations += 1;
            assert!(iterations < 100, "shrinking should converge quickly");
        }
        assert_eq!(cur.0, cur.1, "must bottom out at a single block");

        // A single block that still fails cannot be reduced further.
        assert_eq!(reduce_range_on_failure(cur.0, cur.0), None);
    }
}
