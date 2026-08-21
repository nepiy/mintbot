use crate::error::{BotError, Result};
use alloy::{
    network::{EthereumWallet, NetworkWallet},
    signers::local::PrivateKeySigner,
};
use std::env;
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct LoadedWallet {
    pub address: alloy::primitives::Address,
    pub wallet: EthereumWallet,
}

impl std::fmt::Debug for LoadedWallet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LoadedWallet")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl LoadedWallet {
    pub fn from_env() -> Result<Self> {
        let private_key = Zeroizing::new(env::var("PRIVATE_KEY").map_err(|_| {
            BotError::Wallet(
                "PRIVATE_KEY is not set; load it from .env or the process environment".to_string(),
            )
        })?);
        if private_key.trim().is_empty() {
            return Err(BotError::Wallet("PRIVATE_KEY is empty".to_string()));
        }
        let signer: PrivateKeySigner = private_key
            .as_str()
            .parse::<PrivateKeySigner>()
            .map_err(|err| BotError::Wallet(format!("{err:?}")))?;
        let address = signer.address();
        Ok(Self {
            address,
            wallet: EthereumWallet::new(signer),
        })
    }

    pub async fn sign_request(
        &self,
        request: alloy::rpc::types::TransactionRequest,
    ) -> Result<alloy::consensus::TxEnvelope> {
        <EthereumWallet as NetworkWallet<alloy::network::Ethereum>>::sign_request(
            &self.wallet,
            request,
        )
        .await
        .map_err(|err| BotError::Transaction(format!("local signing failed: {err}")))
    }
}

pub fn short_address(address: alloy::primitives::Address) -> String {
    let text = format!("{address:#x}");
    format!("{}...{}", &text[..8], &text[text.len() - 6..])
}
