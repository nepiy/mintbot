use crate::{
    config::MintConfig,
    error::{BotError, Result},
    security::summarize_rpc_error,
};
use alloy::{
    eips::{BlockId, eip1559::Eip1559Estimation},
    network::Ethereum,
    primitives::{Address, B256, U256, keccak256},
    providers::{DynProvider, Provider, ProviderBuilder, WsConnect},
    pubsub::SubscriptionStream,
    rpc::types::{Filter, Header, Log, TransactionReceipt, TransactionRequest},
};
use futures_util::{StreamExt, stream::FuturesUnordered};
use reqwest::Url;
use std::{
    collections::HashSet,
    env,
    sync::Arc,
    time::{Duration, Instant},
};

const CRITICAL_READ_SETTLE_WINDOW: Duration = Duration::from_millis(75);
// Gas estimates are already protected by the configured gas default and
// multiplier. Keep a short cross-provider window for a higher estimate
// without adding the full consistency delay to the mint critical path.
const GAS_ESTIMATE_SETTLE_WINDOW: Duration = Duration::from_millis(25);

#[derive(Clone)]
pub struct RpcClients {
    pub http: DynProvider<Ethereum>,
    pub ws: DynProvider<Ethereum>,
    pub broadcast: Arc<Vec<(String, DynProvider<Ethereum>)>>,
    pub ws_connect_latency: Duration,
    read: Arc<Vec<(String, DynProvider<Ethereum>)>>,
    request_timeout: Duration,
    broadcast_timeout: Duration,
    ws_url: String,
}

impl std::fmt::Debug for RpcClients {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RpcClients")
            .field("broadcast_count", &self.broadcast.len())
            .finish_non_exhaustive()
    }
}

impl RpcClients {
    pub async fn latest_timestamp(&self) -> Result<u64> {
        let blocks = self
            .read_critical("eth_getBlockByNumber", |provider| async move {
                provider
                    .get_block_by_number(alloy::eips::BlockNumberOrTag::Latest)
                    .await
            })
            .await?;
        blocks
            .into_iter()
            .flatten()
            .map(|block| block.header.timestamp)
            .min()
            .ok_or_else(|| BotError::Rpc("no provider returned a latest block timestamp".into()))
    }

    /// Conservative OP Stack surcharges at the current oracle state. Fail
    /// closed if the oracle is unavailable or its ABI is unsupported.
    pub async fn ink_extra_fee_reserve(&self, calldata_len: usize, gas_limit: u64) -> Result<U256> {
        use alloy::{network::TransactionBuilder, primitives::address, sol_types::SolCall};
        alloy::sol! {
            function getL1FeeUpperBound(uint256 size) external view returns (uint256);
            function getOperatorFee(uint256 gasUsed) external view returns (uint256);
        }
        // Legacy/type-2 requests without access lists have <256 bytes of RLP
        // metadata, even at maximum field widths. Include signature overhead
        // too; overestimating the unsigned size is intentionally conservative.
        let size = calldata_len
            .checked_add(256)
            .ok_or_else(|| BotError::Transaction("transaction size overflowed".into()))?;
        let oracle = address!("420000000000000000000000000000000000000F");
        let call = |input: Vec<u8>| {
            self.call_at(
                TransactionRequest::default()
                    .with_to(oracle)
                    .with_input(input),
                BlockId::latest(),
            )
        };
        let (l1, operator) = tokio::try_join!(
            call(
                getL1FeeUpperBoundCall {
                    size: U256::from(size)
                }
                .abi_encode()
            ),
            call(
                getOperatorFeeCall {
                    gasUsed: U256::from(gas_limit)
                }
                .abi_encode()
            ),
        )?;
        let l1 = getL1FeeUpperBoundCall::abi_decode_returns(&l1)
            .map_err(|_| BotError::Rpc("invalid Ink L1 fee oracle response".into()))?;
        let operator = getOperatorFeeCall::abi_decode_returns(&operator)
            .map_err(|_| BotError::Rpc("invalid Ink operator fee oracle response".into()))?;
        l1.checked_add(operator)
            .and_then(|fee| fee.checked_mul(U256::from(2)))
            .ok_or_else(|| BotError::Transaction("Ink fee reserve overflowed".into()))
    }

    pub async fn connect_from_env() -> Result<Self> {
        Self::connect_from_env_with_profile("").await
    }

    pub async fn connect_from_env_for_chain(chain_id: u64) -> Result<Self> {
        let profile = match chain_id {
            crate::config::ROBINHOOD_MAINNET_CHAIN_ID => "ROBINHOOD_",
            crate::config::INK_MAINNET_CHAIN_ID => "INK_",
            crate::config::HYPEREVM_MAINNET_CHAIN_ID => "HYPEREVM_",
            _ => "",
        };
        Self::connect_from_env_with_profile(profile).await
    }

    async fn connect_from_env_with_profile(profile: &str) -> Result<Self> {
        let profile_http_name = format!("{profile}HTTP_RPC_URL");
        let profile_ws_name = format!("{profile}WS_RPC_URL");
        let use_profile = !profile.is_empty()
            && (optional_env(&profile_http_name).is_some()
                || optional_env(&profile_ws_name).is_some());
        let http_name = if use_profile {
            profile_http_name
        } else {
            "HTTP_RPC_URL".to_string()
        };
        let ws_name = if use_profile {
            profile_ws_name
        } else {
            "WS_RPC_URL".to_string()
        };
        let backup_name = if use_profile {
            format!("{profile}BACKUP_RPC_URL")
        } else {
            "BACKUP_RPC_URL".to_string()
        };
        let broadcast_name = if use_profile {
            format!("{profile}BROADCAST_RPC_URLS")
        } else {
            "BROADCAST_RPC_URLS".to_string()
        };
        let http_url = required_env(&http_name)?;
        let ws_url = required_env(&ws_name)?;
        let request_timeout = duration_from_env("RPC_TIMEOUT_MS", 5_000)?;
        let broadcast_timeout = duration_from_env("BROADCAST_TIMEOUT_MS", 3_000)?;
        let http = connect_http(&http_name, &http_url).await?;
        let ws_started = Instant::now();
        let ws = tokio::time::timeout(request_timeout, connect_ws(&ws_name, &ws_url))
            .await
            .map_err(|_| BotError::Rpc("WebSocket connection timed out".to_string()))??;
        let ws_connect_latency = ws_started.elapsed();

        let mut broadcast = vec![("primary".to_string(), http.clone())];
        let mut seen_urls = HashSet::from([validate_rpc_url(&http_name, &http_url, false)?]);
        if let Some(backup) = optional_env(&backup_name)
            && register_http_endpoint(&mut seen_urls, &backup_name, &backup)?
        {
            broadcast.push((
                "backup".to_string(),
                connect_http(&backup_name, &backup).await?,
            ));
        }
        if let Some(extra) = optional_env(&broadcast_name) {
            for (index, url) in extra
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .enumerate()
            {
                if !register_http_endpoint(&mut seen_urls, &broadcast_name, url)? {
                    continue;
                }
                broadcast.push((
                    format!("broadcast-{index}"),
                    connect_http(&broadcast_name, url).await?,
                ));
            }
        }
        let broadcast = Arc::new(broadcast);
        Ok(Self {
            http,
            ws,
            broadcast: Arc::clone(&broadcast),
            ws_connect_latency,
            read: broadcast,
            request_timeout,
            broadcast_timeout,
            ws_url,
        })
    }

    pub async fn reconnect_ws(&mut self, expected_chain_id: u64) -> Result<()> {
        self.ws =
            tokio::time::timeout(self.request_timeout, connect_ws("WS_RPC_URL", &self.ws_url))
                .await
                .map_err(|_| BotError::Rpc("WebSocket reconnection timed out".to_string()))??;
        let chain_id = tokio::time::timeout(self.request_timeout, self.ws.get_chain_id())
            .await
            .map_err(|_| BotError::Rpc("reconnected WebSocket chain check timed out".to_string()))?
            .map_err(|err| {
                BotError::Rpc(format!(
                    "reconnected WebSocket chain check failed: {}",
                    summarize_rpc_error(&err.to_string())
                ))
            })?;
        if chain_id != expected_chain_id {
            return Err(BotError::ChainMismatch {
                configured: expected_chain_id,
                reported: chain_id,
            });
        }
        self.replace_websocket_broadcast_endpoint();
        Ok(())
    }

    pub async fn validate_chain(&mut self, config: &MintConfig) -> Result<()> {
        let ws_chain_id = tokio::time::timeout(self.request_timeout, self.ws.get_chain_id())
            .await
            .map_err(|_| BotError::Rpc("WebSocket chain ID check timed out".to_string()))?
            .map_err(|err| {
                BotError::Rpc(format!(
                    "WebSocket chain ID check failed: {}",
                    summarize_rpc_error(&err.to_string())
                ))
            })?;
        if ws_chain_id != config.chain_id {
            return Err(BotError::ChainMismatch {
                configured: config.chain_id,
                reported: ws_chain_id,
            });
        }
        let mut healthy = Vec::new();
        for (name, provider) in self.broadcast.iter() {
            match tokio::time::timeout(self.request_timeout, provider.get_chain_id()).await {
                Ok(Ok(chain_id)) if chain_id == config.chain_id => {
                    healthy.push((name.clone(), provider.clone()));
                }
                Ok(Ok(chain_id)) => {
                    return Err(BotError::Config(format!(
                        "RPC provider `{name}` reports chain ID {chain_id}, expected {}",
                        config.chain_id
                    )));
                }
                Ok(Err(err)) => {
                    tracing::warn!(provider = %name, error = %summarize_rpc_error(&err.to_string()), "excluding unavailable RPC provider");
                }
                Err(_) => {
                    tracing::warn!(provider = %name, "excluding RPC provider after health-check timeout");
                }
            }
        }
        if healthy.is_empty() {
            return Err(BotError::Rpc(
                "no configured HTTP RPC provider passed the startup health check".to_string(),
            ));
        }
        self.http = healthy[0].1.clone();
        let healthy = Arc::new(healthy);
        let mut broadcast = Vec::with_capacity(healthy.len().saturating_add(1));
        // The already-connected WebSocket can often accept the raw
        // transaction sooner than a newly-idle HTTP connection. Keep the HTTP
        // endpoints as concurrent fallbacks.
        broadcast.push(("ws".to_string(), self.ws.clone()));
        broadcast.extend(healthy.iter().cloned());
        self.broadcast = Arc::new(broadcast);
        self.read = healthy;
        Ok(())
    }

    fn replace_websocket_broadcast_endpoint(&mut self) {
        let mut broadcast = self.broadcast.as_ref().clone();
        if let Some((_, provider)) = broadcast.iter_mut().find(|(name, _)| name == "ws") {
            *provider = self.ws.clone();
        } else {
            broadcast.insert(0, ("ws".to_string(), self.ws.clone()));
        }
        self.broadcast = Arc::new(broadcast);
    }

    pub async fn validate_contract(&self, config: &MintConfig) -> Result<B256> {
        let contract = config.contract()?;
        let codes = self
            .read_all("eth_getCode", move |provider| async move {
                provider.get_code_at(contract).await
            })
            .await?;
        if codes.iter().all(|code| code.is_empty()) {
            return Err(BotError::MissingContract { address: contract });
        }
        if codes.iter().any(|code| code.is_empty()) {
            return Err(BotError::Rpc(
                "RPC providers disagree about whether the configured contract is deployed"
                    .to_string(),
            ));
        }
        let expected_hash = keccak256(&codes[0]);
        if codes
            .iter()
            .skip(1)
            .any(|code| keccak256(code) != expected_hash)
        {
            return Err(BotError::Rpc(
                "RPC providers returned different bytecode for the configured contract".to_string(),
            ));
        }
        if let Some(pinned_hash) = config.expected_contract_code_hash_value()?
            && expected_hash != pinned_hash
        {
            return Err(BotError::Config(format!(
                "contract bytecode hash mismatch: configured {pinned_hash}, RPC returned {expected_hash}"
            )));
        }
        Ok(expected_hash)
    }

    pub async fn subscribe_blocks(&self) -> Result<SubscriptionStream<Header>> {
        tokio::time::timeout(self.request_timeout, self.ws.subscribe_blocks())
            .await
            .map_err(|_| BotError::Rpc("block subscription timed out".to_string()))?
            .map(|subscription| subscription.into_stream())
            .map_err(|err| {
                BotError::Rpc(format!(
                    "block subscription failed: {}",
                    summarize_rpc_error(&err.to_string())
                ))
            })
    }

    pub async fn subscribe_logs(&self, filter: &Filter) -> Result<SubscriptionStream<Log>> {
        tokio::time::timeout(self.request_timeout, self.ws.subscribe_logs(filter))
            .await
            .map_err(|_| BotError::Rpc("log subscription timed out".to_string()))?
            .map(|subscription| subscription.into_stream())
            .map_err(|err| {
                BotError::Rpc(format!(
                    "log subscription failed: {}",
                    summarize_rpc_error(&err.to_string())
                ))
            })
    }

    pub async fn preload_nonce(&self, address: Address) -> Result<u64> {
        self.read_critical("eth_getTransactionCount", move |provider| async move {
            provider.get_transaction_count(address).pending().await
        })
        .await?
        .into_iter()
        .max()
        .ok_or_else(|| BotError::Rpc("no provider returned a pending nonce".to_string()))
    }

    pub async fn check_balance(&self, address: Address) -> Result<U256> {
        self.read_critical("eth_getBalance", move |provider| async move {
            provider.get_balance(address).await
        })
        .await?
        .into_iter()
        .min()
        .ok_or_else(|| BotError::Rpc("no provider returned a wallet balance".to_string()))
    }

    pub async fn estimate_gas(&self, request: TransactionRequest) -> Result<u64> {
        self.read_critical_with_window(
            "eth_estimateGas",
            move |provider| {
                let request = request.clone();
                async move { provider.estimate_gas(request).await }
            },
            GAS_ESTIMATE_SETTLE_WINDOW,
        )
        .await?
        .into_iter()
        .max()
        .ok_or_else(|| BotError::Rpc("no provider returned a gas estimate".to_string()))
    }

    pub async fn estimate_eip1559_fees(&self) -> Result<Eip1559Estimation> {
        let estimates = self
            .read_critical("eth_feeHistory", move |provider| async move {
                provider.estimate_eip1559_fees().await
            })
            .await?;
        estimates
            .into_iter()
            .reduce(|left, right| Eip1559Estimation {
                max_fee_per_gas: left.max_fee_per_gas.max(right.max_fee_per_gas),
                max_priority_fee_per_gas: left
                    .max_priority_fee_per_gas
                    .max(right.max_priority_fee_per_gas),
            })
            .ok_or_else(|| {
                BotError::Rpc("no provider returned an EIP-1559 fee estimate".to_string())
            })
    }

    pub async fn call_at(&self, request: TransactionRequest, block: BlockId) -> Result<Vec<u8>> {
        let values = self
            .read_critical("eth_call", move |provider| {
                let request = request.clone();
                async move { provider.call(request).block(block).await }
            })
            .await?;
        let Some(first) = values.first() else {
            return Err(BotError::Rpc(
                "no provider returned a view-call result".to_string(),
            ));
        };
        if values.iter().skip(1).any(|value| value != first) {
            return Err(BotError::Rpc(
                "RPC providers returned conflicting view-call results".to_string(),
            ));
        }
        Ok(first.clone().into())
    }

    pub async fn block_number(&self) -> Result<u64> {
        self.read_critical("eth_blockNumber", move |provider| async move {
            provider.get_block_number().await
        })
        .await?
        .into_iter()
        .max()
        .ok_or_else(|| BotError::Rpc("no provider returned a block number".to_string()))
    }

    pub async fn transaction_receipt(&self, hash: B256) -> Result<Option<TransactionReceipt>> {
        let receipts = self
            .read_all("eth_getTransactionReceipt", move |provider| async move {
                provider.get_transaction_receipt(hash).await
            })
            .await?;
        let mut present = receipts.into_iter().flatten();
        let Some(first) = present.next() else {
            return Ok(None);
        };
        if present.any(|receipt| receipt != first) {
            return Err(BotError::Rpc(
                "RPC providers returned conflicting receipts; waiting for a consistent result"
                    .to_string(),
            ));
        }
        Ok(Some(first))
    }

    pub async fn logs(&self, filter: Filter) -> Result<Vec<Log>> {
        let batches = self
            .read_all("eth_getLogs", move |provider| {
                let filter = filter.clone();
                async move { provider.get_logs(&filter).await }
            })
            .await?;
        let mut merged = Vec::new();
        for batch in batches {
            for log in batch {
                if !merged.iter().any(|known: &Log| {
                    known.block_hash == log.block_hash
                        && known.transaction_hash == log.transaction_hash
                        && known.log_index == log.log_index
                }) {
                    merged.push(log);
                }
            }
        }
        Ok(merged)
    }

    async fn read_all<T, E, F, Fut>(&self, operation: &str, call: F) -> Result<Vec<T>>
    where
        F: Fn(DynProvider<Ethereum>) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, E>>,
        E: std::fmt::Display,
    {
        let mut calls = FuturesUnordered::new();
        for (name, provider) in self.read.iter().cloned() {
            let future = call(provider);
            let timeout = self.request_timeout;
            calls.push(async move {
                let result = tokio::time::timeout(timeout, future).await;
                (name, result)
            });
        }

        let mut values = Vec::new();
        let mut failures = Vec::new();
        while let Some((name, result)) = calls.next().await {
            match result {
                Ok(Ok(value)) => values.push(value),
                Ok(Err(err)) => {
                    failures.push(format!("{name}: {}", summarize_rpc_error(&err.to_string())))
                }
                Err(_) => failures.push(format!("{name}: timed out")),
            }
        }
        if values.is_empty() {
            return Err(BotError::Rpc(format!(
                "{operation} failed on every healthy provider: {}",
                failures.join("; ")
            )));
        }
        Ok(values)
    }

    /// Fan out a trigger-critical read, then keep only a short consistency
    /// window open after the first successful response. This retains a chance
    /// to compare/aggregate fast providers without putting a slow backup RPC
    /// on the transaction's critical path.
    async fn read_critical<T, E, F, Fut>(&self, operation: &str, call: F) -> Result<Vec<T>>
    where
        F: Fn(DynProvider<Ethereum>) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, E>>,
        E: std::fmt::Display,
    {
        self.read_critical_with_window(operation, call, CRITICAL_READ_SETTLE_WINDOW)
            .await
    }

    async fn read_critical_with_window<T, E, F, Fut>(
        &self,
        operation: &str,
        call: F,
        settle_window: Duration,
    ) -> Result<Vec<T>>
    where
        F: Fn(DynProvider<Ethereum>) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, E>>,
        E: std::fmt::Display,
    {
        let mut calls = FuturesUnordered::new();
        for (name, provider) in self.read.iter().cloned() {
            let future = call(provider);
            let timeout = self.request_timeout;
            calls.push(async move {
                let result = tokio::time::timeout(timeout, future).await;
                (name, result)
            });
        }

        let mut values = Vec::new();
        let mut failures = Vec::new();
        while let Some((name, result)) = calls.next().await {
            match result {
                Ok(Ok(value)) => {
                    values.push(value);
                    break;
                }
                Ok(Err(err)) => {
                    failures.push(format!("{name}: {}", summarize_rpc_error(&err.to_string())))
                }
                Err(_) => failures.push(format!("{name}: timed out")),
            }
        }
        if values.is_empty() {
            return Err(BotError::Rpc(format!(
                "{operation} failed on every healthy provider: {}",
                failures.join("; ")
            )));
        }

        let deadline = tokio::time::Instant::now() + settle_window;
        loop {
            match tokio::time::timeout_at(deadline, calls.next()).await {
                Ok(Some((_name, Ok(Ok(value))))) => values.push(value),
                Ok(Some((_name, _))) => {}
                Ok(None) | Err(_) => break,
            }
        }
        Ok(values)
    }

    pub async fn benchmark_endpoint(
        &self,
        name: &str,
        provider: &DynProvider<Ethereum>,
    ) -> EndpointBenchmark {
        let chain_id = measure_samples(self.request_timeout, || provider.get_chain_id()).await;
        let block_number =
            measure_samples(self.request_timeout, || provider.get_block_number()).await;
        let balance = measure_samples(self.request_timeout, || async {
            provider
                .get_balance(alloy::primitives::Address::ZERO)
                .latest()
                .await
        })
        .await;
        EndpointBenchmark {
            name: name.to_string(),
            chain_id,
            block_number,
            balance,
        }
    }

    pub async fn benchmark_ws_subscription(&self) -> LatencySummary {
        measure_samples(self.request_timeout, || async {
            self.ws.subscribe_blocks().await
        })
        .await
    }

    pub async fn broadcast_raw(&self, raw: Vec<u8>) -> Result<(alloy::primitives::B256, Duration)> {
        let started = Instant::now();
        let expected_hash = keccak256(&raw);
        let mut tasks = FuturesUnordered::new();
        let mut ambiguous = false;
        for (name, provider) in self.broadcast.iter().cloned() {
            let raw = raw.clone();
            let timeout = self.broadcast_timeout;
            tasks.push(tokio::spawn(async move {
                let result =
                    tokio::time::timeout(timeout, provider.send_raw_transaction(&raw)).await;
                (name, result)
            }));
        }

        while let Some(joined) = tasks.next().await {
            let (name, result) = joined
                .map_err(|err| BotError::Transaction(format!("broadcast task failed: {err}")))?;
            match result {
                Ok(Ok(pending)) => {
                    let hash = *pending.tx_hash();
                    if hash != expected_hash {
                        ambiguous = true;
                        tracing::warn!(provider = %name, returned_hash = %hash, expected_hash = %expected_hash, "RPC returned a transaction hash that does not match the signed bytes");
                        continue;
                    }
                    let elapsed = started.elapsed();
                    tracing::info!(provider = %name, tx_hash = %hash, elapsed_ms = elapsed.as_secs_f64() * 1000.0, "RPC accepted raw transaction");
                    tokio::spawn(async move { while tasks.next().await.is_some() {} });
                    return Ok((hash, elapsed));
                }
                Ok(Err(err)) if is_known_transaction(&err.to_string()) => {
                    let elapsed = started.elapsed();
                    tracing::info!(provider = %name, tx_hash = %expected_hash, elapsed_ms = elapsed.as_secs_f64() * 1000.0, "RPC reports raw transaction already known");
                    tokio::spawn(async move { while tasks.next().await.is_some() {} });
                    return Ok((expected_hash, elapsed));
                }
                Ok(Err(err)) => {
                    tracing::warn!(provider = %name, error = %summarize_rpc_error(&err.to_string()), "broadcast endpoint rejected transaction");
                }
                Err(_) => {
                    ambiguous = true;
                    tracing::warn!(provider = %name, "broadcast endpoint timed out")
                }
            }
        }
        if ambiguous {
            return Err(BotError::BroadcastOutcomeUnknown {
                hash: expected_hash,
            });
        }
        Err(BotError::Transaction(
            "all configured broadcast endpoints rejected the raw transaction".to_string(),
        ))
    }
}

pub struct EndpointBenchmark {
    pub name: String,
    pub chain_id: LatencySummary,
    pub block_number: LatencySummary,
    pub balance: LatencySummary,
}

#[derive(Debug, Clone, Copy)]
pub struct LatencySummary {
    pub min: Duration,
    pub mean: Duration,
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub successful: usize,
    pub failed: usize,
}

async fn connect_http(name: &str, url: &str) -> Result<DynProvider<Ethereum>> {
    let parsed = validate_rpc_url(name, url, false)?;
    Ok(ProviderBuilder::new().connect_http(parsed).erased())
}

fn register_http_endpoint(seen: &mut HashSet<Url>, name: &str, url: &str) -> Result<bool> {
    // Compare the complete normalized URL, including path and query. Distinct
    // provider routes/credentials on the same host remain independent endpoints.
    Ok(seen.insert(validate_rpc_url(name, url, false)?))
}

async fn connect_ws(name: &str, url: &str) -> Result<DynProvider<Ethereum>> {
    let parsed = validate_rpc_url(name, url, true)?;
    ProviderBuilder::new()
        .connect_ws(WsConnect::new(parsed.to_string()).with_max_retries(10))
        .await
        .map(|provider| provider.erased())
        .map_err(|_| {
            BotError::Rpc(format!(
                "WebSocket connection failed; verify {name} and its credentials"
            ))
        })
}

fn validate_rpc_url(name: &str, value: &str, websocket: bool) -> Result<Url> {
    let parsed = value
        .parse::<Url>()
        .map_err(|_| BotError::Config(format!("{name} is not a valid URL")))?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(BotError::Config(format!(
            "{name} must not embed username/password credentials in the URL"
        )));
    }
    let loopback = parsed
        .host_str()
        .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
    let secure = if websocket {
        parsed.scheme() == "wss" || (parsed.scheme() == "ws" && loopback)
    } else {
        parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback)
    };
    if !secure {
        let required = if websocket { "wss://" } else { "https://" };
        return Err(BotError::Config(format!(
            "{name} must use {required}; insecure transport is allowed only for loopback development nodes"
        )));
    }
    Ok(parsed)
}

fn required_env(name: &str) -> Result<String> {
    env::var(name).map_err(|_| BotError::Config(format!("{name} is not set")))
}

fn optional_env(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn duration_from_env(name: &str, default_ms: u64) -> Result<Duration> {
    let milliseconds = match optional_env(name) {
        Some(value) => value.parse::<u64>().map_err(|err| {
            BotError::Config(format!(
                "{name} must be an integer number of milliseconds: {err}"
            ))
        })?,
        None => default_ms,
    };
    if milliseconds == 0 {
        return Err(BotError::Config(format!(
            "{name} must be greater than zero"
        )));
    }
    Ok(Duration::from_millis(milliseconds))
}

fn is_known_transaction(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "already known",
        "known transaction",
        "already imported",
        "tx already exists",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

async fn measure_samples<F, Fut, T, E>(timeout: Duration, mut call: F) -> LatencySummary
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<T, E>>,
{
    let mut samples = Vec::with_capacity(10);
    let mut failed = 0;
    for _ in 0..10 {
        let started = Instant::now();
        if matches!(tokio::time::timeout(timeout, call()).await, Ok(Ok(_))) {
            samples.push(started.elapsed());
        } else {
            failed += 1;
        }
    }
    if samples.is_empty() {
        return LatencySummary {
            min: Duration::ZERO,
            mean: Duration::ZERO,
            p50: Duration::ZERO,
            p95: Duration::ZERO,
            p99: Duration::ZERO,
            max: Duration::ZERO,
            successful: 0,
            failed,
        };
    }
    samples.sort_unstable();
    let nanos: Vec<u128> = samples.iter().map(Duration::as_nanos).collect();
    let mean = nanos.iter().sum::<u128>() / nanos.len() as u128;
    let duration = |value: u128| Duration::from_nanos(value.min(u64::MAX as u128) as u64);
    let percentile = |percent: usize| nanos[((nanos.len() - 1) * percent) / 100];
    LatencySummary {
        min: samples[0],
        mean: duration(mean),
        p50: duration(percentile(50)),
        p95: duration(percentile(95)),
        p99: duration(percentile(99)),
        max: samples[samples.len() - 1],
        successful: samples.len(),
        failed,
    }
}

pub async fn simulate_call(rpc: &RpcClients, tx: TransactionRequest) -> Result<()> {
    rpc.call_at(tx, BlockId::latest())
        .await
        .map(|_| ())
        .map_err(|err| BotError::Transaction(format!("eth_call simulation failed: {err}")))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) async fn mock_rpc(
        response: fn(serde_json::Value) -> serde_json::Value,
    ) -> (RpcClients, tokio::task::JoinHandle<()>) {
        mock_rpc_async(move |request| std::future::ready(response(request))).await
    }

    pub(crate) async fn mock_rpc_async<F, Fut>(
        response: F,
    ) -> (RpcClients, tokio::task::JoinHandle<()>)
    where
        F: Fn(serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = serde_json::Value> + Send + 'static,
    {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let response = Arc::new(response);
        let task = tokio::spawn(async move {
            let mut handlers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    joined = handlers.join_next(), if !handlers.is_empty() => {
                        joined.unwrap().unwrap();
                    }
                    socket = listener.accept() => {
                        let (socket, _) = socket.unwrap();
                        let response = response.clone();
                        handlers.spawn(async move {
                            let mut reader = BufReader::new(socket);
                            let mut length = 0;
                            loop {
                                let mut line = String::new();
                                assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                                if line == "\r\n" {
                                    break;
                                }
                                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                                    length = value.trim().parse::<usize>().unwrap();
                                }
                            }
                            let mut body = vec![0; length];
                            reader.read_exact(&mut body).await.unwrap();
                            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
                            let result = response(request.clone()).await;
                            let body = serde_json::json!({"jsonrpc":"2.0", "id":request["id"], "result":result}).to_string();
                            // Cancellation is expected in racing/failed-read tests.
                            let _ = reader.get_mut().write_all(format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                            ).as_bytes()).await;
                        });
                    }
                }
            }
        });
        let provider = connect_http("test", &url).await.unwrap();
        let endpoints = Arc::new(vec![("test".into(), provider.clone())]);
        (
            RpcClients {
                http: provider.clone(),
                ws: provider,
                broadcast: endpoints.clone(),
                read: endpoints,
                ws_connect_latency: Duration::ZERO,
                request_timeout: Duration::from_secs(1),
                broadcast_timeout: Duration::from_secs(1),
                ws_url: String::new(),
            },
            task,
        )
    }

    #[tokio::test]
    async fn ink_fee_reserve_includes_both_oracle_components() {
        let (rpc, server) = mock_rpc(|request| {
            assert_eq!(request["method"], "eth_call");
            let tx = &request["params"][0];
            let input = tx
                .get("input")
                .or_else(|| tx.get("data"))
                .unwrap()
                .as_str()
                .unwrap();
            let (expected_argument, amount) = if input.starts_with("0xf1c7a58b") {
                (356, 100_u64)
            } else {
                assert!(input.starts_with("0x275aedd2"));
                (200_000, 20_u64)
            };
            assert_eq!(
                U256::from_str_radix(&input[10..], 16).unwrap(),
                U256::from(expected_argument)
            );
            serde_json::json!(format!("0x{amount:064x}"))
        })
        .await;
        assert_eq!(
            rpc.ink_extra_fee_reserve(100, 200_000).await.unwrap(),
            U256::from(240)
        );
        server.abort();
    }

    #[tokio::test]
    async fn ink_fee_reserve_fails_closed_on_missing_oracle() {
        let (rpc, server) = mock_rpc(|_| serde_json::json!("0x")).await;
        assert!(rpc.ink_extra_fee_reserve(100, 200_000).await.is_err());
        server.abort();
    }

    #[test]
    fn duplicate_rpc_urls_are_removed_without_collapsing_distinct_routes() {
        let mut seen = HashSet::new();
        assert!(register_http_endpoint(&mut seen, "test", "https://rpc.example").unwrap());
        assert!(!register_http_endpoint(&mut seen, "test", "https://rpc.example/").unwrap());
        assert!(register_http_endpoint(&mut seen, "test", "https://rpc.example/route-a").unwrap());
        assert!(register_http_endpoint(&mut seen, "test", "https://rpc.example/route-b").unwrap());
        assert!(register_http_endpoint(&mut seen, "test", "https://rpc.example/?route=a").unwrap());
        assert!(register_http_endpoint(&mut seen, "test", "https://rpc.example/?route=b").unwrap());
        assert!(register_http_endpoint(&mut seen, "test", "http://rpc.example").is_err());
    }

    #[test]
    fn recognizes_known_transaction_responses() {
        assert!(is_known_transaction("already known"));
        assert!(is_known_transaction("Known transaction: 0xabc"));
        assert!(!is_known_transaction("nonce too low"));
    }

    #[test]
    fn requires_encrypted_rpc_transport_except_on_loopback() {
        assert!(validate_rpc_url("HTTP_RPC_URL", "https://rpc.example", false).is_ok());
        assert!(validate_rpc_url("WS_RPC_URL", "wss://rpc.example", true).is_ok());
        assert!(validate_rpc_url("HTTP_RPC_URL", "http://rpc.example", false).is_err());
        assert!(validate_rpc_url("WS_RPC_URL", "ws://rpc.example", true).is_err());
        assert!(validate_rpc_url("HTTP_RPC_URL", "http://127.0.0.1:8545", false).is_ok());
        assert!(validate_rpc_url("WS_RPC_URL", "ws://localhost:8545", true).is_ok());
    }

    #[test]
    fn rejects_rpc_urls_with_userinfo_credentials() {
        assert!(
            validate_rpc_url(
                "HTTP_RPC_URL",
                "https://username:password@rpc.example",
                false
            )
            .is_err()
        );
    }
}
