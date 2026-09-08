//! Pool registry data models: lifecycle status, discovery provenance, and
//! eligibility. Separate from `market::MarketState` by design - the
//! registry answers "what pools do we know about and can we trust them",
//! `MarketState` answers "what is the latest observed state of a pool".

use crate::market::models::Pool;
use alloy::primitives::{Address, B256};
use serde::{Deserialize, Serialize};

/// Lifecycle of a discovered pool. Pools only ever move forward through
/// this state machine (never silently reset), except into `Blacklisted`,
/// which can be reached from any state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PoolStatus {
    /// Observed via a factory `PoolCreated` event (or explicit verified
    /// config), but on-chain state has not been fetched yet.
    Discovered,
    /// Hydration (immutable/current state + token metadata) in progress.
    Hydrating,
    /// Hydrated successfully; state can be trusted as of its freshness
    /// metadata.
    Active,
    /// Was `Active` at some point but hydration/updates are currently
    /// failing (e.g. RPC errors) - not necessarily a bad pool, may recover.
    Inactive,
    /// Explicitly excluded - malformed data, unsupported mechanics, or
    /// operator decision. Never reconsidered automatically.
    Blacklisted,
}

/// Provenance: how/where a pool entered the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscoverySource {
    /// A specific factory `PoolCreated`-style event.
    FactoryEvent {
        factory_address: Address,
        block_number: u64,
        tx_hash: B256,
    },
    /// Explicit operator configuration (verified address supplied directly,
    /// bypassing factory-event discovery). Mirrors Day 1's
    /// `AERODROME_POOL_ADDRESS` / `UNISWAP_V3_POOL_ADDRESS` pattern.
    ExplicitConfig,
}

/// Coarse-grained eligibility signal for whether a pool is safe to hand to
/// future strategy/scanning layers. Day 2 only *computes* this; nothing
/// downstream consumes it as a hard gate yet - that's the opportunity
/// engine's job (later days), which will impose stronger criteria on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EligibilityStatus {
    Eligible,
    Ineligible,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolEligibility {
    pub verified_protocol: bool,
    pub token_metadata_available: bool,
    pub pool_type_supported: bool,
    pub liquidity_available: bool,
    pub state_readable: bool,
}

impl PoolEligibility {
    pub fn unknown() -> Self {
        PoolEligibility {
            verified_protocol: false,
            token_metadata_available: false,
            pool_type_supported: false,
            liquidity_available: false,
            state_readable: false,
        }
    }

    /// Overall status derived from the individual signals. All five must be
    /// true for `Eligible`; if none have been evaluated yet (all false, the
    /// `unknown()` default), the status is `Unknown` rather than a
    /// misleadingly confident `Ineligible`.
    pub fn status(&self) -> EligibilityStatus {
        let all_true = self.verified_protocol
            && self.token_metadata_available
            && self.pool_type_supported
            && self.liquidity_available
            && self.state_readable;
        let all_false = !self.verified_protocol
            && !self.token_metadata_available
            && !self.pool_type_supported
            && !self.liquidity_available
            && !self.state_readable;

        if all_true {
            EligibilityStatus::Eligible
        } else if all_false {
            EligibilityStatus::Unknown
        } else {
            EligibilityStatus::Ineligible
        }
    }
}

/// A pool as tracked by the registry: identity/state (`Pool`, reused from
/// `market::models`), lifecycle, provenance, and eligibility. Freshness of
/// the *market data* itself lives on `MarketState`'s `PoolState`, not here -
/// the registry's `last_updated_block` tracks registry-level bookkeeping
/// (when this record was last touched), which is a different concern.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRecord {
    pub pool: Pool,
    pub status: PoolStatus,
    pub discovery: DiscoverySource,
    pub eligibility: PoolEligibility,
    pub discovered_at_block: u64,
    pub last_updated_block: u64,
    pub last_updated_timestamp: Option<u64>,
}

impl PoolRecord {
    pub fn new_discovered(pool: Pool, discovery: DiscoverySource, block_number: u64) -> Self {
        PoolRecord {
            pool,
            status: PoolStatus::Discovered,
            discovery,
            eligibility: PoolEligibility::unknown(),
            discovered_at_block: block_number,
            last_updated_block: block_number,
            last_updated_timestamp: None,
        }
    }
}
