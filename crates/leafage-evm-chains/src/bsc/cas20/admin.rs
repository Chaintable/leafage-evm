//! Roles, pausing, mint/burn/seize and the configurable subset.
//!
//! Port of go-bsc `core/vm/cas20_admin.go`.

use alloy::primitives::{Address, B256, U256};

use super::abi::{
    abi_word_array, enc_bool, enc_u256, encode_tuple, is_enum_word, read_address, read_u256,
    read_u64, read_u8_array, read_word,
};
use super::errors::{rev, rev_panic, w_u256, w_u64, w_u8, Cas20Err, Cas20Result, REVERTED};
use super::frame::Frame;
use super::policy::PolicyReg;
use super::storage::{
    addr_key, OFF_MINT_RECEIVER, OFF_SEIZE_EXEMPT, OFF_SEIZE_RECEIVER, OFF_TRANSFER_EXECUTOR,
    OFF_TRANSFER_RECEIVER, OFF_TRANSFER_SENDER, SLOT_MINT_POLICY, SLOT_SEIZE_POLICIES,
    SLOT_TRANSFER_POLICIES,
};
use super::token::{Token, PAUSE_BURN, PAUSE_MINT, PAUSE_SEIZE};
use super::{hash, sel};

/// DEFAULT_ADMIN is bytes32(0), so an unset role admin reads as DEFAULT_ADMIN.
pub(crate) const ROLE_DEFAULT_ADMIN: B256 = B256::ZERO;

/// `cas20NoSupplyCap`: type(uint128).max.
pub(crate) fn no_supply_cap() -> U256 {
    (U256::from(1) << 128) - U256::from(1)
}

/// `cas20PolicyLanes`: one table for policyId and updatePolicy, so the two cannot disagree.
fn policy_lane(scope: B256) -> Option<(u64, u32)> {
    let lanes = [
        (
            hash!("TRANSFER_SENDER_POLICY"),
            SLOT_TRANSFER_POLICIES,
            OFF_TRANSFER_SENDER,
        ),
        (
            hash!("TRANSFER_RECEIVER_POLICY"),
            SLOT_TRANSFER_POLICIES,
            OFF_TRANSFER_RECEIVER,
        ),
        (
            hash!("TRANSFER_EXECUTOR_POLICY"),
            SLOT_TRANSFER_POLICIES,
            OFF_TRANSFER_EXECUTOR,
        ),
        (
            hash!("MINT_RECEIVER_POLICY"),
            SLOT_MINT_POLICY,
            OFF_MINT_RECEIVER,
        ),
        (
            hash!("SEIZE_EXEMPT_POLICY"),
            SLOT_SEIZE_POLICIES,
            OFF_SEIZE_EXEMPT,
        ),
        (
            hash!("SEIZE_RECEIVER_POLICY"),
            SLOT_SEIZE_POLICIES,
            OFF_SEIZE_RECEIVER,
        ),
    ];
    lanes
        .into_iter()
        .find(|(s, _, _)| *s == scope)
        .map(|(_, slot, off)| (slot, off))
}

fn read_role_account(args: &[u8]) -> Cas20Result<(B256, Address)> {
    Ok((read_word(args, 0)?, read_address(args, 1)?))
}

impl Token {
    pub(crate) fn dispatch_admin(
        &self,
        f: &mut Frame<'_>,
        sel: [u8; 4],
        args: &[u8],
    ) -> Option<Cas20Result<Vec<u8>>> {
        let res = if sel == sel!("DEFAULT_ADMIN_ROLE()") {
            Ok(ROLE_DEFAULT_ADMIN.to_vec())
        } else if sel == sel!("MINT_ROLE()") {
            Ok(hash!("MINT_ROLE").to_vec())
        } else if sel == sel!("BURN_ROLE()") {
            Ok(hash!("BURN_ROLE").to_vec())
        } else if sel == sel!("SEIZE_ROLE()") {
            Ok(hash!("SEIZE_ROLE").to_vec())
        } else if sel == sel!("PAUSE_ROLE()") {
            Ok(hash!("PAUSE_ROLE").to_vec())
        } else if sel == sel!("UNPAUSE_ROLE()") {
            Ok(hash!("UNPAUSE_ROLE").to_vec())
        } else if sel == sel!("METADATA_ROLE()") {
            Ok(hash!("METADATA_ROLE").to_vec())
        } else if sel == sel!("hasRole(bytes32,address)") {
            (|| {
                let role = read_word(args, 0)?;
                let acct = read_address(args, 1)?;
                Ok(enc_bool(self.s.has_role(f, role, acct)))
            })()
        } else if sel == sel!("getRoleAdmin(bytes32)") {
            read_word(args, 0).map(|role| self.s.role_admin(f, role).to_vec())
        } else if sel == sel!("grantRole(bytes32,address)") {
            read_role_account(args).and_then(|(role, acct)| self.grant_role(f, role, acct))
        } else if sel == sel!("revokeRole(bytes32,address)") {
            read_role_account(args).and_then(|(role, acct)| self.revoke_role(f, role, acct))
        } else if sel == sel!("renounceRole(bytes32,address)") {
            read_role_account(args).and_then(|(role, confirm)| self.renounce_role(f, role, confirm))
        } else if sel == sel!("setRoleAdmin(bytes32,bytes32)") {
            (|| {
                let role = read_word(args, 0)?;
                let new_admin = read_word(args, 1)?;
                self.set_role_admin(f, role, new_admin)
            })()
        } else if sel == sel!("renounceLastAdmin()") {
            self.renounce_last_admin(f)
        } else if sel == sel!("isPaused(uint8)") {
            read_word(args, 0).and_then(|w| {
                if !is_enum_word(&w, PAUSE_SEIZE) {
                    return Err(REVERTED);
                }
                Ok(enc_bool(self.is_paused(f, w[31])))
            })
        } else if sel == sel!("pause(uint8[])") {
            self.set_pause(f, args, true)
        } else if sel == sel!("unpause(uint8[])") {
            self.set_pause(f, args, false)
        } else if sel == sel!("mint(address,uint256)") {
            (|| {
                let to = read_address(args, 0)?;
                let amount = read_u256(args, 1)?;
                self.mint(f, to, amount).map(|_| Vec::new())
            })()
        } else if sel == sel!("burn(uint256)") {
            read_u256(args, 0).and_then(|amount| {
                let caller = f.caller;
                self.burn(f, caller, amount).map(|_| Vec::new())
            })
        } else if sel == sel!("seizeWithMemo(address,address,uint256,bytes32)") {
            (|| {
                let from = read_address(args, 0)?;
                let to = read_address(args, 1)?;
                let amount = read_u256(args, 2)?;
                let memo = read_word(args, 3)?;
                self.seize_with_memo(f, from, to, amount, memo)?;
                Ok(enc_bool(true))
            })()
        } else if sel == sel!("updateSupplyCap(uint256)") {
            read_u256(args, 0).and_then(|cap| self.update_supply_cap(f, cap))
        } else if sel == sel!("updatePolicy(bytes32,uint64)") {
            (|| {
                let scope = read_word(args, 0)?;
                let id = read_u64(args, 1)?;
                self.update_policy(f, scope, id)
            })()
        } else if sel == sel!("TRANSFER_SENDER_POLICY()") {
            Ok(hash!("TRANSFER_SENDER_POLICY").to_vec())
        } else if sel == sel!("TRANSFER_RECEIVER_POLICY()") {
            Ok(hash!("TRANSFER_RECEIVER_POLICY").to_vec())
        } else if sel == sel!("TRANSFER_EXECUTOR_POLICY()") {
            Ok(hash!("TRANSFER_EXECUTOR_POLICY").to_vec())
        } else if sel == sel!("MINT_RECEIVER_POLICY()") {
            Ok(hash!("MINT_RECEIVER_POLICY").to_vec())
        } else if sel == sel!("SEIZE_EXEMPT_POLICY()") {
            Ok(hash!("SEIZE_EXEMPT_POLICY").to_vec())
        } else if sel == sel!("SEIZE_RECEIVER_POLICY()") {
            Ok(hash!("SEIZE_RECEIVER_POLICY").to_vec())
        } else if sel == sel!("policyId(bytes32)") {
            read_word(args, 0).and_then(|scope| match policy_lane(scope) {
                Some((slot, off)) => Ok(enc_u256(U256::from(self.s.get_packed_u64(f, slot, off)))),
                None => Err(rev("UnsupportedPolicyType(bytes32)", &[scope])),
            })
        } else {
            return None;
        };
        Some(res)
    }

    // --- RoleManaged ------------------------------------------------------------------

    fn grant_role(&self, f: &mut Frame<'_>, role: B256, account: Address) -> Cas20Result<Vec<u8>> {
        self.ensure_role_mutable(f, role)?;
        if !self.s.has_role(f, role, account) {
            self.s.set_role(f, role, account, true);
            if role == ROLE_DEFAULT_ADMIN {
                let count = self.s.admin_count(f);
                self.s.set_admin_count(f, count.wrapping_add(U256::from(1)));
            }
            let caller = f.caller;
            if !f.add_log(
                vec![
                    hash!("RoleGranted(bytes32,address,address)"),
                    role,
                    addr_key(account),
                    addr_key(caller),
                ],
                Vec::new(),
            ) {
                return Err(Cas20Err::OutOfGas);
            }
        }
        Ok(Vec::new())
    }

    fn revoke_role(&self, f: &mut Frame<'_>, role: B256, account: Address) -> Cas20Result<Vec<u8>> {
        self.ensure_role_mutable(f, role)?;
        if role == ROLE_DEFAULT_ADMIN
            && self.s.has_role(f, role, account)
            && self.s.admin_count(f) == U256::from(1)
        {
            return Err(rev("LastAdminCannotRenounce()", &[]));
        }
        if !self.remove_role(f, role, account) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }

    fn renounce_role(
        &self,
        f: &mut Frame<'_>,
        role: B256,
        confirmation: Address,
    ) -> Cas20Result<Vec<u8>> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        let caller = f.caller;
        if confirmation != caller {
            return Err(rev("AccessControlBadConfirmation()", &[]));
        }
        if role == ROLE_DEFAULT_ADMIN
            && self.s.has_role(f, role, caller)
            && self.s.admin_count(f) == U256::from(1)
        {
            return Err(rev("LastAdminCannotRenounce()", &[]));
        }
        if !self.remove_role(f, role, caller) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }

    fn renounce_last_admin(&self, f: &mut Frame<'_>) -> Cas20Result<Vec<u8>> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        let caller = f.caller;
        // A stranger is unauthorized; NotSoleAdmin is for an admin who is not the last.
        if !self.s.has_role(f, ROLE_DEFAULT_ADMIN, caller) {
            return Err(rev(
                "AccessControlUnauthorizedAccount(address,bytes32)",
                &[addr_key(caller), ROLE_DEFAULT_ADMIN],
            ));
        }
        if self.s.admin_count(f) != U256::from(1) {
            return Err(rev("NotSoleAdmin()", &[]));
        }
        self.s.set_role(f, ROLE_DEFAULT_ADMIN, caller, false);
        self.s.set_admin_count(f, U256::ZERO);
        f.admin_renounced = true;
        if !f.add_log(
            vec![
                hash!("RoleRevoked(bytes32,address,address)"),
                ROLE_DEFAULT_ADMIN,
                addr_key(caller),
                addr_key(caller),
            ],
            Vec::new(),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        if !f.add_log(
            vec![hash!("LastAdminRenounced(address)"), addr_key(caller)],
            Vec::new(),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }

    fn set_role_admin(
        &self,
        f: &mut Frame<'_>,
        role: B256,
        new_admin: B256,
    ) -> Cas20Result<Vec<u8>> {
        self.ensure_role_mutable(f, role)?;
        let prev = self.s.role_admin(f, role);
        self.s.set_role_admin(f, role, new_admin);
        if !f.add_log(
            vec![
                hash!("RoleAdminChanged(bytes32,bytes32,bytes32)"),
                role,
                prev,
                new_admin,
            ],
            Vec::new(),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }

    fn remove_role(&self, f: &mut Frame<'_>, role: B256, account: Address) -> bool {
        if !self.s.has_role(f, role, account) {
            return true;
        }
        self.s.set_role(f, role, account, false);
        if role == ROLE_DEFAULT_ADMIN {
            let count = self.s.admin_count(f);
            self.s.set_admin_count(f, count.wrapping_sub(U256::from(1)));
        }
        let caller = f.caller;
        f.add_log(
            vec![
                hash!("RoleRevoked(bytes32,address,address)"),
                role,
                addr_key(account),
                addr_key(caller),
            ],
            Vec::new(),
        )
    }

    pub(crate) fn ensure_role(&self, f: &mut Frame<'_>, role: B256) -> Cas20Result<()> {
        let caller = f.caller;
        if self.privileged || self.s.has_role(f, role, caller) {
            return Ok(());
        }
        Err(rev(
            "AccessControlUnauthorizedAccount(address,bytes32)",
            &[addr_key(caller), role],
        ))
    }

    fn ensure_role_mutable(&self, f: &mut Frame<'_>, role: B256) -> Cas20Result<()> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        let caller = f.caller;
        // The bootstrap window may configure an ownerless token, but not one this frame has
        // just renounced: that transition is permanent.
        if (!self.privileged || f.admin_renounced) && self.s.admin_count(f).is_zero() {
            let admin = self.s.role_admin(f, role);
            return Err(rev(
                "AccessControlUnauthorizedAccount(address,bytes32)",
                &[addr_key(caller), admin],
            ));
        }
        if !self.privileged {
            let admin = self.s.role_admin(f, role);
            if !self.s.has_role(f, admin, caller) {
                // go-bsc re-reads the role admin for the error argument.
                let admin = self.s.role_admin(f, role);
                return Err(rev(
                    "AccessControlUnauthorizedAccount(address,bytes32)",
                    &[addr_key(caller), admin],
                ));
            }
        }
        Ok(())
    }

    // --- Pausable ---------------------------------------------------------------------

    fn set_pause(&self, f: &mut Frame<'_>, args: &[u8], on: bool) -> Cas20Result<Vec<u8>> {
        let features = read_u8_array(args)?;
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        let role = if on {
            hash!("PAUSE_ROLE")
        } else {
            hash!("UNPAUSE_ROLE")
        };
        self.ensure_role(f, role)?;
        if features.is_empty() {
            return Err(rev("EmptyFeatureSet()", &[]));
        }
        // Read before the caller-sized loop: an unpaid read must be out-of-gas, not an empty
        // mask.
        let mut p = self.s.paused_checked(f).ok_or(Cas20Err::OutOfGas)?;
        let mut words = Vec::with_capacity(features.len());
        for feature in features {
            if feature > PAUSE_SEIZE {
                return Err(REVERTED);
            }
            words.push(w_u8(feature));
            let mask = U256::from(1) << feature as usize;
            if on {
                p |= mask;
            } else {
                p &= !mask;
            }
        }
        self.s.set_paused(f, p);
        let topic = if on {
            hash!("Paused(address,uint8[])")
        } else {
            hash!("Unpaused(address,uint8[])")
        };
        let caller = f.caller;
        if !f.add_log(
            vec![topic, addr_key(caller)],
            encode_tuple(vec![abi_word_array(&words)]),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }

    // --- Mintable / Burnable ----------------------------------------------------------

    pub(crate) fn mint(&self, f: &mut Frame<'_>, to: Address, amount: U256) -> Cas20Result<()> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(f, PAUSE_MINT) {
            return Err(rev("ContractPaused(uint8)", &[w_u8(PAUSE_MINT)]));
        }
        self.ensure_role(f, hash!("MINT_ROLE"))?;
        self.mint_core(f, to, amount)
    }

    /// `mintCore` assumes the caller has checked pause and role.
    pub(crate) fn mint_core(
        &self,
        f: &mut Frame<'_>,
        to: Address,
        amount: U256,
    ) -> Cas20Result<()> {
        if to.is_zero() {
            return Err(rev("InvalidReceiver(address)", &[addr_key(to)]));
        }
        // Enforced even during the privileged bootstrap.
        let mint_receiver = self.s.mint_receiver_policy(f);
        if !self.policy_allows(f, mint_receiver, to) {
            return Err(rev(
                "PolicyForbids(bytes32,uint64)",
                &[hash!("MINT_RECEIVER_POLICY"), w_u64(mint_receiver)],
            ));
        }
        let supply = self.s.total_supply(f);
        let new_supply = supply.wrapping_add(amount);
        if new_supply < supply {
            return Err(rev_panic(0x11));
        }
        let cap = self.s.supply_cap(f);
        if new_supply > cap {
            return Err(rev(
                "SupplyCapExceeded(uint256,uint256)",
                &[w_u256(cap), w_u256(new_supply)],
            ));
        }
        let to_slot = self.s.balance_slot(f, to);
        let bal = self.s.get_u256_at(f, to_slot);
        self.s.set_u256_at(f, to_slot, bal.wrapping_add(amount));
        self.s.set_total_supply(f, new_supply);
        if !self.emit(
            f,
            hash!("Transfer(address,address,uint256)"),
            Address::ZERO,
            to,
            amount,
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    pub(crate) fn burn(&self, f: &mut Frame<'_>, from: Address, amount: U256) -> Cas20Result<()> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(f, PAUSE_BURN) {
            return Err(rev("ContractPaused(uint8)", &[w_u8(PAUSE_BURN)]));
        }
        self.ensure_role(f, hash!("BURN_ROLE"))?;
        let from_slot = self.s.balance_slot(f, from);
        let bal = self.s.get_u256_at(f, from_slot);
        if bal < amount {
            return Err(rev(
                "InsufficientBalance(address,uint256,uint256)",
                &[addr_key(from), w_u256(bal), w_u256(amount)],
            ));
        }
        self.s.set_u256_at(f, from_slot, bal.wrapping_sub(amount));
        let supply = self.s.total_supply(f);
        if supply < amount {
            return Err(rev_panic(0x11));
        }
        self.s.set_total_supply(f, supply.wrapping_sub(amount));
        if !self.emit(
            f,
            hash!("Transfer(address,address,uint256)"),
            from,
            Address::ZERO,
            amount,
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    /// Accounts authorized by SEIZE_EXEMPT are exempt from seizure.
    fn seize_with_memo(
        &self,
        f: &mut Frame<'_>,
        from: Address,
        to: Address,
        amount: U256,
        memo: B256,
    ) -> Cas20Result<()> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(f, PAUSE_SEIZE) {
            return Err(rev("ContractPaused(uint8)", &[w_u8(PAUSE_SEIZE)]));
        }
        self.ensure_role(f, hash!("SEIZE_ROLE"))?;
        // A self-seize is a no-op that would still emit Seized; a zero source is a malformed
        // argument, not an empty balance.
        if to.is_zero() || from == to {
            return Err(rev("InvalidReceiver(address)", &[addr_key(to)]));
        }
        if from.is_zero() {
            return Err(rev("InvalidSender(address)", &[addr_key(from)]));
        }
        let (seize_exempt, seize_receiver) = self.s.seize_policies(f);
        if self.policy_allows(f, seize_exempt, from) {
            return Err(rev("AccountNotSeizable(address)", &[addr_key(from)]));
        }
        if !self.policy_allows(f, seize_receiver, to) {
            return Err(rev(
                "PolicyForbids(bytes32,uint64)",
                &[hash!("SEIZE_RECEIVER_POLICY"), w_u64(seize_receiver)],
            ));
        }
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
        if !self.emit(
            f,
            hash!("Transfer(address,address,uint256)"),
            from,
            to,
            amount,
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        if !self.emit_memo(f, memo) {
            return Err(Cas20Err::OutOfGas);
        }
        let caller = f.caller;
        if !f.add_log(
            vec![
                hash!("Seized(address,address,address,uint256)"),
                addr_key(caller),
                addr_key(from),
                addr_key(to),
            ],
            amount.to_be_bytes::<32>().to_vec(),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    // --- Configurable (subset) --------------------------------------------------------

    fn update_supply_cap(&self, f: &mut Frame<'_>, new_cap: U256) -> Cas20Result<Vec<u8>> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        self.ensure_role(f, ROLE_DEFAULT_ADMIN)?;
        let supply = self.s.total_supply(f);
        if new_cap < supply || new_cap > no_supply_cap() {
            return Err(rev(
                "InvalidSupplyCap(uint256,uint256)",
                &[w_u256(supply), w_u256(new_cap)],
            ));
        }
        let previous = self.s.supply_cap(f);
        self.s.set_supply_cap(f, new_cap);
        let caller = f.caller;
        let mut data = w_u256(previous).to_vec();
        data.extend_from_slice(w_u256(new_cap).as_slice());
        if !f.add_log(
            vec![
                hash!("SupplyCapUpdated(address,uint256,uint256)"),
                addr_key(caller),
            ],
            data,
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }

    /// Rejects a never-created id so the read path's empty-set tolerance cannot be bound on
    /// purpose.
    fn update_policy(&self, f: &mut Frame<'_>, scope: B256, id: u64) -> Cas20Result<Vec<u8>> {
        if f.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        self.ensure_role(f, ROLE_DEFAULT_ADMIN)?;
        // Scope before id: an unknown scope is reported as such whatever id accompanies it.
        let Some((slot, off)) = policy_lane(scope) else {
            return Err(rev("UnsupportedPolicyType(bytes32)", &[scope]));
        };
        if !PolicyReg::new().policy_exists(f, id) {
            return Err(rev("PolicyNotFound(uint64)", &[w_u64(id)]));
        }
        let previous = self.s.get_packed_u64(f, slot, off);
        self.s.set_packed_u64(f, slot, off, id);
        let mut data = w_u64(previous).to_vec();
        data.extend_from_slice(w_u64(id).as_slice());
        if !f.add_log(
            vec![hash!("PolicyUpdated(bytes32,uint64,uint64)"), scope],
            data,
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }
}
