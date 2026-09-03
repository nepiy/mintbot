use nft_mint_bot::config::{
    HYPEREVM_DEFAULT_GAS_LIMIT, HYPEREVM_MAINNET_CHAIN_ID, INK_DEFAULT_GAS_LIMIT,
    INK_MAINNET_CHAIN_ID, ROBINHOOD_DEFAULT_GAS_LIMIT, ROBINHOOD_MAINNET_CHAIN_ID,
};

#[test]
fn interactive_mint_targets_robinhood_mainnet() {
    assert_eq!(ROBINHOOD_MAINNET_CHAIN_ID, 4663);
    assert_eq!(ROBINHOOD_DEFAULT_GAS_LIMIT, 200_000);
}

#[test]
fn interactive_mint_supports_ink_mainnet() {
    assert_eq!(INK_MAINNET_CHAIN_ID, 57_073);
    assert_eq!(INK_DEFAULT_GAS_LIMIT, 230_000);
}

#[test]
fn interactive_mint_supports_hyperevm_mainnet() {
    assert_eq!(HYPEREVM_MAINNET_CHAIN_ID, 999);
    assert_eq!(HYPEREVM_DEFAULT_GAS_LIMIT, 230_000);
}
