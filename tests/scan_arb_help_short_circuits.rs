//! Integration test (real compiled binary, real process boundary) proving
//! `scan-arb` short-circuits before `Config::load()` and before any network
//! access, both for `--help`/`-h` and for invalid arguments.
//!
//! Follows `tests/inspect_v3_help_short_circuits.rs`: the binary is spawned
//! with `env_clear()` (no `BASE_RPC_URL` or any other config value can
//! reach `Config::load()`) and a freshly created, empty `current_dir` (no
//! `.env` can be discovered). If any of these paths fell through to
//! `Config::load()` or the network, the process would fail with a config
//! error instead of the exit status/output asserted here.

use std::process::{Command, Output};

fn run_scan_arb(args: &[&str], label: &str) -> Output {
    let temp_dir =
        std::env::temp_dir().join(format!("scan_arb_cli_test_{label}_{}", std::process::id()));
    std::fs::create_dir_all(&temp_dir)
        .expect("failed to create an isolated, empty temp dir for this test");

    let output = Command::new(env!("CARGO_BIN_EXE_base_arb_engine"))
        .arg("scan-arb")
        .args(args)
        .env_clear()
        .current_dir(&temp_dir)
        .output()
        .expect("failed to spawn the compiled binary");

    let _ = std::fs::remove_dir_all(&temp_dir);
    output
}

fn assert_help_short_circuits(help_flag: &str, label: &str) {
    let output = run_scan_arb(&[help_flag], label);
    assert!(
        output.status.success(),
        "scan-arb {help_flag} must exit successfully with a completely empty \
         environment and no discoverable .env file - a non-zero exit means it \
         fell through to Config::load() (or further).\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Usage: cargo run -- scan-arb"),
        "expected the scan-arb usage banner on stdout, got:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn scan_arb_long_help_short_circuits_before_config_load() {
    assert_help_short_circuits("--help", "long");
}

#[test]
fn scan_arb_short_help_short_circuits_before_config_load() {
    assert_help_short_circuits("-h", "short");
}

/// Argument errors must be reported (exit status 2 plus the usage text) by
/// the parser alone, BEFORE `Config::load()` - with an empty environment a
/// fall-through would instead fail with a missing-`BASE_RPC_URL` config
/// error.
#[test]
fn scan_arb_invalid_arguments_fail_before_config_load_or_rpc() {
    let cases: [(&str, &[&str]); 3] = [
        ("noargs", &[]),
        (
            "dup",
            &[
                "--aerodrome-pool",
                "0xcDAC0d6c6C59727a65F871236188350531885C43",
                "--uniswap-v3-pool",
                "0xcDAC0d6c6C59727a65F871236188350531885C43",
                "--block",
                "1",
                "--token-in",
                "0x4200000000000000000000000000000000000006",
                "--amount-in",
                "1",
            ],
        ),
        (
            "zeroamount",
            &[
                "--aerodrome-pool",
                "0xcDAC0d6c6C59727a65F871236188350531885C43",
                "--uniswap-v3-pool",
                "0xd0b53D9277642d899DF5C87A3966A349A798F224",
                "--block",
                "1",
                "--token-in",
                "0x4200000000000000000000000000000000000006",
                "--amount-in",
                "0",
            ],
        ),
    ];

    for (label, args) in cases {
        let output = run_scan_arb(args, label);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(2),
            "case {label}: expected parser exit status 2.\nstdout: {}\nstderr: {stderr}",
            String::from_utf8_lossy(&output.stdout),
        );
        assert!(
            stderr.contains("error:") && stderr.contains("Usage: cargo run -- scan-arb"),
            "case {label}: expected a parser error plus usage on stderr, got: {stderr}"
        );
        // The usage text itself mentions BASE_RPC_URL, so match the actual
        // `Config::load()` failure message instead.
        assert!(
            !stderr.contains("missing required environment variable"),
            "case {label}: reached Config::load() - validation did not run first: {stderr}"
        );
    }
}
