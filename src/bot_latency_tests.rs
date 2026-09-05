use super::*;
use crate::rpc::tests::{mock_rpc, mock_rpc_async};
use alloy::sol_types::SolCall;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::Barrier;

struct Fixture {
    config: MintConfig,
    rpc: RpcClients,
    wallet: LoadedWallet,
    prepared: PreparedTransaction,
    state: AtomicBotState,
    trigger: TriggerEngine,
    manual: tokio::sync::mpsc::Receiver<()>,
}

impl Fixture {
    fn new(rpc: RpcClients, mode: Option<OpenSeaExecutionMode>) -> Self {
        let mut config: MintConfig = serde_json::from_value(json!({
            "name": "Latency regression", "chain_id": 31337,
            "contract_address": "0x0000000000000000000000000000000000000001",
            "quantity": 1,
            "mint": {"function":"mint(uint256)", "arguments":["$quantity"]},
            "trigger": {"type":"block_timestamp", "timestamp":100},
            "nonce_strategy":"just_before_trigger",
            "gas": {"mode":"legacy", "gas_price_gwei":"1", "gas_limit":100000,
                    "multiplier":1.0, "max_total_gas_cost_native":"0.01"}
        }))
        .unwrap();
        if let Some(mode) = mode {
            config.opensea_drop_slug = Some("latency-regression".into());
            config.opensea_execution_mode = mode;
            config.require_zero_value = true;
        }
        let signer = alloy::signers::local::PrivateKeySigner::random();
        let wallet = LoadedWallet {
            address: signer.address(),
            wallet: alloy::network::EthereumWallet::new(signer),
        };
        let calldata = encode_mint(&config.mint, 1, wallet.address, None)
            .unwrap()
            .bytes;
        let request = TransactionRequest::default()
            .with_from(wallet.address)
            .with_to(config.contract().unwrap())
            .with_chain_id(config.chain_id)
            .with_nonce(7)
            .with_input(calldata.clone())
            .with_value(U256::ZERO)
            .with_gas_limit(100_000)
            .with_gas_price(1_000_000_000);
        let prepared = PreparedTransaction {
            request,
            calldata,
            mint_value: U256::ZERO,
            gas_limit: 100_000,
            fee_cap: 1_000_000_000,
            available_balance: U256::from(10u64).pow(U256::from(18)),
            opensea_hydrated: false,
            force_nonce_refresh: false,
        };
        let trigger = TriggerEngine::new(&config).unwrap();
        let (_, manual) = tokio::sync::mpsc::channel(1);
        Self {
            config,
            rpc,
            wallet,
            prepared,
            trigger,
            manual,
            state: AtomicBotState::new(BotState::WaitingForTrigger),
        }
    }

    fn context(&mut self) -> MonitorContext<'_> {
        MonitorContext {
            config: &self.config,
            rpc: &mut self.rpc,
            wallet: &self.wallet,
            prepared: &mut self.prepared,
            state: &self.state,
            trigger_engine: &mut self.trigger,
            manual_rx: &mut self.manual,
            dry_run: false,
            dynamic_fields_healthy: true,
            armed_refresh: None,
            nonce_lock: None,
            last_seen_block: None,
            last_chain_timestamp: 99,
            last_closed_retry_notice: None,
            opensea_stages: Vec::new(),
            opensea_stage_index: 0,
            opensea_client: None,
            auto_opensea_schedule: false,
            opensea_schedule: None,
        }
    }

    fn mint(&self) -> OpenSeaMintTransaction {
        OpenSeaMintTransaction {
            target: OPENSEA_SEADROP_ADDRESS,
            calldata: crate::opensea::SeaDropMint::mintPublicCall {
                nftContract: self.config.contract().unwrap(),
                feeRecipient: Address::ZERO,
                minterIfNotPayer: self.wallet.address,
                quantity: U256::from(1),
            }
            .abi_encode(),
            value: U256::ZERO,
        }
    }
}

fn response(request: &Value) -> Value {
    match request["method"].as_str().unwrap() {
        "eth_getTransactionCount" => json!("0x9"),
        "eth_getBalance" => json!("0xde0b6b3a7640000"),
        "eth_feeHistory" => json!({"oldestBlock":"0x1", "baseFeePerGas":["0x1","0x1"],
                                   "gasUsedRatio":[0.5], "reward":[["0x1"]]}),
        "eth_estimateGas" => json!("0x249f0"), // 150,000, above the configured fallback
        "eth_call" => json!("0x"),
        _ => panic!("unexpected RPC method: {}", request["method"]),
    }
}

// A barrier makes sequential implementations fail by timeout, without relying
// on fragile elapsed-time thresholds on busy CI hosts.
#[tokio::test]
async fn normal_opensea_overlaps_build_fees_and_balance_and_uses_live_gas() {
    let barrier = Arc::new(Barrier::new(3));
    let balance_calls = Arc::new(AtomicUsize::new(0));
    let (rpc, server) = mock_rpc_async({
        let barrier = barrier.clone();
        let balance_calls = balance_calls.clone();
        move |request| {
            let barrier = barrier.clone();
            let balance_calls = balance_calls.clone();
            async move {
                match request["method"].as_str().unwrap() {
                    "eth_getBalance" => {
                        balance_calls.fetch_add(1, Ordering::SeqCst);
                        barrier.wait().await;
                    }
                    "eth_feeHistory" => {
                        barrier.wait().await;
                    }
                    "eth_estimateGas" => {
                        assert!(request["params"][0]["maxFeePerGas"].is_string());
                        assert!(request["params"][0].get("gasPrice").is_none());
                    }
                    _ => panic!("unexpected read"),
                }
                response(&request)
            }
        }
    })
    .await;
    let mut fixture = Fixture::new(rpc, Some(OpenSeaExecutionMode::Normal));
    fixture.config.gas.mode = GasMode::Auto;
    fixture.prepared.request.gas_price = None;
    let mint = fixture.mint();
    let mut context = fixture.context();
    let inputs = tokio::time::timeout(
        Duration::from_secs(2),
        fetch_trigger_opensea_inputs(&mut context, async {
            barrier.wait().await;
            Ok(mint)
        }),
    )
    .await
    .expect("build, fees and balance must overlap")
    .unwrap()
    .unwrap();
    apply_opensea_inputs(
        context.config,
        context.rpc,
        context.wallet,
        context.prepared,
        inputs,
    )
    .await
    .unwrap();
    assert_eq!(context.prepared.gas_limit, 150_000);
    assert_eq!(balance_calls.load(Ordering::SeqCst), 1);
    validate_signing_request(
        context.config,
        context.wallet.address,
        context.prepared,
        &context.prepared.request,
    )
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn aggressive_refresh_coalesces_and_overlaps_opensea_build() {
    let barrier = Arc::new(Barrier::new(3));
    let calls = Arc::new(AtomicUsize::new(0));
    let (rpc, server) = mock_rpc_async({
        let barrier = barrier.clone();
        let calls = calls.clone();
        move |request| {
            let barrier = barrier.clone();
            let calls = calls.clone();
            async move {
                assert!(matches!(
                    request["method"].as_str().unwrap(),
                    "eth_getTransactionCount" | "eth_getBalance"
                ));
                calls.fetch_add(1, Ordering::SeqCst);
                barrier.wait().await;
                response(&request)
            }
        }
    })
    .await;
    let mut fixture = Fixture::new(rpc, Some(OpenSeaExecutionMode::Aggressive));
    let mint = fixture.mint();
    let mut context = fixture.context();
    for _ in 0..100 {
        context.start_armed_refresh();
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "starting a refresh must not block the monitor"
    );
    let inputs = tokio::time::timeout(
        Duration::from_secs(2),
        fetch_trigger_opensea_inputs(&mut context, async {
            barrier.wait().await;
            Ok(mint)
        }),
    )
    .await
    .expect("refresh and OpenSea build must overlap")
    .unwrap()
    .unwrap();
    apply_opensea_inputs(
        context.config,
        context.rpc,
        context.wallet,
        context.prepared,
        inputs,
    )
    .await
    .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one refresh, no live gas simulation or extra balance read"
    );
    assert_eq!(context.prepared.request.nonce, Some(9));
    assert!(context.dynamic_fields_healthy);
    assert!(context.armed_refresh.is_none());
    validate_signing_request(
        context.config,
        context.wallet.address,
        context.prepared,
        &context.prepared.request,
    )
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn nonce_lookup_overlaps_preflight_and_lock_stays_held_until_submission() {
    let barrier = Arc::new(Barrier::new(2));
    let (rpc, server) = mock_rpc_async(move |request| {
        let barrier = barrier.clone();
        async move {
            assert!(matches!(
                request["method"].as_str().unwrap(),
                "eth_call" | "eth_getTransactionCount"
            ));
            barrier.wait().await;
            response(&request)
        }
    })
    .await;
    let mut fixture = Fixture::new(rpc, None);
    let mut context = fixture.context();
    assert!(
        tokio::time::timeout(
            Duration::from_secs(2),
            ensure_transaction_ready(&mut context)
        )
        .await
        .expect("nonce and preflight must overlap")
        .unwrap()
    );
    assert_eq!(context.prepared.request.nonce, Some(9));
    let mut competing_lock = Box::pin(WalletNonceLock::acquire(
        context.config.chain_id,
        context.wallet.address,
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut competing_lock)
            .await
            .is_err()
    );
    drop(context.nonce_lock.take());
    let lock = tokio::time::timeout(Duration::from_secs(2), competing_lock)
        .await
        .unwrap()
        .unwrap();
    assert!(lock.was_contended());
    server.abort();
}

#[tokio::test]
async fn cancelled_refresh_invalidates_cache_and_recovers_before_next_attempt() {
    let (rpc, server) = mock_rpc(|request| response(&request)).await;
    let mut fixture = Fixture::new(rpc, Some(OpenSeaExecutionMode::Aggressive));
    let mut context = fixture.context();
    context.armed_refresh = Some(std::future::pending().boxed());
    assert!(ensure_dynamic_fields(&mut context).now_or_never().is_none());
    assert!(!context.dynamic_fields_healthy);
    assert!(ensure_dynamic_fields(&mut context).await);
    assert_eq!(context.prepared.request.nonce, Some(9));
    server.abort();
}

#[tokio::test]
async fn failed_refresh_blocks_aggressive_hydration() {
    let (rpc, server) = mock_rpc(|_| json!("not-a-quantity")).await;
    let mut fixture = Fixture::new(rpc, Some(OpenSeaExecutionMode::Aggressive));
    let mint = fixture.mint();
    let mut context = fixture.context();
    context.start_armed_refresh();
    assert!(
        fetch_trigger_opensea_inputs(&mut context, std::future::ready(Ok(mint)))
            .await
            .unwrap()
            .is_none()
    );
    assert!(!context.dynamic_fields_healthy);
    assert!(!context.prepared.opensea_hydrated);
    server.abort();
}

#[tokio::test]
async fn final_budget_rejects_unaffordable_normal_and_aggressive_mints() {
    for mode in [
        OpenSeaExecutionMode::Normal,
        OpenSeaExecutionMode::Aggressive,
    ] {
        let (rpc, server) = mock_rpc(|request| response(&request)).await;
        let mut fixture = Fixture::new(rpc, Some(mode));
        let mint = fixture.mint();
        fixture.prepared.available_balance = U256::ZERO;
        let inputs = OpenSeaInputs {
            mint,
            fees: None,
            balance: if mode == OpenSeaExecutionMode::Normal {
                Some(U256::ZERO)
            } else {
                None
            },
        };
        let context = fixture.context();
        assert!(
            apply_opensea_inputs(
                context.config,
                context.rpc,
                context.wallet,
                context.prepared,
                inputs
            )
            .await
            .is_err()
        );
        assert!(!context.prepared.opensea_hydrated);
        server.abort();
    }
}

/// Reproducible latency comparison with 100 ms per external operation. The
/// baseline preserves the previous ordering; the optimized cases exercise the
/// production preparation helpers. No signatures or broadcasts are performed.
#[tokio::test]
async fn mocked_opensea_latency_comparison() {
    let delay = Duration::from_millis(100);
    let (rpc, server) = mock_rpc_async(move |request| async move {
        tokio::time::sleep(delay).await;
        response(&request)
    })
    .await;
    let mut normal = Fixture::new(rpc.clone(), Some(OpenSeaExecutionMode::Normal));
    normal.config.gas.mode = GasMode::Auto;
    normal.prepared.request.gas_price = None;
    let baseline_started = Instant::now();
    let (_, fees) = tokio::join!(tokio::time::sleep(delay), rpc.estimate_eip1559_fees());
    let fees = fees.unwrap();
    let request = normal
        .prepared
        .request
        .clone()
        .with_max_fee_per_gas(fees.max_fee_per_gas)
        .with_max_priority_fee_per_gas(fees.max_priority_fee_per_gas);
    rpc.estimate_gas(request).await.unwrap();
    rpc.check_balance(normal.wallet.address).await.unwrap();
    rpc.preload_nonce(normal.wallet.address).await.unwrap();
    let normal_before = baseline_started.elapsed();

    let mint = normal.mint();
    let mut context = normal.context();
    let optimized_started = Instant::now();
    let wallet = context.wallet.address;
    let (inputs, nonce) = tokio::try_join!(
        fetch_trigger_opensea_inputs(&mut context, async {
            tokio::time::sleep(delay).await;
            Ok(mint)
        }),
        rpc.preload_nonce(wallet),
    )
    .unwrap();
    apply_opensea_inputs(
        context.config,
        context.rpc,
        context.wallet,
        context.prepared,
        inputs.unwrap(),
    )
    .await
    .unwrap();
    context.prepared.request.set_nonce(nonce);
    let normal_after = optimized_started.elapsed();

    let mut aggressive = Fixture::new(rpc, Some(OpenSeaExecutionMode::Aggressive));
    let baseline_started = Instant::now();
    refresh_transaction_fields(
        &aggressive.config,
        &aggressive.rpc,
        aggressive.wallet.address,
        &mut aggressive.prepared,
    )
    .await
    .unwrap();
    tokio::time::sleep(delay).await;
    let aggressive_before = baseline_started.elapsed();
    let mint = aggressive.mint();
    let mut context = aggressive.context();
    context.start_armed_refresh();
    let optimized_started = Instant::now();
    let inputs = fetch_trigger_opensea_inputs(&mut context, async {
        tokio::time::sleep(delay).await;
        Ok(mint)
    })
    .await
    .unwrap()
    .unwrap();
    apply_opensea_inputs(
        context.config,
        context.rpc,
        context.wallet,
        context.prepared,
        inputs,
    )
    .await
    .unwrap();
    let aggressive_after = optimized_started.elapsed();
    println!(
        "Mocked 100 ms operations — normal preparation: {:.1} ms → {:.1} ms; aggressive with pending refresh: {:.1} ms → {:.1} ms",
        normal_before.as_secs_f64() * 1000.0,
        normal_after.as_secs_f64() * 1000.0,
        aggressive_before.as_secs_f64() * 1000.0,
        aggressive_after.as_secs_f64() * 1000.0
    );
    // Ordering is enforced by the barrier regressions, not timing assertions.
    server.abort();
}

#[tokio::test]
async fn block_monitor_consumes_new_headers_while_refresh_is_pending() {
    let (rpc, server) = mock_rpc(|request| response(&request)).await;
    let mut fixture = Fixture::new(rpc, Some(OpenSeaExecutionMode::Aggressive));
    let mut context = fixture.context();
    context.armed_refresh = Some(std::future::pending().boxed());
    let mut blocks = futures_util::stream::iter((1..=3).map(|number| Header {
        inner: alloy::consensus::Header {
            number,
            timestamp: 99,
            ..Default::default()
        },
        ..Default::default()
    }));
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        monitor_block_stream(&mut context, &mut blocks),
    )
    .await
    .expect("a stalled refresh must not block subsequent headers");
    assert!(matches!(result, Err(MonitorFailure::Transport(_))));
    assert_eq!(context.last_seen_block, Some(3));
    server.abort();
}

#[tokio::test]
async fn contended_cached_nonce_is_refetched_and_failed_preflight_releases_lock() {
    let (rpc, server) = mock_rpc(|request| response(&request)).await;
    let mut fixture = Fixture::new(rpc, None);
    fixture.config.nonce_strategy = NonceStrategy::Preloaded;
    let mut context = fixture.context();
    let original_lock = WalletNonceLock::acquire(context.config.chain_id, context.wallet.address)
        .await
        .unwrap();
    {
        let mut ready = Box::pin(ensure_transaction_ready(&mut context));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut ready)
                .await
                .is_err()
        );
        drop(original_lock);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), ready)
                .await
                .unwrap()
                .unwrap()
        );
    }
    assert_eq!(
        context.prepared.request.nonce,
        Some(9),
        "contended preloaded nonce 7 must be discarded"
    );
    drop(context.nonce_lock.take());
    server.abort();

    let (rpc, server) = mock_rpc(|request| match request["method"].as_str().unwrap() {
        "eth_call" => json!("invalid-call-result"),
        _ => response(&request),
    })
    .await;
    let mut fixture = Fixture::new(rpc, None);
    let mut context = fixture.context();
    assert!(ensure_transaction_ready(&mut context).await.is_err());
    assert!(context.nonce_lock.is_none());
    tokio::time::timeout(
        Duration::from_secs(1),
        WalletNonceLock::acquire(context.config.chain_id, context.wallet.address),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();
}

#[tokio::test]
async fn refresh_completion_preserves_stage_invalidation_and_retry_flags() {
    let (rpc, server) = mock_rpc(|request| response(&request)).await;
    let mut fixture = Fixture::new(rpc, Some(OpenSeaExecutionMode::Aggressive));
    let mut context = fixture.context();
    context.prepared.opensea_hydrated = true;
    let mut snapshot = context.prepared.clone();
    snapshot.request.set_nonce(12);
    // A stage change and a retry happen after the snapshot was taken.
    context.prepared.opensea_hydrated = false;
    context.prepared.force_nonce_refresh = true;
    context.finish_armed_refresh(Ok(snapshot));
    assert!(!context.prepared.opensea_hydrated);
    assert!(context.prepared.force_nonce_refresh);
    assert_eq!(context.prepared.request.nonce, Some(12));
    server.abort();
}
