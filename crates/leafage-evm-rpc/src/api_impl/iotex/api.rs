use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::{ApiCore, ApiImpl, EvmExecutor, GasFeeHandler};
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::iotex::{is_unsupported, IotexEvm, IotexHardfork};
use leafage_evm_types::{BlockEnv, CallRequest};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::TxEnv;
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::primitives::TxKind;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

/// Pre-check that short-circuits a call to one of IoTeX's 4 protocol addresses
/// before revm validates the tx envelope (gas/nonce/balance). Without this,
/// a `gas == 0` call to a protocol address — which is the on-chain shape of
/// IoTeX's native rewarding/staking actions — fails with revm's
/// `CallGasCostMoreThanGasLimit` (mapped to `-39001 GasExhausted`) before
/// reaching `IotexEvm::frame_init`, where the precompile guard lives. nodex-proxy
/// only retries against the writer on `-39008 UnsupportedPrecompile`, so the
/// envelope-level error swallows the entire fallback path.
///
/// The error string format must match the parser at
/// `leafage-evm-rpc/src/api_impl/api_impl.rs::ToJsonRpcError for EVMError`,
/// which turns `EVMError::Custom("unsupported precompile address: ...")` into
/// the DeBank-standard `-39008` code.
fn unsupported_precompile_error<DBError>(
    tx: &TxEnv,
) -> Option<EVMError<DBError, InvalidTransaction>> {
    if let TxKind::Call(addr) = tx.kind {
        if is_unsupported(&addr) {
            return Some(EVMError::Custom(format!(
                "unsupported precompile address: {}",
                addr
            )));
        }
    }
    None
}

type IotexApiImpl<DB> = ApiImpl<DB, IotexHardfork, NoneEvmCustomConfig>;

impl<DB> GasFeeHandler for IotexApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
}

impl<DB> EvmExecutor for IotexApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        block_env: &BlockEnv,
        request: CallRequest,
        db: StateDB,
        chain_id: u64,
    ) -> RpcResult<Self::Tx> {
        create_mainnet_txn_env(block_env, self.evm_cfg.cfg.clone(), request, db, chain_id)
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
        if let Some(err) = unsupported_precompile_error::<StateDB::Error>(&tx) {
            return Err(err);
        }
        let evm_env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut evm = IotexEvm::new(evm_env, wrap_database_ref, NoOpInspector {});
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
        if let Some(err) = unsupported_precompile_error::<StateDB::Error>(&tx) {
            return Err(err);
        }
        let evm_env = EvmEnv::new(self.evm_cfg.cfg.clone(), block_env.clone());
        let wrap_database_ref = WrapDatabaseRef(state);
        let mut inspector = TracingInspector::new(inspector_cfg);
        let mut evm = IotexEvm::new(evm_env, wrap_database_ref, &mut inspector);
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for IotexApiImpl<DB> where DB: Sync + Send + 'static {}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::primitives::{address, Address, U256};
    use std::convert::Infallible;

    fn build_tx(to: TxKind, gas_limit: u64) -> TxEnv {
        TxEnv {
            kind: to,
            gas_limit,
            gas_price: 0,
            value: U256::ZERO,
            data: Default::default(),
            ..Default::default()
        }
    }

    /// gas=0 to a protocol address must short-circuit at the executor entry
    /// (before revm validates `CallGasCostMoreThanGasLimit`). Otherwise the
    /// envelope error masks `-39008` and nodex-proxy never falls back to the
    /// writer. Regression test for the leafage→writer fallback path.
    #[test]
    fn gas_zero_to_protocol_addr_returns_unsupported_precompile() {
        let rewarding = address!("0xa576c141e5659137ddda4223d209d4744b2106be");
        let tx = build_tx(TxKind::Call(rewarding), 0);
        let err = unsupported_precompile_error::<Infallible>(&tx)
            .expect("expected EVMError::Custom for protocol addr with gas=0");
        match err {
            EVMError::Custom(msg) => {
                assert!(
                    msg.starts_with("unsupported precompile address: "),
                    "wire-format must match api_impl.rs parser, got: {msg}"
                );
                assert!(msg.to_lowercase().contains(&format!("{rewarding:#x}")));
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    /// All four IoTeX system protocol addresses must be detected at the
    /// executor entry, regardless of gas value. Mirrors the
    /// `iotex/precompile.rs::UNSUPPORTED_LIST` set.
    #[test]
    fn all_four_protocol_addrs_short_circuit() {
        let addrs: [Address; 4] = [
            address!("0x04c22afae6a03438b8fed74cb1cf441168df3f12"), // STAKING
            address!("0xa576c141e5659137ddda4223d209d4744b2106be"), // REWARDING
            address!("0x166b743c2c1a57c93c2e2bc3e169d28bbb9f6da3"), // POLL
            address!("0x041370e00a711cd81da1918f0e494459aadae50e"), // ROLLDPOS
        ];
        for addr in addrs {
            // gas=0 (on-chain native action shape) and gas>0 (regular EVM call)
            // must both short-circuit.
            for gas in [0u64, 100_000] {
                let tx = build_tx(TxKind::Call(addr), gas);
                let got = unsupported_precompile_error::<Infallible>(&tx);
                assert!(
                    matches!(got, Some(EVMError::Custom(_))),
                    "addr {addr} gas {gas} should short-circuit"
                );
            }
        }
    }

    /// Regular contract calls and contract creation must NOT be short-circuited.
    #[test]
    fn regular_calls_and_create_pass_through() {
        let regular = address!("0x1234567890123456789012345678901234567890");
        let tx = build_tx(TxKind::Call(regular), 100_000);
        assert!(unsupported_precompile_error::<Infallible>(&tx).is_none());

        let create_tx = build_tx(TxKind::Create, 100_000);
        assert!(unsupported_precompile_error::<Infallible>(&create_tx).is_none());
    }
}
