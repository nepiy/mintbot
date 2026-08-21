use crate::error::{BotError, Result};
use alloy::primitives::{Address, U256};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use zeroize::Zeroizing;

const OPENSEA_API_BASE: &str = "https://api.opensea.io/api/v2";

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
            .map_err(|err| BotError::Transaction(format!("OpenSea API request failed: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            let message = response
                .text()
                .await
                .unwrap_or_else(|err| format!("could not read response body: {err}"));
            return Err(BotError::OpenSeaApi {
                status: status.as_u16(),
                message: truncate_message(&message),
            });
        }
        let response: BuildMintResponse = response.json().await.map_err(|err| {
            BotError::Transaction(format!(
                "OpenSea API returned invalid transaction data: {err}"
            ))
        })?;
        let target = response
            .target
            .parse()
            .map_err(|_| BotError::InvalidAddress {
                value: response.target.clone(),
            })?;
        let calldata = decode_hex(&response.calldata, "calldata")?;
        let value = parse_u256(&response.value)?;
        Ok(OpenSeaMintTransaction {
            target,
            calldata,
            value,
        })
    }

    pub async fn get_stages(&self, drop_slug: &str) -> Result<Vec<OpenSeaStage>> {
        let url = format!("{OPENSEA_API_BASE}/drops/{drop_slug}");
        let response = self
            .client
            .get(url)
            .header("X-API-KEY", self.api_key.as_str())
            .send()
            .await
            .map_err(|err| BotError::Transaction(format!("OpenSea API request failed: {err}")))?;
        let status = response.status();
        if !status.is_success() {
            let message = response
                .text()
                .await
                .unwrap_or_else(|err| format!("could not read response body: {err}"));
            return Err(BotError::OpenSeaApi {
                status: status.as_u16(),
                message: truncate_message(&message),
            });
        }
        let body: Value = response.json().await.map_err(|err| {
            BotError::Transaction(format!("OpenSea returned invalid drop details: {err}"))
        })?;
        let stages = body
            .get("stages")
            .or_else(|| body.get("drop").and_then(|drop| drop.get("stages")))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                BotError::Transaction(
                    "OpenSea drop details did not include a stages array".to_string(),
                )
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
        Ok(parsed)
    }
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
    let label = stage
        .get("label")
        .and_then(Value::as_str)
        .map_or_else(|| format!("stage {}", index + 1), ToOwned::to_owned);
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
    hex::decode(value).map_err(|err| {
        BotError::Transaction(format!("OpenSea returned invalid {field} hex: {err}"))
    })
}

fn parse_u256(value: &str) -> Result<U256> {
    let value = value.trim();
    let (digits, radix) = value
        .strip_prefix("0x")
        .map_or((value, 10), |digits| (digits, 16));
    U256::from_str_radix(digits, radix).map_err(|err| BotError::InvalidAmount {
        value: value.to_string(),
        reason: format!("OpenSea returned an invalid transaction value: {err}"),
    })
}

fn truncate_message(message: &str) -> String {
    const MAX_MESSAGE_LENGTH: usize = 512;
    let mut message = message.trim().to_string();
    if message.len() > MAX_MESSAGE_LENGTH {
        message.truncate(MAX_MESSAGE_LENGTH);
        message.push_str("...");
    }
    message
}

#[cfg(test)]
mod tests {
    use super::{decode_hex, parse_stage, parse_u256};
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
}
