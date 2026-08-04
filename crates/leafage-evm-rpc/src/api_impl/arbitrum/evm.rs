use leafage_evm_chains::arbitrum::arbos_state::ArbStateReader;
use leafage_evm_chains::arbitrum::evm::ArbitrumEvm;
use leafage_evm_chains::arbitrum::evm::ArbitrumExecutionContext;
use leafage_evm_chains::arbitrum::precompile::ArbitrumPrecompileEnv;
use leafage_evm_chains::arbitrum::ArbitrumHardfork;
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::database::{DatabaseRef, WrapDatabaseRef};

pub(crate) fn create_arbitrum_evm_from_state<StateDB, INSP>(
    block_env: BlockEnv,
    mut cfg: CfgEnv<ArbitrumHardfork>,
    state: StateDB,
    inspector: INSP,
    mut precompile_env: ArbitrumPrecompileEnv,
    execution_context: ArbitrumExecutionContext,
) -> Result<ArbitrumEvm<WrapDatabaseRef<StateDB>, INSP>, StateDB::Error>
where
    StateDB: DatabaseRef,
{
    let arbos_version = state.try_arbos_version()?;
    precompile_env.current_arbos_version = arbos_version;

    // Nitro applies the EIP-7623 calldata gas floor only from ArbOS 40 with
    // the chain-owner feature flag set (`state_transition.go`,
    // `TxProcessor.IsCalldataPricingIncreaseEnabled`); revm would otherwise
    // always apply it from Prague.
    cfg.disable_eip7623 = !state.try_is_calldata_price_increase_enabled(arbos_version)?;
    Ok(ArbitrumEvm::new(
        block_env,
        cfg,
        WrapDatabaseRef(state),
        inspector,
        precompile_env,
        execution_context,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use leafage_evm_chains::arbitrum::tx::ArbitrumTxEnv;
    use revm::context::TxEnv;
    use revm::database::{in_memory_db::CacheDB, EmptyDB};
    use revm::inspector::NoOpInspector;
    use revm::primitives::{address, keccak256, Bytes, TxKind, U256};
    use revm::ExecuteEvm;

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct ExpectedDbError;

    impl core::fmt::Display for ExpectedDbError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("expected database error")
        }
    }

    impl std::error::Error for ExpectedDbError {}
    impl revm::database_interface::DBErrorMarker for ExpectedDbError {}

    struct FailingArbosState {
        version: Result<u64, ExpectedDbError>,
        fail_features: bool,
    }

    impl DatabaseRef for FailingArbosState {
        type Error = ExpectedDbError;

        fn basic_ref(
            &self,
            _: revm::primitives::Address,
        ) -> Result<Option<revm::state::AccountInfo>, Self::Error> {
            Ok(None)
        }

        fn code_by_hash_ref(
            &self,
            _: revm::primitives::B256,
        ) -> Result<revm::bytecode::Bytecode, Self::Error> {
            Ok(Default::default())
        }

        fn storage_ref(
            &self,
            _: revm::primitives::Address,
            index: U256,
        ) -> Result<U256, Self::Error> {
            if index == slot(&[], 0) {
                return self
                    .version
                    .as_ref()
                    .map(|version| U256::from(*version))
                    .map_err(Clone::clone);
            }
            let features_key = keccak256([&[] as &[u8], &[9u8][..]].concat());
            if self.fail_features && index == slot(features_key.as_slice(), 0) {
                return Err(ExpectedDbError);
            }
            Ok(U256::ZERO)
        }

        fn block_hash_ref(&self, _: u64) -> Result<revm::primitives::B256, Self::Error> {
            Ok(revm::primitives::B256::ZERO)
        }
    }

    /// Independent re-derivation of the ArbOS slot scheme
    /// (`keccak(storageKey ++ key[:31])[:31] ++ key[31]`).
    fn slot(storage_key: &[u8], offset: u64) -> U256 {
        let key = U256::from(offset).to_be_bytes::<32>();
        let mut input = storage_key.to_vec();
        input.extend_from_slice(&key[..31]);
        let hashed = keccak256(&input);
        let mut slot = [0u8; 32];
        slot[..31].copy_from_slice(&hashed[..31]);
        slot[31] = key[31];
        U256::from_be_bytes(slot)
    }

    fn calldata_heavy_gas(feature_bit: U256) -> u64 {
        const ARBOS_STATE: revm::primitives::Address =
            address!("a4b05fffffffffffffffffffffffffffffffffff");
        let mut db = CacheDB::new(EmptyDB::default());
        db.insert_account_storage(ARBOS_STATE, slot(&[], 0), U256::from(40))
            .expect("write ArbOS version");
        let features_key = keccak256([&[] as &[u8], &[9u8][..]].concat());
        db.insert_account_storage(ARBOS_STATE, slot(features_key.as_slice(), 0), feature_bit)
            .expect("write features flag");

        let mut evm = create_arbitrum_evm_from_state(
            BlockEnv {
                gas_limit: 30_000_000,
                ..Default::default()
            },
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            db,
            NoOpInspector {},
            ArbitrumPrecompileEnv::default(),
            ArbitrumExecutionContext::default(),
        )
        .expect("create Arbitrum EVM");
        let tx = ArbitrumTxEnv::new(
            TxEnv {
                caller: address!("0000000000000000000000000000000000ca11e4"),
                kind: TxKind::Call(address!("0000000000000000000000000000000000000e0a")),
                gas_limit: 100_000,
                data: Bytes::from(vec![0x11u8; 1_000]),
                ..Default::default()
            },
            Default::default(),
        );
        let result = evm.transact(tx).expect("transact").result;
        assert!(result.is_success(), "expected success, got {result:?}");
        result.gas_used()
    }

    /// Nitro applies the EIP-7623 floor only when the ArbOS feature flag is
    /// set; 1000 nonzero calldata bytes: floor 21000 + 4000 tokens * 10 =
    /// 61000 vs standard intrinsic 21000 + 1000 * 16 = 37000.
    #[test]
    fn eip7623_floor_follows_arbos_feature_flag() {
        assert_eq!(calldata_heavy_gas(U256::ZERO), 37_000);
        assert_eq!(calldata_heavy_gas(U256::ONE), 61_000);
    }

    #[test]
    fn evm_factory_preserves_arbos_version_database_error() {
        let result = create_arbitrum_evm_from_state(
            BlockEnv::default(),
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            FailingArbosState {
                version: Err(ExpectedDbError),
                fail_features: false,
            },
            NoOpInspector {},
            ArbitrumPrecompileEnv::default(),
            ArbitrumExecutionContext::default(),
        );

        assert!(matches!(result, Err(ExpectedDbError)));
    }

    #[test]
    fn evm_factory_preserves_calldata_feature_database_error() {
        let result = create_arbitrum_evm_from_state(
            BlockEnv::default(),
            CfgEnv::new_with_spec(ArbitrumHardfork::Prague),
            FailingArbosState {
                version: Ok(40),
                fail_features: true,
            },
            NoOpInspector {},
            ArbitrumPrecompileEnv::default(),
            ArbitrumExecutionContext::default(),
        );

        assert!(matches!(result, Err(ExpectedDbError)));
    }
}
