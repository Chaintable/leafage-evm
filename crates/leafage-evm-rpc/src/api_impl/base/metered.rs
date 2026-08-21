//! Gas-metered [`B20Port`] backed by the live revm journal.
//!
//! Port of Base reth's `EvmPrecompileStorageProvider`
//! (`base/crates/common/precompile-storage/src/evm.rs`) onto revm 36's `JournalTr` — the
//! only structural difference is that Base reaches state through `alloy_evm::EvmInternals`
//! while leafage holds the journal directly.
//!
//! All costs come from revm's own [`GasParams`] table for the active spec, driven by the
//! journal's cold/warm flags and original/present/new slot values. Nothing here hardcodes a
//! gas number, which is what makes the result track Base across hardforks rather than
//! matching it only at the fork it was written against.

use leafage_evm_types::{Address, U256};
use leafage_evm_chains::base::b20::{B20Error, B20Port, Result as B20Result};
use revm::context::JournalTr;
use revm::context_interface::cfg::GasParams;
use revm::interpreter::gas::Gas;
use revm::primitives::{Log, LogData};

/// Wraps the EVM journal so B20 logic reads and writes real state, paying real gas.
pub struct MeteredB20Port<'a, J: JournalTr> {
    journal: &'a mut J,
    gas: Gas,
    gas_params: GasParams,
    caller: Address,
    call_value: U256,
    chain_id: u64,
    timestamp: U256,
    is_static: bool,
}

impl<'a, J: JournalTr> MeteredB20Port<'a, J> {
    /// Builds a port over `journal` for a call with `gas_limit`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        journal: &'a mut J,
        gas_limit: u64,
        gas_params: GasParams,
        caller: Address,
        call_value: U256,
        chain_id: u64,
        timestamp: U256,
        is_static: bool,
    ) -> Self {
        Self {
            journal,
            gas: Gas::new(gas_limit),
            gas_params,
            caller,
            call_value,
            chain_id,
            timestamp,
            is_static,
        }
    }

    /// Gas consumed so far.
    pub fn gas_spent(&self) -> u64 {
        self.gas.spent()
    }

    /// Accumulated EIP-3529 refund, for revm to apply under the transaction-level cap.
    pub fn gas_refunded(&self) -> i64 {
        self.gas.refunded()
    }

    fn charge(&mut self, cost: u64) -> B20Result<()> {
        if self.gas.record_cost(cost) {
            Ok(())
        } else {
            Err(B20Error::OutOfGas)
        }
    }
}

impl<J: JournalTr> B20Port for MeteredB20Port<'_, J> {
    fn sload(&mut self, address: Address, key: U256) -> B20Result<U256> {
        let loaded = self
            .journal
            .sload(address, key)
            .map_err(|_| B20Error::Fatal("sload failed".to_string()))?;

        // EIP-2929: the warm cost is always paid; a cold slot pays the extra penalty on top.
        self.charge(self.gas_params.warm_storage_read_cost())?;
        if loaded.is_cold {
            self.charge(self.gas_params.cold_storage_additional_cost())?;
        }
        Ok(loaded.data)
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> B20Result<()> {
        if self.is_static {
            return Err(B20Error::StaticCallViolation);
        }
        // EIP-2200 reentrancy sentry: a frame left with only the 2300 call stipend must not
        // be able to write. Without this, a warm no-op rewrite (~100 gas) would succeed
        // where the SSTORE opcode would have halted.
        if self.gas.remaining() <= self.gas_params.call_stipend() {
            return Err(B20Error::OutOfGas);
        }

        let stored = self
            .journal
            .sstore(address, key, value)
            .map_err(|_| B20Error::Fatal("sstore failed".to_string()))?;

        self.charge(self.gas_params.sstore_static_gas())?;
        self.charge(self.gas_params.sstore_dynamic_gas(true, &stored.data, stored.is_cold))?;
        self.gas.record_refund(self.gas_params.sstore_refund(true, &stored.data));
        Ok(())
    }

    fn emit_event(&mut self, address: Address, log: LogData) -> B20Result<()> {
        if self.is_static {
            return Err(B20Error::StaticCallViolation);
        }
        let cost = revm::interpreter::gas::LOG
            + self.gas_params.log_cost(log.topics().len() as u8, log.data.len() as u64);
        self.charge(cost)?;
        self.journal.log(Log { address, data: log });
        Ok(())
    }

    fn has_code(&mut self, address: Address) -> B20Result<bool> {
        // `load_account` is enough: only the code hash is needed, and it is always
        // populated eagerly, so this avoids pulling bytecode out of the database.
        let (is_empty_code, is_cold) = {
            let loaded = self
                .journal
                .load_account(address)
                .map_err(|_| B20Error::Fatal("load_account failed".to_string()))?;
            (loaded.data.info.is_empty_code_hash(), loaded.is_cold)
        };

        self.charge(self.gas_params.warm_storage_read_cost())?;
        if is_cold {
            self.charge(self.gas_params.cold_account_additional_cost())?;
        }
        Ok(!is_empty_code)
    }

    fn deduct_gas(&mut self, gas: u64) -> B20Result<()> {
        self.charge(gas)
    }

    fn caller(&self) -> Address {
        self.caller
    }

    fn call_value(&self) -> U256 {
        self.call_value
    }

    fn chain_id(&self) -> u64 {
        self.chain_id
    }

    fn timestamp(&self) -> U256 {
        self.timestamp
    }

    fn is_static(&self) -> bool {
        self.is_static
    }
}
