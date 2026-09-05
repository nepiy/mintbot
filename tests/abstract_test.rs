use assert_cmd::Command;
use nft_mint_bot::config::MintConfig;
use predicates::str::contains;

fn isolated_bot(directory: &std::path::Path) -> Command {
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("nft-mint-bot"));
    command.current_dir(directory).env_clear();
    command
}

#[test]
fn abstract_normal_and_aggressive_launcher_configs_validate() {
    let directory = tempfile::tempdir().unwrap();
    for (mode, limit) in [("normal", ""), ("aggressive", "1500000\n")] {
        isolated_bot(directory.path())
            .args(["start", "--dry-run"])
            .write_stdin(format!(
                "4\n0x0000000000000000000000000000000000000001\nabstract-drop\n1\nyes\n{mode}\n{limit}"
            ))
            .assert()
            .failure()
            .stdout(contains("Network: Abstract mainnet (chain ID 2741)"))
            .stdout(contains(format!("Execution mode: {mode}")))
            // Reaching wallet loading proves the generated config validated.
            .stderr(contains("PRIVATE_KEY is not set"));
    }
}

#[test]
fn abstract_aggressive_requires_a_tested_positive_gas_limit() {
    let directory = tempfile::tempdir().unwrap();
    for limit in ["", "0", "invalid"] {
        isolated_bot(directory.path())
            .args(["start", "--dry-run"])
            .write_stdin(format!(
                "4\n0x0000000000000000000000000000000000000001\nabstract-drop\n1\nyes\naggressive\n{limit}\n"
            ))
            .assert()
            .failure()
            .stderr(contains("Abstract gas limit must be"));
    }
}

#[test]
fn abstract_direct_setup_can_estimate_or_use_a_tested_limit() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("abstract.json");
    for (input, expected) in [("", None), ("1500000", Some(1_500_000))] {
        isolated_bot(directory.path())
            .args(["setup", "--output"])
            .arg(&path)
            .write_stdin(format!(
                "Abstract test\n2741\n0x0000000000000000000000000000000000000001\n2\nmint(uint256)\n$quantity\n0\n\n{input}\n5\n"
            ))
            .assert()
            .success();
        let config = MintConfig::load(&path).unwrap();
        assert_eq!(config.chain_id, 2741);
        assert_eq!(config.quantity, 2);
        assert_eq!(config.gas.gas_limit, expected);
    }
}

#[test]
fn abstract_rpc_profile_is_selected_without_using_robinhood_endpoints() {
    let directory = tempfile::tempdir().unwrap();
    isolated_bot(directory.path())
        .args(["rpc-test", "--chain-id", "2741"])
        .env("ABSTRACT_HTTP_RPC_URL", "http://abstract.example")
        .env("ABSTRACT_WS_RPC_URL", "wss://abstract.example/ws")
        .env("HTTP_RPC_URL", "https://generic.example")
        .env("WS_RPC_URL", "wss://generic.example")
        .env("ROBINHOOD_HTTP_RPC_URL", "https://robinhood.example")
        .assert()
        .failure()
        // URL validation fails before any network request.
        .stderr(contains("ABSTRACT_HTTP_RPC_URL"));
    isolated_bot(directory.path())
        .args(["rpc-test", "--chain-id", "2741"])
        .env("ABSTRACT_WS_RPC_URL", "wss://abstract.example/ws")
        .env("HTTP_RPC_URL", "https://generic.example")
        .assert()
        .failure()
        .stderr(contains("ABSTRACT_HTTP_RPC_URL is not set"));
    isolated_bot(directory.path())
        .args(["rpc-test", "--chain-id", "2741"])
        .assert()
        .failure()
        .stderr(contains("HTTP_RPC_URL is not set"));
}
