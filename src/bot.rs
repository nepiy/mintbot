use crate::{
    abi::encode_mint,
    config::{GasMode, MintConfig, MintTrigger, NonceStrategy, parse_gwei, parse_native_amount},
    error::{BotError, Result},
    metrics::LatencyMetrics,
    rpc::{RpcClients, simulate_call},
    setup::{bind_manual_control, cleanup_manual_control},
    state::{AtomicBotState, BotState},
    trigger::{TriggerEngine, TriggerObservation},
    wallet::{LoadedWallet, short_address},
};
use alloy::{
    consensus::BlockHeader,
    eips::Encodable2718,
    network::TransactionBuilder,
    primitives::{B256, U256},
    pubsub::SubscriptionStream,
    rpc::types::{Filter, Header, Log, TransactionRequest},
};
use futures_util::StreamExt;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Debug, Clone)]
pub struct PreparedTransaction {
    pub request: TransactionRequest,
    pub calldata: Vec<u8>,
    pub mint_value: U256,
    pub gas_limit: u64,
    pub fee_cap: u128,
}

pub async fn run_bot(config_path: PathBuf, dry_run: bool) -> Result<()> {
    let config = MintConfig::load(&config_path)?;
    let state = AtomicBotState::default();
    state.store(BotState::LoadingConfiguration);
    let wallet = LoadedWallet::from_env()?;
    println!("NFT Mint Bot");
    println!("--------------------------------------");
    println!("Collection: {}", config.name);
    println!("Chain ID: {}", config.chain_id);
    println!("Wallet: {}", short_address(wallet.address));
    println!("Contract: {}", config.contract_address);

    state.store(BotState::ConnectingRpc);
    let mut rpc = RpcClients::connect_from_env().await?;
    println!("\nWS RPC: CONNECTED");
    println!("HTTP RPC: CONNECTED");
    println!(
        "Backup/broadcast RPCs: {}",
        rpc.broadcast.len().saturating_sub(1)
    );

    state.store(BotState::Validating);
    rpc.validate_chain(&config).await?;
    rpc.validate_contract(&config).await?;
    println!("Contract: VALID");

    state.store(BotState::Preparing);
    let mut prepared = prepare_transaction(&config, &rpc, &wallet).await?;
    let mut trigger_engine = TriggerEngine::new(&config)?;
    println!("Wallet balance: OK");
    println!("Calldata: PREPARED ({} bytes)", prepared.calldata.len());
    println!("Signer: READY");
    println!("Gas strategy: READY");
    println!("Nonce strategy: READY");

    let manual_enabled = matches!(config.trigger, MintTrigger::Manual);
    let (mut manual_rx, control_path) = if manual_enabled {
        let (receiver, path) = bind_manual_control(&config_path).await?;
        (receiver, Some(path))
    } else {
        let (_sender, receiver) = tokio::sync::mpsc::channel(1);
        (receiver, None)
    };

    let (streams, last_seen_block) = prepare_monitor_streams(&rpc, &trigger_engine, None).await?;
    println!("Subscriptions: READY");

    state.store(BotState::Armed);
    print_armed(&config, &wallet, &prepared, dry_run);
    state.store(BotState::WaitingForTrigger);

    let mut monitor = MonitorContext {
        config: &config,
        rpc: &mut rpc,
        wallet: &wallet,
        trigger_engine: &mut trigger_engine,
        manual_rx: &mut manual_rx,
        prepared: &mut prepared,
        state: &state,
        dry_run,
        dynamic_fields_healthy: true,
        last_seen_block,
    };
    let result = monitor_until_trigger(&mut monitor, streams).await;
    if let Some(path) = control_path {
        cleanup_manual_control(&path);
    }
    result
}

pub async fn run_simulation(config_path: PathBuf) -> Result<()> {
    let config = MintConfig::load(&config_path)?;
    let wallet = LoadedWallet::from_env()?;
    let mut rpc = RpcClients::connect_from_env().await?;
    rpc.validate_chain(&config).await?;
    rpc.validate_contract(&config).await?;
    let prepared = prepare_transaction(&config, &rpc, &wallet).await?;
    println!("SIMULATION");
    println!("--------------------------------");
    println!("Collection: {}", config.name);
    println!("Chain ID: {}", config.chain_id);
    println!("Wallet: {}", wallet.address);
    println!("Contract: {}", config.contract_address);
    println!("Quantity: {}", config.quantity);
    println!("Mint value: {} wei", prepared.mint_value);
    println!(
        "Nonce: {}",
        prepared.request.nonce.map_or_else(
            || "just-before-trigger".to_string(),
            |nonce| nonce.to_string()
        )
    );
    println!("Estimated gas: {}", prepared.gas_limit);
    simulate_call(&rpc, prepared.request.clone()).await?;
    println!("\nSimulation: SUCCESS");
    println!("Configuration appears ready.");
    Ok(())
}

pub async fn prepare_transaction(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
) -> Result<PreparedTransaction> {
    let contract = config.contract()?;
    let calldata = encode_mint(
        &config.mint,
        config.quantity,
        wallet.address,
        config.mint.proof.as_deref(),
    )?;
    let mint_value = config.mint_value_wei()?;
    let nonce = rpc.preload_nonce(wallet.address).await?;
    let mut request = TransactionRequest::default()
        .with_from(wallet.address)
        .with_to(contract)
        .with_chain_id(config.chain_id)
        .with_input(calldata.bytes.clone())
        .with_value(mint_value);

    let gas_limit = if let Some(limit) = config.gas.gas_limit {
        limit
    } else {
        rpc.estimate_gas(request.clone()).await.map_err(|err| {
            BotError::Transaction(format!(
                "gas estimation failed, possibly because the mint is not active yet: {err}. \
                 Configure gas.gas_limit from a trusted prior simulation before arming"
            ))
        })?
    };
    let gas_limit = scale_u64(gas_limit, config.gas.multiplier)?;
    request.set_gas_limit(gas_limit);

    let fee_cap = match config.gas.mode {
        GasMode::Auto => {
            let mut estimation = rpc.estimate_eip1559_fees().await.map_err(|err| {
                BotError::Transaction(format!("EIP-1559 fee estimation failed: {err}"))
            })?;
            estimation.max_fee_per_gas =
                scale_u128(estimation.max_fee_per_gas, config.gas.multiplier)?;
            estimation.max_priority_fee_per_gas =
                scale_u128(estimation.max_priority_fee_per_gas, config.gas.multiplier)?;
            request = request
                .with_max_fee_per_gas(estimation.max_fee_per_gas)
                .with_max_priority_fee_per_gas(estimation.max_priority_fee_per_gas);
            estimation.max_fee_per_gas
        }
        GasMode::Eip1559 | GasMode::Manual => {
            let max_fee = config
                .gas
                .max_fee_gwei
                .as_deref()
                .ok_or_else(|| {
                    BotError::Config("max_fee_gwei is required for eip1559/manual gas".to_string())
                })
                .and_then(parse_gwei)?;
            let priority = config
                .gas
                .max_priority_fee_gwei
                .as_deref()
                .ok_or_else(|| {
                    BotError::Config(
                        "max_priority_fee_gwei is required for eip1559/manual gas".to_string(),
                    )
                })
                .and_then(parse_gwei)?;
            request = request
                .with_max_fee_per_gas(max_fee)
                .with_max_priority_fee_per_gas(priority);
            max_fee
        }
        GasMode::Legacy => {
            let gas_price = config
                .gas
                .gas_price_gwei
                .as_deref()
                .ok_or_else(|| {
                    BotError::Config("gas_price_gwei is required for legacy gas".to_string())
                })
                .and_then(parse_gwei)?;
            request = request.with_gas_price(gas_price);
            gas_price
        }
    };

    if !matches!(config.nonce_strategy, NonceStrategy::JustBeforeTrigger) {
        request.set_nonce(nonce);
    }
    validate_transaction_budget(config, rpc, wallet.address, mint_value, gas_limit, fee_cap)
        .await?;
    Ok(PreparedTransaction {
        request,
        calldata: calldata.bytes,
        mint_value,
        gas_limit,
        fee_cap,
    })
}

async fn validate_transaction_budget(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: alloy::primitives::Address,
    mint_value: U256,
    gas_limit: u64,
    fee_cap: u128,
) -> Result<()> {
    let maximum_gas_cost = U256::from(gas_limit)
        .checked_mul(U256::from(fee_cap))
        .ok_or_else(|| BotError::Transaction("gas cost overflowed U256".to_string()))?;
    if let Some(maximum) = config.gas.max_total_gas_cost_native.as_deref() {
        let maximum = parse_native_amount(maximum)?;
        if maximum_gas_cost > maximum {
            return Err(BotError::GasLimitExceeded {
                estimated: maximum_gas_cost.to_string(),
                maximum: maximum.to_string(),
            });
        }
    }
    let balance = rpc.check_balance(wallet).await?;
    let required = mint_value
        .checked_add(maximum_gas_cost)
        .ok_or_else(|| BotError::Transaction("required balance overflowed U256".to_string()))?;
    if balance < required {
        return Err(BotError::InsufficientBalance {
            needed: required.to_string(),
            available: balance.to_string(),
        });
    }
    Ok(())
}

struct MonitorContext<'a> {
    config: &'a MintConfig,
    rpc: &'a mut RpcClients,
    wallet: &'a LoadedWallet,
    trigger_engine: &'a mut TriggerEngine,
    manual_rx: &'a mut tokio::sync::mpsc::Receiver<()>,
    prepared: &'a mut PreparedTransaction,
    state: &'a AtomicBotState,
    dry_run: bool,
    dynamic_fields_healthy: bool,
    last_seen_block: Option<u64>,
}

enum MonitorStreams {
    Blocks(SubscriptionStream<Header>),
    Events {
        blocks: SubscriptionStream<Header>,
        logs: SubscriptionStream<Log>,
        filter: Box<Filter>,
    },
    Manual,
}

enum MonitorFailure {
    Transport(BotError),
    Execution(BotError),
}

async fn prepare_monitor_streams(
    rpc: &RpcClients,
    trigger_engine: &TriggerEngine,
    backfill_from: Option<u64>,
) -> Result<(MonitorStreams, Option<u64>)> {
    if trigger_engine.event_filter().is_some() {
        let filter = trigger_engine
            .event_filter()
            .ok_or_else(|| BotError::Trigger("event filter was not prepared".to_string()))?;
        // Subscribe to logs first so an activation event cannot fall between the
        // block and log subscription setup calls.
        let logs = rpc.subscribe_logs(&filter).await?;
        let blocks = rpc.subscribe_blocks().await?;
        let current_block = rpc.block_number().await?;
        return Ok((
            MonitorStreams::Events {
                blocks,
                logs,
                filter: Box::new(filter),
            },
            Some(backfill_from.unwrap_or(current_block)),
        ));
    }
    if matches!(trigger_engine.trigger(), MintTrigger::Manual) {
        return Ok((MonitorStreams::Manual, None));
    }
    let blocks = rpc.subscribe_blocks().await?;
    Ok((MonitorStreams::Blocks(blocks), backfill_from))
}

async fn monitor_until_trigger(
    context: &mut MonitorContext<'_>,
    mut streams: MonitorStreams,
) -> Result<()> {
    loop {
        let result = match &mut streams {
            MonitorStreams::Blocks(blocks) => monitor_block_stream(context, blocks).await,
            MonitorStreams::Events {
                blocks,
                logs,
                filter,
            } => monitor_event_stream(context, blocks, logs, filter.as_ref()).await,
            MonitorStreams::Manual => monitor_manual(context).await,
        };
        match result {
            Ok(MonitorOutcome::Shutdown) => {
                context.state.store(BotState::Stopped);
                println!("Stopping mint monitor...\nNo transaction submitted.");
                return Ok(());
            }
            Ok(MonitorOutcome::Done) => return Ok(()),
            Err(MonitorFailure::Execution(err)) => {
                context.state.store(BotState::Failed);
                return Err(err);
            }
            Err(MonitorFailure::Transport(err)) => {
                tracing::warn!(error = %err, "WebSocket subscription ended; reconnecting");
                println!("[WARN] WebSocket disconnected");
                println!("[INFO] reconnecting...");
                tokio::select! {
                    result = context.rpc.reconnect_ws(context.config.chain_id) => result?,
                    _ = shutdown_signal() => {
                        context.state.store(BotState::Stopped);
                        println!("Stopping mint monitor...\nNo transaction submitted.");
                        return Ok(());
                    }
                }
                let backfill_from = context.last_seen_block.map(|block| block.saturating_sub(1));
                let (new_streams, _) =
                    prepare_monitor_streams(context.rpc, context.trigger_engine, backfill_from)
                        .await?;
                let backfill_ready = match (&new_streams, backfill_from) {
                    (MonitorStreams::Events { filter, .. }, Some(from_block)) => {
                        apply_event_backfill(context, filter.as_ref(), from_block).await?
                    }
                    _ => None,
                };
                if let Some(received) = backfill_ready
                    && ensure_dynamic_fields(context).await
                    && context.state.try_acquire_trigger()
                {
                    let validated = Instant::now();
                    let acquired = Instant::now();
                    execute_transaction(
                        context.config,
                        context.rpc,
                        context.wallet,
                        context.prepared,
                        context.state,
                        context.dry_run,
                        TriggerTiming {
                            received,
                            validated,
                            acquired,
                        },
                    )
                    .await?;
                    return Ok(());
                }
                streams = new_streams;
                println!("[INFO] WebSocket restored");
                println!("[INFO] subscriptions restored");
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum MonitorOutcome {
    Done,
    Shutdown,
}

#[derive(Debug, Clone, Copy)]
struct TriggerTiming {
    received: Instant,
    validated: Instant,
    acquired: Instant,
}

async fn monitor_block_stream(
    context: &mut MonitorContext<'_>,
    blocks: &mut SubscriptionStream<Header>,
) -> std::result::Result<MonitorOutcome, MonitorFailure> {
    let mut shutdown = Box::pin(shutdown_signal());
    println!("\nMonitoring: every new block via WebSocket");
    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(MonitorOutcome::Shutdown),
            header = blocks.next() => {
                let received = Instant::now();
                let Some(header) = header else {
                    return Err(MonitorFailure::Transport(BotError::Rpc("block subscription closed".to_string())));
                };
                context.last_seen_block = Some(header.number());
                let number = header.number();
                match context
                    .trigger_engine
                    .observe_block(&header, context.rpc)
                    .await
                {
                    Ok(TriggerObservation::Ready) => {
                        if ensure_dynamic_fields(context).await {
                            let validated = Instant::now();
                            if !context.state.try_acquire_trigger() {
                                continue;
                            }
                            let acquired = Instant::now();
                            return execute_transaction(
                                context.config,
                                context.rpc,
                                context.wallet,
                                context.prepared,
                                context.state,
                                context.dry_run,
                                TriggerTiming {
                                    received,
                                    validated,
                                    acquired,
                                },
                            )
                            .await
                            .map_err(MonitorFailure::Execution);
                        }
                    }
                    Ok(TriggerObservation::NotReady) => {
                        tracing::debug!(block = number, "mint trigger not ready");
                        refresh_armed_fields(context).await;
                    }
                    Err(err) => {
                        tracing::warn!(block = number, error = %err, "trigger evaluation failed; waiting for the next block");
                        refresh_armed_fields(context).await;
                    }
                }
            }
        }
    }
}

async fn monitor_event_stream(
    context: &mut MonitorContext<'_>,
    blocks: &mut SubscriptionStream<Header>,
    logs: &mut SubscriptionStream<Log>,
    filter: &Filter,
) -> std::result::Result<MonitorOutcome, MonitorFailure> {
    let mut shutdown = Box::pin(shutdown_signal());
    println!(
        "\nMonitoring: WebSocket blocks + {}",
        filter_description(filter)
    );
    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(MonitorOutcome::Shutdown),
            header = blocks.next() => {
                let received = Instant::now();
                let Some(header) = header else {
                    return Err(MonitorFailure::Transport(BotError::Rpc("block subscription closed".to_string())));
                };
                context.last_seen_block = Some(header.number());
                let observation = context
                    .trigger_engine
                    .observe_block(&header, context.rpc)
                    .await;
                match observation {
                    Ok(TriggerObservation::Ready) => {
                        if event_is_canonical(context, filter).await
                            && ensure_dynamic_fields(context).await
                        {
                            let validated = Instant::now();
                            if !context.state.try_acquire_trigger() {
                                continue;
                            }
                            let acquired = Instant::now();
                            return execute_transaction(
                                context.config,
                                context.rpc,
                                context.wallet,
                                context.prepared,
                                context.state,
                                context.dry_run,
                                TriggerTiming {
                                    received,
                                    validated,
                                    acquired,
                                },
                            )
                            .await
                            .map_err(MonitorFailure::Execution);
                        }
                    }
                    Ok(TriggerObservation::NotReady) => refresh_armed_fields(context).await,
                    Err(err) => {
                        tracing::warn!(error = %err, "event confirmation evaluation failed");
                        refresh_armed_fields(context).await;
                    }
                }
            }
            log = logs.next() => {
                let received = Instant::now();
                let Some(log) = log else {
                    return Err(MonitorFailure::Transport(BotError::Rpc("log subscription closed".to_string())));
                };
                tracing::debug!(block = ?log.block_number, removed = log.removed, "contract event observed");
                let observation = context.trigger_engine.observe_event(
                    log.block_number,
                    log.block_hash,
                    log.removed,
                );
                if observation == TriggerObservation::Ready && ensure_dynamic_fields(context).await {
                    let validated = Instant::now();
                    if !context.state.try_acquire_trigger() {
                        continue;
                    }
                    let acquired = Instant::now();
                    return execute_transaction(
                        context.config,
                        context.rpc,
                        context.wallet,
                        context.prepared,
                        context.state,
                        context.dry_run,
                        TriggerTiming {
                            received,
                            validated,
                            acquired,
                        },
                    )
                    .await
                    .map_err(MonitorFailure::Execution);
                }
            }
        }
    }
}

async fn monitor_manual(
    context: &mut MonitorContext<'_>,
) -> std::result::Result<MonitorOutcome, MonitorFailure> {
    tokio::select! {
        _ = shutdown_signal() => Ok(MonitorOutcome::Shutdown),
        manual = context.manual_rx.recv() => {
            if manual.is_none() {
                return Err(MonitorFailure::Execution(BotError::ManualTrigger(
                    "manual control channel closed".to_string(),
                )));
            }
            let received = Instant::now();
            // Manual mode has no block subscription to keep nonce and automatic
            // fee fields warm, so always refresh them before accepting the trigger.
            refresh_armed_fields(context).await;
            if !context.dynamic_fields_healthy {
                return Err(MonitorFailure::Execution(BotError::Transaction(
                    "could not refresh transaction fields for manual trigger".to_string(),
                )));
            }
            let validated = Instant::now();
            if context.state.try_acquire_trigger() {
                let acquired = Instant::now();
                execute_transaction(
                    context.config,
                    context.rpc,
                    context.wallet,
                    context.prepared,
                    context.state,
                    context.dry_run,
                    TriggerTiming {
                        received,
                        validated,
                        acquired,
                    },
                )
                .await
                .map_err(MonitorFailure::Execution)
            } else {
                Ok(MonitorOutcome::Done)
            }
        }
    }
}

async fn apply_event_backfill(
    context: &mut MonitorContext<'_>,
    filter: &Filter,
    from_block: u64,
) -> Result<Option<Instant>> {
    let to_block = context.rpc.block_number().await?;
    let logs = context
        .rpc
        .logs(filter.clone().from_block(from_block).to_block(to_block))
        .await?;
    let mut ready_at = None;
    for log in logs {
        let received = Instant::now();
        if context
            .trigger_engine
            .observe_event(log.block_number, log.block_hash, log.removed)
            == TriggerObservation::Ready
        {
            ready_at.get_or_insert(received);
        }
    }
    context.last_seen_block = Some(to_block);
    Ok(ready_at)
}

async fn event_is_canonical(context: &mut MonitorContext<'_>, filter: &Filter) -> bool {
    let Some((block_number, block_hash)) = context.trigger_engine.pending_event() else {
        return false;
    };
    let canonical = context
        .rpc
        .logs(
            filter
                .clone()
                .from_block(block_number)
                .to_block(block_number),
        )
        .await
        .map(|logs| {
            logs.into_iter().any(|log| {
                !log.removed && block_hash.is_none_or(|expected| log.block_hash == Some(expected))
            })
        })
        .unwrap_or(false);
    if !canonical {
        context.trigger_engine.clear_pending_event();
        tracing::warn!(
            block = block_number,
            "discarded non-canonical activation event"
        );
    }
    canonical
}

async fn refresh_armed_fields(context: &mut MonitorContext<'_>) {
    match refresh_transaction_fields(
        context.config,
        context.rpc,
        context.wallet,
        context.prepared,
    )
    .await
    {
        Ok(()) => context.dynamic_fields_healthy = true,
        Err(err) => {
            context.dynamic_fields_healthy = false;
            tracing::warn!(error = %err, "could not refresh armed transaction fields");
        }
    }
}

async fn ensure_dynamic_fields(context: &mut MonitorContext<'_>) -> bool {
    if context.dynamic_fields_healthy {
        return true;
    }
    refresh_armed_fields(context).await;
    context.dynamic_fields_healthy
}

async fn refresh_transaction_fields(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    prepared: &mut PreparedTransaction,
) -> Result<()> {
    if matches!(config.nonce_strategy, NonceStrategy::RefreshEachBlock) {
        prepared
            .request
            .set_nonce(rpc.preload_nonce(wallet.address).await?);
    }
    if matches!(config.gas.mode, GasMode::Auto) {
        let mut estimation = rpc.estimate_eip1559_fees().await?;
        estimation.max_fee_per_gas = scale_u128(estimation.max_fee_per_gas, config.gas.multiplier)?;
        estimation.max_priority_fee_per_gas =
            scale_u128(estimation.max_priority_fee_per_gas, config.gas.multiplier)?;
        prepared
            .request
            .set_max_fee_per_gas(estimation.max_fee_per_gas);
        prepared
            .request
            .set_max_priority_fee_per_gas(estimation.max_priority_fee_per_gas);
        prepared.fee_cap = estimation.max_fee_per_gas;
    }
    validate_transaction_budget(
        config,
        rpc,
        wallet.address,
        prepared.mint_value,
        prepared.gas_limit,
        prepared.fee_cap,
    )
    .await
}

async fn execute_transaction(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    prepared: &mut PreparedTransaction,
    state: &AtomicBotState,
    dry_run: bool,
    timing: TriggerTiming,
) -> Result<MonitorOutcome> {
    let mut metrics = LatencyMetrics::new(timing.received);
    metrics.trigger_evaluation_started = timing.received;
    metrics.trigger_validated = timing.validated;
    metrics.trigger_acquired = timing.acquired;
    if dry_run {
        println!("\nTRIGGER DETECTED");
        println!("\nDry-run enabled.\nTransaction NOT submitted.");
        state.store(BotState::Stopped);
        metrics.print();
        return Ok(MonitorOutcome::Done);
    }

    metrics.finalization_started = Some(Instant::now());
    let mut request = prepared.request.clone();
    if matches!(config.nonce_strategy, NonceStrategy::JustBeforeTrigger) {
        request.set_nonce(rpc.preload_nonce(wallet.address).await?);
    }
    metrics.finalization_completed = Some(Instant::now());
    state.store(BotState::Signing);
    metrics.signing_started = Some(Instant::now());
    let signed = wallet.sign_request(request.clone()).await?;
    metrics.signing_completed = Some(Instant::now());
    let raw = signed.encoded_2718();
    state.store(BotState::Broadcasting);
    metrics.broadcast_started = Some(Instant::now());
    let (hash, rpc_elapsed) = rpc.broadcast_raw(raw).await?;
    metrics.first_rpc_response = Some(Instant::now());
    state.store(BotState::Submitted);
    println!("\nTRIGGER DETECTED");
    if let (Some(start), Some(end)) = (metrics.signing_started, metrics.signing_completed) {
        println!(
            "Signed in {:.3} ms",
            end.saturating_duration_since(start).as_secs_f64() * 1_000.0
        );
    }
    println!("TX: {hash}");
    println!(
        "RPC accepted transaction: {:.3} ms",
        rpc_elapsed.as_secs_f64() * 1000.0
    );
    println!("Waiting for receipt...");
    monitor_receipt(config, rpc, wallet, request, hash, state).await?;
    metrics.print();
    Ok(MonitorOutcome::Done)
}

async fn monitor_receipt(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    mut request: TransactionRequest,
    mut hash: B256,
    state: &AtomicBotState,
) -> Result<()> {
    let mut candidate_hashes = vec![hash];
    let mut last_replacement_block = rpc.block_number().await.ok();
    let mut replacements = 0_u32;
    let mut shutdown = Box::pin(shutdown_signal());
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                state.store(BotState::Stopped);
                println!("\nTransaction already submitted: {hash}");
                println!("Receipt monitoring stopped.");
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
        let mut mined = None;
        for candidate in candidate_hashes.iter().copied() {
            match rpc.transaction_receipt(candidate).await {
                Ok(Some(receipt)) => {
                    mined = Some((candidate, receipt));
                    break;
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(tx_hash = %candidate, error = %err, "receipt lookup failed; continuing with fallback polling");
                }
            }
        }
        if let Some((mined_hash, mut receipt)) = mined {
            if config.confirmations > 1
                && let Some(receipt_block) = receipt.block_number
            {
                let target_block =
                    receipt_block.saturating_add(config.confirmations.saturating_sub(1));
                println!(
                    "Receipt found in block {receipt_block}; waiting for {} confirmations...",
                    config.confirmations
                );
                loop {
                    let current_block = rpc.block_number().await.unwrap_or(receipt_block);
                    if current_block >= target_block {
                        break;
                    }
                    tokio::select! {
                        _ = &mut shutdown => {
                            state.store(BotState::Stopped);
                            println!("\nTransaction already submitted: {hash}");
                            println!("Receipt monitoring stopped.");
                            return Ok(());
                        }
                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                    }
                }
                match rpc.transaction_receipt(mined_hash).await {
                    Ok(Some(canonical_receipt)) => receipt = canonical_receipt,
                    Ok(None) => {
                        tracing::warn!(tx_hash = %mined_hash, "receipt disappeared during confirmation wait; resuming monitoring");
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!(tx_hash = %mined_hash, error = %err, "could not revalidate receipt; resuming monitoring");
                        continue;
                    }
                }
            }
            println!("Transaction hash: {mined_hash}");
            println!("Block: {:?}", receipt.block_number);
            println!(
                "Status: {}",
                if receipt.status() {
                    "SUCCESS"
                } else {
                    "REVERTED"
                }
            );
            println!("Gas used: {}", receipt.gas_used);
            println!("Effective gas price: {} wei", receipt.effective_gas_price);
            println!(
                "Total transaction fee: {} wei",
                u128::from(receipt.gas_used) * receipt.effective_gas_price
            );
            state.store(if receipt.status() {
                BotState::Confirmed
            } else {
                BotState::Failed
            });
            if receipt.status() {
                return Ok(());
            }
            return Err(BotError::Transaction(format!(
                "mint transaction {mined_hash} reverted"
            )));
        }
        if config.replacement.enabled && replacements < config.replacement.max_attempts {
            let Ok(current_block) = rpc.block_number().await else {
                continue;
            };
            let Some(previous_block) = last_replacement_block else {
                last_replacement_block = Some(current_block);
                continue;
            };
            if current_block >= previous_block.saturating_add(config.replacement.after_blocks) {
                let mut replacement = request.clone();
                let bumped = match bump_fees(&mut replacement, config.replacement.fee_multiplier) {
                    Ok(bumped) => bumped,
                    Err(err) => {
                        tracing::warn!(error = %err, "replacement fee calculation failed; receipt monitoring continues");
                        replacements = config.replacement.max_attempts;
                        continue;
                    }
                };
                if !bumped {
                    tracing::warn!(
                        "replacement skipped because the transaction has no bumpable fee fields"
                    );
                    replacements = config.replacement.max_attempts;
                } else if exceeds_gas_cap(config, &replacement) {
                    tracing::warn!(
                        "replacement skipped because it would exceed the configured gas safety limit"
                    );
                    replacements = config.replacement.max_attempts;
                } else {
                    let signed = match wallet.sign_request(replacement.clone()).await {
                        Ok(signed) => signed,
                        Err(err) => {
                            tracing::warn!(error = %err, "replacement signing failed; receipt monitoring continues");
                            replacements = config.replacement.max_attempts;
                            continue;
                        }
                    };
                    match rpc.broadcast_raw(signed.encoded_2718()).await {
                        Ok((replacement_hash, _)) => {
                            println!("Replacement submitted with same nonce: {replacement_hash}");
                            request = replacement;
                            hash = replacement_hash;
                            candidate_hashes.push(replacement_hash);
                            replacements += 1;
                            last_replacement_block = Some(current_block);
                        }
                        Err(err) => {
                            tracing::warn!(error = %err, "replacement broadcast failed; original receipt monitoring continues");
                            replacements = config.replacement.max_attempts;
                        }
                    }
                }
            }
        }
    }
}

fn bump_fees(request: &mut TransactionRequest, multiplier: f64) -> Result<bool> {
    if !(multiplier.is_finite() && multiplier > 1.0) {
        return Err(BotError::Config(
            "replacement.fee_multiplier must be greater than 1.0".to_string(),
        ));
    }
    if let Some(gas_price) = request.gas_price {
        request.set_gas_price(scale_u128(gas_price, multiplier)?);
        return Ok(true);
    }
    if let Some(max_fee) = request.max_fee_per_gas {
        request.set_max_fee_per_gas(scale_u128(max_fee, multiplier)?);
        if let Some(priority) = request.max_priority_fee_per_gas {
            request.set_max_priority_fee_per_gas(scale_u128(priority, multiplier)?);
        }
        return Ok(true);
    }
    Ok(false)
}

fn exceeds_gas_cap(config: &MintConfig, request: &TransactionRequest) -> bool {
    let Some(cap) = config
        .gas
        .max_total_gas_cost_native
        .as_deref()
        .and_then(|value| parse_native_amount(value).ok())
    else {
        return false;
    };
    let fee = request
        .gas_price
        .or(request.max_fee_per_gas)
        .unwrap_or_default();
    U256::from(request.gas.unwrap_or_default()) * U256::from(fee) > cap
}

fn scale_u64(value: u64, multiplier: f64) -> Result<u64> {
    let scaled = (value as f64 * multiplier).ceil();
    if !scaled.is_finite() || scaled > u64::MAX as f64 {
        return Err(BotError::Transaction(
            "scaled gas limit overflowed u64".to_string(),
        ));
    }
    Ok(scaled as u64)
}

fn scale_u128(value: u128, multiplier: f64) -> Result<u128> {
    let scaled = (value as f64 * multiplier).ceil();
    if !scaled.is_finite() || scaled > u128::MAX as f64 {
        return Err(BotError::Transaction(
            "scaled gas fee overflowed u128".to_string(),
        ));
    }
    Ok(scaled as u128)
}

fn print_armed(
    config: &MintConfig,
    wallet: &LoadedWallet,
    prepared: &PreparedTransaction,
    dry_run: bool,
) {
    println!("\n======================================");
    println!("            BOT ARMED");
    println!("======================================");
    println!("Trigger: {}", trigger_label(&config.trigger));
    println!("Quantity: {}", config.quantity);
    println!("Mint value: {} wei", prepared.mint_value);
    println!("Wallet: {}", short_address(wallet.address));
    println!("Gas limit: {}", prepared.gas_limit);
    println!("Maximum fee cap: {} wei/gas", prepared.fee_cap);
    println!(
        "Nonce: {}",
        prepared.request.nonce.map_or_else(
            || "just-before-trigger".to_string(),
            |nonce| nonce.to_string()
        )
    );
    if matches!(config.nonce_strategy, NonceStrategy::Preloaded) {
        println!(
            "WARNING: do not send other transactions from this wallet while the bot is armed."
        );
    }
    if dry_run {
        println!("Mode: DRY-RUN");
    }
    println!("\nWaiting for mint...");
}

fn trigger_label(trigger: &MintTrigger) -> String {
    match trigger {
        MintTrigger::BlockTimestamp { timestamp } => format!("block.timestamp >= {timestamp}"),
        MintTrigger::BooleanContractState {
            function,
            expected_value,
        } => format!("{function} == {expected_value}"),
        MintTrigger::NumericPhase {
            function,
            target_value,
        } => format!("{function} == {target_value}"),
        MintTrigger::ContractEvent {
            signature,
            confirmations,
        } => format!(
            "event {signature} ({} confirmations)",
            confirmations.unwrap_or_default()
        ),
        MintTrigger::Manual => "manual control socket".to_string(),
    }
}

fn filter_description(filter: &alloy::rpc::types::Filter) -> String {
    format!("event subscription ({filter:?})")
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        if let Ok(mut terminate) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        } else {
            let _ = tokio::signal::ctrl_c().await;
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
