//! Decodes raw chain logs into normalized `MarketEvent`s.
//!
//! Decoding is protocol-specific (Aerodrome's Solidly-style Swap event vs
//! Uniswap V3's concentrated-liquidity Swap event have different shapes),
//! but the *output* is always the same normalized `MarketEvent`. Malformed
//! logs are rejected with a clear error rather than silently dropped or
//! guessed at.

use crate::error::{EngineError, EngineResult};
use crate::events::model::{EventKind, MarketEvent, SwapEvent};
use crate::market::models::DexKind;
use alloy::primitives::Log as PrimitiveLog;
use alloy::rpc::types::Log as RpcLog;
use alloy::sol_types::SolEvent;

// IMPORTANT: `alloy::sol!` computes each event's on-chain signature hash
// (topic0) from the literal event name declared here - NOT from the Rust
// item/module name. `AerodromeSwap`/`UniswapV3Swap` (the names used prior
// to this fix) therefore hashed to `keccak256("AerodromeSwap(...)")` /
// `keccak256("UniswapV3Swap(...)")`, which never matches any real on-chain
// log (the actual Solidity event is just `Swap` in both cases). Each event
// is wrapped in its own private module here, both literally named `Swap`,
// so the *signature* is correct while the Rust bindings stay distinct
// (`aerodrome_swap_event::Swap` vs `uniswap_v3_swap_event::Swap`) without a
// name collision in this file. See also `dex::discovery::*`, which had the
// exact same bug for `PoolCreated` and is fixed the same way (those events
// each live in their own file/module already, so no wrapper was needed
// there - just the identifier itself was corrected).
mod aerodrome_swap_event {
    use alloy::sol;
    sol! {
        /// Aerodrome (Solidly-fork) pool Swap event.
        event Swap(
            address indexed sender,
            address indexed to,
            uint256 amount0In,
            uint256 amount1In,
            uint256 amount0Out,
            uint256 amount1Out
        );
    }
}

mod uniswap_v3_swap_event {
    use alloy::sol;
    sol! {
        /// Uniswap V3 pool Swap event.
        event Swap(
            address indexed sender,
            address indexed recipient,
            int256 amount0,
            int256 amount1,
            uint160 sqrtPriceX96,
            uint128 liquidity,
            int24 tick
        );
    }
}

use aerodrome_swap_event::Swap as AerodromeSwap;
use uniswap_v3_swap_event::Swap as UniswapV3Swap;

/// Current wall-clock time in microseconds since the Unix epoch.
pub fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// keccak256 topic0 for `AerodromeSwap`. Exposed so the Day 2 log-polling
/// pipeline can filter `eth_getLogs` queries to just this event without
/// duplicating the event definition.
pub fn aerodrome_swap_topic0() -> alloy::primitives::B256 {
    AerodromeSwap::SIGNATURE_HASH
}

/// keccak256 topic0 for `UniswapV3Swap`. Also used for Aerodrome Slipstream
/// (`CLPool`) swap scanning, which reuses this event shape - see
/// `dex::aerodrome_slipstream` module docs.
pub fn uniswap_v3_swap_topic0() -> alloy::primitives::B256 {
    UniswapV3Swap::SIGNATURE_HASH
}

/// Decode a raw RPC log from a known Aerodrome pool into a `MarketEvent`.
///
/// `received_at_us` should be captured by the caller at the moment the log
/// was received from the transport, before any decoding work happens, so
/// `processing_latency_us` reflects actual decode+normalize cost.
pub fn decode_aerodrome_log(
    log: &RpcLog,
    chain_id: u64,
    received_at_us: u64,
) -> EngineResult<MarketEvent> {
    let processing_started_at_us = now_us();

    let block_number = log
        .block_number
        .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;
    let log_index = log
        .log_index
        .ok_or_else(|| EngineError::MalformedEvent("log missing log_index".into()))?;
    let pool_address = log.inner.address;

    let decoded_log = AerodromeSwap::decode_log(&log.inner).map_err(|e| {
        EngineError::Decode(format!("failed to decode AerodromeSwap log: {e}"))
    })?;
    let decoded = decoded_log.data;

    // Solidly-style pools report gross in/out per side rather than a single
    // signed net amount; normalize to net-into-pool the same way Uniswap V3
    // does, so downstream code has one shape to reason about.
    let amount0_in = i128::try_from(decoded.amount0In)
        .map_err(|_| EngineError::MalformedEvent("amount0In overflows i128".into()))?;
    let amount0_out = i128::try_from(decoded.amount0Out)
        .map_err(|_| EngineError::MalformedEvent("amount0Out overflows i128".into()))?;
    let amount1_in = i128::try_from(decoded.amount1In)
        .map_err(|_| EngineError::MalformedEvent("amount1In overflows i128".into()))?;
    let amount1_out = i128::try_from(decoded.amount1Out)
        .map_err(|_| EngineError::MalformedEvent("amount1Out overflows i128".into()))?;

    let amount0 = amount0_in
        .checked_sub(amount0_out)
        .ok_or_else(|| EngineError::Arithmetic("amount0 net overflow".into()))?;
    let amount1 = amount1_in
        .checked_sub(amount1_out)
        .ok_or_else(|| EngineError::Arithmetic("amount1 net overflow".into()))?;

    let swap = SwapEvent {
        pool_address,
        dex: DexKind::Aerodrome,
        amount0,
        amount1,
        sender: Some(decoded.sender),
        recipient: Some(decoded.to),
    };

    let processing_finished_at_us = now_us();

    Ok(MarketEvent::new(
        chain_id,
        block_number,
        None,
        tx_hash,
        log_index,
        pool_address,
        DexKind::Aerodrome,
        EventKind::Swap(swap),
        received_at_us,
        processing_started_at_us,
        processing_finished_at_us,
    ))
}

/// Decode a raw RPC log from a known Uniswap V3 pool into a `MarketEvent`.
pub fn decode_uniswap_v3_log(
    log: &RpcLog,
    chain_id: u64,
    received_at_us: u64,
) -> EngineResult<MarketEvent> {
    let processing_started_at_us = now_us();

    let block_number = log
        .block_number
        .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;
    let log_index = log
        .log_index
        .ok_or_else(|| EngineError::MalformedEvent("log missing log_index".into()))?;
    let pool_address = log.inner.address;

    let decoded_log = UniswapV3Swap::decode_log(&log.inner).map_err(|e| {
        EngineError::Decode(format!("failed to decode UniswapV3Swap log: {e}"))
    })?;
    let decoded = decoded_log.data;

    let amount0 = i128::try_from(decoded.amount0)
        .map_err(|_| EngineError::MalformedEvent("amount0 overflows i128".into()))?;
    let amount1 = i128::try_from(decoded.amount1)
        .map_err(|_| EngineError::MalformedEvent("amount1 overflows i128".into()))?;

    let swap = SwapEvent {
        pool_address,
        dex: DexKind::UniswapV3,
        amount0,
        amount1,
        sender: Some(decoded.sender),
        recipient: Some(decoded.recipient),
    };

    let processing_finished_at_us = now_us();

    Ok(MarketEvent::new(
        chain_id,
        block_number,
        None,
        tx_hash,
        log_index,
        pool_address,
        DexKind::UniswapV3,
        EventKind::Swap(swap),
        received_at_us,
        processing_started_at_us,
        processing_finished_at_us,
    ))
}

/// Decode a raw RPC log from a known Aerodrome Slipstream (`CLPool`) pool.
///
/// Reuses the `UniswapV3Swap` event shape: Slipstream's `Swap` event is
/// documented as "adapted from Uniswap V3's core contracts", and shares the
/// same `(sender, recipient, amount0, amount1, sqrtPriceX96, liquidity,
/// tick)` signature in every source reviewed for this implementation. If
/// real Slipstream swap logs fail to decode against this shape, that
/// assumption is the first thing to re-verify (see
/// `dex::aerodrome_slipstream` module docs for the same caveat on the
/// discovery event).
pub fn decode_aerodrome_slipstream_log(
    log: &RpcLog,
    chain_id: u64,
    received_at_us: u64,
) -> EngineResult<MarketEvent> {
    let processing_started_at_us = now_us();

    let block_number = log
        .block_number
        .ok_or_else(|| EngineError::MalformedEvent("log missing block_number".into()))?;
    let tx_hash = log
        .transaction_hash
        .ok_or_else(|| EngineError::MalformedEvent("log missing transaction_hash".into()))?;
    let log_index = log
        .log_index
        .ok_or_else(|| EngineError::MalformedEvent("log missing log_index".into()))?;
    let pool_address = log.inner.address;

    let decoded_log = UniswapV3Swap::decode_log(&log.inner).map_err(|e| {
        EngineError::Decode(format!("failed to decode Slipstream Swap log: {e}"))
    })?;
    let decoded = decoded_log.data;

    let amount0 = i128::try_from(decoded.amount0)
        .map_err(|_| EngineError::MalformedEvent("amount0 overflows i128".into()))?;
    let amount1 = i128::try_from(decoded.amount1)
        .map_err(|_| EngineError::MalformedEvent("amount1 overflows i128".into()))?;

    let swap = SwapEvent {
        pool_address,
        dex: DexKind::AerodromeSlipstream,
        amount0,
        amount1,
        sender: Some(decoded.sender),
        recipient: Some(decoded.recipient),
    };

    let processing_finished_at_us = now_us();

    Ok(MarketEvent::new(
        chain_id,
        block_number,
        None,
        tx_hash,
        log_index,
        pool_address,
        DexKind::AerodromeSlipstream,
        EventKind::Swap(swap),
        received_at_us,
        processing_started_at_us,
        processing_finished_at_us,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, Address, Bytes, B256};
    use alloy::rpc::types::Log as RpcLog;
    use alloy::sol_types::SolEvent;

    fn build_log(topics: Vec<B256>, data: Bytes, addr: Address) -> RpcLog {
        let inner = PrimitiveLog::new_unchecked(addr, topics, data);
        RpcLog {
            inner,
            block_hash: None,
            block_number: Some(12_345_678),
            block_timestamp: None,
            transaction_hash: Some(B256::repeat_byte(0xAB)),
            transaction_index: Some(0),
            log_index: Some(3),
            removed: false,
        }
    }

    #[test]
    fn valid_uniswap_v3_swap_decodes() {
        let pool = address!("4200000000000000000000000000000000000006");
        let sender = address!("1111111111111111111111111111111111111111");
        let recipient = address!("2222222222222222222222222222222222222222");

        let event = UniswapV3Swap {
            sender,
            recipient,
            amount0: alloy::primitives::I256::try_from(1_000_000_i64).unwrap(),
            amount1: alloy::primitives::I256::try_from(-2_000_000_i64).unwrap(),
            sqrtPriceX96: alloy::primitives::U160::from(79_228_162_514_264_337_593_543_950_336u128),
            liquidity: 123_456_789_u128,
            tick: alloy::primitives::aliases::I24::try_from(-1234i32).unwrap(),
        };
        let encoded = event.encode_log_data();
        let log = build_log(encoded.topics().to_vec(), encoded.data.clone(), pool);

        let market_event = decode_uniswap_v3_log(&log, 8453, now_us()).expect("should decode");
        match market_event.kind {
            EventKind::Swap(swap) => {
                assert_eq!(swap.amount0, 1_000_000);
                assert_eq!(swap.amount1, -2_000_000);
                assert_eq!(swap.dex, DexKind::UniswapV3);
                assert_eq!(swap.sender, Some(sender));
                assert_eq!(swap.recipient, Some(recipient));
            }
            other => panic!("expected Swap, got {other:?}"),
        }
    }

    #[test]
    fn malformed_log_is_rejected() {
        let pool = address!("4200000000000000000000000000000000000006");
        // Wrong topic0 (event signature) - decoder must reject, not guess.
        let bogus_topic = B256::repeat_byte(0x11);
        let log = build_log(vec![bogus_topic], Bytes::new(), pool);

        let result = decode_uniswap_v3_log(&log, 8453, now_us());
        assert!(result.is_err(), "malformed/mismatched log must be rejected");
    }

    #[test]
    fn log_missing_block_number_is_rejected() {
        let pool = address!("4200000000000000000000000000000000000006");
        let event = UniswapV3Swap {
            sender: Address::ZERO,
            recipient: Address::ZERO,
            amount0: alloy::primitives::I256::ZERO,
            amount1: alloy::primitives::I256::ZERO,
            sqrtPriceX96: alloy::primitives::U160::ZERO,
            liquidity: 0,
            tick: alloy::primitives::aliases::I24::ZERO,
        };
        let encoded = event.encode_log_data();
        let mut log = build_log(encoded.topics().to_vec(), encoded.data.clone(), pool);
        log.block_number = None;

        let result = decode_uniswap_v3_log(&log, 8453, now_us());
        assert!(matches!(result, Err(EngineError::MalformedEvent(_))));
    }
}
