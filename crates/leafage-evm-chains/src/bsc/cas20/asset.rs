//! The Asset variant: decimals, the ERC-8056 scaled UI multiplier, announcements and extra
//! metadata.
//!
//! Port of go-bsc `core/vm/cas20_asset.go`. The extension lives in its own ERC-7201 namespace,
//! disjoint from the core layout.

use alloy::primitives::{B256, U256};

use super::abi::{
    abi_string, address_from_word, enc_bool, enc_string, enc_u256, encode_tuple, read_address,
    read_bytes_array, read_string_arg, read_u256, read_word, read_word_array,
};
use super::errors::{rev, rev_bytes, rev_panic, w_u256, w_u64, Cas20Err, Cas20Result, REVERTED};
use super::frame::Frame;
use super::storage::{addr_key, erc7201_root, low_u64, offset_slot, u256, Store};
use super::token::{Token, PAUSE_MINT};
use super::{hash, sel};

const NAMESPACE: &str = "bsc.cas20.asset";

const SLOT_DECIMALS: u64 = 0;
const SLOT_MULTIPLIER: u64 = 1;
/// mapping(string id => bool used)
const SLOT_ANNOUNCEMENTS: u64 = 2;
/// mapping(string key => string value)
const SLOT_EXTRA_META: u64 = 3;
/// packed: multiplier (u128) | effectiveAt (u64)
const SLOT_PENDING: u64 = 4;

/// LSB-first, as Solidity packs {uint128 multiplier; uint64 effectiveAt}.
const PENDING_WHEN_BITS: usize = 128;

/// 1.0x
fn wad() -> U256 {
    U256::from(1_000_000_000_000_000_000u64)
}

fn u128_mask() -> U256 {
    (U256::from(1) << 128) - U256::from(1)
}

/// All four ERC-8056 interfaces are implemented, so all four are advertised.
const INTERFACE_IDS: [[u8; 4]; 5] = [
    [0x01, 0xff, 0xc9, 0xa7], // IERC165
    [0xa6, 0x0b, 0xf1, 0x3d], // IScaledUIAmount
    [0x4b, 0xd2, 0x76, 0x48], // IScaledUIAmountNewUIMultiplier
    [0xd8, 0x90, 0xfd, 0x71], // IScaledUIAmountBalances
    [0x57, 0x85, 0x4f, 0xc3], // IScaledUIAmountConversion
];

fn asset_slot(offset: u64) -> B256 {
    static ROOT: std::sync::OnceLock<B256> = std::sync::OnceLock::new();
    offset_slot(*ROOT.get_or_init(|| erc7201_root(NAMESPACE)), offset)
}

/// go-bsc `assetExt`.
#[derive(Clone, Copy)]
pub(crate) struct AssetExt {
    s: Store,
}

impl AssetExt {
    pub fn new(f: &Frame<'_>) -> Self {
        Self {
            s: Store::new(f.self_address),
        }
    }

    fn decimals(&self, f: &mut Frame<'_>) -> u8 {
        low_u64(u256(self.s.get_word(f, asset_slot(SLOT_DECIMALS)))) as u8
    }

    fn multiplier(&self, f: &mut Frame<'_>) -> U256 {
        u256(self.s.get_word(f, asset_slot(SLOT_MULTIPLIER)))
    }

    fn set_multiplier(&self, f: &mut Frame<'_>, m: U256) {
        self.s
            .set_word(f, asset_slot(SLOT_MULTIPLIER), B256::from(m));
    }

    // --- ERC-8056 scheduled multiplier ---------------------------------------------
    //
    // The effective multiplier flips from the stored value to the scheduled one at a
    // timestamp, with no transaction and no event at the flip.

    fn pending(&self, f: &mut Frame<'_>) -> (U256, u64) {
        let w = u256(self.s.get_word(f, asset_slot(SLOT_PENDING)));
        (w & u128_mask(), low_u64(w >> PENDING_WHEN_BITS))
    }

    fn set_pending(&self, f: &mut Frame<'_>, mul: U256, effective_at: u64) {
        let packed = (mul & u128_mask()) | (U256::from(effective_at) << PENDING_WHEN_BITS);
        self.s
            .set_word(f, asset_slot(SLOT_PENDING), B256::from(packed));
    }

    fn clear_pending(&self, f: &mut Frame<'_>) {
        self.s.set_word(f, asset_slot(SLOT_PENDING), B256::ZERO);
    }

    /// Reads cannot write, so a matured schedule is folded into the stored multiplier by the
    /// next write that reuses the slot; otherwise that write would revalue the token.
    fn settle_matured(&self, f: &mut Frame<'_>, now: u64) {
        let (mul, at) = self.pending(f);
        if at != 0 && now >= at {
            self.set_multiplier(f, mul);
        }
    }

    fn effective_multiplier(&self, f: &mut Frame<'_>, now: u64) -> U256 {
        let (mul, at) = self.pending(f);
        if at != 0 && now >= at {
            return mul;
        }
        self.multiplier(f)
    }

    fn announcement_slot(&self, f: &mut Frame<'_>, id: &[u8]) -> B256 {
        self.s.str_map_slot(f, asset_slot(SLOT_ANNOUNCEMENTS), id)
    }

    fn announcement_used(&self, f: &mut Frame<'_>, id: &[u8]) -> Option<bool> {
        let slot = self.announcement_slot(f, id);
        self.s.get_word_checked(f, slot).map(|w| !w.is_zero())
    }

    fn mark_announcement(&self, f: &mut Frame<'_>, id: &[u8]) -> bool {
        let slot = self.announcement_slot(f, id);
        let mut one = B256::ZERO;
        one[31] = 1;
        self.s.set_word(f, slot, one)
    }

    fn extra_meta_slot(&self, f: &mut Frame<'_>, key: &[u8]) -> B256 {
        self.s.str_map_slot(f, asset_slot(SLOT_EXTRA_META), key)
    }

    fn extra_metadata(&self, f: &mut Frame<'_>, key: &[u8]) -> Option<Vec<u8>> {
        let slot = self.extra_meta_slot(f, key);
        self.s.get_string_at(f, slot)
    }

    fn set_extra_metadata(&self, f: &mut Frame<'_>, key: &[u8], value: &[u8]) -> bool {
        let slot = self.extra_meta_slot(f, key);
        self.s.set_string_at(f, slot, value)
    }
}

fn apply_multiplier(raw: U256, mul: U256) -> Cas20Result<U256> {
    let (p, overflow) = raw.overflowing_mul(mul);
    if overflow {
        return Err(rev_panic(0x11));
    }
    Ok(p / wad())
}

/// uint256 division by zero yields zero; the setters reject a zero multiplier anyway.
fn remove_multiplier(scaled: U256, mul: U256) -> Cas20Result<U256> {
    let (p, overflow) = scaled.overflowing_mul(wad());
    if overflow {
        return Err(rev_panic(0x11));
    }
    Ok(p.checked_div(mul).unwrap_or_default())
}

pub(crate) fn asset_dispatch(
    f: &mut Frame<'_>,
    tok: Token,
    ext: AssetExt,
    input: &[u8],
) -> Cas20Result<Vec<u8>> {
    if let Some(res) = dispatch_asset(f, tok, ext, input) {
        return res;
    }
    tok.dispatch(f, input)
}

fn dispatch_asset(
    f: &mut Frame<'_>,
    tok: Token,
    ext: AssetExt,
    input: &[u8],
) -> Option<Cas20Result<Vec<u8>>> {
    if input.len() < 4 {
        return None;
    }
    let sel: [u8; 4] = input[..4].try_into().unwrap();
    let args = &input[4..];
    let effective = |f: &mut Frame<'_>| {
        let now = f.block_time;
        ext.effective_multiplier(f, now)
    };

    let res = if sel == sel!("decimals()") {
        Ok(enc_u256(U256::from(ext.decimals(f))))
    } else if sel == sel!("multiplier()") || sel == sel!("uiMultiplier()") {
        Ok(enc_u256(effective(f)))
    } else if sel == sel!("WAD_PRECISION()") {
        Ok(enc_u256(wad()))
    } else if sel == sel!("MAX_UI_MULTIPLIER()") {
        Ok(enc_u256(u128_mask()))
    } else if sel == sel!("OPERATOR_ROLE()") {
        Ok(hash!("OPERATOR_ROLE").to_vec())
    } else if sel == sel!("newUIMultiplier()") {
        // With no live schedule it answers as uiMultiplier does (ERC-8056).
        let (mut mul, at) = ext.pending(f);
        if at <= f.block_time {
            mul = effective(f);
        }
        Ok(enc_u256(mul))
    } else if sel == sel!("effectiveAt()") {
        let (_, at) = ext.pending(f);
        Ok(enc_u256(U256::from(at)))
    } else if sel == sel!("totalSupplyUI()") {
        let supply = tok.s.total_supply(f);
        let mul = effective(f);
        apply_multiplier(supply, mul).map(enc_u256)
    } else if sel == sel!("supportsInterface(bytes4)") {
        read_word(args, 0).and_then(|id| {
            let want: [u8; 4] = id[..4].try_into().unwrap();
            if id[4..].iter().any(|b| *b != 0) {
                return Err(REVERTED);
            }
            Ok(enc_bool(INTERFACE_IDS.contains(&want)))
        })
    } else if sel == sel!("cancelUIMultiplierUpdate()") {
        cancel_ui_multiplier(f, tok, ext).map(|_| Vec::new())
    } else if sel == sel!("updateUIMultiplier(uint256,uint256)") {
        (|| {
            let mul = read_u256(args, 0)?;
            let at = read_u256(args, 1)?;
            update_ui_multiplier(f, tok, ext, mul, at).map(|_| Vec::new())
        })()
    } else if sel == sel!("scaledBalanceOf(address)") || sel == sel!("balanceOfUI(address)") {
        read_address(args, 0).and_then(|a| {
            let bal = tok.s.balance_of(f, a);
            let mul = effective(f);
            apply_multiplier(bal, mul).map(enc_u256)
        })
    } else if sel == sel!("toScaledBalance(uint256)") || sel == sel!("toUIAmount(uint256)") {
        read_u256(args, 0).and_then(|raw| {
            let mul = effective(f);
            apply_multiplier(raw, mul).map(enc_u256)
        })
    } else if sel == sel!("toRawBalance(uint256)") || sel == sel!("fromUIAmount(uint256)") {
        read_u256(args, 0).and_then(|scaled| {
            let mul = effective(f);
            remove_multiplier(scaled, mul).map(enc_u256)
        })
    } else if sel == sel!("updateMultiplier(uint256)") {
        read_u256(args, 0).and_then(|m| update_multiplier(f, tok, ext, m).map(|_| Vec::new()))
    } else if sel == sel!("batchMint(address[],uint256[])") {
        batch_mint(f, tok, args).map(|_| Vec::new())
    } else if sel == sel!("isAnnouncementIdUsed(string)") {
        read_string_arg(args, 0).and_then(|id| {
            ext.announcement_used(f, &id)
                .map(enc_bool)
                .ok_or(Cas20Err::OutOfGas)
        })
    } else if sel == sel!("announce(bytes[],string,string,string)") {
        announce(f, tok, ext, args).map(|_| Vec::new())
    } else if sel == sel!("extraMetadata(string)") {
        read_string_arg(args, 0).and_then(|key| {
            ext.extra_metadata(f, &key)
                .map(|v| enc_string(&v))
                .ok_or(Cas20Err::OutOfGas)
        })
    } else if sel == sel!("updateExtraMetadata(string,string)") {
        (|| {
            let key = read_string_arg(args, 0)?;
            let value = read_string_arg(args, 1)?;
            update_extra_metadata(f, tok, ext, &key, &value).map(|_| Vec::new())
        })()
    } else {
        return None;
    };
    Some(res)
}

fn update_extra_metadata(
    f: &mut Frame<'_>,
    tok: Token,
    ext: AssetExt,
    key: &[u8],
    value: &[u8],
) -> Cas20Result<()> {
    if f.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    tok.ensure_role(f, hash!("METADATA_ROLE"))?;
    if key.is_empty() {
        return Err(rev("InvalidMetadataKey()", &[]));
    }
    if !ext.set_extra_metadata(f, key, value) {
        return Err(Cas20Err::OutOfGas);
    }
    if !f.add_log(
        vec![hash!("ExtraMetadataUpdated(string,string)")],
        encode_tuple(vec![abi_string(key), abi_string(value)]),
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn announce(f: &mut Frame<'_>, mut tok: Token, ext: AssetExt, args: &[u8]) -> Cas20Result<()> {
    let calls = read_bytes_array(args, 0)?;
    let id = read_string_arg(args, 1)?;
    let description = read_string_arg(args, 2)?;
    let uri = read_string_arg(args, 3)?;
    if f.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    if tok.in_announce {
        return Err(rev("AnnouncementInProgress()", &[]));
    }
    tok.ensure_role(f, hash!("OPERATOR_ROLE"))?;
    let used = ext.announcement_used(f, &id).ok_or(Cas20Err::OutOfGas)?;
    if used {
        return Err(rev_bytes("AnnouncementIdAlreadyUsed(string)", &id));
    }
    if !ext.mark_announcement(f, &id) {
        return Err(Cas20Err::OutOfGas);
    }
    let caller = f.caller;
    if !f.add_log(
        vec![
            hash!("Announcement(address,string,string,string)"),
            addr_key(caller),
        ],
        encode_tuple(vec![
            abi_string(&id),
            abi_string(&description),
            abi_string(&uri),
        ]),
    ) {
        return Err(Cas20Err::OutOfGas);
    }

    tok.in_announce = true;
    for c in &calls {
        if !f.charge_internal_dispatch(c) {
            return Err(Cas20Err::OutOfGas);
        }
        if c.len() < 4 {
            return Err(rev_bytes("InternalCallMalformed(bytes)", c));
        }
        if asset_dispatch(f, tok, ext, c).is_err() {
            return Err(rev_bytes("InternalCallFailed(bytes)", c));
        }
    }
    if !f.add_log(
        vec![hash!("EndAnnouncement(string)")],
        encode_tuple(vec![abi_string(&id)]),
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

/// Check order: role, value, both timestamp bounds, then a live schedule.
fn update_ui_multiplier(
    f: &mut Frame<'_>,
    tok: Token,
    ext: AssetExt,
    new_mul: U256,
    at: U256,
) -> Cas20Result<()> {
    if f.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    tok.ensure_role(f, hash!("OPERATOR_ROLE"))?;
    if new_mul.is_zero() || new_mul > u128_mask() {
        return Err(rev("InvalidMultiplier()", &[]));
    }
    let now = f.block_time;
    if at <= U256::from(now) {
        return Err(rev("EffectiveAtInPast(uint256)", &[w_u256(at)]));
    }
    if at > U256::from(u64::MAX) {
        return Err(rev("EffectiveAtTooFar(uint256)", &[w_u256(at)]));
    }
    // Only a live schedule blocks a new one; a matured record is stale state, not a
    // commitment.
    let (_, existing) = ext.pending(f);
    if existing > now {
        return Err(rev("UIMultiplierUpdateExists(uint256)", &[w_u64(existing)]));
    }
    ext.settle_matured(f, now);
    let previous = ext.multiplier(f);
    ext.set_pending(f, new_mul, low_u64(at));
    let mut data = w_u256(previous).to_vec();
    data.extend_from_slice(w_u256(new_mul).as_slice());
    data.extend_from_slice(w_u256(at).as_slice());
    if !f.add_log(
        vec![hash!("UIMultiplierUpdated(uint256,uint256,uint256)")],
        data,
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

/// A matured schedule is already in force, so there is nothing to withdraw.
fn cancel_ui_multiplier(f: &mut Frame<'_>, tok: Token, ext: AssetExt) -> Cas20Result<()> {
    if f.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    tok.ensure_role(f, hash!("OPERATOR_ROLE"))?;
    let (mul, at) = ext.pending(f);
    if at <= f.block_time {
        return Err(rev("UIMultiplierUpdateDoesNotExist()", &[]));
    }
    ext.clear_pending(f);
    let mut data = w_u256(mul).to_vec();
    data.extend_from_slice(w_u64(at).as_slice());
    if !f.add_log(
        vec![hash!("UIMultiplierUpdateCancelled(uint256,uint256)")],
        data,
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn update_multiplier(
    f: &mut Frame<'_>,
    tok: Token,
    ext: AssetExt,
    new_mul: U256,
) -> Cas20Result<()> {
    if f.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    tok.ensure_role(f, hash!("OPERATOR_ROLE"))?;
    if new_mul.is_zero() || new_mul > u128_mask() {
        return Err(rev("InvalidMultiplier()", &[]));
    }
    // The instant setter is the failsafe and overrides any schedule: a live one is withdrawn
    // loudly, a matured one is stale state and goes quietly.
    let now = f.block_time;
    let previous = ext.effective_multiplier(f, now);
    let (pending_mul, at) = ext.pending(f);
    if at != 0 {
        ext.clear_pending(f);
        if at > now {
            let mut data = w_u256(pending_mul).to_vec();
            data.extend_from_slice(w_u64(at).as_slice());
            if !f.add_log(
                vec![hash!("UIMultiplierUpdateCancelled(uint256,uint256)")],
                data,
            ) {
                return Err(Cas20Err::OutOfGas);
            }
        }
    }
    ext.set_multiplier(f, new_mul);
    if !f.add_log(
        vec![hash!("MultiplierUpdated(uint256)")],
        new_mul.to_be_bytes::<32>().to_vec(),
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    // Emitted by both setters so one stream carries every change.
    let mut data = w_u256(previous).to_vec();
    data.extend_from_slice(w_u256(new_mul).as_slice());
    data.extend_from_slice(w_u256(U256::from(now)).as_slice());
    if !f.add_log(
        vec![hash!("UIMultiplierUpdated(uint256,uint256,uint256)")],
        data,
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn batch_mint(f: &mut Frame<'_>, tok: Token, args: &[u8]) -> Cas20Result<()> {
    let recipients = read_word_array(args, 0)?;
    let amounts = read_word_array(args, 1)?;
    if f.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    if tok.is_paused(f, PAUSE_MINT) {
        return Err(rev(
            "ContractPaused(uint8)",
            &[super::errors::w_u8(PAUSE_MINT)],
        ));
    }
    tok.ensure_role(f, hash!("MINT_ROLE"))?;
    if recipients.len() != amounts.len() {
        return Err(rev(
            "LengthMismatch(uint256,uint256)",
            &[w_u64(recipients.len() as u64), w_u64(amounts.len() as u64)],
        ));
    }
    if recipients.is_empty() {
        return Err(rev("EmptyBatch()", &[]));
    }
    for (recipient, amount) in recipients.iter().zip(&amounts) {
        // charge_gas only marks the frame, so without this an exhausted batch would still
        // run to completion before being discarded.
        if f.out_of_gas() {
            return Err(Cas20Err::OutOfGas);
        }
        let to = address_from_word(recipient).ok_or(REVERTED)?;
        tok.mint_core(f, to, u256(*amount))?;
    }
    Ok(())
}
