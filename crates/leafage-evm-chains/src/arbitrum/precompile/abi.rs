alloy::sol! {
    struct WeightedResource {
        uint8 resource;
        uint64 weight;
    }

    struct MultiGasConstraint {
        WeightedResource[] resources;
        uint32 adjustmentWindowSecs;
        uint64 targetPerSec;
        uint64 backlog;
    }

    interface IArbSys {
        error InvalidBlockNumber(uint256 requested, uint256 current);

        function arbBlockNumber() external view returns (uint256);
        function arbBlockHash(uint256 arbBlockNum) external view returns (bytes32);
        function arbChainID() external view returns (uint256);
        function arbOSVersion() external view returns (uint256);
        function getStorageGasAvailable() external view returns (uint256);
        function isTopLevelCall() external view returns (bool);
        function mapL1SenderContractAddressToL2Alias(address sender, address unused) external pure returns (address);
        function wasMyCallersAddressAliased() external view returns (bool);
        function myCallersAddressWithoutAliasing() external view returns (address);
        function withdrawEth(address destination) external payable returns (uint256);
        function sendTxToL1(address destination, bytes calldata data) external payable returns (uint256);
        function sendMerkleTreeState() external view returns (uint256 size, bytes32 root, bytes32[] memory partials);
        event L2ToL1Tx(
            address caller,
            address indexed destination,
            uint256 indexed hash,
            uint256 indexed position,
            uint256 arbBlockNum,
            uint256 ethBlockNum,
            uint256 timestamp,
            uint256 callvalue,
            bytes data
        );
        event L2ToL1Transaction(
            address caller,
            address indexed destination,
            uint256 indexed uniqueId,
            uint256 indexed batchNumber,
            uint256 indexInBatch,
            uint256 arbBlockNum,
            uint256 ethBlockNum,
            uint256 timestamp,
            uint256 callvalue,
            bytes data
        );
        event SendMerkleUpdate(uint256 indexed reserved, bytes32 indexed hash, uint256 indexed position);
    }

    interface IArbInfo {
        function getBalance(address account) external view returns (uint256);
        function getCode(address account) external view returns (bytes memory);
    }

    interface IArbAddressTable {
        function addressExists(address addr) external view returns (bool);
        function compress(address addr) external returns (bytes memory);
        function decompress(bytes calldata buf, uint256 offset) external view returns (address, uint256);
        function lookup(address addr) external view returns (uint256);
        function lookupIndex(uint256 index) external view returns (address);
        function register(address addr) external returns (uint256);
        function size() external view returns (uint256);
    }

    interface IArbFunctionTable {
        function upload(bytes calldata buf) external;
        function size(address addr) external view returns (uint256);
        function get(address addr, uint256 index) external view returns (uint256, bool, uint256);
    }

    interface IArbosTest {
        function burnArbGas(uint256 gasAmount) external pure;
    }

    interface IArbGasInfo {
        function getPricesInWeiWithAggregator(address aggregator) external view returns (uint256, uint256, uint256, uint256, uint256, uint256);
        function getPricesInWei() external view returns (uint256, uint256, uint256, uint256, uint256, uint256);
        function getPricesInArbGasWithAggregator(address aggregator) external view returns (uint256, uint256, uint256);
        function getPricesInArbGas() external view returns (uint256, uint256, uint256);
        function getGasAccountingParams() external view returns (uint256, uint256, uint256);
        function getMaxTxGasLimit() external view returns (uint256);
        function getMinimumGasPrice() external view returns (uint256);
        function getL1BaseFeeEstimate() external view returns (uint256);
        function getL1BaseFeeEstimateInertia() external view returns (uint64);
        function getL1RewardRate() external view returns (uint64);
        function getL1RewardRecipient() external view returns (address);
        function getL1GasPriceEstimate() external view returns (uint256);
        function getCurrentTxL1GasFees() external view returns (uint256);
        function getGasBacklog() external view returns (uint64);
        function getPricingInertia() external view returns (uint64);
        function getGasBacklogTolerance() external view returns (uint64);
        function getL1PricingSurplus() external view returns (int256);
        function getPerBatchGasCharge() external view returns (int64);
        function getAmortizedCostCapBips() external view returns (uint64);
        function getL1FeesAvailable() external view returns (uint256);
        function getL1PricingEquilibrationUnits() external view returns (uint256);
        function getLastL1PricingUpdateTime() external view returns (uint64);
        function getL1PricingFundsDueForRewards() external view returns (uint256);
        function getL1PricingUnitsSinceUpdate() external view returns (uint64);
        function getLastL1PricingSurplus() external view returns (int256);
        function getMaxBlockGasLimit() external view returns (uint64);
        function getGasPricingConstraints() external view returns (uint64[3][] memory constraints);
        function getMultiGasPricingConstraints() external view returns (MultiGasConstraint[] memory constraints);
        function getMultiGasBaseFee() external view returns (uint256[] memory baseFees);
    }

    interface IArbAggregator {
        function getPreferredAggregator(address addr) external view returns (address, bool);
        function getDefaultAggregator() external view returns (address);
        function getBatchPosters() external view returns (address[] memory);
        function addBatchPoster(address newBatchPoster) external;
        function getFeeCollector(address batchPoster) external view returns (address);
        function setFeeCollector(address batchPoster, address newFeeCollector) external;
        function getTxBaseFee(address aggregator) external view returns (uint256);
        function setTxBaseFee(address aggregator, uint256 feeInL1Gas) external;
    }

    interface IArbRetryableTx {
        function redeem(bytes32 ticketId) external returns (bytes32);
        function getLifetime() external view returns (uint256);
        function getTimeout(bytes32 ticketId) external view returns (uint256);
        function keepalive(bytes32 ticketId) external returns (uint256);
        function getBeneficiary(bytes32 ticketId) external view returns (address);
        function cancel(bytes32 ticketId) external;
        function getCurrentRedeemer() external view returns (address);
        function submitRetryable(
            bytes32 requestId,
            uint256 l1BaseFee,
            uint256 deposit,
            uint256 callvalue,
            uint256 gasFeeCap,
            uint64 gasLimit,
            uint256 maxSubmissionFee,
            address feeRefundAddress,
            address beneficiary,
            address retryTo,
            bytes calldata retryData
        ) external;
        event TicketCreated(bytes32 indexed ticketId);
        event LifetimeExtended(bytes32 indexed ticketId, uint256 newTimeout);
        event RedeemScheduled(
            bytes32 indexed ticketId,
            bytes32 indexed retryTxHash,
            uint64 indexed sequenceNum,
            uint64 donatedGas,
            address gasDonor,
            uint256 maxRefund,
            uint256 submissionFeeRefund
        );
        event Canceled(bytes32 indexed ticketId);
        event Redeemed(bytes32 indexed userTxHash);
        error NoTicketWithID();
        error NotCallable();
    }

    interface IArbOwner {
        function addChainOwner(address newOwner) external;
        function removeChainOwner(address ownerToRemove) external;
        function isChainOwner(address addr) external view returns (bool);
        function getAllChainOwners() external view returns (address[] memory);
        function setNativeTokenManagementFrom(uint64 timestamp) external;
        function setTransactionFilteringFrom(uint64 timestamp) external;
        function addNativeTokenOwner(address newOwner) external;
        function removeNativeTokenOwner(address ownerToRemove) external;
        function isNativeTokenOwner(address addr) external view returns (bool);
        function getAllNativeTokenOwners() external view returns (address[] memory);
        function addTransactionFilterer(address filterer) external;
        function removeTransactionFilterer(address filterer) external;
        function isTransactionFilterer(address filterer) external view returns (bool);
        function getAllTransactionFilterers() external view returns (address[] memory);
        function setFilteredFundsRecipient(address newRecipient) external;
        function getFilteredFundsRecipient() external view returns (address);
        function setL1BaseFeeEstimateInertia(uint64 inertia) external;
        function setL2BaseFee(uint256 priceInWei) external;
        function setMinimumL2BaseFee(uint256 priceInWei) external;
        function setSpeedLimit(uint64 limit) external;
        function setMaxTxGasLimit(uint64 limit) external;
        function setMaxBlockGasLimit(uint64 limit) external;
        function setL2GasPricingInertia(uint64 sec) external;
        function setL2GasBacklogTolerance(uint64 sec) external;
        function getNetworkFeeAccount() external view returns (address);
        function getInfraFeeAccount() external view returns (address);
        function setNetworkFeeAccount(address newNetworkFeeAccount) external;
        function setInfraFeeAccount(address newInfraFeeAccount) external;
        function scheduleArbOSUpgrade(uint64 newVersion, uint64 timestamp) external;
        function setL1PricingEquilibrationUnits(uint256 equilibrationUnits) external;
        function setL1PricingInertia(uint64 inertia) external;
        function setL1PricingRewardRecipient(address recipient) external;
        function setL1PricingRewardRate(uint64 weiPerUnit) external;
        function setL1PricePerUnit(uint256 pricePerUnit) external;
        function setParentGasFloorPerToken(uint64 floorPerToken) external;
        function setPerBatchGasCharge(int64 cost) external;
        function setBrotliCompressionLevel(uint64 level) external;
        function setAmortizedCostCapBips(uint64 cap) external;
        function releaseL1PricerSurplusFunds(uint256 maxWeiToRelease) external returns (uint256);
        function setInkPrice(uint32 price) external;
        function setWasmMaxStackDepth(uint32 depth) external;
        function setWasmFreePages(uint16 pages) external;
        function setWasmPageGas(uint16 gas) external;
        function setWasmPageLimit(uint16 limit) external;
        function setWasmMaxSize(uint32 size) external;
        function setWasmMinInitGas(uint8 gas, uint16 cached) external;
        function setWasmInitCostScalar(uint64 percent) external;
        function setWasmExpiryDays(uint16 days) external;
        function setWasmKeepaliveDays(uint16 days) external;
        function setWasmBlockCacheSize(uint16 count) external;
        function addWasmCacheManager(address manager) external;
        function removeWasmCacheManager(address manager) external;
        function setChainConfig(string calldata chainConfig) external;
        function setCalldataPriceIncrease(bool enable) external;
        function setGasBacklog(uint64 backlog) external;
        function setGasPricingConstraints(uint64[3][] calldata constraints) external;
        function setMultiGasPricingConstraints(MultiGasConstraint[] calldata constraints) external;
        function setCollectTips(bool collectTips) external;
        function setMaxStylusContractFragments(uint8 maxFragments) external;
        function setWasmActivationGas(uint64 gas) external;
        event TransactionFiltererAdded(address indexed filterer);
        event TransactionFiltererRemoved(address indexed filterer);
        event FilteredFundsRecipientSet(address indexed newRecipient);
        event ChainOwnerAdded(address indexed owner);
        event ChainOwnerRemoved(address indexed owner);
        event NativeTokenOwnerAdded(address indexed owner);
        event NativeTokenOwnerRemoved(address indexed owner);
        event OwnerActs(bytes4 indexed method, address indexed owner, bytes data);
    }

    interface IArbOwnerPublic {
        function isChainOwner(address addr) external view returns (bool);
        function rectifyChainOwner(address ownerToRectify) external;
        function getAllChainOwners() external view returns (address[] memory);
        function getNativeTokenManagementFrom() external view returns (uint64);
        function isNativeTokenOwner(address addr) external view returns (bool);
        function getAllNativeTokenOwners() external view returns (address[] memory);
        function getTransactionFilteringFrom() external view returns (uint64);
        function isTransactionFilterer(address filterer) external view returns (bool);
        function getAllTransactionFilterers() external view returns (address[] memory);
        function getFilteredFundsRecipient() external view returns (address);
        function getNetworkFeeAccount() external view returns (address);
        function getInfraFeeAccount() external view returns (address);
        function getBrotliCompressionLevel() external view returns (uint64);
        function getParentGasFloorPerToken() external view returns (uint64);
        function getScheduledUpgrade() external view returns (uint64 arbosVersion, uint64 scheduledForTimestamp);
        function isCalldataPriceIncreaseEnabled() external view returns (bool);
        function getCollectTips() external view returns (bool);
        function getMaxStylusContractFragments() external view returns (uint8);
        event ChainOwnerRectified(address rectifiedOwner);
    }

    interface IArbStatistics {
        function getStats() external view returns (uint256, uint256, uint256, uint256, uint256, uint256);
    }

    interface IArbWasm {
        function activateProgram(address program) external payable returns (uint16 version, uint256 dataFee);
        function stylusVersion() external view returns (uint16 version);
        function codehashVersion(bytes32 codehash) external view returns (uint16 version);
        function codehashKeepalive(bytes32 codehash) external payable;
        function codehashAsmSize(bytes32 codehash) external view returns (uint32 size);
        function programVersion(address program) external view returns (uint16 version);
        function programInitGas(address program) external view returns (uint64 gas, uint64 gasWhenCached);
        function programMemoryFootprint(address program) external view returns (uint16 footprint);
        function programTimeLeft(address program) external view returns (uint64 secs);
        function inkPrice() external view returns (uint32 price);
        function maxStackDepth() external view returns (uint32 depth);
        function freePages() external view returns (uint16 pages);
        function pageGas() external view returns (uint16 gas);
        function pageRamp() external view returns (uint64 ramp);
        function pageLimit() external view returns (uint16 limit);
        function minInitGas() external view returns (uint64 gas, uint64 cached);
        function initCostScalar() external view returns (uint64 percent);
        function expiryDays() external view returns (uint16 days);
        function keepaliveDays() external view returns (uint16 days);
        function blockCacheSize() external view returns (uint16 count);
        function activationGas() external view returns (uint64 gas);
        event ProgramActivated(
            bytes32 indexed codehash,
            bytes32 moduleHash,
            address program,
            uint256 dataFee,
            uint16 version
        );
        event ProgramLifetimeExtended(bytes32 indexed codehash, uint256 dataFee);
        error ProgramNotWasm();
        error ProgramNotActivated();
        error ProgramNeedsUpgrade(uint16 version, uint16 stylusVersion);
        error ProgramExpired(uint64 ageInSeconds);
        error ProgramUpToDate();
        error ProgramKeepaliveTooSoon(uint64 ageInSeconds);
        error ProgramInsufficientValue(uint256 have, uint256 want);
    }

    interface IArbWasmCache {
        function isCacheManager(address manager) external view returns (bool);
        function allCacheManagers() external view returns (address[] memory managers);
        function cacheCodehash(bytes32 codehash) external;
        function cacheProgram(address addr) external;
        function evictCodehash(bytes32 codehash) external;
        function codehashIsCached(bytes32 codehash) external view returns (bool);
        event UpdateProgramCache(address indexed manager, bytes32 indexed codehash, bool cached);
    }

    interface IArbNativeTokenManager {
        function mintNativeToken(uint256 amount) external;
        function burnNativeToken(uint256 amount) external;
        event NativeTokenMinted(address indexed to, uint256 amount);
        event NativeTokenBurned(address indexed from, uint256 amount);
    }

    interface IArbFilteredTransactionsManager {
        function addFilteredTransaction(bytes32 txHash) external;
        function deleteFilteredTransaction(bytes32 txHash) external;
        function isTransactionFiltered(bytes32 txHash) external view returns (bool);
        event FilteredTransactionAdded(bytes32 indexed txHash);
        event FilteredTransactionDeleted(bytes32 indexed txHash);
    }

    interface IArbosActs {
        function startBlock(uint256 l1BaseFee, uint64 l1BlockNumber, uint64 l2BlockNumber, uint64 timePassed) external;
        function batchPostingReport(
            uint256 batchTimestamp,
            address batchPosterAddress,
            uint64 batchNumber,
            uint64 batchDataGas,
            uint256 l1BaseFeeWei
        ) external;
        function batchPostingReportV2(
            uint256 batchTimestamp,
            address batchPosterAddress,
            uint64 batchNumber,
            uint64 batchCalldataLength,
            uint64 batchCalldataNonZeros,
            uint64 batchExtraGas,
            uint256 l1BaseFeeWei
        ) external;
        error CallerNotArbOS();
    }

    interface IArbDebug {
        function becomeChainOwner() external;
        function overwriteContractCode(address target, bytes calldata newCode) external returns (bytes memory oldCode);
        function events(bool flag, bytes32 value) external payable returns (address, uint256);
        function eventsView() external view;
        function customRevert(uint64 number) external pure;
        function panic() external;
        function legacyError() external pure;
        event Basic(bool flag, bytes32 indexed value);
        event Mixed(bool indexed flag, bool not, bytes32 indexed value, address conn, address indexed caller);
        event Store(bool indexed flag, address indexed field, uint24 number, bytes32 value, bytes store);
        error Custom(uint64, string, bool);
        error Unused();
    }
}
