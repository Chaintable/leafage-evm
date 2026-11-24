//! BSC CAS20 Compliant Asset Standard tokens (BEP-702), active from Jenner.
//!
//! A CAS20 token is a stateful precompile: its address holds only a one-byte `0xEF` marker
//! as code, its behaviour is native code dispatched by address prefix, and its state lives in
//! the EVM trie at the token address in ERC-7201 namespaced layouts. A node holding BSC state
//! can therefore execute tokens locally.
//!
//! This module is a port of go-bsc `core/vm/cas20*.go` (v1.8.1). It covers the full token
//! surface, reads and writes, for both variants (Asset and Stablecoin), and meters gas per
//! storage access and per keccak exactly as go-bsc does, so `estimateGas` over a CAS20 token
//! agrees with a real BSC node. The logic files mirror the Go ones one to one:
//!
//! | Go                      | here                         |
//! |-------------------------|------------------------------|
//! | `contracts_stateful.go`, `cas20_gas.go` | [`frame`]    |
//! | `cas20_errors.go`       | [`errors`]                   |
//! | `cas20_abi.go`          | `abi`                        |
//! | `cas20_storage.go`      | `storage`                    |
//! | `cas20_token.go`        | `token`                      |
//! | `cas20_admin.go`        | `admin`                      |
//! | `cas20_metadata.go`     | `metadata`                   |
//! | `cas20_permit.go`, `cas20_memo.go` | `permit`          |
//! | `cas20_asset.go`        | `asset`                      |
//! | `cas20_stablecoin.go`   | `stablecoin`                 |
//! | `cas20_policy.go` (read path) | `policy`               |
//!
//! Not ported, and forwarded as `-39008` instead (see [`is_forwarded`]), matching the Base
//! B20 port: the factory, the activation registry and the policy registry's own dispatch.
//! They are administrative; token creation in particular only happens once governance has
//! activated it. The policy registry's read path *is* ported, because every policy-gated
//! transfer consults it.

use alloy::primitives::{address, keccak256, Address, B256};

mod abi;
mod admin;
mod asset;
pub mod errors;
mod evm;
pub mod frame;
mod metadata;
mod permit;
mod policy;
mod stablecoin;
mod storage;
mod token;

pub use errors::Cas20Err;
pub(crate) use evm::token_precompile;
pub use frame::{Cas20Host, Frame};
pub use policy::POLICY_REGISTRY;

/// Cached 4-byte selector of a function signature.
macro_rules! sel {
    ($sig:literal) => {{
        static SEL: ::std::sync::OnceLock<[u8; 4]> = ::std::sync::OnceLock::new();
        *SEL.get_or_init(|| $crate::bsc::cas20::errors::selector($sig))
    }};
}

/// Cached keccak256 of a string: event topics and role/scope identifiers.
macro_rules! hash {
    ($s:literal) => {{
        static HASH: ::std::sync::OnceLock<::alloy::primitives::B256> =
            ::std::sync::OnceLock::new();
        *HASH.get_or_init(|| ::alloy::primitives::keccak256($s.as_bytes()))
    }};
}

pub(crate) use {hash, sel};

/// `CAS20Factory`.
pub const FACTORY: Address = address!("0xCA5F000000000000000000000000000000000000");
/// `ActivationRegistry`.
pub const ACTIVATION_REGISTRY: Address = address!("0x7020000000000000000000000000000000000001");

/// The code every initialized token carries. `0xEF` cannot be deployed (EIP-3541), so nothing
/// can forge it.
pub const MARKER_CODE: [u8; 1] = [0xef];

const VARIANT_ASSET: u8 = 0x00;
const VARIANT_STABLECOIN: u8 = 0x01;

/// `IsCAS20Address`: `0xCA52` followed by eight zero bytes. Byte 10 is the variant.
pub fn is_cas20_address(address: &Address) -> bool {
    let b = address.as_slice();
    b[0] == 0xca && b[1] == 0x52 && b[2..10] == [0u8; 8]
}

/// Whether `address` is a CAS20 token address of a known variant: what go-bsc
/// `resolveCAS20Token` routes to native code.
pub fn is_cas20_token(address: &Address) -> bool {
    is_cas20_address(address) && matches!(address[10], VARIANT_ASSET | VARIANT_STABLECOIN)
}

/// The CAS20 singletons leafage forwards (`-39008`) rather than executing locally.
pub fn is_forwarded(address: &Address) -> bool {
    *address == FACTORY || *address == ACTIVATION_REGISTRY || *address == POLICY_REGISTRY
}

/// How a CAS20 frame ended, after go-bsc's `finishCAS20Metered` precedence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Success, with returndata.
    Return(Vec<u8>),
    /// Revert, with returndata. Unspent gas is returned to the caller.
    Revert(Vec<u8>),
    /// An exceptional halt: all gas is consumed.
    OutOfGas,
}

/// `finishCAS20`: a refused call form is an ABI error, not an exceptional halt
/// (BEP-702 3.2).
fn finish(res: Result<Vec<u8>, Cas20Err>) -> Outcome {
    match res {
        Ok(ret) => Outcome::Return(ret),
        Err(Cas20Err::DelegateCall) => Outcome::Revert(errors_data("DelegateCallNotAllowed()")),
        Err(Cas20Err::WriteProtection) => Outcome::Revert(errors_data("StaticCallNotAllowed()")),
        Err(Cas20Err::Revert(data)) => Outcome::Revert(data),
        Err(Cas20Err::OutOfGas) => Outcome::OutOfGas,
    }
}

fn errors_data(sig: &str) -> Vec<u8> {
    errors::selector(sig).to_vec()
}

/// `finishCAS20Metered`: write protection, then an exhausted budget, outrank whatever the
/// logic returned.
fn finish_metered(f: &Frame<'_>, res: Result<Vec<u8>, Cas20Err>) -> Outcome {
    if f.write_protection_violated() {
        return finish(Err(Cas20Err::WriteProtection));
    }
    if f.out_of_gas() {
        return Outcome::OutOfGas;
    }
    finish(res)
}

/// `cas20EnterCall`: the prologue every CAS20 entry point runs.
fn enter_call(f: &mut Frame<'_>, input: &[u8]) -> Result<(), Cas20Err> {
    if !f.direct_call {
        return Err(Cas20Err::DelegateCall);
    }
    if !f.value.is_zero() {
        return Err(errors::rev("NonPayable()", &[]));
    }
    if !f.charge_calldata(input) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

/// `cas20InitializedMetered`: exact-hash, not non-empty, since foreign code is not a token.
fn initialized_metered(f: &mut Frame<'_>, address: Address) -> bool {
    if !f.charge_account_access(address) {
        return false;
    }
    f.host.code_hash(address) == marker_code_hash()
}

fn marker_code_hash() -> B256 {
    static HASH: std::sync::OnceLock<B256> = std::sync::OnceLock::new();
    *HASH.get_or_init(|| keccak256(MARKER_CODE))
}

/// Runs a call into the CAS20 token at `f.self_address` (`runCAS20Token`).
///
/// The caller must have checked [`is_cas20_token`]; Jenner gating is the caller's too.
pub fn run_token(f: &mut Frame<'_>, input: &[u8]) -> Outcome {
    if let Err(err) = enter_call(f, input) {
        return finish(Err(err));
    }
    if !initialized_metered(f, f.self_address) {
        return finish_metered(f, Err(errors::REVERTED));
    }
    let res = if f.self_address[10] == VARIANT_ASSET {
        let (tok, ext) = (token::Token::new(f, 0), asset::AssetExt::new(f));
        asset::asset_dispatch(f, tok, ext, input)
    } else {
        let tok = token::Token::new(f, stablecoin::STABLECOIN_DECIMALS);
        stablecoin::stablecoin_dispatch(f, tok, input)
    };
    finish_metered(f, res)
}
