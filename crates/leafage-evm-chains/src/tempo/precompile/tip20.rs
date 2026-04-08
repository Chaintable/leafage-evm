//! TIP-20 token precompile -- Tempo's native fungible token standard.
//!
//! Each TIP-20 token lives at a deterministic address with the `0x20C0` prefix.
//! Provides ERC-20-like functionality (balances, allowances, transfers) with
//! additional features: role-based access control, pausability, supply caps,
//! transfer policies (TIP-403), and opt-in staking rewards.
//!
//! Ported from `tempo/crates/precompiles/src/tip20/`.
//!
//! ## Storage layout
//!
//! The `#[contract]` macro in the original Tempo codebase assigns storage slots
//! sequentially with packing for small types. The layout is reproduced here
//! manually to match the on-chain storage exactly.
//!
//! | Slot | Field                    | Type                                    |
//! |------|--------------------------|-----------------------------------------|
//! |  0   | roles                    | Mapping<Address, Mapping<B256, bool>>   |
//! |  1   | role_admins              | Mapping<B256, B256>                     |
//! |  2   | name                     | String                                  |
//! |  3   | symbol                   | String                                  |
//! |  4   | currency                 | String                                  |
//! |  5   | _domain_separator        | B256                                    |
//! |  6   | quote_token              | Address                                 |
//! |  7   | next_quote_token         | Address (offset 0)                      |
//! |  7   | transfer_policy_id       | u64 (offset 20, packed)                 |
//! |  8   | total_supply             | U256                                    |
//! |  9   | balances                 | Mapping<Address, U256>                  |
//! | 10   | allowances               | Mapping<Address, Mapping<Address, U256>>|
//! | 11   | permit_nonces            | Mapping<Address, U256>                  |
//! | 12   | paused                   | bool                                    |
//! | 13   | supply_cap               | U256                                    |
//! | 14   | _salts                   | Mapping<B256, bool>                     |
//! | 15   | global_reward_per_token  | U256                                    |
//! | 16   | opted_in_supply          | u128                                    |
//! | 17   | user_reward_info         | Mapping<Address, UserRewardInfo>        |

use alloy::primitives::{keccak256, Address, Bytes, B256, U256};
use alloy::sol_types::{SolError, SolInterface, SolValue};
use revm::precompile::{PrecompileError, PrecompileResult};
use std::sync::LazyLock;

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx, StorageOps};
use super::storage_types::{
    BytesLikeHandler, FromWord, Handler, Layout, LayoutCtx, Mapping, Slot, Storable, StorableType,
};
use super::{
    dispatch_call, input_cost, metadata, mutate, mutate_void, view, Precompile,
    STABLECOIN_DEX_ADDRESS, TIP_FEE_MANAGER_ADDRESS,
};

// ===========================================================================
// Constants
// ===========================================================================

/// u128::MAX as U256
pub const U128_MAX: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

/// Decimal precision for TIP-20 tokens
const TIP20_DECIMALS: u8 = 6;

/// TIP20 token address prefix (12 bytes)
const TIP20_TOKEN_PREFIX: [u8; 12] = [
    0x20, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Precision multiplier for reward-per-token accumulator (1e18).
pub const ACC_PRECISION: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);

/// The default admin role (zero hash). Holders can grant/revoke any role.
pub const DEFAULT_ADMIN_ROLE: B256 = B256::ZERO;

/// A self-administered role that cannot be granted by any admin.
pub const UNGRANTABLE_ROLE: B256 = B256::new([0xff; 32]);

/// Role hash for pausing token transfers.
pub static PAUSE_ROLE: LazyLock<B256> = LazyLock::new(|| keccak256(b"PAUSE_ROLE"));
/// Role hash for unpausing token transfers.
pub static UNPAUSE_ROLE: LazyLock<B256> = LazyLock::new(|| keccak256(b"UNPAUSE_ROLE"));
/// Role hash for minting new tokens.
pub static ISSUER_ROLE: LazyLock<B256> = LazyLock::new(|| keccak256(b"ISSUER_ROLE"));
/// Role hash that prevents an account from burning tokens.
pub static BURN_BLOCKED_ROLE: LazyLock<B256> = LazyLock::new(|| keccak256(b"BURN_BLOCKED_ROLE"));

/// Returns true if the address has the TIP20 prefix.
pub fn is_tip20_prefix(token: Address) -> bool {
    token.as_slice().starts_with(&TIP20_TOKEN_PREFIX)
}

// ===========================================================================
// Solidity ABI types
// ===========================================================================

alloy::sol! {
    // ---- ITIP20 interface ----

    interface ITIP20 {
        // Metadata (view)
        function name() external view returns (string memory);
        function symbol() external view returns (string memory);
        function decimals() external view returns (uint8);
        function currency() external view returns (string memory);
        function totalSupply() external view returns (uint256);
        function supplyCap() external view returns (uint256);
        function transferPolicyId() external view returns (uint64);
        function paused() external view returns (bool);
        function quoteToken() external view returns (address);
        function nextQuoteToken() external view returns (address);

        // Role constants (view)
        function PAUSE_ROLE() external view returns (bytes32);
        function UNPAUSE_ROLE() external view returns (bytes32);
        function ISSUER_ROLE() external view returns (bytes32);
        function BURN_BLOCKED_ROLE() external view returns (bytes32);

        // View functions
        function balanceOf(address account) external view returns (uint256);
        function allowance(address owner, address spender) external view returns (uint256);
        function nonces(address owner) external view returns (uint256);
        function DOMAIN_SEPARATOR() external view returns (bytes32);

        // Reward view functions
        function globalRewardPerToken() external view returns (uint256);
        function optedInSupply() external view returns (uint128);
        function userRewardInfo(address account) external view returns (UserRewardInfo memory);
        function getPendingRewards(address account) external view returns (uint128);

        // State-changing functions
        function transfer(address to, uint256 amount) external returns (bool);
        function transferFrom(address from, address to, uint256 amount) external returns (bool);
        function approve(address spender, uint256 amount) external returns (bool);
        function mint(address to, uint256 amount) external;
        function mintWithMemo(address to, uint256 amount, bytes32 memo) external;
        function burn(uint256 amount) external;
        function burnWithMemo(uint256 amount, bytes32 memo) external;
        function burnBlocked(address from, uint256 amount) external;
        function pause() external;
        function unpause() external;
        function setSupplyCap(uint256 newSupplyCap) external;
        function changeTransferPolicyId(uint64 newPolicyId) external;
        function setNextQuoteToken(address newQuoteToken) external;
        function completeQuoteTokenUpdate() external;
        function transferWithMemo(address to, uint256 amount, bytes32 memo) external;
        function transferFromWithMemo(address from, address to, uint256 amount, bytes32 memo) external returns (bool);
        function permit(address owner, address spender, uint256 value, uint256 deadline, uint8 v, bytes32 r, bytes32 s) external;

        // Reward state-changing functions
        function distributeReward(uint256 amount) external;
        function setRewardRecipient(address recipient) external;
        function claimRewards() external returns (uint256);

        // Reward info struct
        struct UserRewardInfo {
            address rewardRecipient;
            uint256 rewardPerToken;
            uint256 rewardBalance;
        }

        // Events
        event Transfer(address indexed from, address indexed to, uint256 amount);
        event Approval(address indexed owner, address indexed spender, uint256 amount);
        event Mint(address indexed to, uint256 amount);
        event Burn(address indexed from, uint256 amount);
        event BurnBlocked(address indexed from, uint256 amount);
        event PauseStateUpdate(address indexed updater, bool isPaused);
        event SupplyCapUpdate(address indexed updater, uint256 newSupplyCap);
        event TransferPolicyUpdate(address indexed updater, uint64 newPolicyId);
        event NextQuoteTokenSet(address indexed updater, address nextQuoteToken);
        event QuoteTokenUpdate(address indexed updater, address newQuoteToken);
        event TransferWithMemo(address indexed from, address indexed to, uint256 amount, bytes32 memo);
        event RewardDistributed(address indexed funder, uint256 amount);
        event RewardRecipientSet(address indexed holder, address recipient);

        // Errors
        error InsufficientBalance(uint256 balance, uint256 amount, address token);
        error InsufficientAllowance();
        error InvalidRecipient();
        error ContractPaused();
        error InvalidAmount();
        error PolicyForbids();
        error SupplyCapExceeded();
        error InvalidSupplyCap();
        error InvalidTransferPolicyId();
        error InvalidQuoteToken();
        error InvalidToken();
        error InvalidCurrency();
        error NoOptedInSupply();
        error ProtectedAddress();
        error PermitExpired();
        error InvalidSignature();
        error SpendingLimitExceeded();
        error Uninitialized();
    }

    // ---- IRolesAuth interface ----

    interface IRolesAuth {
        function hasRole(bytes32 role, address account) external view returns (bool);
        function getRoleAdmin(bytes32 role) external view returns (bytes32);
        function grantRole(bytes32 role, address account) external;
        function revokeRole(bytes32 role, address account) external;
        function renounceRole(bytes32 role) external;
        function setRoleAdmin(bytes32 role, bytes32 adminRole) external;

        event RoleMembershipUpdated(bytes32 indexed role, address indexed account, address sender, bool hasRole);
        event RoleAdminUpdated(bytes32 indexed role, bytes32 newAdminRole, address sender);

        error Unauthorized();
    }

    error UnknownFunctionSelector(bytes4 selector);
}

// ===========================================================================
// UserRewardInfo Storable type
// ===========================================================================

/// Per-user reward tracking state for the opt-in staking rewards system.
///
/// Storage layout (3 slots total):
///   - slot+0: reward_recipient (Address, 20 bytes at offset 0)
///   - slot+1: reward_per_token (U256, 32 bytes)
///   - slot+2: reward_balance (U256, 32 bytes)
#[derive(Debug, Clone)]
pub struct UserRewardInfo {
    pub reward_recipient: Address,
    pub reward_per_token: U256,
    pub reward_balance: U256,
}

impl Default for UserRewardInfo {
    fn default() -> Self {
        Self {
            reward_recipient: Address::ZERO,
            reward_per_token: U256::ZERO,
            reward_balance: U256::ZERO,
        }
    }
}

impl StorableType for UserRewardInfo {
    const LAYOUT: Layout = Layout::Slots(3);
    type Handler = Slot<Self>;

    fn handle(slot: U256, _ctx: LayoutCtx, address: Address) -> Self::Handler {
        Slot::new(slot, address)
    }
}

impl Storable for UserRewardInfo {
    fn load<S: StorageOps>(storage: &S, slot: U256, _ctx: LayoutCtx) -> Result<Self> {
        let word0 = storage.load(slot)?;
        let word1 = storage.load(slot + U256::from(1))?;
        let word2 = storage.load(slot + U256::from(2))?;

        Ok(Self {
            reward_recipient: <Address as FromWord>::from_word(word0)?,
            reward_per_token: word1,
            reward_balance: word2,
        })
    }

    fn store<S: StorageOps>(&self, storage: &mut S, slot: U256, _ctx: LayoutCtx) -> Result<()> {
        storage.store(slot, <Address as FromWord>::to_word(&self.reward_recipient))?;
        storage.store(slot + U256::from(1), self.reward_per_token)?;
        storage.store(slot + U256::from(2), self.reward_balance)?;
        Ok(())
    }
}

impl From<UserRewardInfo> for ITIP20::UserRewardInfo {
    fn from(value: UserRewardInfo) -> Self {
        Self {
            rewardRecipient: value.reward_recipient,
            rewardPerToken: value.reward_per_token,
            rewardBalance: value.reward_balance,
        }
    }
}

// ===========================================================================
// TIP20Token struct (manual macro expansion)
// ===========================================================================

/// TIP-20 token contract -- the native token standard on Tempo.
///
/// Each token lives at a deterministic address with the `0x20C0` prefix.
/// The storage fields are manually laid out to match the Tempo `#[contract]` macro
/// expansion exactly.
pub struct TIP20Token {
    // Slot 0: roles
    pub roles: Mapping<Address, Mapping<B256, bool>>,
    // Slot 1: role_admins
    pub role_admins: Mapping<B256, B256>,
    // Slot 2: name
    pub name: BytesLikeHandler<String>,
    // Slot 3: symbol
    pub symbol: BytesLikeHandler<String>,
    // Slot 4: currency
    pub currency: BytesLikeHandler<String>,
    // Slot 5: _domain_separator (unused, kept for layout compatibility)
    _domain_separator: Slot<B256>,
    // Slot 6: quote_token
    pub quote_token: Slot<Address>,
    // Slot 7 offset 0: next_quote_token
    pub next_quote_token: Slot<Address>,
    // Slot 7 offset 20: transfer_policy_id (packed with next_quote_token)
    pub transfer_policy_id: Slot<u64>,
    // Slot 8: total_supply
    pub total_supply: Slot<U256>,
    // Slot 9: balances
    pub balances: Mapping<Address, U256>,
    // Slot 10: allowances
    pub allowances: Mapping<Address, Mapping<Address, U256>>,
    // Slot 11: permit_nonces
    pub permit_nonces: Mapping<Address, U256>,
    // Slot 12: paused
    pub paused: Slot<bool>,
    // Slot 13: supply_cap
    pub supply_cap: Slot<U256>,
    // Slot 14: _salts (unused, kept for layout compatibility)
    _salts: Mapping<B256, bool>,
    // Slot 15: global_reward_per_token
    pub global_reward_per_token: Slot<U256>,
    // Slot 16: opted_in_supply
    pub opted_in_supply: Slot<u128>,
    // Slot 17: user_reward_info
    pub user_reward_info: Mapping<Address, UserRewardInfo>,

    pub address: Address,
    pub storage: StorageCtx,
}

impl TIP20Token {
    /// Creates a TIP20Token instance at the given address.
    ///
    /// Does not validate the address prefix -- use [`from_address`] for validated construction.
    fn __new(address: Address) -> Self {
        Self {
            roles: Mapping::new(U256::from(0), address),
            role_admins: Mapping::new(U256::from(1), address),
            name: BytesLikeHandler::new(U256::from(2), address),
            symbol: BytesLikeHandler::new(U256::from(3), address),
            currency: BytesLikeHandler::new(U256::from(4), address),
            _domain_separator: Slot::new(U256::from(5), address),
            quote_token: Slot::new(U256::from(6), address),
            next_quote_token: Slot::new(U256::from(7), address),
            transfer_policy_id: Slot::new_with_ctx(U256::from(7), LayoutCtx::packed(20), address),
            total_supply: Slot::new(U256::from(8), address),
            balances: Mapping::new(U256::from(9), address),
            allowances: Mapping::new(U256::from(10), address),
            permit_nonces: Mapping::new(U256::from(11), address),
            paused: Slot::new(U256::from(12), address),
            supply_cap: Slot::new(U256::from(13), address),
            _salts: Mapping::new(U256::from(14), address),
            global_reward_per_token: Slot::new(U256::from(15), address),
            opted_in_supply: Slot::new(U256::from(16), address),
            user_reward_info: Mapping::new(U256::from(17), address),
            address,
            storage: StorageCtx::default(),
        }
    }

    fn __initialize(&mut self) -> Result<()> {
        let bytecode = revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(self.address, bytecode)?;
        Ok(())
    }

    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage.emit_event(self.address, event.into_log_data())
    }
}

impl ContractStorage for TIP20Token {
    #[inline]
    fn address(&self) -> Address {
        self.address
    }

    #[inline]
    fn storage(&self) -> &StorageCtx {
        &self.storage
    }

    #[inline]
    fn storage_mut(&mut self) -> &mut StorageCtx {
        &mut self.storage
    }
}

// ===========================================================================
// Construction
// ===========================================================================

impl TIP20Token {
    /// Creates a `TIP20Token` handle from a raw address.
    ///
    /// Returns an error if the address does not carry the `0x20C0` TIP-20 prefix.
    pub fn from_address(address: Address) -> Result<Self> {
        if !is_tip20_prefix(address) {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidToken {}.abi_encode().into(),
            ));
        }
        Ok(Self::__new(address))
    }

    /// Creates a TIP20Token without validating the prefix.
    #[inline]
    pub fn from_address_unchecked(address: Address) -> Self {
        debug_assert!(is_tip20_prefix(address), "address must have TIP20 prefix");
        Self::__new(address)
    }
}

// ===========================================================================
// Metadata & view methods
// ===========================================================================

impl TIP20Token {
    /// Returns the token name.
    pub fn name(&self) -> Result<String> {
        self.name.read()
    }

    /// Returns the token symbol.
    pub fn symbol(&self) -> Result<String> {
        self.symbol.read()
    }

    /// Returns the token decimals (always 6 for TIP-20).
    pub fn decimals(&self) -> Result<u8> {
        Ok(TIP20_DECIMALS)
    }

    /// Returns the token's currency denomination (e.g. `"USD"`).
    pub fn currency(&self) -> Result<String> {
        self.currency.read()
    }

    /// Returns the current total supply.
    pub fn total_supply(&self) -> Result<U256> {
        self.total_supply.read()
    }

    /// Returns the active quote token address used for pricing.
    pub fn quote_token(&self) -> Result<Address> {
        self.quote_token.read()
    }

    /// Returns the pending next quote token address (set but not yet finalized).
    pub fn next_quote_token(&self) -> Result<Address> {
        self.next_quote_token.read()
    }

    /// Returns the maximum mintable supply.
    pub fn supply_cap(&self) -> Result<U256> {
        self.supply_cap.read()
    }

    /// Returns whether the token is currently paused.
    pub fn paused(&self) -> Result<bool> {
        self.paused.read()
    }

    /// Returns the TIP-403 transfer policy ID governing this token's transfers.
    pub fn transfer_policy_id(&self) -> Result<u64> {
        self.transfer_policy_id.read()
    }

    /// Returns the PAUSE_ROLE constant.
    pub fn pause_role() -> B256 {
        *PAUSE_ROLE
    }

    /// Returns the UNPAUSE_ROLE constant.
    pub fn unpause_role() -> B256 {
        *UNPAUSE_ROLE
    }

    /// Returns the ISSUER_ROLE constant.
    pub fn issuer_role() -> B256 {
        *ISSUER_ROLE
    }

    /// Returns the BURN_BLOCKED_ROLE constant.
    pub fn burn_blocked_role() -> B256 {
        *BURN_BLOCKED_ROLE
    }

    /// Returns the token balance of `account`.
    pub fn balance_of(&self, call: ITIP20::balanceOfCall) -> Result<U256> {
        self.balances[call.account].read()
    }

    /// Returns the remaining allowance that `spender` can transfer on behalf of `owner`.
    pub fn allowance(&self, call: ITIP20::allowanceCall) -> Result<U256> {
        self.allowances[call.owner][call.spender].read()
    }

    /// Returns the current nonce for an address (EIP-2612).
    pub fn nonces(&self, call: ITIP20::noncesCall) -> Result<U256> {
        self.permit_nonces[call.owner].read()
    }

    /// Returns the EIP-712 domain separator.
    pub fn domain_separator(&self) -> Result<B256> {
        static EIP712_DOMAIN_TYPEHASH: LazyLock<B256> = LazyLock::new(|| {
            keccak256(
                b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
            )
        });
        static VERSION_HASH: LazyLock<B256> = LazyLock::new(|| keccak256(b"1"));

        let name = self.name()?;
        let name_hash = self.storage.keccak256(name.as_bytes())?;
        let chain_id = U256::from(self.storage.chain_id());

        let encoded = (
            *EIP712_DOMAIN_TYPEHASH,
            name_hash,
            *VERSION_HASH,
            chain_id,
            self.address,
        )
            .abi_encode();

        self.storage.keccak256(&encoded)
    }
}

// ===========================================================================
// Internal balance/allowance helpers
// ===========================================================================

impl TIP20Token {
    fn get_balance(&self, account: Address) -> Result<U256> {
        self.balances[account].read()
    }

    fn set_balance(&mut self, account: Address, amount: U256) -> Result<()> {
        self.balances[account].write(amount)
    }

    fn get_allowance(&self, owner: Address, spender: Address) -> Result<U256> {
        self.allowances[owner][spender].read()
    }

    fn set_allowance(&mut self, owner: Address, spender: Address, amount: U256) -> Result<()> {
        self.allowances[owner][spender].write(amount)
    }

    fn set_total_supply(&mut self, amount: U256) -> Result<()> {
        self.total_supply.write(amount)
    }

    fn check_not_paused(&self) -> Result<()> {
        if self.paused()? {
            return Err(TempoPrecompileError::Revert(
                ITIP20::ContractPaused {}.abi_encode().into(),
            ));
        }
        Ok(())
    }

    /// Validates that the recipient is not the zero address or another TIP20 token.
    fn check_recipient(&self, to: Address) -> Result<()> {
        if to.is_zero() || is_tip20_prefix(to) {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidRecipient {}.abi_encode().into(),
            ));
        }
        Ok(())
    }
}

// ===========================================================================
// TIP-403 compliance
// ===========================================================================

impl TIP20Token {
    /// Check whether a transfer is authorized by the token's TIP-403 policy.
    ///
    /// Reads the token's `transfer_policy_id`, then checks sender and recipient
    /// authorization against TIP403Registry.
    pub fn is_transfer_authorized(&self, from: Address, to: Address) -> Result<bool> {
        let policy_id = self.transfer_policy_id.read()?;
        let registry = super::tip403_registry::TIP403Registry::new();

        // T2+ short-circuit: skip recipient check if sender fails.
        // Pre-T2: always evaluate both sender and recipient (matching writer).
        let sender_auth = registry.is_authorized_as(
            policy_id,
            from,
            super::tip403_registry::AuthRole::sender(),
        )?;
        if self.storage.spec().is_t2() && !sender_auth {
            return Ok(false);
        }
        let recipient_auth = registry.is_authorized_as(
            policy_id,
            to,
            super::tip403_registry::AuthRole::recipient(),
        )?;
        Ok(sender_auth && recipient_auth)
    }

    /// Ensures the transfer is authorized by the token's TIP-403 policy.
    pub fn ensure_transfer_authorized(&self, from: Address, to: Address) -> Result<()> {
        if !self.is_transfer_authorized(from, to)? {
            return Err(TempoPrecompileError::Revert(
                ITIP20::PolicyForbids {}.abi_encode().into(),
            ));
        }
        Ok(())
    }
}

// ===========================================================================
// Roles (access control)
// ===========================================================================

impl TIP20Token {
    /// Initializes the roles precompile by setting UNGRANTABLE_ROLE to be self-administered.
    pub fn initialize_roles(&mut self) -> Result<()> {
        self.set_role_admin_internal(UNGRANTABLE_ROLE, UNGRANTABLE_ROLE)
    }

    /// Grants `DEFAULT_ADMIN_ROLE` to `admin`. Used during token initialization.
    pub fn grant_default_admin(&mut self, msg_sender: Address, admin: Address) -> Result<()> {
        self.grant_role_internal(admin, DEFAULT_ADMIN_ROLE)?;

        self.emit_event(IRolesAuth::RoleMembershipUpdated {
            role: DEFAULT_ADMIN_ROLE,
            account: admin,
            sender: msg_sender,
            hasRole: true,
        })
    }

    /// Returns whether `account` holds the given `role`.
    pub fn has_role(&self, call: IRolesAuth::hasRoleCall) -> Result<bool> {
        self.has_role_internal(call.account, call.role)
    }

    /// Returns the admin role that governs `role`.
    pub fn get_role_admin(&self, call: IRolesAuth::getRoleAdminCall) -> Result<B256> {
        self.get_role_admin_internal(call.role)
    }

    /// Grants `role` to `account`.
    pub fn grant_role(
        &mut self,
        msg_sender: Address,
        call: IRolesAuth::grantRoleCall,
    ) -> Result<()> {
        let admin_role = self.get_role_admin_internal(call.role)?;
        self.check_role_internal(msg_sender, admin_role)?;
        self.grant_role_internal(call.account, call.role)?;

        self.emit_event(IRolesAuth::RoleMembershipUpdated {
            role: call.role,
            account: call.account,
            sender: msg_sender,
            hasRole: true,
        })
    }

    /// Revokes `role` from `account`.
    pub fn revoke_role(
        &mut self,
        msg_sender: Address,
        call: IRolesAuth::revokeRoleCall,
    ) -> Result<()> {
        let admin_role = self.get_role_admin_internal(call.role)?;
        self.check_role_internal(msg_sender, admin_role)?;
        self.revoke_role_internal(call.account, call.role)?;

        self.emit_event(IRolesAuth::RoleMembershipUpdated {
            role: call.role,
            account: call.account,
            sender: msg_sender,
            hasRole: false,
        })
    }

    /// Allows the caller to voluntarily give up their own `role`.
    pub fn renounce_role(
        &mut self,
        msg_sender: Address,
        call: IRolesAuth::renounceRoleCall,
    ) -> Result<()> {
        self.check_role_internal(msg_sender, call.role)?;
        self.revoke_role_internal(msg_sender, call.role)?;

        self.emit_event(IRolesAuth::RoleMembershipUpdated {
            role: call.role,
            account: msg_sender,
            sender: msg_sender,
            hasRole: false,
        })
    }

    /// Changes the admin role that governs `role`.
    pub fn set_role_admin(
        &mut self,
        msg_sender: Address,
        call: IRolesAuth::setRoleAdminCall,
    ) -> Result<()> {
        let current_admin_role = self.get_role_admin_internal(call.role)?;
        self.check_role_internal(msg_sender, current_admin_role)?;

        self.set_role_admin_internal(call.role, call.adminRole)?;

        self.emit_event(IRolesAuth::RoleAdminUpdated {
            role: call.role,
            newAdminRole: call.adminRole,
            sender: msg_sender,
        })
    }

    /// Reverts if `account` does not hold `role`.
    pub fn check_role(&self, account: Address, role: B256) -> Result<()> {
        self.check_role_internal(account, role)
    }

    fn has_role_internal(&self, account: Address, role: B256) -> Result<bool> {
        self.roles[account][role].read()
    }

    fn grant_role_internal(&mut self, account: Address, role: B256) -> Result<()> {
        self.roles[account][role].write(true)
    }

    fn revoke_role_internal(&mut self, account: Address, role: B256) -> Result<()> {
        self.roles[account][role].write(false)
    }

    fn get_role_admin_internal(&self, role: B256) -> Result<B256> {
        self.role_admins[role].read()
    }

    fn set_role_admin_internal(&mut self, role: B256, admin_role: B256) -> Result<()> {
        self.role_admins[role].write(admin_role)
    }

    fn check_role_internal(&self, account: Address, role: B256) -> Result<()> {
        if !self.has_role_internal(account, role)? {
            return Err(TempoPrecompileError::Revert(
                IRolesAuth::Unauthorized {}.abi_encode().into(),
            ));
        }
        Ok(())
    }
}

// ===========================================================================
// Token operations (state-changing)
// ===========================================================================

impl TIP20Token {
    /// Initializes the TIP-20 token precompile with metadata, quote token, supply cap, and
    /// default admin role.
    pub fn initialize(
        &mut self,
        msg_sender: Address,
        name: &str,
        symbol: &str,
        currency: &str,
        quote_token: Address,
        admin: Address,
    ) -> Result<()> {
        self.__initialize()?;

        self.name.write(name.to_string())?;
        self.symbol.write(symbol.to_string())?;
        self.currency.write(currency.to_string())?;

        self.quote_token.write(quote_token)?;
        self.next_quote_token.write(quote_token)?;

        self.supply_cap.write(U256::from(u128::MAX))?;
        self.transfer_policy_id.write(1)?;

        self.initialize_roles()?;
        self.grant_default_admin(msg_sender, admin)
    }

    /// Sets a new supply cap. Must be >= current total supply and <= U128_MAX.
    pub fn set_supply_cap(
        &mut self,
        msg_sender: Address,
        call: ITIP20::setSupplyCapCall,
    ) -> Result<()> {
        self.check_role(msg_sender, DEFAULT_ADMIN_ROLE)?;
        if call.newSupplyCap < self.total_supply()? {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidSupplyCap {}.abi_encode().into(),
            ));
        }
        if call.newSupplyCap > U128_MAX {
            return Err(TempoPrecompileError::Revert(
                ITIP20::SupplyCapExceeded {}.abi_encode().into(),
            ));
        }

        self.supply_cap.write(call.newSupplyCap)?;

        self.emit_event(ITIP20::SupplyCapUpdate {
            updater: msg_sender,
            newSupplyCap: call.newSupplyCap,
        })
    }

    /// Pauses all token transfers.
    pub fn pause(&mut self, msg_sender: Address, _call: ITIP20::pauseCall) -> Result<()> {
        self.check_role(msg_sender, *PAUSE_ROLE)?;
        self.paused.write(true)?;

        self.emit_event(ITIP20::PauseStateUpdate {
            updater: msg_sender,
            isPaused: true,
        })
    }

    /// Unpauses token transfers.
    pub fn unpause(&mut self, msg_sender: Address, _call: ITIP20::unpauseCall) -> Result<()> {
        self.check_role(msg_sender, *UNPAUSE_ROLE)?;
        self.paused.write(false)?;

        self.emit_event(ITIP20::PauseStateUpdate {
            updater: msg_sender,
            isPaused: false,
        })
    }

    /// Updates the TIP-403 transfer policy governing this token's transfers.
    pub fn change_transfer_policy_id(
        &mut self,
        msg_sender: Address,
        call: ITIP20::changeTransferPolicyIdCall,
    ) -> Result<()> {
        self.check_role(msg_sender, DEFAULT_ADMIN_ROLE)?;

        // Validate that the policy exists in TIP403Registry
        if !super::tip403_registry::TIP403Registry::new().policy_exists(
            super::tip403_registry::ITIP403Registry::policyExistsCall {
                policyId: call.newPolicyId,
            },
        )? {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidTransferPolicyId {}.abi_encode().into(),
            ));
        }
        self.transfer_policy_id.write(call.newPolicyId)?;

        self.emit_event(ITIP20::TransferPolicyUpdate {
            updater: msg_sender,
            newPolicyId: call.newPolicyId,
        })
    }

    /// Stages a new quote token.
    pub fn set_next_quote_token(
        &mut self,
        msg_sender: Address,
        call: ITIP20::setNextQuoteTokenCall,
    ) -> Result<()> {
        self.check_role(msg_sender, DEFAULT_ADMIN_ROLE)?;

        if self.address == super::PATH_USD_ADDRESS {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidQuoteToken {}.abi_encode().into(),
            ));
        }

        // Verify the new quote token is a valid deployed TIP20 via factory
        if !super::tip20_factory::TIP20Factory::new().is_tip20(call.newQuoteToken)? {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidQuoteToken {}.abi_encode().into(),
            ));
        }

        // If currency is USD, the quote token's currency must also be USD
        let currency = self.currency()?;
        if currency == "USD" {
            let quote_token_currency = Self::from_address(call.newQuoteToken)?.currency()?;
            if quote_token_currency != "USD" {
                return Err(TempoPrecompileError::Revert(
                    ITIP20::InvalidQuoteToken {}.abi_encode().into(),
                ));
            }
        }

        self.next_quote_token.write(call.newQuoteToken)?;

        self.emit_event(ITIP20::NextQuoteTokenSet {
            updater: msg_sender,
            nextQuoteToken: call.newQuoteToken,
        })
    }

    /// Finalizes the staged quote token update.
    pub fn complete_quote_token_update(
        &mut self,
        msg_sender: Address,
        _call: ITIP20::completeQuoteTokenUpdateCall,
    ) -> Result<()> {
        self.check_role(msg_sender, DEFAULT_ADMIN_ROLE)?;

        let next_quote_token = self.next_quote_token()?;

        // Cycle detection: walk the quote-token chain from next_quote_token back
        // to pathUSD. If we encounter self.address, the update would create a cycle.
        let mut current = next_quote_token;
        while current != super::PATH_USD_ADDRESS {
            if current == self.address {
                return Err(TempoPrecompileError::Revert(
                    ITIP20::InvalidQuoteToken {}.abi_encode().into(),
                ));
            }
            current = Self::from_address(current)?.quote_token()?;
        }

        self.quote_token.write(next_quote_token)?;

        self.emit_event(ITIP20::QuoteTokenUpdate {
            updater: msg_sender,
            newQuoteToken: next_quote_token,
        })
    }

    /// Sets `spender`'s allowance to `amount` for the caller's tokens.
    pub fn approve(&mut self, msg_sender: Address, call: ITIP20::approveCall) -> Result<bool> {
        // AccountKeychain spending limit check for approve
        let old_allowance = self.get_allowance(msg_sender, call.spender)?;
        super::account_keychain::AccountKeychain::new().authorize_approve(
            msg_sender,
            self.address,
            old_allowance,
            call.amount,
        )?;
        self.set_allowance(msg_sender, call.spender, call.amount)?;

        self.emit_event(ITIP20::Approval {
            owner: msg_sender,
            spender: call.spender,
            amount: call.amount,
        })?;

        Ok(true)
    }

    /// EIP-2612 permit.
    pub fn permit(&mut self, call: ITIP20::permitCall) -> Result<()> {
        static PERMIT_TYPEHASH: LazyLock<B256> = LazyLock::new(|| {
            keccak256(
                b"Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)",
            )
        });

        // 1. Check deadline
        if self.storage.timestamp() > call.deadline {
            return Err(TempoPrecompileError::Revert(
                ITIP20::PermitExpired {}.abi_encode().into(),
            ));
        }

        // 2. Construct EIP-712 struct hash
        let nonce = self.permit_nonces[call.owner].read()?;
        let struct_hash = self.storage.keccak256(
            &(
                *PERMIT_TYPEHASH,
                call.owner,
                call.spender,
                call.value,
                nonce,
                call.deadline,
            )
                .abi_encode(),
        )?;

        // 3. Construct EIP-712 digest
        let domain_separator = self.domain_separator()?;
        let digest = self.storage.keccak256(
            &[
                &[0x19, 0x01],
                domain_separator.as_slice(),
                struct_hash.as_slice(),
            ]
            .concat(),
        )?;

        // 4. Validate ECDSA signature
        // Only v=27/28 is accepted; v=0/1 is intentionally NOT normalized (see TIP-1004 spec).
        let recovered = self
            .storage
            .recover_signer(digest, call.v, call.r, call.s)?
            .ok_or_else(|| {
                TempoPrecompileError::Revert(ITIP20::InvalidSignature {}.abi_encode().into())
            })?;
        if recovered != call.owner {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidSignature {}.abi_encode().into(),
            ));
        }

        // 5. Increment nonce
        self.permit_nonces[call.owner].write(
            nonce
                .checked_add(U256::from(1))
                .ok_or(TempoPrecompileError::under_overflow())?,
        )?;

        // 6. Set allowance
        self.set_allowance(call.owner, call.spender, call.value)?;

        // 7. Emit Approval event
        self.emit_event(ITIP20::Approval {
            owner: call.owner,
            spender: call.spender,
            amount: call.value,
        })
    }

    /// Transfers `amount` tokens from the caller to `to`.
    pub fn transfer(&mut self, msg_sender: Address, call: ITIP20::transferCall) -> Result<bool> {
        self.check_not_paused()?;
        self.check_recipient(call.to)?;
        self.ensure_transfer_authorized(msg_sender, call.to)?;

        // AccountKeychain spending limit check for transfer
        super::account_keychain::AccountKeychain::new().authorize_transfer(
            msg_sender,
            self.address,
            call.amount,
        )?;
        self._transfer(msg_sender, call.to, call.amount)?;
        Ok(true)
    }

    /// Transfers `amount` on behalf of `from` using the caller's allowance.
    pub fn transfer_from(
        &mut self,
        msg_sender: Address,
        call: ITIP20::transferFromCall,
    ) -> Result<bool> {
        self._transfer_from(msg_sender, call.from, call.to, call.amount)
    }

    /// Like `transfer_from`, but attaches a 32-byte memo.
    pub fn transfer_from_with_memo(
        &mut self,
        msg_sender: Address,
        call: ITIP20::transferFromWithMemoCall,
    ) -> Result<bool> {
        self._transfer_from(msg_sender, call.from, call.to, call.amount)?;

        self.emit_event(ITIP20::TransferWithMemo {
            from: call.from,
            to: call.to,
            amount: call.amount,
            memo: call.memo,
        })?;

        Ok(true)
    }

    fn _transfer_from(
        &mut self,
        msg_sender: Address,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<bool> {
        self.check_not_paused()?;
        self.check_recipient(to)?;
        self.ensure_transfer_authorized(from, to)?;

        let allowed = self.get_allowance(from, msg_sender)?;
        if amount > allowed {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InsufficientAllowance {}.abi_encode().into(),
            ));
        }

        if allowed != U256::MAX {
            let new_allowance = allowed.checked_sub(amount).ok_or_else(|| {
                TempoPrecompileError::Revert(ITIP20::InsufficientAllowance {}.abi_encode().into())
            })?;
            self.set_allowance(from, msg_sender, new_allowance)?;
        }

        self._transfer(from, to, amount)?;
        Ok(true)
    }

    /// Like `transfer`, but attaches a 32-byte memo.
    pub fn transfer_with_memo(
        &mut self,
        msg_sender: Address,
        call: ITIP20::transferWithMemoCall,
    ) -> Result<()> {
        self.check_not_paused()?;
        self.check_recipient(call.to)?;
        self.ensure_transfer_authorized(msg_sender, call.to)?;

        // AccountKeychain spending limit check for transferWithMemo
        super::account_keychain::AccountKeychain::new().authorize_transfer(
            msg_sender,
            self.address,
            call.amount,
        )?;
        self._transfer(msg_sender, call.to, call.amount)?;

        self.emit_event(ITIP20::TransferWithMemo {
            from: msg_sender,
            to: call.to,
            amount: call.amount,
            memo: call.memo,
        })
    }

    /// Transfers `amount` from `from` to `to` without approval, for use by other
    /// precompiles only (not exposed via ABI). Enforces compliance via TIP-403
    /// and AccountKeychain spending limits.
    pub fn system_transfer_from(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<bool> {
        self.check_not_paused()?;
        self.check_recipient(to)?;
        self.ensure_transfer_authorized(from, to)?;
        // AccountKeychain spending limit
        super::account_keychain::AccountKeychain::new().authorize_transfer(
            from,
            self.address,
            amount,
        )?;
        self._transfer(from, to, amount)?;
        Ok(true)
    }

    /// Transfers fee tokens from `from` to the fee manager before transaction execution.
    /// Respects the token's pause state and deducts from the AccountKeychain spending limit.
    pub fn transfer_fee_pre_tx(&mut self, from: Address, amount: U256) -> Result<()> {
        // This function respects the token's pause state and will revert if the token is paused.
        // transfer_fee_post_tx is intentionally allowed even when paused so that a pause
        // transaction can still receive its fee refund.
        self.check_not_paused()?;
        let from_balance = self.get_balance(from)?;
        if amount > from_balance {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InsufficientBalance {
                    balance: from_balance,
                    amount,
                    token: self.address,
                }
                .abi_encode()
                .into(),
            ));
        }

        // AccountKeychain spending limit
        super::account_keychain::AccountKeychain::new().authorize_transfer(
            from,
            self.address,
            amount,
        )?;

        self.handle_rewards_on_transfer(from, TIP_FEE_MANAGER_ADDRESS, amount)?;

        let new_from_balance = from_balance.checked_sub(amount).ok_or_else(|| {
            TempoPrecompileError::Revert(
                ITIP20::InsufficientBalance {
                    balance: from_balance,
                    amount,
                    token: self.address,
                }
                .abi_encode()
                .into(),
            )
        })?;
        self.set_balance(from, new_from_balance)?;

        let to_balance = self.get_balance(TIP_FEE_MANAGER_ADDRESS)?;
        let new_to_balance = to_balance.checked_add(amount).ok_or_else(|| {
            TempoPrecompileError::Revert(ITIP20::SupplyCapExceeded {}.abi_encode().into())
        })?;
        self.set_balance(TIP_FEE_MANAGER_ADDRESS, new_to_balance)
    }

    /// Refunds unused fee tokens from the fee manager back to `to` and emits a transfer
    /// event for actual gas spent. Intentionally allowed when paused so that a pause
    /// transaction can still receive its fee refund.
    pub fn transfer_fee_post_tx(
        &mut self,
        to: Address,
        refund: U256,
        actual_spending: U256,
    ) -> Result<()> {
        self.emit_event(ITIP20::Transfer {
            from: to,
            to: TIP_FEE_MANAGER_ADDRESS,
            amount: actual_spending,
        })?;

        // Exit early if there is no refund
        if refund.is_zero() {
            return Ok(());
        }

        // Refund spending limit (T1C+, matching writer tip20/mod.rs:1046)
        if self.storage.spec().is_t1c() {
            super::account_keychain::AccountKeychain::new().refund_spending_limit(
                to,
                self.address,
                refund,
            )?;
        }

        self.handle_rewards_on_transfer(TIP_FEE_MANAGER_ADDRESS, to, refund)?;

        let from_balance = self.get_balance(TIP_FEE_MANAGER_ADDRESS)?;
        let new_from_balance = from_balance.checked_sub(refund).ok_or_else(|| {
            TempoPrecompileError::Revert(
                ITIP20::InsufficientBalance {
                    balance: from_balance,
                    amount: refund,
                    token: self.address,
                }
                .abi_encode()
                .into(),
            )
        })?;
        self.set_balance(TIP_FEE_MANAGER_ADDRESS, new_from_balance)?;

        let to_balance = self.get_balance(to)?;
        let new_to_balance = to_balance.checked_add(refund).ok_or_else(|| {
            TempoPrecompileError::Revert(ITIP20::SupplyCapExceeded {}.abi_encode().into())
        })?;
        self.set_balance(to, new_to_balance)?;

        self.emit_event(ITIP20::Transfer {
            from: TIP_FEE_MANAGER_ADDRESS,
            to,
            amount: refund,
        })
    }

    /// Core internal transfer. Adjusts balances and emits Transfer event.
    fn _transfer(&mut self, from: Address, to: Address, amount: U256) -> Result<()> {
        let from_balance = self.get_balance(from)?;
        if amount > from_balance {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InsufficientBalance {
                    balance: from_balance,
                    amount,
                    token: self.address,
                }
                .abi_encode()
                .into(),
            ));
        }

        self.handle_rewards_on_transfer(from, to, amount)?;

        let new_from_balance = from_balance
            .checked_sub(amount)
            .ok_or_else(|| TempoPrecompileError::Fatal("underflow in _transfer".to_string()))?;
        self.set_balance(from, new_from_balance)?;

        if to != Address::ZERO {
            let to_balance = self.get_balance(to)?;
            let new_to_balance = to_balance
                .checked_add(amount)
                .ok_or_else(|| TempoPrecompileError::Fatal("overflow in _transfer".to_string()))?;
            self.set_balance(to, new_to_balance)?;
        }

        self.emit_event(ITIP20::Transfer { from, to, amount })
    }

    /// Mints `amount` tokens to the specified `to` address.
    pub fn mint(&mut self, msg_sender: Address, call: ITIP20::mintCall) -> Result<()> {
        self._mint(msg_sender, call.to, call.amount)?;
        self.emit_event(ITIP20::Mint {
            to: call.to,
            amount: call.amount,
        })
    }

    /// Like `mint`, but attaches a 32-byte memo.
    pub fn mint_with_memo(
        &mut self,
        msg_sender: Address,
        call: ITIP20::mintWithMemoCall,
    ) -> Result<()> {
        self._mint(msg_sender, call.to, call.amount)?;

        self.emit_event(ITIP20::TransferWithMemo {
            from: Address::ZERO,
            to: call.to,
            amount: call.amount,
            memo: call.memo,
        })?;
        self.emit_event(ITIP20::Mint {
            to: call.to,
            amount: call.amount,
        })
    }

    fn _mint(&mut self, msg_sender: Address, to: Address, amount: U256) -> Result<()> {
        self.check_role(msg_sender, *ISSUER_ROLE)?;
        let total_supply = self.total_supply()?;

        // TIP403Registry mint recipient authorization check
        let policy_id = self.transfer_policy_id.read()?;
        if !super::tip403_registry::TIP403Registry::new().is_authorized_as(
            policy_id,
            to,
            super::tip403_registry::AuthRole::mint_recipient(),
        )? {
            return Err(TempoPrecompileError::Revert(
                ITIP20::PolicyForbids {}.abi_encode().into(),
            ));
        }
        let new_supply = total_supply
            .checked_add(amount)
            .ok_or_else(|| TempoPrecompileError::Fatal("overflow in _mint".to_string()))?;

        let supply_cap = self.supply_cap()?;
        if new_supply > supply_cap {
            return Err(TempoPrecompileError::Revert(
                ITIP20::SupplyCapExceeded {}.abi_encode().into(),
            ));
        }

        self.handle_rewards_on_mint(to, amount)?;

        self.set_total_supply(new_supply)?;
        let to_balance = self.get_balance(to)?;
        let new_to_balance = to_balance
            .checked_add(amount)
            .ok_or_else(|| TempoPrecompileError::Fatal("overflow in _mint".to_string()))?;
        self.set_balance(to, new_to_balance)?;

        self.emit_event(ITIP20::Transfer {
            from: Address::ZERO,
            to,
            amount,
        })
    }

    /// Burns `amount` from the caller's balance and reduces total supply.
    pub fn burn(&mut self, msg_sender: Address, call: ITIP20::burnCall) -> Result<()> {
        self._burn(msg_sender, call.amount)?;
        self.emit_event(ITIP20::Burn {
            from: msg_sender,
            amount: call.amount,
        })
    }

    /// Like `burn`, but attaches a 32-byte memo.
    pub fn burn_with_memo(
        &mut self,
        msg_sender: Address,
        call: ITIP20::burnWithMemoCall,
    ) -> Result<()> {
        self._burn(msg_sender, call.amount)?;

        self.emit_event(ITIP20::TransferWithMemo {
            from: msg_sender,
            to: Address::ZERO,
            amount: call.amount,
            memo: call.memo,
        })?;
        self.emit_event(ITIP20::Burn {
            from: msg_sender,
            amount: call.amount,
        })
    }

    /// Burns tokens from addresses blocked by TIP-403 policy.
    pub fn burn_blocked(
        &mut self,
        msg_sender: Address,
        call: ITIP20::burnBlockedCall,
    ) -> Result<()> {
        self.check_role(msg_sender, *BURN_BLOCKED_ROLE)?;

        if call.from == TIP_FEE_MANAGER_ADDRESS || call.from == STABLECOIN_DEX_ADDRESS {
            return Err(TempoPrecompileError::Revert(
                ITIP20::ProtectedAddress {}.abi_encode().into(),
            ));
        }

        // TIP403Registry: verify sender is NOT authorized (burn_blocked targets blacklisted accounts)
        let policy_id = self.transfer_policy_id.read()?;
        if super::tip403_registry::TIP403Registry::new().is_authorized_as(
            policy_id,
            call.from,
            super::tip403_registry::AuthRole::sender(),
        )? {
            // burn_blocked only works on accounts that are NOT authorized (i.e., blocked)
            return Err(TempoPrecompileError::Revert(
                ITIP20::PolicyForbids {}.abi_encode().into(),
            ));
        }
        self._transfer(call.from, Address::ZERO, call.amount)?;

        let total_supply = self.total_supply()?;
        let new_supply = total_supply.checked_sub(call.amount).ok_or_else(|| {
            TempoPrecompileError::Revert(
                ITIP20::InsufficientBalance {
                    balance: total_supply,
                    amount: call.amount,
                    token: self.address,
                }
                .abi_encode()
                .into(),
            )
        })?;
        self.set_total_supply(new_supply)?;

        self.emit_event(ITIP20::BurnBlocked {
            from: call.from,
            amount: call.amount,
        })
    }

    fn _burn(&mut self, msg_sender: Address, amount: U256) -> Result<()> {
        self.check_role(msg_sender, *ISSUER_ROLE)?;

        self._transfer(msg_sender, Address::ZERO, amount)?;

        let total_supply = self.total_supply()?;
        let new_supply = total_supply.checked_sub(amount).ok_or_else(|| {
            TempoPrecompileError::Revert(
                ITIP20::InsufficientBalance {
                    balance: total_supply,
                    amount,
                    token: self.address,
                }
                .abi_encode()
                .into(),
            )
        })?;
        self.set_total_supply(new_supply)
    }
}

// ===========================================================================
// Rewards
// ===========================================================================

impl TIP20Token {
    /// Distributes `amount` of reward tokens from the caller into the opted-in reward pool.
    pub fn distribute_reward(
        &mut self,
        msg_sender: Address,
        call: ITIP20::distributeRewardCall,
    ) -> Result<()> {
        self.check_not_paused()?;
        let token_address = self.address;

        if call.amount == U256::ZERO {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidAmount {}.abi_encode().into(),
            ));
        }

        self.ensure_transfer_authorized(msg_sender, token_address)?;
        // AccountKeychain spending limit check for distributeReward
        super::account_keychain::AccountKeychain::new().authorize_transfer(
            msg_sender,
            self.address,
            call.amount,
        )?;
        self._transfer(msg_sender, token_address, call.amount)?;

        let opted_in_supply = U256::from(self.get_opted_in_supply()?);
        if opted_in_supply.is_zero() {
            return Err(TempoPrecompileError::Revert(
                ITIP20::NoOptedInSupply {}.abi_encode().into(),
            ));
        }

        let delta_rpt = call
            .amount
            .checked_mul(ACC_PRECISION)
            .and_then(|v| v.checked_div(opted_in_supply))
            .ok_or_else(|| {
                TempoPrecompileError::Fatal("overflow in distribute_reward".to_string())
            })?;
        let current_rpt = self.get_global_reward_per_token()?;
        let new_rpt = current_rpt.checked_add(delta_rpt).ok_or_else(|| {
            TempoPrecompileError::Fatal("overflow in distribute_reward".to_string())
        })?;
        self.set_global_reward_per_token(new_rpt)?;

        self.emit_event(ITIP20::RewardDistributed {
            funder: msg_sender,
            amount: call.amount,
        })
    }

    /// Updates and accumulates accrued rewards for a specific token holder.
    pub fn update_rewards(&mut self, holder: Address) -> Result<Address> {
        let mut info = self.user_reward_info[holder].read()?;
        let cached_delegate = info.reward_recipient;

        let global_reward_per_token = self.get_global_reward_per_token()?;
        let reward_per_token_delta = global_reward_per_token
            .checked_sub(info.reward_per_token)
            .ok_or_else(|| {
                TempoPrecompileError::Fatal("underflow in update_rewards".to_string())
            })?;

        if reward_per_token_delta != U256::ZERO {
            if cached_delegate != Address::ZERO {
                let holder_balance = self.get_balance(holder)?;
                let reward = holder_balance
                    .checked_mul(reward_per_token_delta)
                    .and_then(|v| v.checked_div(ACC_PRECISION))
                    .ok_or_else(|| {
                        TempoPrecompileError::Fatal("overflow in update_rewards".to_string())
                    })?;

                if cached_delegate == holder {
                    info.reward_balance =
                        info.reward_balance.checked_add(reward).ok_or_else(|| {
                            TempoPrecompileError::Fatal("overflow in update_rewards".to_string())
                        })?;
                } else {
                    let mut delegate_info = self.user_reward_info[cached_delegate].read()?;
                    delegate_info.reward_balance = delegate_info
                        .reward_balance
                        .checked_add(reward)
                        .ok_or_else(|| {
                            TempoPrecompileError::Fatal("overflow in update_rewards".to_string())
                        })?;
                    self.user_reward_info[cached_delegate].write(delegate_info)?;
                }
            }
            info.reward_per_token = global_reward_per_token;
            self.user_reward_info[holder].write(info)?;
        }

        Ok(cached_delegate)
    }

    /// Sets or changes the reward recipient for a token holder.
    pub fn set_reward_recipient(
        &mut self,
        msg_sender: Address,
        call: ITIP20::setRewardRecipientCall,
    ) -> Result<()> {
        self.check_not_paused()?;
        if call.recipient != Address::ZERO {
            self.ensure_transfer_authorized(msg_sender, call.recipient)?;
        }

        let from_delegate = self.update_rewards(msg_sender)?;
        let holder_balance = self.get_balance(msg_sender)?;

        if from_delegate != Address::ZERO {
            if call.recipient == Address::ZERO {
                let opted_in_supply = U256::from(self.get_opted_in_supply()?)
                    .checked_sub(holder_balance)
                    .ok_or_else(|| {
                        TempoPrecompileError::Fatal("underflow in set_reward_recipient".to_string())
                    })?;
                self.set_opted_in_supply(opted_in_supply.try_into().map_err(|_| {
                    TempoPrecompileError::Fatal("overflow in set_reward_recipient".to_string())
                })?)?;
            }
        } else if call.recipient != Address::ZERO {
            let opted_in_supply = U256::from(self.get_opted_in_supply()?)
                .checked_add(holder_balance)
                .ok_or_else(|| {
                    TempoPrecompileError::Fatal("overflow in set_reward_recipient".to_string())
                })?;
            self.set_opted_in_supply(opted_in_supply.try_into().map_err(|_| {
                TempoPrecompileError::Fatal("overflow in set_reward_recipient".to_string())
            })?)?;
        }

        let mut info = self.user_reward_info[msg_sender].read()?;
        info.reward_recipient = call.recipient;
        self.user_reward_info[msg_sender].write(info)?;

        self.emit_event(ITIP20::RewardRecipientSet {
            holder: msg_sender,
            recipient: call.recipient,
        })
    }

    /// Claims accumulated rewards for a recipient.
    pub fn claim_rewards(&mut self, msg_sender: Address) -> Result<U256> {
        self.check_not_paused()?;
        self.ensure_transfer_authorized(self.address, msg_sender)?;

        self.update_rewards(msg_sender)?;

        let mut info = self.user_reward_info[msg_sender].read()?;
        let amount = info.reward_balance;
        let contract_address = self.address;
        let contract_balance = self.get_balance(contract_address)?;
        let max_amount = amount.min(contract_balance);

        let reward_recipient = info.reward_recipient;
        info.reward_balance = amount
            .checked_sub(max_amount)
            .ok_or_else(|| TempoPrecompileError::Fatal("underflow in claim_rewards".to_string()))?;
        self.user_reward_info[msg_sender].write(info)?;

        if max_amount > U256::ZERO {
            let new_contract_balance =
                contract_balance.checked_sub(max_amount).ok_or_else(|| {
                    TempoPrecompileError::Fatal("underflow in claim_rewards".to_string())
                })?;
            self.set_balance(contract_address, new_contract_balance)?;

            let recipient_balance = self
                .get_balance(msg_sender)?
                .checked_add(max_amount)
                .ok_or_else(|| {
                    TempoPrecompileError::Fatal("overflow in claim_rewards".to_string())
                })?;
            self.set_balance(msg_sender, recipient_balance)?;

            if reward_recipient != Address::ZERO {
                let opted_in_supply = U256::from(self.get_opted_in_supply()?)
                    .checked_add(max_amount)
                    .ok_or_else(|| {
                        TempoPrecompileError::Fatal("overflow in claim_rewards".to_string())
                    })?;
                self.set_opted_in_supply(opted_in_supply.try_into().map_err(|_| {
                    TempoPrecompileError::Fatal("overflow in claim_rewards".to_string())
                })?)?;
            }

            self.emit_event(ITIP20::Transfer {
                from: contract_address,
                to: msg_sender,
                amount: max_amount,
            })?;
        }

        Ok(max_amount)
    }

    /// Gets the accumulated global reward per token.
    pub fn get_global_reward_per_token(&self) -> Result<U256> {
        self.global_reward_per_token.read()
    }

    fn set_global_reward_per_token(&mut self, value: U256) -> Result<()> {
        self.global_reward_per_token.write(value)
    }

    /// Gets the total supply of tokens opted into rewards.
    pub fn get_opted_in_supply(&self) -> Result<u128> {
        self.opted_in_supply.read()
    }

    pub fn set_opted_in_supply(&mut self, value: u128) -> Result<()> {
        self.opted_in_supply.write(value)
    }

    /// Handles reward accounting for both sender and receiver during token transfers.
    pub fn handle_rewards_on_transfer(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<()> {
        let from_delegate = self.update_rewards(from)?;
        let to_delegate = self.update_rewards(to)?;

        if !from_delegate.is_zero() {
            if to_delegate.is_zero() {
                let opted_in_supply = U256::from(self.get_opted_in_supply()?)
                    .checked_sub(amount)
                    .ok_or_else(|| {
                        TempoPrecompileError::Fatal(
                            "underflow in handle_rewards_on_transfer".to_string(),
                        )
                    })?;
                self.set_opted_in_supply(opted_in_supply.try_into().map_err(|_| {
                    TempoPrecompileError::Fatal(
                        "overflow in handle_rewards_on_transfer".to_string(),
                    )
                })?)?;
            }
        } else if !to_delegate.is_zero() {
            let opted_in_supply = U256::from(self.get_opted_in_supply()?)
                .checked_add(amount)
                .ok_or_else(|| {
                    TempoPrecompileError::Fatal(
                        "overflow in handle_rewards_on_transfer".to_string(),
                    )
                })?;
            self.set_opted_in_supply(opted_in_supply.try_into().map_err(|_| {
                TempoPrecompileError::Fatal("overflow in handle_rewards_on_transfer".to_string())
            })?)?;
        }

        Ok(())
    }

    /// Handles reward accounting when tokens are minted to an address.
    pub fn handle_rewards_on_mint(&mut self, to: Address, amount: U256) -> Result<()> {
        let to_delegate = self.update_rewards(to)?;

        if !to_delegate.is_zero() {
            let opted_in_supply = U256::from(self.get_opted_in_supply()?)
                .checked_add(amount)
                .ok_or_else(|| {
                    TempoPrecompileError::Fatal("overflow in handle_rewards_on_mint".to_string())
                })?;
            self.set_opted_in_supply(opted_in_supply.try_into().map_err(|_| {
                TempoPrecompileError::Fatal("overflow in handle_rewards_on_mint".to_string())
            })?)?;
        }

        Ok(())
    }

    /// Retrieves user reward information for a given account.
    pub fn get_user_reward_info(&self, account: Address) -> Result<UserRewardInfo> {
        self.user_reward_info[account].read()
    }

    /// Calculates the pending claimable rewards for an account without modifying state.
    pub fn get_pending_rewards(&self, account: Address) -> Result<u128> {
        let info = self.user_reward_info[account].read()?;

        let mut pending = info.reward_balance;

        if info.reward_recipient == account {
            let holder_balance = self.get_balance(account)?;
            if holder_balance > U256::ZERO {
                let global_reward_per_token = self.get_global_reward_per_token()?;
                let reward_per_token_delta = global_reward_per_token
                    .checked_sub(info.reward_per_token)
                    .ok_or_else(|| {
                        TempoPrecompileError::Fatal("underflow in get_pending_rewards".to_string())
                    })?;

                if reward_per_token_delta > U256::ZERO {
                    let accrued = holder_balance
                        .checked_mul(reward_per_token_delta)
                        .and_then(|v| v.checked_div(ACC_PRECISION))
                        .ok_or_else(|| {
                            TempoPrecompileError::Fatal(
                                "overflow in get_pending_rewards".to_string(),
                            )
                        })?;
                    pending = pending.checked_add(accrued).ok_or_else(|| {
                        TempoPrecompileError::Fatal("overflow in get_pending_rewards".to_string())
                    })?;
                }
            }
        }

        pending
            .try_into()
            .map_err(|_| TempoPrecompileError::Fatal("overflow in get_pending_rewards".to_string()))
    }
}

// ===========================================================================
// Precompile dispatch
// ===========================================================================

/// Decoded call variant -- either a TIP-20 token call or a role-management call.
enum TIP20Call {
    TIP20(ITIP20::ITIP20Calls),
    RolesAuth(IRolesAuth::IRolesAuthCalls),
}

impl TIP20Call {
    fn decode(calldata: &[u8]) -> core::result::Result<Self, alloy::sol_types::Error> {
        let selector: [u8; 4] = calldata[..4].try_into().expect("calldata len >= 4");

        if IRolesAuth::IRolesAuthCalls::valid_selector(selector) {
            IRolesAuth::IRolesAuthCalls::abi_decode(calldata).map(Self::RolesAuth)
        } else {
            ITIP20::ITIP20Calls::abi_decode(calldata).map(Self::TIP20)
        }
    }
}

impl Precompile for TIP20Token {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        // Ensure that the token is initialized (has bytecode)
        if !self.is_initialized().unwrap_or(false) {
            return TempoPrecompileError::Revert(ITIP20::Uninitialized {}.abi_encode().into())
                .into_precompile_result(self.storage.gas_used());
        }

        dispatch_call(calldata, TIP20Call::decode, |call| match call {
            // Metadata functions (no calldata decoding needed)
            TIP20Call::TIP20(ITIP20::ITIP20Calls::name(_)) => {
                metadata::<ITIP20::nameCall>(|| self.name())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::symbol(_)) => {
                metadata::<ITIP20::symbolCall>(|| self.symbol())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::decimals(_)) => {
                metadata::<ITIP20::decimalsCall>(|| self.decimals())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::currency(_)) => {
                metadata::<ITIP20::currencyCall>(|| self.currency())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::totalSupply(_)) => {
                metadata::<ITIP20::totalSupplyCall>(|| self.total_supply())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::supplyCap(_)) => {
                metadata::<ITIP20::supplyCapCall>(|| self.supply_cap())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::transferPolicyId(_)) => {
                metadata::<ITIP20::transferPolicyIdCall>(|| self.transfer_policy_id())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::paused(_)) => {
                metadata::<ITIP20::pausedCall>(|| self.paused())
            }

            // View functions
            TIP20Call::TIP20(ITIP20::ITIP20Calls::balanceOf(call)) => {
                view(call, |c| self.balance_of(c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::allowance(call)) => {
                view(call, |c| self.allowance(c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::quoteToken(call)) => {
                view(call, |_| self.quote_token())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::nextQuoteToken(call)) => {
                view(call, |_| self.next_quote_token())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::PAUSE_ROLE(call)) => {
                view(call, |_| Ok(Self::pause_role()))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::UNPAUSE_ROLE(call)) => {
                view(call, |_| Ok(Self::unpause_role()))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::ISSUER_ROLE(call)) => {
                view(call, |_| Ok(Self::issuer_role()))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::BURN_BLOCKED_ROLE(call)) => {
                view(call, |_| Ok(Self::burn_blocked_role()))
            }

            // State-changing functions
            TIP20Call::TIP20(ITIP20::ITIP20Calls::transferFrom(call)) => {
                mutate(call, msg_sender, |s, c| self.transfer_from(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::transfer(call)) => {
                mutate(call, msg_sender, |s, c| self.transfer(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::approve(call)) => {
                mutate(call, msg_sender, |s, c| self.approve(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::changeTransferPolicyId(call)) => {
                mutate_void(call, msg_sender, |s, c| {
                    self.change_transfer_policy_id(s, c)
                })
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::setSupplyCap(call)) => {
                mutate_void(call, msg_sender, |s, c| self.set_supply_cap(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::pause(call)) => {
                mutate_void(call, msg_sender, |s, c| self.pause(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::unpause(call)) => {
                mutate_void(call, msg_sender, |s, c| self.unpause(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::setNextQuoteToken(call)) => {
                mutate_void(call, msg_sender, |s, c| self.set_next_quote_token(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::completeQuoteTokenUpdate(call)) => {
                mutate_void(call, msg_sender, |s, c| {
                    self.complete_quote_token_update(s, c)
                })
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::mint(call)) => {
                mutate_void(call, msg_sender, |s, c| self.mint(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::mintWithMemo(call)) => {
                mutate_void(call, msg_sender, |s, c| self.mint_with_memo(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::burn(call)) => {
                mutate_void(call, msg_sender, |s, c| self.burn(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::burnWithMemo(call)) => {
                mutate_void(call, msg_sender, |s, c| self.burn_with_memo(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::burnBlocked(call)) => {
                mutate_void(call, msg_sender, |s, c| self.burn_blocked(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::transferWithMemo(call)) => {
                mutate_void(call, msg_sender, |s, c| self.transfer_with_memo(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::transferFromWithMemo(call)) => {
                mutate(call, msg_sender, |sender, c| {
                    self.transfer_from_with_memo(sender, c)
                })
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::distributeReward(call)) => {
                mutate_void(call, msg_sender, |s, c| self.distribute_reward(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::setRewardRecipient(call)) => {
                mutate_void(call, msg_sender, |s, c| self.set_reward_recipient(s, c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::claimRewards(call)) => {
                mutate(call, msg_sender, |_, _| self.claim_rewards(msg_sender))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::globalRewardPerToken(call)) => {
                view(call, |_| self.get_global_reward_per_token())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::optedInSupply(call)) => {
                view(call, |_| self.get_opted_in_supply())
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::userRewardInfo(call)) => view(call, |c| {
                self.get_user_reward_info(c.account).map(|info| info.into())
            }),
            TIP20Call::TIP20(ITIP20::ITIP20Calls::getPendingRewards(call)) => {
                view(call, |c| self.get_pending_rewards(c.account))
            }

            // EIP-2612 (T2+, but leafage always runs latest spec)
            TIP20Call::TIP20(ITIP20::ITIP20Calls::permit(call)) => {
                mutate_void(call, msg_sender, |_s, c| self.permit(c))
            }
            TIP20Call::TIP20(ITIP20::ITIP20Calls::nonces(call)) => view(call, |c| self.nonces(c)),
            TIP20Call::TIP20(ITIP20::ITIP20Calls::DOMAIN_SEPARATOR(call)) => {
                view(call, |_| self.domain_separator())
            }

            // RolesAuth functions
            TIP20Call::RolesAuth(IRolesAuth::IRolesAuthCalls::hasRole(call)) => {
                view(call, |c| self.has_role(c))
            }
            TIP20Call::RolesAuth(IRolesAuth::IRolesAuthCalls::getRoleAdmin(call)) => {
                view(call, |c| self.get_role_admin(c))
            }
            TIP20Call::RolesAuth(IRolesAuth::IRolesAuthCalls::grantRole(call)) => {
                mutate_void(call, msg_sender, |s, c| self.grant_role(s, c))
            }
            TIP20Call::RolesAuth(IRolesAuth::IRolesAuthCalls::revokeRole(call)) => {
                mutate_void(call, msg_sender, |s, c| self.revoke_role(s, c))
            }
            TIP20Call::RolesAuth(IRolesAuth::IRolesAuthCalls::renounceRole(call)) => {
                mutate_void(call, msg_sender, |s, c| self.renounce_role(s, c))
            }
            TIP20Call::RolesAuth(IRolesAuth::IRolesAuthCalls::setRoleAdmin(call)) => {
                mutate_void(call, msg_sender, |s, c| self.set_role_admin(s, c))
            }
        })
    }
}
