use super::{ArcChainConfig, ArcExecutionSpec};
use alloy_evm::{precompiles::PrecompilesMap, Database, EvmEnv};
use leafage_evm_types::{BlockEnv, CfgEnv, MainnetSpecId, U256};
use revm::{
    context::{Evm as RevmEvm, FrameStack, JournalTr, TxEnv},
    handler::{
        evm::{ContextDbError, FrameInitResult},
        instructions::EthInstructions,
        EthFrame, EvmTr, FrameInitOrResult, FrameResult,
    },
    inspector::InspectorEvmTr,
    interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInit},
    precompile::{PrecompileSpecId, Precompiles},
    Context, Inspector, Journal,
};
use std::{
    error::Error,
    fmt,
    ops::{Deref, DerefMut},
};

mod exec;

/// REVM context used by Arc query execution.
pub type ArcContext<DB> = Context<BlockEnv, TxEnv, CfgEnv<MainnetSpecId>, DB>;

/// Errors detected while turning an RPC execution environment into an Arc EVM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArcEvmFactoryError {
    ChainId {
        expected: u64,
        actual: u64,
    },
    EthereumSpec {
        expected: MainnetSpecId,
        actual: MainnetSpecId,
    },
    BlockNumber(U256),
    Timestamp(U256),
}

impl fmt::Display for ArcEvmFactoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChainId { expected, actual } => {
                write!(f, "Arc EVM requires chain ID {expected}, got {actual}")
            }
            Self::EthereumSpec { expected, actual } => {
                write!(
                    f,
                    "Arc EVM requires Ethereum spec {expected:?}, got {actual:?}"
                )
            }
            Self::BlockNumber(number) => {
                write!(f, "Arc block number does not fit in u64: {number}")
            }
            Self::Timestamp(timestamp) => {
                write!(f, "Arc block timestamp does not fit in u64: {timestamp}")
            }
        }
    }
}

impl Error for ArcEvmFactoryError {}

/// Creates Arc query EVMs from the chain configuration and target block environment.
#[derive(Debug, Clone, Copy)]
pub struct ArcEvmFactory {
    chain_config: ArcChainConfig,
}

impl ArcEvmFactory {
    pub const fn new(chain_config: ArcChainConfig) -> Self {
        Self { chain_config }
    }

    pub const fn chain_config(&self) -> &ArcChainConfig {
        &self.chain_config
    }

    pub fn execution_spec(
        &self,
        block_env: &BlockEnv,
    ) -> Result<ArcExecutionSpec, ArcEvmFactoryError> {
        let block_number = u64::try_from(block_env.number)
            .map_err(|_| ArcEvmFactoryError::BlockNumber(block_env.number))?;
        let timestamp = u64::try_from(block_env.timestamp)
            .map_err(|_| ArcEvmFactoryError::Timestamp(block_env.timestamp))?;
        Ok(self.chain_config.execution_spec_at(block_number, timestamp))
    }

    pub fn create<DB: Database, I>(
        &self,
        env: EvmEnv<MainnetSpecId>,
        db: DB,
        inspector: I,
    ) -> Result<ArcEvm<DB, I>, ArcEvmFactoryError> {
        self.validate_cfg(&env.cfg_env)?;
        let execution_spec = self.execution_spec(&env.block_env)?;
        Ok(ArcEvm::new(env, db, inspector, execution_spec))
    }

    fn validate_cfg(&self, cfg: &CfgEnv<MainnetSpecId>) -> Result<(), ArcEvmFactoryError> {
        let expected_chain_id = self.chain_config.chain_id();
        if cfg.chain_id != expected_chain_id {
            return Err(ArcEvmFactoryError::ChainId {
                expected: expected_chain_id,
                actual: cfg.chain_id,
            });
        }

        let expected_spec = self.chain_config.ethereum_spec();
        if cfg.spec != expected_spec {
            return Err(ArcEvmFactoryError::EthereumSpec {
                expected: expected_spec,
                actual: cfg.spec,
            });
        }
        Ok(())
    }
}

/// Arc query EVM wrapper.
///
/// Its separate type prevents Arc RPCs from being implemented by the generic
/// mainnet executor. A4 and A5 extend this wrapper with Arc handler, frame,
/// instruction, and precompile behavior.
#[allow(missing_debug_implementations)]
pub struct ArcEvm<DB: revm::Database, I> {
    pub(crate) inner: RevmEvm<
        ArcContext<DB>,
        I,
        EthInstructions<EthInterpreter, ArcContext<DB>>,
        PrecompilesMap,
        EthFrame,
    >,
    execution_spec: ArcExecutionSpec,
}

impl<DB: Database, I> ArcEvm<DB, I> {
    fn new(
        env: EvmEnv<MainnetSpecId>,
        db: DB,
        inspector: I,
        execution_spec: ArcExecutionSpec,
    ) -> Self {
        let spec = env.cfg_env.spec;
        let precompiles =
            PrecompilesMap::from_static(Precompiles::new(PrecompileSpecId::from_spec_id(spec)));
        Self {
            inner: RevmEvm {
                ctx: Context {
                    block: env.block_env,
                    cfg: env.cfg_env,
                    journaled_state: Journal::new(db),
                    tx: Default::default(),
                    chain: Default::default(),
                    local: Default::default(),
                    error: Ok(()),
                },
                inspector,
                instruction: EthInstructions::new_mainnet_with_spec(spec),
                precompiles,
                frame_stack: Default::default(),
            },
            execution_spec,
        }
    }

    pub const fn execution_spec(&self) -> ArcExecutionSpec {
        self.execution_spec
    }

    pub const fn ctx(&self) -> &ArcContext<DB> {
        &self.inner.ctx
    }

    pub fn ctx_mut(&mut self) -> &mut ArcContext<DB> {
        &mut self.inner.ctx
    }
}

impl<DB: Database, I> Deref for ArcEvm<DB, I> {
    type Target = ArcContext<DB>;

    fn deref(&self) -> &Self::Target {
        self.ctx()
    }
}

impl<DB: Database, I> DerefMut for ArcEvm<DB, I> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.ctx_mut()
    }
}

impl<DB: Database, I> EvmTr for ArcEvm<DB, I> {
    type Context = ArcContext<DB>;
    type Instructions = EthInstructions<EthInterpreter, ArcContext<DB>>;
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
        frame_input: FrameInit,
    ) -> Result<FrameInitResult<'_, Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_init(frame_input)
    }

    fn frame_run(
        &mut self,
    ) -> Result<FrameInitOrResult<Self::Frame>, ContextDbError<Self::Context>> {
        self.inner.frame_run()
    }

    fn frame_return_result(
        &mut self,
        result: FrameResult,
    ) -> Result<Option<FrameResult>, ContextDbError<Self::Context>> {
        self.inner.frame_return_result(result)
    }
}

impl<DB, I> InspectorEvmTr for ArcEvm<DB, I>
where
    DB: Database,
    I: Inspector<ArcContext<DB>, EthInterpreter>,
{
    type Inspector = I;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arc::{ArcHardfork, ArcHardforkFlags, ARC_MAINNET_CHAIN_ID};
    use alloy::primitives::{address, Address, B256};
    use revm::{
        context_interface::block::BlobExcessGasAndPrice, database::InMemoryDB,
        handler::PrecompileProvider, inspector::NoOpInspector, primitives::TxKind, ExecuteEvm,
        InspectCommitEvm,
    };

    fn evm_env() -> EvmEnv<MainnetSpecId> {
        let mut cfg = CfgEnv::new_with_spec(MainnetSpecId::OSAKA);
        cfg.chain_id = ARC_MAINNET_CHAIN_ID;
        let block = BlockEnv {
            number: U256::from(1),
            timestamp: U256::from(1),
            gas_limit: 30_000_000,
            prevrandao: Some(B256::ZERO),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1,
            }),
            ..Default::default()
        };
        EvmEnv::new(cfg, block)
    }

    fn simple_tx() -> TxEnv {
        TxEnv {
            caller: Address::with_last_byte(1),
            kind: TxKind::Call(Address::with_last_byte(2)),
            gas_limit: 21_000,
            chain_id: Some(ARC_MAINNET_CHAIN_ID),
            ..Default::default()
        }
    }

    #[test]
    fn factory_resolves_arc_flags_for_each_block() {
        let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());
        let spec = factory.execution_spec(&evm_env().block_env).unwrap();

        for hardfork in [
            ArcHardfork::Zero3,
            ArcHardfork::Zero4,
            ArcHardfork::Zero5,
            ArcHardfork::Zero6,
        ] {
            assert!(spec.arc_flags.is_active(hardfork));
        }
        assert!(!spec.arc_flags.is_active(ArcHardfork::Zero7));
        assert!(!spec.arc_flags.is_active(ArcHardfork::Zero8));
    }

    #[test]
    fn factory_rejects_wrong_chain_spec_and_oversized_block_fields() {
        let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());

        let mut env = evm_env();
        env.cfg_env.chain_id = 1;
        let err = match factory.create(env, InMemoryDB::default(), NoOpInspector {}) {
            Err(err) => err,
            Ok(_) => panic!("wrong Arc chain ID must be rejected"),
        };
        assert!(matches!(err, ArcEvmFactoryError::ChainId { actual: 1, .. }));

        let mut env = evm_env();
        env.cfg_env
            .set_spec_and_mainnet_gas_params(MainnetSpecId::PRAGUE);
        let err = match factory.create(env, InMemoryDB::default(), NoOpInspector {}) {
            Err(err) => err,
            Ok(_) => panic!("wrong Ethereum spec must be rejected"),
        };
        assert!(matches!(
            err,
            ArcEvmFactoryError::EthereumSpec {
                actual: MainnetSpecId::PRAGUE,
                ..
            }
        ));

        let mut block = evm_env().block_env;
        block.number = U256::from(u64::MAX) + U256::from(1);
        assert!(matches!(
            factory.execution_spec(&block),
            Err(ArcEvmFactoryError::BlockNumber(_))
        ));

        block.number = U256::ZERO;
        block.timestamp = U256::from(u64::MAX) + U256::from(1);
        assert!(matches!(
            factory.execution_spec(&block),
            Err(ArcEvmFactoryError::Timestamp(_))
        ));
    }

    #[test]
    fn osaka_standard_precompiles_include_p256() {
        let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());
        let evm = factory
            .create(evm_env(), InMemoryDB::default(), NoOpInspector {})
            .unwrap();
        let p256 = address!("0000000000000000000000000000000000000100");

        assert!(<PrecompilesMap as PrecompileProvider<
            ArcContext<InMemoryDB>,
        >>::contains(&evm.inner.precompiles, &p256));
    }

    #[test]
    fn normal_and_inspected_execution_share_the_arc_wrapper() {
        let factory = ArcEvmFactory::new(ArcChainConfig::mainnet());
        let mut normal = factory
            .create(evm_env(), InMemoryDB::default(), NoOpInspector {})
            .unwrap();
        let normal_result = normal.transact(simple_tx()).unwrap().result;

        let mut inspected = factory
            .create(evm_env(), InMemoryDB::default(), NoOpInspector {})
            .unwrap();
        let inspected_result = inspected.inspect_tx_commit(simple_tx()).unwrap();

        assert_eq!(normal_result, inspected_result);
        assert_eq!(
            normal.execution_spec().arc_flags,
            ArcHardforkFlags::from_schedule(ArcChainConfig::mainnet().hardforks(), 1, 1)
        );
    }
}
