use clap::Parser;
use nft_mint_bot::cli::Cli;

#[test]
fn no_subcommand_selects_interactive_startup() {
    let cli = Cli::try_parse_from(["nft-mint-bot"]).expect("no-argument CLI should parse");
    assert!(cli.command.is_none());
}
