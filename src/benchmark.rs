use crate::{
    abi::encode_mint,
    config::MintCallConfig,
    error::{BotError, Result},
    state::{AtomicBotState, BotState},
    wallet::LoadedWallet,
};
use alloy::{
    eips::Encodable2718,
    network::TransactionBuilder,
    primitives::{U256, address},
    rpc::types::TransactionRequest,
};
use std::time::{Duration, Instant};

pub async fn run(iterations: usize) -> Result<()> {
    let iterations = iterations.max(1);
    let wallet = test_wallet();
    let call = MintCallConfig {
        function: "mint(address,uint256)".to_string(),
        arguments: vec!["$wallet".to_string(), "$quantity".to_string()],
        proof: None,
        price_per_nft: "0".to_string(),
    };
    let mut calldata = Vec::with_capacity(iterations);
    let mut txs = Vec::with_capacity(iterations);
    let mut signing = Vec::with_capacity(iterations);
    let mut encoding = Vec::with_capacity(iterations);
    let mut atomic = Vec::with_capacity(iterations);
    let mut local_path = Vec::with_capacity(iterations);
    let template = TransactionRequest::default()
        .with_from(wallet.address)
        .with_to(address!("0x0000000000000000000000000000000000000002"))
        .with_chain_id(1)
        .with_nonce(7)
        .with_gas_limit(150_000)
        .with_max_fee_per_gas(20_000_000_000)
        .with_max_priority_fee_per_gas(2_000_000_000)
        .with_value(U256::ZERO);

    for _ in 0..iterations {
        let started = Instant::now();
        let prepared = encode_mint(&call, 1, wallet.address, None)?;
        calldata.push(started.elapsed());
        let started = Instant::now();
        let tx = template.clone().with_input(prepared.bytes);
        txs.push(started.elapsed());
        let started = Instant::now();
        let signed = wallet
            .sign_request(tx)
            .await
            .map_err(|err| BotError::Transaction(err.to_string()))?;
        signing.push(started.elapsed());
        let started = Instant::now();
        let _raw = signed.encoded_2718();
        encoding.push(started.elapsed());
        let state = AtomicBotState::new(BotState::WaitingForTrigger);
        let started = Instant::now();
        let _ = state.try_acquire_trigger();
        atomic.push(started.elapsed());

        let state = AtomicBotState::new(BotState::WaitingForTrigger);
        let started = Instant::now();
        let _ = state.try_acquire_trigger();
        let request = template
            .clone()
            .with_input(encode_mint(&call, 1, wallet.address, None)?.bytes);
        let signed = wallet.sign_request(request).await?;
        let _raw = signed.encoded_2718();
        local_path.push(started.elapsed());
    }

    println!("LOCAL BENCHMARK ({iterations} iterations)");
    println!("--------------------------------");
    print_stat("Calldata preparation", &calldata);
    print_stat("Transaction finalization", &txs);
    print_stat("Transaction signing", &signing);
    print_stat("Raw transaction encoding", &encoding);
    print_stat("Atomic trigger", &atomic);
    print_stat("Trigger → send-ready", &local_path);
    println!("\nNo transaction was broadcast. The signing key is a fixed test-only key.");
    Ok(())
}

fn print_stat(label: &str, samples: &[Duration]) {
    let mut ns = samples.iter().map(Duration::as_nanos).collect::<Vec<_>>();
    ns.sort_unstable();
    let sum: u128 = ns.iter().sum();
    let percentile = |p: usize| ns[((ns.len() - 1) * p) / 100];
    println!(
        "{label:<24} min {:>8.3} µs  mean {:>8.3} µs  p50 {:>8.3} µs  p95 {:>8.3} µs  p99 {:>8.3} µs  max {:>8.3} µs",
        ns[0] as f64 / 1_000.0,
        sum as f64 / ns.len() as f64 / 1_000.0,
        percentile(50) as f64 / 1_000.0,
        percentile(95) as f64 / 1_000.0,
        percentile(99) as f64 / 1_000.0,
        ns[ns.len() - 1] as f64 / 1_000.0
    );
}

fn test_wallet() -> LoadedWallet {
    let signer: alloy::signers::local::PrivateKeySigner =
        "0x0000000000000000000000000000000000000000000000000000000000000001"
            .parse()
            .expect("fixed benchmark key is valid");
    let address = signer.address();
    LoadedWallet {
        address,
        wallet: alloy::network::EthereumWallet::new(signer),
    }
}
