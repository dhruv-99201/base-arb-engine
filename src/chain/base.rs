//! Base L2 connectivity via Alloy, implementing `ChainEventSource`.
//!
//! Treats the RPC/WS endpoint as fully configurable (no hard-coded provider
//! assumptions). WebSocket is optional: if `ws_url` is configured, block and
//! log subscription use a push-based WebSocket stream with reconnect and
//! exponential backoff (unbounded - matches the operator's explicit choice
//! to run against a WebSocket endpoint). If `ws_url` is not configured, the
//! chain source transparently falls back to periodic HTTP polling for the
//! latest block, so the engine keeps running against HTTP-only RPC
//! providers instead of retrying a WebSocket connection that doesn't exist.
//! Every ingestion log line is tagged `source=websocket` or
//! `source=http_poll` so the active mode is unambiguous in logs.

use crate::chain::source::ChainEventSource;
use crate::error::{EngineError, EngineResult};
use crate::market::models::BlockState;
use alloy::primitives::Address;
use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::{Filter, Log as RpcLog};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

const INITIAL_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const CHANNEL_CAPACITY: usize = 256;

/// Which chain-ingestion mode a `BaseChainSource` will use. Determined
/// purely by whether a WebSocket endpoint is configured - see `mode()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainSourceMode {
    WebSocket,
    HttpPoll,
}

impl std::fmt::Display for ChainSourceMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ChainSourceMode::WebSocket => f.write_str("websocket"),
            ChainSourceMode::HttpPoll => f.write_str("http_poll"),
        }
    }
}

#[derive(Clone)]
pub struct BaseChainSource {
    http_url: String,
    ws_url: Option<String>,
    /// Poll interval used only in `ChainSourceMode::HttpPoll`.
    http_poll_interval: Duration,
}

impl BaseChainSource {
    pub fn new(http_url: String, ws_url: Option<String>, http_poll_interval: Duration) -> Self {
        BaseChainSource {
            http_url,
            ws_url,
            http_poll_interval,
        }
    }

    /// Which mode this source will use for block/log ingestion. Pure
    /// function of configuration - safe to call without touching the
    /// network, e.g. for startup logging or tests.
    pub fn mode(&self) -> ChainSourceMode {
        if self.ws_url.is_some() {
            ChainSourceMode::WebSocket
        } else {
            ChainSourceMode::HttpPoll
        }
    }

    async fn http_provider(&self) -> EngineResult<impl Provider + Clone> {
        let url = self
            .http_url
            .parse()
            .map_err(|e| EngineError::Chain(format!("invalid BASE_RPC_URL: {e}")))?;
        Ok(ProviderBuilder::new().connect_http(url))
    }

    /// Runs `connect_and_stream` in a loop, applying exponential backoff
    /// between attempts and resetting the backoff after any period of
    /// successful streaming. Errors are logged, never silently swallowed.
    /// This is WebSocket-only: it is only ever invoked when `ws_url` is
    /// configured, so unbounded retry here reflects the operator's explicit
    /// choice to run against a WebSocket endpoint (see module docs).
    async fn run_with_reconnect<T, F, Fut>(stream_name: &'static str, tx: mpsc::Sender<T>, connect_and_stream: F)
    where
        T: Send + 'static,
        F: Fn(mpsc::Sender<T>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = EngineResult<()>> + Send,
    {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if tx.is_closed() {
                tracing::info!(source = "websocket", stream = stream_name, "receiver dropped, stopping reconnect loop");
                return;
            }

            tracing::info!(source = "websocket", stream = stream_name, "connecting");
            match connect_and_stream(tx.clone()).await {
                Ok(()) => {
                    // Stream ended cleanly (e.g. provider closed it) - treat
                    // as a disconnect and retry with reset backoff, since we
                    // did make forward progress.
                    tracing::warn!(source = "websocket", stream = stream_name, "stream ended, reconnecting");
                    backoff = INITIAL_BACKOFF;
                }
                Err(err) => {
                    tracing::error!(
                        source = "websocket",
                        stream = stream_name,
                        error = %err,
                        backoff_ms = backoff.as_millis() as u64,
                        "connection error, backing off"
                    );
                }
            }

            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
        }
    }

    /// Periodically polls `eth_blockNumber` over HTTP and forwards a
    /// `BlockState` whenever the observed block number advances. Runs until
    /// the receiver is dropped. Individual polling failures (transient RPC
    /// errors, rate limiting, etc.) are logged and retried on the next tick
    /// rather than tearing down the loop - there is no "give up" state here
    /// by design, since HTTP fallback is itself already the fallback.
    async fn run_http_poll(http_url: String, poll_interval: Duration, tx: mpsc::Sender<BlockState>) {
        tracing::info!(
            source = "http_poll",
            poll_interval_secs = poll_interval.as_secs(),
            "entering HTTP fallback mode for block ingestion (no WebSocket endpoint configured)"
        );

        let mut last_seen: Option<u64> = None;
        loop {
            if tx.is_closed() {
                tracing::info!(source = "http_poll", "receiver dropped, stopping poll loop");
                return;
            }

            match Self::poll_latest_block(&http_url).await {
                Ok(number) => {
                    let is_new = last_seen.map(|n| number > n).unwrap_or(true);
                    if is_new {
                        last_seen = Some(number);
                        tracing::info!(source = "http_poll", block = number, "polled latest block");
                        let block = BlockState {
                            number,
                            timestamp: None,
                            hash: None,
                        };
                        if tx.send(block).await.is_err() {
                            return; // receiver dropped
                        }
                    } else {
                        tracing::debug!(source = "http_poll", block = number, "no new block since last poll");
                    }
                }
                Err(err) => {
                    tracing::warn!(source = "http_poll", error = %err, "poll failed, will retry next interval");
                }
            }

            tokio::time::sleep(poll_interval).await;
        }
    }

    async fn poll_latest_block(http_url: &str) -> EngineResult<u64> {
        let url = http_url
            .parse()
            .map_err(|e| EngineError::Chain(format!("invalid BASE_RPC_URL: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        provider
            .get_block_number()
            .await
            .map_err(|e| EngineError::Chain(format!("get_block_number failed: {e}")))
    }
}

#[async_trait]
impl ChainEventSource for BaseChainSource {
    async fn chain_id(&self) -> EngineResult<u64> {
        let provider = self.http_provider().await?;
        let id = provider
            .get_chain_id()
            .await
            .map_err(|e| EngineError::Chain(format!("get_chain_id failed: {e}")))?;
        Ok(id)
    }

    async fn latest_block_number(&self) -> EngineResult<u64> {
        let provider = self.http_provider().await?;
        let number = provider
            .get_block_number()
            .await
            .map_err(|e| EngineError::Chain(format!("get_block_number failed: {e}")))?;
        Ok(number)
    }

    /// WebSocket mode: push-based block subscription with reconnect.
    /// HTTP-poll mode (no `ws_url` configured): periodic `eth_blockNumber`
    /// polling at `http_poll_interval`. Either way this returns immediately
    /// with a live stream backed by a background task - it never blocks
    /// waiting for the first block, and never retries a WebSocket
    /// connection that was never configured in the first place.
    async fn subscribe_blocks(&self) -> EngineResult<BoxStream<'static, BlockState>> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);

        match self.ws_url.clone() {
            Some(ws_url) => {
                tokio::spawn(BaseChainSource::run_with_reconnect(
                    "blocks",
                    tx,
                    move |tx: mpsc::Sender<BlockState>| {
                        let ws_url = ws_url.clone();
                        async move {
                            let provider = ProviderBuilder::new()
                                .connect_ws(WsConnect::new(ws_url))
                                .await
                                .map_err(|e| EngineError::Chain(format!("ws connect failed: {e}")))?;

                            let sub = provider
                                .subscribe_blocks()
                                .await
                                .map_err(|e| EngineError::Chain(format!("subscribe_blocks failed: {e}")))?;

                            let mut stream = sub.into_stream();
                            while let Some(header) = stream.next().await {
                                let block = BlockState {
                                    number: header.number,
                                    timestamp: Some(header.timestamp),
                                    hash: Some(header.hash),
                                };
                                if tx.send(block).await.is_err() {
                                    break; // receiver dropped
                                }
                            }
                            Ok(())
                        }
                    },
                ));
            }
            None => {
                let http_url = self.http_url.clone();
                let poll_interval = self.http_poll_interval;
                tokio::spawn(BaseChainSource::run_http_poll(http_url, poll_interval, tx));
            }
        }

        Ok(ReceiverStream::new(rx).boxed())
    }

    /// Log subscription remains WebSocket-only: HTTP-poll mode has no log
    /// ingestion path today (only latest-block polling, per Day 1 scope).
    /// Callers should check `mode()` before calling this and skip log
    /// ingestion entirely in `ChainSourceMode::HttpPoll`, the same way
    /// `main.rs` already skips it when there are no watched pool addresses.
    async fn subscribe_logs(
        &self,
        addresses: Vec<Address>,
    ) -> EngineResult<BoxStream<'static, RpcLog>> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);

        let ws_url = self.ws_url.clone().ok_or_else(|| {
            EngineError::Chain(
                "BASE_WS_URL is not configured; log subscription requires a WebSocket endpoint \
                 (HTTP-poll mode only supports latest-block polling)"
                    .to_string(),
            )
        })?;

        tokio::spawn(BaseChainSource::run_with_reconnect(
            "logs",
            tx,
            move |tx: mpsc::Sender<RpcLog>| {
                let ws_url = ws_url.clone();
                let addresses = addresses.clone();
                async move {
                    let provider = ProviderBuilder::new()
                        .connect_ws(WsConnect::new(ws_url))
                        .await
                        .map_err(|e| EngineError::Chain(format!("ws connect failed: {e}")))?;

                    let mut filter = Filter::new();
                    if !addresses.is_empty() {
                        filter = filter.address(addresses);
                    }

                    let sub = provider
                        .subscribe_logs(&filter)
                        .await
                        .map_err(|e| EngineError::Chain(format!("subscribe_logs failed: {e}")))?;

                    let mut stream = sub.into_stream();
                    while let Some(log) = stream.next().await {
                        if tx.send(log).await.is_err() {
                            break;
                        }
                    }
                    Ok(())
                }
            },
        ));

        Ok(ReceiverStream::new(rx).boxed())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::MarketState;

    #[test]
    fn empty_or_unset_ws_url_selects_http_fallback_mode() {
        let source = BaseChainSource::new(
            "http://127.0.0.1:1".to_string(),
            None,
            Duration::from_secs(5),
        );
        assert_eq!(source.mode(), ChainSourceMode::HttpPoll);
    }

    #[test]
    fn configured_ws_url_selects_websocket_mode() {
        let source = BaseChainSource::new(
            "http://127.0.0.1:1".to_string(),
            Some("wss://example.invalid".to_string()),
            Duration::from_secs(5),
        );
        assert_eq!(source.mode(), ChainSourceMode::WebSocket);
    }

    #[tokio::test]
    async fn fallback_mode_starts_without_a_websocket_endpoint() {
        // Port 1 is a reserved, effectively-unroutable port: any connection
        // attempt fails fast without needing real network access, which is
        // exactly what we want to exercise here - `subscribe_blocks()` must
        // return `Ok(stream)` immediately in HTTP-poll mode regardless of
        // whether the underlying HTTP polling succeeds in the background.
        let source = BaseChainSource::new(
            "http://127.0.0.1:1".to_string(),
            None,
            Duration::from_millis(50),
        );

        let result = source.subscribe_blocks().await;
        assert!(
            result.is_ok(),
            "HTTP-poll mode must start without a WebSocket endpoint, not error or block"
        );
        // Drop the stream promptly so the background poll task observes a
        // closed receiver and exits instead of polling forever in the test
        // process.
        drop(result);
    }

    #[test]
    fn http_poll_updates_market_state_monotonically() {
        // Mirrors exactly what `run_http_poll` does with each observed
        // block number: feed it into `MarketState::update_latest_block` and
        // rely on that method's own monotonicity guarantee. Out-of-order or
        // stale numbers (as could arrive from retries/races) must never
        // move `latest_block` backwards.
        let mut state = MarketState::new();
        for number in [100u64, 101, 103, 102, 103, 110] {
            state.update_latest_block(BlockState {
                number,
                timestamp: None,
                hash: None,
            });
        }
        assert_eq!(state.latest_block.unwrap().number, 110);
    }
}