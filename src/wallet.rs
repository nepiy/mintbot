use crate::error::{BotError, Result};
use alloy::{
    network::{EthereumWallet, NetworkWallet},
    primitives::keccak256,
    signers::local::PrivateKeySigner,
};
use std::{env, fs::OpenOptions};
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
            .map_err(|_| BotError::Wallet("PRIVATE_KEY has an invalid format".to_string()))?;
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

pub struct WalletNonceLock {
    _file: std::fs::File,
    contended: bool,
}

impl WalletNonceLock {
    pub async fn acquire(chain_id: u64, address: alloy::primitives::Address) -> Result<Self> {
        let mut identity = chain_id.to_be_bytes().to_vec();
        identity.extend_from_slice(address.as_slice());
        let digest = keccak256(identity);
        let path = std::env::temp_dir().join(format!(
            "nft-mint-bot-wallet-{}.lock",
            hex::encode(&digest[..12])
        ));
        let (file, contended) =
            tokio::task::spawn_blocking(move || -> std::io::Result<(std::fs::File, bool)> {
                let mut options = OpenOptions::new();
                options.read(true).write(true).create(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let file = options.open(path)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                }
                let contended = match file.try_lock() {
                    Ok(()) => false,
                    Err(std::fs::TryLockError::WouldBlock) => {
                        file.lock()?;
                        true
                    }
                    Err(std::fs::TryLockError::Error(error)) => return Err(error),
                };
                Ok((file, contended))
            })
            .await
            .map_err(|err| BotError::Wallet(format!("nonce lock task failed: {err}")))??;
        Ok(Self {
            _file: file,
            contended,
        })
    }

    pub fn was_contended(&self) -> bool {
        self.contended
    }
}

pub fn short_address(address: alloy::primitives::Address) -> String {
    let text = format!("{address:#x}");
    format!("{}...{}", &text[..8], &text[text.len() - 6..])
}
