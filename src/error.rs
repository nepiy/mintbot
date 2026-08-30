use alloy::primitives::{Address, B256};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BotError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("invalid address `{value}`")]
    InvalidAddress { value: String },

    #[error("invalid native amount `{value}`: {reason}")]
    InvalidAmount { value: String, reason: String },

    #[error("private key could not be loaded: {0}")]
    Wallet(String),

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("ABI error: {0}")]
    Abi(String),

    #[error("chain ID mismatch: configured {configured}, RPC reported {reported}")]
    ChainMismatch { configured: u64, reported: u64 },

    #[error("contract {address} has no deployed bytecode")]
    MissingContract { address: Address },

    #[error("insufficient balance: need {needed} wei, have {available} wei")]
    InsufficientBalance { needed: String, available: String },

    #[error(
        "gas safety limit exceeded: estimated maximum {estimated} wei > configured maximum {maximum} wei"
    )]
    GasLimitExceeded { estimated: String, maximum: String },

    #[error(
        "mint price safety limit exceeded: OpenSea returned {returned} wei > configured maximum {maximum} wei"
    )]
    MintValueExceeded { returned: String, maximum: String },

    #[error("trigger error: {0}")]
    Trigger(String),

    #[error("transaction error: {0}")]
    Transaction(String),

    #[error(
        "broadcast outcome is unknown for transaction {hash}; verify the transaction before rerunning"
    )]
    BroadcastOutcomeUnknown { hash: B256 },

    #[error("OpenSea API error ({status}): {message}")]
    OpenSeaApi { status: u16, message: String },

    #[error("OpenSea API transport request failed")]
    OpenSeaTransport,

    #[error("manual trigger control error: {0}")]
    ManualTrigger(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, BotError>;
