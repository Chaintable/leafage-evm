//! EIP-2612 permit and the memo-carrying transfer methods.
//!
//! Port of go-bsc `core/vm/cas20_permit.go` and `core/vm/cas20_memo.go`. The EIP-712 domain
//! is derived from the live token name, so updateName invalidates outstanding permits; there
//! is no cached separator to roll.

use alloy::primitives::{Address, Signature, B256, U256};

use super::abi::{read_address, read_strict_u8, read_u256, read_word};
use super::errors::{rev, Cas20Err, Cas20Result};
use super::frame::{gas, keccak, Frame};
use super::storage::addr_key;
use super::token::Token;
use super::{hash, sel};

pub(crate) const EIP712_VERSION: &str = "1";

/// secp256k1 group order.
const SECP256K1_N: U256 = U256::from_be_slice(&[
    0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe,
    0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c, 0xd0, 0x36, 0x41, 0x41,
]);

fn read_to_amount_memo(args: &[u8]) -> Cas20Result<(Address, U256, B256)> {
    Ok((
        read_address(args, 0)?,
        read_u256(args, 1)?,
        read_word(args, 2)?,
    ))
}

impl Token {
    pub(crate) fn dispatch_permit_memo(
        &self,
        f: &mut Frame<'_>,
        sel: [u8; 4],
        args: &[u8],
    ) -> Option<Cas20Result<Vec<u8>>> {
        let res = if sel == sel!("DOMAIN_SEPARATOR()") {
            self.domain_separator(f)
                .map(|d| d.to_vec())
                .ok_or(Cas20Err::OutOfGas)
        } else if sel == sel!("nonces(address)") {
            read_address(args, 0).map(|owner| self.s.nonce(f, owner).to_be_bytes::<32>().to_vec())
        } else if sel == sel!("permit(address,address,uint256,uint256,uint8,bytes32,bytes32)") {
            self.decode_permit(f, args)
        } else if sel == sel!("transferWithMemo(address,uint256,bytes32)") {
            read_to_amount_memo(args).and_then(|(to, amount, memo)| {
                let caller = f.caller;
                let ret = self.transfer(f, caller, to, amount)?;
                if !self.emit_memo(f, memo) {
                    return Err(Cas20Err::OutOfGas);
                }
                Ok(ret)
            })
        } else if sel == sel!("transferFromWithMemo(address,address,uint256,bytes32)") {
            (|| {
                let from = read_address(args, 0)?;
                let to = read_address(args, 1)?;
                let amount = read_u256(args, 2)?;
                let memo = read_word(args, 3)?;
                let caller = f.caller;
                let ret = self.transfer_from(f, caller, from, to, amount)?;
                if !self.emit_memo(f, memo) {
                    return Err(Cas20Err::OutOfGas);
                }
                Ok(ret)
            })()
        } else if sel == sel!("mintWithMemo(address,uint256,bytes32)") {
            read_to_amount_memo(args).and_then(|(to, amount, memo)| {
                self.mint(f, to, amount)?;
                if !self.emit_memo(f, memo) {
                    return Err(Cas20Err::OutOfGas);
                }
                Ok(Vec::new())
            })
        } else if sel == sel!("burnWithMemo(uint256,bytes32)") {
            (|| {
                let amount = read_u256(args, 0)?;
                let memo = read_word(args, 1)?;
                let caller = f.caller;
                self.burn(f, caller, amount)?;
                if !self.emit_memo(f, memo) {
                    return Err(Cas20Err::OutOfGas);
                }
                Ok(Vec::new())
            })()
        } else {
            return None;
        };
        Some(res)
    }

    /// `dispatchMemoFormat`: transfers that also declare the format of the payload behind
    /// the memo. The chain records the declaration and never verifies it.
    pub(crate) fn dispatch_memo_format(
        &self,
        f: &mut Frame<'_>,
        sel: [u8; 4],
        args: &[u8],
    ) -> Option<Cas20Result<Vec<u8>>> {
        let res = if sel == sel!("transferWithMemoFormat(address,uint256,bytes32,bytes32)") {
            (|| {
                let (to, amount, memo) = read_to_amount_memo(args)?;
                let format = read_word(args, 3)?;
                self.transfer_with_memo_format(f, format, memo, |t, f| {
                    let caller = f.caller;
                    t.transfer(f, caller, to, amount)
                })
            })()
        } else if sel == sel!("transferFromWithMemoFormat(address,address,uint256,bytes32,bytes32)")
        {
            (|| {
                let from = read_address(args, 0)?;
                let to = read_address(args, 1)?;
                let amount = read_u256(args, 2)?;
                let memo = read_word(args, 3)?;
                let format = read_word(args, 4)?;
                // Always transferFrom, so a holder moving its own balance spends its
                // self-approval exactly as through transferFromWithMemo.
                self.transfer_with_memo_format(f, format, memo, |t, f| {
                    let caller = f.caller;
                    t.transfer_from(f, caller, from, to, amount)
                })
            })()
        } else {
            return None;
        };
        Some(res)
    }

    /// A zero format is refused: the plain memo methods are how a transfer declares nothing.
    fn transfer_with_memo_format(
        &self,
        f: &mut Frame<'_>,
        format: B256,
        memo: B256,
        transfer: impl FnOnce(&Token, &mut Frame<'_>) -> Cas20Result<Vec<u8>>,
    ) -> Cas20Result<Vec<u8>> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if format.is_zero() {
            return Err(rev("InvalidFormatId()", &[]));
        }
        let ret = transfer(self, f)?;
        if !self.emit_memo(f, memo) {
            return Err(Cas20Err::OutOfGas);
        }
        let caller = f.caller;
        if !f.add_log(
            vec![
                hash!("MemoFormatDeclared(address,bytes32,bytes32)"),
                addr_key(caller),
                memo,
                format,
            ],
            Vec::new(),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(ret)
    }

    pub(crate) fn emit_memo(&self, f: &mut Frame<'_>, memo: B256) -> bool {
        let caller = f.caller;
        f.add_log(
            vec![hash!("Memo(address,bytes32)"), addr_key(caller), memo],
            Vec::new(),
        )
    }

    // --- EIP-2612 permit --------------------------------------------------------------

    fn domain_separator(&self, f: &mut Frame<'_>) -> Option<B256> {
        let name = self.s.get_string(f, super::storage::SLOT_NAME)?;
        if !f.charge_keccak(name.len())
            || !f.charge_keccak(EIP712_VERSION.len())
            || !f.charge_keccak(160)
        {
            return None;
        }
        Some(domain_separator(&name, f.chain_id, f.self_address))
    }

    fn decode_permit(&self, f: &mut Frame<'_>, args: &[u8]) -> Cas20Result<Vec<u8>> {
        let owner = read_address(args, 0)?;
        let spender = read_address(args, 1)?;
        let value = read_u256(args, 2)?;
        let deadline = read_u256(args, 3)?;
        let v = read_strict_u8(args, 4)?;
        let r = read_word(args, 5)?;
        let s = read_word(args, 6)?;
        self.permit(f, owner, spender, value, deadline, v, r, s)
    }

    #[allow(clippy::too_many_arguments)]
    fn permit(
        &self,
        f: &mut Frame<'_>,
        owner: Address,
        spender: Address,
        value: U256,
        deadline: U256,
        v: u8,
        r: B256,
        s: B256,
    ) -> Cas20Result<Vec<u8>> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if owner.is_zero() {
            return Err(rev("InvalidApprover(address)", &[addr_key(owner)]));
        }
        if deadline < U256::from(f.block_time) {
            return Err(rev("ExpiredSignature(uint256)", &[B256::from(deadline)]));
        }
        let nonce = self.s.nonce(f, owner);

        let mut struct_hash = Vec::with_capacity(192);
        struct_hash.extend_from_slice(
            hash!("Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)")
                .as_slice(),
        );
        struct_hash.extend_from_slice(addr_key(owner).as_slice());
        struct_hash.extend_from_slice(addr_key(spender).as_slice());
        struct_hash.extend_from_slice(&value.to_be_bytes::<32>());
        struct_hash.extend_from_slice(&nonce.to_be_bytes::<32>());
        struct_hash.extend_from_slice(&deadline.to_be_bytes::<32>());

        let dom = self.domain_separator(f).ok_or(Cas20Err::OutOfGas)?;
        // Charged whether or not the signature turns out to be valid, as ECRECOVER would be.
        if !f.charge_keccak(struct_hash.len())
            || !f.charge_keccak(66)
            || !f.charge_gas(gas::ECRECOVER)
        {
            return Err(Cas20Err::OutOfGas);
        }
        let digest = keccak(&[
            &[0x19, 0x01],
            dom.as_slice(),
            keccak(&[&struct_hash]).as_slice(),
        ]);

        let signer = ecrecover_address(digest, v, r, s);
        if signer != Some(owner) {
            return Err(rev(
                "InvalidSigner(address,address)",
                &[addr_key(signer.unwrap_or_default()), addr_key(owner)],
            ));
        }

        // After the signature, so a bad signature naming the zero spender is InvalidSigner.
        if spender.is_zero() {
            return Err(rev("InvalidSpender(address)", &[addr_key(spender)]));
        }
        self.s
            .set_nonce(f, owner, nonce.wrapping_add(U256::from(1)));
        self.s.set_allowance(f, owner, spender, value);
        if !self.emit(
            f,
            hash!("Approval(address,address,uint256)"),
            owner,
            spender,
            value,
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }
}

/// `cas20DomainSeparator`.
pub(crate) fn domain_separator(name: &[u8], chain_id: u64, self_address: Address) -> B256 {
    keccak(&[
        hash!("EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)")
            .as_slice(),
        keccak(&[name]).as_slice(),
        keccak(&[EIP712_VERSION.as_bytes()]).as_slice(),
        &U256::from(chain_id).to_be_bytes::<32>(),
        addr_key(self_address).as_slice(),
    ])
}

/// go-bsc `ecrecoverAddress`: EIP-2 low-s and v ∈ {27,28}; ERC-1271 contract signatures are
/// not supported. `None` when the signature is invalid or does not recover.
fn ecrecover_address(digest: B256, v: u8, r: B256, s: B256) -> Option<Address> {
    if v != 27 && v != 28 {
        return None;
    }
    let (r, s) = (U256::from_be_bytes(r.0), U256::from_be_bytes(s.0));
    // crypto.ValidateSignatureValues(v-27, r, s, homestead=true)
    let half_n = SECP256K1_N >> 1;
    if r < U256::from(1) || s < U256::from(1) || s > half_n || r >= SECP256K1_N {
        return None;
    }
    Signature::new(r, s, v == 28)
        .recover_address_from_prehash(&digest)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{address, b256};

    #[test]
    fn secp256k1_order_is_correct() {
        assert_eq!(
            B256::from(SECP256K1_N),
            b256!("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141")
        );
    }

    #[test]
    fn rejects_high_s_and_bad_v() {
        let digest = B256::repeat_byte(7);
        let r = B256::from(U256::from(1));
        let high_s = B256::from(SECP256K1_N - U256::from(1));
        assert_eq!(ecrecover_address(digest, 27, r, high_s), None);
        assert_eq!(ecrecover_address(digest, 29, r, r), None);
        let _ = address!("0x0000000000000000000000000000000000000001");
    }
}
