use alloy::primitives::b256;
use nft_mint_bot::{config::MintConfig, trigger::TriggerEngine};

#[test]
fn prepares_a_contract_event_filter_for_the_configured_collection() {
    let config: MintConfig = serde_json::from_str(
        r#"{
          "name": "Event test",
          "chain_id": 31337,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 1,
          "mint": { "function": "mint(uint256)", "arguments": ["$quantity"] },
          "trigger": {
            "type": "contract_event",
            "signature": "PublicSaleStarted()",
            "confirmations": 1
          }
        }"#,
    )
    .expect("valid config JSON");
    let engine = TriggerEngine::new(&config).expect("trigger should parse");
    let filter = engine.event_filter().expect("event trigger has a filter");
    assert!(filter.address.contains(&config.contract().unwrap()));
}

#[test]
fn rejects_view_triggers_with_arguments() {
    let config: MintConfig = serde_json::from_str(
        r#"{
          "name": "View test",
          "chain_id": 1,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 1,
          "mint": { "function": "mint(uint256)", "arguments": ["$quantity"] },
          "trigger": {
            "type": "boolean_contract_state",
            "function": "publicSaleActive(uint256) returns (bool)",
            "expected_value": true
          }
        }"#,
    )
    .expect("valid config JSON");
    assert!(TriggerEngine::new(&config).is_err());
}

#[test]
fn removed_event_clears_the_pending_canonical_candidate() {
    let config: MintConfig = serde_json::from_str(
        r#"{
          "name": "Reorg test",
          "chain_id": 1,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 1,
          "mint": { "function": "mint(uint256)", "arguments": ["$quantity"] },
          "trigger": {
            "type": "contract_event",
            "signature": "PublicSaleStarted()",
            "confirmations": 2
          }
        }"#,
    )
    .expect("valid config JSON");
    let mut engine = TriggerEngine::new(&config).unwrap();
    let hash = b256!("0000000000000000000000000000000000000000000000000000000000000001");
    engine.observe_event(Some(100), Some(hash), false);
    assert_eq!(engine.pending_event(), Some((100, Some(hash))));
    engine.observe_event(Some(100), Some(hash), true);
    assert_eq!(engine.pending_event(), None);
}

#[test]
fn rejects_invalid_numeric_target_during_startup() {
    let config: MintConfig = serde_json::from_str(
        r#"{
          "name": "Numeric test",
          "chain_id": 1,
          "contract_address": "0x0000000000000000000000000000000000000001",
          "quantity": 1,
          "mint": { "function": "mint(uint256)", "arguments": ["$quantity"] },
          "trigger": {
            "type": "numeric_phase",
            "function": "salePhase() returns (uint256)",
            "target_value": "not-a-number"
          }
        }"#,
    )
    .expect("valid config JSON");
    assert!(TriggerEngine::new(&config).is_err());
}
