//! ABI definitions for the B20 interfaces.
//!
//! Transcribed from Base reth (`base/crates/common/precompiles/src/common/abi.rs`,
//! `b20_asset/abi.rs`, `b20_stablecoin/abi.rs`). Selector sets must stay byte-identical to
//! Base's: an interface that is missing a selector reverts a call Base would execute, and one
//! that adds a selector executes a call Base would revert — either way the gas diverges.

use alloy::sol;

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20 {
        enum PausableFeature {
            TRANSFER,
            MINT,
            BURN
        }

        // --- Errors ---
        error NonPayable();
        error AccessControlUnauthorizedAccount(address account, bytes32 neededRole);
        error Unauthorized();
        error ContractPaused(PausableFeature feature);
        error InsufficientAllowance(address spender, uint256 allowance, uint256 needed);
        error InsufficientBalance(address sender, uint256 balance, uint256 needed);
        error InvalidSender(address sender);
        error InvalidReceiver(address receiver);
        error InvalidApprover(address approver);
        error InvalidSpender(address spender);
        error InvalidAmount();
        error EmptyFeatureSet();
        error InvalidSupplyCap(uint256 currentSupply, uint256 proposedCap);
        error SupplyCapExceeded(uint256 cap, uint256 attempted);
        error PolicyForbids(bytes32 policyScope, uint64 policyId);
        error PolicyNotFound(uint64 policyId);
        error UnsupportedPolicyType(bytes32 policyScope);
        error AccountNotBlocked(address account);
        error ExpiredSignature(uint256 deadline);
        error InvalidSigner(address signer, address owner);
        error LastAdminCannotRenounce();
        error NotSoleAdmin();
        error AccessControlBadConfirmation();

        // --- Events ---
        event Transfer(address indexed from, address indexed to, uint256 amount);
        event Approval(address indexed owner, address indexed spender, uint256 amount);
        event Memo(address indexed caller, bytes32 indexed memo);
        event BurnedBlocked(address indexed caller, address indexed from, uint256 amount);
        event RoleGranted(bytes32 indexed role, address indexed account, address indexed sender);
        event RoleRevoked(bytes32 indexed role, address indexed account, address indexed sender);
        event RoleAdminChanged(bytes32 indexed role, bytes32 indexed previousAdminRole, bytes32 indexed newAdminRole);
        event LastAdminRenounced(address indexed previousAdmin);
        event Paused(address indexed updater, PausableFeature[] features);
        event Unpaused(address indexed updater, PausableFeature[] features);
        event PolicyUpdated(bytes32 indexed policyScope, uint64 oldPolicyId, uint64 newPolicyId);
        event SupplyCapUpdated(address indexed updater, uint256 oldSupplyCap, uint256 newSupplyCap);
        event ContractURIUpdated();
        event NameUpdated(address indexed updater, string newName);
        event SymbolUpdated(address indexed updater, string newSymbol);
        event EIP712DomainChanged();

        // --- Role identifiers ---
        function DEFAULT_ADMIN_ROLE() external view returns (bytes32);
        function MINT_ROLE() external view returns (bytes32);
        function BURN_ROLE() external view returns (bytes32);
        function BURN_BLOCKED_ROLE() external view returns (bytes32);
        function PAUSE_ROLE() external view returns (bytes32);
        function UNPAUSE_ROLE() external view returns (bytes32);
        function METADATA_ROLE() external view returns (bytes32);

        // --- Policy type identifiers ---
        function TRANSFER_SENDER_POLICY() external view returns (bytes32);
        function TRANSFER_RECEIVER_POLICY() external view returns (bytes32);
        function TRANSFER_EXECUTOR_POLICY() external view returns (bytes32);
        function MINT_RECEIVER_POLICY() external view returns (bytes32);

        // --- ERC-20 ---
        function name() external view returns (string);
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
        function totalSupply() external view returns (uint256);
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function transfer(address to, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);

        // --- Metadata updates ---
        function updateName(string calldata newName) external;
        function updateSymbol(string calldata newSymbol) external;

        // --- Memo transfer variants ---
        function transferWithMemo(address to, uint256 amount, bytes32 memo) external returns (bool);
        function transferFromWithMemo(address from, address to, uint256 amount, bytes32 memo) external returns (bool);

        // --- Mint / burn ---
        function mint(address to, uint256 amount) external;
        function mintWithMemo(address to, uint256 amount, bytes32 memo) external;
        function burn(uint256 amount) external;
        function burnWithMemo(uint256 amount, bytes32 memo) external;
        function burnBlocked(address from, uint256 amount) external;

        // --- Roles ---
        function hasRole(bytes32 role, address account) external view returns (bool);
        function getRoleAdmin(bytes32 role) external view returns (bytes32);
        function grantRole(bytes32 role, address account) external;
        function revokeRole(bytes32 role, address account) external;
        function renounceRole(bytes32 role, address callerConfirmation) external;
        function renounceLastAdmin() external;
        function setRoleAdmin(bytes32 role, bytes32 newAdminRole) external;

        // --- Pause ---
        function pausedFeatures() external view returns (PausableFeature[] memory);
        function isPaused(PausableFeature feature) external view returns (bool);
        function pause(PausableFeature[] calldata features) external;
        function unpause(PausableFeature[] calldata features) external;

        // --- Policy ---
        function policyId(bytes32 policyScope) external view returns (uint64);
        function updatePolicy(bytes32 policyScope, uint64 newPolicyId) external;

        // --- Supply cap ---
        function supplyCap() external view returns (uint256);
        function updateSupplyCap(uint256 newSupplyCap) external;

        // --- Permit (EIP-2612 + ERC-5267) ---
        function DOMAIN_SEPARATOR() external view returns (bytes32);
        function nonces(address owner) external view returns (uint256);
        function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) external;
        function eip712Domain() external view returns (bytes1 fields, string memory name, string memory version, uint256 chainId, address verifyingContract, bytes32 salt, uint256[] memory extensions);

        // --- Contract URI (ERC-7572) ---
        function contractURI() external view returns (string);
        function updateContractURI(string calldata newURI) external;
    }
}

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Asset {
        // --- Errors ---
        error AnnouncementIdAlreadyUsed(string id);
        error InvalidMetadataKey();
        error InvalidMultiplier();
        error LengthMismatch(uint256 leftLen, uint256 rightLen);
        error EmptyBatch();
        error AnnouncementInProgress();
        error InternalCallMalformed(bytes call);
        error InternalCallFailed(bytes call);

        // --- Events ---
        event MultiplierUpdated(uint256 multiplier);
        event ExtraMetadataUpdated(string key, string value);
        event Announcement(address indexed caller, string id, string description, string uri);
        event EndAnnouncement(string id);

        // --- Role / precision identifiers ---
        function OPERATOR_ROLE() external view returns (bytes32);
        function WAD_PRECISION() external view returns (uint256);

        // --- Announcements ---
        function announce(
            bytes[] calldata internalCalls,
            string calldata id,
            string calldata description,
            string calldata uri
        ) external;
        function isAnnouncementIdUsed(string calldata id) external view returns (bool);

        // --- Multiplier ---
        function multiplier() external view returns (uint256);
        function toScaledBalance(uint256 rawBalance) external view returns (uint256);
        function toRawBalance(uint256 scaledBalance) external view returns (uint256 rawBalance);
        function scaledBalanceOf(address account) external view returns (uint256);
        function updateMultiplier(uint256 newMultiplier) external;

        // --- Batched issuance ---
        function batchMint(address[] calldata recipients, uint256[] calldata amounts) external;

        // --- Extra metadata ---
        function extraMetadata(string calldata key) external view returns (string);
        function updateExtraMetadata(
            string calldata key,
            string calldata value
        ) external;
    }
}

sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface IB20Stablecoin {
        function currency() external view returns (string);
    }
}
