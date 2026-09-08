//! Base L2 connectivity via Alloy, implementing `ChainEventSource`.
//!
//! Treats the RPC/WS endpoint as fully configurable (no hard-coded provider
//! assumptions). Handles reconnect with exponential backoff for both the
//! block subscription and the log subscription independently, so a failure
//! in one does not take down the other.

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

#[derive(Clone)]
pub struct BaseChainSource {
    http_url: String,
    ws_url: Option<String>,
}

impl BaseChainSource {
    pub fn new(http_url: String, ws_url: Option<String>) -> Self {
        BaseChainSource { http_url, ws_url }
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
    async fn run_with_reconnect<T, F, Fut>(label: &'static str, tx: mpsc::Sender<T>, connect_and_stream: F)
    where
        T: Send + 'static,
        F: Fn(mpsc::Sender<T>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = EngineResult<()>> + Send,
    {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            if tx.is_closed() {
                tracing::info!(source = label, "receiver dropped, stopping reconnect loop");
                return;
            }

            tracing::info!(source = label, "connecting");
            match connect_and_stream(tx.clone()).await {
                Ok(()) => {
                    // Stream ended cleanly (e.g. provider closed it) - treat
                    // as a disconnect and retry with reset backoff, since we
                    // did make forward progress.
                    tracing::warn!(source = label, "stream ended, reconnecting");
                    backoff = INITIAL_BACKOFF;
                }
                Err(err) => {
                    tracing::error!(source = label, error = %err, backoff_ms = backoff.as_millis() as u64, "connection error, backing off");
                }
            }

            if tx.is_closed() {
                return;
            }
            tokio::time::sleep(backoff).await;
            backoff = std::cmp::min(backoff * 2, MAX_BACKOFF);
        }
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

    async fn subscribe_blocks(&self) -> EngineResult<BoxStream<'static, BlockState>> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);

        let ws_url = self.ws_url.clone().ok_or_else(|| {
            EngineError::Chain(
                "BASE_WS_URL is not configured; block subscription requires a WebSocket endpoint"
                    .to_string(),
            )
        })?;

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

        Ok(ReceiverStream::new(rx).boxed())
    }

    async fn subscribe_logs(
        &self,
        addresses: Vec<Address>,
    ) -> EngineResult<BoxStream<'static, RpcLog>> {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);

        let ws_url = self.ws_url.clone().ok_or_else(|| {
            EngineError::Chain(
                "BASE_WS_URL is not configured; log subscription requires a WebSocket endpoint"
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
