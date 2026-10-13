//! Reuse shared ABIs; declare only Classic additions and changed return types.
use super::super::abi as shared;

pub(super) use shared::{
    IArbAddressTable as address_table, IArbFunctionTable as function_table,
    IArbStatistics as statistics,
};

pub(super) mod sys {
    pub use super::shared::IArbSys::*;
    alloy::sol! {
        function getTransactionCount(address account) external view returns (uint256);
        function getStorageAt(address account, uint256 index) external view returns (uint256);
    }
}

pub(super) mod bls {
    alloy::sol! {
        function register(uint256 x0, uint256 x1, uint256 y0, uint256 y1) external;
        function getPublicKey(address addr) external view returns (uint256, uint256, uint256, uint256);
    }
}

pub(super) mod test {
    pub use super::shared::IArbosTest::burnArbGasCall;
    alloy::sol! {
        function installAccount(address addr, bool isEOA, uint256 balance, uint256 nonce, bytes code, bytes initStorage) external;
        // These two methods return raw Mini data, not ABI-encoded tuples.
        function getMarshalledStorage(address addr) external view;
        function getAccountInfo(address addr) external view;
        function setNonce(address addr, uint256 nonce) external;
        function setBalance(address addr, uint256 balance) external;
        function setCode(address addr, bytes code) external;
        function setState(address addr, bytes state) external;
        function store(address addr, uint256 key, uint256 value) external;
    }
}

pub(super) mod gas_info {
    pub use super::shared::IArbGasInfo::*;
    alloy::sol! {
        function setL1GasPriceEstimate(uint256 price) external;
    }
}

pub(super) mod owner {
    pub use super::gas_info::setL1GasPriceEstimateCall;
    pub use super::shared::IArbOwner::{addChainOwnerCall, isChainOwnerCall, removeChainOwnerCall};
    alloy::sol! {
        function addToReserveFunds() external payable;
        function setFairGasPriceSender(address addr, bool enabled) external;
        function isFairGasPriceSender(address addr) external view returns (bool);
        function getAllFairGasPriceSenders() external view returns (bytes);
        function deployContract(bytes constructorData, address deemedSender, uint256 deemedNonce) external payable returns (address);
        function startCodeUpload() external;
        function startCodeUploadWithCheck(bytes32 oldCodeHash) external;
        function continueCodeUpload(bytes marshalledCode) external;
        function getUploadedCodeHash() external view returns (bytes32);
        function finishCodeUploadAsArbosUpgrade(bytes32 newCodeHash, bytes32 oldCodeHash) external;
        function getLastUpgradeHash() external view returns (bytes32);
        function getChainParameter(bytes32 which) external view returns (uint256);
        function setChainParameter(bytes32 which, uint256 value) external;
        function createChainParameter(bytes32 which, uint256 value) external;
        function serializeAllParameters() external view returns (bytes);
        function allowAllSenders() external;
        function allowOnlyOwnerToSend() external;
        function isAllowedSender(address addr) external view returns (bool);
        function addAllowedSender(address addr) external;
        function removeAllowedSender(address addr) external;
        function getAllAllowedSenders() external view returns (bytes);
        function getAllChainOwners() external view returns (bytes);
        function addMappingException(uint256 from, uint256 to) external;
        function removeMappingException(uint256 from, uint256 to) external;
        function isMappingException(uint256 from, uint256 to) external view returns (bool);
        function getAllMappingExceptions() external view returns (bytes);
        function getTotalOfEthBalances() external view returns (uint256);
        // Removed from the final Classic ABI, but valid in earlier blocks.
        function giveOwnership(address newOwner) external;
        function setFeesEnabled(bool enabled) external;
        function getFeeRecipients() external view returns (address, address);
        function setFeeRecipients(address netFeeRecipient, address congestionFeeRecipient) external;
        function setGasAccountingParams(uint256 speedLimitPerBlock, uint256 gasPoolMax, uint256 maxTxGasLimit) external;
        function setSecondsPerSend(uint256 blocksPerSend) external;
        function finishCodeUploadAsPluggable(uint256 id, bool keepState) external;
        function bindAddressToPluggable(address addr, uint256 pluggableId) external;
    }
}

pub(super) mod aggregator {
    pub use super::shared::IArbAggregator::*;
    alloy::sol! {
        function setPreferredAggregator(address prefAgg) external;
        function setDefaultAggregator(address newDefault) external;
    }
}

pub(super) mod retryable {
    pub use super::shared::IArbRetryableTx::{
        cancelCall, getBeneficiaryCall, getLifetimeCall, getTimeoutCall, keepaliveCall,
    };
    alloy::sol! {
        function redeem(bytes32 userTxHash) external;
        function getSubmissionPrice(uint256 calldataSize) external view returns (uint256, uint256);
        function getKeepalivePrice(bytes32 userTxHash) external view returns (uint256, uint256);
    }
}
