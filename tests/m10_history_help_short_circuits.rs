//! M10 CLI help and parser failures must exit before config or RPC access.

use std::process::{Command, Output};

fn run_m10(args: &[&str], label: &str) -> Output {
    let temp_dir = std::env::temp_dir().join(format!(
        "m10_history_cli_test_{label}_{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&temp_dir).expect("create isolated temp directory");
    let output = Command::new(env!("CARGO_BIN_EXE_base_arb_engine"))
        .arg("scan-arb-history")
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
        let output = run_m10(&[flag], label);
        assert!(
            output.status.success(),
            "help must succeed without config or RPC; stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout)
            .contains("Usage: cargo run -- scan-arb-history"));
    }
}

#[test]
fn invalid_arguments_fail_before_config_load_or_rpc() {
    for (args, label) in [(&[][..], "noargs"), (&["--unknown", "1"][..], "unknown")] {
        let output = run_m10(args, label);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "stderr={stderr}");
        assert!(stderr.contains("error:"), "stderr={stderr}");
        assert!(
            stderr.contains("Usage: cargo run -- scan-arb-history"),
            "stderr={stderr}"
        );
        assert!(
            !stderr.contains("missing required environment variable"),
            "parser unexpectedly reached Config::load(): {stderr}"
        );
    }
}
