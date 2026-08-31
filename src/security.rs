use crate::error::{BotError, Result};
use alloy::{json_abi::Function, primitives::keccak256};
use std::path::Path;

const REDACTED_URL: &str = "[redacted URL]";
const REDACTED_ADDRESS: &str = "[redacted address]";

// A direct-mode mint must never become a generic wallet transaction builder.
// Check both names and selectors: selector checks also catch a deliberately
// misleading function name that collides with a standard asset-moving call.
const BLOCKED_DIRECT_SIGNATURES: &[&str] = &[
    "approve(address,uint256)",
    "setApprovalForAll(address,bool)",
    "transfer(address,uint256)",
    "transferFrom(address,address,uint256)",
    "safeTransferFrom(address,address,uint256)",
    "safeTransferFrom(address,address,uint256,bytes)",
    "safeTransferFrom(address,address,uint256,uint256,bytes)",
    "safeBatchTransferFrom(address,address,uint256[],uint256[],bytes)",
    "increaseAllowance(address,uint256)",
    "decreaseAllowance(address,uint256)",
    "permit(address,address,uint256,uint256,uint8,bytes32,bytes32)",
    "permit(address,address,uint256,uint256,bool,uint8,bytes32,bytes32)",
];

const BLOCKED_DIRECT_NAMES: &[&str] = &[
    "approve",
    "setapprovalforall",
    "transfer",
    "transferfrom",
    "safetransferfrom",
    "safebatchtransferfrom",
    "increaseallowance",
    "decreaseallowance",
    "execute",
    "executebatch",
    "multicall",
    "aggregate",
    "aggregate3",
    "tryaggregate",
    "batch",
    "call",
    "delegatecall",
];

pub fn validate_direct_mint_function(function: &Function) -> Result<()> {
    let name = function.name.to_ascii_lowercase();
    let blocked_name = BLOCKED_DIRECT_NAMES.contains(&name.as_str())
        || name.contains("permit")
        || name.contains("approval")
        || name.contains("transfer")
        || name.contains("execute")
        || name.contains("multicall")
        || name.contains("delegatecall")
        || name.contains("withdraw")
        || name.contains("sweep")
        || name.contains("swap");
    let selector = function.selector();
    let blocked_selector = BLOCKED_DIRECT_SIGNATURES.iter().any(|signature| {
        let digest = keccak256(signature.as_bytes());
        selector.as_slice() == &digest[..4]
    });

    if blocked_name || blocked_selector {
        return Err(BotError::Config(format!(
            "direct mint function `{}` is blocked by the signing policy because it can approve, move, or generically execute assets",
            function.signature()
        )));
    }
    Ok(())
}

pub fn verify_dotenv_permissions(path: &Path) -> Result<()> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(BotError::Config(
                ".env permissions are too open; run `chmod 600 .env` before starting the bot"
                    .to_string(),
            ));
        }
    }

    Ok(())
}

pub fn sanitize_external_text(value: &str, maximum_characters: usize) -> String {
    let characters = value.trim().chars().collect::<Vec<_>>();
    let mut sanitized = String::new();
    let mut index = 0;

    while index < characters.len() {
        if let Some(scheme_length) = url_scheme_length(&characters, index) {
            sanitized.push_str(REDACTED_URL);
            index += scheme_length;
            while index < characters.len()
                && !characters[index].is_whitespace()
                && !matches!(characters[index], '"' | '\'' | '<' | '>')
            {
                index += 1;
            }
            continue;
        }

        if looks_like_address(&characters, index) {
            sanitized.push_str(REDACTED_ADDRESS);
            index += 42;
            continue;
        }

        let character = characters[index];
        sanitized.push(if character.is_control() {
            ' '
        } else {
            character
        });
        index += 1;
    }

    let mut output = sanitized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(maximum_characters)
        .collect::<String>();
    if sanitized.chars().count() > maximum_characters {
        output.push_str("...");
    }
    output
}

pub fn summarize_rpc_error(error: &str) -> String {
    const SAFE_MARKERS: [&str; 3] = [
        "server returned an error response",
        "error code",
        "execution reverted",
    ];

    let lowercase = error.to_ascii_lowercase();
    let safe_start = SAFE_MARKERS
        .iter()
        .filter_map(|marker| lowercase.find(marker))
        .min();
    safe_start.map_or_else(
        || "transport request failed (details redacted)".to_string(),
        |start| sanitize_external_text(&error[start..], 384),
    )
}

fn url_scheme_length(characters: &[char], index: usize) -> Option<usize> {
    ["https://", "http://", "wss://", "ws://"]
        .into_iter()
        .find(|scheme| {
            characters[index..]
                .iter()
                .take(scheme.len())
                .copied()
                .eq(scheme.chars())
        })
        .map(str::len)
}

fn looks_like_address(characters: &[char], index: usize) -> bool {
    let remaining = &characters[index..];
    remaining.len() >= 42
        && remaining[0] == '0'
        && matches!(remaining[1], 'x' | 'X')
        && remaining[2..42]
            .iter()
            .all(|character| character.is_ascii_hexdigit())
        && remaining
            .get(42)
            .is_none_or(|character| !character.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::{sanitize_external_text, summarize_rpc_error, validate_direct_mint_function};
    use alloy::json_abi::Function;

    #[test]
    fn direct_signing_policy_blocks_asset_movement_and_generic_execution() {
        for signature in [
            "approve(address,uint256)",
            "setApprovalForAll(address,bool)",
            "safeTransferFrom(address,address,uint256)",
            "permit(address,address,uint256,uint256,uint8,bytes32,bytes32)",
            "execute(address,uint256,bytes)",
            "multicall(bytes[])",
            "mintWithPermit(uint256,bytes)",
        ] {
            let function = Function::parse(signature).expect("valid test signature");
            assert!(
                validate_direct_mint_function(&function).is_err(),
                "{signature} should be blocked"
            );
        }
    }

    #[test]
    fn direct_signing_policy_keeps_custom_mint_and_claim_calls() {
        for signature in [
            "mint(uint256)",
            "publicMint(address,uint256)",
            "claim(uint256,bytes32[])",
            "purchase(uint256)",
        ] {
            let function = Function::parse(signature).expect("valid test signature");
            validate_direct_mint_function(&function)
                .unwrap_or_else(|error| panic!("{signature} should be allowed: {error}"));
        }
    }

    #[test]
    fn redacts_urls_addresses_and_terminal_controls() {
        let value = "\u{1b}[31mfailed https://rpc.example/v2/private-key for \
                     0x1111111111111111111111111111111111111111\nnext";
        let sanitized = sanitize_external_text(value, 512);
        assert!(!sanitized.contains("private-key"));
        assert!(!sanitized.contains("111111111111"));
        assert!(!sanitized.contains('\u{1b}'));
        assert!(sanitized.contains("[redacted URL]"));
        assert!(sanitized.contains("[redacted address]"));
    }

    #[test]
    fn keeps_json_rpc_reason_but_drops_transport_url() {
        let rpc = summarize_rpc_error(
            "request to https://rpc.example/key failed: server returned an error response: error code 3: execution reverted",
        );
        assert_eq!(
            rpc,
            "server returned an error response: error code 3: execution reverted"
        );

        let transport = summarize_rpc_error("request to https://rpc.example/key timed out");
        assert_eq!(transport, "transport request failed (details redacted)");
    }

    #[test]
    fn truncates_unicode_on_character_boundaries() {
        let sanitized = sanitize_external_text(&"é".repeat(600), 512);
        assert_eq!(sanitized.chars().count(), 515);
        assert!(sanitized.ends_with("..."));
    }
}
