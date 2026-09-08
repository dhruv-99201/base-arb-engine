# base-arb-engine: verified Aerodrome Slipstream (CL) factory configuration
# Run from the repository root: C:\Users\Dell\Downloads\base-arb-engine
#   powershell -ExecutionPolicy Bypass -File apply_slipstream_factories.ps1
Write-Host 'Applying verified Slipstream factory configuration...'

# Ensure all target directories exist first.
New-Item -ItemType Directory -Force -Path 'src' | Out-Null

# ---- src/config.rs ----
$content = @'
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
const DEFAULT_AERODROME_FACTORY: &str = "0x420DD381b31aEf6683db6B902084cB0FFECe40Da";

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

'@
Set-Content -Path 'src\config.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/config.rs'

# ---- src/discovery_pipeline.rs ----
$content = @'
//! Orchestrates Day 2's HTTP-only pipeline: scan factories for newly
//! created pools, hydrate them (token metadata + on-chain state), then scan
//! already-known pools for swap events and fold them into `MarketState`.
//!
//! Deliberately transport-independent at the boundary that matters: this
//! module talks to `HttpLogPoller` directly (HTTP `eth_getLogs`), but
//! everything downstream of "raw `RpcLog`" - decoding, dedup (via
//! `MarketState::apply_event`), state application - is the exact same code
//! Day 1's WebSocket path uses. A future low-latency/WS feed for discovery
//! and swaps would plug in beside `HttpLogPoller`, not replace this
//! decode/apply logic.

use crate::chain::log_poller::{HttpLogPoller, LogPollCheckpoint};
use crate::config::{Config, LogStartBlock};
use crate::dex::discovery::{
    AerodromeClassicDiscovery, AerodromeSlipstreamDiscovery, DiscoveryParams, PoolDiscoveryAdapter,
    UniswapV3Discovery,
};
use crate::dex::traits::DexAdapter;
use crate::dex::{AerodromeAdapter, AerodromeSlipstreamAdapter, UniswapV3Adapter};
use crate::events::decoder::{aerodrome_swap_topic0, now_us, uniswap_v3_swap_topic0};
use crate::market::models::{DexKind, Pool, PoolKind, Token};
use crate::market::SharedMarketState;
use crate::pools::models::{DiscoverySource, PoolEligibility, PoolStatus};
use crate::pools::{PoolRegistry, TokenMetadataCache};
use alloy::primitives::{Address, U256};
use std::collections::HashMap;

pub struct DiscoveryPipeline {
    rpc_url: String,
    chain_id: u64,
    log_start_block: LogStartBlock,
    log_poller: HttpLogPoller,

    discovery_adapters: Vec<Box<dyn PoolDiscoveryAdapter>>,
    dex_adapters: HashMap<DexKind, Box<dyn DexAdapter>>,

    pub registry: PoolRegistry,
    token_cache: TokenMetadataCache,

    discovery_checkpoint: LogPollCheckpoint,
    swap_checkpoint: LogPollCheckpoint,
}

impl DiscoveryPipeline {
    pub fn new(config: &Config, chain_id: u64) -> Self {
        let mut discovery_adapters: Vec<Box<dyn PoolDiscoveryAdapter>> = vec![
            Box::new(UniswapV3Discovery::new(config.uniswap_v3_factory_address)),
            Box::new(AerodromeClassicDiscovery::new(
                config.aerodrome_factory_address,
            )),
        ];
        if config.aerodrome_slipstream_factory_addresses.is_empty() {
            tracing::info!(
                "AERODROME_SLIPSTREAM_FACTORY_ADDRESSES is empty - Slipstream pool discovery \
                 is disabled."
            );
        } else {
            for addr in &config.aerodrome_slipstream_factory_addresses {
                discovery_adapters.push(Box::new(AerodromeSlipstreamDiscovery::new(*addr)));
            }
        }

        let mut dex_adapters: HashMap<DexKind, Box<dyn DexAdapter>> = HashMap::new();
        dex_adapters.insert(DexKind::UniswapV3, Box::new(UniswapV3Adapter::new()));
        dex_adapters.insert(DexKind::Aerodrome, Box::new(AerodromeAdapter::new()));
        dex_adapters.insert(
            DexKind::AerodromeSlipstream,
            Box::new(AerodromeSlipstreamAdapter::new()),
        );

        DiscoveryPipeline {
            rpc_url: config.base_rpc_url.clone(),
            chain_id,
            log_start_block: config.log_start_block,
            log_poller: HttpLogPoller::new(
                config.base_rpc_url.clone(),
                config.log_poll_max_block_range,
            ),
            discovery_adapters,
            dex_adapters,
            registry: PoolRegistry::new(),
            token_cache: TokenMetadataCache::new(),
            discovery_checkpoint: LogPollCheckpoint::new(),
            swap_checkpoint: LogPollCheckpoint::new(),
        }
    }

    /// One full pipeline pass: discover -> hydrate -> scan swaps. Safe to
    /// call repeatedly on a timer; every step is checkpointed and
    /// idempotent (redelivered logs/duplicate pools are no-ops, not
    /// errors).
    pub async fn run_once(&mut self, latest_block: u64, market_state: &SharedMarketState) {
        self.scan_discovery(latest_block).await;
        self.hydrate_pending_pools().await;
        self.scan_swaps(latest_block, market_state).await;
    }

    async fn scan_discovery(&mut self, latest_block: u64) {
        let Some((from, to)) = self
            .discovery_checkpoint
            .next_range(latest_block, self.log_start_block)
        else {
            return;
        };

        tracing::info!(
            source = "http_poll",
            scan = "discovery",
            range_from = from,
            range_to = to,
            "scanning for new pools"
        );

        for i in 0..self.discovery_adapters.len() {
            let factory_address = self.discovery_adapters[i].factory_address();
            let topic0 = self.discovery_adapters[i].event_topic0();

            let logs = match self
                .log_poller
                .fetch_logs(from, to, vec![factory_address], topic0)
                .await
            {
                Ok(logs) => logs,
                Err(err) => {
                    tracing::error!(
                        source = "http_poll",
                        scan = "discovery",
                        error = %err,
                        "discovery scan failed for this factory"
                    );
                    continue;
                }
            };

            tracing::info!(
                source = "http_poll",
                scan = "discovery",
                dex = self.discovery_adapters[i].dex().name(),
                logs_returned = logs.len(),
                "discovery scan complete for factory"
            );

            for log in &logs {
                // Defensive reorg guard - see module/README notes: this
                // skips a retracted log rather than applying it as a real
                // discovery event. It does NOT retroactively undo any state
                // from a previous poll; full reorg reconciliation is not
                // implemented.
                if log.removed {
                    tracing::warn!(
                        source = "http_poll",
                        "skipping removed=true log (reorg guard, not full reconciliation)"
                    );
                    continue;
                }

                let discovered = match self.discovery_adapters[i].decode_pool_created(log) {
                    Ok(d) => d,
                    Err(err) => {
                        tracing::error!(source = "http_poll", error = %err, "failed to decode PoolCreated log, skipping");
                        continue;
                    }
                };

                let placeholder_pool = Pool {
                    address: discovered.pool_address,
                    dex: discovered.dex,
                    token0: Token {
                        address: discovered.token0_address,
                        symbol: String::new(),
                        decimals: 0,
                    },
                    token1: Token {
                        address: discovered.token1_address,
                        symbol: String::new(),
                        decimals: 0,
                    },
                    kind: placeholder_pool_kind(&discovered.params),
                };

                let inserted = self.registry.insert_discovered(
                    placeholder_pool,
                    DiscoverySource::FactoryEvent {
                        factory_address: discovered.factory_address,
                        block_number: discovered.block_number,
                        tx_hash: discovered.tx_hash,
                    },
                    discovered.block_number,
                );

                if inserted {
                    tracing::info!(
                        source = "http_poll",
                        event = "pool_discovered",
                        dex = discovered.dex.name(),
                        pool = %discovered.pool_address,
                        token0 = %discovered.token0_address,
                        token1 = %discovered.token1_address,
                        block = discovered.block_number,
                        "pool discovered"
                    );
                }
            }
        }

        self.discovery_checkpoint.advance(to);
    }

    async fn hydrate_pending_pools(&mut self) {
        let pending: Vec<Address> = self
            .registry
            .iter()
            .filter(|(_, record)| record.status == PoolStatus::Discovered)
            .map(|(addr, _)| *addr)
            .collect();

        for address in pending {
            self.registry.set_status(&address, PoolStatus::Hydrating);

            let (dex, token0_addr, token1_addr) = {
                let record = self.registry.get(&address).expect("just looked up");
                (
                    record.pool.dex,
                    record.pool.token0.address,
                    record.pool.token1.address,
                )
            };

            let token0 = self
                .token_cache
                .get_or_fetch(&self.rpc_url, token0_addr)
                .await;
            let token1 = self
                .token_cache
                .get_or_fetch(&self.rpc_url, token1_addr)
                .await;

            let (token0, token1) = match (token0, token1) {
                (Ok(t0), Ok(t1)) => (t0, t1),
                _ => {
                    tracing::warn!(
                        source = "http_poll",
                        pool = %address,
                        "token metadata hydration failed (decimals unavailable) - marking pool inactive"
                    );
                    self.registry.set_status(&address, PoolStatus::Inactive);
                    continue;
                }
            };

            let Some(adapter) = self.dex_adapters.get(&dex) else {
                self.registry.set_status(&address, PoolStatus::Inactive);
                continue;
            };

            let skeleton_kind = self
                .registry
                .get(&address)
                .map(|r| r.pool.kind.clone())
                .unwrap_or(PoolKind::Aerodrome {
                    reserve0: U256::ZERO,
                    reserve1: U256::ZERO,
                    stable: false,
                });

            let skeleton = Pool {
                address,
                dex,
                token0,
                token1,
                kind: skeleton_kind,
            };

            match adapter.get_pool_state(&self.rpc_url, &skeleton).await {
                Ok(pool_state) => {
                    if let Some(record) = self.registry.get_mut(&address) {
                        let liquidity_available = has_liquidity(&pool_state.pool.kind);
                        record.pool = pool_state.pool;
                        record.status = PoolStatus::Active;
                        record.last_updated_block = pool_state.freshness.last_updated_block;
                        record.last_updated_timestamp =
                            pool_state.freshness.last_updated_timestamp;
                        record.eligibility = PoolEligibility {
                            verified_protocol: true,
                            token_metadata_available: true,
                            pool_type_supported: true,
                            liquidity_available,
                            state_readable: true,
                        };
                        tracing::info!(
                            source = "http_poll",
                            event = "pool_hydrated",
                            dex = dex.name(),
                            pool = %address,
                            eligibility = ?record.eligibility.status(),
                            "pool hydrated"
                        );
                    }
                }
                Err(err) => {
                    tracing::warn!(source = "http_poll", pool = %address, error = %err, "state hydration failed - marking pool inactive");
                    self.registry.set_status(&address, PoolStatus::Inactive);
                }
            }
        }
    }

    async fn scan_swaps(&mut self, latest_block: u64, market_state: &SharedMarketState) {
        let Some((from, to)) = self
            .swap_checkpoint
            .next_range(latest_block, self.log_start_block)
        else {
            return;
        };

        let active_by_dex: HashMap<DexKind, Vec<Address>> = {
            let mut map: HashMap<DexKind, Vec<Address>> = HashMap::new();
            for (addr, record) in self.registry.iter() {
                if record.status == PoolStatus::Active {
                    map.entry(record.pool.dex).or_default().push(*addr);
                }
            }
            map
        };

        if active_by_dex.is_empty() {
            self.swap_checkpoint.advance(to);
            return;
        }

        tracing::info!(
            source = "http_poll",
            scan = "swaps",
            range_from = from,
            range_to = to,
            pools_watched = active_by_dex.values().map(|v| v.len()).sum::<usize>(),
            "scanning known pools for swap events"
        );

        for (dex, addresses) in &active_by_dex {
            let topic0 = match dex {
                DexKind::Aerodrome => aerodrome_swap_topic0(),
                DexKind::UniswapV3 | DexKind::AerodromeSlipstream => uniswap_v3_swap_topic0(),
            };

            let logs = match self
                .log_poller
                .fetch_logs(from, to, addresses.clone(), topic0)
                .await
            {
                Ok(logs) => logs,
                Err(err) => {
                    tracing::error!(
                        source = "http_poll",
                        scan = "swaps",
                        dex = dex.name(),
                        error = %err,
                        "swap scan failed"
                    );
                    continue;
                }
            };

            tracing::info!(
                source = "http_poll",
                scan = "swaps",
                dex = dex.name(),
                logs_returned = logs.len(),
                "swap scan complete"
            );

            let Some(adapter) = self.dex_adapters.get(dex) else {
                continue;
            };

            for log in &logs {
                if log.removed {
                    tracing::warn!(
                        source = "http_poll",
                        "skipping removed=true swap log (reorg guard, not full reconciliation)"
                    );
                    continue;
                }

                let received_at_us = now_us();
                match adapter.decode_event(log, self.chain_id, received_at_us) {
                    Ok(event) => {
                        let mut guard = market_state.write().await;
                        if guard.apply_event(event.clone()) {
                            crate::telemetry::log_event_received(&event);
                        } else {
                            crate::telemetry::log_event_duplicate(&event);
                        }
                    }
                    Err(err) => {
                        tracing::error!(source = "http_poll", error = %err, "failed to decode swap log, skipping");
                    }
                }
            }
        }

        self.swap_checkpoint.advance(to);
    }
}

fn placeholder_pool_kind(params: &DiscoveryParams) -> PoolKind {
    match params {
        DiscoveryParams::AerodromeClassic { stable } => PoolKind::Aerodrome {
            reserve0: U256::ZERO,
            reserve1: U256::ZERO,
            stable: *stable,
        },
        DiscoveryParams::ConcentratedLiquidity { tick_spacing, .. } => {
            PoolKind::ConcentratedLiquidity {
                fee_tier: 0,
                tick_spacing: *tick_spacing,
                sqrt_price_x96: U256::ZERO,
                current_tick: 0,
                liquidity: 0,
                initialized_ticks: Default::default(),
            }
        }
    }
}

fn has_liquidity(kind: &PoolKind) -> bool {
    match kind {
        PoolKind::Aerodrome {
            reserve0, reserve1, ..
        } => !reserve0.is_zero() && !reserve1.is_zero(),
        PoolKind::ConcentratedLiquidity { liquidity, .. } => *liquidity > 0,
    }
}

'@
Set-Content -Path 'src\discovery_pipeline.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/discovery_pipeline.rs'

# ---- src/cli.rs ----
$content = @'
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
use crate::error::{EngineError, EngineResult};
use alloy::primitives::{Address, B256};
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

'@
Set-Content -Path 'src\cli.rs' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote src/cli.rs'

# ---- .env.example ----
$content = @'
# Copy this file to `.env` and fill in real values. NEVER commit `.env`.

# --- Base RPC / connectivity (required) ---
# Any Base-compatible HTTP RPC endpoint. Do not assume a specific provider.
BASE_RPC_URL=https://mainnet.base.org

# WebSocket endpoint. OPTIONAL. If set, block/log ingestion streams over
# WebSocket with reconnect + exponential backoff. If left empty/unset, the
# app automatically falls back to periodic HTTP polling of BASE_RPC_URL for
# the latest block (see HTTP_POLL_INTERVAL_SECS below) and keeps running -
# it does not exit. Log/event ingestion for configured pools requires
# WebSocket; HTTP fallback only polls latest-block state.
#
# mainnet.base.org's default WS endpoint may reject connections (HTTP 405)
# for some clients/providers - if that happens, just leave this blank to run
# in HTTP fallback mode until you have a working WS endpoint (e.g. from a
# paid RPC provider).
BASE_WS_URL=

# Base mainnet chain ID is 8453. Base Sepolia testnet is 84532.
BASE_CHAIN_ID=8453

# How often (seconds) the HTTP-fallback chain source polls for the latest
# block when BASE_WS_URL is not configured. Ignored in WebSocket mode.
# Safe development default: 5.
HTTP_POLL_INTERVAL_SECS=5

# --- Logging ---
# trace | debug | info | warn | error
LOG_LEVEL=info

# --- Execution mode (safety gate) ---
# DRY_RUN (default) | SIMULATION | LIVE
# Day 1: no mode has a working trade path. LIVE exists as a config value only.
EXECUTION_MODE=DRY_RUN

# --- DEX pool addresses (optional, verify before use) ---
# Leave unset unless you have verified the exact deployed pool address you
# want to watch. This program will never invent or guess an address.
AERODROME_POOL_ADDRESS=
UNISWAP_V3_POOL_ADDRESS=

# --- Day 2: automated pool discovery ---
# Factory addresses default to verified Base deployments (see README /
# config.rs comments for how each was confirmed) - only set these if you
# need to override.
# UNISWAP_V3_FACTORY_ADDRESS=0x33128a8fC17869897dcE68Ed026d694621f6FDfD
# AERODROME_FACTORY_ADDRESS=0x420DD381b31aEf6683db6B902084cB0FFECe40Da

# Aerodrome Slipstream (concentrated liquidity) factories - comma-separated
# list, since multiple factory generations remain simultaneously live
# (pools are never migrated between generations). Defaults to the three
# verified deployments from the official aerodrome-finance/slipstream repo
# README, so you normally don't need to set this at all:
#   0x5e7BB104d84c7CB9B682AaC2F3d509f5F406809A (Initial)
#   0xaDe65c38CD4849aDBA595a4323a8C7DdfE89716a (Gauge Caps)
#   0xf8f2eB4940CFE7d13603DDDD87f123820Fc061Ef (Gauges V3, latest)
# Set to an empty string to explicitly disable Slipstream discovery, or to
# a comma-separated list to override/extend the defaults.
# AERODROME_SLIPSTREAM_FACTORY_ADDRESSES=

# Max blocks per eth_getLogs call (chunked). Safe default below common
# public-RPC-provider limits.
LOG_POLL_MAX_BLOCK_RANGE=2000

# Where to start pool-discovery/swap scanning on first run (no checkpoint
# yet). "latest" = start from the current chain head, no history scan -
# the safe default. Set an explicit block number for a controlled backfill.
LOG_START_BLOCK=latest

# --- NEVER put a real private key in this file or in source. ---
# Day 1/2 have no signer and never read this variable, but it is documented
# here so future days don't accidentally introduce it insecurely.
# PRIVATE_KEY=

'@
Set-Content -Path '.env.example' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote .env.example'

# ---- README.md ----
$content = @'
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

'@
Set-Content -Path 'README.md' -Value $content -NoNewline -Encoding UTF8
Write-Host '  wrote README.md'

Write-Host 'Done. Now run: cargo check'
Write-Host 'Then: cargo test'
Write-Host 'Then: cargo run -- discover-test --from-block <BLOCK> --to-block <BLOCK>  (use a verified Slipstream PoolCreated block)'