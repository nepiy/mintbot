use crate::{
    config::{
        GasConfig, MintCallConfig, MintConfig, MintTrigger, NonceStrategy,
        ROBINHOOD_DEFAULT_GAS_LIMIT, ROBINHOOD_MAINNET_CHAIN_ID,
    },
    error::{BotError, Result},
};
use alloy::primitives::keccak256;
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

#[derive(Debug, Serialize, Deserialize)]
struct ManualControlInfo {
    port: u16,
    token: String,
}

pub fn run_wizard(output: &Path) -> Result<PathBuf> {
    let config = prompt_config(true)?;
    let name = config.name.clone();
    let path = if output.extension().is_some() {
        output.to_path_buf()
    } else {
        output.join(format!("{}.json", slugify(&name)))
    };
    config.save_pretty(&path)?;
    println!("\nSaved: {}", path.display());
    println!("Private keys are loaded only from PRIVATE_KEY and are never stored in this file.");
    Ok(path)
}

pub fn prompt_interactive_config() -> Result<MintConfig> {
    prompt_config(false)
}

fn prompt_config(allow_manual: bool) -> Result<MintConfig> {
    println!("NFT Mint Setup\n");
    let name = if allow_manual {
        ask("Collection name", "Example NFT")?
    } else {
        "Robinhood NFT".to_string()
    };
    let chain_id = if allow_manual {
        ask("Chain ID", "1")?
            .parse()
            .map_err(|err| BotError::Config(format!("invalid chain ID: {err}")))?
    } else {
        println!("Network: Robinhood Chain mainnet (chain ID {ROBINHOOD_MAINNET_CHAIN_ID})");
        ROBINHOOD_MAINNET_CHAIN_ID
    };
    let contract_address = ask(
        "Contract address",
        "0x0000000000000000000000000000000000000000",
    )?;
    let opensea_drop_slug = if allow_manual {
        None
    } else {
        let api_key_configured = std::env::var("OPENSEA_API_KEY")
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
        let slug = if api_key_configured {
            ask(
                "OpenSea drop slug (enter `direct` only for a custom contract mint)",
                "",
            )?
        } else {
            ask("OpenSea drop slug (blank for direct contract mint)", "")?
        };
        if api_key_configured && slug.trim().is_empty() {
            return Err(BotError::Config(
                "OPENSEA_API_KEY is configured, so enter the OpenSea drop slug or type `direct`"
                    .to_string(),
            ));
        }
        if slug.eq_ignore_ascii_case("direct") {
            None
        } else {
            (!slug.trim().is_empty()).then_some(slug)
        }
    };
    let quantity = ask("Mint quantity", "1")?
        .parse()
        .map_err(|err| BotError::Config(format!("invalid quantity: {err}")))?;

    if let Some(opensea_drop_slug) = opensea_drop_slug {
        let require_zero_value = ask_yes_no(
            "Require free mint (stop if OpenSea returns any payment)",
            true,
        )?;
        let stage_start = ask(
            "Earliest OpenSea stage time (Unix seconds; 0 to auto-use active/upcoming stages)",
            "0",
        )?
        .parse()
        .map_err(|err| BotError::Config(format!("invalid stage start time: {err}")))?;
        let config = MintConfig {
            name: "Robinhood NFT".to_string(),
            chain_id,
            native_currency: None,
            contract_address,
            opensea_drop_slug: Some(opensea_drop_slug),
            require_zero_value,
            quantity,
            mint: MintCallConfig {
                function: "mint(uint256)".to_string(),
                arguments: vec!["$quantity".to_string()],
                proof: None,
                // OpenSea returns the exact payable value for the active stage.
                price_per_nft: "0".to_string(),
            },
            trigger: MintTrigger::BlockTimestamp {
                timestamp: stage_start,
            },
            gas: GasConfig {
                gas_limit: Some(ROBINHOOD_DEFAULT_GAS_LIMIT),
                ..GasConfig::default()
            },
            nonce_strategy: NonceStrategy::JustBeforeTrigger,
            replacement: Default::default(),
            expected_start_time: None,
            confirmations: 1,
        };
        config.validate()?;
        println!(
            "OpenSea mode: GTD/FCFS stage and price are selected by the Drops API when the stage is active."
        );
        return Ok(config);
    }

    let (function, arguments) = if allow_manual {
        let function = ask("Mint function", "mint(uint256)")?;
        let arguments = ask(
            "Mint arguments (comma-separated; placeholders: $quantity, $wallet, $proof)",
            "$quantity",
        )?
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect();
        (function, arguments)
    } else {
        println!("Mint call: mint(uint256) with quantity as the only argument");
        ("mint(uint256)".to_string(), vec!["$quantity".to_string()])
    };
    let price_per_nft = ask("Price per NFT (native currency)", "0.005")?;
    let proof = ask(
        "Merkle proof (comma-separated bytes32 values; blank if none)",
        "",
    )?;
    let proof = if proof.trim().is_empty() {
        None
    } else {
        Some(
            proof
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect(),
        )
    };
    let gas_limit = if allow_manual {
        ask(
            "Prepared gas limit (required when a closed sale makes estimation revert)",
            &ROBINHOOD_DEFAULT_GAS_LIMIT.to_string(),
        )?
        .parse::<u64>()
        .map_err(|err| BotError::Config(format!("invalid gas limit: {err}")))?
    } else {
        println!(
            "Gas limit: {ROBINHOOD_DEFAULT_GAS_LIMIT} (automatic default; advanced config can override)"
        );
        ROBINHOOD_DEFAULT_GAS_LIMIT
    };
    if allow_manual {
        println!(
            "\nSelect trigger:\n1. Blockchain timestamp\n2. Boolean sale state\n3. Numeric sale phase\n4. Contract event\n5. Manual"
        );
    } else {
        println!(
            "\nSelect automatic trigger:\n1. Blockchain timestamp\n2. Boolean sale state (recommended)\n3. Numeric sale phase\n4. Contract event"
        );
    }
    let trigger_choice = ask("Trigger", "2")?;
    let trigger = match trigger_choice.trim() {
        "1" => MintTrigger::BlockTimestamp {
            timestamp: ask("Block timestamp (Unix seconds)", "0")?
                .parse()
                .map_err(|err| BotError::Config(format!("invalid timestamp: {err}")))?,
        },
        "2" => MintTrigger::BooleanContractState {
            function: ask("Sale state function", "publicSaleActive() returns (bool)")?,
            expected_value: ask("Expected value", "true")?
                .parse()
                .map_err(|err| BotError::Config(format!("invalid bool: {err}")))?,
        },
        "3" => MintTrigger::NumericPhase {
            function: ask("Sale phase function", "salePhase() returns (uint256)")?,
            target_value: ask("Target phase", "2")?,
        },
        "4" => MintTrigger::ContractEvent {
            signature: ask("Event signature", "PublicSaleStarted()")?,
            confirmations: Some(0),
        },
        "5" if allow_manual => MintTrigger::Manual,
        _ => {
            return Err(BotError::Config(if allow_manual {
                "trigger must be a number from 1 to 5".to_string()
            } else {
                "trigger must be a number from 1 to 4".to_string()
            }));
        }
    };
    let config = MintConfig {
        name,
        chain_id,
        native_currency: None,
        contract_address,
        opensea_drop_slug: None,
        require_zero_value: false,
        quantity,
        mint: MintCallConfig {
            function,
            arguments,
            proof,
            price_per_nft,
        },
        trigger,
        gas: GasConfig {
            gas_limit: Some(gas_limit),
            ..GasConfig::default()
        },
        nonce_strategy: if allow_manual {
            NonceStrategy::Preloaded
        } else {
            NonceStrategy::JustBeforeTrigger
        },
        replacement: Default::default(),
        expected_start_time: None,
        confirmations: 1,
    };
    config.validate()?;
    Ok(config)
}

pub async fn bind_manual_control(config_path: &Path) -> Result<(mpsc::Receiver<()>, PathBuf)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let path = control_path(config_path);
    let mut token_bytes = [0_u8; 32];
    getrandom::fill(&mut token_bytes)
        .map_err(|err| BotError::ManualTrigger(format!("could not create control token: {err}")))?;
    let token = hex::encode(token_bytes);
    write_control_file(
        &path,
        &ManualControlInfo {
            port,
            token: token.clone(),
        },
    )?;
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                break;
            };
            let mut line = String::new();
            let mut reader = BufReader::new(socket).take(129);
            if reader.read_line(&mut line).await.is_ok()
                && line.len() <= 128
                && line.trim_end() == format!("TRIGGER {token}")
            {
                let _ = sender.send(()).await;
                break;
            }
        }
    });
    println!(
        "Manual trigger control: nft-mint-bot trigger --config {}",
        config_path.display()
    );
    Ok((receiver, path))
}

pub async fn send_manual_trigger(config_path: &Path) -> Result<()> {
    let path = control_path(config_path);
    let contents = std::fs::read_to_string(&path).map_err(|_| {
        BotError::ManualTrigger(format!(
            "no running manual bot found for {}",
            config_path.display()
        ))
    })?;
    let control: ManualControlInfo = serde_json::from_str(&contents)
        .map_err(|err| BotError::ManualTrigger(format!("invalid control file: {err}")))?;
    let mut socket = TcpStream::connect(("127.0.0.1", control.port))
        .await
        .map_err(|err| BotError::ManualTrigger(err.to_string()))?;
    socket
        .write_all(format!("TRIGGER {}\n", control.token).as_bytes())
        .await?;
    socket.shutdown().await?;
    println!("Manual trigger sent.");
    Ok(())
}

pub fn cleanup_manual_control(path: &Path) {
    let _ = std::fs::remove_file(path);
}

fn control_path(config_path: &Path) -> PathBuf {
    let normalized = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.into());
    let digest = keccak256(normalized.to_string_lossy().as_bytes());
    std::env::temp_dir().join(format!("nft-mint-bot-{}.port", hex::encode(&digest[..8])))
}

fn write_control_file(path: &Path, control: &ManualControlInfo) -> Result<()> {
    let contents = serde_json::to_vec(control)?;
    let open_new = || {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)
    };
    let mut file = match open_new() {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            // A prior unclean shutdown may leave the file behind. Removing and
            // exclusively recreating it avoids following an attacker-supplied symlink.
            std::fs::remove_file(path)?;
            open_new()?
        }
        Err(err) => return Err(err.into()),
    };
    std::io::Write::write_all(&mut file, &contents)?;
    file.sync_all()?;
    Ok(())
}

fn ask(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let value = line.trim();
    Ok(if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    })
}

fn ask_yes_no(label: &str, default: bool) -> Result<bool> {
    let default_text = if default { "yes" } else { "no" };
    let value = ask(&format!("{label} (yes/no)"), default_text)?;
    match value.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Ok(true),
        "n" | "no" => Ok(false),
        _ => Err(BotError::Config(format!(
            "{label} must be answered yes or no"
        ))),
    }
}

fn slugify(value: &str) -> String {
    let mut slug = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>();
    slug.retain(|ch| ch != '-');
    if slug.is_empty() {
        "mint".to_string()
    } else {
        slug
    }
}
