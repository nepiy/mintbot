use crate::{
    abi::{encode_view_call, parse_event},
    config::{MintConfig, MintTrigger},
    error::{BotError, Result},
    rpc::RpcClients,
};
use alloy::{
    consensus::BlockHeader,
    dyn_abi::FunctionExt,
    eips::BlockId,
    network::TransactionBuilder,
    primitives::{Address, B256, U256},
    rpc::types::{Filter, Header, TransactionRequest},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerObservation {
    NotReady,
    Ready,
}

pub struct TriggerEngine {
    contract: Address,
    trigger: MintTrigger,
    view_call: Option<(alloy::json_abi::Function, Vec<u8>)>,
    numeric_target: Option<U256>,
    event_filter: Option<Filter>,
    pending_event: Option<PendingEvent>,
}

#[derive(Debug, Clone, Copy)]
struct PendingEvent {
    block_number: u64,
    block_hash: Option<B256>,
}

impl TriggerEngine {
    pub fn new(config: &MintConfig) -> Result<Self> {
        let view_call = match &config.trigger {
            MintTrigger::BooleanContractState { function, .. }
            | MintTrigger::NumericPhase { function, .. } => Some(encode_view_call(function)?),
            _ => None,
        };
        let numeric_target = match &config.trigger {
            MintTrigger::NumericPhase { target_value, .. } => {
                Some(target_value.parse::<U256>().map_err(|err| {
                    BotError::Trigger(format!("invalid numeric phase `{target_value}`: {err}"))
                })?)
            }
            _ => None,
        };
        let event_filter = match &config.trigger {
            MintTrigger::ContractEvent { signature, .. } => {
                let event = parse_event(signature)?;
                Some(
                    Filter::new()
                        .address(config.contract()?)
                        .event(&event.signature()),
                )
            }
            _ => None,
        };
        Ok(Self {
            contract: config.contract()?,
            trigger: config.trigger.clone(),
            view_call,
            numeric_target,
            event_filter,
            pending_event: None,
        })
    }

    pub fn event_filter(&self) -> Option<Filter> {
        self.event_filter.clone()
    }

    pub fn trigger(&self) -> &MintTrigger {
        &self.trigger
    }

    pub fn observe_event(
        &mut self,
        block_number: Option<u64>,
        block_hash: Option<B256>,
        removed: bool,
    ) -> TriggerObservation {
        let MintTrigger::ContractEvent { confirmations, .. } = &self.trigger else {
            return TriggerObservation::NotReady;
        };
        if removed {
            if self
                .pending_event
                .is_some_and(|pending| pending.block_hash == block_hash)
            {
                self.pending_event = None;
            }
            return TriggerObservation::NotReady;
        }
        self.pending_event = block_number.map(|block_number| PendingEvent {
            block_number,
            block_hash,
        });
        let confirmations = confirmations.unwrap_or(0);
        if confirmations == 0 {
            return TriggerObservation::Ready;
        }
        TriggerObservation::NotReady
    }

    pub fn pending_event(&self) -> Option<(u64, Option<B256>)> {
        self.pending_event
            .map(|event| (event.block_number, event.block_hash))
    }

    pub fn clear_pending_event(&mut self) {
        self.pending_event = None;
    }

    pub async fn observe_block(
        &mut self,
        header: &Header,
        rpc: &RpcClients,
    ) -> Result<TriggerObservation> {
        match &self.trigger {
            MintTrigger::BlockTimestamp { timestamp } => Ok(if header.timestamp() >= *timestamp {
                TriggerObservation::Ready
            } else {
                TriggerObservation::NotReady
            }),
            MintTrigger::BooleanContractState { expected_value, .. } => {
                let actual = self.read_bool(rpc, BlockId::hash(header.hash)).await?;
                Ok(if actual == *expected_value {
                    TriggerObservation::Ready
                } else {
                    TriggerObservation::NotReady
                })
            }
            MintTrigger::NumericPhase { .. } => {
                let target = self.numeric_target.ok_or_else(|| {
                    BotError::Trigger("numeric phase target was not prepared".to_string())
                })?;
                let actual = self.read_uint(rpc, BlockId::hash(header.hash)).await?;
                Ok(if actual == target {
                    TriggerObservation::Ready
                } else {
                    TriggerObservation::NotReady
                })
            }
            MintTrigger::ContractEvent { confirmations, .. } => {
                let Some(event) = self.pending_event else {
                    return Ok(TriggerObservation::NotReady);
                };
                let needed = event
                    .block_number
                    .saturating_add(confirmations.unwrap_or(0));
                Ok(if header.number() >= needed {
                    TriggerObservation::Ready
                } else {
                    TriggerObservation::NotReady
                })
            }
            MintTrigger::Manual => Ok(TriggerObservation::NotReady),
        }
    }

    async fn read_bool(&self, rpc: &RpcClients, block: BlockId) -> Result<bool> {
        let (function, calldata) = self
            .view_call
            .as_ref()
            .ok_or_else(|| BotError::Trigger("boolean view call was not prepared".to_string()))?;
        let output = rpc
            .call_at(
                TransactionRequest::default()
                    .with_to(self.contract)
                    .with_input(calldata.clone()),
                block,
            )
            .await
            .map_err(|err| BotError::Rpc(err.to_string()))?;
        let values = function
            .abi_decode_output(&output)
            .map_err(|err| BotError::Trigger(format!("could not decode boolean state: {err}")))?;
        match values.first() {
            Some(alloy::dyn_abi::DynSolValue::Bool(value)) => Ok(*value),
            _ => Err(BotError::Trigger(format!(
                "{} did not return a bool",
                function.signature()
            ))),
        }
    }

    async fn read_uint(&self, rpc: &RpcClients, block: BlockId) -> Result<U256> {
        let (function, calldata) = self
            .view_call
            .as_ref()
            .ok_or_else(|| BotError::Trigger("numeric view call was not prepared".to_string()))?;
        let output = rpc
            .call_at(
                TransactionRequest::default()
                    .with_to(self.contract)
                    .with_input(calldata.clone()),
                block,
            )
            .await
            .map_err(|err| BotError::Rpc(err.to_string()))?;
        let values = function
            .abi_decode_output(&output)
            .map_err(|err| BotError::Trigger(format!("could not decode numeric phase: {err}")))?;
        match values.first() {
            Some(alloy::dyn_abi::DynSolValue::Uint(value, _)) => Ok(*value),
            _ => Err(BotError::Trigger(format!(
                "{} did not return an unsigned integer",
                function.signature()
            ))),
        }
    }
}
