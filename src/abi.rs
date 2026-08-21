use crate::{
    config::MintCallConfig,
    error::{BotError, Result},
};
use alloy::{
    dyn_abi::{DynSolType, DynSolValue, JsonAbiExt, Specifier},
    json_abi::{Event, Function},
    primitives::{Address, U256},
};

// Compile-time bindings are useful for known contracts. Generic collection
// mode below intentionally stays dynamic so changing a collection does not
// require recompiling the bot.
alloy::sol! {
    interface MockMint {
        function publicSaleActive() external view returns (bool);
        function salePhase() external view returns (uint256);
        function mint(uint256 quantity) external payable;
    }
}

#[derive(Debug, Clone)]
pub struct PreparedCalldata {
    pub function: Function,
    pub bytes: Vec<u8>,
}

pub fn parse_function(signature: &str) -> Result<Function> {
    Function::parse(signature).map_err(|err| BotError::Abi(format!("{signature}: {err}")))
}

pub fn encode_mint(
    mint: &MintCallConfig,
    quantity: u64,
    wallet: Address,
    proof: Option<&[String]>,
) -> Result<PreparedCalldata> {
    let function = parse_function(&mint.function)?;
    if function.inputs.len() != mint.arguments.len() {
        return Err(BotError::Abi(format!(
            "{} expects {} arguments but configuration supplied {}",
            function.signature(),
            function.inputs.len(),
            mint.arguments.len()
        )));
    }

    let mut values = Vec::with_capacity(function.inputs.len());
    for (param, raw) in function.inputs.iter().zip(&mint.arguments) {
        let ty = param.resolve().map_err(|err| {
            BotError::Abi(format!("could not resolve parameter `{param}`: {err}"))
        })?;
        let expanded = expand_placeholder(raw, quantity, wallet, proof);
        let value = coerce_argument(&ty, &expanded).map_err(|err| {
            BotError::Abi(format!("could not encode argument `{raw}` as {ty}: {err}"))
        })?;
        values.push(value);
    }

    let bytes = function
        .abi_encode_input(&values)
        .map_err(|err| BotError::Abi(format!("{}: {err}", function.signature())))?;
    Ok(PreparedCalldata { function, bytes })
}

pub fn encode_view_call(signature: &str) -> Result<(Function, Vec<u8>)> {
    let function = parse_function(signature)?;
    if !function.inputs.is_empty() {
        return Err(BotError::Trigger(format!(
            "trigger function `{signature}` has arguments; configure a zero-argument view"
        )));
    }
    let bytes = function
        .abi_encode_input(&[])
        .map_err(|err| BotError::Abi(format!("{signature}: {err}")))?;
    Ok((function, bytes))
}

pub fn parse_event(signature: &str) -> Result<Event> {
    Event::parse(signature).map_err(|err| BotError::Trigger(format!("{signature}: {err}")))
}

fn expand_placeholder(
    raw: &str,
    quantity: u64,
    wallet: Address,
    proof: Option<&[String]>,
) -> String {
    let mut value = raw.replace("$quantity", &quantity.to_string());
    value = value.replace("$wallet", &format!("{wallet:#x}"));
    if value.contains("$proof") {
        let proof_text = proof
            .unwrap_or_default()
            .iter()
            .map(|item| item.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        value = value.replace("$proof", &format!("[{proof_text}]"));
    }
    value
}

fn coerce_argument(ty: &DynSolType, raw: &str) -> std::result::Result<DynSolValue, String> {
    if matches!(ty, DynSolType::Address) {
        return raw
            .parse::<Address>()
            .map(DynSolValue::Address)
            .map_err(|err| err.to_string());
    }
    if let DynSolType::Uint(bits) = ty {
        return raw
            .parse::<U256>()
            .map(|value| DynSolValue::Uint(value, *bits))
            .map_err(|err| err.to_string());
    }
    if let DynSolType::Bool = ty {
        return raw
            .parse::<bool>()
            .map(DynSolValue::Bool)
            .map_err(|err| err.to_string());
    }
    ty.coerce_str(raw).map_err(|err| err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_quantity_and_wallet_placeholders() {
        let call = MintCallConfig {
            function: "mint(address,uint256)".to_string(),
            arguments: vec!["$wallet".to_string(), "$quantity".to_string()],
            proof: None,
            price_per_nft: "0".to_string(),
        };
        let wallet = "0x0000000000000000000000000000000000000001"
            .parse()
            .unwrap();
        let prepared = encode_mint(&call, 2, wallet, None).unwrap();
        assert_eq!(prepared.bytes.len(), 4 + 64);
        assert_eq!(prepared.function.signature(), "mint(address,uint256)");
    }
}
