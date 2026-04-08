//! Citrea L1 fee handler for estimating gas overhead from DA (data availability) costs.
//!
//! This module provides a custom revm Handler that tracks journal entries during EVM execution
//! to calculate the state diff size, which determines the L1 fee for posting transaction data.
//! The L1 fee is then converted into a gas overhead that gets added to the estimate.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::{Deref, DerefMut};

use alloy_evm::precompiles::PrecompilesMap;
use alloy_evm::Database;
use leafage_evm_types::{Address, BlockEnv, CfgEnv, MainnetSpecId, U256};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, ResultGas};
use revm::context::ContextSetters;
use revm::context::{Evm as EvmCtx, FrameStack};
use revm::context_interface::journaled_state::entry::JournalEntry;
use revm::context_interface::{ContextTr, JournalTr, LocalContextTr, Transaction};
use revm::handler::instructions::EthInstructions;
use revm::handler::{EthFrame, EvmTr, FrameResult, FrameTr, Handler, MainnetHandler};
use revm::inspector::{Inspector, InspectorEvmTr, InspectorHandler};
use revm::interpreter::interpreter::EthInterpreter;
use revm::interpreter::SuccessOrHalt;
use revm::primitives::KECCAK_EMPTY;
use revm::{Context, Journal};

use crate::citrea::precompile::CitreaPrecompiles;
use crate::citrea::CitreaHardfork;

// ── Compression / sizing constants ──────────────────────────────────

/// Brotli compression ratio numerator (48%).
const BROTLI_COMPRESSION_PERCENTAGE: u64 = 48;

/// Fixed overhead added after compression (bytes).
const BROTLI_EXTRA_BYTES: u64 = 2;

/// Weight for account-info diffs in the final size calculation (32%).
const ACCOUNT_DIFF_WEIGHT: u64 = 32;

/// Weight for storage diffs in the final size calculation (66%).
const STORAGE_DIFF_WEIGHT: u64 = 66;

/// Byte size of a single storage key+value diff entry (36-byte key + 32-byte value).
const STORAGE_ENTRY_SIZE: u64 = 68;

/// Byte size of an account-info diff for accounts without code (nonce + balance + flags).
const ACCOUNT_INFO_SIZE_NO_CODE: u64 = 41;

/// Byte size of an account-info diff for accounts with code (adds code_hash).
const ACCOUNT_INFO_SIZE_WITH_CODE: u64 = 73;

/// Fixed per-account overhead in the diff encoding.
const ACCOUNT_DIFF_OVERHEAD: u64 = 12;

/// Fixed byte cost for a newly created account (address + metadata).
const NEW_ACCOUNT_SIZE: u64 = 32;

// ── Chain extension context ─────────────────────────────────────────

/// Holds per-transaction L1 fee info collected during handler execution.
#[derive(Debug, Clone, Default)]
pub struct TxInfo {
    /// Raw L1 fee in wei, computed from diff_size * l1_fee_rate.
    pub l1_fee: U256,
    /// Estimated diff size in bytes (after compression weighting).
    pub diff_size: u64,
}

/// Chain extension stored in `Context.chain`.
/// Provides the L1 fee rate and collects tx-level L1 fee info.
#[derive(Debug, Clone, Default)]
pub struct CitreaChain {
    /// L1 fee rate (wei per byte of DA data).
    pub l1_fee_rate: u128,
    /// Per-transaction info populated after execution.
    pub tx_info: TxInfo,
}

// ── Context + EVM types ─────────────────────────────────────────────

/// Context type with CitreaChain as the chain extension.
pub type CitreaHandlerContext<DB> =
    Context<BlockEnv, revm::context::TxEnv, CfgEnv<MainnetSpecId>, DB, Journal<DB>, CitreaChain>;

/// Citrea handler EVM wrapping the revm EvmCtx with CitreaChain.
#[allow(missing_debug_implementations)]
pub struct CitreaHandlerEvm<DB: revm::database::Database, I> {
    pub inner: EvmCtx<
        CitreaHandlerContext<DB>,
        I,
        EthInstructions<EthInterpreter, CitreaHandlerContext<DB>>,
        PrecompilesMap,
        EthFrame,
    >,
}

impl<DB: Database, I> CitreaHandlerEvm<DB, I> {
    /// Creates a new [`CitreaHandlerEvm`].
    pub fn new(
        block_env: BlockEnv,
        cfg: CfgEnv<CitreaHardfork>,
        db: DB,
        inspector: I,
        l1_fee_rate: u128,
    ) -> Self {
        let spec = cfg.spec;
        let precompiles = PrecompilesMap::from_static(CitreaPrecompiles::new(spec).precompiles());
        let mainnet_cfg = cfg.with_spec_and_mainnet_gas_params(MainnetSpecId::from(spec));

        Self {
            inner: EvmCtx {
                ctx: Context {
                    block: block_env,
                    cfg: mainnet_cfg,
                    journaled_state: Journal::new(db),
                    tx: Default::default(),
                    chain: CitreaChain {
                        l1_fee_rate,
                        tx_info: TxInfo::default(),
                    },
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: EthInstructions::new_mainnet_with_spec(
                    revm::primitives::hardfork::SpecId::default(),
                ),
                precompiles,
                frame_stack: Default::default(),
            },
        }
    }

    /// Returns a reference to the chain extension.
    pub fn citrea_chain(&self) -> &CitreaChain {
        &self.inner.ctx.chain
    }

    /// Returns a reference to the collected tx info after execution.
    pub fn tx_info(&self) -> &TxInfo {
        &self.inner.ctx.chain.tx_info
    }
}

impl<DB: Database, I> Deref for CitreaHandlerEvm<DB, I> {
    type Target = CitreaHandlerContext<DB>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner.ctx
    }
}

impl<DB: Database, I> DerefMut for CitreaHandlerEvm<DB, I> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner.ctx
    }
}

// ── EvmTr implementation ────────────────────────────────────────────

impl<DB, INSP> EvmTr for CitreaHandlerEvm<DB, INSP>
where
    DB: Database,
{
    type Context = CitreaHandlerContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, CitreaHandlerContext<DB>>;
    type Precompiles = PrecompilesMap;
    type Frame = EthFrame;

    fn all(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
    ) {
        self.inner.all()
    }

    fn all_mut(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
    ) {
        self.inner.all_mut()
    }

    fn frame_init(
        &mut self,
        frame_input: revm::interpreter::interpreter_action::FrameInit,
    ) -> Result<
        revm::handler::evm::FrameInitResult<'_, Self::Frame>,
        revm::handler::evm::ContextDbError<Self::Context>,
    > {
        self.inner.frame_init(frame_input)
    }

    fn frame_run(
        &mut self,
    ) -> Result<
        revm::handler::FrameInitOrResult<Self::Frame>,
        revm::handler::evm::ContextDbError<Self::Context>,
    > {
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, revm::handler::evm::ContextDbError<Self::Context>> {
        self.inner.frame_return_result(result)
    }
}

impl<DB, INSP> InspectorEvmTr for CitreaHandlerEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CitreaHandlerContext<DB>, EthInterpreter>,
{
    type Inspector = INSP;

    fn all_inspector(
        &self,
    ) -> (
        &Self::Context,
        &Self::Instructions,
        &Self::Precompiles,
        &FrameStack<Self::Frame>,
        &Self::Inspector,
    ) {
        self.inner.all_inspector()
    }

    fn all_mut_inspector(
        &mut self,
    ) -> (
        &mut Self::Context,
        &mut Self::Instructions,
        &mut Self::Precompiles,
        &mut FrameStack<Self::Frame>,
        &mut Self::Inspector,
    ) {
        self.inner.all_mut_inspector()
    }
}

// ── ExecuteEvm implementation ───────────────────────────────────────

impl<DB, INSP> revm::ExecuteEvm for CitreaHandlerEvm<DB, INSP>
where
    DB: Database,
{
    type ExecutionResult = ExecutionResult;
    type State = revm::state::EvmState;
    type Error = EVMError<DB::Error>;
    type Tx = revm::context::TxEnv;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        self.inner.set_block(block);
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        CitreaHandler::new().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(&mut self) -> Result<revm::context_interface::result::ResultAndState, Self::Error> {
        CitreaHandler::new().run(self).map(|result| {
            let state = self.finalize();
            revm::context_interface::result::ResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> revm::ExecuteCommitEvm for CitreaHandlerEvm<DB, INSP>
where
    DB: Database + revm::DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> revm::InspectEvm for CitreaHandlerEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CitreaHandlerContext<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        CitreaHandler::new().inspect_run(self)
    }
}

impl<DB, INSP> revm::InspectCommitEvm for CitreaHandlerEvm<DB, INSP>
where
    DB: Database + revm::DatabaseCommit,
    INSP: Inspector<CitreaHandlerContext<DB>>,
{
}

// ── CitreaHandler ───────────────────────────────────────────────────

pub struct CitreaHandler<DB: revm::database::Database, INSP> {
    pub mainnet: MainnetHandler<CitreaHandlerEvm<DB, INSP>, EVMError<DB::Error>, EthFrame>,
}

impl<DB: revm::database::Database, INSP> CitreaHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            mainnet: MainnetHandler::default(),
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for CitreaHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for CitreaHandler<DB, INSP> {
    type Evm = CitreaHandlerEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = HaltReason;

    /// Override `execution_result` to calculate diff_size from journal entries
    /// and store L1 fee info before the journal is committed.
    fn execution_result(
        &mut self,
        evm: &mut Self::Evm,
        result: <<Self::Evm as EvmTr>::Frame as FrameTr>::FrameResult,
        result_gas: ResultGas,
    ) -> Result<ExecutionResult<Self::HaltReason>, Self::Error> {
        // Check for context errors first.
        match core::mem::replace(evm.ctx().error(), Ok(())) {
            Err(revm::context::ContextError::Db(e)) => return Err(e.into()),
            Err(revm::context::ContextError::Custom(e)) => {
                return Err(EVMError::Custom(e));
            }
            Ok(_) => (),
        }

        // Calculate diff_size from journal entries before commit clears them.
        let caller = evm.ctx().tx().caller();
        let journal_entries = &evm.inner.ctx.journaled_state.inner.journal;
        let state = &evm.inner.ctx.journaled_state.inner.state;
        let diff_size = calc_diff_size(journal_entries, state, caller);

        // Compute L1 fee.
        let l1_fee_rate = evm.inner.ctx.chain.l1_fee_rate;
        let compressed_size =
            (diff_size * BROTLI_COMPRESSION_PERCENTAGE / 100) + BROTLI_EXTRA_BYTES;
        let l1_fee = U256::from(l1_fee_rate) * U256::from(compressed_size);

        // Store in chain extension for caller to retrieve.
        evm.inner.ctx.chain.tx_info = TxInfo {
            l1_fee,
            diff_size: compressed_size,
        };

        // Standard output processing (same as MainnetHandler::execution_result).
        let output = result.output();
        let instruction_result = result.into_interpreter_result();
        let logs = evm.ctx().journal_mut().take_logs();

        let exec_result = match SuccessOrHalt::from(instruction_result.result) {
            SuccessOrHalt::Success(reason) => ExecutionResult::Success {
                reason,
                gas: result_gas,
                logs,
                output,
            },
            SuccessOrHalt::Revert => ExecutionResult::Revert {
                gas: result_gas,
                logs,
                output: output.into_data(),
            },
            SuccessOrHalt::Halt(reason) => ExecutionResult::Halt {
                reason,
                gas: result_gas,
                logs,
            },
            flag @ (SuccessOrHalt::FatalExternalError | SuccessOrHalt::Internal(_)) => {
                panic!(
                    "Encountered unexpected internal return flag: {flag:?} with instruction result: {instruction_result:?}"
                )
            }
        };

        evm.ctx().journal_mut().commit_tx();
        evm.ctx().local_mut().clear();
        evm.frame_stack().clear();

        Ok(exec_result)
    }
}

impl<DB, INSP> InspectorHandler for CitreaHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<CitreaHandlerContext<DB>>,
{
    type IT = EthInterpreter;
}

// ── calc_diff_size ──────────────────────────────────────────────────

/// Per-account change tracking used by `calc_diff_size`.
struct AccountChanges {
    info_changed: bool,
    storage_keys: BTreeSet<revm::primitives::StorageKey>,
}

impl Default for AccountChanges {
    fn default() -> Self {
        Self {
            info_changed: false,
            storage_keys: BTreeSet::new(),
        }
    }
}

/// Calculates the uncompressed diff size from journal entries.
///
/// Walks every journal entry to determine which accounts had info changes
/// (nonce, balance, code) and which storage keys were modified. The caller
/// address is always marked as info-changed (nonce increment).
///
/// The final size is a weighted sum:
///   - account_diff * 32% + storage_diff * 66% + new_account_diff
fn calc_diff_size(journal: &[JournalEntry], state: &revm::state::EvmState, caller: Address) -> u64 {
    let mut changes: BTreeMap<Address, AccountChanges> = BTreeMap::new();
    let mut new_accounts: BTreeSet<Address> = BTreeSet::new();

    // Caller always has info changed (nonce increment).
    changes.entry(caller).or_default().info_changed = true;

    for entry in journal {
        match entry {
            JournalEntry::NonceChange { address, .. } | JournalEntry::NonceBump { address } => {
                changes.entry(*address).or_default().info_changed = true;
            }
            JournalEntry::BalanceTransfer { from, to, .. } => {
                changes.entry(*from).or_default().info_changed = true;
                changes.entry(*to).or_default().info_changed = true;
            }
            JournalEntry::BalanceChange { address, .. } => {
                changes.entry(*address).or_default().info_changed = true;
            }
            JournalEntry::StorageChanged { address, key, .. } => {
                changes
                    .entry(*address)
                    .or_default()
                    .storage_keys
                    .insert(*key);
            }
            JournalEntry::CodeChange { address } => {
                changes.entry(*address).or_default().info_changed = true;
            }
            JournalEntry::AccountCreated { address, .. } => {
                changes.entry(*address).or_default().info_changed = true;
                new_accounts.insert(*address);
            }
            JournalEntry::AccountDestroyed {
                address,
                target,
                had_balance,
                ..
            } => {
                changes.entry(*address).or_default().info_changed = true;
                if !had_balance.is_zero() {
                    changes.entry(*target).or_default().info_changed = true;
                }
            }
            // Warm/touch/transient entries do not affect state diff.
            JournalEntry::AccountWarmed { .. }
            | JournalEntry::AccountTouched { .. }
            | JournalEntry::StorageWarmed { .. }
            | JournalEntry::TransientStorageChange { .. } => {}
        }
    }

    // Calculate sizes.
    let mut account_diff: u64 = 0;
    let mut storage_diff: u64 = 0;

    for (addr, change) in &changes {
        if change.info_changed {
            // Determine account info size based on whether it has code.
            let has_code = state
                .get(addr)
                .map(|acc| acc.info.code_hash != KECCAK_EMPTY)
                .unwrap_or(false);

            let db_size = if has_code {
                ACCOUNT_INFO_SIZE_WITH_CODE
            } else {
                ACCOUNT_INFO_SIZE_NO_CODE
            };
            account_diff += db_size + ACCOUNT_DIFF_OVERHEAD;
        }

        storage_diff += STORAGE_ENTRY_SIZE * change.storage_keys.len() as u64;
    }

    let new_account_diff = NEW_ACCOUNT_SIZE * new_accounts.len() as u64;

    // Weighted sum: account_diff * 32% + storage_diff * 66% + new_account_diff.
    (account_diff * ACCOUNT_DIFF_WEIGHT / 100)
        + (storage_diff * STORAGE_DIFF_WEIGHT / 100)
        + new_account_diff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_journal_has_caller_diff() {
        let caller = Address::ZERO;
        let state = revm::state::EvmState::default();
        let journal = vec![];
        let size = calc_diff_size(&journal, &state, caller);
        // Caller always counted: (41 + 12) * 32 / 100 = 16
        assert_eq!(
            size,
            (ACCOUNT_INFO_SIZE_NO_CODE + ACCOUNT_DIFF_OVERHEAD) * ACCOUNT_DIFF_WEIGHT / 100
        );
    }

    #[test]
    fn test_storage_change_counted() {
        let caller = Address::ZERO;
        let addr = Address::repeat_byte(1);
        let state = revm::state::EvmState::default();
        let journal = vec![JournalEntry::StorageChanged {
            address: addr,
            key: Default::default(),
            had_value: Default::default(),
        }];
        let size = calc_diff_size(&journal, &state, caller);
        // caller account_diff + 1 storage entry
        let expected_account =
            (ACCOUNT_INFO_SIZE_NO_CODE + ACCOUNT_DIFF_OVERHEAD) * ACCOUNT_DIFF_WEIGHT / 100;
        let expected_storage = STORAGE_ENTRY_SIZE * STORAGE_DIFF_WEIGHT / 100;
        assert_eq!(size, expected_account + expected_storage);
    }
}
