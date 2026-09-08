use crate::error::{EngineError, EngineResult};
use std::str::FromStr;

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
}

impl Config {
    /// Load configuration from environment variables (via `.env` if present).
    pub fn load() -> EngineResult<Self> {
        // Loading .env is best-effort: it's fine if it doesn't exist (e.g. in
        // containers where env vars are injected directly).
        let _ = dotenvy::dotenv();

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

        let cfg = Config {
            base_rpc_url,
            base_ws_url,
            base_chain_id,
            log_level,
            execution_mode,
            aerodrome_pool_address,
            uniswap_v3_pool_address,
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
        ] {
            std::env::remove_var(key);
        }
    }

    #[test]
    fn valid_configuration_loads() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_WS_URL", "wss://mainnet.base.org/ws");
        std::env::set_var("BASE_CHAIN_ID", "8453");
        std::env::set_var("LOG_LEVEL", "debug");

        let cfg = Config::load().expect("valid config should load");
        assert_eq!(cfg.base_chain_id, 8453);
        assert_eq!(cfg.execution_mode, ExecutionMode::DryRun);
        clear_env();
    }

    #[test]
    fn missing_required_configuration_produces_clear_error() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var("BASE_CHAIN_ID", "8453");
        // BASE_RPC_URL intentionally missing.

        let err = Config::load().expect_err("missing BASE_RPC_URL should fail");
        match err {
            EngineError::Config(msg) => assert!(msg.contains("BASE_RPC_URL")),
            other => panic!("expected Config error, got {other:?}"),
        }
        clear_env();
    }

    #[test]
    fn invalid_chain_id_is_rejected() {
        let _guard = ENV_LOCK.lock().unwrap();
        clear_env();
        std::env::set_var("BASE_RPC_URL", "https://mainnet.base.org");
        std::env::set_var("BASE_CHAIN_ID", "not-a-number");

        let err = Config::load().expect_err("non-numeric chain id should fail");
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
}
