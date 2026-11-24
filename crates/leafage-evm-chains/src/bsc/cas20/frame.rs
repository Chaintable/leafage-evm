//! The execution environment of one CAS20 call frame.
//!
//! Port of go-bsc `PrecompileContext` (`core/vm/contracts_stateful.go`) and the charges in
//! `core/vm/cas20_gas.go`. Every charge mirrors an EVM cost function, so nothing a CAS20 call
//! does is cheaper than the same work through bytecode (BEP-702 3.14).
//!
//! Out-of-gas and write protection are sticky flags, exactly as in go-bsc: a refused charge
//! marks the frame and the logic keeps running on zero values, and the exit
//! ([`super::finish_metered`]) lets the flags outrank whatever the logic returned. Keeping
//! the flag model (rather than early-returning a `Result`) preserves go-bsc's precedence when
//! both happen in one frame, e.g. a write attempted in a static frame after gas ran out.

use alloy::primitives::{keccak256, Address, Log, LogData, B256, U256};

/// Gas schedule, in go-bsc `params` names.
pub(crate) mod gas {
    pub const LOG: u64 = 375;
    pub const LOG_TOPIC: u64 = 375;
    pub const LOG_DATA: u64 = 8;
    pub const KECCAK256: u64 = 30;
    pub const KECCAK256_WORD: u64 = 6;
    pub const COPY: u64 = 3;
    pub const MEMORY: u64 = 3;
    pub const WARM_STORAGE_READ: u64 = 100;
    pub const COLD_ACCOUNT_ACCESS: u64 = 2600;
    pub const COLD_SLOAD: u64 = 2100;
    pub const SSTORE_SET: u64 = 20_000;
    pub const SSTORE_RESET: u64 = 5_000;
    pub const SSTORE_SENTRY: u64 = 2_300;
    pub const TX_ACCESS_LIST_STORAGE_KEY: u64 = 1_900;
    pub const SSTORE_CLEARS_SCHEDULE_REFUND: u64 =
        SSTORE_RESET - COLD_SLOAD + TX_ACCESS_LIST_STORAGE_KEY;
    pub const ECRECOVER: u64 = 3_000;

    /// `cas20CalldataWordGas`: what copying one calldata word into memory costs.
    pub const CALLDATA_WORD: u64 = COPY + MEMORY;
}

/// The state a CAS20 frame reads and writes, a subset of go-bsc's `StateDB`.
///
/// Methods are infallible: a database failure is recorded by the implementor and surfaces
/// once the call returns (see [`Cas20Host::take_fatal`]), so the ported logic stays a
/// line-by-line match of go-bsc.
pub trait Cas20Host {
    /// `AddressInAccessList`, then `AddAddressToAccessList`: returns whether `address` was
    /// already warm, and leaves it warm.
    fn access_account(&mut self, address: Address) -> bool;

    /// `GetCodeHash` (zero for a missing account).
    fn code_hash(&mut self, address: Address) -> B256;

    /// `SlotInAccessList`, then `AddSlotToAccessList`: returns whether the slot was already
    /// warm, and leaves it (and its account) warm.
    fn access_slot(&mut self, address: Address, slot: B256) -> bool;

    /// `GetState`.
    fn state(&mut self, address: Address, slot: B256) -> B256;

    /// `GetStateAndCommittedState`: the current value and the value at the start of the
    /// transaction.
    fn state_and_committed(&mut self, address: Address, slot: B256) -> (B256, B256);

    /// `SetState`.
    fn set_state(&mut self, address: Address, slot: B256, value: B256);

    /// `AddLog`.
    fn add_log(&mut self, log: Log);

    /// A database failure seen since the last call, if any.
    fn take_fatal(&mut self) -> Option<String>;
}

/// go-bsc `PrecompileContext` plus its `frameAccounting`.
pub struct Frame<'h> {
    pub(crate) host: &'h mut dyn Cas20Host,
    /// The callee, whose storage the call works on.
    pub self_address: Address,
    /// `msg.sender` of this frame.
    pub caller: Address,
    /// Inside a STATICCALL or any read-only ancestor frame.
    pub read_only: bool,
    /// False for CALLCODE and DELEGATECALL.
    pub direct_call: bool,
    /// Wei attached to the frame.
    pub value: U256,
    /// Block timestamp in seconds.
    pub block_time: u64,
    pub chain_id: u64,

    gas: u64,
    refund: i64,
    out_of_gas: bool,
    write_protected: bool,
    /// Per-frame, not stored: outside the bootstrap window the zero admin count freezes role
    /// mutation on its own (BEP-702 3.4).
    pub(crate) admin_renounced: bool,
}

impl<'h> Frame<'h> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        host: &'h mut dyn Cas20Host,
        self_address: Address,
        caller: Address,
        read_only: bool,
        direct_call: bool,
        value: U256,
        gas: u64,
        block_time: u64,
        chain_id: u64,
    ) -> Self {
        Self {
            host,
            self_address,
            caller,
            read_only,
            direct_call,
            value,
            block_time,
            chain_id,
            gas,
            refund: 0,
            out_of_gas: false,
            write_protected: false,
            admin_renounced: false,
        }
    }

    /// Gas left in the frame.
    pub fn gas_remaining(&self) -> u64 {
        self.gas
    }

    /// Net EIP-3529 refund the frame accrued.
    pub fn gas_refunded(&self) -> i64 {
        self.refund
    }

    pub(crate) fn add_refund(&mut self, gas: u64) {
        self.refund += gas as i64;
    }

    pub(crate) fn sub_refund(&mut self, gas: u64) {
        self.refund -= gas as i64;
    }

    /// `UseGas`: exhausts the budget when the charge cannot be covered, as the EVM does.
    fn use_gas(&mut self, cost: u64) -> bool {
        if self.gas < cost {
            self.gas = 0;
            return false;
        }
        self.gas -= cost;
        true
    }

    pub(crate) fn mark_out_of_gas(&mut self) {
        self.out_of_gas = true;
    }

    pub fn out_of_gas(&self) -> bool {
        self.out_of_gas
    }

    pub(crate) fn mark_write_protected(&mut self) {
        self.write_protected = true;
    }

    pub fn write_protection_violated(&self) -> bool {
        self.write_protected
    }

    /// `chargeGas`: where every CAS20 charge arrives. False means stop before the operation
    /// the charge pays for.
    pub(crate) fn charge_gas(&mut self, cost: u64) -> bool {
        if self.out_of_gas {
            return false;
        }
        if !self.use_gas(cost) {
            self.mark_out_of_gas();
            return false;
        }
        true
    }

    /// `chargeInternalDispatch`: a warm CALL plus copying the entry into the callee's input.
    pub(crate) fn charge_internal_dispatch(&mut self, call: &[u8]) -> bool {
        let words = (call.len() as u64).div_ceil(32);
        self.charge_gas(gas::WARM_STORAGE_READ + words.saturating_mul(gas::CALLDATA_WORD))
    }

    pub(crate) fn charge_calldata(&mut self, input: &[u8]) -> bool {
        let words = (input.len() as u64).div_ceil(32);
        if words == 0 {
            return true;
        }
        self.charge_gas(words.saturating_mul(gas::CALLDATA_WORD))
    }

    pub(crate) fn charge_keccak(&mut self, size: usize) -> bool {
        let words = (size as u64).div_ceil(32);
        self.charge_gas(gas::KECCAK256 + gas::KECCAK256_WORD.saturating_mul(words))
    }

    pub(crate) fn charge_log(&mut self, topics: usize, data_len: usize) -> bool {
        self.charge_gas(
            gas::LOG
                + gas::LOG_TOPIC * topics as u64
                + gas::LOG_DATA.saturating_mul(data_len as u64),
        )
    }

    /// Warmed before the charge, in the interpreter's order.
    pub(crate) fn charge_account_access(&mut self, address: Address) -> bool {
        if self.host.access_account(address) {
            return self.charge_gas(gas::WARM_STORAGE_READ);
        }
        self.charge_gas(gas::COLD_ACCOUNT_ACCESS)
    }

    /// EIP-2200's reentrancy guard: a CAS20 token writes state without SSTORE, so it applies
    /// the check itself.
    pub(crate) fn sstore_sentry(&self) -> bool {
        self.gas > gas::SSTORE_SENTRY
    }

    /// `AddLog`: its result must be honoured, since an Approval log fits inside the 2,300 gas
    /// the EIP-2200 sentry leaves behind.
    pub(crate) fn add_log(&mut self, topics: Vec<B256>, data: Vec<u8>) -> bool {
        if self.read_only {
            self.mark_write_protected();
            return false;
        }
        if !self.charge_log(topics.len(), data.len()) {
            return false;
        }
        let log = Log {
            address: self.self_address,
            data: LogData::new_unchecked(topics, data.into()),
        };
        self.host.add_log(log);
        true
    }
}

/// `crypto.Keccak256Hash` over the concatenation of `parts`.
pub(crate) fn keccak(parts: &[&[u8]]) -> B256 {
    let mut buf = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for part in parts {
        buf.extend_from_slice(part);
    }
    keccak256(buf)
}
