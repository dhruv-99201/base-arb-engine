//! ERC-20 token metadata hydration, with caching so the same token contract
//! is never queried more than once per process lifetime.
//!
//! Decimals are treated as required (a pool can't be safely reasoned about
//! without them - see Day 1's financial-code rules). `symbol`/`name` are
//! best-effort: a revert or non-standard implementation just leaves them
//! empty rather than failing the whole hydration.

use crate::error::{EngineError, EngineResult};
use crate::market::models::Token;
use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use alloy::sol;
use std::collections::HashMap;

sol! {
    #[sol(rpc)]
    interface IERC20Metadata {
        function decimals() external view returns (uint8);
        function symbol() external view returns (string memory);
        function name() external view returns (string memory);
    }
}

#[derive(Debug, Default)]
pub struct TokenMetadataCache {
    cache: HashMap<Address, Token>,
}

impl TokenMetadataCache {
    pub fn new() -> Self {
        TokenMetadataCache {
            cache: HashMap::new(),
        }
    }

    pub fn get_cached(&self, address: &Address) -> Option<&Token> {
        self.cache.get(address)
    }

    pub fn len(&self) -> usize {
        self.cache.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Fetch (or return cached) metadata for `address`. `decimals()` must
    /// succeed - everything else is best-effort.
    pub async fn get_or_fetch(&mut self, rpc_url: &str, address: Address) -> EngineResult<Token> {
        if let Some(token) = self.cache.get(&address) {
            return Ok(token.clone());
        }

        let url = rpc_url
            .parse()
            .map_err(|e| EngineError::Config(format!("invalid rpc url: {e}")))?;
        let provider = ProviderBuilder::new().connect_http(url);
        let contract = IERC20Metadata::new(address, provider);

        let decimals = contract.decimals().call().await.map_err(|e| EngineError::Dex {
            dex: "erc20".into(),
            reason: format!("decimals() failed for {address}: {e}"),
        })?;

        // Best-effort: a nonstandard/missing symbol or name must never fail
        // hydration - decimals is the only value later pricing math depends
        // on.
        let symbol = contract
            .symbol()
            .call()
            .await
            .unwrap_or_default();
        let _name = contract.name().call().await.unwrap_or_default();

        let token = Token {
            address,
            symbol,
            decimals,
        };
        self.cache.insert(address, token.clone());
        Ok(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_starts_empty() {
        let cache = TokenMetadataCache::new();
        assert!(cache.is_empty());
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn cache_hit_avoids_refetch() {
        let mut cache = TokenMetadataCache::new();
        let addr = Address::from_slice(&[0x42; 20]);
        let token = Token {
            address: addr,
            symbol: "TEST".into(),
            decimals: 18,
        };
        cache.cache.insert(addr, token.clone());

        assert_eq!(cache.get_cached(&addr), Some(&token));
        assert_eq!(cache.len(), 1);
    }
}
