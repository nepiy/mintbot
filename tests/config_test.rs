use nft_mint_bot::config::{MintConfig, MintTrigger, parse_gwei, parse_native_amount};
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
