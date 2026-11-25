use crate::config::Config;
use crate::dex::discovery::{DiscoveredPool, DiscoveryParams};
use crate::error::{EngineError, EngineResult};
use crate::market::models::DexKind;
use alloy::primitives::{aliases::U24, Address, B256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::rpc::types::BlockId;
use alloy::sol;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

sol! {
    #[sol(rpc)]
    interface IM13AerodromeFactory {
        function getPool(address tokenA, address tokenB, bool stable) external view returns (address pool);
    }
}

sol! {
    #[sol(rpc)]
    interface IM13UniswapV3Factory {
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool);
    }
}

sol! {
    #[sol(rpc)]
    interface IM13UniswapV3Pool {
        function tickSpacing() external view returns (int24);
    }
}

const MAX_PAIRS: usize = 20;

pub const M13_POOL_UNIVERSE_USAGE: &str = "\
Usage: cargo run -- discover-pool-universe --pair <TOKEN_A>:<TOKEN_B> [repeat] --from-block <BLOCK> --to-block <BLOCK> --csv-out <PATH>

M13: read-only, targeted historical pool discovery on Base.

Uses block-pinned factory getPool lookups for the requested token pairs.
Pool existence is checked at --to-block; pools first appearing inside the
requested interval are located with a binary search of historical state.

Aerodrome Classic volatile pools are included; stable pools are excluded.
Uniswap V3 fee candidates 100, 500, 3000, and 10000 are checked. This
covers the standard tiers, not arbitrary custom tiers. Slipstream is
excluded because the current M10 historical validator does not support it.

The CSV records the first block in the requested interval where the pool
is confirmed to exist. For pools already present at --from-block, that
value is a conservative lower bound, not their original creation block.
Factory lookups do not return the creation transaction hash, so that
CSV field is blank. M10 still validates historical state and profitability.

Supply 1..=20 distinct token pairs.

Options:
  --pair <A>:<B>          Token addresses; repeat for additional pairs.
  --from-block <BLOCK>    First block to scan (inclusive). Required.
  --to-block <BLOCK>      Last block to scan (inclusive). Required.
  --csv-out <PATH>        New CSV path; existing files are never overwritten.
  --help, -h              Show this help and exit.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct M13PoolUniverseArgs {
    pub pairs: Vec<(Address, Address)>,
    pub from_block: u64,
    pub to_block: u64,
    pub csv_out: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum M13PoolUniverseCommand {
    Help,
    Run(M13PoolUniverseArgs),
}

fn canonical_pair_key(token_a: Address, token_b: Address) -> (String, String) {
    let a = token_a.to_string().to_ascii_lowercase();
    let b = token_b.to_string().to_ascii_lowercase();

    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

fn parse_pair(raw: &str) -> Result<(Address, Address), String> {
    let (raw_a, raw_b) = raw
        .split_once(':')
        .ok_or_else(|| format!("invalid --pair '{raw}': expected TOKEN_A:TOKEN_B"))?;

    if raw_b.contains(':') {
        return Err(format!(
            "invalid --pair '{raw}': expected exactly two token addresses"
        ));
    }

    let token_a = raw_a
        .trim()
        .parse::<Address>()
        .map_err(|error| format!("invalid first token in --pair '{raw}': {error}"))?;

    let token_b = raw_b
        .trim()
        .parse::<Address>()
        .map_err(|error| format!("invalid second token in --pair '{raw}': {error}"))?;

    if token_a == Address::ZERO || token_b == Address::ZERO {
        return Err(format!("--pair '{raw}' cannot contain the zero address"));
    }

    if token_a == token_b {
        return Err(format!("--pair '{raw}' must contain two different tokens"));
    }

    Ok((token_a, token_b))
}

pub fn parse_m13_pool_universe_args(args: &[String]) -> Result<M13PoolUniverseCommand, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(M13PoolUniverseCommand::Help);
    }

    let mut pairs = Vec::new();
    let mut seen_pairs = HashSet::new();
    let mut from_block = None;
    let mut to_block = None;
    let mut csv_out = None;

    let mut index = 0;

    while index < args.len() {
        let flag = args[index].as_str();

        match flag {
            "--pair" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| "--pair requires TOKEN_A:TOKEN_B".to_string())?;

                let pair = parse_pair(raw)?;
                let key = canonical_pair_key(pair.0, pair.1);

                if !seen_pairs.insert(key) {
                    return Err(format!(
                        "duplicate --pair '{raw}' (token order does not matter)"
                    ));
                }

                pairs.push(pair);

                if pairs.len() > MAX_PAIRS {
                    return Err(format!(
                        "at most {MAX_PAIRS} distinct --pair values are allowed"
                    ));
                }

                index += 2;
            }

            "--from-block" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| "--from-block requires a value".to_string())?;

                if from_block.is_some() {
                    return Err("--from-block may only be supplied once".into());
                }

                from_block = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid --from-block value: '{raw}'"))?,
                );

                index += 2;
            }

            "--to-block" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| "--to-block requires a value".to_string())?;

                if to_block.is_some() {
                    return Err("--to-block may only be supplied once".into());
                }

                to_block = Some(
                    raw.parse::<u64>()
                        .map_err(|_| format!("invalid --to-block value: '{raw}'"))?,
                );

                index += 2;
            }

            "--csv-out" => {
                let raw = args
                    .get(index + 1)
                    .ok_or_else(|| "--csv-out requires a path".to_string())?;

                if csv_out.is_some() {
                    return Err("--csv-out may only be supplied once".into());
                }

                if raw.trim().is_empty() {
                    return Err("--csv-out cannot be empty".into());
                }

                csv_out = Some(PathBuf::from(raw));
                index += 2;
            }

            other => return Err(format!("unrecognized argument: '{other}'")),
        }
    }

    if pairs.is_empty() {
        return Err("at least one --pair TOKEN_A:TOKEN_B is required".into());
    }

    let from_block =
        from_block.ok_or_else(|| "missing required --from-block <BLOCK>".to_string())?;

    let to_block = to_block.ok_or_else(|| "missing required --to-block <BLOCK>".to_string())?;

    let csv_out = csv_out.ok_or_else(|| "missing required --csv-out <PATH>".to_string())?;

    if from_block > to_block {
        return Err(format!(
            "--from-block ({from_block}) must be <= --to-block ({to_block})"
        ));
    }

    Ok(M13PoolUniverseCommand::Run(M13PoolUniverseArgs {
        pairs,
        from_block,
        to_block,
        csv_out,
    }))
}

fn matches_pair(
    token_a: Address,
    token_b: Address,
    discovered_token0: Address,
    discovered_token1: Address,
) -> bool {
    (token_a == discovered_token0 && token_b == discovered_token1)
        || (token_a == discovered_token1 && token_b == discovered_token0)
}

fn is_supported_pool(dex: &DexKind, params: &DiscoveryParams) -> bool {
    matches!(
        (dex, params),
        (
            DexKind::Aerodrome,
            DiscoveryParams::AerodromeClassic { stable: false }
        ) | (
            DexKind::UniswapV3,
            DiscoveryParams::ConcentratedLiquidity { .. }
        )
    )
}

fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

fn build_catalog_csv(pools: &[DiscoveredPool]) -> String {
    let mut csv = String::from(
        "dex,pool_address,token0_address,token1_address,factory_address,\
first_seen_block,creation_tx_hash,stable,fee,tick_spacing\n",
    );

    for pool in pools {
        let (stable, fee, tick_spacing) = match &pool.params {
            DiscoveryParams::AerodromeClassic { stable } => {
                (stable.to_string(), String::new(), String::new())
            }
            DiscoveryParams::ConcentratedLiquidity { tick_spacing, fee } => (
                String::new(),
                fee.map(|value| value.to_string()).unwrap_or_default(),
                tick_spacing.to_string(),
            ),
        };

        let fields = vec![
            pool.dex.name().to_string(),
            pool.pool_address.to_string(),
            pool.token0_address.to_string(),
            pool.token1_address.to_string(),
            pool.factory_address.to_string(),
            pool.block_number.to_string(),
            if pool.tx_hash == B256::ZERO {
                String::new()
            } else {
                pool.tx_hash.to_string()
            },
            stable,
            fee,
            tick_spacing,
        ];

        let escaped = fields
            .iter()
            .map(|field| csv_escape(field))
            .collect::<Vec<_>>()
            .join(",");

        csv.push_str(&escaped);
        csv.push('\n');
    }

    csv
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct M13CatalogSelection {
    pub aerodrome_pools: Vec<Address>,
    pub uniswap_v3_pools: Vec<Address>,
    pub matched_pairs: usize,
}

#[derive(Debug, Default)]
struct CatalogPairPools {
    aerodrome: Vec<Address>,
    uniswap_v3: Vec<Address>,
}

/// Read and strictly validate an M13 catalog, then select pools containing
/// token_in that existed by earliest_block and have both supported DEX types.
pub fn select_m10_pools_from_catalog(
    csv_path: &std::path::Path,
    token_in: Address,
    earliest_block: u64,
) -> Result<M13CatalogSelection, String> {
    let contents = std::fs::read_to_string(csv_path).map_err(|error| {
        format!(
            "could not read M13 pool catalog '{}': {error}",
            csv_path.display()
        )
    })?;

    let mut lines = contents.lines();

    let expected_header =
        "dex,pool_address,token0_address,token1_address,factory_address,first_seen_block,creation_tx_hash,stable,fee,tick_spacing";

    let header = lines
        .next()
        .ok_or_else(|| "M13 pool catalog is empty".to_string())?;

    if header.trim_start_matches('\u{feff}') != expected_header {
        return Err(format!(
            "invalid M13 catalog header in '{}'",
            csv_path.display()
        ));
    }

    let mut seen_addresses = HashSet::new();
    let mut pairs: HashMap<(String, String), CatalogPairPools> = HashMap::new();

    for (index, line) in lines.enumerate() {
        let row_number = index + 2;

        if line.trim().is_empty() {
            continue;
        }

        // M13 emits only plain addresses, numbers, and flags in these
        // columns. Commas or quoted fields are not valid in this schema.
        let fields: Vec<&str> = line.split(',').collect();

        if fields.len() != 10 {
            return Err(format!(
                "M13 catalog row {row_number}: expected 10 columns, found {}",
                fields.len()
            ));
        }

        let parse_address = |column: usize, label: &str| -> Result<Address, String> {
            fields[column].parse::<Address>().map_err(|error| {
                format!(
                    "M13 catalog row {row_number}: invalid {label} '{}': {error}",
                    fields[column]
                )
            })
        };

        let dex = fields[0].trim();
        let pool_address = parse_address(1, "pool address")?;
        let token0 = parse_address(2, "token0")?;
        let token1 = parse_address(3, "token1")?;
        let factory = parse_address(4, "factory address")?;

        if pool_address == Address::ZERO
            || token0 == Address::ZERO
            || token1 == Address::ZERO
            || factory == Address::ZERO
        {
            return Err(format!(
                "M13 catalog row {row_number}: zero addresses are not permitted"
            ));
        }

        if token0 == token1 {
            return Err(format!(
                "M13 catalog row {row_number}: token0 and token1 are identical"
            ));
        }

        if !seen_addresses.insert(pool_address) {
            return Err(format!(
                "M13 catalog row {row_number}: duplicate pool address {pool_address}"
            ));
        }

        let created_block = fields[5].parse::<u64>().map_err(|_| {
            format!(
                "M13 catalog row {row_number}: invalid first-seen block '{}'",
                fields[5]
            )
        })?;

        // An empty creation_tx_hash is expected for factory getPool lookups:
        // the mapping identifies a pool but does not reveal its creation transaction.
        if !fields[6].trim().is_empty() {
            fields[6].parse::<B256>().map_err(|error| {
                format!("M13 catalog row {row_number}: invalid transaction hash: {error}")
            })?;
        }

        match dex {
            "aerodrome" => {
                if fields[7].trim() != "false"
                    || !fields[8].trim().is_empty()
                    || !fields[9].trim().is_empty()
                {
                    return Err(format!(
                        "M13 catalog row {row_number}: invalid Aerodrome Classic parameters"
                    ));
                }
            }

            "uniswap_v3" => {
                if !fields[7].trim().is_empty() {
                    return Err(format!(
                        "M13 catalog row {row_number}: unexpected Uniswap V3 stable field"
                    ));
                }

                let tick_spacing = fields[9].parse::<i32>().map_err(|_| {
                    format!(
                        "M13 catalog row {row_number}: invalid Uniswap V3 tick spacing '{}'",
                        fields[9]
                    )
                })?;

                if tick_spacing <= 0 {
                    return Err(format!(
                        "M13 catalog row {row_number}: tick spacing must be positive"
                    ));
                }

                if !fields[8].trim().is_empty() {
                    fields[8].parse::<u32>().map_err(|_| {
                        format!(
                            "M13 catalog row {row_number}: invalid Uniswap V3 fee '{}'",
                            fields[8]
                        )
                    })?;
                }
            }

            other => {
                return Err(format!(
                    "M13 catalog row {row_number}: unsupported DEX '{other}'"
                ));
            }
        }

        // A pool created after the earliest requested M10 block did not
        // exist throughout the complete historical validation window.
        if created_block > earliest_block {
            continue;
        }

        if token_in != token0 && token_in != token1 {
            continue;
        }

        let key = canonical_pair_key(token0, token1);
        let entry = pairs.entry(key).or_default();

        match dex {
            "aerodrome" => entry.aerodrome.push(pool_address),
            "uniswap_v3" => entry.uniswap_v3.push(pool_address),
            _ => unreachable!("DEX was validated above"),
        }
    }

    let mut selection = M13CatalogSelection::default();

    for (_, mut pair_pools) in pairs {
        if pair_pools.aerodrome.is_empty() || pair_pools.uniswap_v3.is_empty() {
            continue;
        }

        selection.matched_pairs += 1;
        selection.aerodrome_pools.append(&mut pair_pools.aerodrome);
        selection
            .uniswap_v3_pools
            .append(&mut pair_pools.uniswap_v3);
    }

    selection
        .aerodrome_pools
        .sort_by_key(|address| address.to_string());

    selection
        .uniswap_v3_pools
        .sort_by_key(|address| address.to_string());

    Ok(selection)
}

/// Expand the optional M13 catalog argument into the existing M10 pool flags.
/// Without --pool-catalog, arguments are returned unchanged.
pub fn expand_m10_catalog_args(args: &[String]) -> Result<Vec<String>, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(args.to_vec());
    }

    let mut catalog_path: Option<PathBuf> = None;
    let mut expanded = Vec::with_capacity(args.len());

    let mut index = 0;
    while index < args.len() {
        if args[index] == "--pool-catalog" {
            let raw = args
                .get(index + 1)
                .ok_or_else(|| "--pool-catalog requires a CSV path".to_string())?;

            if catalog_path.is_some() {
                return Err("--pool-catalog may only be supplied once".into());
            }

            if raw.trim().is_empty() {
                return Err("--pool-catalog path cannot be empty".into());
            }

            catalog_path = Some(PathBuf::from(raw));
            index += 2;
        } else {
            expanded.push(args[index].clone());
            index += 1;
        }
    }

    let Some(catalog_path) = catalog_path else {
        return Ok(args.to_vec());
    };

    if expanded
        .iter()
        .any(|arg| arg == "--aerodrome-pool" || arg == "--uniswap-v3-pool")
    {
        return Err(
            "--pool-catalog cannot be combined with manual pool flags; use one pool source".into(),
        );
    }

    let mut token_in = None;
    let mut blocks = Vec::new();

    let mut index = 0;
    while index < expanded.len() {
        match expanded[index].as_str() {
            "--token-in" => {
                let raw = expanded
                    .get(index + 1)
                    .ok_or_else(|| "--token-in requires an address".to_string())?;

                if token_in.is_some() {
                    return Err("--token-in may only be supplied once".into());
                }

                token_in = Some(
                    raw.parse::<Address>()
                        .map_err(|_| format!("invalid --token-in value: '{raw}'"))?,
                );

                index += 2;
            }

            "--block" => {
                let raw = expanded
                    .get(index + 1)
                    .ok_or_else(|| "--block requires a value".to_string())?;

                let block = raw
                    .parse::<u64>()
                    .map_err(|_| format!("invalid --block value: '{raw}'"))?;

                if blocks.contains(&block) {
                    return Err(format!("duplicate --block value: {block}"));
                }

                blocks.push(block);
                index += 2;
            }

            "--aerodrome-pool" | "--uniswap-v3-pool" => {
                return Err("--pool-catalog cannot be combined with manual pool flags".into());
            }

            _ => index += 1,
        }
    }

    if blocks.len() < 2 {
        return Err(
            "at least two distinct --block values are required for multi-block validation".into(),
        );
    }

    if blocks.len() > 100 {
        return Err("at most 100 --block values are allowed per run".into());
    }

    let token_in = token_in.ok_or_else(|| "missing required --token-in <ADDRESS>".to_string())?;

    let earliest_block = *blocks
        .iter()
        .min()
        .expect("at least two blocks were validated");

    let selection = select_m10_pools_from_catalog(&catalog_path, token_in, earliest_block)?;

    if selection.aerodrome_pools.is_empty() || selection.uniswap_v3_pools.is_empty() {
        return Err(format!(
            "M13 catalog has no eligible cross-DEX pair containing token-in {token_in} \
             that existed by block {earliest_block}; matched pairs={}, Aerodrome pools={}, \
             Uniswap V3 pools={}",
            selection.matched_pairs,
            selection.aerodrome_pools.len(),
            selection.uniswap_v3_pools.len()
        ));
    }

    println!(
        "[M13 -> M10] catalog matched pairs={}, Aerodrome pools={}, Uniswap V3 pools={}",
        selection.matched_pairs,
        selection.aerodrome_pools.len(),
        selection.uniswap_v3_pools.len()
    );

    for address in selection.aerodrome_pools {
        expanded.push("--aerodrome-pool".into());
        expanded.push(address.to_string());
    }

    for address in selection.uniswap_v3_pools {
        expanded.push("--uniswap-v3-pool".into());
        expanded.push(address.to_string());
    }

    Ok(expanded)
}

fn same_discovery_record(a: &DiscoveredPool, b: &DiscoveredPool) -> bool {
    a.pool_address == b.pool_address
        && a.token0_address == b.token0_address
        && a.token1_address == b.token1_address
        && a.dex == b.dex
        && a.params == b.params
        && a.block_number == b.block_number
        && a.tx_hash == b.tx_hash
        && a.factory_address == b.factory_address
}

/// Read-only targeted discovery using block-pinned factory mappings. The output
/// file is created only after all historical lookups and pool metadata reads succeed.
pub async fn run_m13_pool_universe(config: &Config, args: M13PoolUniverseArgs) -> EngineResult<()> {
    if args.csv_out.exists() {
        return Err(EngineError::Config(format!(
            "CSV output already exists: {}; choose a new --csv-out path",
            args.csv_out.display()
        )));
    }

    let url = config
        .base_rpc_url
        .parse()
        .map_err(|error| EngineError::Config(format!("invalid BASE_RPC_URL: {error}")))?;
    let provider = ProviderBuilder::new().connect_http(url);

    let latest_block = provider
        .get_block_number()
        .await
        .map_err(|error| EngineError::Chain(format!("get_block_number failed: {error}")))?;
    if args.to_block > latest_block {
        return Err(EngineError::Config(format!(
            "M13 --to-block {} is above the current Base head {}",
            args.to_block, latest_block
        )));
    }

    let aerodrome_factory =
        IM13AerodromeFactory::new(config.aerodrome_factory_address, provider.clone());
    let uniswap_factory =
        IM13UniswapV3Factory::new(config.uniswap_v3_factory_address, provider.clone());

    // These are the standard fee candidates. A factory getPool lookup returns
    // zero for a tier/pair with no pool; arbitrary custom tiers are not enumerated.
    const UNISWAP_V3_FEE_TIERS: [u32; 4] = [100, 500, 3_000, 10_000];

    let mut pools = Vec::new();
    let mut seen_pools: HashMap<Address, DiscoveredPool> = HashMap::new();
    let mut factory_calls = 0usize;
    let mut binary_search_calls = 0usize;
    let mut duplicate_pools = 0usize;

    println!(
        "M13 pool universe: blocks {}..={}, pairs={}",
        args.from_block,
        args.to_block,
        args.pairs.len()
    );
    println!("Discovery method: historical block-pinned factory getPool calls.");
    println!("Uniswap V3 standard fee candidates: 100, 500, 3000, 10000.");
    println!("No eth_getLogs range scan is performed.");

    for (token_a, token_b) in &args.pairs {
        println!("\nPair: {token_a} / {token_b}");
        let (token0, token1) = if token_a.to_string().to_ascii_lowercase()
            <= token_b.to_string().to_ascii_lowercase()
        {
            (*token_a, *token_b)
        } else {
            (*token_b, *token_a)
        };

        // Aerodrome Classic volatile pool lookup at the end of the interval.
        let aerodrome_at_end = aerodrome_factory
            .getPool(*token_a, *token_b, false)
            .block(BlockId::number(args.to_block))
            .call()
            .await
            .map_err(|error| {
                EngineError::Chain(format!(
                    "M13 Aerodrome getPool({token_a}, {token_b}, false) failed at block {}: {error}",
                    args.to_block
                ))
            })?;
        factory_calls += 1;

        if aerodrome_at_end != Address::ZERO {
            let aerodrome_at_start = aerodrome_factory
                .getPool(*token_a, *token_b, false)
                .block(BlockId::number(args.from_block))
                .call()
                .await
                .map_err(|error| {
                    EngineError::Chain(format!(
                        "M13 Aerodrome getPool failed at block {} for {token_a}/{token_b}: {error}",
                        args.from_block
                    ))
                })?;
            factory_calls += 1;

            let first_seen_block = if aerodrome_at_start == aerodrome_at_end {
                args.from_block
            } else if aerodrome_at_start == Address::ZERO {
                let mut absent_block = args.from_block;
                let mut present_block = args.to_block;
                while present_block - absent_block > 1 {
                    let middle_block = absent_block + (present_block - absent_block) / 2;
                    let middle_pool = aerodrome_factory
                        .getPool(*token_a, *token_b, false)
                        .block(BlockId::number(middle_block))
                        .call()
                        .await
                        .map_err(|error| {
                            EngineError::Chain(format!(
                                "M13 Aerodrome historical getPool failed at block {middle_block} for {token_a}/{token_b}: {error}"
                            ))
                        })?;
                    factory_calls += 1;
                    binary_search_calls += 1;

                    if middle_pool == Address::ZERO {
                        absent_block = middle_block;
                    } else if middle_pool == aerodrome_at_end {
                        present_block = middle_block;
                    } else {
                        return Err(EngineError::Chain(format!(
                            "M13 Aerodrome pool mapping changed for {token_a}/{token_b}: expected {aerodrome_at_end}, found {middle_pool} at block {middle_block}"
                        )));
                    }
                }
                present_block
            } else {
                return Err(EngineError::Chain(format!(
                    "M13 Aerodrome pool mapping changed between blocks {} and {} for {token_a}/{token_b}: {} -> {}",
                    args.from_block, args.to_block, aerodrome_at_start, aerodrome_at_end
                )));
            };

            let record = DiscoveredPool {
                pool_address: aerodrome_at_end,
                token0_address: token0,
                token1_address: token1,
                dex: DexKind::Aerodrome,
                params: DiscoveryParams::AerodromeClassic { stable: false },
                block_number: first_seen_block,
                tx_hash: B256::ZERO,
                factory_address: config.aerodrome_factory_address,
            };

            if let Some(existing) = seen_pools.get(&record.pool_address) {
                if !same_discovery_record(existing, &record) {
                    return Err(EngineError::Chain(format!(
                        "conflicting M13 records for pool {}; refusing to write the catalog",
                        record.pool_address
                    )));
                }
                duplicate_pools += 1;
            } else {
                seen_pools.insert(record.pool_address, record.clone());
                pools.push(record);
            }
            println!(
                "  Aerodrome volatile: {} (first seen at/after block {})",
                aerodrome_at_end, first_seen_block
            );
        } else {
            println!("  Aerodrome volatile: no pool at block {}", args.to_block);
        }

        // Uniswap V3 has a separate getPool mapping for each fee tier.
        for fee_tier in UNISWAP_V3_FEE_TIERS {
            let fee_arg = U24::try_from(fee_tier).map_err(|error| {
                EngineError::Arithmetic(format!("invalid Uniswap V3 fee tier {fee_tier}: {error}"))
            })?;

            let uniswap_at_end = uniswap_factory
                .getPool(*token_a, *token_b, fee_arg)
                .block(BlockId::number(args.to_block))
                .call()
                .await
                .map_err(|error| {
                    EngineError::Chain(format!(
                        "M13 Uniswap V3 getPool({token_a}, {token_b}, fee={fee_tier}) failed at block {}: {error}",
                        args.to_block
                    ))
                })?;
            factory_calls += 1;

            if uniswap_at_end == Address::ZERO {
                println!(
                    "  Uniswap V3 fee {fee_tier}: no pool at block {}",
                    args.to_block
                );
                continue;
            }

            let uniswap_at_start = uniswap_factory
                .getPool(*token_a, *token_b, fee_arg)
                .block(BlockId::number(args.from_block))
                .call()
                .await
                .map_err(|error| {
                    EngineError::Chain(format!(
                        "M13 Uniswap V3 getPool failed at block {} for {token_a}/{token_b}, fee={fee_tier}: {error}",
                        args.from_block
                    ))
                })?;
            factory_calls += 1;

            let first_seen_block = if uniswap_at_start == uniswap_at_end {
                args.from_block
            } else if uniswap_at_start == Address::ZERO {
                let mut absent_block = args.from_block;
                let mut present_block = args.to_block;
                while present_block - absent_block > 1 {
                    let middle_block = absent_block + (present_block - absent_block) / 2;
                    let middle_pool = uniswap_factory
                        .getPool(*token_a, *token_b, fee_arg)
                        .block(BlockId::number(middle_block))
                        .call()
                        .await
                        .map_err(|error| {
                            EngineError::Chain(format!(
                                "M13 Uniswap V3 historical getPool failed at block {middle_block} for {token_a}/{token_b}, fee={fee_tier}: {error}"
                            ))
                        })?;
                    factory_calls += 1;
                    binary_search_calls += 1;

                    if middle_pool == Address::ZERO {
                        absent_block = middle_block;
                    } else if middle_pool == uniswap_at_end {
                        present_block = middle_block;
                    } else {
                        return Err(EngineError::Chain(format!(
                            "M13 Uniswap V3 pool mapping changed for {token_a}/{token_b}, fee={fee_tier}: expected {uniswap_at_end}, found {middle_pool} at block {middle_block}"
                        )));
                    }
                }
                present_block
            } else {
                return Err(EngineError::Chain(format!(
                    "M13 Uniswap V3 pool mapping changed between blocks {} and {} for {token_a}/{token_b}, fee={fee_tier}: {} -> {}",
                    args.from_block, args.to_block, uniswap_at_start, uniswap_at_end
                )));
            };

            // Read immutable pool tick spacing at the same endpoint block used
            // to discover the pool. The value is needed by the existing M10 model.
            let tick_spacing_wire = IM13UniswapV3Pool::new(uniswap_at_end, provider.clone())
                .tickSpacing()
                .block(BlockId::number(args.to_block))
                .call()
                .await
                .map_err(|error| {
                    EngineError::Chain(format!(
                        "M13 Uniswap V3 tickSpacing() failed for pool {uniswap_at_end} at block {}: {error}",
                        args.to_block
                    ))
                })?;
            factory_calls += 1;
            let tick_spacing: i32 = tick_spacing_wire.try_into().map_err(|_| {
                EngineError::Arithmetic(format!(
                    "M13 Uniswap V3 tick spacing for pool {uniswap_at_end} does not fit i32"
                ))
            })?;
            if tick_spacing <= 0 {
                return Err(EngineError::Chain(format!(
                    "M13 Uniswap V3 pool {uniswap_at_end} returned invalid tick spacing {tick_spacing}"
                )));
            }

            let record = DiscoveredPool {
                pool_address: uniswap_at_end,
                token0_address: token0,
                token1_address: token1,
                dex: DexKind::UniswapV3,
                params: DiscoveryParams::ConcentratedLiquidity {
                    tick_spacing,
                    fee: Some(fee_tier),
                },
                block_number: first_seen_block,
                tx_hash: B256::ZERO,
                factory_address: config.uniswap_v3_factory_address,
            };

            if let Some(existing) = seen_pools.get(&record.pool_address) {
                if !same_discovery_record(existing, &record) {
                    return Err(EngineError::Chain(format!(
                        "conflicting M13 records for pool {}; refusing to write the catalog",
                        record.pool_address
                    )));
                }
                duplicate_pools += 1;
            } else {
                seen_pools.insert(record.pool_address, record.clone());
                pools.push(record);
            }
            println!("  Uniswap V3 fee {fee_tier}: {} (tick spacing {tick_spacing}, first seen at/after block {})", uniswap_at_end, first_seen_block);
        }
    }

    pools.sort_by_key(|pool| pool.pool_address.to_string());
    let csv = build_catalog_csv(&pools);

    // create_new protects prior research output from accidental overwrite.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.csv_out)
        .map_err(|error| {
            EngineError::Chain(format!(
                "failed to create M13 catalog '{}': {error}",
                args.csv_out.display()
            ))
        })?;
    file.write_all(csv.as_bytes())
        .and_then(|_| file.flush())
        .map_err(|error| {
            EngineError::Chain(format!(
                "failed to write M13 catalog '{}': {error}",
                args.csv_out.display()
            ))
        })?;

    let aerodrome_count = pools
        .iter()
        .filter(|pool| pool.dex == DexKind::Aerodrome)
        .count();
    let uniswap_count = pools
        .iter()
        .filter(|pool| pool.dex == DexKind::UniswapV3)
        .count();
    println!("\nM13 pool universe complete");
    println!("  historical factory/tick calls: {factory_calls}");
    println!("  binary-search calls:           {binary_search_calls}");
    println!("  duplicate pools:               {duplicate_pools}");
    println!("  Aerodrome Classic volatile:    {aerodrome_count}");
    println!("  Uniswap V3:                    {uniswap_count}");
    println!("  unique pools written:          {}", pools.len());
    println!(
        "  output:                        {}",
        args.csv_out.display()
    );
    println!("\nCompatibility:");
    println!("  Aerodrome stable pools: excluded");
    println!("  Uniswap V3 custom fee tiers: not enumerated; standard candidates only");
    println!("  Aerodrome Slipstream: excluded");
    println!("  tx hash: blank because getPool does not expose creation transaction provenance");
    println!("  historical liquidity: not established");
    println!("  profitability: not established");
    println!("  transaction execution: not performed");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::address;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    const TOKEN_A: Address = address!("0000000000000000000000000000000000000001");
    const TOKEN_B: Address = address!("0000000000000000000000000000000000000002");

    fn valid_args() -> Vec<String> {
        args(&[
            "--pair",
            "0x0000000000000000000000000000000000000001:0x0000000000000000000000000000000000000002",
            "--from-block",
            "26000000",
            "--to-block",
            "26100000",
            "--csv-out",
            "m13-pools.csv",
        ])
    }

    #[test]
    fn parses_valid_pool_universe_arguments() {
        let parsed = parse_m13_pool_universe_args(&valid_args()).unwrap();

        assert_eq!(
            parsed,
            M13PoolUniverseCommand::Run(M13PoolUniverseArgs {
                pairs: vec![(TOKEN_A, TOKEN_B)],
                from_block: 26_000_000,
                to_block: 26_100_000,
                csv_out: PathBuf::from("m13-pools.csv"),
            })
        );
    }

    #[test]
    fn accepts_multiple_distinct_pairs() {
        let parsed = parse_m13_pool_universe_args(&args(&[
            "--pair",
            "0x0000000000000000000000000000000000000001:0x0000000000000000000000000000000000000002",
            "--pair",
            "0x0000000000000000000000000000000000000003:0x0000000000000000000000000000000000000004",
            "--from-block",
            "100",
            "--to-block",
            "200",
            "--csv-out",
            "out.csv",
        ]))
        .unwrap();

        let M13PoolUniverseCommand::Run(parsed) = parsed else {
            panic!("expected Run");
        };

        assert_eq!(parsed.pairs.len(), 2);
    }

    #[test]
    fn help_short_circuits_invalid_arguments() {
        assert_eq!(
            parse_m13_pool_universe_args(&args(&["--bad", "--help"])),
            Ok(M13PoolUniverseCommand::Help)
        );
    }

    #[test]
    fn rejects_missing_pair() {
        let error = parse_m13_pool_universe_args(&args(&[
            "--from-block",
            "100",
            "--to-block",
            "200",
            "--csv-out",
            "out.csv",
        ]))
        .unwrap_err();

        assert!(error.contains("--pair"));
    }

    #[test]
    fn rejects_reversed_block_range() {
        let error = parse_m13_pool_universe_args(&args(&[
            "--pair",
            "0x0000000000000000000000000000000000000001:0x0000000000000000000000000000000000000002",
            "--from-block",
            "200",
            "--to-block",
            "100",
            "--csv-out",
            "out.csv",
        ]))
        .unwrap_err();

        assert!(error.contains("must be <="));
    }

    #[test]
    fn rejects_duplicate_pair_even_when_order_is_reversed() {
        let error = parse_m13_pool_universe_args(&args(&[
            "--pair",
            "0x0000000000000000000000000000000000000001:0x0000000000000000000000000000000000000002",
            "--pair",
            "0x0000000000000000000000000000000000000002:0x0000000000000000000000000000000000000001",
            "--from-block",
            "100",
            "--to-block",
            "200",
            "--csv-out",
            "out.csv",
        ]))
        .unwrap_err();

        assert!(error.contains("duplicate --pair"));
    }

    #[test]
    fn rejects_identical_tokens() {
        let error = parse_pair(
            "0x0000000000000000000000000000000000000001:\
             0x0000000000000000000000000000000000000001",
        )
        .unwrap_err();

        assert!(error.contains("different tokens"));
    }

    #[test]
    fn rejects_zero_address() {
        let error = parse_pair(
            "0x0000000000000000000000000000000000000000:\
             0x0000000000000000000000000000000000000001",
        )
        .unwrap_err();

        assert!(error.contains("zero address"));
    }

    #[test]
    fn rejects_malformed_pair() {
        assert!(parse_pair("not-an-address").is_err());
        assert!(parse_pair("0x01:0x02:0x03").is_err());
    }

    #[test]
    fn accepts_only_supported_pool_types() {
        assert!(is_supported_pool(
            &DexKind::Aerodrome,
            &DiscoveryParams::AerodromeClassic { stable: false }
        ));

        assert!(!is_supported_pool(
            &DexKind::Aerodrome,
            &DiscoveryParams::AerodromeClassic { stable: true }
        ));

        assert!(is_supported_pool(
            &DexKind::UniswapV3,
            &DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: 10,
                fee: Some(500),
            }
        ));

        assert!(!is_supported_pool(
            &DexKind::AerodromeSlipstream,
            &DiscoveryParams::ConcentratedLiquidity {
                tick_spacing: 1,
                fee: Some(100),
            }
        ));
    }

    #[test]
    fn csv_escape_quotes_fields_with_commas_quotes_or_newlines() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
        assert_eq!(csv_escape("a\nb"), "\"a\nb\"");
    }

    fn test_discovered_pool(
        dex: DexKind,
        pool_address: Address,
        token0: Address,
        token1: Address,
        block_number: u64,
        stable: bool,
    ) -> DiscoveredPool {
        let (params, factory_address) = match dex {
            DexKind::Aerodrome => (
                DiscoveryParams::AerodromeClassic { stable },
                address!("0000000000000000000000000000000000000010"),
            ),
            DexKind::UniswapV3 => (
                DiscoveryParams::ConcentratedLiquidity {
                    tick_spacing: 10,
                    fee: Some(500),
                },
                address!("0000000000000000000000000000000000000011"),
            ),
            DexKind::AerodromeSlipstream => panic!("unsupported test DEX"),
        };

        DiscoveredPool {
            pool_address,
            token0_address: token0,
            token1_address: token1,
            dex,
            params,
            block_number,
            tx_hash: alloy::primitives::B256::ZERO,
            factory_address,
        }
    }

    fn write_test_catalog(pools: &[DiscoveredPool]) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let path = std::env::temp_dir().join(format!(
            "m13-catalog-test-{}-{}.csv",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ));

        std::fs::write(&path, build_catalog_csv(pools)).expect("test catalog should be written");

        path
    }

    #[test]
    fn catalog_selection_requires_matching_cross_dex_pair_and_token_in() {
        let aerodrome_ab = address!("0000000000000000000000000000000000000031");
        let v3_ab = address!("0000000000000000000000000000000000000032");

        let aerodrome_ac = address!("0000000000000000000000000000000000000033");
        let v3_ac = address!("0000000000000000000000000000000000000034");

        let token_c = address!("0000000000000000000000000000000000000003");

        let pools = vec![
            test_discovered_pool(
                DexKind::Aerodrome,
                aerodrome_ab,
                TOKEN_A,
                TOKEN_B,
                80,
                false,
            ),
            test_discovered_pool(DexKind::UniswapV3, v3_ab, TOKEN_A, TOKEN_B, 90, false),
            // This pair includes TOKEN_A but its V3 pool was created after
            // the earliest requested M10 block.
            test_discovered_pool(
                DexKind::Aerodrome,
                aerodrome_ac,
                TOKEN_A,
                token_c,
                70,
                false,
            ),
            test_discovered_pool(DexKind::UniswapV3, v3_ac, TOKEN_A, token_c, 120, false),
        ];

        let path = write_test_catalog(&pools);

        let selection = select_m10_pools_from_catalog(&path, TOKEN_A, 100).unwrap();

        std::fs::remove_file(&path).ok();

        assert_eq!(selection.matched_pairs, 1);
        assert_eq!(selection.aerodrome_pools, vec![aerodrome_ab]);
        assert_eq!(selection.uniswap_v3_pools, vec![v3_ab]);
    }

    #[test]
    fn catalog_selection_excludes_pairs_without_both_dexes() {
        let pools = vec![test_discovered_pool(
            DexKind::Aerodrome,
            address!("0000000000000000000000000000000000000041"),
            TOKEN_A,
            TOKEN_B,
            80,
            false,
        )];

        let path = write_test_catalog(&pools);
        let selection = select_m10_pools_from_catalog(&path, TOKEN_A, 100).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(selection.matched_pairs, 0);
        assert!(selection.aerodrome_pools.is_empty());
        assert!(selection.uniswap_v3_pools.is_empty());
    }

    #[test]
    fn catalog_loader_rejects_stable_aerodrome_pool() {
        let pools = vec![test_discovered_pool(
            DexKind::Aerodrome,
            address!("0000000000000000000000000000000000000051"),
            TOKEN_A,
            TOKEN_B,
            80,
            true,
        )];

        let path = write_test_catalog(&pools);
        let result = select_m10_pools_from_catalog(&path, TOKEN_A, 100);
        std::fs::remove_file(&path).ok();

        assert!(result.unwrap_err().contains("Aerodrome Classic parameters"));
    }

    #[test]
    fn catalog_loader_rejects_duplicate_pool_addresses() {
        let duplicate_address = address!("0000000000000000000000000000000000000061");

        let pools = vec![
            test_discovered_pool(
                DexKind::Aerodrome,
                duplicate_address,
                TOKEN_A,
                TOKEN_B,
                80,
                false,
            ),
            test_discovered_pool(
                DexKind::UniswapV3,
                duplicate_address,
                TOKEN_A,
                TOKEN_B,
                80,
                false,
            ),
        ];

        let path = write_test_catalog(&pools);
        let result = select_m10_pools_from_catalog(&path, TOKEN_A, 100);
        std::fs::remove_file(&path).ok();

        assert!(result.unwrap_err().contains("duplicate pool address"));
    }

    #[test]
    fn catalog_loader_rejects_wrong_header() {
        let path =
            std::env::temp_dir().join(format!("m13-invalid-header-{}.csv", std::process::id()));

        std::fs::write(&path, "wrong,header\n").unwrap();

        let result = select_m10_pools_from_catalog(&path, TOKEN_A, 100);
        std::fs::remove_file(&path).ok();

        assert!(result.unwrap_err().contains("invalid M13 catalog header"));
    }

    #[test]
    fn m10_catalog_expansion_preserves_manual_mode() {
        let original = args(&[
            "--aerodrome-pool",
            "0xcDAC0d6c6C59727a65F871236188350531885C43",
            "--uniswap-v3-pool",
            "0xd0b53D9277642d899DF5C87A3966A349A798F224",
            "--block",
            "100",
            "--token-in",
            "0x4200000000000000000000000000000000000006",
        ]);

        assert_eq!(expand_m10_catalog_args(&original).unwrap(), original);
    }

    #[test]
    fn m10_catalog_expansion_rejects_mixed_manual_and_catalog_pools() {
        let path = std::env::temp_dir().join("m13-mixed-mode-test.csv");

        let result = expand_m10_catalog_args(&args(&[
            "--pool-catalog",
            path.to_str().unwrap(),
            "--aerodrome-pool",
            "0xcDAC0d6c6C59727a65F871236188350531885C43",
        ]));

        assert!(result.unwrap_err().contains("cannot be combined"));
    }
}
