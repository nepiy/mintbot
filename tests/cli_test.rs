use clap::Parser;
use nft_mint_bot::cli::Cli;

#[test]
fn no_subcommand_selects_interactive_startup() {
    let cli = Cli::try_parse_from(["nft-mint-bot"]).expect("no-argument CLI should parse");
    assert!(cli.command.is_none());
}

#[test]
fn startup_does_not_load_a_parent_dotenv() {
    let directory = tempfile::tempdir().unwrap();
    let child = directory.path().join("child");
    std::fs::create_dir(&child).unwrap();
    std::fs::write(
        directory.path().join(".env"),
        "PRIVATE_KEY=invalid-parent-key\n",
    )
    .unwrap();
    let config = child.join("mint.json");
    std::fs::write(
        &config,
        r#"{
        "name":"test", "chain_id":1,
        "contract_address":"0x0000000000000000000000000000000000000001",
        "quantity":1,
        "mint":{"function":"mint(uint256)","arguments":["$quantity"]},
        "trigger":{"type":"manual"}
    }"#,
    )
    .unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("nft-mint-bot"))
        .current_dir(&child)
        .env_remove("PRIVATE_KEY")
        .args(["simulate", "--config"])
        .arg(config)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PRIVATE_KEY is not set"));
}

#[cfg(unix)]
#[test]
fn startup_rejects_insecure_and_malformed_local_dotenv_without_leaking_values() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let env_path = directory.path().join(".env");
    std::fs::write(&env_path, "PRIVATE_KEY='dummy-sensitive-value\n").unwrap();
    for (mode, expected) in [(0o644, "permissions"), (0o600, "could not load .env")] {
        std::fs::set_permissions(&env_path, std::fs::Permissions::from_mode(mode)).unwrap();
        let output = std::process::Command::new(assert_cmd::cargo::cargo_bin!("nft-mint-bot"))
            .current_dir(directory.path())
            .arg("--help")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "{stderr}");
        assert!(!stderr.contains("dummy-sensitive-value"));
    }
}
