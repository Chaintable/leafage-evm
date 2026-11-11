use alloy::primitives::B256;
use md5::{Digest, Md5};
use primitive_types::{H160, U256};
use zksync_vm2::interface::{
    CallframeInterface, CallingMode, Opcode, OpcodeType, ReturnType, ShouldStop, StateInterface,
    Tracer,
};
use zksync_vm2::FatPointer;

use super::constants::{CONTRACT_DEPLOYER_ADDRESS, EVENT_WRITER_ADDRESS};
use leafage_evm_types::{DebankEvent, DebankTrace};

pub const PARENT_CALL_FAILED_ERROR: &str = "parent call failed";

#[derive(Debug, Clone, Default)]
pub struct VmEvent {
    pub address: H160,
    pub indexed_topics: Vec<B256>,
    pub value: Vec<u8>,
    pub position: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallType {
    Call(CallingMode),
    Create,
    Near,
}

#[derive(Debug, Clone)]
pub struct Call {
    pub r#type: CallType,
    pub from: H160,
    pub to: H160,
    pub parent_gas: u64,
    pub gas: u64,
    pub gas_used: u64,
    pub value: U256,
    pub input: Vec<u8>,
    pub output: Vec<u8>,
    pub error: Option<String>,
    pub revert_reason: Option<String>,
    pub calls: Vec<Call>,
    pub events: Vec<VmEvent>,
    pub trace_id: String,
    pub parent_trace_id: Option<String>,
    pub pos_in_parent_trace: usize,
    pub self_storage_change: bool,
    pub storage_change: bool,
    pub parent_failed: bool,
}

impl Default for Call {
    fn default() -> Self {
        Self {
            r#type: CallType::Call(CallingMode::Normal),
            from: H160::zero(),
            to: H160::zero(),
            parent_gas: 0,
            gas: 0,
            gas_used: 0,
            value: U256::zero(),
            input: Vec::new(),
            output: Vec::new(),
            error: None,
            revert_reason: None,
            calls: Vec::new(),
            events: Vec::new(),
            trace_id: String::new(),
            parent_trace_id: None,
            pos_in_parent_trace: 0,
            self_storage_change: false,
            storage_change: false,
            parent_failed: false,
        }
    }
}

pub fn to_hash(args: &[&str]) -> String {
    let mut hasher = Md5::new();
    for arg in args {
        hasher.update(arg.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

pub fn decode_vm_revert_reason(bytes: &[u8]) -> String {
    if bytes.len() >= 4 && &bytes[0..4] == &[0x08, 0xc3, 0x79, 0xa0] {
        let rest = &bytes[4..];
        if rest.len() >= 64 {
            let offset_bytes: [u8; 32] = rest[0..32].try_into().unwrap();
            let offset = primitive_types::U256::from_big_endian(&offset_bytes).as_usize();
            if offset >= 32 && offset < rest.len() {
                let data = &rest[offset..];
                if data.len() >= 32 {
                    let len_bytes: [u8; 32] = data[0..32].try_into().unwrap();
                    let str_len = primitive_types::U256::from_big_endian(&len_bytes).as_usize();
                    if 32 + str_len <= data.len() {
                        let str_bytes = &data[32..32 + str_len];
                        return String::from_utf8_lossy(str_bytes).to_string();
                    }
                }
            }
        }
    }
    if bytes.is_empty() {
        "Error function_selector = 0x, data = 0x".to_string()
    } else {
        let hex_data = alloy::primitives::hex::encode(bytes);
        if bytes.len() >= 4 {
            let sel = alloy::primitives::hex::encode(&bytes[0..4]);
            format!("Error function_selector = 0x{}, data = 0x{}", sel, hex_data)
        } else {
            format!("Error function_selector = 0x{}, data = 0x", hex_data)
        }
    }
}

pub fn call_failed(call: &Call) -> bool {
    call.revert_reason.is_some() || call.error.is_some()
}

pub fn effective_failed(call: &Call) -> bool {
    call_failed(call) || call.parent_failed
}

pub fn child_trace_address(a: &[u32], i: u32) -> Vec<u32> {
    let mut child = Vec::with_capacity(a.len() + 1);
    child.extend_from_slice(a);
    child.push(i);
    child
}

pub fn set_parent_failed(cf: &mut Call, parent_failed: bool) {
    let failed = call_failed(cf) || parent_failed;
    for subcall in &mut cf.calls {
        subcall.parent_failed = failed;
        set_parent_failed(subcall, failed);
    }
}

fn trace_error(call: &Call) -> String {
    if let Some(error) = &call.revert_reason {
        return error.clone();
    }
    if let Some(error) = &call.error {
        return error.clone();
    }
    if call.parent_failed {
        return PARENT_CALL_FAILED_ERROR.to_owned();
    }
    String::new()
}

pub fn to_debank_trace(call: &Call, tx_hash: B256, trace_addresses: Vec<u32>) -> DebankTrace {
    let (call_create_type, call_type) = match call.r#type {
        CallType::Create => ("create".to_string(), "".to_string()),
        CallType::Call(CallingMode::Normal | CallingMode::Mimic) => {
            ("call".to_string(), "call".to_string())
        }
        CallType::Call(CallingMode::Delegate) => {
            ("call".to_string(), "delegatecall".to_string())
        }
        CallType::Near => ("call".to_string(), "call".to_string()),
    };

    DebankTrace {
        id: call.trace_id.clone(),
        from_addr: alloy::primitives::Address::from_slice(call.from.as_bytes()),
        gas_limit: call.gas,
        input: alloy::primitives::Bytes::from(call.input.clone()),
        to_addr: alloy::primitives::Address::from_slice(call.to.as_bytes()),
        value: alloy::primitives::U256::from_be_bytes({
            let mut b = [0u8; 32];
            call.value.to_big_endian(&mut b);
            b
        }),
        gas_used: call.gas_used,
        output: alloy::primitives::Bytes::from(call.output.clone()),
        call_create_type,
        call_type,
        tx_id: tx_hash,
        parent_trace_id: call.parent_trace_id.clone().unwrap_or_default(),
        pos_in_parent_trace: call.pos_in_parent_trace,
        self_storage_change: call.self_storage_change,
        storage_change: call.storage_change,
        subtraces: Some(call.calls.len() as u32),
        trace_address: Some(trace_addresses),
        error: trace_error(call),
    }
}

pub fn add_trace_log(
    tx_hash: B256,
    outtraces: &mut Vec<DebankTrace>,
    outerrtraces: &mut Vec<DebankTrace>,
    outevents: &mut Vec<DebankEvent>,
    outerrevents: &mut Vec<DebankEvent>,
    trace_addresses: Vec<u32>,
    cf: &mut Call,
) {
    for (i, subcall) in cf.calls.iter_mut().enumerate() {
        subcall.parent_trace_id = Some(cf.trace_id.clone());
        subcall.trace_id = to_hash(&[
            &tx_hash.to_string(),
            &cf.trace_id,
            &subcall.pos_in_parent_trace.to_string(),
        ]);
        add_trace_log(
            tx_hash,
            outtraces,
            outerrtraces,
            outevents,
            outerrevents,
            child_trace_address(&trace_addresses, i as u32),
            subcall,
        );
    }

    for vm_event in &cf.events {
        let pos_in_parent_trace = vm_event.position;
        let e = DebankEvent {
            id: to_hash(&[&cf.trace_id, &pos_in_parent_trace.to_string()]),
            contract_id: alloy::primitives::Address::from_slice(vm_event.address.as_bytes()),
            selector: vm_event
                .indexed_topics
                .first()
                .map(|t| format!("{:#x}", t))
                .unwrap_or_default(),
            topics: vm_event
                .indexed_topics
                .iter()
                .skip(1)
                .map(|t| format!("{:#x}", t))
                .collect(),
            data: alloy::primitives::Bytes::from(vm_event.value.clone()),
            tx_id: tx_hash,
            parent_trace_id: cf.trace_id.clone(),
            pos_in_parent_trace,
            log_index: None,
        };

        if effective_failed(cf) {
            outerrevents.push(e);
        } else {
            outevents.push(e);
        }
    }

    for (i, subcall) in cf.calls.iter().enumerate() {
        let debank_trace = to_debank_trace(
            subcall,
            tx_hash,
            child_trace_address(&trace_addresses, i as u32),
        );
        if effective_failed(subcall) {
            outerrtraces.push(debank_trace);
        } else {
            outtraces.push(debank_trace);
        }
    }
}

pub fn merge_events_to_vm_event(events: &[zksync_vm2::interface::Event]) -> Vec<VmEvent> {
    let mut result = Vec::new();
    let mut current: Option<(usize, u32, Vec<[u8; 32]>, Vec<u8>)> = None;

    for message in events {
        if !message.is_first {
            if let Some((mut remaining_data_length, mut remaining_topics, mut topics, mut data)) =
                current.take()
            {
                let mut data_0 = [0u8; 32];
                let mut data_1 = [0u8; 32];
                message.key.to_big_endian(&mut data_0);
                message.value.to_big_endian(&mut data_1);
                for el in [&data_0, &data_1] {
                    if remaining_topics != 0 {
                        topics.push(*el);
                        remaining_topics -= 1;
                    } else if remaining_data_length != 0 {
                        if remaining_data_length >= 32 {
                            data.extend_from_slice(el);
                            remaining_data_length -= 32;
                        } else {
                            data.extend_from_slice(&el[..remaining_data_length]);
                            remaining_data_length = 0;
                        }
                    }
                }

                if remaining_data_length != 0 || remaining_topics != 0 {
                    current = Some((remaining_data_length, remaining_topics, topics, data));
                } else if !topics.is_empty() {
                    let address = H160::from_slice(&topics[0][12..32]);
                    let indexed_topics = topics.into_iter().skip(1).map(B256::from).collect();
                    result.push(VmEvent {
                        address,
                        indexed_topics,
                        value: data,
                        position: 0,
                    });
                }
            }
        } else {
            if let Some((remaining_data_length, remaining_topics, topics, data)) = current.take() {
                if remaining_data_length == 0 && remaining_topics == 0 && !topics.is_empty() {
                    let address = H160::from_slice(&topics[0][12..32]);
                    let indexed_topics = topics.into_iter().skip(1).map(B256::from).collect();
                    result.push(VmEvent {
                        address,
                        indexed_topics,
                        value: data,
                        position: 0,
                    });
                }
            }

            let num_topics = message.key.low_u64() as u32;
            let data_length = (message.key.low_u64() >> 32) as usize;

            let mut buffer = [0u8; 32];
            message.value.to_big_endian(&mut buffer);

            let (topics, data, remaining_topics, remaining_data_length) =
                if num_topics == 0 && data_length == 0 {
                    (vec![], vec![], 0, 0)
                } else if num_topics == 0 {
                    let take_len = 32.min(data_length);
                    (vec![], buffer[..take_len].to_vec(), 0, data_length - take_len)
                } else {
                    (vec![buffer], vec![], num_topics - 1, data_length)
                };

            if remaining_data_length == 0 && remaining_topics == 0 && !topics.is_empty() {
                let address = H160::from_slice(&topics[0][12..32]);
                let indexed_topics = topics.into_iter().skip(1).map(B256::from).collect();
                result.push(VmEvent {
                    address,
                    indexed_topics,
                    value: data,
                    position: 0,
                });
            } else {
                current = Some((remaining_data_length, remaining_topics, topics, data));
            }
        }
    }

    if let Some((remaining_data_length, remaining_topics, topics, data)) = current.take() {
        if remaining_data_length == 0 && remaining_topics == 0 && !topics.is_empty() {
            let address = H160::from_slice(&topics[0][12..32]);
            let indexed_topics = topics.into_iter().skip(1).map(B256::from).collect();
            result.push(VmEvent {
                address,
                indexed_topics,
                value: data,
                position: 0,
            });
        }
    }

    result
}

#[derive(Debug, Clone)]
struct FarcallAndNearCallCount {
    farcall: Call,
    near_calls_after: usize,
    events_at_call_start: usize,
}

#[derive(Debug, Clone, Default)]
pub struct EraCallTracer {
    stack: Vec<FarcallAndNearCallCount>,
    pub finished_calls: Vec<Call>,
}

fn read_fat_pointer(state: &impl StateInterface, fp: FatPointer) -> Vec<u8> {
    if fp.offset > fp.length {
        return Vec::new();
    }
    (fp.start + fp.offset..fp.start + fp.length)
        .map(|addr| state.read_heap_byte(fp.memory_page, addr))
        .collect()
}

impl Tracer for EraCallTracer {
    fn after_instruction<OP: OpcodeType, S: StateInterface>(
        &mut self,
        state: &mut S,
    ) -> ShouldStop {
        match OP::VALUE {
            Opcode::FarCall(ty) => {
                let current_gas = state.current_frame().gas() as u64;
                let from = state.current_frame().caller();
                let to = state.current_frame().address();
                let input = if current_gas == 0 {
                    Vec::new()
                } else {
                    let (raw_ptr, _) = state.read_register(1);
                    let fp = FatPointer::from(raw_ptr);
                    read_fat_pointer(state, fp)
                };
                let value = U256::from(state.current_frame().context_u128());
                let call_type = match ty {
                    CallingMode::Normal => CallType::Call(CallingMode::Normal),
                    CallingMode::Delegate => CallType::Call(CallingMode::Delegate),
                    CallingMode::Mimic => {
                        let prev_this = state.callframe(1).address();
                        if prev_this == CONTRACT_DEPLOYER_ADDRESS {
                            CallType::Create
                        } else {
                            CallType::Call(CallingMode::Mimic)
                        }
                    }
                };

                let events_at_call_start = state.events().count();

                self.stack.push(FarcallAndNearCallCount {
                    farcall: Call {
                        r#type: call_type,
                        from,
                        to,
                        parent_gas: current_gas + state.callframe(1).gas() as u64,
                        gas: current_gas,
                        input,
                        value,
                        ..Default::default()
                    },
                    near_calls_after: 0,
                    events_at_call_start,
                });
            }
            Opcode::NearCall => {
                if let Some(frame) = self.stack.last_mut() {
                    frame.near_calls_after += 1;
                }
            }
            Opcode::StorageWrite | Opcode::TransientStorageWrite => {
                if let Some(frame) = self.stack.last_mut() {
                    frame.farcall.self_storage_change = true;
                    frame.farcall.storage_change = true;
                }
            }
            Opcode::Ret(variant) => {
                let Some(mut current_call) = self.stack.pop() else {
                    return ShouldStop::Continue;
                };

                if current_call.near_calls_after == 0 {
                    current_call.farcall.gas_used = current_call
                        .farcall
                        .parent_gas
                        .saturating_sub(state.current_frame().gas() as u64);

                    let (maybe_output_ptr, is_pointer) = state.read_register(1);
                    let output = if is_pointer {
                        let output_ptr = FatPointer::from(maybe_output_ptr);
                        if output_ptr.length == 0 && output_ptr.offset == 0 {
                            None
                        } else {
                            Some(read_fat_pointer(state, output_ptr))
                        }
                    } else {
                        None
                    };

                    match variant {
                        ReturnType::Normal => {
                            current_call.farcall.output = output.unwrap_or_default();
                            if current_call.farcall.to == EVENT_WRITER_ADDRESS {
                                let new_events: Vec<_> = state
                                    .events()
                                    .skip(current_call.events_at_call_start)
                                    .collect();
                                current_call.farcall.events = merge_events_to_vm_event(&new_events);
                            }
                        }
                        ReturnType::Revert => {
                            current_call.farcall.revert_reason = Some(if let Some(output) = &output {
                                decode_vm_revert_reason(output)
                            } else {
                                "Unknown revert reason".to_string()
                            });
                        }
                        ReturnType::Panic => {
                            current_call.farcall.error = Some("Panic".to_string());
                        }
                    }

                    if let Some(parent_call) = self.stack.last_mut() {
                        if current_call.farcall.storage_change {
                            parent_call.farcall.storage_change = true;
                        }
                        current_call.farcall.pos_in_parent_trace = parent_call.farcall.calls.len();
                        parent_call.farcall.calls.push(current_call.farcall);
                    } else {
                        current_call.farcall.pos_in_parent_trace = self.finished_calls.len();
                        self.finished_calls.push(current_call.farcall);
                    }
                } else {
                    current_call.near_calls_after -= 1;
                    self.stack.push(current_call);
                }
            }
            _ => {}
        }

        ShouldStop::Continue
    }
}
