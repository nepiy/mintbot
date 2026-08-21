use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "nft-mint-bot",
    version,
    about = "Event-driven EVM NFT mint bot"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Setup {
        #[arg(long, default_value = "configs")]
        output: PathBuf,
    },
    RpcTest,
    Simulate {
        #[arg(long)]
        config: PathBuf,
    },
    Run {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
    #[command(alias = "interactive")]
    Start {
        #[arg(long)]
        dry_run: bool,
    },
    Benchmark {
        #[arg(long, default_value_t = 10_000)]
        iterations: usize,
    },
    Trigger {
        #[arg(long)]
        config: PathBuf,
    },
}
