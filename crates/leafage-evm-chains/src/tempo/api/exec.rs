use crate::tempo::api::{TempoContext, TempoEvm};
use crate::tempo::hardfork::TempoHardfork;
use crate::tempo::tx::{ScopeCounts, TempoCall, TempoSigType, TempoTxEnv, TempoTxFields};
use alloy_evm::Database;
use revm::{
    context::{BlockEnv, ContextSetters},
    context_interface::{
        cfg::gas_params::{GasId, GasParams},
        journaled_state::account::JournaledAccountTr,
        result::{EVMError, ExecutionResult, ResultAndState},
        Cfg, ContextTr, JournalTr,
    },
    handler::{EthFrame, FrameResult, Handler, MainnetHandler},
    inspector::{InspectCommitEvm, InspectEvm, Inspector, InspectorHandler},
    interpreter::{interpreter::EthInterpreter, Gas, InitialAndFloorGas},
    primitives::U256,
    state::EvmState,
    DatabaseCommit, ExecuteCommitEvm, ExecuteEvm,
};

// ---------------------------------------------------------------------------
// AA gas constants — ported from Tempo writer: crates/revm/src/handler.rs
// ---------------------------------------------------------------------------

/// Expiring nonce key sentinel value (U256::MAX).
const TEMPO_EXPIRING_NONCE_KEY: U256 = U256::MAX;

/// Gas cost for expiring nonce transactions (replay check + insert).
/// 2 cold SLOADs + 1 warm SLOAD + 3 SSTOREs at RESET price.
/// Total: 2*2100 + 100 + 3*2900 = 13,000 gas.
///
/// See TIP-1009: <https://docs.tempo.xyz/protocol/tips/tip-1009>
const EXPIRING_NONCE_GAS: u64 = 2 * 2_100 + 100 + 3 * 2_900; // 13_000

/// Additional gas for P256 signature verification.
/// P256 precompile (6900, EIP-7951) + 1100 for 129 extra bytes - ecrecover savings (3000).
const P256_VERIFY_GAS: u64 = 5_000;

/// Additional gas for Keychain signatures: COLD_SLOAD_COST (2100) + 900 processing.
const KEYCHAIN_VALIDATION_GAS: u64 = 2_100 + 900; // 3_000

/// ECRECOVER gas cost (baseline for key authorization signature).
const ECRECOVER_GAS: u64 = 3_000;

/// Base gas for KeyAuthorization pre-T1B (22k storage + 5k buffer).
const KEY_AUTH_BASE_GAS: u64 = 27_000;

/// Gas per spending limit in KeyAuthorization pre-T1B.
const KEY_AUTH_PER_LIMIT_GAS: u64 = 22_000;

/// Custom GasId for TIP-1000 auth account creation cost (250k).
/// Same as Tempo writer: crates/revm/src/gas_params.rs `GasId::new(255)`.
const TIP1000_AUTH_ACCOUNT_CREATION_GAS_ID: GasId = GasId::new(255);

// ---------------------------------------------------------------------------
// EIP-7702 delegation helper for AA auth list
// ---------------------------------------------------------------------------

/// Lightweight EIP-7702 delegation entry for `apply_auth_list`.
///
/// Implements `AuthorizationTr` with authority provided directly (no signature recovery).
/// Used in the `apply_eip7702_auth_list` override for AA transactions where the
/// RPC caller provides authority + delegate addresses explicitly.
struct TempoAuthDelegation {
    authority: revm::primitives::Address,
    delegate: revm::primitives::Address,
    chain_id: U256,
    nonce: u64,
}

impl revm::context_interface::transaction::eip7702::AuthorizationTr for TempoAuthDelegation {
    fn authority(&self) -> Option<revm::primitives::Address> {
        Some(self.authority)
    }
    fn chain_id(&self) -> U256 {
        self.chain_id
    }
    fn nonce(&self) -> u64 {
        self.nonce
    }
    fn address(&self) -> revm::primitives::Address {
        self.delegate
    }
}

/// Tempo handler — wraps [`MainnetHandler`] with batch execution support.
///
/// For standard (non-batch) transactions, delegates to [`MainnetHandler`].
/// For Tempo batch transactions (type 0x76 with `aa_calls`), executes each
/// call atomically using journal checkpoints.
pub struct TempoHandler<DB: revm::database::Database, INSP> {
    _phantom: core::marker::PhantomData<(TempoEvm<DB, INSP>, EVMError<DB::Error>, EthFrame)>,
}

impl<DB: revm::database::Database, INSP> TempoHandler<DB, INSP> {
    pub fn new() -> Self {
        Self {
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<DB: revm::database::Database, INSP> Default for TempoHandler<DB, INSP> {
    fn default() -> Self {
        Self::new()
    }
}

impl<DB: Database, INSP> Handler for TempoHandler<DB, INSP> {
    type Evm = TempoEvm<DB, INSP>;
    type Error = EVMError<DB::Error>;
    type HaltReason = revm::context::result::HaltReason;

    /// Validates the transaction environment for Tempo.
    ///
    /// Ported from Tempo writer: crates/revm/src/handler.rs `validate_env`.
    /// Key Tempo-specific checks:
    /// 1. Value transfer rejection (Tempo has no native token, all balances are 0)
    /// 2. AA calls structure validation (non-empty, CREATE rules)
    #[inline]
    fn validate_env(&self, evm: &mut Self::Evm) -> Result<(), Self::Error> {
        use revm::context_interface::transaction::Transaction;

        // Tempo: value transfer is not allowed (all accounts have zero native balance).
        // Ported from writer: handler.rs:1243
        if !evm.ctx().tx.value().is_zero() {
            return Err(EVMError::Custom(
                "value transfer not allowed on Tempo".into(),
            ));
        }

        // Tempo system transactions (from=0x0, gas_limit=0) skip all validation.
        if evm.ctx().tx.base.caller.is_zero() && evm.ctx().tx.base.gas_limit == 0 {
            return Ok(());
        }

        // Standard validation (chain_id, gas limits, tx type, etc.).
        MainnetHandler::<Self::Evm, Self::Error, EthFrame>::default().validate_env(evm)?;

        // AA-specific validations.
        if let Some(fields) = evm.ctx().tx.tempo_fields.as_ref() {
            let calls = &fields.aa_calls;

            // Validate calls structure (ported from writer: validate_calls).
            if calls.is_empty() {
                return Err(EVMError::Custom("AA calls list cannot be empty".into()));
            }
            let has_auth_list = !fields.auth_list.is_empty();
            let mut iter = calls.iter();
            if let Some(first) = iter.next() {
                if has_auth_list && first.to.is_create() {
                    return Err(EVMError::Custom(
                        "calls cannot contain CREATE when authorization list is non-empty".into(),
                    ));
                }
            }
            for call in iter {
                if call.to.is_create() {
                    return Err(EVMError::Custom(
                        "only the first call in a batch can be CREATE".into(),
                    ));
                }
            }

            // Validate time window (ported from writer: handler.rs:1755-1782).
            let block_ts: u64 = evm.ctx().block.timestamp.saturating_to();
            validate_time_window(fields.valid_after, fields.valid_before, block_ts)?;

            // Expiring nonce (nonceKey=MAX) requires validBefore to be set.
            // Ported from writer: handler.rs validate_env expiring nonce check.
            if fields.nonce_key == U256::MAX && fields.valid_before.is_none() {
                return Err(EVMError::Custom(
                    "expiring nonce transaction requires valid_before to be set".into(),
                ));
            }

            // Note: keychain version, subblock, and priority fee validations are skipped —
            // leafage eth_call mode has no real signatures, no subblock txs,
            // and disable_base_fee=true. These checks are writer-only concerns.
        }

        Ok(())
    }

    /// Calculates initial gas costs with custom handling for AA transactions.
    ///
    /// Dispatches between standard and AA paths:
    /// - Standard tx: delegates to MainnetHandler + TIP-1000 nonce==0 surcharge
    /// - AA tx (0x76 with aa_calls): custom batch gas calculation
    ///
    /// Ported from Tempo writer: crates/revm/src/handler.rs
    #[inline]
    fn validate_initial_tx_gas(
        &self,
        evm: &mut Self::Evm,
    ) -> Result<InitialAndFloorGas, Self::Error> {
        let is_aa = evm
            .ctx()
            .tx
            .tempo_fields
            .as_ref()
            .is_some_and(|f| !f.aa_calls.is_empty());

        // Tempo system transactions (from=0x0, gas_limit=0) skip intrinsic gas validation.
        // Writer handles these in apply_pre_execution_changes with unlimited gas.
        // Leafage may encounter them in simulateTransactions when replaying block traces.
        if evm.ctx().tx.base.caller.is_zero() && evm.ctx().tx.base.gas_limit == 0 {
            return Ok(InitialAndFloorGas::default());
        }

        if is_aa {
            // AA transaction — use batch gas calculation.
            validate_aa_initial_tx_gas(evm)
        } else {
            // Standard transaction — use GasParams::initial_tx_gas() instead of
            // MainnetHandler (which uses hardcoded constants from SpecId, ignoring
            // TIP-1000 overrides like tx_create_cost=500k).
            // Ported from writer: handler.rs:1337
            use revm::context_interface::transaction::{AccessListItemTr, Transaction};
            let tx = &evm.ctx().tx.base;
            let gas_params = &evm.ctx().cfg.gas_params;

            let (acc, storage) = if tx.tx_type() != revm::context_interface::transaction::TransactionType::Legacy {
                tx.access_list()
                    .map(|al| {
                        al.fold((0u64, 0u64), |(a, s), item| {
                            (a + 1, s + item.storage_slots().count() as u64)
                        })
                    })
                    .unwrap_or_default()
            } else {
                (0, 0)
            };

            let mut init_gas = gas_params.initial_tx_gas(
                tx.input(),
                tx.kind().is_create(),
                acc,
                storage,
                tx.authorization_list.len() as u64,
            );

            // TIP-1000: EIP-7702 authorization_list entries with nonce==0
            // require additional auth_account_creation cost (250k gas).
            for auth in &evm.ctx().tx.base.authorization_list {
                if auth.nonce() == 0 {
                    init_gas.initial_gas +=
                        gas_params.get(TIP1000_AUTH_ACCOUNT_CREATION_GAS_ID);
                }
            }

            // TIP-1000: nonce == 0 requires additional new_account_cost (250k gas).
            let hardfork = TempoHardfork::from_timestamp(
                evm.ctx().block.timestamp.saturating_to::<u64>(),
            );
            if hardfork.is_t1() && evm.ctx().tx.base.nonce == 0 {
                init_gas.initial_gas += evm.ctx().cfg.gas_params.get(GasId::new_account_cost());
            }

            // Re-validate gas_limit after adding surcharges.
            let gas_limit = evm.ctx().tx.base.gas_limit;
            if gas_limit < init_gas.initial_gas {
                return Err(EVMError::Custom(format!(
                    "insufficient gas for intrinsic cost: gas_limit {} < intrinsic_gas {}",
                    gas_limit, init_gas.initial_gas
                )));
            }

            Ok(init_gas)
        }
    }

    /// Pre-execution: standard flow + TIP-20 fee balance warm-up.
    ///
    /// Writer's `validate_against_state_and_deduct_caller` reads the caller's TIP-20
    /// fee token balance through the journal (handler.rs:695), which warms the
    /// storage slot. This affects subsequent precompile sload gas (cold 2100 → warm 100).
    ///
    /// Leafage doesn't need actual fee deduction, but must do the same sload to
    /// warm the slot and match writer's gas behavior exactly.
    #[inline]
    fn pre_execution(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        // For 2D nonce AA (nonceKey > 0), disable protocol nonce check.
        // Writer bypasses it in validate_against_state_and_deduct_caller.
        // Without this, tx.nonce (from NonceManager, may be 0) != account.nonce → NonceTooLow.
        if evm.ctx().tx.tempo_fields.as_ref().is_some_and(|f| !f.nonce_key.is_zero()) {
            evm.ctx_mut().cfg.disable_nonce_check = true;
        }

        // Standard pre_execution (load accounts, warm coinbase, etc.).
        let gas = MainnetHandler::<Self::Evm, Self::Error, EthFrame>::default()
            .pre_execution(evm)?;

        // Warm the caller's TIP-20 fee token balance slot via journal sload.
        // Mirrors writer's load_fee_fields + validate_against_state_and_deduct_caller.
        //
        // Writer reads get_fee_token (FeeManager.user_tokens[caller]) and
        // get_token_balance (TIP20.balances[caller]) through the journal before
        // execution. These sloads warm the storage slots, making subsequent
        // precompile sloads warm (100 gas) instead of cold (2100 gas).
        //
        // Errors are ignored — warm-up is best-effort (EmptyDB in tests, etc.).
        let _ = warm_fee_token_balance(evm);

        // Increment 2D nonce in NonceManager for AA txs with nonceKey > 0.
        // Writer does this in validate_against_state_and_deduct_caller (handler.rs:854-860).
        // Without this, multi-tx batches (pre_traceMany) don't accumulate nonce state,
        // causing every tx to see nonce=0 and trigger 250k new_account_cost.
        increment_2d_nonce_if_needed(evm);

        // Set tx_origin in AccountKeychain transient storage for spending limit checks.
        // Writer does this in validate_against_state_and_deduct_caller (handler.rs:677-683)
        // for ALL transactions. Without it, tx_origin stays Address::ZERO and
        // authorize_transfer/authorize_approve/refund_spending_limit skip enforcement.
        set_keychain_tx_origin(evm);

        // Set transaction_key if this is an access key (keychain) transaction.
        // Writer: handler.rs:1128-1133 — sets transaction_key when signature is Keychain.
        // Without this, transaction_key stays ZERO and TIP20 authorize_transfer/
        // authorize_approve/refund_spending_limit skip spending limit enforcement.
        if let Some(key_id) = evm
            .ctx()
            .tx
            .tempo_fields
            .as_ref()
            .and_then(|f| f.key_id)
        {
            set_keychain_transaction_key(evm, key_id);
        }

        Ok(gas)
    }

    /// Applies EIP-7702 delegations from AA authorization list.
    ///
    /// Writer: handler.rs:620-665. For AA txs (0x76), applies tempo_authorization_list
    /// entries as EIP-7702 delegations. Each entry with `authority` + `delegate` fields
    /// sets the authority's code to `0xef0100 || delegate` in the journal.
    ///
    /// Entries without delegation fields (gas-only) are skipped.
    /// T1+: no refund (matching writer behavior).
    #[inline]
    fn apply_eip7702_auth_list(&self, evm: &mut Self::Evm) -> Result<u64, Self::Error> {
        let has_aa_delegations = evm
            .ctx()
            .tx
            .tempo_fields
            .as_ref()
            .map(|f| f.auth_list.iter().any(|a| a.authority.is_some() && a.delegate.is_some()))
            .unwrap_or(false);

        if !has_aa_delegations {
            // No AA delegation entries — use default EIP-7702 path (handles type 0x04).
            return MainnetHandler::<Self::Evm, Self::Error, EthFrame>::default()
                .apply_eip7702_auth_list(evm);
        }

        // Build delegation entries from auth_list items that have authority + delegate.
        let delegations: Vec<TempoAuthDelegation> = evm
            .ctx()
            .tx
            .tempo_fields
            .as_ref()
            .unwrap()
            .auth_list
            .iter()
            .filter_map(|auth| {
                let authority = auth.authority?;
                let delegate = auth.delegate?;
                let chain_id = auth.chain_id.unwrap_or(U256::from(evm.ctx().cfg.chain_id));
                Some(TempoAuthDelegation {
                    authority,
                    delegate,
                    chain_id,
                    nonce: auth.nonce,
                })
            })
            .collect();

        let chain_id = evm.ctx().cfg.chain_id;
        let refund_per_auth = evm.ctx().cfg.gas_params.tx_eip7702_auth_refund();

        let refunded = revm::handler::pre_execution::apply_auth_list::<_, Self::Error>(
            chain_id,
            refund_per_auth,
            delegations.iter(),
            evm.ctx_mut().journal_mut(),
        )?;

        // TIP-1000: no refund on T1+ (matching writer handler.rs:660).
        let hardfork = crate::tempo::hardfork::TempoHardfork::from_timestamp(
            evm.ctx().block.timestamp.saturating_to::<u64>(),
        );
        if hardfork.is_t1() {
            return Ok(0);
        }
        Ok(refunded)
    }

    /// Overridden execution: dispatches to batch path when `aa_calls` is present.
    #[inline]
    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        let calls = evm
            .ctx()
            .tx
            .tempo_fields
            .as_ref()
            .filter(|f| !f.aa_calls.is_empty())
            .map(|f| f.aa_calls.clone());

        if let Some(calls) = calls {
            execute_multi_call(evm, init_and_floor_gas, calls, |evm, zero_init| {
                MainnetHandler::<TempoEvm<DB, INSP>, EVMError<DB::Error>, EthFrame>::default()
                    .execution(evm, zero_init)
            })
        } else {
            MainnetHandler::<Self::Evm, Self::Error, EthFrame>::default()
                .execution(evm, init_and_floor_gas)
        }
    }
}

// ---------------------------------------------------------------------------
// AA gas validation — ported from Tempo writer: crates/revm/src/handler.rs
// ---------------------------------------------------------------------------

/// Validates the time window for AA transactions.
///
/// Ported from Tempo writer: handler.rs:1755-1782 `validate_time_window`.
/// - `valid_after`: block_timestamp must be >= valid_after
/// - `valid_before`: block_timestamp must be < valid_before
#[inline]
fn validate_time_window<E: core::fmt::Debug>(
    valid_after: Option<u64>,
    valid_before: Option<u64>,
    block_timestamp: u64,
) -> Result<(), EVMError<E>> {
    if let Some(after) = valid_after {
        if block_timestamp < after {
            return Err(EVMError::Custom(format!(
                "transaction not yet valid: block_timestamp ({}) < valid_after ({})",
                block_timestamp, after
            )));
        }
    }
    if let Some(before) = valid_before {
        if block_timestamp >= before {
            return Err(EVMError::Custom(format!(
                "transaction expired: block_timestamp ({}) >= valid_before ({})",
                block_timestamp, before
            )));
        }
    }
    Ok(())
}

/// Warms the caller's TIP-20 fee token balance slot in the journal.
///
/// Mirrors writer's `load_fee_fields` + `validate_against_state_and_deduct_caller`:
/// 1. Reads FeeManager.user_tokens[caller] (slot 1) → determines fee_token
/// 2. Reads TIP20.balances[caller] (slot 9) of the fee_token
///
/// These sloads add entries to the journal's accessed_storage_keys, making
/// subsequent precompile reads of the same slots warm (100 gas vs 2100 gas).
///
/// Returns Ok(()) on success, Err on any DB/journal error. Caller should
/// ignore errors (warm-up is best-effort).
fn warm_fee_token_balance<DB: Database, INSP>(
    evm: &mut TempoEvm<DB, INSP>,
) -> Result<(), EVMError<DB::Error>> {
    use crate::tempo::precompile::{DEFAULT_FEE_TOKEN, TIP_FEE_MANAGER_ADDRESS};
    use revm::context_interface::JournalTr;

    let caller = evm.ctx().tx.base.caller;

    // 1. Read FeeManager.user_tokens[caller] — Mapping<Address,Address> at slot 1
    let fee_manager_slot = {
        let mut data = [0u8; 64];
        data[12..32].copy_from_slice(caller.as_slice());
        data[63] = 1;
        revm::primitives::keccak256(&data)
    };
    // load_account first to avoid panic on fresh journal
    let _ = evm
        .ctx_mut()
        .journal_mut()
        .load_account(TIP_FEE_MANAGER_ADDRESS)?;
    let user_token = evm
        .ctx_mut()
        .journal_mut()
        .sload(TIP_FEE_MANAGER_ADDRESS, fee_manager_slot.into())
        .map(|r| r.data)?;

    let fee_token = if user_token.is_zero() {
        DEFAULT_FEE_TOKEN
    } else {
        revm::primitives::Address::from_word(user_token.into())
    };

    // 2. Read TIP20.balances[caller] — Mapping<Address,U256> at slot 9
    let balance_slot = {
        let mut data = [0u8; 64];
        data[12..32].copy_from_slice(caller.as_slice());
        data[63] = 9;
        revm::primitives::keccak256(&data)
    };
    let _ = evm
        .ctx_mut()
        .journal_mut()
        .load_account(fee_token)?;
    let _ = evm
        .ctx_mut()
        .journal_mut()
        .sload(fee_token, balance_slot.into())?;

    Ok(())
}

/// Increments the 2D nonce in NonceManager for AA txs with nonceKey > 0.
///
/// Writer does this in `validate_against_state_and_deduct_caller` (handler.rs:854-860)
/// via `StorageCtx::enter_evm`. Leafage uses direct journal sload/sstore.
///
/// This is critical for multi-tx batches (pre_traceMany): without it, every tx
/// sees nonce=0 in NonceManager and triggers the 250k new_account_cost surcharge.
/// With it, the first tx increments nonce to 1, subsequent txs see nonce=1 and
/// only pay the 5k existing_nonce_key cost.
#[inline]
fn increment_2d_nonce_if_needed<DB: Database, INSP>(evm: &mut TempoEvm<DB, INSP>) {
    use crate::tempo::precompile::NONCE_PRECOMPILE_ADDRESS;
    use crate::tempo::precompile::storage_types::StorageKey;
    use revm::context_interface::JournalTr;

    let nonce_key = evm
        .ctx()
        .tx
        .tempo_fields
        .as_ref()
        .map(|f| f.nonce_key)
        .unwrap_or_default();

    // Only for 2D nonce (nonceKey > 0, not expiring nonce MAX)
    if nonce_key.is_zero() || nonce_key == U256::MAX {
        return;
    }

    let caller = evm.ctx().tx.base.caller;
    let slot = caller.mapping_slot(U256::ZERO);
    let slot = nonce_key.mapping_slot(slot);

    // load_account first to ensure NonceManager is in journal
    let _ = evm
        .ctx_mut()
        .journal_mut()
        .load_account(NONCE_PRECOMPILE_ADDRESS);

    // sload current nonce
    let current = evm
        .ctx_mut()
        .journal_mut()
        .sload(NONCE_PRECOMPILE_ADDRESS, slot)
        .map(|r| r.data.saturating_to::<u64>())
        .unwrap_or(0);

    // sstore incremented nonce
    let _ = evm.ctx_mut().journal_mut().sstore(
        NONCE_PRECOMPILE_ADDRESS,
        slot,
        U256::from(current + 1),
    );
}

fn set_keychain_tx_origin<DB: Database, INSP>(evm: &mut TempoEvm<DB, INSP>) {
    use crate::tempo::precompile::ACCOUNT_KEYCHAIN_ADDRESS;
    use revm::context_interface::JournalTr;

    let caller = evm.ctx().tx.base.caller;
    // tx_origin = Slot::new(U256::from(3), ACCOUNT_KEYCHAIN_ADDRESS)
    // tstore is infallible on the journal (no DB access needed).
    evm.ctx_mut()
        .journal_mut()
        .tstore(ACCOUNT_KEYCHAIN_ADDRESS, U256::from(3), caller.into_word().into());
}

/// Sets `transaction_key` in AccountKeychain transient storage (slot 2).
///
/// Writer does this in `validate_against_state_and_deduct_caller` (handler.rs:1128-1133)
/// when the transaction uses a Keychain signature. TIP20 `authorize_transfer` and
/// `authorize_approve` check `transaction_key`: when it's non-zero, spending limits
/// for that access key are enforced.
#[inline]
fn set_keychain_transaction_key<DB: Database, INSP>(
    evm: &mut TempoEvm<DB, INSP>,
    key_id: revm::primitives::Address,
) {
    use crate::tempo::precompile::ACCOUNT_KEYCHAIN_ADDRESS;
    use revm::context_interface::JournalTr;

    evm.ctx_mut()
        .journal_mut()
        .tstore(ACCOUNT_KEYCHAIN_ADDRESS, U256::from(2), key_id.into_word().into());
}

/// Executes a batch of AA calls atomically.
///
/// Shared between `execution()` (non-inspect) and `inspect_execution()` (inspect).
/// The `exec_single` closure determines how each sub-call is executed:
/// - Non-inspect: `MainnetHandler::execution()`
/// - Inspect: `inspect_run_exec_loop()` (inspector-aware frame loop)
fn execute_multi_call<DB: Database, INSP>(
    evm: &mut TempoEvm<DB, INSP>,
    init_and_floor_gas: &InitialAndFloorGas,
    calls: Vec<TempoCall>,
    exec_single: impl Fn(
        &mut TempoEvm<DB, INSP>,
        &InitialAndFloorGas,
    ) -> Result<FrameResult, EVMError<DB::Error>>,
) -> Result<FrameResult, EVMError<DB::Error>> {
    let checkpoint = evm.ctx_mut().journal_mut().checkpoint();

    let gas_limit = evm.ctx().tx.base.gas_limit;
    let mut remaining_gas = gas_limit.saturating_sub(init_and_floor_gas.initial_gas);
    let mut accumulated_gas_refund: i64 = 0;

    let original_kind = evm.ctx().tx.base.kind;
    let original_value = evm.ctx().tx.base.value;
    let original_data = evm.ctx().tx.base.data.clone();

    let mut final_result: Option<FrameResult> = None;

    for call in &calls {
        {
            let tx = &mut evm.ctx_mut().tx;
            tx.base.kind = call.to;
            tx.base.value = call.value;
            tx.base.data = call.input.clone();
            tx.base.gas_limit = remaining_gas;
        }

        let zero_init = InitialAndFloorGas::new(0, 0);
        let result = exec_single(evm, &zero_init);

        {
            let tx = &mut evm.ctx_mut().tx;
            tx.base.kind = original_kind;
            tx.base.value = original_value;
            tx.base.data = original_data.clone();
            tx.base.gas_limit = gas_limit;
        }

        let mut frame_result = result?;

        if !frame_result.instruction_result().is_ok() {
            evm.ctx_mut().journal_mut().checkpoint_revert(checkpoint);

            // After checkpoint_revert, if the first call was a CREATE using protocol nonce
            // (nonce_key == 0), the nonce bump from make_create_frame was rolled back.
            // Re-bump it to match writer behavior (burn the nonce even on failed CREATE).
            // With 2D nonces (nonce_key != 0), NonceManager handles replay protection,
            // so we don't need to burn the protocol nonce.
            let uses_protocol_nonce = evm.ctx().tx.tempo_fields
                    .as_ref()
                    .map(|f| f.nonce_key.is_zero())
                    .unwrap_or(true);
            if uses_protocol_nonce && calls.first().map(|c| c.to.is_create()).unwrap_or(false) {
                let caller = evm.ctx().tx.base.caller;
                if let Ok(mut acc) = evm.ctx_mut().journal_mut().load_account_mut_skip_cold_load(caller, true) {
                    acc.data.bump_nonce();
                }
            }

            let gas_spent_by_failed = frame_result.gas().spent();
            let total_gas_spent = (gas_limit - remaining_gas) + gas_spent_by_failed;

            let mut corrected_gas = Gas::new(gas_limit);
            if frame_result.instruction_result().is_revert() {
                corrected_gas.set_spent(total_gas_spent);
            } else {
                corrected_gas.spend_all();
            }
            corrected_gas.set_refund(0);
            *frame_result.gas_mut() = corrected_gas;

            return Ok(frame_result);
        }

        let gas_spent = frame_result.gas().spent();
        let gas_refunded = frame_result.gas().refunded();
        accumulated_gas_refund = accumulated_gas_refund.saturating_add(gas_refunded);
        remaining_gas = remaining_gas.saturating_sub(gas_spent);

        final_result = Some(frame_result);
    }

    evm.ctx_mut().journal_mut().checkpoint_commit();

    let mut result = final_result
        .ok_or_else(|| EVMError::Custom("No calls executed in batch".into()))?;

    let total_gas_spent = gas_limit - remaining_gas;
    let mut corrected_gas = Gas::new(gas_limit);
    corrected_gas.set_spent(total_gas_spent);
    corrected_gas.set_refund(accumulated_gas_refund);
    *result.gas_mut() = corrected_gas;

    Ok(result)
}

/// Validates and calculates initial transaction gas for AA transactions.
///
/// Computes intrinsic gas for the batch (base stipend + signature + per-call costs +
/// key auth + authorization list + 2D nonce gas) then validates gas_limit is sufficient.
///
/// Ported from Tempo writer: crates/revm/src/handler.rs `validate_aa_initial_tx_gas`.
fn validate_aa_initial_tx_gas<DB: Database, INSP>(
    evm: &mut TempoEvm<DB, INSP>,
) -> Result<InitialAndFloorGas, EVMError<DB::Error>> {
    let hardfork = TempoHardfork::from_timestamp(
        evm.ctx().block.timestamp.saturating_to::<u64>(),
    );

    let gas_params = &evm.ctx().cfg.gas_params;
    let gas_limit = evm.ctx().tx.base.gas_limit;
    let nonce = evm.ctx().tx.base.nonce;

    let tempo_fields = evm
        .ctx()
        .tx
        .tempo_fields
        .as_ref()
        .expect("validate_aa_initial_tx_gas called for non-AA transaction");

    let nonce_key = tempo_fields.nonce_key;

    // Validate initcode size for CREATE calls (EIP-3860).
    let max_initcode_size = evm.ctx().cfg.max_initcode_size();
    for call in &tempo_fields.aa_calls {
        if call.to.is_create() && call.input.len() > max_initcode_size {
            return Err(EVMError::Custom(format!(
                "initcode size {} exceeds max {}",
                call.input.len(),
                max_initcode_size
            )));
        }
    }

    // Calculate batch intrinsic gas.
    let mut batch_gas = calculate_aa_batch_intrinsic_gas(tempo_fields, &gas_params, evm, hardfork)?;

    // Calculate 2D nonce gas based on hardfork and nonce_key.
    // For nonceKey > 0, the relevant nonce is the 2D nonce from NonceManager storage,
    // NOT the protocol nonce (tx.base.nonce). Writer uses tx.nonce which comes from the
    // signed transaction (explicitly set by the AA sender). Leafage reads from DB.
    // Read the 2D nonce from NonceManager to determine new_account vs existing_key gas.
    // For nonceKey > 0, read the 2D nonce from NonceManager storage to determine
    // whether this is a new nonce key (nonce==0 → +250k) or existing (→ +5k).
    // Writer uses tx.nonce (from signed tx where sender explicitly sets nonce=0 for new keys).
    // Leafage reads from storage since tx.nonce comes from DB (protocol nonce, not 2D nonce).
    let mut nonce_2d_gas: u64 = 0;

    if hardfork.is_t1() {
        if nonce_key == TEMPO_EXPIRING_NONCE_KEY {
            batch_gas.initial_gas += EXPIRING_NONCE_GAS;
        } else if nonce == 0 {
            batch_gas.initial_gas += gas_params.get(GasId::new_account_cost());
        } else if !nonce_key.is_zero() {
            batch_gas.initial_gas += hardfork.gas_existing_nonce_key();
        }
    } else if !nonce_key.is_zero() {
        nonce_2d_gas = if nonce == 0 {
            hardfork.gas_new_nonce_key()
        } else {
            hardfork.gas_existing_nonce_key()
        };
    }

    if hardfork.is_t0() {
        batch_gas.initial_gas += nonce_2d_gas;
    }

    if gas_limit < batch_gas.initial_gas {
        return Err(EVMError::Custom(format!(
            "insufficient gas for AA intrinsic cost: gas_limit={}, intrinsic={}",
            gas_limit, batch_gas.initial_gas
        )));
    }

    if !hardfork.is_t0() {
        batch_gas.initial_gas += nonce_2d_gas;
    }

    if gas_limit < batch_gas.floor_gas {
        return Err(EVMError::Custom(format!(
            "insufficient gas for AA floor: gas_limit={}, floor={}",
            gas_limit, batch_gas.floor_gas
        )));
    }

    Ok(batch_gas)
}

// ---------------------------------------------------------------------------
// Signature gas helpers — ported from Tempo writer: crates/revm/src/handler.rs
// ---------------------------------------------------------------------------

/// Additional gas for a primitive signature type (beyond base 21k).
/// Ported from Tempo writer: `primitive_signature_verification_gas`.
#[inline]
fn primitive_sig_gas(sig_type: TempoSigType, webauthn_data_size: usize) -> u64 {
    use revm::context_interface::cfg::gas::get_tokens_in_calldata_istanbul;

    match sig_type {
        TempoSigType::Secp256k1 => 0,
        TempoSigType::P256 => P256_VERIFY_GAS,
        TempoSigType::WebAuthn => {
            // Construct mock WebAuthn data matching writer's create_mock_primitive_signature.
            // Structure: 37 bytes authenticator data (mostly zeros) + clientDataJSON (ASCII text).
            // This ensures token counting matches writer exactly.
            const AUTH_DATA_SIZE: usize = 37;
            const BASE_CLIENT_JSON: &str =
                r#"{"type":"webauthn.get","challenge":"","origin":""}"#;
            const MIN_SIZE: usize = AUTH_DATA_SIZE + BASE_CLIENT_JSON.len(); // 87

            let size = webauthn_data_size.max(MIN_SIZE);
            let mut mock_data = vec![0u8; AUTH_DATA_SIZE];
            mock_data[32] = 0x01; // UP flag

            let additional = size.saturating_sub(MIN_SIZE);
            let client_json = if additional > 0 {
                format!(
                    r#"{{"type":"webauthn.get","challenge":"","origin":"{}"}}"#,
                    "x".repeat(additional)
                )
            } else {
                BASE_CLIENT_JSON.to_string()
            };
            mock_data.extend_from_slice(client_json.as_bytes());

            let tokens = get_tokens_in_calldata_istanbul(&mock_data);
            P256_VERIFY_GAS + tokens * gas_params_tx_token_cost()
        }
    }
}

/// Returns the standard tx token cost (4 gas per token).
/// Matches revm's STANDARD_TOKEN_COST (context-interface/cfg/gas.rs).
/// Note: non-zero bytes cost 16 gas each = 4 tokens * 4 gas/token.
/// `get_tokens_in_calldata_istanbul` already converts bytes to tokens.
#[inline]
fn gas_params_tx_token_cost() -> u64 {
    4
}

/// Gas for AA transaction signature (may include Keychain overhead).
#[inline]
fn tempo_sig_gas(fields: &TempoTxFields) -> u64 {
    let base = primitive_sig_gas(fields.sig_type, fields.webauthn_data_size);
    if fields.is_keychain {
        base + KEYCHAIN_VALIDATION_GAS
    } else {
        base
    }
}

// Call-scope helper-gas constants (TIP-1046, T4+).
const BASE_SCOPE_GAS: u64 = 5_000;
const TARGET_SCOPE_GAS: u64 = 7_000;
const SELECTOR_SCOPE_GAS: u64 = 7_000;
const RECIPIENT_SCOPE_GAS: u64 = 5_000;

/// Counts the `KeyAuthorization` SSTORE-set rows charged by the dynamic path,
/// per `ScopeCounts` and active spec. Mirrors writer
/// `call_scope_storage_slots(KeyAuthorization, TempoHardfork)`.
///
/// - `has_allowed_calls = false`: 0 (key is unrestricted, no scope tree).
/// - empty `allowedCalls`: 1 (account mode write).
/// - non-empty `allowedCalls` (`spec.is_t3()` only):
///     - T3: `1 + scopes*3 + selectors*3 + constrained_selectors + recipients*2`
///       (counts every persisted row).
///     - T4: `1 + scopes*2 + 1 + selectors*2 + selector_sets + constrained_selectors + recipients*2`
///       (only storage-creating rows; same-tx set-length rewrites are not
///       re-counted).
#[inline]
fn call_scope_storage_slots(s: &ScopeCounts, spec: TempoHardfork) -> u64 {
    if !s.has_allowed_calls {
        return 0;
    }
    if s.scopes == 0 {
        return 1;
    }
    let scopes = s.scopes as u64;
    let selectors = s.selectors as u64;
    let constrained = s.constrained_selectors as u64;
    let recipients = s.recipients as u64;

    if spec.is_t4() {
        // selector_sets = number of scopes that have at least one selector_rule.
        // Without per-scope detail we conservatively use the number of scopes
        // that contain any selector (== scopes when selectors > 0).
        let selector_sets = if selectors > 0 { scopes } else { 0 };
        1 + scopes * 2 + 1 + selectors * 2 + selector_sets + constrained + recipients * 2
    } else {
        // T3
        1 + scopes * 3 + selectors * 3 + constrained + recipients * 2
    }
}

/// Unpriced bookkeeping gas around the scope tree (T4+). Mirrors writer
/// `call_scope_extra_gas(KeyAuthorization)`:
/// `BASE + TARGET*targets + SELECTOR*selectors + RECIPIENT*recipients`.
/// `BASE_SCOPE_GAS` is always charged even when `allowed_calls` is `None`.
#[inline]
fn call_scope_extra_gas(s: &ScopeCounts) -> u64 {
    if !s.has_allowed_calls {
        return BASE_SCOPE_GAS;
    }
    BASE_SCOPE_GAS
        + TARGET_SCOPE_GAS.saturating_mul(s.scopes as u64)
        + SELECTOR_SCOPE_GAS.saturating_mul(s.selectors as u64)
        + RECIPIENT_SCOPE_GAS.saturating_mul(s.recipients as u64)
}

/// Key authorization gas calculation. Four active branches matching writer
/// `calculate_key_authorization_gas`:
///
/// - **Pre-T1B**: heuristic constants `KEY_AUTH_BASE_GAS + sig_gas + n × KEY_AUTH_PER_LIMIT_GAS`.
/// - **T1B-T2**: precise storage cost: `sig_gas + sload + sstore × (1 + n) + BUFFER`.
/// - **T3**: T1B-T2 with `limit_slots = n × 2` (T3 stores periodic-limit meta
///   in a second slot) and `+ sstore × call_scope_storage_slots(T3)`.
/// - **T4**: T3 with the T4 `call_scope_storage_slots` formula and
///   `+ call_scope_extra_gas` (BASE/TARGET/SELECTOR/RECIPIENT surcharges).
///
/// TIP-1016 state gas is NOT implemented: writer gates it under
/// `cfg_env.enable_amsterdam_eip8037` which is disabled on mainnet, so
/// state_gas is 0 at T4 activation.
///
/// `scope_counts` is `ScopeCounts::default()` for keys without call scopes,
/// which is byte-accurate vs writer for that case. Once tx-envelope parsing
/// fills `scope_counts` from `KeyAuthorization.allowedCalls`, call-scope tx
/// will also be byte-accurate.
#[inline]
fn key_auth_gas(
    sig_type: TempoSigType,
    num_limits: u32,
    scope_counts: &ScopeCounts,
    gas_params: &GasParams,
    hardfork: TempoHardfork,
) -> u64 {
    let sig_gas = ECRECOVER_GAS + primitive_sig_gas(sig_type, 0);
    let num_limits = num_limits as u64;

    if !hardfork.is_t1b() {
        // Pre-T1B: heuristic constants.
        return KEY_AUTH_BASE_GAS + sig_gas + num_limits * KEY_AUTH_PER_LIMIT_GAS;
    }

    const BUFFER: u64 = 2_000;
    let sstore_cost = gas_params.get(GasId::sstore_set_without_load_cost());
    let sload_cost =
        gas_params.warm_storage_read_cost() + gas_params.cold_storage_additional_cost();

    // T3+ stores 2 slots per spending limit (remaining + packed period meta).
    let limit_slots = if hardfork.is_t3() {
        num_limits.saturating_mul(2)
    } else {
        num_limits
    };

    // T3+ adds call-scope storage rows.
    let scope_slots = if hardfork.is_t3() {
        call_scope_storage_slots(scope_counts, hardfork)
    } else {
        0
    };

    let mut total =
        sig_gas + sload_cost + sstore_cost * (1 + limit_slots + scope_slots) + BUFFER;

    // T4+: bookkeeping surcharge around the scope tree.
    if hardfork.is_t4() {
        total = total.saturating_add(call_scope_extra_gas(scope_counts));
    }

    total
}

/// Computes intrinsic gas for an AA batch transaction.
///
/// Ported from Tempo writer `calculate_aa_batch_intrinsic_gas`.
/// Gas components:
/// 1. Base stipend (21k)
/// 2. Signature verification gas (based on sig_type from request)
/// 3. Per-call cold account access cost
/// 4. Authorization list costs (EIP-7702 + per-auth signature + TIP-1000)
/// 5. Key authorization costs (if present)
/// 6. Per-call calldata + CREATE costs
/// 7. Access list costs
/// 8. Floor gas (EIP-7623)
fn calculate_aa_batch_intrinsic_gas<DB: Database, INSP>(
    fields: &TempoTxFields,
    gas_params: &GasParams,
    evm: &TempoEvm<DB, INSP>,
    hardfork: TempoHardfork,
) -> Result<InitialAndFloorGas, EVMError<DB::Error>> {
    use revm::context_interface::cfg::gas::get_tokens_in_calldata_istanbul;
    use revm::context_interface::transaction::{AccessListItemTr, Transaction};

    let calls = &fields.aa_calls;
    let mut gas = InitialAndFloorGas::default();

    // 1. Base stipend (21k).
    gas.initial_gas += gas_params.tx_base_stipend();

    // 2. Signature verification gas.
    gas.initial_gas += tempo_sig_gas(fields);

    // 3. Per-call cold account access.
    let cold_account_cost =
        gas_params.warm_storage_read_cost() + gas_params.cold_account_additional_cost();
    gas.initial_gas += cold_account_cost * calls.len().saturating_sub(1) as u64;

    // 4. Authorization list costs (tempo_authorization_list only, same as writer).
    // Writer uses aa_env.tempo_authorization_list for ALL auth costs in AA path,
    // NOT TxEnv.authorization_list.
    let auth_list = &fields.auth_list;
    gas.initial_gas +=
        auth_list.len() as u64 * gas_params.tx_eip7702_per_empty_account_cost();

    for auth in auth_list {
        let auth_sig_gas = primitive_sig_gas(auth.sig_type, 0);
        gas.initial_gas += if auth.is_keychain {
            auth_sig_gas + KEYCHAIN_VALIDATION_GAS
        } else {
            auth_sig_gas
        };
        // TIP-1000: auth with nonce==0 incurs 250k account creation cost.
        if auth.nonce == 0 {
            gas.initial_gas += gas_params.get(TIP1000_AUTH_ACCOUNT_CREATION_GAS_ID);
        }
    }

    // 5. Key authorization costs (if present).
    if let Some(ka) = &fields.key_auth {
        gas.initial_gas +=
            key_auth_gas(ka.sig_type, ka.num_limits, &ka.scope_counts, gas_params, hardfork);
    }

    // 6. Per-call costs (calldata + CREATE).
    let mut total_tokens: u64 = 0;
    for call in calls {
        let tokens = get_tokens_in_calldata_istanbul(&call.input);
        total_tokens += tokens;

        if call.to.is_create() {
            gas.initial_gas += gas_params.create_cost();
            gas.initial_gas += gas_params.tx_initcode_cost(call.input.len());
        }
    }
    gas.initial_gas += total_tokens * gas_params.tx_token_cost();

    // 7. Access list costs (from base TxEnv).
    let tx = &evm.ctx().tx;
    if let Some(access_list) = Transaction::access_list(tx) {
        let (accounts, storages) = access_list.fold((0u64, 0u64), |(acc, stor), item| {
            (acc + 1, stor + item.storage_slots().count() as u64)
        });
        gas.initial_gas += accounts * gas_params.tx_access_list_address_cost();
        gas.initial_gas += storages * gas_params.tx_access_list_storage_key_cost();
    }

    // 8. Floor gas (EIP-7623).
    gas.floor_gas = gas_params.tx_floor_cost(total_tokens);

    Ok(gas)
}

impl<DB, INSP> InspectorHandler for TempoHandler<DB, INSP>
where
    DB: Database,
    INSP: Inspector<TempoContext<DB>>,
{
    type IT = EthInterpreter;

    /// Inspector-aware execution for AA batch tracing.
    ///
    /// Overrides the default `inspect_execution` to dispatch AA batch calls
    /// through `inspect_run_exec_loop` (inspector-aware frame loop), ensuring
    /// each sub-call's opcodes are visible to the tracing inspector.
    ///
    /// Without this override, `inspect_run` calls the default `inspect_execution`
    /// which only handles single-call execution — AA batch sub-calls would be
    /// invisible to the inspector (pre_traceMany would miss inner call traces).
    fn inspect_execution(
        &mut self,
        evm: &mut Self::Evm,
        init_and_floor_gas: &InitialAndFloorGas,
    ) -> Result<FrameResult, Self::Error> {
        let calls = evm
            .ctx()
            .tx
            .tempo_fields
            .as_ref()
            .filter(|f| !f.aa_calls.is_empty())
            .map(|f| f.aa_calls.clone());

        if let Some(calls) = calls {
            // AA batch: each sub-call goes through inspector-aware execution.
            execute_multi_call(evm, init_and_floor_gas, calls, |evm, zero_init| {
                // Use inspect_execution (inspector-aware) for each sub-call.
                let gas_limit = evm.ctx().tx.base.gas_limit - zero_init.initial_gas;
                let first_frame_input =
                    MainnetHandler::<TempoEvm<DB, INSP>, EVMError<DB::Error>, EthFrame>::default()
                        .first_frame_input(evm, gas_limit)?;
                let mut frame_result =
                    TempoHandler::<DB, INSP>::new().inspect_run_exec_loop(evm, first_frame_input)?;
                MainnetHandler::<TempoEvm<DB, INSP>, EVMError<DB::Error>, EthFrame>::default()
                    .last_frame_result(evm, &mut frame_result)?;
                Ok(frame_result)
            })
        } else {
            // Standard single call: default inspect_execution.
            let gas_limit = evm.ctx().tx.base.gas_limit - init_and_floor_gas.initial_gas;
            let first_frame_input =
                MainnetHandler::<Self::Evm, Self::Error, EthFrame>::default()
                    .first_frame_input(evm, gas_limit)?;
            let mut frame_result = self.inspect_run_exec_loop(evm, first_frame_input)?;
            MainnetHandler::<Self::Evm, Self::Error, EthFrame>::default()
                .last_frame_result(evm, &mut frame_result)?;
            Ok(frame_result)
        }
    }
}

impl<DB, INSP> ExecuteEvm for TempoEvm<DB, INSP>
where
    DB: Database,
{
    type ExecutionResult = ExecutionResult;
    type State = EvmState;
    type Error = EVMError<DB::Error>;
    type Tx = TempoTxEnv;
    type Block = BlockEnv;

    fn set_block(&mut self, block: Self::Block) {
        // Wrap BlockEnv into TempoBlockEnv (millis_part defaults to 0).
        use crate::tempo::block::TempoBlockEnv;
        self.inner.ctx.block = TempoBlockEnv {
            inner: block,
            timestamp_millis_part: 0,
        };
    }

    fn transact_one(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        TempoHandler::new().run(self)
    }

    fn finalize(&mut self) -> Self::State {
        self.inner.finalize()
    }

    fn replay(&mut self) -> Result<ResultAndState, Self::Error> {
        TempoHandler::new().run(self).map(|result| {
            let state = self.finalize();
            ResultAndState::new(result, state)
        })
    }
}

impl<DB, INSP> ExecuteCommitEvm for TempoEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
{
    fn commit(&mut self, state: Self::State) {
        self.inner.ctx.db_mut().commit(state);
    }
}

impl<DB, INSP> InspectEvm for TempoEvm<DB, INSP>
where
    DB: Database,
    INSP: Inspector<TempoContext<DB>>,
{
    type Inspector = INSP;

    fn set_inspector(&mut self, inspector: Self::Inspector) {
        self.inner.set_inspector(inspector);
    }

    fn inspect_one_tx(&mut self, tx: Self::Tx) -> Result<Self::ExecutionResult, Self::Error> {
        self.inner.ctx.set_tx(tx);
        TempoHandler::new().inspect_run(self)
    }
}

impl<DB, INSP> InspectCommitEvm for TempoEvm<DB, INSP>
where
    DB: Database + DatabaseCommit,
    INSP: Inspector<TempoContext<DB>>,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::api::TempoEvm;
    use crate::tempo::precompile::ACCOUNT_KEYCHAIN_ADDRESS;
    use alloy_evm::EvmEnv;
    use crate::tempo::hardfork::TempoHardfork;
    use revm::context::{BlockEnv, CfgEnv};
    use revm::database::EmptyDB;
    use revm::inspector::NoOpInspector;
    use revm::primitives::Address;

    fn make_evm() -> TempoEvm<EmptyDB, NoOpInspector> {
        let mut cfg = CfgEnv::new_with_spec(TempoHardfork::default());
        cfg.chain_id = 4217;
        let mut block_env = BlockEnv::default();
        block_env.timestamp = revm::primitives::U256::from(1_770_908_500u64); // Post-T1A
        block_env.gas_limit = 100_000_000;
        let env = EvmEnv::new(cfg, block_env);
        TempoEvm::new(env, EmptyDB::default(), NoOpInspector, false)
    }

    #[test]
    fn test_set_keychain_tx_origin_writes_caller_to_transient_storage() {
        let caller = Address::with_last_byte(0xAA);
        let mut evm = make_evm();
        evm.inner.ctx.tx.base.caller = caller;

        // Before: slot 3 should be zero (transient storage default)
        let before = evm
            .inner
            .ctx
            .journal_mut()
            .tload(ACCOUNT_KEYCHAIN_ADDRESS, U256::from(3));
        assert_eq!(before, U256::ZERO, "tx_origin should be zero before set");

        // Act
        set_keychain_tx_origin(&mut evm);

        // After: slot 3 should contain the caller address
        let after = evm
            .inner
            .ctx
            .journal_mut()
            .tload(ACCOUNT_KEYCHAIN_ADDRESS, U256::from(3));
        let expected: U256 = caller.into_word().into();
        assert_eq!(after, expected, "tx_origin should equal caller after set");
    }

    #[test]
    fn test_set_keychain_tx_origin_zero_caller() {
        let mut evm = make_evm();
        // caller defaults to Address::ZERO
        assert_eq!(evm.inner.ctx.tx.base.caller, Address::ZERO);

        set_keychain_tx_origin(&mut evm);

        // tstore with zero value removes the entry, tload returns zero
        let after = evm
            .inner
            .ctx
            .journal_mut()
            .tload(ACCOUNT_KEYCHAIN_ADDRESS, U256::from(3));
        assert_eq!(after, U256::ZERO, "zero caller should result in zero tx_origin");
    }

    // ==================== set_transaction_key tests ====================

    #[test]
    fn test_set_keychain_transaction_key_writes_to_slot_2() {
        let key_id = Address::with_last_byte(0xBB);
        let mut evm = make_evm();

        let before = evm
            .inner
            .ctx
            .journal_mut()
            .tload(ACCOUNT_KEYCHAIN_ADDRESS, U256::from(2));
        assert_eq!(before, U256::ZERO, "transaction_key should be zero before set");

        set_keychain_transaction_key(&mut evm, key_id);

        let after = evm
            .inner
            .ctx
            .journal_mut()
            .tload(ACCOUNT_KEYCHAIN_ADDRESS, U256::from(2));
        let expected: U256 = key_id.into_word().into();
        assert_eq!(after, expected, "transaction_key should equal key_id after set");
    }

    #[test]
    fn test_set_keychain_transaction_key_not_set_without_key_id() {
        let mut evm = make_evm();
        // No key_id → set_keychain_transaction_key not called
        // Verify slot 2 stays zero
        let val = evm
            .inner
            .ctx
            .journal_mut()
            .tload(ACCOUNT_KEYCHAIN_ADDRESS, U256::from(2));
        assert_eq!(val, U256::ZERO, "transaction_key should be zero when key_id absent");
    }

    // ==================== validate_time_window tests ====================

    #[test]
    fn test_validate_time_window_rejects_early() {
        let result = validate_time_window::<std::convert::Infallible>(
            Some(2000), // valid_after = 2000
            None,
            1000, // block_ts = 1000 < 2000
        );
        assert!(result.is_err(), "should reject: block_ts < valid_after");
    }

    #[test]
    fn test_validate_time_window_rejects_expired() {
        let result = validate_time_window::<std::convert::Infallible>(
            None,
            Some(1000), // valid_before = 1000
            1000,       // block_ts = 1000 >= 1000
        );
        assert!(result.is_err(), "should reject: block_ts >= valid_before");
    }

    #[test]
    fn test_validate_time_window_passes_in_range() {
        let result = validate_time_window::<std::convert::Infallible>(
            Some(1000), // valid_after = 1000
            Some(2000), // valid_before = 2000
            1500,       // block_ts = 1500 (in range)
        );
        assert!(result.is_ok(), "should pass: valid_after <= block_ts < valid_before");
    }

    #[test]
    fn test_validate_time_window_none_skips() {
        let result = validate_time_window::<std::convert::Infallible>(None, None, 999999);
        assert!(result.is_ok(), "should pass when both are None");
    }

    // ==================== per-auth keychain gas test ====================

    #[test]
    fn test_aa_gas_per_auth_keychain_adds_3000() {
        use crate::tempo::tx::{TempoAuthGas, TempoCall, TempoSigType, TempoTxFields};
        use revm::primitives::{Bytes, TxKind};

        let fields_no_keychain = TempoTxFields {
            aa_calls: vec![TempoCall {
                to: TxKind::Call(Address::with_last_byte(0x01)),
                value: U256::ZERO,
                input: Bytes::new(),
            }],
            auth_list: vec![TempoAuthGas {
                sig_type: TempoSigType::Secp256k1,
                nonce: 1,
                is_keychain: false,
                ..Default::default()
            }],
            ..Default::default()
        };

        let fields_keychain = TempoTxFields {
            aa_calls: vec![TempoCall {
                to: TxKind::Call(Address::with_last_byte(0x01)),
                value: U256::ZERO,
                input: Bytes::new(),
            }],
            auth_list: vec![TempoAuthGas {
                sig_type: TempoSigType::Secp256k1,
                nonce: 1,
                is_keychain: true,
                ..Default::default()
            }],
            ..Default::default()
        };

        let mut evm = make_evm();
        evm.inner.ctx.tx.base.gas_limit = 10_000_000;
        let gas_params = &evm.inner.ctx.cfg.gas_params;
        let hardfork = crate::tempo::hardfork::TempoHardfork::from_timestamp(1_770_908_500);

        evm.inner.ctx.tx.tempo_fields = Some(fields_no_keychain.clone());
        let gas_no_kc = calculate_aa_batch_intrinsic_gas(
            evm.ctx().tx.tempo_fields.as_ref().unwrap(),
            gas_params,
            &evm,
            hardfork,
        )
        .unwrap();

        evm.inner.ctx.tx.tempo_fields = Some(fields_keychain.clone());
        let gas_kc = calculate_aa_batch_intrinsic_gas(
            evm.ctx().tx.tempo_fields.as_ref().unwrap(),
            gas_params,
            &evm,
            hardfork,
        )
        .unwrap();

        assert_eq!(
            gas_kc.initial_gas - gas_no_kc.initial_gas,
            KEYCHAIN_VALIDATION_GAS,
            "keychain auth should add exactly {} gas",
            KEYCHAIN_VALIDATION_GAS
        );
    }

    // ==================== apply_eip7702_auth_list test ====================

    #[test]
    fn test_apply_eip7702_auth_list_sets_delegation_code() {
        use crate::tempo::tx::{TempoAuthGas, TempoTxFields};
        use revm::database::in_memory_db::CacheDB;
        use revm::primitives::Address;
        use revm::state::AccountInfo;

        let authority = Address::with_last_byte(0xAA);
        let delegate = Address::with_last_byte(0xDD);

        let mut db = CacheDB::new(EmptyDB::default());
        // Authority must exist with nonce=0 (matching auth nonce).
        db.insert_account_info(authority, AccountInfo { nonce: 0, ..Default::default() });

        let mut cfg = CfgEnv::new_with_spec(TempoHardfork::default());
        cfg.chain_id = 4217;
        let mut block_env = BlockEnv::default();
        block_env.timestamp = revm::primitives::U256::from(1_770_908_500u64);
        block_env.gas_limit = 100_000_000;
        let env = EvmEnv::new(cfg, block_env);
        let mut evm = TempoEvm::new(env, db, NoOpInspector, false);

        evm.inner.ctx.tx.tempo_fields = Some(TempoTxFields {
            auth_list: vec![TempoAuthGas {
                nonce: 0,
                authority: Some(authority),
                delegate: Some(delegate),
                chain_id: Some(U256::from(4217)),
                ..Default::default()
            }],
            ..Default::default()
        });

        let handler = TempoHandler::<_, NoOpInspector>::new();
        let refund = handler.apply_eip7702_auth_list(&mut evm).unwrap();

        // T1+ → no refund
        assert_eq!(refund, 0, "T1+ should return 0 refund");

        // Verify authority's code is set to EIP-7702 delegation.
        use revm::context_interface::JournalTr;
        let acc = evm.inner.ctx.journal_mut().load_account(authority).unwrap();
        let code = acc.data.info.code.as_ref().expect("authority should have code");
        assert!(
            code.is_eip7702(),
            "authority's code should be EIP-7702 delegation, got: {:?}",
            code
        );
    }

    #[test]
    fn test_apply_eip7702_auth_list_skips_gas_only_entries() {
        use crate::tempo::tx::{TempoAuthGas, TempoTxFields};

        let mut evm = make_evm();
        evm.inner.ctx.tx.tempo_fields = Some(TempoTxFields {
            auth_list: vec![TempoAuthGas {
                nonce: 1,
                is_keychain: false,
                // No authority/delegate → gas-only entry
                ..Default::default()
            }],
            ..Default::default()
        });

        let handler = TempoHandler::<_, NoOpInspector>::new();
        // Should not panic and should delegate to MainnetHandler (which returns 0 for non-0x04)
        let result = handler.apply_eip7702_auth_list(&mut evm);
        assert!(result.is_ok());
    }

    // ==================== validate_env rejection tests ====================

    /// Helper: construct an AA tx for validate_env tests.
    fn make_aa_tx_for_validate(
        calls: Vec<crate::tempo::tx::TempoCall>,
    ) -> crate::tempo::tx::TempoTxEnv {
        use crate::tempo::tx::{TempoTxEnv, TempoTxFields};
        use revm::primitives::Address;
        TempoTxEnv {
            base: revm::context::TxEnv {
                caller: Address::with_last_byte(0x01),
                gas_limit: 10_000_000,
                nonce: 1,
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: Some(TempoTxFields {
                aa_calls: calls,
                ..Default::default()
            }),
        }
    }

    #[test]
    fn test_validate_env_rejects_value_transfer() {
        use crate::tempo::tx::TempoTxEnv;

        let mut evm = make_evm();
        let tx = TempoTxEnv {
            base: revm::context::TxEnv {
                caller: Address::with_last_byte(0x01),
                gas_limit: 10_000_000,
                value: U256::from(1u64), // non-zero value
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: None,
        };
        let result = evm.transact(tx);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("value transfer not allowed"),
            "expected 'value transfer not allowed', got: {err}"
        );
    }

    #[test]
    fn test_validate_env_rejects_empty_calls() {
        let mut evm = make_evm();
        let tx = make_aa_tx_for_validate(vec![]); // empty aa_calls
        let result = evm.transact(tx);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("AA calls list cannot be empty"),
            "expected 'AA calls list cannot be empty', got: {err}"
        );
    }

    #[test]
    fn test_validate_env_rejects_create_not_first() {
        use crate::tempo::tx::TempoCall;
        use revm::primitives::{Bytes, TxKind};

        let mut evm = make_evm();
        let calls = vec![
            TempoCall {
                to: TxKind::Call(Address::with_last_byte(0x01)),
                value: U256::ZERO,
                input: Bytes::new(),
            },
            TempoCall {
                to: TxKind::Create, // CREATE as second call
                value: U256::ZERO,
                input: Bytes::new(),
            },
        ];
        let tx = make_aa_tx_for_validate(calls);
        let result = evm.transact(tx);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("only the first call"),
            "expected 'only the first call', got: {err}"
        );
    }

    #[test]
    fn test_validate_env_rejects_create_with_auth_list() {
        use crate::tempo::tx::{TempoAuthGas, TempoCall, TempoTxEnv, TempoTxFields};
        use revm::primitives::{Bytes, TxKind};

        let mut evm = make_evm();
        let tx = TempoTxEnv {
            base: revm::context::TxEnv {
                caller: Address::with_last_byte(0x01),
                gas_limit: 10_000_000,
                nonce: 1,
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: Some(TempoTxFields {
                aa_calls: vec![TempoCall {
                    to: TxKind::Create,
                    value: U256::ZERO,
                    input: Bytes::new(),
                }],
                auth_list: vec![TempoAuthGas {
                    sig_type: crate::tempo::tx::TempoSigType::Secp256k1,
                    nonce: 1,
                    ..Default::default()
                }],
                ..Default::default()
            }),
        };
        let result = evm.transact(tx);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("calls cannot contain CREATE when authorization list"),
            "expected 'calls cannot contain CREATE when authorization list', got: {err}"
        );
    }

    #[test]
    fn test_validate_env_rejects_expiring_nonce_without_valid_before() {
        use crate::tempo::tx::{TempoCall, TempoTxEnv, TempoTxFields};
        use revm::primitives::{Bytes, TxKind};

        let mut evm = make_evm();
        let tx = TempoTxEnv {
            base: revm::context::TxEnv {
                caller: Address::with_last_byte(0x01),
                gas_limit: 10_000_000,
                nonce: 1,
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: Some(TempoTxFields {
                aa_calls: vec![TempoCall {
                    to: TxKind::Call(Address::with_last_byte(0x01)),
                    value: U256::ZERO,
                    input: Bytes::new(),
                }],
                nonce_key: U256::MAX,      // expiring nonce
                valid_before: None,         // missing valid_before
                ..Default::default()
            }),
        };
        let result = evm.transact(tx);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("expiring nonce transaction requires valid_before"),
            "expected 'expiring nonce transaction requires valid_before', got: {err}"
        );
    }

    #[test]
    fn test_validate_env_allows_system_tx() {
        use crate::tempo::tx::TempoTxEnv;

        let mut evm = make_evm();
        // System tx: caller = Address::ZERO, gas_limit = 0
        let tx = TempoTxEnv {
            base: revm::context::TxEnv {
                caller: Address::ZERO,
                gas_limit: 0,
                chain_id: Some(4217),
                ..Default::default()
            },
            tempo_fields: None,
        };
        // System tx should pass validate_env (though execution may fail later)
        // We only check it doesn't fail at the validate_env step.
        // transact() calls validate_env first, then validate_initial_tx_gas, etc.
        // System tx has gas_limit=0, validate_initial_tx_gas returns default (0).
        // Execution with 0 gas will hit OOG, but that's a Halt, not an Err.
        let result = evm.transact(tx);
        // Should not be an Err from validate_env — it should be Ok (possibly with a Halt)
        assert!(
            result.is_ok(),
            "system tx should not error at validate_env, got: {:?}",
            result.err()
        );
    }

    // ==================== key authorization gas tests ====================

    #[test]
    fn test_aa_gas_key_authorization() {
        use crate::tempo::tx::{TempoCall, TempoKeyAuthGas, TempoSigType, TempoTxFields};
        use revm::primitives::{Bytes, TxKind};

        let make_fields = |key_auth: Option<TempoKeyAuthGas>| TempoTxFields {
            aa_calls: vec![TempoCall {
                to: TxKind::Call(Address::with_last_byte(0x01)),
                value: U256::ZERO,
                input: Bytes::new(),
            }],
            key_auth,
            ..Default::default()
        };

        let fields_none = make_fields(None);
        let fields_with = make_fields(Some(TempoKeyAuthGas {
            sig_type: TempoSigType::Secp256k1,
            num_limits: 0,
            scope_counts: Default::default(),
        }));

        let mut evm = make_evm();
        evm.inner.ctx.tx.base.gas_limit = 10_000_000;
        let gas_params = &evm.inner.ctx.cfg.gas_params;
        let hardfork = crate::tempo::hardfork::TempoHardfork::from_timestamp(1_770_908_500);

        evm.inner.ctx.tx.tempo_fields = Some(fields_none.clone());
        let gas_none = calculate_aa_batch_intrinsic_gas(
            evm.ctx().tx.tempo_fields.as_ref().unwrap(),
            gas_params,
            &evm,
            hardfork,
        )
        .unwrap();

        evm.inner.ctx.tx.tempo_fields = Some(fields_with.clone());
        let gas_with = calculate_aa_batch_intrinsic_gas(
            evm.ctx().tx.tempo_fields.as_ref().unwrap(),
            gas_params,
            &evm,
            hardfork,
        )
        .unwrap();

        // key_auth with secp256k1 and 0 limits:
        // key_auth_gas = ECRECOVER_GAS + primitive_sig_gas(Secp256k1, 0) + KEY_AUTH_BASE_GAS
        //              = 3000 + 0 + 27000 = 30000
        let expected_diff = KEY_AUTH_BASE_GAS + ECRECOVER_GAS;
        assert_eq!(
            gas_with.initial_gas - gas_none.initial_gas,
            expected_diff,
            "key_auth (secp256k1, 0 limits) should add {} gas",
            expected_diff
        );
    }

    #[test]
    fn test_aa_gas_key_authorization_with_limits() {
        use crate::tempo::tx::{TempoCall, TempoKeyAuthGas, TempoSigType, TempoTxFields};
        use revm::primitives::{Bytes, TxKind};

        let make_fields = |num_limits: u32| TempoTxFields {
            aa_calls: vec![TempoCall {
                to: TxKind::Call(Address::with_last_byte(0x01)),
                value: U256::ZERO,
                input: Bytes::new(),
            }],
            key_auth: Some(TempoKeyAuthGas {
                sig_type: TempoSigType::Secp256k1,
                num_limits,
                scope_counts: Default::default(),
            }),
            ..Default::default()
        };

        let mut evm = make_evm();
        evm.inner.ctx.tx.base.gas_limit = 10_000_000;
        let gas_params = &evm.inner.ctx.cfg.gas_params;
        let hardfork = crate::tempo::hardfork::TempoHardfork::from_timestamp(1_770_908_500);

        evm.inner.ctx.tx.tempo_fields = Some(make_fields(0));
        let gas_0 = calculate_aa_batch_intrinsic_gas(
            evm.ctx().tx.tempo_fields.as_ref().unwrap(),
            gas_params,
            &evm,
            hardfork,
        )
        .unwrap();

        evm.inner.ctx.tx.tempo_fields = Some(make_fields(3));
        let gas_3 = calculate_aa_batch_intrinsic_gas(
            evm.ctx().tx.tempo_fields.as_ref().unwrap(),
            gas_params,
            &evm,
            hardfork,
        )
        .unwrap();

        // Pre-T1B: each limit costs KEY_AUTH_PER_LIMIT_GAS (22000).
        // With 3 limits: 3 * 22000 = 66000 more than 0 limits.
        let expected_diff = 3 * KEY_AUTH_PER_LIMIT_GAS;
        assert_eq!(
            gas_3.initial_gas - gas_0.initial_gas,
            expected_diff,
            "3 limits should add {} gas (3 * {})",
            expected_diff,
            KEY_AUTH_PER_LIMIT_GAS
        );
    }

    // ==================== 2D nonce increment tests ====================

    #[test]
    fn test_increment_2d_nonce_writes_to_journal() {
        use crate::tempo::precompile::NONCE_PRECOMPILE_ADDRESS;
        use crate::tempo::precompile::storage_types::StorageKey;
        use revm::context_interface::JournalTr;

        let caller = Address::with_last_byte(0xAA);
        let nonce_key = U256::from(1);
        let mut evm = make_evm();
        evm.inner.ctx.tx.base.caller = caller;
        evm.inner.ctx.tx.tempo_fields = Some(crate::tempo::tx::TempoTxFields {
            nonce_key,
            ..Default::default()
        });

        // Before: nonce should be 0
        let slot = caller.mapping_slot(U256::ZERO);
        let slot = nonce_key.mapping_slot(slot);
        let _ = evm.inner.ctx.journal_mut().load_account(NONCE_PRECOMPILE_ADDRESS);
        let before = evm
            .inner
            .ctx
            .journal_mut()
            .sload(NONCE_PRECOMPILE_ADDRESS, slot)
            .map(|r| r.data.saturating_to::<u64>())
            .unwrap_or(0);
        assert_eq!(before, 0, "nonce should be 0 before increment");

        // Act
        increment_2d_nonce_if_needed(&mut evm);

        // After: nonce should be 1
        let after = evm
            .inner
            .ctx
            .journal_mut()
            .sload(NONCE_PRECOMPILE_ADDRESS, slot)
            .map(|r| r.data.saturating_to::<u64>())
            .unwrap_or(0);
        assert_eq!(after, 1, "nonce should be 1 after increment");
    }

    #[test]
    fn test_increment_2d_nonce_skips_protocol_nonce() {
        let mut evm = make_evm();
        evm.inner.ctx.tx.tempo_fields = Some(crate::tempo::tx::TempoTxFields {
            nonce_key: U256::ZERO, // protocol nonce
            ..Default::default()
        });
        // Should not panic or modify anything
        increment_2d_nonce_if_needed(&mut evm);
    }

    #[test]
    fn test_increment_2d_nonce_skips_expiring_nonce() {
        let mut evm = make_evm();
        evm.inner.ctx.tx.tempo_fields = Some(crate::tempo::tx::TempoTxFields {
            nonce_key: U256::MAX, // expiring nonce
            ..Default::default()
        });
        // Should not panic or modify anything
        increment_2d_nonce_if_needed(&mut evm);
    }

    #[test]
    fn test_increment_2d_nonce_no_tempo_fields() {
        let mut evm = make_evm();
        // No tempo_fields at all (standard tx)
        increment_2d_nonce_if_needed(&mut evm);
        // Should not panic
    }

    // ========================================================================
    // key_auth_gas hardfork branch tests (T1B / T2 / T3 / T4)
    // ========================================================================

    fn gas_params_for(hf: TempoHardfork) -> GasParams {
        let mut gp = GasParams::new_spec(hf.into());
        if hf.is_t1() {
            gp.override_gas([
                (GasId::sstore_set_without_load_cost(), 250_000),
                (GasId::create(), 500_000),
                (GasId::tx_create_cost(), 500_000),
                (GasId::new_account_cost(), 250_000),
                (GasId::new_account_cost_for_selfdestruct(), 250_000),
                (GasId::code_deposit_cost(), 1_000),
                (GasId::tx_eip7702_per_empty_account_cost(), 12_500),
                (GasId::new(255), 250_000),
            ]);
        }
        gp
    }

    /// Builds the precise-SSTORE branch (T1B+) expected value for a given
    /// `limit_slots` count and an optional T4 base-scope surcharge.
    fn expected_t1b_plus(
        hf: TempoHardfork,
        sig_type: TempoSigType,
        limit_slots: u64,
        scope_extra: u64,
    ) -> u64 {
        const BUFFER: u64 = 2_000;
        let gp = gas_params_for(hf);
        let sig_gas = ECRECOVER_GAS + primitive_sig_gas(sig_type, 0);
        let sstore_cost = gp.get(GasId::sstore_set_without_load_cost());
        let sload_cost = gp.warm_storage_read_cost() + gp.cold_storage_additional_cost();
        sig_gas + sload_cost + sstore_cost * (1 + limit_slots) + BUFFER + scope_extra
    }

    #[test]
    fn key_auth_gas_pre_t1b_uses_heuristic() {
        let gp = gas_params_for(TempoHardfork::T1A);
        let g = key_auth_gas(
            TempoSigType::Secp256k1,
            2,
            &ScopeCounts::default(),
            &gp,
            TempoHardfork::T1A,
        );
        // Pre-T1B: KEY_AUTH_BASE_GAS + sig_gas(ECRECOVER) + 2 * KEY_AUTH_PER_LIMIT_GAS
        let expected = KEY_AUTH_BASE_GAS + ECRECOVER_GAS + 2 * KEY_AUTH_PER_LIMIT_GAS;
        assert_eq!(g, expected);
    }

    #[test]
    fn key_auth_gas_t1b_uses_precise_sstore() {
        let g = key_auth_gas(
            TempoSigType::Secp256k1,
            2,
            &ScopeCounts::default(),
            &gas_params_for(TempoHardfork::T1B),
            TempoHardfork::T1B,
        );
        let expected = expected_t1b_plus(TempoHardfork::T1B, TempoSigType::Secp256k1, 2, 0);
        assert_eq!(g, expected);
    }

    #[test]
    fn key_auth_gas_t2_matches_t1b_formula() {
        let g = key_auth_gas(
            TempoSigType::Secp256k1,
            3,
            &ScopeCounts::default(),
            &gas_params_for(TempoHardfork::T2),
            TempoHardfork::T2,
        );
        let expected = expected_t1b_plus(TempoHardfork::T2, TempoSigType::Secp256k1, 3, 0);
        assert_eq!(g, expected);
    }

    #[test]
    fn key_auth_gas_t3_doubles_limit_slots() {
        // T3 stores 2 slots per spending limit; no scopes -> scope_slots = 0.
        let g = key_auth_gas(
            TempoSigType::Secp256k1,
            2,
            &ScopeCounts::default(),
            &gas_params_for(TempoHardfork::T3),
            TempoHardfork::T3,
        );
        let expected = expected_t1b_plus(TempoHardfork::T3, TempoSigType::Secp256k1, 2 * 2, 0);
        assert_eq!(g, expected);
    }

    #[test]
    fn key_auth_gas_t4_adds_base_scope_gas() {
        // T4, no scope tree -> scope_slots = 0, extra_gas = BASE_SCOPE_GAS.
        let g = key_auth_gas(
            TempoSigType::Secp256k1,
            2,
            &ScopeCounts::default(),
            &gas_params_for(TempoHardfork::T4),
            TempoHardfork::T4,
        );
        let expected = expected_t1b_plus(
            TempoHardfork::T4,
            TempoSigType::Secp256k1,
            2 * 2,
            BASE_SCOPE_GAS,
        );
        assert_eq!(g, expected);
    }

    #[test]
    fn key_auth_gas_zero_limits_per_fork() {
        // Sanity: num_limits = 0 still pays the key-write SSTORE + buffer.
        for hf in [
            TempoHardfork::T1B,
            TempoHardfork::T2,
            TempoHardfork::T3,
            TempoHardfork::T4,
        ] {
            let g = key_auth_gas(
                TempoSigType::Secp256k1,
                0,
                &ScopeCounts::default(),
                &gas_params_for(hf),
                hf,
            );
            let scope_extra = if hf.is_t4() { BASE_SCOPE_GAS } else { 0 };
            assert_eq!(
                g,
                expected_t1b_plus(hf, TempoSigType::Secp256k1, 0, scope_extra),
                "zero-limit gas mismatch on {hf:?}"
            );
        }
    }

    // ------------------------------------------------------------------------
    // call_scope_storage_slots / call_scope_extra_gas (Commit 12)
    // ------------------------------------------------------------------------

    #[test]
    fn call_scope_storage_slots_none() {
        // has_allowed_calls = false -> 0
        let s = ScopeCounts::default();
        assert_eq!(call_scope_storage_slots(&s, TempoHardfork::T3), 0);
        assert_eq!(call_scope_storage_slots(&s, TempoHardfork::T4), 0);
    }

    #[test]
    fn call_scope_storage_slots_empty_list() {
        // has_allowed_calls = true, empty list -> 1
        let s = ScopeCounts {
            has_allowed_calls: true,
            ..Default::default()
        };
        assert_eq!(call_scope_storage_slots(&s, TempoHardfork::T3), 1);
        assert_eq!(call_scope_storage_slots(&s, TempoHardfork::T4), 1);
    }

    #[test]
    fn call_scope_storage_slots_t3_formula() {
        // 1 target, 1 selector, 1 recipient (constrained)
        let s = ScopeCounts {
            has_allowed_calls: true,
            scopes: 1,
            selectors: 1,
            constrained_selectors: 1,
            recipients: 1,
        };
        // T3 = 1 + 1*3 + 1*3 + 1 + 1*2 = 10
        assert_eq!(call_scope_storage_slots(&s, TempoHardfork::T3), 10);
    }

    #[test]
    fn call_scope_storage_slots_t4_formula() {
        let s = ScopeCounts {
            has_allowed_calls: true,
            scopes: 1,
            selectors: 1,
            constrained_selectors: 1,
            recipients: 1,
        };
        // T4 = 1 + 1*2 + 1 + 1*2 + 1 (selector_set since selectors > 0) + 1 + 1*2 = 10
        assert_eq!(call_scope_storage_slots(&s, TempoHardfork::T4), 10);

        // 2 targets, 3 selectors, 2 constrained, 5 recipients:
        // T4 = 1 + 2*2 + 1 + 3*2 + 2 (selector_set per scope, 2 scopes have selectors) + 2 + 5*2
        //    = 1 + 4 + 1 + 6 + 2 + 2 + 10 = 26
        let s = ScopeCounts {
            has_allowed_calls: true,
            scopes: 2,
            selectors: 3,
            constrained_selectors: 2,
            recipients: 5,
        };
        assert_eq!(call_scope_storage_slots(&s, TempoHardfork::T4), 26);
    }

    #[test]
    fn call_scope_extra_gas_no_scopes() {
        let s = ScopeCounts::default();
        // has_allowed_calls = false still pays BASE.
        assert_eq!(call_scope_extra_gas(&s), BASE_SCOPE_GAS);
    }

    #[test]
    fn call_scope_extra_gas_with_scopes() {
        // 2 targets, 3 selectors, 5 recipients -> base + 2*TARGET + 3*SELECTOR + 5*RECIPIENT
        let s = ScopeCounts {
            has_allowed_calls: true,
            scopes: 2,
            selectors: 3,
            constrained_selectors: 2,
            recipients: 5,
        };
        let expected =
            BASE_SCOPE_GAS + 2 * TARGET_SCOPE_GAS + 3 * SELECTOR_SCOPE_GAS + 5 * RECIPIENT_SCOPE_GAS;
        assert_eq!(call_scope_extra_gas(&s), expected);
    }

    #[test]
    fn key_auth_gas_t4_with_scope_counts() {
        let s = ScopeCounts {
            has_allowed_calls: true,
            scopes: 1,
            selectors: 1,
            constrained_selectors: 1,
            recipients: 1,
        };
        let g = key_auth_gas(
            TempoSigType::Secp256k1,
            0,
            &s,
            &gas_params_for(TempoHardfork::T4),
            TempoHardfork::T4,
        );
        // Expected = T4 baseline (limit_slots=0, scope_slots=10) + extra_gas.
        // = sig + sload + sstore*(1 + 0 + 10) + BUFFER + extra_gas
        let gp = gas_params_for(TempoHardfork::T4);
        let sig_gas = ECRECOVER_GAS + primitive_sig_gas(TempoSigType::Secp256k1, 0);
        let sstore = gp.get(GasId::sstore_set_without_load_cost());
        let sload = gp.warm_storage_read_cost() + gp.cold_storage_additional_cost();
        let extra = BASE_SCOPE_GAS + 1 * TARGET_SCOPE_GAS + 1 * SELECTOR_SCOPE_GAS + 1 * RECIPIENT_SCOPE_GAS;
        let expected = sig_gas + sload + sstore * (1 + 0 + 10) + 2_000 + extra;
        assert_eq!(g, expected);
    }
}
