//! Classic Mini validation and execution over shared Solidity ABI types.
use super::abi::*;
use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::{SolCall, SolInterface};
use revm::interpreter::CallInputs;

pub(super) enum Error {
    Revert,
    Unavailable,
}

#[derive(Clone, Copy)]
pub(super) struct Rules {
    pub arbos34: bool,
    pub arbos39: bool,
    pub arbos42: bool,
    pub arbos54: bool,
}

// Mini's dynamic layouts use fixed offsets and zero-pad truncated header words.
pub(super) fn word(data: &[u8], offset: usize) -> U256 {
    let mut bytes = [0; 32];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = data.get(offset + i).copied().unwrap_or(0);
    }
    U256::from_be_bytes(bytes)
}

pub(super) fn encode<T: SolCall>(value: &T::Return) -> Bytes {
    T::abi_encode_returns(value).into()
}

pub(super) fn padded_bytes_length(data: &[u8], start: usize, size_at: usize) -> bool {
    word(data, size_at)
        .checked_add(U256::from(31))
        .map(|n| n / U256::from(32) * U256::from(32))
        .and_then(|n| n.checked_add(U256::from(start)))
        == Some(U256::from(data.len()))
}

#[derive(Clone, Copy)]
enum Length {
    Exact(usize),
    AtLeast(usize),
}

pub(super) fn run(
    inputs: &CallInputs,
    data: &[u8],
    depth: usize,
    rules: Option<Rules>,
) -> Result<Bytes, Error> {
    use Length::{AtLeast, Exact};
    let Some(selector) = data.get(..4).and_then(|s| <[u8; 4]>::try_from(s).ok()) else {
        return Err(Error::Revert);
    };
    let id = inputs.bytecode_address.as_slice()[19];
    // These checks come from arb_os/*.mini, not Solidity mutability annotations.
    // If a method reads private state before rejecting malformed input, either
    // outcome still reverts, so it is safe to perform the rejection first.
    let (length, forbid_static) = match (id, selector) {
        (0x66, address_table::registerCall::SELECTOR) => (Exact(36), true),
        (
            0x66,
            address_table::lookupCall::SELECTOR
            | address_table::addressExistsCall::SELECTOR
            | address_table::lookupIndexCall::SELECTOR
            | address_table::compressCall::SELECTOR,
        ) => (Exact(36), false),
        (0x66, address_table::sizeCall::SELECTOR) => (Exact(4), false),
        (0x66, address_table::decompressCall::SELECTOR) => (AtLeast(132), false),
        (0x67, bls::registerCall::SELECTOR) => (Exact(132), true),
        (0x67, bls::getPublicKeyCall::SELECTOR) => (Exact(36), false),
        (0x68, function_table::uploadCall::SELECTOR) => (AtLeast(68), true),
        (0x68, function_table::sizeCall::SELECTOR) => (Exact(36), false),
        (0x68, function_table::getCall::SELECTOR) => (Exact(68), false),
        (0x69, test::installAccountCall::SELECTOR) => (AtLeast(165), true),
        (
            0x69,
            test::getMarshalledStorageCall::SELECTOR
            | test::getAccountInfoCall::SELECTOR
            | test::burnArbGasCall::SELECTOR,
        ) => (Exact(36), false),
        (0x69, test::setNonceCall::SELECTOR | test::setBalanceCall::SELECTOR) => (Exact(68), false),
        (0x69, test::setCodeCall::SELECTOR | test::setStateCall::SELECTOR) => (AtLeast(100), false),
        (0x69, test::storeCall::SELECTOR) => (Exact(100), false),
        (
            0x6b,
            owner::addToReserveFundsCall::SELECTOR
            | owner::startCodeUploadCall::SELECTOR
            | owner::allowAllSendersCall::SELECTOR
            | owner::allowOnlyOwnerToSendCall::SELECTOR,
        ) => (Exact(4), true),
        (
            0x6b,
            owner::startCodeUploadWithCheckCall::SELECTOR
            | owner::getChainParameterCall::SELECTOR
            | owner::addAllowedSenderCall::SELECTOR
            | owner::removeAllowedSenderCall::SELECTOR
            | owner::setL1GasPriceEstimateCall::SELECTOR
            | owner::addChainOwnerCall::SELECTOR
            | owner::removeChainOwnerCall::SELECTOR,
        ) => (Exact(36), true),
        (
            0x6b,
            owner::finishCodeUploadAsArbosUpgradeCall::SELECTOR
            | owner::setChainParameterCall::SELECTOR
            | owner::createChainParameterCall::SELECTOR
            | owner::addMappingExceptionCall::SELECTOR
            | owner::removeMappingExceptionCall::SELECTOR,
        ) => (Exact(68), true),
        (
            0x6b,
            owner::getUploadedCodeHashCall::SELECTOR
            | owner::getLastUpgradeHashCall::SELECTOR
            | owner::serializeAllParametersCall::SELECTOR
            | owner::getTotalOfEthBalancesCall::SELECTOR
            | owner::getAllFairGasPriceSendersCall::SELECTOR
            | owner::getAllAllowedSendersCall::SELECTOR
            | owner::getAllChainOwnersCall::SELECTOR
            | owner::getAllMappingExceptionsCall::SELECTOR,
        ) => (Exact(4), false),
        (
            0x6b,
            owner::isFairGasPriceSenderCall::SELECTOR
            | owner::isAllowedSenderCall::SELECTOR
            | owner::isChainOwnerCall::SELECTOR,
        ) => (Exact(36), false),
        (
            0x6b,
            owner::setFairGasPriceSenderCall::SELECTOR | owner::isMappingExceptionCall::SELECTOR,
        ) => (Exact(68), false),
        (0x6b, owner::continueCodeUploadCall::SELECTOR) => (AtLeast(68), true),
        (0x6b, owner::deployContractCall::SELECTOR) => (AtLeast(132), true),
        // Historical ArbOwner methods removed from the final Classic ABI.
        // Their valid calls need private state and must never become unknown
        // selector reverts merely because the current ABI omits them.
        (0x6b, owner::getFeeRecipientsCall::SELECTOR) => (Exact(4), false),
        (
            0x6b,
            owner::giveOwnershipCall::SELECTOR
            | owner::setFeesEnabledCall::SELECTOR
            | owner::setSecondsPerSendCall::SELECTOR,
        ) => (Exact(36), true),
        (
            0x6b,
            owner::setFeeRecipientsCall::SELECTOR
            | owner::finishCodeUploadAsPluggableCall::SELECTOR
            | owner::bindAddressToPluggableCall::SELECTOR,
        ) => (Exact(68), true),
        (0x6b, owner::setGasAccountingParamsCall::SELECTOR) => (Exact(100), true),
        (
            0x6c,
            gas_info::getPricesInWeiWithAggregatorCall::SELECTOR
            | gas_info::getPricesInArbGasWithAggregatorCall::SELECTOR,
        ) => (Exact(36), false),
        (
            0x6c,
            gas_info::getPricesInWeiCall::SELECTOR
            | gas_info::getPricesInArbGasCall::SELECTOR
            | gas_info::getGasAccountingParamsCall::SELECTOR
            | gas_info::getL1GasPriceEstimateCall::SELECTOR,
        ) => (Exact(4), false),
        (0x6c, gas_info::setL1GasPriceEstimateCall::SELECTOR) => (Exact(36), true),
        (0x6c, gas_info::getCurrentTxL1GasFeesCall::SELECTOR) => (AtLeast(4), false), // getCurrentTxL1GasFees ignores trailing data
        (
            0x6d,
            aggregator::getPreferredAggregatorCall::SELECTOR
            | aggregator::getFeeCollectorCall::SELECTOR
            | aggregator::getTxBaseFeeCall::SELECTOR,
        ) => (Exact(36), false),
        (
            0x6d,
            aggregator::setPreferredAggregatorCall::SELECTOR
            | aggregator::setDefaultAggregatorCall::SELECTOR,
        ) => (Exact(36), true),
        (0x6d, aggregator::getDefaultAggregatorCall::SELECTOR) => (Exact(4), false),
        (0x6d, aggregator::setFeeCollectorCall::SELECTOR) => (Exact(68), true),
        (0x6d, aggregator::setTxBaseFeeCall::SELECTOR) => (Exact(68), false), // setTxBaseFee has no static guard
        (
            0x6e,
            retryable::redeemCall::SELECTOR
            | retryable::keepaliveCall::SELECTOR
            | retryable::cancelCall::SELECTOR,
        ) => (Exact(36), true),
        (0x6e, retryable::getLifetimeCall::SELECTOR) => (Exact(4), false),
        (
            0x6e,
            retryable::getTimeoutCall::SELECTOR
            | retryable::getSubmissionPriceCall::SELECTOR
            | retryable::getKeepalivePriceCall::SELECTOR
            | retryable::getBeneficiaryCall::SELECTOR,
        ) => (Exact(36), false),
        (0x6f, statistics::getStatsCall::SELECTOR) => (Exact(4), false),
        // 0xc8 is revertPrecompile in EVM execution. The native RPC's node
        // interface intercept is a separate path, not an EVM precompile.
        _ => return Err(Error::Revert),
    };
    if match length {
        Exact(n) => data.len() != n,
        AtLeast(n) => data.len() < n,
    } {
        return Err(Error::Revert);
    }
    if id == 0x69 && selector != test::burnArbGasCall::SELECTOR && !inputs.caller.is_zero() {
        return Err(Error::Revert);
    }
    if id == 0x67 && rules.is_some_and(|r| r.arbos34 && !r.arbos42) {
        return Err(Error::Revert); // ArbBLS was disabled throughout ArbOS 34–41.
    }
    if inputs.is_static {
        let guard = match (id, selector) {
            (0x66, address_table::compressCall::SELECTOR)
            | (0x6b, owner::setFairGasPriceSenderCall::SELECTOR) => {
                rules.map(|r| r.arbos34 && !r.arbos42)
            }
            (
                0x6b,
                owner::getChainParameterCall::SELECTOR
                | owner::setChainParameterCall::SELECTOR
                | owner::createChainParameterCall::SELECTOR,
            )
            | (0x6c, gas_info::setL1GasPriceEstimateCall::SELECTOR) => rules.map(|r| r.arbos42),
            _ if forbid_static => rules.map(|r| r.arbos34),
            _ => Some(false),
        };
        match guard {
            Some(true) => return Err(Error::Revert),
            None => return Err(Error::Unavailable),
            _ => {}
        }
    }
    match (id, selector) {
        (0x66, address_table::decompressCall::SELECTOR) => return decompress(data, rules),
        // Only index/address zero is independent of the private address table.
        (0x66, _) => {
            use address_table::IArbAddressTableCalls::*;
            let call = address_table::IArbAddressTableCalls::abi_decode(data)
                .map_err(|_| Error::Revert)?;
            return match call {
                register(call) if call.addr.is_zero() => {
                    Ok(encode::<address_table::registerCall>(&U256::ZERO))
                }
                lookup(call) if call.addr.is_zero() => {
                    Ok(encode::<address_table::lookupCall>(&U256::ZERO))
                }
                addressExists(call) if call.addr.is_zero() => {
                    Ok(encode::<address_table::addressExistsCall>(&true))
                }
                compress(call) if call.addr.is_zero() => Ok(encode::<address_table::compressCall>(
                    &Bytes::from_static(&[0x80]),
                )),
                lookupIndex(call) if call.index.is_zero() => {
                    Ok(encode::<address_table::lookupIndexCall>(&Address::ZERO))
                }
                _ => Err(Error::Unavailable),
            };
        }
        (0x68, function_table::uploadCall::SELECTOR) => {
            if word(data, 4) != U256::from(32) && rules.is_some_and(|r| r.arbos54) {
                return Err(Error::Revert);
            }
            validate_function_table(data, rules)?;
        }
        (0x69, test::burnArbGasCall::SELECTOR) => {
            let call = test::burnArbGasCall::abi_decode(data).map_err(|_| Error::Revert)?;
            if call.gasAmount.is_zero() {
                return Ok(encode::<test::burnArbGasCall>(&().into()));
            }
        }
        (0x69, test::setCodeCall::SELECTOR | test::setStateCall::SELECTOR) => {
            // Unlike ArbSys.sendTxToL1's safeAdd, these Mini methods use
            // wrapping uint arithmetic. An overflowing size can pass this
            // guard and reach the unsupported account mutation path.
            if word(data, 68).wrapping_add(U256::from(100)) > U256::from(data.len()) {
                return Err(Error::Revert);
            }
        }
        (0x6b, owner::deployContractCall::SELECTOR) => {
            let padded = (word(data, 100).wrapping_add(U256::from(31)) / U256::from(32))
                .wrapping_add(U256::from(4))
                .wrapping_mul(U256::from(32))
                .wrapping_add(U256::from(4));
            if padded != U256::from(data.len()) {
                return Err(Error::Revert);
            }
        }
        // Both operations are restricted to the top-level AVM call frame.
        (0x6b, owner::finishCodeUploadAsArbosUpgradeCall::SELECTOR)
        | (0x6e, retryable::redeemCall::SELECTOR)
            if depth != 1 =>
        {
            return Err(Error::Revert);
        }
        _ => {}
    }
    // A valid method that needs unexported state must not become a catchable
    // revert (or a fabricated zero). This also covers unknown historical
    // method availability, e.g. getCurrentTxL1GasFees before its introduction.
    Err(Error::Unavailable)
}

// Model a Mini ByteArray slice without allocating its declared length. Mini
// extracts share the backing AVM buffer: reading outside the visible calldata
// may expose parent memory (or overflow a hidden slice offset). That context
// isn't available here; do not fabricate padded zeroes for those reads.
struct Stream<'a> {
    data: &'a [u8],
    start: u64,
    length: u64,
    position: u64,
}

impl<'a> Stream<'a> {
    fn new(data: &'a [u8], start: U256, length: U256) -> Option<Self> {
        let limit = U256::from(1u128 << 64);
        if start >= limit || length.is_zero() || length > limit || start + length > limit {
            return None;
        }
        // start is nonzero for both ABI payloads, so length is below 2^64.
        Some(Self {
            data,
            start: start.to(),
            length: length.to(),
            position: 0,
        })
    }

    fn byte(&mut self) -> Result<u8, Error> {
        if self.position >= self.length {
            return Err(Error::Revert);
        }
        let index = self.start + self.position;
        self.position += 1;
        usize::try_from(index)
            .ok()
            .and_then(|i| self.data.get(i))
            .copied()
            .ok_or(Error::Unavailable)
    }

    fn uint(&mut self, rules: Option<Rules>) -> Result<(u8, U256), Error> {
        let prefix = self.byte()?;
        if prefix < 0x80 {
            return Ok((prefix, U256::from(prefix)));
        }
        let length = prefix - 0x80;
        if length > 32 && rules.is_some_and(|r| r.arbos39) {
            return Err(Error::Revert);
        }
        if u64::from(length) > self.length - self.position {
            return Err(Error::Revert);
        }
        let mut value = U256::ZERO;
        for _ in 0..length {
            value = (value << 8) | U256::from(self.byte()?);
        }
        Ok((prefix, value))
    }
}

fn decompress(data: &[u8], rules: Option<Rules>) -> Result<Bytes, Error> {
    let offset = word(data, 36);
    let length = word(data, 68);
    if offset >= length {
        return Err(Error::Revert);
    }
    if word(data, 4) != U256::from(64) {
        match rules {
            Some(rules) if rules.arbos54 => return Err(Error::Revert),
            None => return Err(Error::Unavailable),
            _ => {}
        }
    }
    let Some(mut stream) = Stream::new(data, offset.wrapping_add(U256::from(100)), length - offset)
    else {
        return Err(Error::Revert);
    };
    let (prefix, value) = stream.uint(rules)?;
    if prefix > 0xa0 && rules.is_none() {
        return Err(Error::Unavailable);
    }
    if prefix < 0x94 && !value.is_zero() {
        return Err(Error::Unavailable); // nonzero table index
    }
    if value >> 160 != U256::ZERO {
        return Err(Error::Revert);
    }
    Ok(encode::<address_table::decompressCall>(
        &(
            Address::from_word(value.into()),
            offset.wrapping_add(U256::from(stream.position)),
        )
            .into(),
    ))
}

fn validate_function_table(data: &[u8], rules: Option<Rules>) -> Result<(), Error> {
    let Some(mut stream) = Stream::new(data, U256::from(68), word(data, 36)) else {
        return Err(Error::Revert);
    };
    let (_, entries) = stream.uint(rules)?;
    if entries > U256::from(1024) {
        return Err(Error::Revert);
    }
    // Each entry needs four selector bytes, a payable byte and at least one
    // RLP byte. Prove truncation before trying to read unavailable backing data.
    if entries.to::<u64>() * 6 > stream.length - stream.position {
        return Err(Error::Revert);
    }
    for _ in 0..entries.to::<usize>() {
        if stream.length - stream.position < 6 {
            return Err(Error::Revert);
        }
        // Selector and payable bytes have no content restrictions.
        stream.position += 5;
        stream.uint(rules)?;
    }
    Ok(()) // Mini ignores trailing bytes and accepts noncanonical RLP integers.
}
