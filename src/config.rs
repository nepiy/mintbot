use crate::{
    error::{BotError, Result},
    security::validate_direct_mint_function,
};
use alloy::{
    json_abi::Function,
    primitives::{Address, B256, U256},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
};

pub const ROBINHOOD_MAINNET_CHAIN_ID: u64 = 4663;
pub const INK_MAINNET_CHAIN_ID: u64 = 57073;
pub const HYPEREVM_MAINNET_CHAIN_ID: u64 = 999;
pub const ROBINHOOD_DEFAULT_GAS_LIMIT: u64 = 200_000;
pub const ROBINHOOD_DEFAULT_MAX_GAS_COST_NATIVE: &str = "0.001";
pub const INK_DEFAULT_GAS_LIMIT: u64 = 230_000;
pub const INK_DEFAULT_MAX_GAS_COST_NATIVE: &str = "0.001";
pub const HYPEREVM_DEFAULT_GAS_LIMIT: u64 = 230_000;
pub const HYPEREVM_DEFAULT_MAX_GAS_COST_NATIVE: &str = "0.001";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintConfig {
    pub name: String,
    pub chain_id: u64,
    #[serde(default)]
    pub native_currency: Option<String>,
    pub contract_address: String,
    #[serde(default)]
    pub expected_contract_code_hash: Option<String>,
    #[serde(default)]
    pub opensea_drop_slug: Option<String>,
    #[serde(default)]
    pub opensea_execution_mode: OpenSeaExecutionMode,
    #[serde(default)]
    pub require_zero_value: bool,
    #[serde(default)]
    pub max_price_per_nft: Option<String>,
    pub quantity: u64,
    pub mint: MintCallConfig,
    pub trigger: MintTrigger,
    #[serde(default)]
    pub gas: GasConfig,
    #[serde(default)]
    pub nonce_strategy: NonceStrategy,
    #[serde(default)]
    pub replacement: ReplacementConfig,
    #[serde(default)]
    pub expected_start_time: Option<u64>,
    #[serde(default = "default_confirmations")]
    pub confirmations: u64,
}

/// Controls how much OpenSea transaction preparation is performed on the
/// mint critical path. Both modes still obtain wallet-specific calldata from
/// OpenSea and enforce the configured payment guards.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OpenSeaExecutionMode {
    #[default]
    Normal,
    Aggressive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MintCallConfig {
    pub function: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub proof: Option<Vec<String>>,
    #[serde(default = "default_price")]
    pub price_per_nft: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum MintTrigger {
    BlockTimestamp {
        timestamp: u64,
    },
    BooleanContractState {
        function: String,
        expected_value: bool,
    },
    NumericPhase {
        function: String,
        target_value: String,
    },
    ContractEvent {
        signature: String,
        #[serde(default)]
        confirmations: Option<u64>,
    },
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GasMode {
    #[default]
    Auto,
    Eip1559,
    Legacy,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GasConfig {
    #[serde(default)]
    pub mode: GasMode,
    #[serde(default = "default_multiplier")]
    pub multiplier: f64,
    pub gas_limit: Option<u64>,
    pub gas_price_gwei: Option<String>,
    pub max_fee_gwei: Option<String>,
    pub max_priority_fee_gwei: Option<String>,
    pub max_total_gas_cost_native: Option<String>,
}

impl Default for GasConfig {
    fn default() -> Self {
        Self {
            mode: GasMode::Auto,
            multiplier: default_multiplier(),
            gas_limit: None,
            gas_price_gwei: None,
            max_fee_gwei: None,
            max_priority_fee_gwei: None,
            max_total_gas_cost_native: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NonceStrategy {
    #[default]
    Preloaded,
    RefreshEachBlock,
    JustBeforeTrigger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacementConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_after_blocks")]
    pub after_blocks: u64,
    #[serde(default = "default_replacement_multiplier")]
    pub fee_multiplier: f64,
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
}

impl Default for ReplacementConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            after_blocks: default_after_blocks(),
            fee_multiplier: default_replacement_multiplier(),
            max_attempts: default_max_attempts(),
        }
    }
}

fn default_confirmations() -> u64 {
    1
}

fn default_price() -> String {
    "0".to_string()
}

fn default_multiplier() -> f64 {
    1.15
}

fn default_after_blocks() -> u64 {
    2
}

fn default_replacement_multiplier() -> f64 {
    1.15
}

fn default_max_attempts() -> u32 {
    2
}

impl MintConfig {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let text = fs::read_to_string(path)
            .map_err(|err| BotError::Config(format!("could not read {}: {err}", path.display())))?;
        let config: Self = serde_json::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    pub fn save_pretty(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all((serde_json::to_string_pretty(self)? + "\n").as_bytes())?;
        file.sync_all()?;
        Ok(())
    }

    pub fn contract(&self) -> Result<Address> {
        self.contract_address
            .parse()
            .map_err(|_| BotError::InvalidAddress {
                value: self.contract_address.clone(),
            })
    }

    pub fn expected_contract_code_hash_value(&self) -> Result<Option<B256>> {
        self.expected_contract_code_hash
            .as_deref()
            .map(|value| {
                if value.len() != 66
                    || !value.starts_with("0x")
                    || !value[2..].chars().all(|character| character.is_ascii_hexdigit())
                {
                    return Err(BotError::Config(format!(
                        "expected_contract_code_hash must be a 0x-prefixed 32-byte hash, got `{value}`"
                    )));
                }
                value
                    .parse::<B256>()
                    .map_err(|_| BotError::Config(format!(
                        "expected_contract_code_hash must be a 0x-prefixed 32-byte hash, got `{value}`"
                    )))
            })
            .transpose()
    }

    pub fn mint_value_wei(&self) -> Result<U256> {
        parse_native_amount(&self.mint.price_per_nft)?
            .checked_mul(U256::from(self.quantity))
            .ok_or_else(|| BotError::InvalidAmount {
                value: self.mint.price_per_nft.clone(),
                reason: "quantity multiplication overflowed".to_string(),
            })
    }

    pub fn maximum_opensea_mint_value_wei(&self) -> Result<Option<U256>> {
        self.max_price_per_nft
            .as_deref()
            .map(parse_native_amount)
            .transpose()?
            .map(|price| {
                price.checked_mul(U256::from(self.quantity)).ok_or_else(|| {
                    BotError::InvalidAmount {
                        value: self.max_price_per_nft.clone().unwrap_or_default(),
                        reason: "quantity multiplication overflowed".to_string(),
                    }
                })
            })
            .transpose()
    }

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(BotError::Config("name must not be empty".to_string()));
        }
        if self.name.chars().any(char::is_control) {
            return Err(BotError::Config(
                "name must not contain terminal control characters".to_string(),
            ));
        }
        if self.chain_id == 0 {
            return Err(BotError::Config(
                "chain_id must be greater than zero".to_string(),
            ));
        }
        let _ = self.contract()?;
        let _ = self.expected_contract_code_hash_value()?;
        if let Some(slug) = self.opensea_drop_slug.as_deref() {
            let slug = slug.trim();
            if slug.is_empty()
                || !slug.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
                })
            {
                return Err(BotError::Config(
                    "opensea_drop_slug must contain only letters, numbers, `-`, and `_`"
                        .to_string(),
                ));
            }
            if !matches!(self.trigger, MintTrigger::BlockTimestamp { .. }) {
                return Err(BotError::Config(
                    "OpenSea mode requires a block_timestamp trigger".to_string(),
                ));
            }
            if self.quantity > 100 {
                return Err(BotError::Config(
                    "OpenSea mint quantity must be between 1 and 100".to_string(),
                ));
            }
            if matches!(
                self.opensea_execution_mode,
                OpenSeaExecutionMode::Aggressive
            ) && (self.gas.gas_limit.is_none() || self.gas.max_total_gas_cost_native.is_none())
            {
                return Err(BotError::Config(
                    "aggressive OpenSea mode requires gas.gas_limit and gas.max_total_gas_cost_native"
                        .to_string(),
                ));
            }
            let maximum = self.maximum_opensea_mint_value_wei()?;
            if self.require_zero_value {
                if maximum.is_some_and(|value| !value.is_zero()) {
                    return Err(BotError::Config(
                        "max_price_per_nft must be zero or omitted when require_zero_value is enabled"
                            .to_string(),
                    ));
                }
            } else if maximum.is_none() {
                return Err(BotError::Config(
                    "max_price_per_nft is required for a paid OpenSea mint".to_string(),
                ));
            }
        }
        if self.opensea_drop_slug.is_none()
            && !matches!(self.opensea_execution_mode, OpenSeaExecutionMode::Normal)
        {
            return Err(BotError::Config(
                "opensea_execution_mode requires opensea_drop_slug".to_string(),
            ));
        }
        if self.quantity == 0 {
            return Err(BotError::Config(
                "quantity must be greater than zero".to_string(),
            ));
        }
        if self.mint.function.trim().is_empty() {
            return Err(BotError::Config(
                "mint.function must not be empty".to_string(),
            ));
        }
        if !self.mint.function.contains('(') {
            return Err(BotError::Config(
                "mint.function must be a Solidity signature such as mint(uint256)".to_string(),
            ));
        }
        let mint_function = Function::parse(&self.mint.function)
            .map_err(|error| BotError::Abi(format!("{}: {error}", self.mint.function)))?;
        if self.opensea_drop_slug.is_none() {
            validate_direct_mint_function(&mint_function)?;
        }
        let _ = parse_native_amount(&self.mint.price_per_nft)?;
        if !(self.gas.multiplier.is_finite() && self.gas.multiplier >= 1.0) {
            return Err(BotError::Config(
                "gas.multiplier must be finite and at least 1.0".to_string(),
            ));
        }
        if self.gas.gas_limit == Some(0) {
            return Err(BotError::Config(
                "gas.gas_limit must be greater than zero".to_string(),
            ));
        }
        if let Some(maximum) = self.gas.max_total_gas_cost_native.as_deref() {
            let _ = parse_native_amount(maximum)?;
        }
        match self.gas.mode {
            GasMode::Auto => {}
            GasMode::Legacy => {
                let value = self.gas.gas_price_gwei.as_deref().ok_or_else(|| {
                    BotError::Config("gas.gas_price_gwei is required for legacy mode".to_string())
                })?;
                let _ = parse_gwei(value)?;
            }
            GasMode::Eip1559 | GasMode::Manual => {
                let max_fee = self.gas.max_fee_gwei.as_deref().ok_or_else(|| {
                    BotError::Config(
                        "gas.max_fee_gwei is required for eip1559/manual mode".to_string(),
                    )
                })?;
                let priority = self.gas.max_priority_fee_gwei.as_deref().ok_or_else(|| {
                    BotError::Config(
                        "gas.max_priority_fee_gwei is required for eip1559/manual mode".to_string(),
                    )
                })?;
                if parse_gwei(priority)? > parse_gwei(max_fee)? {
                    return Err(BotError::Config(
                        "gas.max_priority_fee_gwei must not exceed max_fee_gwei".to_string(),
                    ));
                }
            }
        }
        if self.confirmations == 0 {
            return Err(BotError::Config(
                "confirmations must be greater than zero".to_string(),
            ));
        }
        if self.replacement.enabled {
            if self.replacement.max_attempts == 0 {
                return Err(BotError::Config(
                    "replacement.max_attempts must be greater than zero when enabled".to_string(),
                ));
            }
            if self.replacement.after_blocks == 0 {
                return Err(BotError::Config(
                    "replacement.after_blocks must be greater than zero when enabled".to_string(),
                ));
            }
            if !(self.replacement.fee_multiplier.is_finite()
                && self.replacement.fee_multiplier > 1.0)
            {
                return Err(BotError::Config(
                    "replacement.fee_multiplier must be finite and greater than 1.0".to_string(),
                ));
            }
        }
        Ok(())
    }
}

pub fn parse_native_amount(value: &str) -> Result<U256> {
    let value = value.trim();
    if value.is_empty() {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: "empty value".to_string(),
        });
    }
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some() || whole.is_empty() || !whole.chars().all(|c| c.is_ascii_digit()) {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: "expected a non-negative decimal number".to_string(),
        });
    }
    if fraction.len() > 18 || !fraction.chars().all(|c| c.is_ascii_digit()) {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: "native amounts support at most 18 decimal places".to_string(),
        });
    }
    let whole = U256::from_str_radix(whole, 10).map_err(|err| BotError::InvalidAmount {
        value: value.to_string(),
        reason: err.to_string(),
    })?;
    let fraction_padded = format!("{fraction:0<18}");
    let fraction =
        U256::from_str_radix(&fraction_padded, 10).map_err(|err| BotError::InvalidAmount {
            value: value.to_string(),
            reason: err.to_string(),
        })?;
    whole
        .checked_mul(U256::from(1_000_000_000_000_000_000u128))
        .and_then(|base| base.checked_add(fraction))
        .ok_or_else(|| BotError::InvalidAmount {
            value: value.to_string(),
            reason: "value overflowed U256".to_string(),
        })
}

pub fn parse_gwei(value: &str) -> Result<u128> {
    let wei = parse_decimal_units(value, 9)?;
    if wei > U256::from(u128::MAX) {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: "gwei value does not fit into u128 wei".to_string(),
        });
    }
    Ok(wei.to::<u128>())
}

fn parse_decimal_units(value: &str, decimals: usize) -> Result<U256> {
    let value = value.trim();
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.chars().all(|c| c.is_ascii_digit())
        || fraction.len() > decimals
        || !fraction.chars().all(|c| c.is_ascii_digit())
    {
        return Err(BotError::InvalidAmount {
            value: value.to_string(),
            reason: format!("expected a decimal with at most {decimals} places"),
        });
    }
    let whole = U256::from_str_radix(whole, 10).map_err(|err| BotError::InvalidAmount {
        value: value.to_string(),
        reason: err.to_string(),
    })?;
    let scale = U256::from(10u64).pow(U256::from(decimals));
    let fraction = format!("{fraction:0<decimals$}");
    let fraction = U256::from_str_radix(&fraction, 10).map_err(|err| BotError::InvalidAmount {
        value: value.to_string(),
        reason: err.to_string(),
    })?;
    whole
        .checked_mul(scale)
        .and_then(|base| base.checked_add(fraction))
        .ok_or_else(|| BotError::InvalidAmount {
            value: value.to_string(),
            reason: "value overflowed U256".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_native_amounts() {
        assert_eq!(
            parse_native_amount("0.005").unwrap(),
            U256::from(5_000_000_000_000_000u64)
        );
        assert_eq!(
            parse_native_amount("1").unwrap(),
            U256::from(1_000_000_000_000_000_000u64)
        );
    }

    #[test]
    fn rejects_more_than_eighteen_decimals() {
        assert!(parse_native_amount("0.0000000000000000001").is_err());
    }
}
