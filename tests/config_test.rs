use nft_mint_bot::config::{
    MintConfig, MintTrigger, OpenSeaExecutionMode, parse_gwei, parse_native_amount,
};
use std::fs;

#[test]
fn loads_a_collection_config_with_defaults() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("mint.json");
    fs::write(
        &path,
        r#"{
          "name": "Test collection",
          "chain_id": 31337,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 2,
          "mint": {
            "function": "mint(uint256)",
            "arguments": ["$quantity"],
            "price_per_nft": "0.001"
          },
          "trigger": { "type": "manual" },
          "gas": { "mode": "auto", "multiplier": 1.1 }
        }"#,
    )
    .expect("write config");

    let config = MintConfig::load(&path).expect("config should load");
    assert_eq!(config.quantity, 2);
    assert!(matches!(config.trigger, MintTrigger::Manual));
    assert_eq!(
        config.mint_value_wei().unwrap().to_string(),
        "2000000000000000"
    );
}

#[test]
fn parses_native_units_and_gwei_without_floating_point() {
    assert_eq!(
        parse_native_amount("1.25").unwrap().to_string(),
        "1250000000000000000"
    );
    assert_eq!(parse_gwei("2.5").unwrap(), 2_500_000_000);
    assert!(parse_gwei("2.5000000001").is_err());
}

#[test]
fn rejects_unsafe_replacement_settings_before_arming() {
    let mut config: MintConfig = serde_json::from_str(
        r#"{
          "name": "Unsafe replacement",
          "chain_id": 1,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 1,
          "mint": { "function": "mint(uint256)", "arguments": ["$quantity"] },
          "trigger": { "type": "manual" },
          "replacement": {
            "enabled": true,
            "after_blocks": 0,
            "fee_multiplier": 1.0,
            "max_attempts": 2
          }
        }"#,
    )
    .expect("valid JSON shape");
    assert!(config.validate().is_err());
    config.replacement.after_blocks = 2;
    assert!(config.validate().is_err());
    config.replacement.fee_multiplier = 1.15;
    assert!(config.validate().is_ok());
}

#[test]
fn validates_opensea_drop_slug() {
    let mut config: MintConfig = serde_json::from_str(
        r#"{
          "name": "OpenSea drop",
          "chain_id": 4663,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "opensea_drop_slug": "robinhood-drop-2026",
          "require_zero_value": true,
          "quantity": 1,
          "mint": { "function": "mint(uint256)" },
          "trigger": { "type": "block_timestamp", "timestamp": 0 }
        }"#,
    )
    .expect("valid JSON shape");
    assert!(config.validate().is_ok());
    config.opensea_drop_slug = Some("not/a-safe-slug".to_string());
    assert!(config.validate().is_err());
}

#[test]
fn aggressive_opensea_mode_requires_a_fixed_gas_limit_and_cost_cap() {
    let mut config: MintConfig = serde_json::from_str(
        r#"{
          "name": "Aggressive OpenSea drop",
          "chain_id": 4663,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "opensea_drop_slug": "aggressive-drop",
          "opensea_execution_mode": "aggressive",
          "require_zero_value": true,
          "quantity": 1,
          "mint": { "function": "mint(uint256)" },
          "trigger": { "type": "block_timestamp", "timestamp": 0 }
        }"#,
    )
    .expect("valid JSON shape");
    assert!(matches!(
        config.opensea_execution_mode,
        OpenSeaExecutionMode::Aggressive
    ));
    assert!(config.validate().is_err());
    config.gas.gas_limit = Some(230_000);
    assert!(config.validate().is_err());
    config.gas.max_total_gas_cost_native = Some("0.001".to_string());
    assert!(config.validate().is_ok());
}

#[test]
fn rejects_terminal_control_characters_in_collection_name() {
    let config: MintConfig = serde_json::from_str(
        r#"{
          "name": "Unsafe\u001b[31m name",
          "chain_id": 31337,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 1,
          "mint": { "function": "mint(uint256)" },
          "trigger": { "type": "manual" }
        }"#,
    )
    .expect("valid JSON shape");
    assert!(config.validate().is_err());
}

#[test]
fn rejects_asset_movement_in_direct_mode() {
    let config: MintConfig = serde_json::from_str(
        r#"{
          "name": "Not a mint",
          "chain_id": 1,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 1,
          "mint": {
            "function": "setApprovalForAll(address,bool)",
            "arguments": ["0x0000000000000000000000000000000000000002", "true"]
          },
          "trigger": { "type": "manual" }
        }"#,
    )
    .expect("valid JSON shape");
    assert!(config.validate().is_err());
}

#[test]
fn validates_optional_contract_code_hash_format() {
    let mut config: MintConfig = serde_json::from_str(
        r#"{
          "name": "Pinned contract",
          "chain_id": 1,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "expected_contract_code_hash": "0x1111111111111111111111111111111111111111111111111111111111111111",
          "quantity": 1,
          "mint": { "function": "mint(uint256)", "arguments": ["$quantity"] },
          "trigger": { "type": "manual" }
        }"#,
    )
    .expect("valid JSON shape");
    assert!(config.validate().is_ok());
    config.expected_contract_code_hash = Some("1111".to_string());
    assert!(config.validate().is_err());
}

#[test]
fn rejects_unknown_fields_instead_of_ignoring_misspelled_safety_settings() {
    let parsed = serde_json::from_str::<MintConfig>(
        r#"{
          "name": "Misspelled cap",
          "chain_id": 1,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 1,
          "mint": { "function": "mint(uint256)", "arguments": ["$quantity"] },
          "trigger": { "type": "manual" },
          "gas": { "max_total_gas_cost_nativ": "0.001" }
        }"#,
    );
    assert!(parsed.is_err());
}

#[test]
fn paid_opensea_mints_require_an_explicit_price_cap() {
    let mut config: MintConfig = serde_json::from_str(
        r#"{
          "name": "Paid OpenSea drop",
          "chain_id": 4663,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "opensea_drop_slug": "paid-drop",
          "quantity": 2,
          "mint": { "function": "mint(uint256)" },
          "trigger": { "type": "block_timestamp", "timestamp": 0 }
        }"#,
    )
    .expect("valid JSON shape");
    assert!(config.validate().is_err());
    config.max_price_per_nft = Some("0.001".to_string());
    assert!(config.validate().is_ok());
    assert_eq!(
        config
            .maximum_opensea_mint_value_wei()
            .expect("valid cap")
            .expect("cap exists")
            .to_string(),
        "2000000000000000"
    );
}

#[cfg(unix)]
#[test]
fn saved_configs_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("source.json");
    fs::write(
        &source,
        r#"{
          "name": "Private config",
          "chain_id": 1,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 1,
          "mint": { "function": "mint(uint256)" },
          "trigger": { "type": "manual" }
        }"#,
    )
    .expect("write source");
    let config = MintConfig::load(&source).expect("load source");
    let destination = directory.path().join("saved.json");
    config.save_pretty(&destination).expect("save config");
    let mode = fs::metadata(destination)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}
