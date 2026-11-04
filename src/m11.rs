use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const M10_COLUMNS: [&str; 19] = [
    "block",
    "token_in",
    "size_in_raw",
    "route_index",
    "start_token",
    "intermediate_token",
    "leg1_dex",
    "leg1_pool",
    "leg2_dex",
    "leg2_pool",
    "status",
    "intermediate_amount_raw",
    "final_amount_raw",
    "gross_profit_raw",
    "estimated_cost_raw",
    "estimated_net_profit_raw",
    "net_profit_status",
    "failure_kind",
    "failure_detail",
];

pub const M11_ANALYTICS_USAGE: &str = "\
Usage: cargo run -- analyze-arb-history --input <M10_CSV> --summary-csv <PATH> --report-out <PATH>

M11 offline analytics for an existing M10 historical-sweep CSV.
No RPC connection is made and the input file is never modified.

Options:
  --input <PATH>        Existing M10 CSV file; required.
  --summary-csv <PATH>  New grouped summary CSV; required.
  --report-out <PATH>   New human-readable report; required.
  --help, -h            Show this help and exit.
";

#[derive(Debug, Clone, PartialEq, Eq)]
struct M11Args {
    input: PathBuf,
    summary_csv: PathBuf,
    report_out: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum M11Command {
    Help,
    Run(M11Args),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey {
    size_in_raw: i128,
    start_token: String,
    intermediate_token: String,
    leg1_dex: String,
    leg1_pool: String,
    leg2_dex: String,
    leg2_pool: String,
}

#[derive(Debug, Default)]
struct GroupAggregate {
    quote_count: usize,
    gross_profitable_quotes: usize,
    total_gross_profit_raw: i128,
    gross_profits: Vec<i128>,
    returns_bps_x100: Vec<i128>,
    evaluated_blocks: BTreeSet<u64>,
    profitable_blocks: BTreeSet<u64>,
}

#[derive(Debug, Default)]
struct Analytics {
    data_rows: usize,
    pool_rows: usize,
    no_route_rows: usize,
    failed_rows: usize,
    other_rows: usize,
    successful_quotes: usize,
    gross_profitable_quotes: usize,
    evaluated_blocks: BTreeSet<u64>,
    profitable_blocks: BTreeSet<u64>,
    groups: BTreeMap<GroupKey, GroupAggregate>,
}

fn parse_m11_args(args: &[String]) -> Result<M11Command, String> {
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        return Ok(M11Command::Help);
    }

    let mut input = None;
    let mut summary_csv = None;
    let mut report_out = None;
    let mut index = 0;

    while index < args.len() {
        let flag = args[index].as_str();
        let value = args
            .get(index + 1)
            .ok_or_else(|| format!("{flag} requires a value"))?;
        match flag {
            "--input" => input = Some(PathBuf::from(value)),
            "--summary-csv" => summary_csv = Some(PathBuf::from(value)),
            "--report-out" => report_out = Some(PathBuf::from(value)),
            other => return Err(format!("unrecognized argument: '{other}'")),
        }
        index += 2;
    }

    let args = M11Args {
        input: input.ok_or_else(|| "missing required --input <M10_CSV>".to_string())?,
        summary_csv: summary_csv
            .ok_or_else(|| "missing required --summary-csv <PATH>".to_string())?,
        report_out: report_out.ok_or_else(|| "missing required --report-out <PATH>".to_string())?,
    };

    if args.input.as_os_str().is_empty()
        || args.summary_csv.as_os_str().is_empty()
        || args.report_out.as_os_str().is_empty()
    {
        return Err("input and output paths must not be empty".into());
    }

    Ok(M11Command::Run(args))
}

pub fn run_m11_cli(args: &[String]) -> Result<(), String> {
    match parse_m11_args(args)? {
        M11Command::Help => {
            println!("{M11_ANALYTICS_USAGE}");
            Ok(())
        }
        M11Command::Run(args) => run_analytics(args),
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.exists() {
        fs::canonicalize(path)
            .map_err(|error| format!("could not resolve path {}: {error}", path.display()))
    } else {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = fs::canonicalize(parent).map_err(|error| {
            format!(
                "could not resolve parent directory {}: {error}",
                parent.display()
            )
        })?;
        let name = path
            .file_name()
            .ok_or_else(|| format!("output path has no filename: {}", path.display()))?;
        Ok(parent.join(name))
    }
}

fn validate_paths(args: &M11Args) -> Result<(), String> {
    if !args.input.is_file() {
        return Err(format!(
            "input CSV does not exist or is not a file: {}",
            args.input.display()
        ));
    }
    if args.summary_csv.exists() {
        return Err(format!(
            "summary CSV already exists; choose a new path: {}",
            args.summary_csv.display()
        ));
    }
    if args.report_out.exists() {
        return Err(format!(
            "report file already exists; choose a new path: {}",
            args.report_out.display()
        ));
    }

    let input = absolute_path(&args.input)?;
    let summary = absolute_path(&args.summary_csv)?;
    let report = absolute_path(&args.report_out)?;
    if input == summary || input == report || summary == report {
        return Err("input, summary CSV, and report paths must all be different".into());
    }
    Ok(())
}

fn parse_csv(text: &str) -> Result<Vec<Vec<String>>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut after_quote = false;
    let mut index = 0;

    while index < chars.len() {
        let ch = chars[index];
        if in_quotes {
            if ch == '"' {
                if index + 1 < chars.len() && chars[index + 1] == '"' {
                    field.push('"');
                    index += 2;
                    continue;
                }
                in_quotes = false;
                after_quote = true;
            } else {
                field.push(ch);
            }
            index += 1;
            continue;
        }

        if after_quote {
            match ch {
                ',' => {
                    row.push(std::mem::take(&mut field));
                    after_quote = false;
                }
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                    after_quote = false;
                }
                '\r' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                    after_quote = false;
                    if index + 1 < chars.len() && chars[index + 1] == '\n' {
                        index += 1;
                    }
                }
                ' ' | '\t' => {}
                other => {
                    return Err(format!(
                    "unexpected character '{other}' after closing CSV quote at character {index}"
                ))
                }
            }
            index += 1;
            continue;
        }

        match ch {
            '"' if field.is_empty() => in_quotes = true,
            '"' => {
                return Err(format!(
                    "unexpected quote inside unquoted CSV field at character {index}"
                ))
            }
            ',' => row.push(std::mem::take(&mut field)),
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\r' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                if index + 1 < chars.len() && chars[index + 1] == '\n' {
                    index += 1;
                }
            }
            other => field.push(other),
        }
        index += 1;
    }

    if in_quotes {
        return Err("unterminated quoted field in CSV input".into());
    }
    if after_quote || !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    if rows.is_empty() {
        return Err("input CSV is empty".into());
    }
    Ok(rows)
}

fn csv_escape(value: &str) -> String {
    if value.contains(',') || value.contains('"') || value.contains('\n') || value.contains('\r') {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn field<'a>(
    row: &'a [String],
    columns: &BTreeMap<String, usize>,
    name: &str,
) -> Result<&'a str, String> {
    let index = columns
        .get(name)
        .ok_or_else(|| format!("required M10 CSV column is missing: {name}"))?;
    row.get(*index)
        .map(String::as_str)
        .ok_or_else(|| format!("row is missing column {name}"))
}

fn parse_i128(value: &str, label: &str, row_number: usize) -> Result<i128, String> {
    value.parse::<i128>().map_err(|_| {
        format!("row {row_number}: {label}='{value}' is not a valid signed 128-bit integer")
    })
}

fn format_scaled(value: i128, scale: u128) -> String {
    let magnitude = value.unsigned_abs();
    let scale = scale.max(1);
    let whole = magnitude / scale;
    let fraction = magnitude % scale;
    let digits = scale.to_string().len() - 1;
    if digits == 0 {
        format!("{}{}", if value < 0 { "-" } else { "" }, whole)
    } else {
        format!(
            "{}{}.{:0width$}",
            if value < 0 { "-" } else { "" },
            whole,
            fraction,
            width = digits
        )
    }
}

fn median(values: &[i128]) -> Result<i128, String> {
    if values.is_empty() {
        return Err("cannot calculate median of an empty sample".into());
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 1 {
        Ok(sorted[middle])
    } else {
        let left = sorted[middle - 1];
        let right = sorted[middle];
        left.checked_add(right)
            .map(|sum| sum / 2)
            .ok_or_else(|| "median arithmetic overflow".into())
    }
}

fn percent_x100(numerator: usize, denominator: usize) -> Option<u128> {
    if denominator == 0 {
        return None;
    }
    (numerator as u128)
        .checked_mul(10_000)
        .map(|scaled| scaled / denominator as u128)
}

fn analyze_rows(rows: &[Vec<String>]) -> Result<Analytics, String> {
    if rows.is_empty() {
        return Err("input CSV is empty".into());
    }
    let header = &rows[0];
    let mut columns = BTreeMap::new();
    for (index, name) in header.iter().enumerate() {
        if columns.insert(name.clone(), index).is_some() {
            return Err(format!("duplicate CSV column name: {name}"));
        }
    }
    for required in M10_COLUMNS {
        if !columns.contains_key(required) {
            return Err(format!(
                "input is not a recognized M10 CSV: missing column '{required}'"
            ));
        }
    }

    let mut analytics = Analytics::default();
    for (offset, row) in rows.iter().enumerate().skip(1) {
        let row_number = offset + 1;
        if row.len() != header.len() {
            return Err(format!(
                "row {row_number} has {} fields; header has {}. Check CSV quoting or file integrity",
                row.len(),
                header.len()
            ));
        }
        analytics.data_rows += 1;
        let status = field(row, &columns, "status")?;
        match status {
            "POOL_LOADED" | "POOL_FAILED" => {
                analytics.pool_rows += 1;
            }
            "NO_ROUTE" => {
                analytics.no_route_rows += 1;
            }
            "FAILED" => {
                analytics.failed_rows += 1;
            }
            "OK" => {
                let block = field(row, &columns, "block")?
                    .parse::<u64>()
                    .map_err(|_| format!("row {row_number}: invalid block number"))?;
                let size = parse_i128(
                    field(row, &columns, "size_in_raw")?,
                    "size_in_raw",
                    row_number,
                )?;
                if size <= 0 {
                    return Err(format!("row {row_number}: size_in_raw must be positive"));
                }
                let gross = parse_i128(
                    field(row, &columns, "gross_profit_raw")?,
                    "gross_profit_raw",
                    row_number,
                )?;
                let leg1 = field(row, &columns, "leg1_dex")?.to_ascii_lowercase();
                let leg2 = field(row, &columns, "leg2_dex")?.to_ascii_lowercase();
                let start_token = field(row, &columns, "start_token")?.to_ascii_lowercase();
                let intermediate_token =
                    field(row, &columns, "intermediate_token")?.to_ascii_lowercase();
                let leg1_pool = field(row, &columns, "leg1_pool")?.to_ascii_lowercase();
                let leg2_pool = field(row, &columns, "leg2_pool")?.to_ascii_lowercase();
                if leg1.is_empty()
                    || leg2.is_empty()
                    || start_token.is_empty()
                    || intermediate_token.is_empty()
                    || leg1_pool.is_empty()
                    || leg2_pool.is_empty()
                {
                    return Err(format!(
                        "row {row_number}: successful quote is missing route identity"
                    ));
                }
                let return_bps_x100 = gross.checked_mul(1_000_000).ok_or_else(|| {
                    format!("row {row_number}: return basis-point scaling overflow")
                })? / size;

                let key = GroupKey {
                    size_in_raw: size,
                    start_token,
                    intermediate_token,
                    leg1_dex: leg1,
                    leg1_pool,
                    leg2_dex: leg2,
                    leg2_pool,
                };
                let group = analytics.groups.entry(key).or_default();
                group.quote_count += 1;
                group.total_gross_profit_raw = group
                    .total_gross_profit_raw
                    .checked_add(gross)
                    .ok_or_else(|| {
                        format!("row {row_number}: grouped gross-profit sum overflow")
                    })?;
                group.gross_profits.push(gross);
                group.returns_bps_x100.push(return_bps_x100);
                group.evaluated_blocks.insert(block);
                analytics.evaluated_blocks.insert(block);
                if gross > 0 {
                    group.gross_profitable_quotes += 1;
                    group.profitable_blocks.insert(block);
                    analytics.gross_profitable_quotes += 1;
                    analytics.profitable_blocks.insert(block);
                }
                analytics.successful_quotes += 1;
            }
            _ => analytics.other_rows += 1,
        }
    }
    Ok(analytics)
}

fn build_summary_csv(analytics: &Analytics) -> Result<String, String> {
    let mut output = String::from(
    "size_in_raw,start_token,intermediate_token,leg1_dex,leg1_pool,leg2_dex,leg2_pool,quote_count,gross_profitable_quotes,profitable_quote_rate_pct_x100,evaluable_blocks,profitable_blocks,block_hit_rate_pct_x100,total_gross_profit_raw,mean_gross_profit_raw,best_gross_profit_raw,worst_gross_profit_raw,mean_return_bps_x100,median_return_bps_x100,best_return_bps_x100,worst_return_bps_x100\n",
);

    for (key, group) in &analytics.groups {
        let count = group.quote_count as i128;
        let mean_profit = group.total_gross_profit_raw / count;
        let mean_return = group
            .returns_bps_x100
            .iter()
            .try_fold(0_i128, |sum, value| sum.checked_add(*value))
            .ok_or_else(|| "mean return arithmetic overflow".to_string())?
            / count;
        let median_return = median(&group.returns_bps_x100)?;
        let best_profit = *group.gross_profits.iter().max().expect("nonempty group");
        let worst_profit = *group.gross_profits.iter().min().expect("nonempty group");
        let best_return = *group.returns_bps_x100.iter().max().expect("nonempty group");
        let worst_return = *group.returns_bps_x100.iter().min().expect("nonempty group");
        let quote_rate = percent_x100(group.gross_profitable_quotes, group.quote_count)
            .expect("group has quotes");
        let block_rate = percent_x100(group.profitable_blocks.len(), group.evaluated_blocks.len());
        let fields = vec![
            key.size_in_raw.to_string(),
            key.start_token.clone(),
            key.intermediate_token.clone(),
            key.leg1_dex.clone(),
            key.leg1_pool.clone(),
            key.leg2_dex.clone(),
            key.leg2_pool.clone(),
            group.quote_count.to_string(),
            group.gross_profitable_quotes.to_string(),
            quote_rate.to_string(),
            group.evaluated_blocks.len().to_string(),
            group.profitable_blocks.len().to_string(),
            block_rate
                .map(|value| value.to_string())
                .unwrap_or_else(|| "NOT_AVAILABLE".into()),
            group.total_gross_profit_raw.to_string(),
            mean_profit.to_string(),
            best_profit.to_string(),
            worst_profit.to_string(),
            mean_return.to_string(),
            median_return.to_string(),
            best_return.to_string(),
            worst_return.to_string(),
        ];
        output.push_str(
            &fields
                .iter()
                .map(|value| csv_escape(value))
                .collect::<Vec<_>>()
                .join(","),
        );
        output.push('\n');
    }
    Ok(output)
}

fn build_report(analytics: &Analytics, input: &Path, summary_csv: &Path) -> String {
    let hit_rate = percent_x100(
        analytics.profitable_blocks.len(),
        analytics.evaluated_blocks.len(),
    )
    .map(|value| value.to_string())
    .unwrap_or_else(|| "NOT_AVAILABLE".into());
    let mut report = String::new();
    report.push_str("M11 offline historical sweep analytics\n");
    report.push_str(&format!("input_csv={}\n", input.display()));
    report.push_str(&format!("summary_csv={}\n", summary_csv.display()));
    report.push_str(&format!(
        "data_rows={}\npool_rows={}\nno_route_rows={}\nfailed_route_rows={}\nother_rows={}\n",
        analytics.data_rows,
        analytics.pool_rows,
        analytics.no_route_rows,
        analytics.failed_rows,
        analytics.other_rows
    ));
    report.push_str(&format!(
        "successful_route_quotes={}\ngross_profitable_quotes={}\nevaluable_blocks={}\nblocks_with_gross_profitable_quote={}\ngross_profitable_block_hit_rate_pct_x100={}\ngroup_count={}\n",
        analytics.successful_quotes,
        analytics.gross_profitable_quotes,
        analytics.evaluated_blocks.len(),
        analytics.profitable_blocks.len(),
        hit_rate,
        analytics.groups.len()
    ));
    report.push_str("return_basis= gross_profit_raw / size_in_raw, expressed as basis points multiplied by 100; integer truncation toward zero\n");
    report.push_str("net_profit=NOT_ANALYZED; supplied M10 cost is an operator assumption, not a verified gas model\n");
    report.push_str("method=independent historical block snapshots; no capital, inventory, compounding, execution, or state carry-over\n");
    report.push_str("\nGROUPS\n");
    for (key, group) in &analytics.groups {
        let mean_return = group
            .returns_bps_x100
            .iter()
            .try_fold(0_i128, |sum, value| sum.checked_add(*value))
            .map(|sum| sum / group.quote_count as i128);
        let mean_return = mean_return
            .map(|value| format_scaled(value, 100))
            .unwrap_or_else(|| "OVERFLOW".into());
        report.push_str(&format!(
            "size_in_raw={} tokens={}->{} route={}[{}]->{}[{}] quotes={} profitable_quotes={} mean_return_bps={}\n",
            key.size_in_raw,
            key.start_token,
            key.intermediate_token,
            key.leg1_dex,
            key.leg1_pool,
            key.leg2_dex,
            key.leg2_pool,
            group.quote_count,
            group.gross_profitable_quotes,
            mean_return
        ));
    }
    report.push_str("\nM11 is descriptive research output, not a live trading signal. No RPC, signing, or transaction submission is performed.\n");
    report
}

fn run_analytics(args: M11Args) -> Result<(), String> {
    validate_paths(&args)?;
    let input = fs::read_to_string(&args.input)
        .map_err(|error| format!("could not read {}: {error}", args.input.display()))?;
    let rows = parse_csv(&input)?;
    let analytics = analyze_rows(&rows)?;
    let summary = build_summary_csv(&analytics)?;
    let report = build_report(&analytics, &args.input, &args.summary_csv);

    fs::write(&args.summary_csv, summary)
        .map_err(|error| format!("could not write {}: {error}", args.summary_csv.display()))?;
    if let Err(error) = fs::write(&args.report_out, report) {
        let _ = fs::remove_file(&args.summary_csv);
        return Err(format!(
            "could not write {}; summary CSV was removed: {error}",
            args.report_out.display()
        ));
    }

    println!("M11 analytics complete");
    println!("input_rows={}", analytics.data_rows);
    println!("pool_rows={}", analytics.pool_rows);
    println!("no_route_rows={}", analytics.no_route_rows);
    println!("failed_route_rows={}", analytics.failed_rows);
    println!("successful_route_quotes={}", analytics.successful_quotes);
    println!(
        "gross_profitable_quotes={}",
        analytics.gross_profitable_quotes
    );
    println!("summary_csv={}", args.summary_csv.display());
    println!("report_out={}", args.report_out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_csv() -> String {
        let mut csv = M10_COLUMNS.join(",");
        csv.push('\n');
        for (block, size, gross, leg1, leg2) in [
            (
                "100",
                "1000000000000000",
                "-1000000000000",
                "aerodrome",
                "uniswap_v3",
            ),
            (
                "101",
                "1000000000000000",
                "1000000000000",
                "aerodrome",
                "uniswap_v3",
            ),
            (
                "100",
                "1000000000000000",
                "-2000000000000",
                "uniswap_v3",
                "aerodrome",
            ),
        ] {
            csv.push_str(&format!(
                "{block},0x01,{size},1,0x01,0x02,{leg1},0x03,{leg2},0x04,OK,10,20,{gross},,,, ,\n"
            ));
        }
        csv
    }

    #[test]
    fn csv_parser_handles_quoted_commas_quotes_and_embedded_newlines() {
        let parsed = parse_csv("a,b\n1,\"hello, \"\"world\"\"\nagain\"\n").unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1][1], "hello, \"world\"\nagain");
    }

    #[test]
    fn csv_parser_rejects_unterminated_quotes() {
        assert!(parse_csv("a,b\n1,\"unfinished\n")
            .unwrap_err()
            .contains("unterminated"));
    }

    #[test]
    fn parser_help_short_circuits_invalid_arguments() {
        assert_eq!(
            parse_m11_args(&["--bad".into(), "--help".into()]).unwrap(),
            M11Command::Help
        );
    }

    #[test]
    fn parser_requires_all_paths() {
        assert!(parse_m11_args(&["--input".into(), "x.csv".into()])
            .unwrap_err()
            .contains("--summary-csv"));
    }
    #[test]
    fn analytics_groups_by_size_and_ordered_route_and_counts_valid_quotes() {
        let rows = parse_csv(&fixture_csv()).unwrap();
        let analytics = analyze_rows(&rows).unwrap();

        assert_eq!(analytics.successful_quotes, 3);
        assert_eq!(analytics.groups.len(), 2);

        let forward = GroupKey {
            size_in_raw: 1_000_000_000_000_000,
            start_token: "0x01".into(),
            intermediate_token: "0x02".into(),
            leg1_dex: "aerodrome".into(),
            leg1_pool: "0x03".into(),
            leg2_dex: "uniswap_v3".into(),
            leg2_pool: "0x04".into(),
        };

        let group = analytics.groups.get(&forward).unwrap();

        assert_eq!(group.quote_count, 2);
        assert_eq!(group.gross_profitable_quotes, 1);
        assert_eq!(group.evaluated_blocks.len(), 2);
        assert_eq!(group.profitable_blocks.len(), 1);
    }

    #[test]
    fn different_pool_combinations_are_kept_in_separate_groups() {
        let mut rows = parse_csv(&fixture_csv()).unwrap();

        // Duplicate a successful Aerodrome -> Uniswap V3 quote,
        // changing only the Aerodrome pool address.
        let mut alternate = rows[1].clone();
        alternate[0] = "102".into();
        alternate[7] = "0x05".into();
        rows.push(alternate);

        let analytics = analyze_rows(&rows).unwrap();

        assert_eq!(analytics.groups.len(), 3);

        let alternate_key = GroupKey {
            size_in_raw: 1_000_000_000_000_000,
            start_token: "0x01".into(),
            intermediate_token: "0x02".into(),
            leg1_dex: "aerodrome".into(),
            leg1_pool: "0x05".into(),
            leg2_dex: "uniswap_v3".into(),
            leg2_pool: "0x04".into(),
        };

        let group = analytics.groups.get(&alternate_key).unwrap();
        assert_eq!(group.quote_count, 1);
    }

    #[test]
    fn zero_evaluable_blocks_have_no_hit_rate() {
        assert_eq!(percent_x100(0, 0), None);
    }

    #[test]
    fn fixed_point_basis_points_are_exact_integer_values() {
        let gross = -2_203_343_975_828_i128;
        let size = 1_000_000_000_000_000_i128;
        assert_eq!(gross * 1_000_000 / size, -2_203);
        assert_eq!(format_scaled(-2_203, 100), "-22.03");
    }
}
