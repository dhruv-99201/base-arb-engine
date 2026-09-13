use crate::error::{EngineError, EngineResult};
use alloy::primitives::Address;
use std::str::FromStr;

/// Uniswap V3 canonical factory on Base, per Uniswap's official deployments
/// page (developers.uniswap.org/docs/protocols/v3/deployments/v3-base-deployments).
/// Overridable via `UNISWAP_V3_FACTORY_ADDRESS` - never trust this blindly
/// for chains other than Base mainnet (8453).
const DEFAULT_UNISWAP_V3_FACTORY: &str = "0x33128a8fC17869897dcE68Ed026d694621f6FDfD";

/// Aerodrome classic (Solidly-style) PoolFactory on Base. Verified against
/// the contract's own source on BaseScan (labeled "Aerodrome: Pool Factory",
/// address 0x420DD381b31aEf6683db6B902084cB0FFECe40Da, actively creating
/// pools as of this writing). Overridable via `AERODROME_FACTORY_ADDRESS`.
pub(crate) const DEFAULT_AERODROME_FACTORY: &str = "0x420DD381b31aEf6683db6B902084cB0FFECe40Da";

/// Aerodrome Slipstream (concentrated-liquidity) `CLFactory` deployments on
/// Base. Verified against the official deployment table in the
/// `aerodrome-finance/slipstream` GitHub README ("Deployments" section) and
/// cross-checked against `CLFactory.sol`'s own source, which chains each
/// factory to its predecessor via an immutable `legacyCLFactory` reference
/// (`constructor(address _voter, address _clFactory, address
/// _poolImplementation)`) - confirming pools created under earlier
/// factories are never migrated and remain live, so all generations stay
/// relevant for discovery, not just the newest:
///
/// 1. Initial deployment
/// 2. Gauge Caps deployment
/// 3. Gauges V3 deployment (current/latest, per the README as of this
///    writing)
///
/// Independently corroborated by a third-party MEV/router codebase's
/// `univ3forks/AerodromeSlipstream.sol` constants, which list the same
/// first two addresses. Overridable/extendable via the comma-separated
/// `AERODROME_SLIPSTREAM_FACTORY_ADDRESSES` (plural) - set it to an empty
/// string to disable Slipstream discovery entirely.
const DEFAULT_AERODROME_SLIPSTREAM_FACTORIES: &[&str] = &[
    "0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A",
    "0xaDe65c38CD4849aDBA595a4323a8C7DdfE89716a",
    "0xf8f2eB4940CFE7d13603DDDD87f123820Fc061Ef",
];

/// Where the HTTP log poller should begin pool-discovery/swap scanning on
/// first startup (i.e. when no checkpoint exists yet). Never defaults to
/// scanning full chain history - that's a deliberate safety default per the
/// Day 2 spec ("do not automatically scan the entire history of Base").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStartBlock {
    /// Start from the current chain head - the safe default. No historical
    /// backfill.
    Latest,
    /// Start from an explicit, operator-chosen block number (controlled
    /// backfill).
    Block(u64),
}

impl FromStr for LogStartBlock {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("latest") {
            return Ok(LogStartBlock::Latest);
        }
        trimmed
            .parse::<u64>()
            .map(LogStartBlock::Block)
            .map_err(|_| {
                EngineError::Config(format!(
                    "invalid LOG_START_BLOCK '{trimmed}': expected 'latest' or a block number"
                ))
            })
    }
}

/// Execution mode gates what the engine is *allowed* to do at runtime.
///
/// Day 1 only ever runs in `DryRun`. `Simulation` and `Live` are defined now
/// so later days can extend the same config surface, but there is
/// deliberately no code path today that reads `Live` and does anything
/// other than refuse to proceed with trade execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    #[default]
    DryRun,
    Simulation,
    Live,
}

impl FromStr for ExecutionMode {
    type Err = EngineError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "DRY_RUN" | "DRYRUN" | "" => Ok(ExecutionMode::DryRun),
            "SIMULATION" | "SIM" => Ok(ExecutionMode::Simulation),
            "LIVE" => Ok(ExecutionMode::Live),
            other => Err(EngineError::Config(format!(
                "invalid EXECUTION_MODE '{other}': expected DRY_RUN | SIMULATION | LIVE"
            ))),
        }
    }
}

impl ExecutionMode {
    /// Day 1 hard rule: there is no trade path at all, regardless of mode.
    /// This function exists so any future execution entry point has a single,
    /// obvious place to check before doing anything irreversible.
    pub fn can_execute_trades(&self) -> bool {
        // Intentionally always false today. Day 1 acceptance criteria requires
        // that LIVE mode has no working trade path. When execution is built
        // (later days), this should still require explicit, separate
        // confirmation beyond just `self == Live`.
        false
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub base_rpc_url: String,
    pub base_ws_url: Option<String>,
    pub base_chain_id: u64,
    pub log_level: String,
    pub execution_mode: ExecutionMode,

    /// Optional, explicit pool addresses. Left unset unless the operator
    /// supplies verified addresses - see README "Not implemented yet" /
    /// DEX adapter docs. We never invent these.
    pub aerodrome_pool_address: Option<String>,
    pub uniswap_v3_pool_address: Option<String>,

    /// How often the HTTP-fallback chain source polls for the latest block
    /// when no WebSocket endpoint is configured (or WebSocket is otherwise
    /// unavailable). Only used in HTTP-poll mode - ignored in WebSocket
    /// mode, which is push-based.
    pub http_poll_interval: std::time::Duration,

    // --- Day 2: pool discovery / log polling ---
    /// Uniswap V3 factory address to watch for `PoolCreated` events.
    pub uniswap_v3_factory_address: Address,
    /// Aerodrome classic (Solidly-style) factory address to watch for
    /// `PoolCreated` events.
    pub aerodrome_factory_address: Address,
    /// Aerodrome Slipstream (concentrated-liquidity) factory addresses -
    /// plural, since multiple factory generations remain simultaneously
    /// live (see `DEFAULT_AERODROME_SLIPSTREAM_FACTORIES` docs above).
    /// Empty means Slipstream discovery is skipped entirely.
    pub aerodrome_slipstream_factory_addresses: Vec<Address>,
    /// Maximum block range per `eth_getLogs` call. Chunked to stay under
    /// RPC-provider limits.
    pub log_poll_max_block_range: u64,
    /// Where to start scanning from when no checkpoint exists yet.
    pub log_start_block: LogStartBlock,
}

impl Config {
    /// Load configuration from environment variables (via `.env` if present).
    pub fn load() -> EngineResult<Self> {
        // Loading .env is best-effort: it's fine if it doesn't exist (e.g. in
        // containers where env vars are injected directly).
        let _ = dotenvy::dotenv();
        Self::load_from_env()
    }

    /// Loads config purely from whatever is already in the process
    /// environment, without touching any `.env` file on disk.
    ///
    /// This split exists because `dotenvy::dotenv()` only sets a variable if
    /// it isn't already set - so in tests that `remove_var` a variable to
    /// simulate it being missing, `dotenv()` would silently reload it from a
    /// developer's real local `.env` file (which is expected to contain real
    /// values for `cargo run`) and defeat the test. Tests call this
    /// directly; `main.rs` goes through `load()`.
    fn load_from_env() -> EngineResult<Self> {
        let base_rpc_url = require_env("BASE_RPC_URL")?;
        let base_ws_url = std::env::var("BASE_WS_URL").ok().filter(|s| !s.is_empty());

        let base_chain_id_raw = require_env("BASE_CHAIN_ID")?;
        let base_chain_id: u64 = base_chain_id_raw.parse().map_err(|_| {
            EngineError::Config(format!(
                "BASE_CHAIN_ID must be a positive integer, got '{base_chain_id_raw}'"
            ))
        })?;

        let log_level = std::env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());

        let execution_mode = std::env::var("EXECUTION_MODE")
            .unwrap_or_else(|_| "DRY_RUN".to_string())
            .parse()?;

        let aerodrome_pool_address = std::env::var("AERODROME_POOL_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty());
        let uniswap_v3_pool_address = std::env::var("UNISWAP_V3_POOL_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty());

        let http_poll_interval_secs: u64 = std::env::var("HTTP_POLL_INTERVAL_SECS")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<u64>().map_err(|_| {
                    EngineError::Config(format!(
                        "HTTP_POLL_INTERVAL_SECS must be a positive integer, got '{s}'"
                    ))
                })
            })
            .transpose()?
            // Safe development default: frequent enough to be useful for a
            // Day 1 foundation, gentle enough not to hammer a public RPC.
            .unwrap_or(5);

        let uniswap_v3_factory_address = std::env::var("UNISWAP_V3_FACTORY_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_UNISWAP_V3_FACTORY.to_string())
            .parse::<Address>()
            .map_err(|e| {
                EngineError::Config(format!("invalid UNISWAP_V3_FACTORY_ADDRESS: {e}"))
            })?;

        let aerodrome_factory_address = std::env::var("AERODROME_FACTORY_ADDRESS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| DEFAULT_AERODROME_FACTORY.to_string())
            .parse::<Address>()
            .map_err(|e| EngineError::Config(format!("invalid AERODROME_FACTORY_ADDRESS: {e}")))?;

        let aerodrome_slipstream_factory_addresses: Vec<Address> =
            match std::env::var("AERODROME_SLIPSTREAM_FACTORY_ADDRESSES") {
                // Unset: use the verified defaults (see constant docs above).
                Err(_) => DEFAULT_AERODROME_SLIPSTREAM_FACTORIES
                    .iter()
                    .map(|s| {
                        s.parse::<Address>()
                            .expect("default Slipstream factory addresses must be valid")
                    })
                    .collect(),
                // Explicitly set to empty: operator wants Slipstream discovery off.
                Ok(raw) if raw.trim().is_empty() => Vec::new(),
                // Explicit comma-separated override/extension.
                Ok(raw) => raw
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        s.parse::<Address>().map_err(|e| {
                            EngineError::Config(format!(
                                "invalid address in AERODROME_SLIPSTREAM_FACTORY_ADDRESSES: '{s}': {e}"
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            };

        let log_poll_max_block_range: u64 = std::env::var("LOG_POLL_MAX_BLOCK_RANGE")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|s| {
                s.parse::<u64>().map_err(|_| {
                    EngineError::Config(format!(
                        "LOG_POLL_MAX_BLOCK_RANGE must be a positive integer, got '{s}'"
                    ))
                })
            })
            .transpose()?
            // Conservative default: comfortably under most public RPC
            // providers' eth_getLogs range limits (commonly 2000-10000).
            .unwrap_or(2000);

        let log_start_block: LogStartBlock = std::env::var("LOG_START_BLOCK")
            .unwrap_or_else(|_| "latest".to_string())
            .parse()?;

        let cfg = Config {
            base_rpc_url,
            base_ws_url,
            base_chain_id,
            log_level,
            execution_mode,
            aerodrome_pool_address,
            uniswap_v3_pool_address,
            http_poll_interval: std::time::Duration::from_secs(http_poll_interval_secs),
            uniswap_v3_factory_address,
            aerodrome_factory_address,
            aerodrome_slipstream_factory_addresses,
            log_poll_max_block_range,
            log_start_block,
        };

        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> EngineResult<()> {
        if self.base_rpc_url.trim().is_empty() {
            return Err(EngineError::Config("BASE_RPC_URL is empty".into()));
        }
        if !(self.base_rpc_url.starts_with("http://") || self.base_rpc_url.starts_with("https://"))
        {
            return Err(EngineError::Config(
                "BASE_RPC_URL must start with http:// or https://".into(),
            ));
        }
        if let Some(ws) = &self.base_ws_url {
            if !(ws.starts_with("ws://") || ws.starts_with("wss://")) {
                return Err(EngineError::Config(
                    "BASE_WS_URL must start with ws:// or wss://".into(),
                ));
            }
        }
        if self.base_chain_id == 0 {
            return Err(EngineError::Config("BASE_CHAIN_ID must be nonzero".into()));
        }
        if self.http_poll_interval.as_secs() == 0 {
            return Err(EngineError::Config(
                "HTTP_POLL_INTERVAL_SECS must be greater than zero".into(),
            ));
        }
        if self.log_poll_max_block_range == 0 {
            return Err(EngineError::Config(
                "LOG_POLL_MAX_BLOCK_RANGE must be greater than zero".into(),
            ));
        }

        // Hard safety invariant, independent of ExecutionMode::can_execute_trades:
        // Day 1 refuses to even start if anything smells like a private key was
        // configured for use. We don't scan the whole environment (too broad /
        // fragile), but we never read one ourselves anywhere in this codebase.
        if std::env::var("PRIVATE_KEY").is_ok() {
            tracing::warn!(
                "PRIVATE_KEY is set in the environment but is never read by this program. \
                 Day 1 has no signer and no trade execution path."
            );
        }

        Ok(())
    }
}

fn require_env(key: &str) -> EngineResult<String> {
    std::env::var(key)
        .map_err(|_| EngineError::Config(format!("missing required environment variable: {key}")))
        .and_then(|v| {
            if v.trim().is_empty() {
                Err(EngineError::Config(format!(
                    "environment variable {key} is set but empty"
                )))
            } else {
                Ok(v)
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // Environment variables are process-global, so serialize tests that touch them.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn clear_env() {
        for key in [
            "BASE_RPC_URL",
            "BASE_WS_URL",
            "BASE_CHAIN_ID",
            "LOG_LEVEL",
            "EXECUTION_MODE",
            "AERODROME_POOL_ADDRESS",
            "UNISWAP_V3_POOL_ADDRESS",
            "PRIVATE_KEY",
            "HTTP_POLL_INTERVAL_SECS",
            "UNISWAP_V3_FACTORY_ADDRESS",
            "AERODROME_FACTORY_ADDRESS",
            "AERODROME_SLIPSTREAM_FACTORY_ADDRESSES",
            "LOG_POLL_MAX_BLOCK_RANGE",
            "LOG_START_BLOCK",
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn valid_configuration_loads() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_WS_URL", "wss://mainnet.base.org/ws");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("LOG_LEVEL", "debug");

        let cfg = Config::load_from_env().expect("valid config should load");
        assert_eq!(cfg.base_chain_id, 8453);
        assert_eq!(cfg.execution_mode, ExecutionMode::DryRun);
        clear_env();
    }

    #[test]
    fn missing_required_configuration_produces_clear_error() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_CHAIN_ID", "8453");
        // BASE_RPC_URL intentionally missing.

        let err = Config::load_from_env().expect_err("missing BASE_RPC_URL should fail");
        match err {
            EngineError::Config(msg) => assert!(msg.contains("BASE_RPC_URL")),
            other => panic!("expected Config error, got {other:?}"),
        }
        clear_env();
    }

    #[test]
    fn invalid_chain_id_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "not-a-number");

        let err = Config::load_from_env().expect_err("non-numeric chain id should fail");
        assert!(matches!(err, EngineError::Config(_)));
        clear_env();
    }

    #[test]
    fn live_mode_never_permits_trade_execution() {
        assert!(!ExecutionMode::Live.can_execute_trades());
        assert!(!ExecutionMode::DryRun.can_execute_trades());
        assert!(!ExecutionMode::Simulation.can_execute_trades());
    }

    #[test]
    fn execution_mode_parses_case_insensitively() {
        assert_eq!("dry_run".parse::<ExecutionMode>().unwrap(), ExecutionMode::DryRun);
        assert_eq!("LIVE".parse::<ExecutionMode>().unwrap(), ExecutionMode::Live);
        assert_eq!(
            "simulation".parse::<ExecutionMode>().unwrap(),
            ExecutionMode::Simulation
        );
        assert!("bogus".parse::<ExecutionMode>().is_err());
    }

    #[test]
    fn empty_base_ws_url_is_treated_as_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("BASE_WS_URL", ""); // explicitly empty, not just unset

        let cfg = Config::load_from_env().expect("config without a WS endpoint should still load");
        assert!(
            cfg.base_ws_url.is_none(),
            "empty BASE_WS_URL must be normalized to None so HTTP fallback is selected"
        );
        clear_env();
    }

    #[test]
    fn unset_base_ws_url_is_none() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        // BASE_WS_URL not set at all.

        let cfg = Config::load_from_env().expect("config without BASE_WS_URL should still load");
        assert!(cfg.base_ws_url.is_none());
        clear_env();
    }

    #[test]
    fn http_poll_interval_defaults_to_a_safe_value_when_unset() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");

        let cfg = Config::load_from_env().expect("config should load with default poll interval");
        assert_eq!(cfg.http_poll_interval, std::time::Duration::from_secs(5));
        clear_env();
    }

    #[test]
    fn http_poll_interval_is_configurable() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("HTTP_POLL_INTERVAL_SECS", "15");

        let cfg = Config::load_from_env().expect("config with custom poll interval should load");
        assert_eq!(cfg.http_poll_interval, std::time::Duration::from_secs(15));
        clear_env();
    }

    #[test]
    fn zero_http_poll_interval_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("HTTP_POLL_INTERVAL_SECS", "0");

        let err = Config::load_from_env().expect_err("zero poll interval must be rejected");
        assert!(matches!(err, EngineError::Config(_)));
        clear_env();
    }

    #[test]
    fn factory_addresses_default_to_verified_base_deployments() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");

        let cfg = Config::load_from_env().expect("config should load with default factories");
        assert_eq!(
            cfg.uniswap_v3_factory_address,
            DEFAULT_UNISWAP_V3_FACTORY.parse::<Address>().unwrap()
        );
        assert_eq!(
            cfg.aerodrome_factory_address,
            DEFAULT_AERODROME_FACTORY.parse::<Address>().unwrap()
        );
        let expected: Vec<Address> = DEFAULT_AERODROME_SLIPSTREAM_FACTORIES
            .iter()
            .map(|s| s.parse::<Address>().unwrap())
            .collect();
        assert_eq!(
            cfg.aerodrome_slipstream_factory_addresses, expected,
            "Slipstream factories must default to the verified deployment list"
        );
        clear_env();
    }

    #[test]
    fn slipstream_factories_are_overridable_with_a_single_address() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var(
            "AERODROME_SLIPSTREAM_FACTORY_ADDRESSES",
            "0xdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaD",
        );

        let cfg = Config::load_from_env().expect("config should load with override");
        assert_eq!(
            cfg.aerodrome_slipstream_factory_addresses,
            vec!["0xdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaD"
                .parse::<Address>()
                .unwrap()]
        );
        clear_env();
    }

    #[test]
    fn slipstream_factories_support_a_configurable_list() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var(
            "AERODROME_SLIPSTREAM_FACTORY_ADDRESSES",
            "0xdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaD, 0xbEEFbEEFbEEFbEEFbEEFbEEFbEEFbEEFbEEFbEEF",
        );

        let cfg = Config::load_from_env().expect("config should load with multiple factories");
        assert_eq!(
            cfg.aerodrome_slipstream_factory_addresses,
            vec![
                "0xdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaD"
                    .parse::<Address>()
                    .unwrap(),
                "0xbEEFbEEFbEEFbEEFbEEFbEEFbEEFbEEFbEEFbEEF"
                    .parse::<Address>()
                    .unwrap(),
            ],
            "must support more than one configured CL factory at once"
        );
        clear_env();
    }

    #[test]
    fn slipstream_factories_can_be_explicitly_disabled() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("AERODROME_SLIPSTREAM_FACTORY_ADDRESSES", "");

        let cfg = Config::load_from_env().expect("config should load with Slipstream disabled");
        assert!(
            cfg.aerodrome_slipstream_factory_addresses.is_empty(),
            "explicit empty value must disable Slipstream discovery, not fall back to defaults"
        );
        clear_env();
    }

    #[test]
    fn slipstream_factories_reject_a_malformed_address_in_the_list() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var(
            "AERODROME_SLIPSTREAM_FACTORY_ADDRESSES",
            "0xdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaDdEaD,not-an-address",
        );

        let err = Config::load_from_env().expect_err("malformed address must be rejected");
        assert!(matches!(err, EngineError::Config(_)));
        clear_env();
    }

    #[test]
    fn log_start_block_defaults_to_latest() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");

        let cfg = Config::load_from_env().expect("config should load");
        assert_eq!(cfg.log_start_block, LogStartBlock::Latest);
        clear_env();
    }

    #[test]
    fn log_start_block_accepts_explicit_block_number() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("LOG_START_BLOCK", "12345678");

        let cfg = Config::load_from_env().expect("config should load");
        assert_eq!(cfg.log_start_block, LogStartBlock::Block(12345678));
        clear_env();
    }

    #[test]
    fn zero_log_poll_max_block_range_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("LOG_POLL_MAX_BLOCK_RANGE", "0");

        let err = Config::load_from_env().expect_err("zero max range must be rejected");
        assert!(matches!(err, EngineError::Config(_)));
        clear_env();
    }
}
