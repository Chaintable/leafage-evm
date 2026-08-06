use super::abi::{
    IArbAddressTable, IArbAggregator, IArbDebug, IArbFilteredTransactionsManager,
    IArbFunctionTable, IArbGasInfo, IArbInfo, IArbNativeTokenManager, IArbOwnerPublic,
    IArbRetryableTx, IArbStatistics, IArbSys, IArbWasm, IArbWasmCache, IArbosActs, IArbosTest,
};
use super::address_table::ArbAddressTable;
use super::aggregator::ArbAggregator;
use super::arb_bls::ArbBls;
use super::arb_info::ArbInfo;
use super::arb_sys::ArbSys;
use super::arbos_acts::ArbosActs;
use super::arbos_test::ArbosTest;
use super::debug::ArbDebug;
use super::env::ArbPrecompileInput;
use super::filtered_transactions::ArbFilteredTransactionsManager;
use super::function_table::ArbFunctionTable;
use super::gas_info::ArbGasInfo;
use super::native_token_manager::ArbNativeTokenManager;
use super::owner::ArbOwner;
use super::owner_public::ArbOwnerPublic;
use super::retryable_tx::ArbRetryableTx;
use super::statistics::ArbStatistics;
use super::wasm::ArbWasm;
use super::wasm_cache::ArbWasmCache;
use super::{
    ArbitrumContext, ARBOS_ACTS_ADDRESS, ARBOS_TEST_ADDRESS, ARBOS_VERSION_NATIVE_TOKEN,
    ARBOS_VERSION_STYLUS, ARBOS_VERSION_TRANSACTION_FILTERING, ARB_ADDRESS_TABLE_ADDRESS,
    ARB_AGGREGATOR_ADDRESS, ARB_BLS_ADDRESS, ARB_DEBUG_ADDRESS,
    ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS, ARB_FUNCTION_TABLE_ADDRESS, ARB_GAS_INFO_ADDRESS,
    ARB_INFO_ADDRESS, ARB_NATIVE_TOKEN_MANAGER_ADDRESS, ARB_OWNER_ADDRESS,
    ARB_OWNER_PUBLIC_ADDRESS, ARB_RETRYABLE_TX_ADDRESS, ARB_STATISTICS_ADDRESS, ARB_SYS_ADDRESS,
    ARB_WASM_ADDRESS, ARB_WASM_CACHE_ADDRESS,
};
use alloy::primitives::Address;
use alloy::sol_types::SolInterface;
use revm::precompile::PrecompileResult;
use revm::{Database, DatabaseRef};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ArbitrumPrecompile {
    ArbSys,
    ArbInfo,
    ArbAddressTable,
    ArbBls,
    ArbFunctionTable,
    ArbosTest,
    ArbOwnerPublic,
    ArbGasInfo,
    ArbAggregator,
    ArbRetryableTx,
    ArbStatistics,
    ArbOwner,
    ArbWasm,
    ArbWasmCache,
    ArbNativeTokenManager,
    ArbFilteredTransactionsManager,
    ArbDebug,
    ArbosActs,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum ArbitrumPrecompilePurity {
    Pure,
    View,
    Write,
    Payable,
}

impl ArbitrumPrecompilePurity {
    pub(super) fn uses_precompile_context(self) -> bool {
        self >= Self::View
    }

    pub(super) fn mutates_state(self) -> bool {
        self >= Self::Write
    }

    pub(super) fn accepts_value(self) -> bool {
        self == Self::Payable
    }
}

impl ArbitrumPrecompile {
    pub(super) const ALL: [Self; 18] = [
        Self::ArbSys,
        Self::ArbInfo,
        Self::ArbAddressTable,
        Self::ArbBls,
        Self::ArbFunctionTable,
        Self::ArbosTest,
        Self::ArbOwnerPublic,
        Self::ArbGasInfo,
        Self::ArbAggregator,
        Self::ArbRetryableTx,
        Self::ArbStatistics,
        Self::ArbOwner,
        Self::ArbWasm,
        Self::ArbWasmCache,
        Self::ArbNativeTokenManager,
        Self::ArbFilteredTransactionsManager,
        Self::ArbDebug,
        Self::ArbosActs,
    ];

    pub(super) fn from_address(address: Address) -> Option<Self> {
        match address {
            ARB_SYS_ADDRESS => Some(Self::ArbSys),
            ARB_INFO_ADDRESS => Some(Self::ArbInfo),
            ARB_ADDRESS_TABLE_ADDRESS => Some(Self::ArbAddressTable),
            ARB_BLS_ADDRESS => Some(Self::ArbBls),
            ARB_FUNCTION_TABLE_ADDRESS => Some(Self::ArbFunctionTable),
            ARBOS_TEST_ADDRESS => Some(Self::ArbosTest),
            ARB_OWNER_PUBLIC_ADDRESS => Some(Self::ArbOwnerPublic),
            ARB_GAS_INFO_ADDRESS => Some(Self::ArbGasInfo),
            ARB_AGGREGATOR_ADDRESS => Some(Self::ArbAggregator),
            ARB_RETRYABLE_TX_ADDRESS => Some(Self::ArbRetryableTx),
            ARB_STATISTICS_ADDRESS => Some(Self::ArbStatistics),
            ARB_OWNER_ADDRESS => Some(Self::ArbOwner),
            ARB_WASM_ADDRESS => Some(Self::ArbWasm),
            ARB_WASM_CACHE_ADDRESS => Some(Self::ArbWasmCache),
            ARB_NATIVE_TOKEN_MANAGER_ADDRESS => Some(Self::ArbNativeTokenManager),
            ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS => Some(Self::ArbFilteredTransactionsManager),
            ARB_DEBUG_ADDRESS => Some(Self::ArbDebug),
            ARBOS_ACTS_ADDRESS => Some(Self::ArbosActs),
            _ => None,
        }
    }

    pub(super) fn address(self) -> Address {
        match self {
            Self::ArbSys => ARB_SYS_ADDRESS,
            Self::ArbInfo => ARB_INFO_ADDRESS,
            Self::ArbAddressTable => ARB_ADDRESS_TABLE_ADDRESS,
            Self::ArbBls => ARB_BLS_ADDRESS,
            Self::ArbFunctionTable => ARB_FUNCTION_TABLE_ADDRESS,
            Self::ArbosTest => ARBOS_TEST_ADDRESS,
            Self::ArbOwnerPublic => ARB_OWNER_PUBLIC_ADDRESS,
            Self::ArbGasInfo => ARB_GAS_INFO_ADDRESS,
            Self::ArbAggregator => ARB_AGGREGATOR_ADDRESS,
            Self::ArbRetryableTx => ARB_RETRYABLE_TX_ADDRESS,
            Self::ArbStatistics => ARB_STATISTICS_ADDRESS,
            Self::ArbOwner => ARB_OWNER_ADDRESS,
            Self::ArbWasm => ARB_WASM_ADDRESS,
            Self::ArbWasmCache => ARB_WASM_CACHE_ADDRESS,
            Self::ArbNativeTokenManager => ARB_NATIVE_TOKEN_MANAGER_ADDRESS,
            Self::ArbFilteredTransactionsManager => ARB_FILTERED_TRANSACTIONS_MANAGER_ADDRESS,
            Self::ArbDebug => ARB_DEBUG_ADDRESS,
            Self::ArbosActs => ARBOS_ACTS_ADDRESS,
        }
    }

    fn min_arbos_version(self) -> u64 {
        match self {
            Self::ArbWasm | Self::ArbWasmCache => ARBOS_VERSION_STYLUS,
            Self::ArbNativeTokenManager => ARBOS_VERSION_NATIVE_TOKEN,
            Self::ArbFilteredTransactionsManager => ARBOS_VERSION_TRANSACTION_FILTERING,
            _ => 0,
        }
    }

    pub(super) fn is_active(self, arbos_version: u64) -> bool {
        arbos_version >= self.min_arbos_version()
    }

    pub(super) fn purity(self, data: &[u8]) -> Option<ArbitrumPrecompilePurity> {
        use ArbitrumPrecompilePurity::{Payable, Pure, View, Write};

        match self {
            Self::ArbSys => IArbSys::IArbSysCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IArbSys::IArbSysCalls::mapL1SenderContractAddressToL2Alias(_) => Pure,
                    IArbSys::IArbSysCalls::withdrawEth(_)
                    | IArbSys::IArbSysCalls::sendTxToL1(_) => Payable,
                    _ => View,
                }),
            Self::ArbInfo => Self::fixed_purity::<IArbInfo::IArbInfoCalls>(data, View),
            Self::ArbAddressTable => IArbAddressTable::IArbAddressTableCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IArbAddressTable::IArbAddressTableCalls::compress(_)
                    | IArbAddressTable::IArbAddressTableCalls::register(_) => Write,
                    _ => View,
                }),
            Self::ArbBls => None,
            Self::ArbFunctionTable => IArbFunctionTable::IArbFunctionTableCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IArbFunctionTable::IArbFunctionTableCalls::upload(_) => Write,
                    _ => View,
                }),
            Self::ArbosTest => Self::fixed_purity::<IArbosTest::IArbosTestCalls>(data, Pure),
            Self::ArbOwnerPublic => IArbOwnerPublic::IArbOwnerPublicCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IArbOwnerPublic::IArbOwnerPublicCalls::rectifyChainOwner(_) => Write,
                    _ => View,
                }),
            Self::ArbGasInfo => Self::fixed_purity::<IArbGasInfo::IArbGasInfoCalls>(data, View),
            Self::ArbAggregator => IArbAggregator::IArbAggregatorCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IArbAggregator::IArbAggregatorCalls::addBatchPoster(_)
                    | IArbAggregator::IArbAggregatorCalls::setFeeCollector(_)
                    | IArbAggregator::IArbAggregatorCalls::setTxBaseFee(_) => Write,
                    _ => View,
                }),
            Self::ArbRetryableTx => IArbRetryableTx::IArbRetryableTxCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IArbRetryableTx::IArbRetryableTxCalls::getLifetime(_)
                    | IArbRetryableTx::IArbRetryableTxCalls::getTimeout(_)
                    | IArbRetryableTx::IArbRetryableTxCalls::getBeneficiary(_)
                    | IArbRetryableTx::IArbRetryableTxCalls::getCurrentRedeemer(_) => View,
                    _ => Write,
                }),
            Self::ArbStatistics => Self::fixed_purity::<IArbStatistics::IArbStatisticsCalls>(
                data, View,
            ),
            Self::ArbOwner => None,
            Self::ArbWasm => IArbWasm::IArbWasmCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IArbWasm::IArbWasmCalls::activateProgram(_)
                    | IArbWasm::IArbWasmCalls::codehashKeepalive(_) => Payable,
                    _ => View,
                }),
            Self::ArbWasmCache => IArbWasmCache::IArbWasmCacheCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IArbWasmCache::IArbWasmCacheCalls::cacheCodehash(_)
                    | IArbWasmCache::IArbWasmCacheCalls::cacheProgram(_)
                    | IArbWasmCache::IArbWasmCacheCalls::evictCodehash(_) => Write,
                    _ => View,
                }),
            Self::ArbNativeTokenManager => {
                Self::fixed_purity::<IArbNativeTokenManager::IArbNativeTokenManagerCalls>(
                    data, Write,
                )
            }
            Self::ArbFilteredTransactionsManager => {
                IArbFilteredTransactionsManager::IArbFilteredTransactionsManagerCalls::abi_decode(
                    data,
                )
                .ok()
                .map(|call| match call {
                    IArbFilteredTransactionsManager::IArbFilteredTransactionsManagerCalls::isTransactionFiltered(_) => {
                        View
                    }
                    _ => Write,
                })
            }
            Self::ArbDebug => IArbDebug::IArbDebugCalls::abi_decode(data)
                .ok()
                .map(|call| match call {
                    IArbDebug::IArbDebugCalls::customRevert(_)
                    | IArbDebug::IArbDebugCalls::legacyError(_) => Pure,
                    IArbDebug::IArbDebugCalls::eventsView(_) => View,
                    IArbDebug::IArbDebugCalls::events(_) => Payable,
                    _ => Write,
                }),
            Self::ArbosActs => Self::fixed_purity::<IArbosActs::IArbosActsCalls>(data, Write),
        }
    }

    fn fixed_purity<T: SolInterface>(
        data: &[u8],
        purity: ArbitrumPrecompilePurity,
    ) -> Option<ArbitrumPrecompilePurity> {
        T::abi_decode(data).ok().map(|_| purity)
    }

    pub(super) fn run<DB: Database + DatabaseRef>(
        self,
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        match self {
            Self::ArbSys => ArbSys::run(input),
            Self::ArbInfo => ArbInfo::run(input),
            Self::ArbAddressTable => ArbAddressTable::run(input),
            Self::ArbBls => ArbBls::run(input),
            Self::ArbFunctionTable => ArbFunctionTable::run(input),
            Self::ArbosTest => ArbosTest::run(input),
            Self::ArbOwnerPublic => ArbOwnerPublic::run(input),
            Self::ArbGasInfo => ArbGasInfo::run(input),
            Self::ArbAggregator => ArbAggregator::run(input),
            Self::ArbRetryableTx => ArbRetryableTx::run(input),
            Self::ArbStatistics => ArbStatistics::run(input),
            Self::ArbOwner => ArbOwner::run(input),
            Self::ArbWasm => ArbWasm::run(input),
            Self::ArbWasmCache => ArbWasmCache::run(input),
            Self::ArbNativeTokenManager => ArbNativeTokenManager::run(input),
            Self::ArbFilteredTransactionsManager => ArbFilteredTransactionsManager::run(input),
            Self::ArbDebug => ArbDebug::run(input),
            Self::ArbosActs => ArbosActs::run(input),
        }
    }
}
