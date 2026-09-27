//! Integration test (real compiled binary, real process boundary) proving
//! `inspect-v3 --help`/`-h` short-circuit before `Config::load()` and
//! before any network access.
//!
//! A parser-only unit test (`parse_inspect_v3_args` returning
//! `InspectV3Command::Help`) is NOT sufficient proof of this: that
//! function never touches `Config::load()` or the network either way, so
//! it cannot show anything about `main()`'s actual dispatch order. This
//! test instead spawns the ACTUAL compiled `inspect-v3` binary as a
//! subprocess, with:
//!   - `env_clear()` - a completely empty process environment, so no
//!     `BASE_RPC_URL` or any other config value can reach `Config::load()`
//!     via a real environment variable, and
//!   - `current_dir` set to a freshly created, otherwise-empty temporary
//!     directory - so no `.env` file from the real project root can be
//!     discovered either.
//!
//! If `--help` truly short-circuits before `Config::load()`/network (as
//! `main.rs`'s dispatch is written to do), the process must still exit
//! successfully and print the `inspect-v3` usage banner, REGARDLESS of
//! this empty environment. If a future change accidentally made `--help`
//! fall through to `Config::load()` first, that call would fail against
//! this deliberately-empty environment and the process would exit
//! non-zero instead - which is exactly the regression this test catches.
//!
//! Assumption flagged: this assumes the compiled binary's Cargo target
//! name is `base_arb_engine` (matching `[package] name = "base_arb_engine"`
//! from the `Cargo.toml` on file, which was NOT part of the `cd4e701`
//! git-archive supplied for this round - only `src/cli.rs`, `src/main.rs`,
//! `src/pricing/v3_quote.rs`, and `src/dex/uniswap_v3.rs` were). If the
//! real `Cargo.toml` defines a different `[[bin]] name`, the
//! `env!("CARGO_BIN_EXE_...")` line below needs updating to match, or this
//! file will fail to compile (a hard compile-time error from the `env!`
//! macro, not a silent runtime failure) - `cargo test` will report exactly
//! which name it expected.

use std::process::Command;

fn run_inspect_v3_help(help_flag: &str, label: &str) {
    let temp_dir = std::env::temp_dir().join(format!(
        "inspect_v3_help_test_{label}_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&temp_dir)
        .expect("failed to create an isolated, empty temp dir for this test");

    let output = Command::new(env!("CARGO_BIN_EXE_base_arb_engine"))
        .arg("inspect-v3")
        .arg(help_flag)
        .env_clear()
        .current_dir(&temp_dir)
        .output()
        .expect("failed to spawn the compiled binary");

    let _ = std::fs::remove_dir_all(&temp_dir);

    assert!(
        output.status.success(),
        "inspect-v3 {help_flag} must exit successfully with a completely \
         empty environment and no discoverable .env file - a non-zero \
         exit means it fell through to Config::load() (or further), not a \
         true short-circuit.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Usage: cargo run -- inspect-v3"),
        "expected the inspect-v3 usage banner on stdout, got:\nstdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn inspect_v3_long_help_short_circuits_before_config_load() {
    run_inspect_v3_help("--help", "long");
}

#[test]
fn inspect_v3_short_help_short_circuits_before_config_load() {
    run_inspect_v3_help("-h", "short");
}
