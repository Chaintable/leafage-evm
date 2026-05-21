use crate::api_impl::core::{ApiCore, EvmExecutor, GasFeeHandler, TxSetter};
use revm::context::Transaction as TransactionTrait;
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::tempo::tx::TempoTxEnv;
use leafage_evm_chains::tempo::TempoEvm;
use leafage_evm_chains::tempo::hardfork::TempoHardfork;
use leafage_evm_types::{BlockEnv, CallRequest};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

/// DB wrapper that injects `0xef` bytecode for ValidatorConfigV2 at T2+ blocks.
///
/// Writer injects this code via `apply_pre_execution_changes` at the T2 activation
/// block. Pipeline may not sync this state change. This wrapper transparently makes
/// VCV2 appear to have code, fixing the +2100 cold-access gas diff.
#[derive(Debug)]
struct Vcv2CodeInjector<DB> {
    inner: DB,
    is_t2: bool,
}

impl<DB> Vcv2CodeInjector<DB> {
    fn new(inner: DB, block_timestamp: u64) -> Self {
        let is_t2 = leafage_evm_chains::tempo::hardfork::TempoHardfork::from_timestamp(
            block_timestamp,
        )
        .is_t2();
        Self { inner, is_t2 }
    }
}

impl<DB: DatabaseCommit> DatabaseCommit for Vcv2CodeInjector<DB> {
    fn commit(&mut self, changes: revm::state::EvmState) {
        self.inner.commit(changes);
    }
}

impl<DB: DatabaseRef> DatabaseRef for Vcv2CodeInjector<DB> {
    type Error = DB::Error;

    fn basic_ref(
        &self,
        address: revm::primitives::Address,
    ) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
        let mut info = self.inner.basic_ref(address)?;
        if self.is_t2
            && address == leafage_evm_chains::tempo::precompile::VALIDATOR_CONFIG_V2_ADDRESS
        {
            let code = revm::bytecode::Bytecode::new_legacy(
                alloy::primitives::Bytes::from_static(&[0xef]),
            );
            let acc = info.get_or_insert_with(Default::default);
            if acc.is_empty_code_hash() {
                acc.code_hash = code.hash_slow();
                acc.code = Some(code);
            }
        }
        Ok(info)
    }

    fn code_by_hash_ref(
        &self,
        code_hash: revm::primitives::B256,
    ) -> Result<revm::bytecode::Bytecode, Self::Error> {
        self.inner.code_by_hash_ref(code_hash)
    }

    fn storage_ref(
        &self,
        address: revm::primitives::Address,
        index: revm::primitives::U256,
    ) -> Result<revm::primitives::U256, Self::Error> {
        self.inner.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<revm::primitives::B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }
}

/// Marker type to differentiate `TempoApiImpl` from `MainnetApiImpl`.
///
#[derive(Debug, Clone)]
pub struct TempoEvmCustomConfig;

type TempoApiImpl<DB> = ApiImpl<DB, TempoHardfork, TempoEvmCustomConfig>;

/// Derive `ScopeCounts` from a TIP-1011 `allowedCalls` wire value.
///
/// `None` → all zero, `has_allowed_calls = false`, which selects the gas
/// formula's pre-T3 branch.
/// `Some(&[])` → `has_allowed_calls = true` with all counts zero (scoped
/// deny-all; gas reflects the `BASE_SCOPE_GAS` + storage-slot reservation).
/// `Some(scopes)` → counts of targets, selectors, recipient-bearing selectors,
/// and total recipients. Matches writer's per-field accounting in
/// `crates/revm/src/handler.rs:203-244`.
fn derive_scope_counts(
    allowed_calls: Option<&[leafage_evm_types::CallScope]>,
) -> leafage_evm_chains::tempo::tx::ScopeCounts {
    use leafage_evm_chains::tempo::tx::ScopeCounts;
    match allowed_calls {
        None => ScopeCounts::default(),
        Some(scopes) => {
            let selectors_total: usize =
                scopes.iter().map(|s| s.selector_rules.len()).sum();
            let constrained_selectors: usize = scopes
                .iter()
                .flat_map(|s| &s.selector_rules)
                .filter(|r| !r.recipients.is_empty())
                .count();
            let recipients_total: usize = scopes
                .iter()
                .flat_map(|s| &s.selector_rules)
                .map(|r| r.recipients.len())
                .sum();
            ScopeCounts {
                has_allowed_calls: true,
                scopes: scopes.len() as u32,
                selectors: selectors_total as u32,
                constrained_selectors: constrained_selectors as u32,
                recipients: recipients_total as u32,
            }
        }
    }
}

impl<DB> EvmExecutor for TempoApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TempoTxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        mut request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        use leafage_evm_chains::tempo::tx::{
            TempoAuthGas, TempoCall, TempoKeyAuthGas, TempoSigType, TempoTxFields,
        };
        use revm::primitives::TxKind;

        // Extract Tempo-specific fields before consuming the request.
        let te = request.tempo.clone().unwrap_or_default();
        let tempo_calls = te.tempo_calls;
        let nonce_key = te.nonce_key;
        let key_type = te.key_type;
        let key_data = te.key_data;
        let key_id = te.key_id;
        let key_authorization = te.key_authorization;
        let tempo_authorization_list = te.tempo_authorization_list;
        let fee_token = te.fee_token;
        let fee_payer = te.fee_payer;
        let valid_after = te.valid_after;
        let valid_before = te.valid_before;

        // Auto-fill 2D nonce from NonceManager storage when not provided.
        // Ported from writer compat.rs:309-324.
        if let Some(nk) = nonce_key {
            if !nk.is_zero() && request.inner.nonce.is_none() {
                use leafage_evm_chains::tempo::precompile::NONCE_PRECOMPILE_ADDRESS;
                use leafage_evm_chains::tempo::precompile::storage_types::StorageKey;
                let nonce = if nk == revm::primitives::U256::MAX {
                    0u64 // expiring nonce must be 0
                } else {
                    let caller = request.inner.from.unwrap_or_default();
                    let slot = caller.mapping_slot(revm::primitives::U256::ZERO);
                    let slot = nk.mapping_slot(slot);
                    db.storage_ref(NONCE_PRECOMPILE_ADDRESS, slot)
                        .map(|v| v.saturating_to::<u64>())
                        .unwrap_or(0)
                };
                request.inner.nonce = Some(nonce);
            }
        }

        // Build standard TxEnv.
        let base =
            create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)?;

        // Determine if this is an AA transaction (same logic as writer compat.rs:136-144).
        let has_aa_fields = tempo_calls.as_ref().is_some_and(|c| !c.is_empty())
            || tempo_authorization_list
                .as_ref()
                .is_some_and(|l| !l.is_empty())
            || nonce_key.is_some()
            || key_authorization.is_some()
            || key_id.is_some();

        let tempo_fields = if has_aa_fields {
            // Parse signature type.
            let sig_type = key_type
                .as_deref()
                .map(TempoSigType::from_str_lossy)
                .unwrap_or_default();

            // WebAuthn data size for calldata gas.
            let webauthn_data_size = if sig_type == TempoSigType::WebAuthn {
                parse_webauthn_size(key_data.as_ref())
            } else {
                0
            };

            // Key authorization gas info. `scope_counts` is derived from the
            // TIP-1011 `allowedCalls` list on the wire if present; otherwise
            // ScopeCounts::default() (has_allowed_calls = false) and the gas
            // formula's pre-T3 branch fires.
            let key_auth = key_authorization.map(|ka| TempoKeyAuthGas {
                sig_type: ka
                    .sig_type
                    .as_deref()
                    .map(TempoSigType::from_str_lossy)
                    .unwrap_or_default(),
                num_limits: ka.num_limits,
                scope_counts: derive_scope_counts(ka.allowed_calls.as_deref()),
            });

            // Tempo authorization list: gas info + optional delegation fields.
            let auth_list = tempo_authorization_list
                .unwrap_or_default()
                .into_iter()
                .map(|a| TempoAuthGas {
                    sig_type: a
                        .sig_type
                        .as_deref()
                        .map(TempoSigType::from_str_lossy)
                        .unwrap_or_default(),
                    nonce: a.nonce,
                    is_keychain: a.is_keychain,
                    authority: a.authority,
                    delegate: a.address,
                    chain_id: a.chain_id,
                })
                .collect();

            // Build AA calls from `tempo_calls` + the outer to/value/input (same as writer).
            let mut aa_calls: Vec<TempoCall> = tempo_calls
                .unwrap_or_default()
                .into_iter()
                .map(|c| TempoCall {
                    to: c.to.unwrap_or(TxKind::Create),
                    value: c.value.unwrap_or_default(),
                    input: c.input.into_input().unwrap_or_default(),
                })
                .collect();

            // Writer appends outer to/value/input as the last call (compat.rs:158-163).
            if let Some(to) = base.kind.to() {
                aa_calls.push(TempoCall {
                    to: TxKind::Call(*to),
                    value: base.value,
                    input: base.data.clone(),
                });
            }

            Some(TempoTxFields {
                aa_calls,
                nonce_key: nonce_key.unwrap_or_default(),
                sig_type,
                is_keychain: key_id.is_some(),
                webauthn_data_size,
                key_auth,
                auth_list,
                key_id,
                fee_token,
                fee_payer,
                valid_after,
                valid_before,
            })
        } else {
            None
        };

        Ok(TempoTxEnv {
            base,
            tempo_fields,
        })
    }

    fn apply_pre_execution_changes<StateDB>(
        &self,
        _header: impl alloy::consensus::BlockHeader,
        block_env: &BlockEnv,
        state: &mut StateDB,
    ) -> RpcResult<()>
    where
        StateDB: revm::DatabaseCommit + revm::DatabaseRef + core::fmt::Debug,
        StateDB::Error: Sync + Send + 'static,
    {
        use leafage_evm_chains::tempo::hardfork::TempoHardfork;
        use leafage_evm_chains::tempo::precompile::VALIDATOR_CONFIG_V2_ADDRESS;

        // T2: inject 0xef bytecode into ValidatorConfigV2 if not already present.
        // Writer does this in apply_pre_execution_changes at T2 activation.
        // Pipeline may not sync this code change, so we inject it on every T2+ call.
        let ts: u64 = block_env.timestamp.saturating_to();
        if TempoHardfork::from_timestamp(ts).is_t2() {
            let has_code = state
                .basic_ref(VALIDATOR_CONFIG_V2_ADDRESS)
                .ok()
                .flatten()
                .map(|acc| !acc.is_empty_code_hash())
                .unwrap_or(false);
            if !has_code {
                use revm::state::{Account, AccountInfo, AccountStatus};
                use revm::bytecode::Bytecode;
                let code = Bytecode::new_legacy(alloy::primitives::Bytes::from_static(&[0xef]));
                let mut acc = Account {
                    info: AccountInfo {
                        code_hash: code.hash_slow(),
                        code: Some(code),
                        nonce: 1,
                        ..Default::default()
                    },
                    status: AccountStatus::Touched,
                    ..Default::default()
                };
                acc.mark_created();
                let mut changes = revm::state::EvmState::default();
                changes.insert(VALIDATOR_CONFIG_V2_ADDRESS, acc);
                state.commit(changes);
            }
        }
        Ok(())
    }

    fn transact<StateDB: DatabaseRef + Debug>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        tx: Self::Tx,
    ) -> Result<
        ExecutionResult<Self::EvmHaltReason>,
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB::Error: Sync + Send + 'static,
    {
        let evm_env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let ts: u64 = block_env.timestamp.saturating_to();
        let db = Vcv2CodeInjector::new(state, ts);
        let wrap_database_ref = WrapDatabaseRef(db);
        let mut evm = TempoEvm::new(evm_env, wrap_database_ref, NoOpInspector {}, false);
        evm.transact(tx).map(|res| res.result.into())
    }

    fn inspect_tx_commit<StateDB, R, F>(
        &self,
        block_env: &BlockEnv,
        state: StateDB,
        inspector_cfg: TracingInspectorConfig,
        inspector_collect: F,
        tx: Self::Tx,
    ) -> Result<
        (ExecutionResult<Self::EvmHaltReason>, R),
        EVMError<StateDB::Error, Self::TransactionError>,
    >
    where
        StateDB: DatabaseCommit + DatabaseRef + Debug,
        StateDB::Error: Sync + Send + 'static,
        F: FnOnce(TracingInspector) -> R,
    {
        let evm_env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let ts: u64 = block_env.timestamp.saturating_to();
        let db = Vcv2CodeInjector::new(state, ts);
        let wrap_database_ref = WrapDatabaseRef(db);
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = TempoEvm::new(evm_env, wrap_database_ref, &mut inspector, true);
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }

}

impl<DB> GasFeeHandler for TempoApiImpl<DB>
where
    DB: Sync + Send + 'static,
{

    type Tx = TempoTxEnv;

    fn virtual_balance(&self) -> Option<alloy::primitives::U256> {
        Some(leafage_evm_chains::tempo::VIRTUAL_BALANCE)
    }

    fn gas_allowance<StateDB: DatabaseRef>(
        &self,
        request: &CallRequest,
        tx: &Self::Tx,
        db: &StateDB,
        block_env: &BlockEnv,
    ) -> RpcResult<u64> {
        use leafage_evm_chains::tempo::fee_payer::{self as fp, Call as FpCall};

        let te = request.tempo.as_ref();
        let payer = if let Some(sig) = te.and_then(|t| t.fee_payer_signature.as_ref()) {
            let calls: Vec<FpCall> = te
                .and_then(|t| t.tempo_calls.as_ref())
                .map(|cs| {
                    cs.iter()
                        .map(|c| {
                            let to = c.to.as_ref().and_then(|t| t.to().copied());
                            FpCall {
                                to,
                                value: c.value.unwrap_or_default(),
                                input: c.input.clone().into_input().unwrap_or_default(),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            let access_list = alloy::eips::eip2930::AccessList::default();
            fp::recover_fee_payer(
                sig,
                self.evm_cfg.cfg.chain_id,
                tx.max_priority_fee_per_gas().unwrap_or(0),
                tx.max_fee_per_gas(),
                tx.gas_limit(),
                &calls,
                &access_list,
                te.and_then(|t| t.nonce_key).unwrap_or_default(),
                tx.nonce(),
                te.and_then(|t| t.valid_before),
                te.and_then(|t| t.valid_after),
                te.and_then(|t| t.fee_token),
                tx.caller(),
                &[],
                None,
            )
            .unwrap_or(tx.caller())
        } else {
            te.and_then(|t| t.fee_payer).unwrap_or(tx.caller())
        };
        Ok(leafage_evm_chains::tempo::precompile::tempo_caller_gas_allowance(
            db,
            payer,
            tx.gas_price(),
            block_env.timestamp.saturating_to::<u64>(),
            self.evm_cfg.cfg.chain_id,
            te.and_then(|t| t.fee_token),
        )
        .unwrap_or(u64::MAX))
    }
}

impl TxSetter for TempoTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.gas_limit = gas_limit;
    }
}

impl<DB> ApiCore for TempoApiImpl<DB> where DB: Sync + Send + 'static {}

/// Parse WebAuthn data size from `key_data`.
/// Mirrors Tempo writer: `create_mock_primitive_signature` in compat.rs.
/// Format: 1/2/4 bytes BE integer, default 800, clamped to [87, 8192].
fn parse_webauthn_size(key_data: Option<&alloy::primitives::Bytes>) -> usize {
    const MIN_WEBAUTHN_SIZE: usize = 87;
    const DEFAULT_WEBAUTHN_SIZE: usize = 800;
    const MAX_WEBAUTHN_SIZE: usize = 8192;

    let size = if let Some(data) = key_data {
        match data.len() {
            1 => data[0] as usize,
            2 => u16::from_be_bytes([data[0], data[1]]) as usize,
            4 => u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize,
            _ => DEFAULT_WEBAUTHN_SIZE,
        }
    } else {
        DEFAULT_WEBAUTHN_SIZE
    };
    size.clamp(MIN_WEBAUTHN_SIZE, MAX_WEBAUTHN_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::tempo::precompile::VALIDATOR_CONFIG_V2_ADDRESS;
    use revm::database::EmptyDB;

    #[test]
    fn test_vcv2_injector_t2_injects_code_for_missing_account() {
        // EmptyDB returns None for all accounts
        let db = EmptyDB::default();
        let injector = Vcv2CodeInjector::new(db, 1_774_965_700); // T2+

        let info = injector.basic_ref(VALIDATOR_CONFIG_V2_ADDRESS).unwrap();
        assert!(info.is_some(), "VCV2 should have info at T2");
        let acc = info.unwrap();
        assert!(!acc.is_empty_code_hash(), "VCV2 should have code at T2");
        assert_eq!(
            acc.code.as_ref().map(|c| c.original_byte_slice()),
            Some(&[0xef][..]),
            "VCV2 code should be 0xef"
        );
    }

    #[test]
    fn test_vcv2_injector_pre_t2_no_injection() {
        let db = EmptyDB::default();
        let injector = Vcv2CodeInjector::new(db, 1_774_965_599); // pre-T2

        let info = injector.basic_ref(VALIDATOR_CONFIG_V2_ADDRESS).unwrap();
        assert!(info.is_none(), "VCV2 should be None pre-T2 on EmptyDB");
    }

    #[test]
    fn test_vcv2_injector_other_address_passthrough() {
        let db = EmptyDB::default();
        let injector = Vcv2CodeInjector::new(db, 1_774_965_700); // T2+
        let other = revm::primitives::Address::with_last_byte(0x42);

        let info = injector.basic_ref(other).unwrap();
        assert!(info.is_none(), "other address should still be None");
    }

    #[test]
    fn test_vcv2_injector_existing_account_with_code_untouched() {
        use revm::database::in_memory_db::CacheDB;
        use revm::state::AccountInfo;
        use revm::bytecode::Bytecode;

        let mut db = CacheDB::new(EmptyDB::default());
        // Pre-populate VCV2 with existing code
        let existing_code = Bytecode::new_legacy(alloy::primitives::Bytes::from_static(&[0xfe]));
        db.insert_account_info(
            VALIDATOR_CONFIG_V2_ADDRESS,
            AccountInfo {
                code_hash: existing_code.hash_slow(),
                code: Some(existing_code.clone()),
                nonce: 5,
                ..Default::default()
            },
        );

        let injector = Vcv2CodeInjector::new(&db, 1_774_965_700); // T2+
        let info = injector.basic_ref(VALIDATOR_CONFIG_V2_ADDRESS).unwrap();
        let acc = info.unwrap();
        assert_eq!(acc.nonce, 5, "nonce should be preserved");
        assert_eq!(
            acc.code.as_ref().map(|c| c.original_byte_slice()),
            Some(&[0xfe][..]),
            "existing code should NOT be overwritten"
        );
    }

    // -- derive_scope_counts (FU-2) -----------------------------------------

    fn make_scope(
        target: alloy::primitives::Address,
        rules: Vec<(alloy::primitives::FixedBytes<4>, Vec<alloy::primitives::Address>)>,
    ) -> leafage_evm_types::CallScope {
        leafage_evm_types::CallScope {
            target,
            selector_rules: rules
                .into_iter()
                .map(|(selector, recipients)| leafage_evm_types::SelectorRule {
                    selector,
                    recipients,
                })
                .collect(),
        }
    }

    #[test]
    fn derive_scope_counts_none_returns_default() {
        let counts = derive_scope_counts(None);
        assert!(!counts.has_allowed_calls);
        assert_eq!(counts.scopes, 0);
        assert_eq!(counts.selectors, 0);
        assert_eq!(counts.constrained_selectors, 0);
        assert_eq!(counts.recipients, 0);
    }

    #[test]
    fn derive_scope_counts_empty_marks_has_allowed_calls() {
        let counts = derive_scope_counts(Some(&[]));
        assert!(counts.has_allowed_calls);
        assert_eq!(counts.scopes, 0);
        assert_eq!(counts.selectors, 0);
        assert_eq!(counts.constrained_selectors, 0);
        assert_eq!(counts.recipients, 0);
    }

    #[test]
    fn derive_scope_counts_aggregates_across_nested_rules() {
        use alloy::primitives::{address, FixedBytes};

        let scopes = vec![
            make_scope(
                address!("0x20C0000000000000000000000000000000000001"),
                vec![
                    // 1 selector with 2 recipients (constrained)
                    (
                        FixedBytes::from([0xa9, 0x05, 0x9c, 0xbb]),
                        vec![
                            address!("0x1111111111111111111111111111111111111111"),
                            address!("0x2222222222222222222222222222222222222222"),
                        ],
                    ),
                    // 1 selector with 0 recipients (unconstrained)
                    (FixedBytes::from([0x09, 0x5e, 0xa7, 0xb3]), Vec::new()),
                ],
            ),
            make_scope(
                address!("0x20C0000000000000000000000000000000000002"),
                vec![
                    // 1 selector with 1 recipient (constrained)
                    (
                        FixedBytes::from([0xa9, 0x05, 0x9c, 0xbb]),
                        vec![address!("0x3333333333333333333333333333333333333333")],
                    ),
                ],
            ),
        ];

        let counts = derive_scope_counts(Some(&scopes));
        assert!(counts.has_allowed_calls);
        assert_eq!(counts.scopes, 2, "2 targets");
        assert_eq!(counts.selectors, 3, "2 + 1 selectors");
        assert_eq!(counts.constrained_selectors, 2, "1 + 1 with non-empty recipients");
        assert_eq!(counts.recipients, 3, "2 + 1 recipients total");
    }

    #[test]
    fn test_vcv2_injector_existing_empty_account_gets_code() {
        use revm::database::in_memory_db::CacheDB;
        use revm::state::AccountInfo;

        let mut db = CacheDB::new(EmptyDB::default());
        // VCV2 exists in DB but without code (pipeline state)
        db.insert_account_info(
            VALIDATOR_CONFIG_V2_ADDRESS,
            AccountInfo {
                nonce: 1,
                ..Default::default()
            },
        );

        let injector = Vcv2CodeInjector::new(&db, 1_774_965_700); // T2+
        let info = injector.basic_ref(VALIDATOR_CONFIG_V2_ADDRESS).unwrap();
        let acc = info.unwrap();
        assert_eq!(acc.nonce, 1, "nonce should be preserved");
        assert_eq!(
            acc.code.as_ref().map(|c| c.original_byte_slice()),
            Some(&[0xef][..]),
            "empty-code account should get 0xef injected"
        );
    }
}
