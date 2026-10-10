//! EIP-2612 permit and EIP-712 domain operations.
//!
//! Transcribed from Base reth (`base/crates/common/precompiles/src/common/ops/permittable.rs`).
//! The domain is the canonical four-field shape `(name, version, chainId, verifyingContract)`
//! with `version` pinned to `"1"`, and `name` read live from storage — so a successful
//! `updateName` invalidates outstanding signatures.
//!
//! Cobalt meters the cryptography (Base #4891, Cantina #20): every keccak in the domain
//! separator and signing digest is charged at the EVM `KECCAK256` schedule before hashing, and
//! signer recovery is charged the `ECRECOVER` base cost up front, before `v` is even checked.
//! Beryl's gas schedule is frozen, so there all of this stays free.

use alloy::primitives::{b256, keccak256, Address, B256, FixedBytes, Signature, U256};
use alloy::sol_types::SolValue;

use super::abi::IB20;
use super::error::{B20Error, Result};
use super::layout::{checked_add, B20Store};
use super::ops::approve;
use super::port::B20Port;

/// EVM `KECCAK256` base cost.
const KECCAK256_GAS: u64 = 30;
/// EVM `KECCAK256` cost per 32-byte word.
const KECCAK256_WORD_GAS: u64 = 6;
/// Flat signer-recovery charge, priced to the `ECRECOVER` precompile (Cobalt).
pub const RECOVER_GAS: u64 = 3000;

/// `keccak256(data)`, charging the EVM schedule first when `metered`.
fn hash<P: B20Port>(store: &mut B20Store<'_, P>, metered: bool, data: &[u8]) -> Result<B256> {
    if metered {
        let words = data.len().div_ceil(32) as u64;
        store.port().deduct_gas(KECCAK256_GAS + KECCAK256_WORD_GAS * words)?;
    }
    Ok(keccak256(data))
}

/// `keccak256("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")`
const DOMAIN_TYPEHASH: B256 =
    b256!("8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f");

/// `keccak256("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")`
const PERMIT_TYPEHASH: B256 =
    b256!("6e71edae12b1b97f4d1f60370fef10105fa2faae0126114a169c64845d6126c9");

/// EIP-712 domain version, pinned to `"1"`.
const VERSION: &[u8] = b"1";

/// EIP-191 prefix for structured data, followed by the EIP-712 version byte.
const EIP712_SIGNING_PREFIX: [u8; 2] = [0x19, 0x01];

/// Legacy `v` for even-Y ECDSA recovery.
const RECOVERY_ID_EVEN_Y: u8 = 27;
/// Legacy `v` for odd-Y ECDSA recovery.
const RECOVERY_ID_ODD_Y: u8 = 28;

/// ERC-5267 `eip712Domain()` return tuple.
pub type Eip712Domain = (FixedBytes<1>, String, String, U256, Address, B256, Vec<U256>);

/// EIP-2612 `permit` arguments.
#[derive(Clone, Debug)]
pub struct PermitArgs {
    /// Token owner whose allowance is being set.
    pub owner: Address,
    /// Account being granted the allowance.
    pub spender: Address,
    /// Allowance amount.
    pub value: U256,
    /// Unix timestamp after which the signature is no longer valid.
    pub deadline: U256,
    /// Signature recovery id: 27 or 28.
    pub v: u8,
    /// Signature `r` component.
    pub r: B256,
    /// Signature `s` component.
    pub s: B256,
}

impl PermitArgs {
    /// `keccak256("\x19\x01" ++ domainSeparator ++ keccak256(Permit struct))`: two keccak
    /// rounds, both metered when `metered`.
    fn signing_hash<P: B20Port>(
        &self,
        store: &mut B20Store<'_, P>,
        metered: bool,
        domain_separator: B256,
        nonce: U256,
    ) -> Result<B256> {
        let encoded =
            (PERMIT_TYPEHASH, self.owner, self.spender, self.value, nonce, self.deadline)
                .abi_encode();
        let struct_hash = hash(store, metered, &encoded)?;
        let mut buf = [0u8; 66];
        buf[..2].copy_from_slice(&EIP712_SIGNING_PREFIX);
        buf[2..34].copy_from_slice(domain_separator.as_slice());
        buf[34..66].copy_from_slice(struct_hash.as_slice());
        hash(store, metered, &buf)
    }

    fn invalid_signer(&self) -> B20Error {
        B20Error::revert(IB20::InvalidSigner { signer: Address::ZERO, owner: self.owner })
    }

    /// Maps Ethereum `v` (27/28) to secp256k1 parity, then recovers the signer.
    fn recover_signer(&self, signing_hash: B256) -> Result<Address> {
        let odd_y_parity = match self.v {
            RECOVERY_ID_EVEN_Y => false,
            RECOVERY_ID_ODD_Y => true,
            _ => return Err(self.invalid_signer()),
        };
        let sig = Signature::from_scalars_and_parity(self.r, self.s, odd_y_parity);
        sig.recover_address_from_prehash(&signing_hash).map_err(|_| self.invalid_signer())
    }

    /// Rejects a zero or mismatched recovered address, matching Solidity's guard.
    fn validate_recovered_address(recovered: Address, owner: Address) -> Result<()> {
        if recovered.is_zero() || recovered != owner {
            return Err(B20Error::revert(IB20::InvalidSigner { signer: recovered, owner }));
        }
        Ok(())
    }
}

/// Computes this token's EIP-712 domain separator. Cobalt meters its three keccak rounds —
/// including when it is only read through `DOMAIN_SEPARATOR()`.
pub fn domain_separator<P: B20Port>(store: &mut B20Store<'_, P>, chain_id: u64) -> Result<B256> {
    let metered = store.version().is_cobalt();
    let name = store.name()?;
    let name_hash = hash(store, metered, name.as_bytes())?;
    let version_hash = hash(store, metered, VERSION)?;
    let address = store.address();
    let encoded =
        (DOMAIN_TYPEHASH, name_hash, version_hash, U256::from(chain_id), address).abi_encode();
    hash(store, metered, &encoded)
}

/// Returns the ERC-5267 `eip712Domain()` tuple.
pub fn eip712_domain<P: B20Port>(
    store: &mut B20Store<'_, P>,
    chain_id: u64,
) -> Result<Eip712Domain> {
    let name = store.name()?;
    let address = store.address();
    Ok((
        // bits 0+1+2+3: name, version, chainId, verifyingContract
        FixedBytes::<1>::from([0x0f]),
        name,
        String::from("1"),
        U256::from(chain_id),
        address,
        B256::ZERO,
        Vec::new(),
    ))
}

/// EIP-2612 permit. EOA signatures only — no ERC-1271 fallback.
pub fn permit<P: B20Port>(
    store: &mut B20Store<'_, P>,
    chain_id: u64,
    now: U256,
    args: PermitArgs,
) -> Result<()> {
    if now > args.deadline {
        return Err(B20Error::revert(IB20::ExpiredSignature { deadline: args.deadline }));
    }

    let metered = store.version().is_cobalt();
    let domain_sep = domain_separator(store, chain_id)?;
    let nonce = store.nonce(args.owner)?;
    let signing_hash = args.signing_hash(store, metered, domain_sep, nonce)?;
    if metered {
        store.port().deduct_gas(RECOVER_GAS)?;
    }
    let recovered = args.recover_signer(signing_hash)?;
    PermitArgs::validate_recovered_address(recovered, args.owner)?;

    // Base's `increment_nonce` re-reads the nonce (a warm SLOAD) rather than reusing `nonce`.
    let current = store.nonce(args.owner)?;
    store.set_nonce(args.owner, checked_add(current, U256::ONE)?)?;
    approve(store, args.owner, args.spender, args.value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typehashes_match_their_preimages() {
        assert_eq!(
            PERMIT_TYPEHASH,
            keccak256(
                "Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)"
            )
        );
        assert_eq!(
            DOMAIN_TYPEHASH,
            keccak256(
                "EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)"
            )
        );
    }
}
