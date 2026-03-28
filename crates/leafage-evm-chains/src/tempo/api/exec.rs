use crate::tempo::api::{TempoContext, TempoEvm};
use crate::tempo::hardfork::TempoHardfork;
use crate::tempo::tx::{TempoCall, TempoSigType, TempoTxEnv, TempoTxFields};
use alloy_evm::Database;
use revm::{
    context::{BlockEnv, ContextSetters},
    context_interface::{
        cfg::gas_params::{GasId, GasParams},
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

            // Note: keychain version, subblock, priority fee, and time window validations
            // are skipped — leafage eth_call mode has no real signatures, no subblock txs,
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

        if is_aa {
            // AA transaction — use batch gas calculation.
            validate_aa_initial_tx_gas(evm)
        } else {
            // Standard transaction — delegate to mainnet handler.
            let mut init_gas = MainnetHandler::<Self::Evm, Self::Error, EthFrame>::default()
                .validate_initial_tx_gas(evm)?;

            // TIP-1000: nonce == 0 requires additional new_account_cost (250k gas).
            let hardfork = TempoHardfork::from_timestamp(
                evm.ctx().block.timestamp.saturating_to::<u64>(),
            );
            if hardfork.is_t1() && evm.ctx().tx.base.nonce == 0 {
                init_gas.initial_gas += evm.ctx().cfg.gas_params.get(GasId::new_account_cost());

                // Re-validate gas_limit after adding surcharge.
                // Without this, gas_limit - init_gas underflows in execution(),
                // giving the precompile near-infinite gas.
                let gas_limit = evm.ctx().tx.base.gas_limit;
                if gas_limit < init_gas.initial_gas {
                    return Err(EVMError::Custom(format!(
                        "insufficient gas for intrinsic cost: gas_limit {} < intrinsic_gas {}",
                        gas_limit, init_gas.initial_gas
                    )));
                }
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

        Ok(gas)
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
    let mut batch_gas = calculate_aa_batch_intrinsic_gas(tempo_fields, gas_params, evm, hardfork)?;

    // Calculate 2D nonce gas based on hardfork and nonce_key.
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

/// Returns the standard tx token cost (16 gas per token).
/// Equivalent to `gas_params.tx_token_cost()` but as a standalone const
/// since we don't have GasParams in the sig gas helper.
#[inline]
fn gas_params_tx_token_cost() -> u64 {
    // Standard value from revm GasParams::tx_token_cost() for post-Istanbul.
    16
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

/// Key authorization gas calculation.
/// Pre-T1B: heuristic constants. T1B+: accurate storage-based calculation.
/// Ported from Tempo writer: `calculate_key_authorization_gas`.
#[inline]
fn key_auth_gas(
    sig_type: TempoSigType,
    num_limits: u32,
    gas_params: &GasParams,
    hardfork: TempoHardfork,
) -> u64 {
    let sig_gas = ECRECOVER_GAS + primitive_sig_gas(sig_type, 0);
    let num_limits = num_limits as u64;

    if hardfork.is_t1b() {
        const BUFFER: u64 = 2_000;
        let sstore_cost = gas_params.get(GasId::sstore_set_without_load_cost());
        let sload_cost =
            gas_params.warm_storage_read_cost() + gas_params.cold_storage_additional_cost();
        sig_gas + sload_cost + sstore_cost * (1 + num_limits) + BUFFER
    } else {
        KEY_AUTH_BASE_GAS + sig_gas + num_limits * KEY_AUTH_PER_LIMIT_GAS
    }
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
        gas.initial_gas += primitive_sig_gas(auth.sig_type, 0);
        // TIP-1000: auth with nonce==0 incurs 250k account creation cost.
        if auth.nonce == 0 {
            gas.initial_gas += gas_params.get(TIP1000_AUTH_ACCOUNT_CREATION_GAS_ID);
        }
    }

    // 5. Key authorization costs (if present).
    if let Some(ka) = &fields.key_auth {
        gas.initial_gas += key_auth_gas(ka.sig_type, ka.num_limits, gas_params, hardfork);
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
