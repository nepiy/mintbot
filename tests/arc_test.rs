use assert_cmd::Command;
use nft_mint_bot::config::MintConfig;
use predicates::str::contains;

fn isolated_bot(directory: &std::path::Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nft-mint-bot"));
    command.current_dir(directory).env_clear();
    command
}

#[test]
fn arc_opensea_normal_and_aggressive_launcher_configs_validate() {
    let directory = tempfile::tempdir().unwrap();
    for (mode, limit) in [("normal", ""), ("aggressive", "300000\n")] {
        isolated_bot(directory.path())
            .args(["start", "--dry-run"])
            .write_stdin(format!(
                "5\n0x0000000000000000000000000000000000000001\narchouses\n1\nyes\n{mode}\n{limit}"
            ))
            .assert()
            .failure()
            .stdout(contains("Network: Arc mainnet (chain ID 5042)"))
            .stdout(contains(format!("Execution mode: {mode}")))
            .stderr(contains("PRIVATE_KEY is not set"));
    }
}

#[test]
fn arc_direct_setup_uses_live_estimation_when_no_tested_limit_is_given() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("arc.json");
    isolated_bot(directory.path())
        .args(["setup", "--output"])
        .arg(&path)
        .write_stdin(
            "Arc test\n5042\n0x0000000000000000000000000000000000000001\n1\nmint(uint256)\n$quantity\n0\n\n\n5\n",
        )
        .assert()
        .success();

    let config = MintConfig::load(&path).unwrap();
    assert_eq!(config.chain_id, 5042);
    assert_eq!(config.native_currency.as_deref(), Some("USDC"));
    assert_eq!(config.gas.gas_limit, None);
}

#[test]
fn arc_aggressive_launcher_rejects_missing_tested_limit() {
    let directory = tempfile::tempdir().unwrap();
    isolated_bot(directory.path())
        .args(["start", "--dry-run"])
        .write_stdin(
            "5\n0x0000000000000000000000000000000000000001\narchouses\n1\nyes\naggressive\n",
        )
        .assert()
        .failure()
        .stderr(contains("Arc gas limit must be"));
}
