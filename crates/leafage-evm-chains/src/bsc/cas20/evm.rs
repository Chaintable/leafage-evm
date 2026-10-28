//! Running CAS20 tokens inside revm.
//!
//! [`JournalHost`] implements [`Cas20Host`] over alloy-evm's [`EvmInternals`], the journal a
//! dynamic precompile receives, and [`token_precompile`] is the precompile `BscEvm` routes
//! every CAS20 token address to from Jenner on.

use alloy::primitives::{Address, Log, B256, U256};
use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_evm::EvmInternals;
use revm::precompile::{PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput};
use std::borrow::Cow;

use super::frame::{Cas20Host, Frame};
use super::{run_token, Outcome};

/// [`Cas20Host`] over the live revm journal.
///
/// revm's journal loads an account before touching its storage, so reading a registry the
/// transaction has not touched yet is safe (the failure mode Base's B20 port hit on revm 36).
struct JournalHost<'a, 'b> {
    internals: &'a mut EvmInternals<'b>,
    fatal: Option<String>,
}

impl JournalHost<'_, '_> {
    fn fail(&mut self, err: impl std::fmt::Display) {
        if self.fatal.is_none() {
            self.fatal = Some(err.to_string());
        }
    }
}

impl Cas20Host for JournalHost<'_, '_> {
    fn access_account(&mut self, address: Address) -> bool {
        match self.internals.load_account(address) {
            Ok(loaded) => !loaded.is_cold,
            Err(err) => {
                self.fail(err);
                true
            }
        }
    }

    fn code_hash(&mut self, address: Address) -> B256 {
        match self.internals.load_account(address) {
            Ok(loaded) => loaded.data.info.code_hash,
            Err(err) => {
                self.fail(err);
                B256::ZERO
            }
        }
    }

    fn access_slot(&mut self, address: Address, slot: B256) -> bool {
        match self.internals.sload(address, slot.into()) {
            Ok(loaded) => !loaded.is_cold,
            Err(err) => {
                self.fail(err);
                true
            }
        }
    }

    fn state(&mut self, address: Address, slot: B256) -> B256 {
        match self.internals.sload(address, slot.into()) {
            Ok(loaded) => loaded.data.into(),
            Err(err) => {
                self.fail(err);
                B256::ZERO
            }
        }
    }

    fn state_and_committed(&mut self, address: Address, slot: B256) -> (B256, B256) {
        let key: U256 = slot.into();
        let current = self.state(address, slot);
        let original = match self.internals.load_account(address) {
            Ok(loaded) => loaded
                .data
                .storage
                .get(&key)
                .map(|s| s.original_value.into())
                .unwrap_or(current),
            Err(err) => {
                self.fail(err);
                current
            }
        };
        (current, original)
    }

    fn set_state(&mut self, address: Address, slot: B256, value: B256) {
        if let Err(err) = self.internals.sstore(address, slot.into(), value.into()) {
            self.fail(err);
        }
    }

    fn add_log(&mut self, log: Log) {
        self.internals.log(log);
    }

    fn take_fatal(&mut self) -> Option<String> {
        self.fatal.take()
    }
}

/// The precompile every CAS20 token address dispatches to.
pub(crate) fn token_precompile() -> DynPrecompile {
    DynPrecompile::new_stateful(
        PrecompileId::Custom(Cow::Borrowed("BSC_CAS20_TOKEN")),
        |input: PrecompileInput<'_>| {
            let PrecompileInput {
                data,
                gas,
                reservoir,
                caller,
                value,
                target_address,
                is_static,
                bytecode_address,
                mut internals,
            } = input;
            let block_time = internals.block_timestamp().as_limbs()[0];
            let chain_id = internals.chain_id();
            let mut host = JournalHost {
                internals: &mut internals,
                fatal: None,
            };
            // CALLCODE and DELEGATECALL run the token's code against another account.
            let direct_call = target_address == bytecode_address;
            let mut frame = Frame::new(
                &mut host,
                bytecode_address,
                caller,
                is_static,
                direct_call,
                value,
                gas,
                block_time,
                chain_id,
            );
            let outcome = run_token(&mut frame, data);
            let used = gas - frame.gas_remaining();
            let refunded = frame.gas_refunded();
            if let Some(err) = host.take_fatal() {
                return Err(PrecompileError::Fatal(err));
            }
            Ok(match outcome {
                Outcome::Return(bytes) => {
                    let mut output = PrecompileOutput::new(used, bytes.into(), reservoir);
                    output.gas_refunded = refunded;
                    output
                }
                Outcome::Revert(bytes) => PrecompileOutput::revert(used, bytes.into(), reservoir),
                Outcome::OutOfGas => PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir),
            })
        },
    )
}
