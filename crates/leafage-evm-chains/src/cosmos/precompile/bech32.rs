use crate::cosmos::precompile::bech32::Bech32I::{bech32ToHexCall, hexToBech32Call, Bech32ICalls};
use alloy::primitives::{address, Address};
use alloy_sol_types::{sol, SolInterface};
use bech32::{Bech32m, Hrp};
use leafage_evm_types::Bytes;
use revm::precompile::{
    PrecompileError, PrecompileOutput, PrecompileResult, PrecompileWithAddress,
};

pub const BECH32: PrecompileWithAddress = PrecompileWithAddress(
    address!("0x0000000000000000000000000000000000000400"),
    bech32_run,
);

const BECH32PRECOMPILE_BASE_GAS: u64 = 6000;

sol!(
    #[sol(all_derives)]
    #[sol(extra_methods)]
    /// @author Evmos Team
    /// @title Bech32 Precompiled Contract
    /// @dev The interface through which solidity contracts can convert addresses from
    /// hex to bech32 and vice versa.
    /// @custom:address 0x0000000000000000000000000000000000000400
interface Bech32I {
    /// @dev Defines a method for converting a hex formatted address to bech32.
    /// @param addr The hex address to be converted.
    /// @param prefix The human readable prefix (HRP) of the bech32 address.
    /// @return bech32Address The address in bech32 format.
    function hexToBech32(
        address addr,
        string memory prefix
    ) external returns (string memory bech32Address);

    /// @dev Defines a method for converting a bech32 formatted address to hex.
    /// @param bech32Address The bech32 address to be converted.
    /// @return addr The address in hex format.
    function bech32ToHex(
        string memory bech32Address
    ) external returns (address addr);
});

fn bech32_run(input: &[u8], gas_limit: u64) -> PrecompileResult {
    if gas_limit < BECH32PRECOMPILE_BASE_GAS {
        return Err(PrecompileError::OutOfGas);
    }
    if input.len() < 4 {
        return Err(PrecompileError::other("invalid input"));
    }
    let call = Bech32ICalls::abi_decode(&input);
    match call {
        Ok(Bech32ICalls::hexToBech32(call)) => hex_to_bech32(call),
        Ok(Bech32ICalls::bech32ToHex(call)) => bech32_to_hex(call),
        Err(err) => return Err(PrecompileError::other(format!("{:?}", err))),
    }
}

fn hex_to_bech32(call: hexToBech32Call) -> PrecompileResult {
    let hexToBech32Call { addr, prefix } = call;
    if prefix.trim().is_empty() {
        return Err(PrecompileError::other("invalid bech32 human readable prefix (HRP). Please provide a either an account, validator or consensus address prefix"));
    }
    valid_address(&addr)?;
    let hrp = Hrp::parse(&prefix).map_err(|err| PrecompileError::other(err.to_string()))?;
    let bech32_str = bech32::encode::<Bech32m>(hrp, &addr.to_vec())
        .map_err(|err| PrecompileError::other(err.to_string()))?;
    Ok(PrecompileOutput::new(
        BECH32PRECOMPILE_BASE_GAS,
        Bytes::copy_from_slice(bech32_str.as_bytes()),
    ))
}

fn bech32_to_hex(call: bech32ToHexCall) -> PrecompileResult {
    let bech32ToHexCall { bech32Address } = call;
    if bech32Address.trim().is_empty() {
        return Err(PrecompileError::other(format!(
            "invalid bech32 address: {bech32Address}"
        )));
    }
    let (_, decoded_data) = bech32::decode(&bech32Address)
        .map_err(|err| PrecompileError::other(format!("decoding bech32 failed: {}", err)))?;
    let address = Address::from_slice(&decoded_data);
    valid_address(&address)?;
    Ok(PrecompileOutput::new(
        BECH32PRECOMPILE_BASE_GAS,
        Bytes::copy_from_slice(address.as_slice()),
    ))
}

fn valid_address(addr: &Address) -> PrecompileResult {
    if addr.is_empty() {
        return Err(PrecompileError::other("address cannot be empty"));
    }
    if addr.len() > 255 {
        return Err(PrecompileError::other(format!(
            "address max length is {}, got {}",
            255,
            addr.len()
        )));
    }
    Ok(PrecompileOutput::new(0, Default::default()))
}
