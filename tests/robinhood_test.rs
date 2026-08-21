use nft_mint_bot::config::{ROBINHOOD_DEFAULT_GAS_LIMIT, ROBINHOOD_MAINNET_CHAIN_ID};

#[test]
fn interactive_mint_targets_robinhood_mainnet() {
    assert_eq!(ROBINHOOD_MAINNET_CHAIN_ID, 4663);
    assert_eq!(ROBINHOOD_DEFAULT_GAS_LIMIT, 200_000);
}
