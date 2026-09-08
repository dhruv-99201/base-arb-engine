use thiserror::Error;

/// Top-level error type for the Base arbitrage engine.
///
/// Every fallible boundary in the system (config, chain I/O, event decoding,
/// state transitions, DEX adapters) should ultimately produce one of these
/// variants so callers can match on failure category without downcasting.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("chain connection error: {0}")]
    Chain(String),

    #[error("rpc error: {0}")]
    Rpc(#[from] alloy::transports::TransportError),

    #[error("event decode error: {0}")]
    Decode(String),

    #[error("malformed event: {0}")]
    MalformedEvent(String),

    #[error("dex adapter not implemented: {0}")]
    NotImplemented(String),

    #[error("dex adapter error ({dex}): {reason}")]
    Dex { dex: String, reason: String },

    #[error("state error: {0}")]
    State(String),

    #[error("arithmetic overflow/underflow: {0}")]
    Arithmetic(String),

    #[error("unsafe operation blocked: {0}")]
    UnsafeOperation(String),

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type EngineResult<T> = Result<T, EngineError>;
