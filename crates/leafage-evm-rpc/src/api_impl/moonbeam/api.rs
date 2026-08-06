use crate::api_impl::api_impl::NoneEvmCustomConfig;
use crate::api_impl::mainnet::evm::create_mainnet_txn_env;
use crate::api_impl::{ApiCore, ApiImpl, EvmExecutor, GasFeeHandler};
use alloy_evm::EvmEnv;
use jsonrpsee::core::RpcResult;
use leafage_evm_chains::moonbeam::{is_unsupported, MoonbeamEvm, MoonbeamHardfork};
use leafage_evm_types::{BlockEnv, BlockInfo, CallRequest};
use revm::context::result::{EVMError, ExecutionResult, HaltReason, InvalidTransaction};
use revm::context::TxEnv;
use revm::database::WrapDatabaseRef;
use revm::inspector::NoOpInspector;
use revm::primitives::TxKind;
use revm::{DatabaseCommit, DatabaseRef, ExecuteEvm, InspectCommitEvm};
use revm_inspectors::tracing::{TracingInspector, TracingInspectorConfig};
use std::fmt::Debug;

/// Pre-check that short-circuits a call whose top-level `to` is one of the
/// Moonbeam precompiles leafage cannot execute locally, before revm validates
/// the tx envelope (gas/nonce/balance). Mirrors the IoTeX executor pre-check:
/// without it, a `gas == 0` call to such an address fails with revm's
/// `CallGasCostMoreThanGasLimit` (mapped to `-39001 GasExhausted`) before
/// reaching `MoonbeamEvm::frame_init`, where the precompile guard lives.
/// nodex-proxy only retries against a real node on `-39008
/// UnsupportedPrecompile`, so the envelope-level error would swallow the
/// fallback path.
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

type MoonbeamApiImpl<DB> = ApiImpl<DB, MoonbeamHardfork, NoneEvmCustomConfig>;

impl<DB> GasFeeHandler for MoonbeamApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
}

impl<DB> EvmExecutor for MoonbeamApiImpl<DB>
where
    DB: Sync + Send + 'static,
{
    type Tx = TxEnv;
    type TransactionError = InvalidTransaction;
    type EvmHaltReason = HaltReason;

    fn create_txn_env<StateDB: DatabaseRef>(
        &self,
        _block: &BlockInfo,
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
        let mut evm = MoonbeamEvm::new(evm_env, wrap_database_ref, NoOpInspector {});
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
        let mut evm = MoonbeamEvm::new(evm_env, wrap_database_ref, &mut inspector);
        evm.inspect_tx_commit(tx)
            .map(|res| (res.into(), inspector_collect(inspector)))
    }
}

impl<DB> ApiCore for MoonbeamApiImpl<DB> where DB: Sync + Send + 'static {}

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

    /// gas=0 to a Moonbeam precompile must short-circuit at the executor entry
    /// (before revm validates `CallGasCostMoreThanGasLimit`). Otherwise the
    /// envelope error masks `-39008` and nodex-proxy never falls back to a real
    /// node. Regression test for the leafage→writer fallback path.
    #[test]
    fn gas_zero_to_precompile_returns_unsupported_precompile() {
        let staking = address!("0x0000000000000000000000000000000000000800");
        let tx = build_tx(TxKind::Call(staking), 0);
        let err = unsupported_precompile_error::<Infallible>(&tx)
            .expect("expected EVMError::Custom for precompile addr with gas=0");
        match err {
            EVMError::Custom(msg) => {
                assert!(
                    msg.starts_with("unsupported precompile address: "),
                    "wire-format must match api_impl.rs parser, got: {msg}"
                );
                assert!(msg.to_lowercase().contains(&format!("{staking:#x}")));
            }
            other => panic!("expected Custom, got {other:?}"),
        }
    }

    /// A representative spread of Moonbeam precompile addresses must be detected
    /// at the executor entry regardless of gas value. Mirrors the
    /// `moonbeam/precompile.rs::UNSUPPORTED_LIST` set.
    #[test]
    fn moonbeam_precompiles_short_circuit() {
        let addrs: [Address; 7] = [
            address!("0x000000000000000000000000000000000000000a"), // KZG slot (absent on Moonbeam)
            address!("0x0000000000000000000000000000000000000400"), // Sha3FIPS256
            address!("0x0000000000000000000000000000000000000803"), // Democracy (removed → reverts)
            address!("0x0000000000000000000000000000000000000800"), // ParachainStaking
            address!("0x0000000000000000000000000000000000000802"), // Erc20Balances
            address!("0x0000000000000000000000000000000000000811"), // Referenda
            address!("0x000000000000000000000000000000000000081a"), // PalletXcm
        ];
        for addr in addrs {
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

    /// Locally-executable precompiles, regular calls and contract creation must
    /// NOT be short-circuited. (Removed Moonbeam slots such as 0x803 are *not*
    /// here: they revert on the runtime and so must forward — see
    /// `moonbeam_precompiles_short_circuit` / the chains-side precompile tests.)
    #[test]
    fn standard_and_regular_calls_pass_through() {
        for addr in [
            address!("0x0000000000000000000000000000000000000001"), // ECRecover (revm-native)
            address!("0x0000000000000000000000000000000000000009"), // Blake2F (revm-native)
            address!("0x0000000000000000000000000000000000000100"), // P256Verify (revm-native)
            address!("0x1234567890123456789012345678901234567890"), // regular contract
        ] {
            let tx = build_tx(TxKind::Call(addr), 100_000);
            assert!(unsupported_precompile_error::<Infallible>(&tx).is_none());
        }

        let create_tx = build_tx(TxKind::Create, 100_000);
        assert!(unsupported_precompile_error::<Infallible>(&create_tx).is_none());
    }
}
