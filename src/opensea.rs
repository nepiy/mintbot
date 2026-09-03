use crate::{
    error::{BotError, Result},
    security::sanitize_external_text,
};
use alloy::{
    primitives::{Address, U256, address},
    sol_types::SolInterface,
};
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use zeroize::Zeroizing;

const OPENSEA_API_BASE: &str = "https://api.opensea.io/api/v2";
const MAX_RESPONSE_BYTES: u64 = 1_048_576;
const MAX_CALLDATA_BYTES: usize = 262_144;

/// Canonical OpenSea SeaDrop deployment used as the transaction target for
/// OpenSea Drops. The configured collection address remains the NFT contract;
/// SeaDrop is authorized by that contract to perform the stage-aware mint.
pub const OPENSEA_SEADROP_ADDRESS: Address = address!("0x00005EA00Ac477B1030CE78506496e8C2dE24bf5");

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    struct SeaDropMintParams {
        uint256 mintPrice;
        uint256 maxTotalMintableByWallet;
        uint256 startTime;
        uint256 endTime;
        uint256 dropStageIndex;
        uint256 maxTokenSupplyForStage;
        uint256 feeBps;
        bool restrictFeeRecipients;
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SeaDropTokenGatedMintParams {
        address allowedNftToken;
        uint256[] allowedNftTokenIds;
    }

    #[derive(Debug, PartialEq, Eq)]
    interface SeaDropMint {
        function mintPublic(
            address nftContract,
            address feeRecipient,
            address minterIfNotPayer,
            uint256 quantity
        ) external payable;

        function mintAllowList(
            address nftContract,
            address feeRecipient,
            address minterIfNotPayer,
            uint256 quantity,
            SeaDropMintParams mintParams,
            bytes32[] proof
        ) external payable;

        function mintSigned(
            address nftContract,
            address feeRecipient,
            address minterIfNotPayer,
            uint256 quantity,
            SeaDropMintParams mintParams,
            uint256 salt,
            bytes signature
        ) external payable;

        function mintAllowedTokenHolder(
            address nftContract,
            address feeRecipient,
            address minterIfNotPayer,
            SeaDropTokenGatedMintParams mintParams
        ) external payable;
    }
}

#[derive(Debug, Clone)]
pub struct OpenSeaMintTransaction {
    pub target: Address,
    pub calldata: Vec<u8>,
    pub value: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSeaStage {
    pub label: String,
    pub start_time: u64,
    pub end_time: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenSeaDrop {
    pub stages: Vec<OpenSeaStage>,
    pub total_supply: Option<U256>,
    pub max_supply: Option<U256>,
}

impl OpenSeaDrop {
    pub fn remaining_supply(&self) -> Option<U256> {
        let total = self.total_supply?;
        let maximum = self.max_supply?;
        Some(if total >= maximum {
            U256::ZERO
        } else {
            maximum - total
        })
    }
}

#[derive(Clone)]
pub struct OpenSeaClient {
    client: Client,
    api_key: Zeroizing<String>,
}

#[derive(Debug, Serialize)]
struct BuildMintRequest {
    minter: String,
    quantity: u64,
}

#[derive(Debug, Deserialize)]
struct BuildMintResponse {
    #[serde(alias = "to")]
    target: String,
    #[serde(alias = "data")]
    calldata: String,
    value: String,
}

impl OpenSeaClient {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("OPENSEA_API_KEY").map_err(|_| {
            BotError::Config(
                "OPENSEA_API_KEY is required when opensea_drop_slug is configured".to_string(),
            )
        })?;
        if api_key.trim().is_empty() {
            return Err(BotError::Config(
                "OPENSEA_API_KEY must not be empty".to_string(),
            ));
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|err| {
                BotError::Transaction(format!("could not create OpenSea client: {err}"))
            })?;
        Ok(Self {
            client,
            api_key: Zeroizing::new(api_key),
        })
    }

    pub async fn build_mint(
        &self,
        drop_slug: &str,
        minter: Address,
        quantity: u64,
    ) -> Result<OpenSeaMintTransaction> {
        let url = format!("{OPENSEA_API_BASE}/drops/{drop_slug}/mint");
        let body = BuildMintRequest {
            minter: minter.to_string(),
            quantity,
        };
        let response = self
            .client
            .post(url)
            .header("X-API-KEY", self.api_key.as_str())
            .json(&body)
            .send()
            .await
            .map_err(|_| BotError::OpenSeaTransport)?;
        let status = response.status();
        if !status.is_success() {
            let message = classify_rejection(status.as_u16(), response).await;
            return Err(BotError::OpenSeaApi {
                status: status.as_u16(),
                message,
            });
        }
        let response: BuildMintResponse =
            read_json_response(response, "OpenSea API returned invalid transaction data").await?;
        let target = response.target.parse().map_err(|_| {
            BotError::Transaction("OpenSea returned an invalid target address".to_string())
        })?;
        let calldata = decode_hex(&response.calldata, "calldata")?;
        let value = parse_u256(&response.value)?;
        Ok(OpenSeaMintTransaction {
            target,
            calldata,
            value,
        })
    }

    pub async fn build_mint_with_retry(
        &self,
        drop_slug: &str,
        minter: Address,
        quantity: u64,
    ) -> Result<OpenSeaMintTransaction> {
        const MAX_ATTEMPTS: usize = 4;
        let mut delay = Duration::from_millis(100);
        for attempt in 0..MAX_ATTEMPTS {
            match self.build_mint(drop_slug, minter, quantity).await {
                Ok(mint) => return Ok(mint),
                Err(error) if attempt + 1 < MAX_ATTEMPTS && is_retryable_mint_error(&error) => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(800));
                }
                Err(error) => return Err(error),
            }
        }
        Err(BotError::Transaction(
            "OpenSea retry loop exhausted without a response".to_string(),
        ))
    }

    pub async fn get_drop(&self, drop_slug: &str) -> Result<OpenSeaDrop> {
        let url = format!("{OPENSEA_API_BASE}/drops/{drop_slug}");
        let response = self
            .client
            .get(url)
            .header("X-API-KEY", self.api_key.as_str())
            .send()
            .await
            .map_err(|_| BotError::OpenSeaTransport)?;
        let status = response.status();
        if !status.is_success() {
            let message = classify_rejection(status.as_u16(), response).await;
            return Err(BotError::OpenSeaApi {
                status: status.as_u16(),
                message,
            });
        }
        let body: Value =
            read_json_response(response, "OpenSea returned invalid drop details").await?;
        parse_drop_details(&body)
    }

    pub async fn get_stages(&self, drop_slug: &str) -> Result<Vec<OpenSeaStage>> {
        Ok(self.get_drop(drop_slug).await?.stages)
    }

    pub async fn verify_collection_contract(
        &self,
        slug: &str,
        chain_id: u64,
        address: Address,
    ) -> Result<()> {
        let chain = opensea_chain_slug(chain_id)?;
        let url = format!("{OPENSEA_API_BASE}/chain/{chain}/contract/{address:#x}");
        let response = self
            .client
            .get(url)
            .header("X-API-KEY", self.api_key.as_str())
            .send()
            .await
            .map_err(|_| BotError::OpenSeaTransport)?;
        let status = response.status();
        if !status.is_success() {
            let message = classify_rejection(status.as_u16(), response).await;
            return Err(BotError::OpenSeaApi {
                status: status.as_u16(),
                message,
            });
        }
        let body: Value = read_json_response(
            response,
            "OpenSea returned invalid chain-specific contract details",
        )
        .await?;
        let returned_slug = collection_slug_from_contract_response(&body).ok_or_else(|| {
            BotError::Transaction(
                "OpenSea contract details did not identify an associated collection".to_string(),
            )
        })?;
        if returned_slug != slug {
            return Err(BotError::Config(format!(
                "contract {address:#x} belongs to OpenSea collection `{returned_slug}` on {chain}, not `{slug}`"
            )));
        }
        Ok(())
    }
}

fn parse_drop_details(body: &Value) -> Result<OpenSeaDrop> {
    let drop = body.get("drop").unwrap_or(body);
    let stages = body
        .get("stages")
        .or_else(|| body.get("drop").and_then(|drop| drop.get("stages")))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            BotError::Transaction("OpenSea drop details did not include a stages array".to_string())
        })?;
    let mut parsed = stages
        .iter()
        .enumerate()
        .map(|(index, stage)| parse_stage(stage, index))
        .collect::<Result<Vec<_>>>()?;
    parsed.sort_by_key(|stage| stage.start_time);
    if parsed.is_empty() {
        return Err(BotError::Transaction(
            "OpenSea drop details contained no usable mint stages".to_string(),
        ));
    }
    Ok(OpenSeaDrop {
        stages: parsed,
        total_supply: parse_supply(drop, "totalSupply", "total_supply"),
        max_supply: parse_supply(drop, "maxSupply", "max_supply"),
    })
}

fn parse_supply(body: &Value, camel_case: &str, snake_case: &str) -> Option<U256> {
    let value = body.get(camel_case).or_else(|| body.get(snake_case))?;
    value
        .as_str()
        .and_then(|value| parse_u256(value).ok())
        .or_else(|| value.as_u64().map(U256::from))
}

pub fn validate_seadrop_calldata(
    calldata: &[u8],
    expected_contract: Address,
    expected_minter: Address,
    expected_quantity: u64,
    expected_value: U256,
) -> Result<()> {
    let decoded = SeaDropMint::SeaDropMintCalls::abi_decode(calldata).map_err(|_| {
        BotError::Transaction(
            "OpenSea returned unsupported or malformed SeaDrop calldata".to_string(),
        )
    })?;

    let (nft_contract, minter_if_not_payer, quantity, embedded_price) = match decoded {
        SeaDropMint::SeaDropMintCalls::mintPublic(call) => {
            (call.nftContract, call.minterIfNotPayer, call.quantity, None)
        }
        SeaDropMint::SeaDropMintCalls::mintAllowList(call) => (
            call.nftContract,
            call.minterIfNotPayer,
            call.quantity,
            Some(call.mintParams.mintPrice),
        ),
        SeaDropMint::SeaDropMintCalls::mintSigned(call) => (
            call.nftContract,
            call.minterIfNotPayer,
            call.quantity,
            Some(call.mintParams.mintPrice),
        ),
        SeaDropMint::SeaDropMintCalls::mintAllowedTokenHolder(call) => (
            call.nftContract,
            call.minterIfNotPayer,
            U256::from(call.mintParams.allowedNftTokenIds.len()),
            None,
        ),
    };

    if nft_contract != expected_contract {
        return Err(BotError::Transaction(format!(
            "OpenSea SeaDrop calldata targets NFT contract {nft_contract}, expected {expected_contract}"
        )));
    }
    if !minter_if_not_payer.is_zero() && minter_if_not_payer != expected_minter {
        return Err(BotError::Transaction(format!(
            "OpenSea SeaDrop calldata would mint to {minter_if_not_payer}, expected {expected_minter}"
        )));
    }
    if quantity != U256::from(expected_quantity) {
        return Err(BotError::Transaction(format!(
            "OpenSea SeaDrop calldata quantity is {quantity}, expected {expected_quantity}"
        )));
    }
    if let Some(price_per_nft) = embedded_price {
        let total = price_per_nft
            .checked_mul(quantity)
            .ok_or_else(|| BotError::Transaction("SeaDrop mint value overflowed".to_string()))?;
        if total != expected_value {
            return Err(BotError::Transaction(format!(
                "OpenSea SeaDrop calldata price totals {total} wei, but the transaction value is {expected_value} wei"
            )));
        }
    }
    Ok(())
}

fn opensea_chain_slug(chain_id: u64) -> Result<&'static str> {
    match chain_id {
        crate::config::ROBINHOOD_MAINNET_CHAIN_ID => Ok("robinhood"),
        crate::config::INK_MAINNET_CHAIN_ID => Ok("ink"),
        crate::config::HYPEREVM_MAINNET_CHAIN_ID => Ok("hyperevm"),
        _ => Err(BotError::Config(format!(
            "OpenSea contract verification is not configured for chain ID {chain_id}"
        ))),
    }
}

fn collection_slug_from_contract_response(body: &Value) -> Option<&str> {
    body.get("collection")
        .and_then(|collection| {
            collection
                .as_str()
                .or_else(|| collection.get("slug").and_then(Value::as_str))
        })
        .or_else(|| body.get("collection_slug").and_then(Value::as_str))
}

fn is_retryable_mint_error(error: &BotError) -> bool {
    matches!(
        error,
        BotError::OpenSeaTransport
            | BotError::OpenSeaApi {
                status: 429 | 500 | 502 | 503 | 504,
                ..
            }
    )
}

/// Start a low-frequency schedule refresher for automatic OpenSea stage
/// selection. The initial stage list is a snapshot; this task lets a running
/// bot notice a developer moving a stage earlier or later.
pub fn spawn_schedule_refresh(
    client: OpenSeaClient,
    drop_slug: String,
    initial_stages: Vec<OpenSeaStage>,
) -> (watch::Receiver<Vec<OpenSeaStage>>, JoinHandle<()>) {
    let (sender, receiver) = watch::channel(initial_stages.clone());
    let task = tokio::spawn(async move {
        refresh_schedule(client, drop_slug, initial_stages, sender).await;
    });
    (receiver, task)
}

async fn refresh_schedule(
    client: OpenSeaClient,
    drop_slug: String,
    mut previous: Vec<OpenSeaStage>,
    sender: watch::Sender<Vec<OpenSeaStage>>,
) {
    let mut consecutive_failures = 0_u32;
    loop {
        let normal_delay = schedule_refresh_delay(&previous);
        let backoff_seconds = 5_u64.saturating_mul(1_u64 << consecutive_failures.min(4));
        let delay = normal_delay.max(Duration::from_secs(backoff_seconds.min(60)));
        tokio::time::sleep(delay).await;
        let stages = match client.get_stages(&drop_slug).await {
            Ok(stages) => {
                consecutive_failures = 0;
                stages
            }
            Err(error) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                tracing::warn!(
                    error = %error,
                    "OpenSea stage schedule refresh failed; keeping the previous schedule"
                );
                continue;
            }
        };
        let now = unix_seconds();
        let mut current = stages
            .into_iter()
            .filter(|stage| stage.end_time.is_none_or(|end_time| end_time >= now))
            .collect::<Vec<_>>();
        current.sort_by_key(|stage| stage.start_time);
        if current.is_empty() || current == previous {
            continue;
        }
        if sender.send(current.clone()).is_err() {
            return;
        }
        previous = current;
    }
}

fn schedule_refresh_delay(stages: &[OpenSeaStage]) -> Duration {
    let now = unix_seconds();
    let soon = stages
        .iter()
        .filter(|stage| stage.start_time > now)
        .any(|stage| stage.start_time.saturating_sub(now) <= 300);
    if soon {
        Duration::from_secs(5)
    } else {
        Duration::from_secs(30)
    }
}

fn unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_stage(stage: &Value, index: usize) -> Result<OpenSeaStage> {
    let start = stage
        .get("startTime")
        .or_else(|| stage.get("start_time"))
        .and_then(parse_timestamp)
        .ok_or_else(|| {
            BotError::Transaction(format!(
                "OpenSea stage {} has no parseable startTime",
                index + 1
            ))
        })?;
    let end = stage
        .get("endTime")
        .or_else(|| stage.get("end_time"))
        .and_then(parse_timestamp);
    let label = stage.get("label").and_then(Value::as_str).map_or_else(
        || format!("stage {}", index + 1),
        |label| sanitize_external_text(label, 80),
    );
    Ok(OpenSeaStage {
        label,
        start_time: start,
        end_time: end,
    })
}

fn parse_timestamp(value: &Value) -> Option<u64> {
    if let Some(timestamp) = value.as_u64() {
        return Some(timestamp);
    }
    let value = value.as_str()?;
    if let Ok(timestamp) = value.parse::<u64>() {
        return Some(timestamp);
    }
    let timestamp = OffsetDateTime::parse(value, &Rfc3339)
        .ok()?
        .unix_timestamp();
    u64::try_from(timestamp).ok()
}

fn decode_hex(value: &str, field: &str) -> Result<Vec<u8>> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.len() > MAX_CALLDATA_BYTES.saturating_mul(2) {
        return Err(BotError::Transaction(format!(
            "OpenSea returned {field} larger than the safety limit"
        )));
    }
    hex::decode(value).map_err(|err| {
        BotError::Transaction(format!("OpenSea returned invalid {field} hex: {err}"))
    })
}

fn parse_u256(value: &str) -> Result<U256> {
    let value = value.trim();
    let (digits, radix) = value
        .strip_prefix("0x")
        .map_or((value, 10), |digits| (digits, 16));
    U256::from_str_radix(digits, radix)
        .map_err(|_| BotError::Transaction("OpenSea returned an invalid transaction value".into()))
}

async fn classify_rejection(status: u16, mut response: Response) -> String {
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        if body
            .len()
            .checked_add(chunk.len())
            .is_none_or(|length| length as u64 > MAX_RESPONSE_BYTES)
        {
            break;
        }
        body.extend_from_slice(&chunk);
    }
    let body = String::from_utf8_lossy(&body);
    classify_rejection_body(status, &body)
}

fn classify_rejection_body(status: u16, body: &str) -> String {
    let body = body.to_ascii_lowercase();

    match status {
        400 => "invalid mint request".to_string(),
        401 | 403 => "OpenSea API authorization rejected".to_string(),
        404 => "drop slug was not found".to_string(),
        409 => "drop stage is not active".to_string(),
        422 if body.contains("supply") || body.contains("sold out") => {
            "supply exhausted or unavailable".to_string()
        }
        422 if body.contains("allowlist")
            || body.contains("eligible")
            || body.contains("whitelist") =>
        {
            "wallet is not eligible for this stage".to_string()
        }
        422 if body.contains("limit") || body.contains("maximum") => {
            "wallet mint limit exceeded".to_string()
        }
        422 if body.contains("balance") || body.contains("fund") => {
            "insufficient native balance for the mint".to_string()
        }
        422 => {
            "OpenSea mint precondition failed (eligibility, limit, balance, or supply)".to_string()
        }
        429 => "OpenSea API rate limit reached".to_string(),
        _ => "request rejected by OpenSea".to_string(),
    }
}

async fn read_json_response<T: DeserializeOwned>(
    mut response: Response,
    context: &str,
) -> Result<T> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES)
    {
        return Err(BotError::Transaction(format!(
            "{context}: response exceeded the 1 MiB safety limit"
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| BotError::Transaction(format!("{context}: response body could not be read")))?
    {
        let next_length = body.len().checked_add(chunk.len()).ok_or_else(|| {
            BotError::Transaction(format!("{context}: response length overflowed"))
        })?;
        if next_length as u64 > MAX_RESPONSE_BYTES {
            return Err(BotError::Transaction(format!(
                "{context}: response exceeded the 1 MiB safety limit"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|_| BotError::Transaction(format!("{context}: malformed JSON")))
}

#[cfg(test)]
mod tests {
    use super::{
        SeaDropMint, SeaDropMintParams, classify_rejection_body,
        collection_slug_from_contract_response, decode_hex, is_retryable_mint_error,
        opensea_chain_slug, parse_drop_details, parse_stage, parse_u256, validate_seadrop_calldata,
    };
    use crate::error::BotError;
    use alloy::{
        primitives::{Address, U256, address},
        sol_types::SolCall,
    };
    use serde_json::json;

    #[test]
    fn parses_decimal_and_hex_values() {
        assert_eq!(parse_u256("100").unwrap().to_string(), "100");
        assert_eq!(parse_u256("0x64").unwrap().to_string(), "100");
    }

    #[test]
    fn decodes_optional_hex_prefix() {
        assert_eq!(decode_hex("0x1234", "calldata").unwrap(), vec![0x12, 0x34]);
        assert_eq!(decode_hex("", "calldata").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn parses_numeric_and_rfc3339_stage_times() {
        let stage = parse_stage(
            &json!({
                "label": "Public",
                "startTime": "2026-01-01T00:00:00Z",
                "endTime": 1_767_225_600_u64
            }),
            0,
        )
        .unwrap();
        assert_eq!(stage.label, "Public");
        assert_eq!(stage.start_time, 1_767_225_600);
        assert_eq!(stage.end_time, Some(1_767_225_600));
    }

    #[test]
    fn parses_live_drop_supply_with_the_stage_schedule() {
        let drop = parse_drop_details(&json!({
            "totalSupply": "3332",
            "maxSupply": "3333",
            "stages": [{
                "label": "Public",
                "startTime": 1_767_225_600_u64
            }]
        }))
        .unwrap();
        assert_eq!(drop.remaining_supply(), Some(U256::from(1)));
        assert_eq!(drop.stages[0].label, "Public");
    }

    #[test]
    fn classifies_safe_rejection_reasons_without_returning_body() {
        assert_eq!(
            classify_rejection_body(422, "mint sold out; wallet=0xdeadbeef"),
            "supply exhausted or unavailable"
        );
    }

    #[test]
    fn does_not_retry_a_known_inactive_stage_response() {
        assert!(!is_retryable_mint_error(&BotError::OpenSeaApi {
            status: 409,
            message: "drop stage is not active".to_string(),
        }));
        assert!(is_retryable_mint_error(&BotError::OpenSeaApi {
            status: 503,
            message: "temporary service failure".to_string(),
        }));
    }

    #[test]
    fn validates_public_seadrop_collection_recipient_and_quantity() {
        let collection = address!("0x0000000000000000000000000000000000000011");
        let wallet = address!("0x0000000000000000000000000000000000000022");
        let calldata = SeaDropMint::mintPublicCall {
            nftContract: collection,
            feeRecipient: Address::ZERO,
            minterIfNotPayer: wallet,
            quantity: U256::from(2),
        }
        .abi_encode();

        assert!(validate_seadrop_calldata(&calldata, collection, wallet, 2, U256::ZERO).is_ok());
        assert!(
            validate_seadrop_calldata(
                &calldata,
                address!("0x0000000000000000000000000000000000000033"),
                wallet,
                2,
                U256::ZERO,
            )
            .is_err()
        );
        assert!(validate_seadrop_calldata(&calldata, collection, wallet, 1, U256::ZERO).is_err());
    }

    #[test]
    fn validates_allowlist_embedded_price_against_transaction_value() {
        let collection = address!("0x0000000000000000000000000000000000000011");
        let wallet = address!("0x0000000000000000000000000000000000000022");
        let calldata = SeaDropMint::mintAllowListCall {
            nftContract: collection,
            feeRecipient: Address::ZERO,
            minterIfNotPayer: wallet,
            quantity: U256::from(2),
            mintParams: SeaDropMintParams {
                mintPrice: U256::from(10),
                maxTotalMintableByWallet: U256::from(2),
                startTime: U256::from(1),
                endTime: U256::from(2),
                dropStageIndex: U256::from(1),
                maxTokenSupplyForStage: U256::from(100),
                feeBps: U256::ZERO,
                restrictFeeRecipients: false,
            },
            proof: Vec::new(),
        }
        .abi_encode();

        assert!(
            validate_seadrop_calldata(&calldata, collection, wallet, 2, U256::from(20)).is_ok()
        );
        assert!(
            validate_seadrop_calldata(&calldata, collection, wallet, 2, U256::from(21)).is_err()
        );
    }

    #[test]
    fn binds_contract_verification_to_supported_opensea_chains() {
        assert_eq!(opensea_chain_slug(4663).unwrap(), "robinhood");
        assert_eq!(opensea_chain_slug(57073).unwrap(), "ink");
        assert_eq!(opensea_chain_slug(999).unwrap(), "hyperevm");
        assert!(opensea_chain_slug(1).is_err());
        assert_eq!(
            collection_slug_from_contract_response(&json!({
                "collection": { "slug": "expected-drop" }
            })),
            Some("expected-drop")
        );
    }
}
