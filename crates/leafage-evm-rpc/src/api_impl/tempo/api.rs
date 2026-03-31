use crate::api_impl::core::{ApiCore, EvmExecutor, TxSetter};
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::ApiImpl;
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::tempo::tx::TempoTxEnv;
use leafage_evm_chains::tempo::TempoEvm;
use leafage_evm_types::{BlockEnv, CallRequest, MainnetSpecId};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

/// Marker type to differentiate `TempoApiImpl` from `MainnetApiImpl`.
///
/// Both use `MainnetSpecId`, but Rust's type system requires distinct types
/// for separate `EvmExecutor` implementations.
#[derive(Debug, Clone)]
pub struct TempoEvmCustomConfig;

type TempoApiImpl<DB> = ApiImpl<DB, MainnetSpecId, TempoEvmCustomConfig>;

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
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        use leafage_evm_chains::tempo::tx::{
            TempoAuthGas, TempoCall, TempoKeyAuthGas, TempoSigType, TempoTxFields,
        };
        use revm::primitives::TxKind;

        // Extract Tempo-specific fields before consuming the request.
        let tempo_calls = request.tempo_calls.clone();
        let nonce_key = request.nonce_key;
        let key_type = request.key_type.clone();
        let key_data = request.key_data.clone();
        let key_id = request.key_id;
        let key_authorization = request.key_authorization.clone();
        let tempo_authorization_list = request.tempo_authorization_list.clone();
        let fee_token = request.fee_token;
        let fee_payer = request.fee_payer;
        let valid_after = request.valid_after;
        let valid_before = request.valid_before;

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

            // Key authorization gas info.
            let key_auth = key_authorization.map(|ka| TempoKeyAuthGas {
                sig_type: ka
                    .sig_type
                    .as_deref()
                    .map(TempoSigType::from_str_lossy)
                    .unwrap_or_default(),
                num_limits: ka.num_limits,
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
        let wrap_database_ref = WrapDatabaseRef(state);
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
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = TempoEvm::new(evm_env, wrap_database_ref, &mut inspector, true);
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
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
