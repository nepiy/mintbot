use crate::{
    abi::encode_mint,
    config::{
        GasMode, MintConfig, MintTrigger, NonceStrategy, OpenSeaExecutionMode, parse_gwei,
        parse_native_amount,
    },
    error::{BotError, Result},
    metrics::LatencyMetrics,
    opensea::{
        OPENSEA_SEADROP_ADDRESS, OpenSeaClient, OpenSeaDrop, OpenSeaStage, spawn_schedule_refresh,
        validate_seadrop_calldata,
    },
    rpc::{RpcClients, simulate_call},
    setup::{bind_manual_control, cleanup_manual_control, prompt_interactive_config},
    state::{AtomicBotState, BotState},
    trigger::{TriggerEngine, TriggerObservation},
    wallet::{LoadedWallet, WalletNonceLock, short_address},
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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

#[derive(Debug, Clone)]
pub struct PreparedTransaction {
    pub request: TransactionRequest,
    pub calldata: Vec<u8>,
    pub mint_value: U256,
    pub gas_limit: u64,
    pub fee_cap: u128,
    pub available_balance: U256,
    pub opensea_hydrated: bool,
}

pub async fn run_bot(config_path: PathBuf, dry_run: bool) -> Result<()> {
    let config = MintConfig::load(&config_path)?;
    run_bot_with_config(config, Some(config_path), dry_run).await
}

pub async fn run_interactive(dry_run: bool) -> Result<()> {
    let config = prompt_interactive_config()?;
    run_bot_with_config(config, None, dry_run).await
}

async fn run_bot_with_config(
    mut config: MintConfig,
    control_identity: Option<PathBuf>,
    dry_run: bool,
) -> Result<()> {
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
    let mut rpc = RpcClients::connect_from_env_for_chain(config.chain_id).await?;
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
    let auto_opensea_schedule = config.opensea_drop_slug.is_some()
        && matches!(
            &config.trigger,
            MintTrigger::BlockTimestamp { timestamp: 0 }
        );
    let opensea_client = if config.opensea_drop_slug.is_some() {
        Some(OpenSeaClient::from_env()?)
    } else {
        None
    };
    let opensea_stages = match opensea_client.as_ref() {
        Some(client) => load_opensea_stage_schedule(&config, client).await?,
        None => Vec::new(),
    };
    if let Some(stage) = opensea_stages.first() {
        config.trigger = MintTrigger::BlockTimestamp {
            timestamp: stage.start_time,
        };
    }
    if let Some(drop_slug) = config.opensea_drop_slug.as_deref() {
        let configured_contract = config.contract()?;
        opensea_client
            .as_ref()
            .ok_or_else(|| BotError::Config("OpenSea client was not initialized".to_string()))?
            .verify_collection_contract(drop_slug, config.chain_id, configured_contract)
            .await?;
    }
    let mut trigger_engine = TriggerEngine::new(&config)?;
    println!("Wallet balance: OK");
    if config.opensea_drop_slug.is_some() {
        println!("OpenSea transaction: DEFERRED until the selected stage is active");
    } else {
        println!("Calldata: PREPARED ({} bytes)", prepared.calldata.len());
    }
    println!("Signer: READY");
    println!("Gas strategy: READY");
    println!("Nonce strategy: READY");

    let manual_enabled = matches!(config.trigger, MintTrigger::Manual);
    let (mut manual_rx, control_path) = if manual_enabled {
        let identity = control_identity.as_deref().ok_or_else(|| {
            BotError::ManualTrigger(
                "interactive mode requires an automatic trigger; use `run --config ...` for manual control"
                    .to_string(),
            )
        })?;
        let (receiver, path) = bind_manual_control(identity).await?;
        (receiver, Some(path))
    } else {
        let (_sender, receiver) = tokio::sync::mpsc::channel(1);
        (receiver, None)
    };

    let (streams, last_seen_block) = prepare_monitor_streams(&rpc, &trigger_engine, None).await?;
    println!("Subscriptions: READY");

    let (opensea_schedule, opensea_schedule_task) = if !opensea_stages.is_empty() {
        if auto_opensea_schedule {
            let client = opensea_client.clone().ok_or_else(|| {
                BotError::Config("OpenSea client was not initialized".to_string())
            })?;
            let drop_slug = config.opensea_drop_slug.clone().ok_or_else(|| {
                BotError::Config("OpenSea drop slug was not configured".to_string())
            })?;
            let (receiver, task) =
                spawn_schedule_refresh(client, drop_slug, opensea_stages.clone());
            (Some(receiver), Some(task))
        } else {
            let (_sender, receiver) = watch::channel(opensea_stages.clone());
            (Some(receiver), None)
        }
    } else {
        (None, None)
    };

    state.store(BotState::Armed);
    print_armed(&config, &wallet, &prepared, dry_run);
    if auto_opensea_schedule {
        println!("OpenSea schedule: REFRESHING while waiting (5s near a stage, otherwise 30s)");
    }
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
        opensea_stages,
        opensea_stage_index: 0,
        opensea_client: opensea_client.as_ref(),
        auto_opensea_schedule,
        opensea_schedule,
    };
    let result = monitor_until_trigger(&mut monitor, streams).await;
    if let Some(task) = opensea_schedule_task {
        task.abort();
        let _ = task.await;
    }
    if let Some(path) = control_path {
        cleanup_manual_control(&path);
    }
    result
}

pub async fn run_simulation(config_path: PathBuf) -> Result<()> {
    let config = MintConfig::load(&config_path)?;
    let wallet = LoadedWallet::from_env()?;
    let mut rpc = RpcClients::connect_from_env_for_chain(config.chain_id).await?;
    rpc.validate_chain(&config).await?;
    rpc.validate_contract(&config).await?;
    let mut prepared = prepare_transaction(&config, &rpc, &wallet).await?;
    if config.opensea_drop_slug.is_some() {
        hydrate_opensea_transaction(&config, &rpc, &wallet, &mut prepared).await?;
    }
    println!("SIMULATION");
    println!("--------------------------------");
    println!("Collection: {}", config.name);
    println!("Chain ID: {}", config.chain_id);
    println!("Wallet: {}", short_address(wallet.address));
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
    if config.opensea_drop_slug.is_some() {
        // Fail before arming if the API credential is missing rather than
        // discovering it only when the stage opens.
        let _ = OpenSeaClient::from_env()?;
    }
    let (calldata, mint_value) = if config.opensea_drop_slug.is_some() {
        // OpenSea only returns valid calldata once an eligible stage is active.
        // Build a safe zero-value placeholder so the bot can arm in advance.
        (Vec::new(), U256::ZERO)
    } else {
        let calldata = encode_mint(
            &config.mint,
            config.quantity,
            wallet.address,
            config.mint.proof.as_deref(),
        )?;
        (calldata.bytes, config.mint_value_wei()?)
    };
    let nonce = if uses_cached_nonce(config) {
        Some(rpc.preload_nonce(wallet.address).await?)
    } else {
        None
    };
    let mut request = TransactionRequest::default()
        .with_from(wallet.address)
        .with_to(contract)
        .with_chain_id(config.chain_id)
        .with_input(calldata.clone())
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

    if let Some(nonce) = nonce {
        request.set_nonce(nonce);
    }
    let budgeted_mint_value = if config.opensea_drop_slug.is_some() {
        // A generic OpenSea 422 can mean insufficient balance. Reserve against
        // the user's configured maximum before arming so that case is caught
        // locally and does not look like a transient stage failure.
        config
            .maximum_opensea_mint_value_wei()?
            .unwrap_or(U256::ZERO)
    } else {
        mint_value
    };
    let available_balance = validate_transaction_budget(
        config,
        rpc,
        wallet.address,
        budgeted_mint_value,
        gas_limit,
        fee_cap,
    )
    .await?;
    Ok(PreparedTransaction {
        request,
        calldata,
        mint_value,
        gas_limit,
        fee_cap,
        available_balance,
        opensea_hydrated: false,
    })
}

async fn load_opensea_stage_schedule(
    config: &MintConfig,
    client: &OpenSeaClient,
) -> Result<Vec<OpenSeaStage>> {
    let Some(drop_slug) = config.opensea_drop_slug.as_deref() else {
        return Ok(Vec::new());
    };
    let minimum_start = match config.trigger {
        MintTrigger::BlockTimestamp { timestamp } => timestamp,
        _ => {
            return Err(BotError::Config(
                "OpenSea mode requires a block timestamp trigger".to_string(),
            ));
        }
    };
    let drop = client.get_drop(drop_slug).await?;
    ensure_opensea_supply(&drop, config.quantity)?;
    let stages = drop.stages;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut stages = stages
        .into_iter()
        .filter(|stage| {
            if minimum_start > 0 {
                stage.start_time >= minimum_start
            } else {
                stage.end_time.is_none_or(|end_time| end_time >= now)
            }
        })
        .collect::<Vec<_>>();
    stages.sort_by_key(|stage| stage.start_time);
    if stages.is_empty() {
        return Err(BotError::Config(
            "OpenSea returned no upcoming or active stages matching the selected start time"
                .to_string(),
        ));
    }
    Ok(stages)
}

async fn hydrate_opensea_transaction(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    prepared: &mut PreparedTransaction,
) -> Result<()> {
    let client = OpenSeaClient::from_env()?;
    hydrate_opensea_transaction_with_client(config, rpc, wallet, prepared, &client).await
}

async fn hydrate_opensea_transaction_with_client(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    prepared: &mut PreparedTransaction,
    client: &OpenSeaClient,
) -> Result<()> {
    let Some(drop_slug) = config.opensea_drop_slug.as_deref() else {
        return Ok(());
    };
    // These requests do not depend on one another. Overlap them so the
    // trigger path pays for the slower of the OpenSea request and the fee
    // estimate, rather than their sum.
    let build_mint = client.build_mint_with_retry(drop_slug, wallet.address, config.quantity);
    let estimate_fees = async {
        if matches!(config.gas.mode, GasMode::Auto) && !is_aggressive_opensea(config) {
            let mut estimation = rpc.estimate_eip1559_fees().await?;
            estimation.max_fee_per_gas =
                scale_u128(estimation.max_fee_per_gas, config.gas.multiplier)?;
            estimation.max_priority_fee_per_gas =
                scale_u128(estimation.max_priority_fee_per_gas, config.gas.multiplier)?;
            Ok(Some(estimation))
        } else {
            Ok(None)
        }
    };
    let (mint, current_fees) = tokio::try_join!(build_mint, estimate_fees)?;
    if mint.target != OPENSEA_SEADROP_ADDRESS {
        return Err(BotError::Transaction(format!(
            "OpenSea returned unsupported transaction target {}; expected the canonical SeaDrop contract {}",
            mint.target, OPENSEA_SEADROP_ADDRESS
        )));
    }
    validate_opensea_mint_value(config, mint.value)?;
    validate_seadrop_calldata(
        &mint.calldata,
        config.contract()?,
        wallet.address,
        config.quantity,
        mint.value,
    )?;

    let mut request = TransactionRequest::default()
        .with_from(wallet.address)
        .with_to(mint.target)
        .with_chain_id(config.chain_id)
        .with_input(mint.calldata.clone())
        .with_value(mint.value);
    if let Some(nonce) = prepared.request.nonce {
        request.set_nonce(nonce);
    }
    if let Some(gas_price) = prepared.request.gas_price {
        request.set_gas_price(gas_price);
    }
    if let Some(max_fee) = prepared.request.max_fee_per_gas {
        request.set_max_fee_per_gas(max_fee);
    }
    if let Some(priority) = prepared.request.max_priority_fee_per_gas {
        request.set_max_priority_fee_per_gas(priority);
    }

    let fee_cap = if let Some(estimation) = current_fees {
        request.set_max_fee_per_gas(estimation.max_fee_per_gas);
        request.set_max_priority_fee_per_gas(estimation.max_priority_fee_per_gas);
        estimation.max_fee_per_gas
    } else {
        prepared.fee_cap
    };

    let gas_limit = if is_aggressive_opensea(config) {
        let configured = config
            .gas
            .gas_limit
            .or(prepared.request.gas)
            .ok_or_else(|| {
                BotError::Config(
                "aggressive OpenSea mode requires gas.gas_limit because gas estimation is skipped"
                    .to_string(),
            )
            })?;
        scale_u64(configured, config.gas.multiplier)?
    } else {
        let estimated_gas = rpc.estimate_gas(request.clone()).await.map_err(|err| {
            BotError::Transaction(format!(
                "OpenSea transaction gas estimation failed after the stage opened: {err}"
            ))
        })?;
        select_opensea_gas_limit(estimated_gas, config.gas.gas_limit, config.gas.multiplier)?
    };
    request.set_gas_limit(gas_limit);
    let available_balance = if is_aggressive_opensea(config) {
        validate_transaction_budget_with_balance(
            config,
            prepared.available_balance,
            mint.value,
            gas_limit,
            fee_cap,
        )?;
        prepared.available_balance
    } else {
        validate_transaction_budget(config, rpc, wallet.address, mint.value, gas_limit, fee_cap)
            .await?
    };
    prepared.request = request;
    prepared.calldata = mint.calldata;
    prepared.mint_value = mint.value;
    prepared.gas_limit = gas_limit;
    prepared.fee_cap = fee_cap;
    prepared.available_balance = available_balance;
    prepared.opensea_hydrated = true;
    Ok(())
}

fn validate_opensea_mint_value(config: &MintConfig, value: U256) -> Result<()> {
    if config.require_zero_value && !value.is_zero() {
        return Err(BotError::Transaction(format!(
            "OpenSea returned a nonzero mint value of {} wei; free-mint price guard refused to sign",
            value
        )));
    }
    if let Some(maximum) = config.maximum_opensea_mint_value_wei()?
        && value > maximum
    {
        return Err(BotError::MintValueExceeded {
            returned: value.to_string(),
            maximum: maximum.to_string(),
        });
    }
    Ok(())
}

async fn validate_transaction_budget(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: alloy::primitives::Address,
    mint_value: U256,
    gas_limit: u64,
    fee_cap: u128,
) -> Result<U256> {
    let balance = rpc.check_balance(wallet).await?;
    validate_transaction_budget_with_balance(config, balance, mint_value, gas_limit, fee_cap)?;
    Ok(balance)
}

fn validate_transaction_budget_with_balance(
    config: &MintConfig,
    balance: U256,
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
    opensea_stages: Vec<OpenSeaStage>,
    opensea_stage_index: usize,
    opensea_client: Option<&'a OpenSeaClient>,
    auto_opensea_schedule: bool,
    opensea_schedule: Option<watch::Receiver<Vec<OpenSeaStage>>>,
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
                let backfill_can_execute = if backfill_ready.is_some() {
                    match ensure_transaction_ready(context).await {
                        Ok(ready) => ready,
                        Err(err) => return Err(err),
                    }
                } else {
                    false
                };
                if let Some(received) = backfill_ready
                    && backfill_can_execute
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
                if let Err(err) = context.apply_opensea_schedule_update() {
                    return Err(MonitorFailure::Execution(err));
                }
                let number = header.number();
                match context
                    .trigger_engine
                    .observe_block(&header, context.rpc)
                    .await
                {
                    Ok(TriggerObservation::Ready) => {
                        match ensure_transaction_ready(context).await {
                            Err(err) => return Err(MonitorFailure::Execution(err)),
                            Ok(false) => continue,
                            Ok(true) => {}
                        }
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
                if let Err(err) = context.apply_opensea_schedule_update() {
                    return Err(MonitorFailure::Execution(err));
                }
                let observation = context
                    .trigger_engine
                    .observe_block(&header, context.rpc)
                    .await;
                match observation {
                    Ok(TriggerObservation::Ready) => {
                        if !event_is_canonical(context, filter).await {
                            continue;
                        }
                        match ensure_transaction_ready(context).await {
                            Err(err) => return Err(MonitorFailure::Execution(err)),
                            Ok(false) => continue,
                            Ok(true) => {}
                        }
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
                if observation != TriggerObservation::Ready {
                    continue;
                }
                match ensure_transaction_ready(context).await {
                    Err(err) => return Err(MonitorFailure::Execution(err)),
                    Ok(false) => continue,
                    Ok(true) => {}
                }
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
            match ensure_transaction_ready(context).await {
                Ok(true) => {}
                Ok(false) => {
                    return Err(MonitorFailure::Execution(BotError::Transaction(
                        "could not prepare transaction for manual trigger".to_string(),
                    )));
                }
                Err(err) => return Err(MonitorFailure::Execution(err)),
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

async fn ensure_transaction_ready(context: &mut MonitorContext<'_>) -> Result<bool> {
    if !ensure_dynamic_fields(context).await {
        return Ok(false);
    }
    if context.config.opensea_drop_slug.is_some() && !context.prepared.opensea_hydrated {
        let client = context
            .opensea_client
            .ok_or_else(|| BotError::Config("OpenSea client was not initialized".to_string()))?;
        let hydration = hydrate_opensea_transaction_with_client(
            context.config,
            context.rpc,
            context.wallet,
            context.prepared,
            client,
        )
        .await;
        if let Err(err) = hydration {
            let stage_unavailable = matches!(
                &err,
                BotError::OpenSeaApi {
                    status: 409 | 422,
                    ..
                }
            );
            if stage_unavailable {
                if context.auto_opensea_schedule && context.refresh_opensea_schedule_now().await? {
                    return Ok(false);
                }
                if is_advanceable_opensea_rejection(&err) {
                    if advance_opensea_stage(context)? {
                        return Ok(false);
                    }
                } else if context.auto_opensea_schedule && is_ambiguous_opensea_precondition(&err) {
                    // A bare 422 does not tell us whether the API is briefly
                    // behind the active stage or whether a hard precondition
                    // failed. Retrying preserves an eligible mint window. Only
                    // move on once the next scheduled stage has actually begun.
                    if advance_to_started_opensea_stage(context)? {
                        return Ok(false);
                    }
                    let delay_seconds = if is_aggressive_opensea(context.config) {
                        0
                    } else {
                        2
                    };
                    context.defer_automatic_opensea_retry(delay_seconds)?;
                    let label = context
                        .opensea_stages
                        .get(context.opensea_stage_index)
                        .map_or("current stage", |stage| stage.label.as_str());
                    println!(
                        "[WARN] OpenSea returned an ambiguous validation failure for {label}; retrying the same stage"
                    );
                    return Ok(false);
                } else if context.auto_opensea_schedule
                    && is_opensea_stage_not_active_rejection(&err)
                {
                    // OpenSea can lag the chain at the exact stage boundary.
                    // Keep the current trigger armed and retry on the next block
                    // instead of skipping a potentially eligible stage.
                    let delay_seconds = if is_aggressive_opensea(context.config) {
                        0
                    } else {
                        2
                    };
                    context.defer_automatic_opensea_retry(delay_seconds)?;
                    return Ok(false);
                }
            }
            return Err(err);
        }
    }
    Ok(true)
}

impl MonitorContext<'_> {
    fn defer_automatic_opensea_retry(&mut self, delay_seconds: u64) -> Result<()> {
        if !self.auto_opensea_schedule {
            return Ok(());
        }
        let retry_at = automatic_opensea_retry_timestamp(delay_seconds);
        self.trigger_engine.set_block_timestamp(retry_at)?;
        self.prepared.opensea_hydrated = false;
        Ok(())
    }

    fn apply_opensea_schedule_update(&mut self) -> Result<()> {
        if !self.auto_opensea_schedule {
            return Ok(());
        }
        let Some(schedule) = self.opensea_schedule.as_mut() else {
            return Ok(());
        };
        if !schedule.has_changed().unwrap_or(false) {
            return Ok(());
        }

        let updated = schedule.borrow_and_update().clone();
        if updated.is_empty() {
            return Ok(());
        }
        let selected_index =
            refreshed_stage_index(&self.opensea_stages, self.opensea_stage_index, &updated);
        let stage = updated[selected_index].clone();
        self.opensea_stages = updated;
        self.opensea_stage_index = selected_index;
        self.prepared.opensea_hydrated = false;
        self.trigger_engine.set_block_timestamp(stage.start_time)?;
        println!(
            "[INFO] OpenSea schedule refreshed; selected {} at Unix timestamp {}",
            stage.label, stage.start_time
        );
        Ok(())
    }

    async fn refresh_opensea_schedule_now(&mut self) -> Result<bool> {
        let Some(drop_slug) = self.config.opensea_drop_slug.as_deref() else {
            return Ok(false);
        };
        let Some(client) = self.opensea_client else {
            return Ok(false);
        };
        let drop = match client.get_drop(drop_slug).await {
            Ok(drop) => drop,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "OpenSea stage schedule refresh after a rejected mint request failed"
                );
                return Ok(false);
            }
        };
        ensure_opensea_supply(&drop, self.config.quantity)?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut stages = drop
            .stages
            .into_iter()
            .filter(|stage| stage.end_time.is_none_or(|end_time| end_time >= now))
            .collect::<Vec<_>>();
        stages.sort_by_key(|stage| stage.start_time);
        if stages.is_empty() || stages == self.opensea_stages {
            return Ok(false);
        }
        let selected_index =
            refreshed_stage_index(&self.opensea_stages, self.opensea_stage_index, &stages);
        let stage = stages[selected_index].clone();
        self.opensea_stages = stages;
        self.opensea_stage_index = selected_index;
        self.prepared.opensea_hydrated = false;
        self.trigger_engine.set_block_timestamp(stage.start_time)?;
        println!(
            "[INFO] OpenSea schedule refreshed after rejection; selected {} at Unix timestamp {}",
            stage.label, stage.start_time
        );
        Ok(true)
    }
}

fn is_opensea_stage_not_active_rejection(error: &BotError) -> bool {
    matches!(error, BotError::OpenSeaApi { status: 409, .. })
}

fn is_advanceable_opensea_rejection(error: &BotError) -> bool {
    matches!(
        error,
        BotError::OpenSeaApi {
            status: 422,
            message,
        } if message == "wallet is not eligible for this stage"
            || message == "supply exhausted or unavailable"
            || message == "wallet mint limit exceeded"
    )
}

fn is_ambiguous_opensea_precondition(error: &BotError) -> bool {
    matches!(
        error,
        BotError::OpenSeaApi {
            status: 422,
            message,
        } if message == "OpenSea mint precondition failed (eligibility, limit, balance, or supply)"
    )
}

fn advance_to_started_opensea_stage(context: &mut MonitorContext<'_>) -> Result<bool> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    if !next_opensea_stage_has_started(&context.opensea_stages, context.opensea_stage_index, now) {
        return Ok(false);
    }
    advance_opensea_stage(context)
}

fn next_opensea_stage_has_started(stages: &[OpenSeaStage], current_index: usize, now: u64) -> bool {
    next_opensea_stage(stages, current_index)
        .is_some_and(|(_, next_stage)| next_stage.start_time <= now)
}

fn ensure_opensea_supply(drop: &OpenSeaDrop, quantity: u64) -> Result<()> {
    let Some(remaining) = drop.remaining_supply() else {
        return Ok(());
    };
    if remaining < U256::from(quantity) {
        return Err(BotError::Transaction(format!(
            "OpenSea drop has {remaining} NFTs remaining, fewer than the requested quantity {quantity}"
        )));
    }
    Ok(())
}

fn advance_opensea_stage(context: &mut MonitorContext<'_>) -> Result<bool> {
    let Some((next_index, next_stage)) =
        next_opensea_stage(&context.opensea_stages, context.opensea_stage_index)
            .map(|(index, stage)| (index, stage.clone()))
    else {
        if context.auto_opensea_schedule {
            // A later stage may not be published by OpenSea yet. Keep the
            // monitor alive and let the background schedule refresh discover
            // it, but avoid retrying the mint endpoint on every block.
            context.defer_automatic_opensea_retry(5)?;
            println!(
                "[INFO] OpenSea stage unavailable; no later stage is published yet, continuing automatic monitoring"
            );
            return Ok(true);
        }
        return Ok(false);
    };
    context.opensea_stage_index = next_index;
    context
        .trigger_engine
        .set_block_timestamp(next_stage.start_time)?;
    context.prepared.opensea_hydrated = false;
    println!(
        "[INFO] OpenSea stage unavailable; waiting for {} at Unix timestamp {}",
        next_stage.label, next_stage.start_time
    );
    Ok(true)
}

fn next_opensea_stage(
    stages: &[OpenSeaStage],
    current_index: usize,
) -> Option<(usize, &OpenSeaStage)> {
    let next_index = current_index.saturating_add(1);
    stages.get(next_index).map(|stage| (next_index, stage))
}

fn refreshed_stage_index(
    previous: &[OpenSeaStage],
    previous_index: usize,
    updated: &[OpenSeaStage],
) -> usize {
    let Some(selected) = previous.get(previous_index) else {
        return previous_index.min(updated.len().saturating_sub(1));
    };
    updated
        .iter()
        .position(|stage| stage == selected)
        .or_else(|| {
            updated
                .iter()
                .position(|stage| stage.label == selected.label)
        })
        .or_else(|| {
            updated
                .iter()
                .position(|stage| stage.start_time >= selected.start_time)
        })
        .unwrap_or_else(|| previous_index.min(updated.len().saturating_sub(1)))
}

fn automatic_opensea_retry_timestamp(delay_seconds: u64) -> u64 {
    if delay_seconds == 0 {
        // The current block has already been processed, so zero makes the
        // trigger ready on the next block without relying on wall-clock skew.
        return 0;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_add(delay_seconds)
}

async fn refresh_transaction_fields(
    config: &MintConfig,
    rpc: &RpcClients,
    wallet: &LoadedWallet,
    prepared: &mut PreparedTransaction,
) -> Result<()> {
    // Normal OpenSea mode deliberately obtains fresh fees, gas, balance, and
    // nonce after OpenSea returns the final calldata. Refreshing those same
    // fields on every waiting block only consumes RPC quota and can turn an
    // irrelevant transient read failure into trigger-path latency.
    if config.opensea_drop_slug.is_some() && !is_aggressive_opensea(config) {
        return Ok(());
    }
    let refresh_nonce = matches!(config.nonce_strategy, NonceStrategy::RefreshEachBlock)
        || is_aggressive_opensea(config);
    let nonce = async {
        if refresh_nonce {
            Ok(Some(rpc.preload_nonce(wallet.address).await?))
        } else {
            Ok(None)
        }
    };
    let fees = async {
        if matches!(config.gas.mode, GasMode::Auto) {
            let mut estimation = rpc.estimate_eip1559_fees().await?;
            estimation.max_fee_per_gas =
                scale_u128(estimation.max_fee_per_gas, config.gas.multiplier)?;
            estimation.max_priority_fee_per_gas =
                scale_u128(estimation.max_priority_fee_per_gas, config.gas.multiplier)?;
            Ok(Some(estimation))
        } else {
            Ok(None)
        }
    };
    let (nonce, fees, balance) = tokio::try_join!(nonce, fees, rpc.check_balance(wallet.address))?;
    if let Some(nonce) = nonce {
        prepared.request.set_nonce(nonce);
    }
    if let Some(estimation) = fees {
        prepared
            .request
            .set_max_fee_per_gas(estimation.max_fee_per_gas);
        prepared
            .request
            .set_max_priority_fee_per_gas(estimation.max_priority_fee_per_gas);
        prepared.fee_cap = estimation.max_fee_per_gas;
    }
    validate_transaction_budget_with_balance(
        config,
        balance,
        prepared.mint_value,
        prepared.gas_limit,
        prepared.fee_cap,
    )?;
    prepared.available_balance = balance;
    Ok(())
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

    if config.opensea_drop_slug.is_some() && !prepared.opensea_hydrated {
        hydrate_opensea_transaction(config, rpc, wallet, prepared).await?;
    }

    metrics.finalization_started = Some(Instant::now());
    let nonce_lock = WalletNonceLock::acquire(config.chain_id, wallet.address).await?;
    let mut request = prepared.request.clone();
    if !uses_cached_nonce(config) || nonce_lock.was_contended() {
        // A cached nonce is no longer trustworthy after waiting for another
        // bot process using the same wallet to release its cross-process lock.
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
    drop(nonce_lock);
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
    U256::from(request.gas.unwrap_or_default())
        .checked_mul(U256::from(fee))
        .is_none_or(|cost| cost > cap)
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

fn is_aggressive_opensea(config: &MintConfig) -> bool {
    config.opensea_drop_slug.is_some()
        && matches!(
            config.opensea_execution_mode,
            OpenSeaExecutionMode::Aggressive
        )
}

fn uses_cached_nonce(config: &MintConfig) -> bool {
    matches!(
        config.nonce_strategy,
        NonceStrategy::Preloaded | NonceStrategy::RefreshEachBlock
    ) || is_aggressive_opensea(config)
}

fn select_opensea_gas_limit(
    estimated: u64,
    configured_default: Option<u64>,
    multiplier: f64,
) -> Result<u64> {
    let base = configured_default.map_or(estimated, |configured| configured.max(estimated));
    scale_u64(base, multiplier)
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
    if let Some(slug) = config.opensea_drop_slug.as_deref() {
        println!("OpenSea drop: {slug}");
        println!("Mint value: fetched from OpenSea when the stage is active");
        println!(
            "OpenSea execution: {}",
            match config.opensea_execution_mode {
                OpenSeaExecutionMode::Normal => "normal (fresh gas simulation)",
                OpenSeaExecutionMode::Aggressive => {
                    "aggressive (configured gas limit; gas simulation skipped)"
                }
            }
        );
        if config.require_zero_value {
            println!("Price guard: REQUIRED FREE MINT (nonzero value will abort)");
        } else if let Some(maximum_per_nft) = config.max_price_per_nft.as_deref()
            && let Ok(Some(maximum_total)) = config.maximum_opensea_mint_value_wei()
        {
            println!(
                "Price guard: <= {maximum_per_nft} per NFT ({maximum_total} wei total maximum)"
            );
        }
    } else {
        println!("Mint value: {} wei", prepared.mint_value);
    }
    println!("Wallet: {}", short_address(wallet.address));
    println!("Gas limit: {}", prepared.gas_limit);
    println!("Current maximum fee: {} wei/gas", prepared.fee_cap);
    if let Some(maximum) = config.gas.max_total_gas_cost_native.as_deref() {
        println!("Maximum total gas cost: {maximum} native currency");
    }
    println!(
        "Nonce: {}",
        prepared.request.nonce.map_or_else(
            || "just-before-trigger".to_string(),
            |nonce| nonce.to_string()
        )
    );
    if uses_cached_nonce(config) {
        println!(
            "WARNING: do not send other transactions from this wallet while the bot is armed; the cached nonce could become stale."
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

#[cfg(test)]
mod tests {
    use super::{
        automatic_opensea_retry_timestamp, ensure_opensea_supply, is_advanceable_opensea_rejection,
        is_aggressive_opensea, is_ambiguous_opensea_precondition,
        is_opensea_stage_not_active_rejection, next_opensea_stage, next_opensea_stage_has_started,
        refreshed_stage_index, select_opensea_gas_limit, uses_cached_nonce,
        validate_opensea_mint_value, validate_transaction_budget_with_balance,
    };
    use crate::config::{MintConfig, OpenSeaExecutionMode};
    use crate::error::BotError;
    use crate::opensea::{OpenSeaDrop, OpenSeaStage};
    use alloy::primitives::U256;

    fn opensea_config(require_zero_value: bool, maximum: Option<&str>) -> MintConfig {
        serde_json::from_value(serde_json::json!({
            "name": "Price guard test",
            "chain_id": 4663,
            "contract_address": "0x0000000000000000000000000000000000000001",
            "opensea_drop_slug": "price-guard-test",
            "require_zero_value": require_zero_value,
            "max_price_per_nft": maximum,
            "quantity": 2,
            "mint": { "function": "mint(uint256)" },
            "trigger": { "type": "block_timestamp", "timestamp": 0 }
        }))
        .expect("valid test config")
    }

    #[test]
    fn paid_opensea_guard_accepts_lower_price_and_rejects_higher_price() {
        let config = opensea_config(false, Some("0.001"));
        assert!(validate_opensea_mint_value(&config, U256::from(1_000_000_000_000_000u64)).is_ok());
        assert!(validate_opensea_mint_value(&config, U256::from(2_000_000_000_000_000u64)).is_ok());
        assert!(
            validate_opensea_mint_value(&config, U256::from(2_000_000_000_000_001u64)).is_err()
        );
    }

    #[test]
    fn free_opensea_guard_rejects_any_payment() {
        let config = opensea_config(true, Some("0"));
        assert!(validate_opensea_mint_value(&config, U256::ZERO).is_ok());
        assert!(validate_opensea_mint_value(&config, U256::from(1)).is_err());
    }

    #[test]
    fn aggressive_opensea_uses_the_refreshed_nonce_cache() {
        let mut config = opensea_config(true, Some("0"));
        assert!(!is_aggressive_opensea(&config));
        config.opensea_execution_mode = OpenSeaExecutionMode::Aggressive;
        assert!(is_aggressive_opensea(&config));
        assert!(uses_cached_nonce(&config));
    }

    #[test]
    fn refresh_each_block_nonce_strategy_uses_its_cached_nonce() {
        let mut config = opensea_config(true, Some("0"));
        config.opensea_drop_slug = None;
        config.nonce_strategy = crate::config::NonceStrategy::RefreshEachBlock;
        assert!(uses_cached_nonce(&config));
    }

    #[test]
    fn zero_delay_opensea_retry_is_ready_on_the_next_block() {
        assert_eq!(automatic_opensea_retry_timestamp(0), 0);
        assert!(automatic_opensea_retry_timestamp(2) >= 2);
    }

    #[test]
    fn cached_balance_budget_check_keeps_payment_and_gas_caps() {
        let mut config = opensea_config(false, Some("1"));
        config.gas.max_total_gas_cost_native = Some("0.001".to_string());
        assert!(
            validate_transaction_budget_with_balance(
                &config,
                U256::from(100),
                U256::from(80),
                10,
                2,
            )
            .is_ok()
        );
        assert!(
            validate_transaction_budget_with_balance(
                &config,
                U256::from(99),
                U256::from(80),
                10,
                2,
            )
            .is_err()
        );
    }

    #[test]
    fn opensea_gas_limit_never_uses_less_than_the_live_estimate() {
        assert_eq!(
            select_opensea_gas_limit(250_000, Some(200_000), 1.15).unwrap(),
            287_500
        );
        assert_eq!(
            select_opensea_gas_limit(150_000, Some(200_000), 1.15).unwrap(),
            230_000
        );
    }

    #[test]
    fn only_stage_or_eligibility_failures_advance_the_stage() {
        let not_active = BotError::OpenSeaApi {
            status: 409,
            message: "drop stage is not active".to_string(),
        };
        assert!(is_opensea_stage_not_active_rejection(&not_active));
        assert!(!is_advanceable_opensea_rejection(&not_active));

        let ineligible = BotError::OpenSeaApi {
            status: 422,
            message: "wallet is not eligible for this stage".to_string(),
        };
        assert!(is_advanceable_opensea_rejection(&ineligible));

        let stage_limit = BotError::OpenSeaApi {
            status: 422,
            message: "wallet mint limit exceeded".to_string(),
        };
        assert!(is_advanceable_opensea_rejection(&stage_limit));

        let balance = BotError::OpenSeaApi {
            status: 422,
            message: "insufficient native balance for the mint".to_string(),
        };
        assert!(!is_advanceable_opensea_rejection(&balance));

        let ambiguous_precondition = BotError::OpenSeaApi {
            status: 422,
            message: "OpenSea mint precondition failed (eligibility, limit, balance, or supply)"
                .to_string(),
        };
        assert!(!is_advanceable_opensea_rejection(&ambiguous_precondition));
        assert!(is_ambiguous_opensea_precondition(&ambiguous_precondition));
    }

    #[test]
    fn automatic_stage_cursor_can_move_after_a_previous_phase_was_used() {
        let stages = vec![
            OpenSeaStage {
                label: "GTD".to_string(),
                start_time: 100,
                end_time: Some(200),
            },
            OpenSeaStage {
                label: "FCFS".to_string(),
                start_time: 300,
                end_time: Some(400),
            },
        ];

        let (index, next) = next_opensea_stage(&stages, 0).expect("FCFS should follow GTD");
        assert_eq!(index, 1);
        assert_eq!(next.label, "FCFS");
        assert!(!next_opensea_stage_has_started(&stages, 0, 299));
        assert!(next_opensea_stage_has_started(&stages, 0, 300));
        assert!(next_opensea_stage(&stages, index).is_none());
    }

    #[test]
    fn live_drop_supply_must_cover_the_requested_quantity() {
        let drop = OpenSeaDrop {
            stages: Vec::new(),
            total_supply: Some(U256::from(3_332)),
            max_supply: Some(U256::from(3_333)),
        };
        assert!(ensure_opensea_supply(&drop, 1).is_ok());
        assert!(ensure_opensea_supply(&drop, 2).is_err());
    }

    #[test]
    fn schedule_refresh_preserves_the_selected_later_stage() {
        let previous = vec![
            OpenSeaStage {
                label: "GTD".to_string(),
                start_time: 100,
                end_time: Some(200),
            },
            OpenSeaStage {
                label: "FCFS".to_string(),
                start_time: 300,
                end_time: Some(400),
            },
        ];
        let updated = vec![
            previous[0].clone(),
            OpenSeaStage {
                label: "FCFS".to_string(),
                start_time: 290,
                end_time: Some(400),
            },
            OpenSeaStage {
                label: "Public".to_string(),
                start_time: 500,
                end_time: None,
            },
        ];

        assert_eq!(refreshed_stage_index(&previous, 1, &updated), 1);
    }
}
