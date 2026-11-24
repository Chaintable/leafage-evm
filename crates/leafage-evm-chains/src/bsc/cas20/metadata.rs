//! Name, symbol, contract URI and ERC-5267 domain.
//!
//! Port of go-bsc `core/vm/cas20_metadata.go`. Metadata writes are not pause-gated
//! (BEP-702 3.6).

use alloy::primitives::{B256, U256};

use super::abi::{
    abi_string, abi_word, abi_word_array, enc_string, enc_u256, encode_tuple, read_string_arg,
};
use super::errors::{w_u256, w_u8, Cas20Err, Cas20Result};
use super::frame::Frame;
use super::permit::EIP712_VERSION;
use super::storage::{addr_key, SLOT_CONTRACT_URI, SLOT_NAME, SLOT_SYMBOL};
use super::token::{Token, PAUSE_SEIZE};
use super::{hash, sel};

impl Token {
    pub(crate) fn dispatch_metadata(
        &self,
        f: &mut Frame<'_>,
        sel: [u8; 4],
        args: &[u8],
    ) -> Option<Cas20Result<Vec<u8>>> {
        let res = if sel == sel!("contractURI()") {
            self.s
                .get_string(f, SLOT_CONTRACT_URI)
                .map(|v| enc_string(&v))
                .ok_or(Cas20Err::OutOfGas)
        } else if sel == sel!("supplyCap()") {
            Ok(enc_u256(self.s.supply_cap(f)))
        } else if sel == sel!("pausedFeatures()") {
            let features = self.paused_features(f);
            Ok(encode_tuple(vec![abi_word_array(&features)]))
        } else if sel == sel!("eip712Domain()") {
            self.eip712_domain(f).ok_or(Cas20Err::OutOfGas)
        } else if sel == sel!("updateName(string)") {
            read_string_arg(args, 0).and_then(|v| self.update_name(f, &v))
        } else if sel == sel!("updateSymbol(string)") {
            read_string_arg(args, 0).and_then(|v| self.update_symbol(f, &v))
        } else if sel == sel!("updateContractURI(string)") {
            read_string_arg(args, 0).and_then(|v| self.update_contract_uri(f, &v))
        } else {
            return None;
        };
        Some(res)
    }

    fn ensure_metadata_write(&self, f: &mut Frame<'_>) -> Cas20Result<()> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        self.ensure_role(f, hash!("METADATA_ROLE"))
    }

    fn update_name(&self, f: &mut Frame<'_>, v: &[u8]) -> Cas20Result<Vec<u8>> {
        self.ensure_metadata_write(f)?;
        if !self.s.set_string(f, SLOT_NAME, v) {
            return Err(Cas20Err::OutOfGas);
        }
        let caller = f.caller;
        if !f.add_log(
            vec![hash!("NameUpdated(address,string)"), addr_key(caller)],
            enc_string(v),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        if !f.add_log(vec![hash!("EIP712DomainChanged()")], Vec::new()) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }

    fn update_symbol(&self, f: &mut Frame<'_>, v: &[u8]) -> Cas20Result<Vec<u8>> {
        self.ensure_metadata_write(f)?;
        if !self.s.set_string(f, SLOT_SYMBOL, v) {
            return Err(Cas20Err::OutOfGas);
        }
        let caller = f.caller;
        if !f.add_log(
            vec![hash!("SymbolUpdated(address,string)"), addr_key(caller)],
            enc_string(v),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }

    fn update_contract_uri(&self, f: &mut Frame<'_>, v: &[u8]) -> Cas20Result<Vec<u8>> {
        self.ensure_metadata_write(f)?;
        if !self.s.set_string(f, SLOT_CONTRACT_URI, v) {
            return Err(Cas20Err::OutOfGas);
        }
        if !f.add_log(vec![hash!("ContractURIUpdated()")], Vec::new()) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }

    fn paused_features(&self, f: &mut Frame<'_>) -> Vec<B256> {
        let p = self.s.paused(f);
        (0..=PAUSE_SEIZE)
            .filter(|feature| (p >> *feature as usize).bit(0))
            .map(w_u8)
            .collect()
    }

    /// ERC-5267. fields 0x0f names the four members domainSeparator hashes.
    fn eip712_domain(&self, f: &mut Frame<'_>) -> Option<Vec<u8>> {
        let name = self.s.get_string(f, SLOT_NAME)?;
        let mut fields = B256::ZERO;
        fields[0] = 0x0f;
        Some(encode_tuple(vec![
            abi_word(fields),
            abi_string(&name),
            abi_string(EIP712_VERSION),
            abi_word(w_u256(U256::from(f.chain_id))),
            abi_word(addr_key(f.self_address)),
            abi_word(B256::ZERO),
            abi_word_array(&[]),
        ]))
    }
}
