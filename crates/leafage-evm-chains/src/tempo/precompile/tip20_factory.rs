//! TIP-20 token factory precompile -- deploys new TIP-20 tokens at deterministic addresses.
//!
//! Ported from `tempo/crates/precompiles/src/tip20_factory/`.
//!
//! ## Storage layout
//!
//! The TIP20Factory has **no persistent fields** -- it is stateless aside from the bytecode
//! sentinel. Token state lives at the deterministic TIP-20 addresses themselves.

use alloy::primitives::{keccak256, Address, Bytes, B256};
use alloy::sol_types::{SolError, SolInterface, SolValue};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx};
use super::tip20::{is_tip20_prefix, TIP20Token, ITIP20};
use super::{
    fill_precompile_output, input_cost, mutate, view, Precompile, PATH_USD_ADDRESS,
    TIP20_FACTORY_ADDRESS,
};

// ===========================================================================
// Constants
// ===========================================================================

/// Number of reserved addresses (0 to RESERVED_SIZE-1) that cannot be deployed via factory.
const RESERVED_SIZE: u64 = 1024;

/// TIP20 token address prefix (12 bytes): 0x20C000000000000000000000
const TIP20_PREFIX_BYTES: [u8; 12] = [
    0x20, 0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// USD currency identifier.
const USD_CURRENCY: &str = "USD";

// ===========================================================================
// Solidity ABI types
// ===========================================================================

alloy::sol! {
    interface ITIP20Factory {
        function createToken(
            string memory name,
            string memory symbol,
            string memory currency,
            address quoteToken,
            address admin,
            bytes32 salt
        ) external returns (address);

        function isTIP20(address token) external view returns (bool);

        function getTokenAddress(address sender, bytes32 salt) external view returns (address);

        event TokenCreated(
            address indexed token,
            string name,
            string symbol,
            string currency,
            address quoteToken,
            address admin,
            bytes32 salt
        );

        error TokenAlreadyExists(address token);
        error AddressReserved();
        error AddressNotReserved();
    }
}

// ===========================================================================
// Address computation
// ===========================================================================

/// Computes the deterministic TIP20 address from sender and salt.
pub fn compute_tip20_address(sender: Address, salt: B256) -> (Address, u64) {
    let hash = keccak256((sender, salt).abi_encode());

    let mut padded = [0u8; 8];
    padded.copy_from_slice(&hash[..8]);
    let lower_bytes = u64::from_be_bytes(padded);

    let mut address_bytes = [0u8; 20];
    address_bytes[..12].copy_from_slice(&TIP20_PREFIX_BYTES);
    address_bytes[12..].copy_from_slice(&hash[..8]);

    (Address::from(address_bytes), lower_bytes)
}

// ===========================================================================
// TIP20Factory struct (manual macro expansion)
// ===========================================================================

/// Factory precompile for deploying new TIP-20 tokens at deterministic addresses.
pub struct TIP20Factory {
    pub address: Address,
    pub storage: StorageCtx,
}

impl TIP20Factory {
    pub fn new() -> Self {
        Self {
            address: TIP20_FACTORY_ADDRESS,
            storage: StorageCtx::default(),
        }
    }

    fn __initialize(&mut self) -> Result<()> {
        let bytecode = revm::state::Bytecode::new_legacy(Bytes::from_static(&[0xef]));
        self.storage.set_code(self.address, bytecode)?;
        Ok(())
    }

    fn emit_event(&mut self, event: impl alloy::primitives::IntoLogData) -> Result<()> {
        self.storage
            .emit_event(self.address, event.into_log_data())
    }

    /// Initializes the TIP-20 factory precompile.
    pub fn initialize(&mut self) -> Result<()> {
        self.__initialize()
    }

    /// Computes the deterministic address for a token. Reverts if reserved.
    pub fn get_token_address(&self, call: ITIP20Factory::getTokenAddressCall) -> Result<Address> {
        let (address, lower_bytes) = compute_tip20_address(call.sender, call.salt);

        if lower_bytes < RESERVED_SIZE {
            return Err(TempoPrecompileError::Revert(
                ITIP20Factory::AddressReserved {}.abi_encode().into(),
            ));
        }

        Ok(address)
    }

    /// Returns `true` if `token` has the correct TIP-20 prefix and has code deployed.
    pub fn is_tip20(&self, token: Address) -> Result<bool> {
        if !is_tip20_prefix(token) {
            return Ok(false);
        }
        self.storage
            .with_account_info(token, |info| Ok(!info.is_empty_code_hash()))
    }

    /// Deploys a new TIP-20 token at a deterministic address.
    pub fn create_token(
        &mut self,
        sender: Address,
        call: ITIP20Factory::createTokenCall,
    ) -> Result<Address> {
        let (token_address, lower_bytes) = compute_tip20_address(sender, call.salt);

        if self.is_tip20(token_address)? {
            return Err(TempoPrecompileError::Revert(
                ITIP20Factory::TokenAlreadyExists {
                    token: token_address,
                }
                .abi_encode()
                .into(),
            ));
        }

        // Ensure quote token is a valid deployed TIP20
        if !self.is_tip20(call.quoteToken)? {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidQuoteToken {}.abi_encode().into(),
            ));
        }

        // If token is USD, its quote token must also be USD
        if call.currency == USD_CURRENCY
            && TIP20Token::from_address(call.quoteToken)?.currency()? != USD_CURRENCY
        {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidQuoteToken {}.abi_encode().into(),
            ));
        }

        if lower_bytes < RESERVED_SIZE {
            return Err(TempoPrecompileError::Revert(
                ITIP20Factory::AddressReserved {}.abi_encode().into(),
            ));
        }

        TIP20Token::from_address(token_address)?.initialize(
            sender,
            &call.name,
            &call.symbol,
            &call.currency,
            call.quoteToken,
            call.admin,
        )?;

        self.emit_event(ITIP20Factory::TokenCreated {
            token: token_address,
            name: call.name,
            symbol: call.symbol,
            currency: call.currency,
            quoteToken: call.quoteToken,
            admin: call.admin,
            salt: call.salt,
        })?;

        Ok(token_address)
    }

    /// Deploys a TIP-20 token at a reserved address. Used during genesis/hardforks.
    pub fn create_token_reserved_address(
        &mut self,
        address: Address,
        name: &str,
        symbol: &str,
        currency: &str,
        quote_token: Address,
        admin: Address,
    ) -> Result<Address> {
        if !is_tip20_prefix(address) {
            return Err(TempoPrecompileError::Revert(
                ITIP20::InvalidToken {}.abi_encode().into(),
            ));
        }

        if self.is_tip20(address)? {
            return Err(TempoPrecompileError::Revert(
                ITIP20Factory::TokenAlreadyExists { token: address }
                    .abi_encode()
                    .into(),
            ));
        }

        // quote_token must be address(0) or a valid TIP20
        if !quote_token.is_zero() {
            if address == PATH_USD_ADDRESS || !self.is_tip20(quote_token)? {
                return Err(TempoPrecompileError::Revert(
                    ITIP20::InvalidQuoteToken {}.abi_encode().into(),
                ));
            }
            if currency == USD_CURRENCY
                && TIP20Token::from_address(quote_token)?.currency()? != USD_CURRENCY
            {
                return Err(TempoPrecompileError::Revert(
                    ITIP20::InvalidQuoteToken {}.abi_encode().into(),
                ));
            }
        }

        // Validate that the address is within the reserved range
        let mut padded = [0u8; 8];
        padded.copy_from_slice(&address.as_slice()[12..]);
        let lower_bytes = u64::from_be_bytes(padded);
        if lower_bytes >= RESERVED_SIZE {
            return Err(TempoPrecompileError::Revert(
                ITIP20Factory::AddressNotReserved {}.abi_encode().into(),
            ));
        }

        let mut token = TIP20Token::from_address(address)?;
        token.initialize(admin, name, symbol, currency, quote_token, admin)?;

        self.emit_event(ITIP20Factory::TokenCreated {
            token: address,
            name: name.into(),
            symbol: symbol.into(),
            currency: currency.into(),
            quoteToken: quote_token,
            admin,
            salt: B256::ZERO,
        })?;

        Ok(address)
    }
}

impl ContractStorage for TIP20Factory {
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
// Dispatch
// ===========================================================================

/// Dispatches calldata, handling selector validation and ABI decode errors.
fn dispatch_call<T>(
    calldata: &[u8],
    decode: impl FnOnce(&[u8]) -> core::result::Result<T, alloy::sol_types::Error>,
    f: impl FnOnce(T) -> PrecompileResult,
) -> PrecompileResult {
    let storage = StorageCtx::default();

    if calldata.len() < 4 {
        return Ok(fill_precompile_output(
            PrecompileOutput::new_reverted(0, Bytes::new()),
            &storage,
        ));
    }

    let result = decode(calldata);

    match result {
        Ok(call) => f(call).map(|res| fill_precompile_output(res, &storage)),
        Err(alloy::sol_types::Error::UnknownSelector { selector, .. }) => {
            unknown_selector(*selector, storage.gas_used())
                .map(|res| fill_precompile_output(res, &storage))
        }
        Err(_) => Ok(fill_precompile_output(
            PrecompileOutput::new_reverted(0, Bytes::new()),
            &storage,
        )),
    }
}

fn unknown_selector(selector: [u8; 4], gas: u64) -> PrecompileResult {
    TempoPrecompileError::UnknownFunctionSelector(selector).into_precompile_result(gas)
}

impl Precompile for TIP20Factory {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            ITIP20Factory::ITIP20FactoryCalls::abi_decode,
            |call| match call {
                ITIP20Factory::ITIP20FactoryCalls::createToken(call) => {
                    mutate(call, msg_sender, |s, c| self.create_token(s, c))
                }
                ITIP20Factory::ITIP20FactoryCalls::isTIP20(call) => {
                    view(call, |c| self.is_tip20(c.token))
                }
                ITIP20Factory::ITIP20FactoryCalls::getTokenAddress(call) => {
                    view(call, |c| self.get_token_address(c))
                }
            },
        )
    }
}
