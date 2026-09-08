//! Structured logging setup and ingestion telemetry helpers.

use crate::events::model::MarketEvent;
use tracing_subscriber::EnvFilter;

/// Initialize the global `tracing` subscriber. `log_level` is used as the
/// default filter directive when `RUST_LOG` is not set, so `LOG_LEVEL` from
/// config still works out of the box.
pub fn init_tracing(log_level: &str) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .init();
}

/// Emit a structured `event_received` log line for a normalized event,
/// including full ingestion-latency telemetry. Call this once per accepted
/// (non-duplicate) event.
pub fn log_event_received(event: &MarketEvent) {
    tracing::info!(
        event = "event_received",
        dex = %event.dex,
        pool = %event.pool_address,
        block = event.block_number,
        tx = %event.tx_hash,
        log_index = event.log_index,
        processing_latency_us = event.processing_latency_us,
        "event_received"
    );
}

/// Emit a structured log line for a duplicate event that was safely ignored.
pub fn log_event_duplicate(event: &MarketEvent) {
    tracing::debug!(
        event = "event_duplicate",
        dex = %event.dex,
        pool = %event.pool_address,
        tx = %event.tx_hash,
        log_index = event.log_index,
        "duplicate event ignored"
    );
}
