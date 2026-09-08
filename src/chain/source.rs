//! Chain event source abstraction.
//!
//! Strategy/state code depends on this trait, never on a concrete transport.
//! Today's implementation (`chain::base::BaseChainSource`) uses the
//! configured Base WebSocket/HTTP RPC. A future low-latency feed (a native
//! ~200ms-block source, a dedicated sequencer feed, etc.) can be dropped in
//! by implementing this trait, with zero changes to `events`/`market` code.

use crate::error::EngineResult;
use crate::market::models::BlockState;
use alloy::primitives::Address;
use alloy::rpc::types::Log as RpcLog;
use async_trait::async_trait;
use futures::stream::BoxStream;

#[async_trait]
pub trait ChainEventSource: Send + Sync {
    /// The chain ID reported by the connected node.
    async fn chain_id(&self) -> EngineResult<u64>;

    /// The latest block number known to the connected node.
    async fn latest_block_number(&self) -> EngineResult<u64>;

    /// A stream of new block headers as they arrive. Implementations are
    /// responsible for their own reconnect/backoff behavior; the stream
    /// should keep producing items across transient disconnects rather than
    /// terminating.
    async fn subscribe_blocks(&self) -> EngineResult<BoxStream<'static, BlockState>>;

    /// A stream of raw logs matching `addresses` (any topic). Same
    /// reconnect contract as `subscribe_blocks`.
    async fn subscribe_logs(
        &self,
        addresses: Vec<Address>,
    ) -> EngineResult<BoxStream<'static, RpcLog>>;
}
