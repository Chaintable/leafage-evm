//! The PolicyRegistry read path tokens consult.
//!
//! Port of the token-facing part of go-bsc `core/vm/cas20_policy.go`: `isAuthorized`,
//! `policyExists` and the composite children they walk. A token consulting the registry pays
//! as if the slot were its own, with no account-access surcharge (BEP-702 3.14). The
//! registry's own dispatch (policy creation and administration) is forwarded, see
//! [`super::is_forwarded`].

use alloy::primitives::{address, Address, B256, U256};

use super::frame::Frame;
use super::storage::{addr_key, erc7201_root, low_u64, offset_slot, u256, Store};

/// PolicyRegistry precompile.
pub const POLICY_REGISTRY: Address = address!("0x7020000000000000000000000000000000000002");

const NAMESPACE: &str = "bsc.policy_registry";

const POLICY_ALLOWLIST: u8 = 1;
const POLICY_UNION: u8 = 2;
const POLICY_INTERSECT: u8 = 3;

const COMPOSITE_MAX_CHILDREN: u64 = 4;

/// Blocklist type, empty: allows all.
const POLICY_ALWAYS_ALLOW: u64 = 0;
/// Allowlist type, empty: blocks all.
const POLICY_ALWAYS_BLOCK: u64 = (1 << 56) | 1;

const SLOT_POLICIES: u64 = 0;
const SLOT_MEMBERS: u64 = 1;
const SLOT_CHILDREN: u64 = 4;

fn pol_slot(offset: u64) -> B256 {
    static ROOT: std::sync::OnceLock<B256> = std::sync::OnceLock::new();
    offset_slot(*ROOT.get_or_init(|| erc7201_root(NAMESPACE)), offset)
}

fn id_type(id: u64) -> u8 {
    (id >> 56) as u8
}

fn id_well_formed(id: u64) -> bool {
    id_type(id) <= POLICY_INTERSECT
}

fn is_sentinel(id: u64) -> bool {
    id == POLICY_ALWAYS_ALLOW || id == POLICY_ALWAYS_BLOCK
}

fn id_key(id: u64) -> B256 {
    B256::from(U256::from(id))
}

/// go-bsc `policyReg`: a gas-metered view over the registry's storage.
pub(crate) struct PolicyReg {
    s: Store,
}

impl PolicyReg {
    pub fn new() -> Self {
        Self {
            s: Store::new(POLICY_REGISTRY),
        }
    }

    fn policy_word(&self, f: &mut Frame<'_>, id: u64) -> B256 {
        let slot = self.s.map_slot(f, pol_slot(SLOT_POLICIES), id_key(id));
        self.s.get_word(f, slot)
    }

    /// Bit 255 of the policy word marks existence.
    fn exists(&self, f: &mut Frame<'_>, id: u64) -> bool {
        self.policy_word(f, id)[0] & 0x80 != 0
    }

    fn member(&self, f: &mut Frame<'_>, id: u64, account: Address) -> bool {
        let inner = self.s.map_slot(f, pol_slot(SLOT_MEMBERS), id_key(id));
        let slot = self.s.map_slot(f, inner, addr_key(account));
        !self.s.get_word(f, slot).is_zero()
    }

    /// `isAuthorized` never reverts: it sits on every transfer's path.
    pub fn is_authorized(&self, f: &mut Frame<'_>, id: u64, account: Address) -> bool {
        if !id_well_formed(id) {
            return false;
        }
        match id {
            POLICY_ALWAYS_ALLOW => return true,
            POLICY_ALWAYS_BLOCK => return false,
            _ => {}
        }
        match id_type(id) {
            POLICY_UNION => {
                for child in self.children(f, id) {
                    if self.is_authorized(f, child, account) {
                        return true;
                    }
                }
                return false;
            }
            POLICY_INTERSECT => {
                for child in self.children(f, id) {
                    if !self.is_authorized(f, child, account) {
                        return false;
                    }
                }
                return true;
            }
            _ => {}
        }
        let member = self.member(f, id, account);
        if id_type(id) == POLICY_ALLOWLIST {
            member
        } else {
            !member
        }
    }

    /// Four uint64 lanes per word, LSB-first as Solidity packs them.
    fn children(&self, f: &mut Frame<'_>, id: u64) -> Vec<u64> {
        let slot = self.s.map_slot(f, pol_slot(SLOT_CHILDREN), id_key(id));
        let n = low_u64(u256(self.s.get_word(f, slot)));
        if n == 0 || n > COMPOSITE_MAX_CHILDREN {
            return Vec::new();
        }
        let base = self.s.string_data_root(f, slot);
        let mut out = Vec::with_capacity(n as usize);
        for i in 0..n {
            if f.out_of_gas() {
                return Vec::new();
            }
            let w = self
                .s
                .get_word(f, B256::from(base.wrapping_add(U256::from(i / 4))));
            let lane = ((i % 4) * 8) as usize;
            out.push(low_u64(u256(w) >> (lane * 8)));
        }
        out
    }

    pub fn policy_exists(&self, f: &mut Frame<'_>, id: u64) -> bool {
        if !id_well_formed(id) {
            return false;
        }
        if is_sentinel(id) {
            return true;
        }
        self.exists(f, id)
    }
}
