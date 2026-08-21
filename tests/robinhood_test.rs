use nft_mint_bot::config::ROBINHOOD_MAINNET_CHAIN_ID;

#[test]
fn interactive_mint_targets_robinhood_mainnet() {
    assert_eq!(ROBINHOOD_MAINNET_CHAIN_ID, 4663);
}
