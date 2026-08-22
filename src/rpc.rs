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
    env,
    sync::Arc,
    time::{Duration, Instant},
};

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
    pub async fn connect_from_env() -> Result<Self> {
        let http_url = required_env("HTTP_RPC_URL")?;
        let ws_url = required_env("WS_RPC_URL")?;
        let request_timeout = duration_from_env("RPC_TIMEOUT_MS", 5_000)?;
        let broadcast_timeout = duration_from_env("BROADCAST_TIMEOUT_MS", 3_000)?;
        let http = connect_http("HTTP_RPC_URL", &http_url).await?;
        let ws_started = Instant::now();
        let ws = tokio::time::timeout(request_timeout, connect_ws(&ws_url))
            .await
            .map_err(|_| BotError::Rpc("WebSocket connection timed out".to_string()))??;
        let ws_connect_latency = ws_started.elapsed();

        let mut broadcast = vec![("primary".to_string(), http.clone())];
        if let Some(backup) = optional_env("BACKUP_RPC_URL") {
            broadcast.push((
                "backup".to_string(),
                connect_http("BACKUP_RPC_URL", &backup).await?,
            ));
        }
        if let Some(extra) = optional_env("BROADCAST_RPC_URLS") {
            for (index, url) in extra
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .enumerate()
            {
                broadcast.push((
                    format!("broadcast-{index}"),
                    connect_http("BROADCAST_RPC_URLS", url).await?,
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
        self.ws = tokio::time::timeout(self.request_timeout, connect_ws(&self.ws_url))
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
        self.broadcast = Arc::clone(&healthy);
        self.read = healthy;
        Ok(())
    }

    pub async fn validate_contract(&self, config: &MintConfig) -> Result<()> {
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
        Ok(())
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
        self.read_all("eth_getTransactionCount", move |provider| async move {
            provider.get_transaction_count(address).pending().await
        })
        .await?
        .into_iter()
        .max()
        .ok_or_else(|| BotError::Rpc("no provider returned a pending nonce".to_string()))
    }

    pub async fn check_balance(&self, address: Address) -> Result<U256> {
        self.read_all("eth_getBalance", move |provider| async move {
            provider.get_balance(address).await
        })
        .await?
        .into_iter()
        .min()
        .ok_or_else(|| BotError::Rpc("no provider returned a wallet balance".to_string()))
    }

    pub async fn estimate_gas(&self, request: TransactionRequest) -> Result<u64> {
        self.read_all("eth_estimateGas", move |provider| {
            let request = request.clone();
            async move { provider.estimate_gas(request).await }
        })
        .await?
        .into_iter()
        .max()
        .ok_or_else(|| BotError::Rpc("no provider returned a gas estimate".to_string()))
    }

    pub async fn estimate_eip1559_fees(&self) -> Result<Eip1559Estimation> {
        let estimates = self
            .read_all("eth_feeHistory", move |provider| async move {
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
        self.read_fallback("eth_call", move |provider| {
            let request = request.clone();
            async move { provider.call(request).block(block).await }
        })
        .await
        .map(Into::into)
    }

    pub async fn block_number(&self) -> Result<u64> {
        self.read_all("eth_blockNumber", move |provider| async move {
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
        Ok(receipts.into_iter().flatten().next())
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

    async fn read_fallback<T, E, F, Fut>(&self, operation: &str, call: F) -> Result<T>
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

        let mut failures = Vec::new();
        while let Some((name, result)) = calls.next().await {
            match result {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(err)) => {
                    failures.push(format!("{name}: {}", summarize_rpc_error(&err.to_string())))
                }
                Err(_) => failures.push(format!("{name}: timed out")),
            }
        }
        Err(BotError::Rpc(format!(
            "{operation} failed on every healthy provider: {}",
            failures.join("; ")
        )))
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
                Err(_) => tracing::warn!(provider = %name, "broadcast endpoint timed out"),
            }
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

async fn connect_ws(url: &str) -> Result<DynProvider<Ethereum>> {
    let parsed = validate_rpc_url("WS_RPC_URL", url, true)?;
    ProviderBuilder::new()
        .connect_ws(WsConnect::new(parsed.to_string()).with_max_retries(10))
        .await
        .map(|provider| provider.erased())
        .map_err(|_| {
            BotError::Rpc(
                "WebSocket connection failed; verify WS_RPC_URL and its credentials".to_string(),
            )
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
mod tests {
    use super::*;

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
