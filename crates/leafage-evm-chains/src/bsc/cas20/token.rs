//! The ERC-20 core every CAS20 variant shares.
//!
//! Port of go-bsc `core/vm/cas20_token.go`.

use alloy::primitives::{Address, B256, U256};

use super::abi::{enc_bool, enc_string, enc_u256, read_address, read_u256};
use super::errors::{rev, w_u256, w_u64, w_u8, Cas20Err, Cas20Result, REVERTED};
use super::frame::Frame;
use super::policy::PolicyReg;
use super::storage::{addr_key, Store};
use super::{hash, sel};

/// Pause feature bits in the paused bitmask (slot 11).
pub(crate) const PAUSE_TRANSFER: u8 = 0;
pub(crate) const PAUSE_MINT: u8 = 1;
pub(crate) const PAUSE_BURN: u8 = 2;
pub(crate) const PAUSE_SEIZE: u8 = 3;

/// go-bsc `cas20Token`.
#[derive(Clone, Copy)]
pub(crate) struct Token {
    pub s: Store,
    pub decimals: u8,
    /// The factory's bootstrap frame. Never set here: token creation is forwarded.
    pub privileged: bool,
    /// Travels by value into announce's internal calls, so a nested announce reverts.
    pub in_announce: bool,
}

impl Token {
    pub fn new(f: &Frame<'_>, decimals: u8) -> Self {
        Self {
            s: Store::new(f.self_address),
            decimals,
            privileged: false,
            in_announce: false,
        }
    }

    pub fn dispatch(&self, f: &mut Frame<'_>, input: &[u8]) -> Cas20Result<Vec<u8>> {
        if input.len() < 4 {
            return Err(REVERTED);
        }
        let sel: [u8; 4] = input[..4].try_into().unwrap();
        let args = &input[4..];

        if sel == sel!("name()") {
            let v = self
                .s
                .get_string(f, super::storage::SLOT_NAME)
                .ok_or(Cas20Err::OutOfGas)?;
            return Ok(enc_string(&v));
        }
        if sel == sel!("symbol()") {
            let v = self
                .s
                .get_string(f, super::storage::SLOT_SYMBOL)
                .ok_or(Cas20Err::OutOfGas)?;
            return Ok(enc_string(&v));
        }
        if sel == sel!("decimals()") {
            return Ok(enc_u256(U256::from(self.decimals)));
        }
        if sel == sel!("totalSupply()") {
            return Ok(enc_u256(self.s.total_supply(f)));
        }
        if sel == sel!("balanceOf(address)") {
            let a = read_address(args, 0)?;
            return Ok(enc_u256(self.s.balance_of(f, a)));
        }
        if sel == sel!("allowance(address,address)") {
            let owner = read_address(args, 0)?;
            let spender = read_address(args, 1)?;
            return Ok(enc_u256(self.s.allowance(f, owner, spender)));
        }
        if sel == sel!("approve(address,uint256)") {
            let spender = read_address(args, 0)?;
            let amount = read_u256(args, 1)?;
            let caller = f.caller;
            return self.approve(f, caller, spender, amount);
        }
        if sel == sel!("transfer(address,uint256)") {
            let to = read_address(args, 0)?;
            let amount = read_u256(args, 1)?;
            let caller = f.caller;
            return self.transfer(f, caller, to, amount);
        }
        if sel == sel!("transferFrom(address,address,uint256)") {
            let from = read_address(args, 0)?;
            let to = read_address(args, 1)?;
            let amount = read_u256(args, 2)?;
            let caller = f.caller;
            return self.transfer_from(f, caller, from, to, amount);
        }
        if let Some(res) = self.dispatch_admin(f, sel, args) {
            return res;
        }
        if let Some(res) = self.dispatch_metadata(f, sel, args) {
            return res;
        }
        if let Some(res) = self.dispatch_permit_memo(f, sel, args) {
            return res;
        }
        if let Some(res) = self.dispatch_memo_format(f, sel, args) {
            return res;
        }
        Err(REVERTED)
    }

    // --- ERC-20 core ------------------------------------------------------------------

    pub fn approve(
        &self,
        f: &mut Frame<'_>,
        owner: Address,
        spender: Address,
        amount: U256,
    ) -> Cas20Result<Vec<u8>> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        // owner is msg.sender, so this only trips in a frame with no caller.
        if owner.is_zero() {
            return Err(rev("InvalidApprover(address)", &[addr_key(owner)]));
        }
        if spender.is_zero() {
            return Err(rev("InvalidSpender(address)", &[addr_key(spender)]));
        }
        // Neither the pause features (BEP-702 3.9) nor the policy scopes (3.8) name approve.
        self.s.set_allowance(f, owner, spender, amount);
        if !self.emit(
            f,
            hash!("Approval(address,address,uint256)"),
            owner,
            spender,
            amount,
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(enc_bool(true))
    }

    pub fn transfer(
        &self,
        f: &mut Frame<'_>,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Cas20Result<Vec<u8>> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(f, PAUSE_TRANSFER) {
            return Err(rev("ContractPaused(uint8)", &[w_u8(PAUSE_TRANSFER)]));
        }
        self.move_balance(f, from, to, amount)?;
        if !self.emit(
            f,
            hash!("Transfer(address,address,uint256)"),
            from,
            to,
            amount,
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(enc_bool(true))
    }

    pub fn transfer_from(
        &self,
        f: &mut Frame<'_>,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Cas20Result<Vec<u8>> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(f, PAUSE_TRANSFER) {
            return Err(rev("ContractPaused(uint8)", &[w_u8(PAUSE_TRANSFER)]));
        }
        // Zero-address checks before the allowance: a bad receiver is reported as such
        // whatever the allowance is. move_balance repeats them for the direct path.
        if to.is_zero() {
            return Err(rev("InvalidReceiver(address)", &[addr_key(to)]));
        }
        if from.is_zero() {
            return Err(rev("InvalidSender(address)", &[addr_key(from)]));
        }
        let slot = self.s.allowance_slot(f, from, spender);
        let allowed = self.s.get_u256_at(f, slot);
        let infinite = allowed == U256::MAX;
        if !infinite && allowed < amount {
            return Err(rev(
                "InsufficientAllowance(address,uint256,uint256)",
                &[addr_key(spender), w_u256(allowed), w_u256(amount)],
            ));
        }
        // After the allowance: an unauthorized executor with too little allowance is told
        // about the allowance.
        if !self.privileged && spender != from {
            let (_, _, executor) = self.s.transfer_policies(f);
            if !self.policy_allows(f, executor, spender) {
                return Err(rev(
                    "PolicyForbids(bytes32,uint64)",
                    &[hash!("TRANSFER_EXECUTOR_POLICY"), w_u64(executor)],
                ));
            }
        }
        if !infinite {
            self.s.set_u256_at(f, slot, allowed.wrapping_sub(amount));
        }
        self.move_balance(f, from, to, amount)?;
        if !self.emit(
            f,
            hash!("Transfer(address,address,uint256)"),
            from,
            to,
            amount,
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(enc_bool(true))
    }

    pub fn policy_allows(&self, f: &mut Frame<'_>, id: u64, account: Address) -> bool {
        if id == 0 {
            return true;
        }
        PolicyReg::new().is_authorized(f, id, account)
    }

    /// go-bsc `move`.
    pub fn move_balance(
        &self,
        f: &mut Frame<'_>,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Cas20Result<()> {
        if to.is_zero() {
            return Err(rev("InvalidReceiver(address)", &[addr_key(to)]));
        }
        if from.is_zero() {
            return Err(rev("InvalidSender(address)", &[addr_key(from)]));
        }
        if !self.privileged {
            let (sender, receiver, _) = self.s.transfer_policies(f);
            if !self.policy_allows(f, sender, from) {
                return Err(rev(
                    "PolicyForbids(bytes32,uint64)",
                    &[hash!("TRANSFER_SENDER_POLICY"), w_u64(sender)],
                ));
            }
            if !self.policy_allows(f, receiver, to) {
                return Err(rev(
                    "PolicyForbids(bytes32,uint64)",
                    &[hash!("TRANSFER_RECEIVER_POLICY"), w_u64(receiver)],
                ));
            }
        }
        // Both writes happen even when from == to or amount is zero: bytecode would perform
        // them, and skipping them would underprice the native token (BEP-702 3.14).
        let from_slot = self.s.balance_slot(f, from);
        let bal = self.s.get_u256_at(f, from_slot);
        if bal < amount {
            return Err(rev(
                "InsufficientBalance(address,uint256,uint256)",
                &[addr_key(from), w_u256(bal), w_u256(amount)],
            ));
        }
        self.s.set_u256_at(f, from_slot, bal.wrapping_sub(amount));
        let to_slot = self.s.balance_slot(f, to);
        let to_bal = self.s.get_u256_at(f, to_slot);
        self.s.set_u256_at(f, to_slot, to_bal.wrapping_add(amount));
        Ok(())
    }

    pub fn is_paused(&self, f: &mut Frame<'_>, bit: u8) -> bool {
        (self.s.paused(f) >> bit as usize).bit(0)
    }

    pub fn emit(
        &self,
        f: &mut Frame<'_>,
        topic0: B256,
        a: Address,
        b: Address,
        value: U256,
    ) -> bool {
        f.add_log(
            vec![topic0, addr_key(a), addr_key(b)],
            value.to_be_bytes::<32>().to_vec(),
        )
    }
}
