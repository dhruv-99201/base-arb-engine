//! Proves `scan-arb-sizes` help and parser errors exit before config loading
//! or any RPC access, using an empty environment and empty working directory.

use std::process::{Command, Output};

fn run_m9(args: &[&str], label: &str) -> Output {
    let temp_dir = std::env::temp_dir().join(format!(
        "m9_size_sweep_cli_test_{label}_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&temp_dir).expect("create isolated temp directory");
    let output = Command::new(env!("CARGO_BIN_EXE_base_arb_engine"))
        .arg("scan-arb-sizes")
        .args(args)
        .env_clear()
        .current_dir(&temp_dir)
        .output()
        .expect("spawn compiled engine binary");
    let _ = std::fs::remove_dir_all(&temp_dir);
    output
}

#[test]
fn help_short_circuits_before_config_load() {
    for (flag, label) in [("--help", "long"), ("-h", "short")] {
        let output = run_m9(&[flag], label);
        assert!(
            output.status.success(),
            "help must succeed without config or RPC; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("Usage: cargo run -- scan-arb-sizes")
        );
    }
}

#[test]
fn invalid_arguments_fail_before_config_load_or_rpc() {
    for (args, label) in [(&[][..], "noargs"), (&["--unknown", "1"][..], "unknown")] {
        let output = run_m9(args, label);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "stderr={stderr}");
        assert!(stderr.contains("error:"), "stderr={stderr}");
        assert!(
            stderr.contains("Usage: cargo run -- scan-arb-sizes"),
            "stderr={stderr}"
        );
        assert!(
            !stderr.contains("missing required environment variable"),
            "parser unexpectedly reached Config::load(): {stderr}"
        );
    }
}
