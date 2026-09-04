use clap::Parser;
use nft_mint_bot::{
    benchmark,
    cli::{Cli, Command},
    error::Result,
    rpc::RpcClients,
    run_bot, run_interactive, run_simulation,
    security::verify_dotenv_permissions,
    setup::{run_wizard, send_manual_trigger},
};
use std::path::Path;

#[tokio::main]
async fn main() -> Result<()> {
    verify_dotenv_permissions(Path::new(".env"))?;
    // Load exactly the file whose permissions were checked, never a parent's.
    if let Err(error) = dotenvy::from_path(".env")
        && !matches!(&error, dotenvy::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
    {
        return Err(nft_mint_bot::error::BotError::Config(
            "could not load .env; check its syntax and permissions".to_string(),
        ));
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "nft_mint_bot=info".into()),
        )
        .with_target(false)
        .compact()
        .init();

    match Cli::parse().command {
        Some(Command::Setup { output }) => {
            run_wizard(&output)?;
        }
        Some(Command::RpcTest) => run_rpc_test().await?,
        Some(Command::Simulate { config }) => run_simulation(config).await?,
        Some(Command::Run { config, dry_run }) => run_bot(config, dry_run).await?,
        Some(Command::Start { dry_run }) => run_interactive(dry_run).await?,
        Some(Command::Benchmark { iterations }) => benchmark::run(iterations).await?,
        Some(Command::Trigger { config }) => send_manual_trigger(&config).await?,
        None => run_interactive(false).await?,
    }
    Ok(())
}

async fn run_rpc_test() -> Result<()> {
    let rpc = RpcClients::connect_from_env().await?;
    println!("RPC LATENCY TEST");
    println!("--------------------------------");
    println!(
        "WS Connect       {:.3} ms",
        rpc.ws_connect_latency.as_secs_f64() * 1000.0
    );
    print_summary("WS Subscription", &rpc.benchmark_ws_subscription().await);
    println!(
        "\nEach HTTP method below uses 10 sequential attempts; failed/timed-out attempts are counted separately and excluded from latency percentiles."
    );
    for (name, provider) in rpc.broadcast.iter() {
        let benchmark = rpc.benchmark_endpoint(name, provider).await;
        println!("\nProvider {name}");
        print_summary("eth_chainId", &benchmark.chain_id);
        print_summary("eth_blockNumber", &benchmark.block_number);
        print_summary("eth_getBalance", &benchmark.balance);
    }
    println!("\nRecommended HTTP: compare the measured means and reliability for your mint route.");
    println!(
        "Recommended WS: the startup connection above; run several times for a stable comparison."
    );
    Ok(())
}

fn print_summary(label: &str, summary: &nft_mint_bot::rpc::LatencySummary) {
    let milliseconds = |duration: std::time::Duration| duration.as_secs_f64() * 1000.0;
    println!(
        "{label:<18} ok {:>2} fail {:>2} min {:>7.3} mean {:>7.3} p50 {:>7.3} p95 {:>7.3} p99 {:>7.3} max {:>7.3} ms",
        summary.successful,
        summary.failed,
        milliseconds(summary.min),
        milliseconds(summary.mean),
        milliseconds(summary.p50),
        milliseconds(summary.p95),
        milliseconds(summary.p99),
        milliseconds(summary.max),
    );
}
