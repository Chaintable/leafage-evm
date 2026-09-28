//! Storage-access goldens transcribed from Base reth v1.4.2.
//!
//! Base's `tests/b20_{asset,stablecoin}_v{1,2}_golden.rs::golden_gas_footprints` pin, per
//! operation, how many SLOADs, SSTOREs and metered KECCAK256s the token performs. Those counts
//! are what drive real gas, independently of any gas schedule, so matching them is the
//! strongest cross-check available short of executing on a Base node. The cases below mirror
//! Base's fixtures one-for-one — same setup, same calldata, same factory privilege — and the
//! expected tuples are copied verbatim.
//!
//! As in Base, only the token's own storage is counted: Base's goldens stub the policy
//! registry with `FakePolicyAccounting`, so registry reads are not part of the footprint.
//! Here the registry is real storage; the port simply does not count accesses to it.

use std::collections::HashMap;

use alloy::primitives::{address, b256, keccak256, Address, LogData, B256, U256};
use alloy::signers::{local::PrivateKeySigner, SignerSync};
use alloy::sol_types::{SolCall, SolValue};

use super::abi::{IB20Asset, IB20};
use super::dispatch::run;
use super::error::{B20Error, Result};
use super::ids;
use super::layout::{field_slot, mapping_slot, B20Store, PolicySlot, ROOT_B20, WAD};
use super::ops::B20_MAX_SUPPLY_CAP;
use super::permit::RECOVER_GAS;
use super::policy::{POLICY_REGISTRY, ROOT_POLICY_REGISTRY};
use super::port::B20Port;
use super::version::B20Version;

const ASSET: Address = address!("0xb200000000000000000000000000000000000a55");
const STABLECOIN: Address = address!("0xb200000000000000000001000000000000005cd1");
const ADMIN: Address = address!("0x00000000000000000000000000000000000000ad");
const ALICE: Address = address!("0x000000000000000000000000000000000000a11c");
const BOB: Address = address!("0x0000000000000000000000000000000000000b0b");
const CAROL: Address = address!("0x00000000000000000000000000000000000ca201");
const MEMO: B256 = b256!("00000000000000000000000000000000000000000000000000000000000000aa");
/// ALLOWLIST, counter 7 — no members, so it authorizes nobody.
const POLICY_ID: u64 = (1u64 << 56) | 7;
/// ALLOWLIST, counter 8.
const POLICY_ID_2: u64 = (1u64 << 56) | 8;
const CHAIN_ID: u64 = 8453;
/// Anvil's first dev key, as Base's `anvil_owner()`.
const ANVIL_KEY: B256 = b256!("ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80");

/// In-memory port that counts the token's storage accesses and metered keccaks.
struct CountingPort {
    token: Address,
    storage: HashMap<(Address, U256), U256>,
    logs: Vec<LogData>,
    sloads: u64,
    sstores: u64,
    keccaks: u64,
    /// Gas charged through `deduct_gas` (metered keccak + signer recovery), for metering pins.
    deducted: u64,
    caller: Address,
    timestamp: U256,
}

impl CountingPort {
    fn new(token: Address) -> Self {
        Self {
            token,
            storage: HashMap::new(),
            logs: Vec::new(),
            sloads: 0,
            sstores: 0,
            keccaks: 0,
            deducted: 0,
            caller: ADMIN,
            timestamp: U256::ZERO,
        }
    }

    fn reset_counters(&mut self) {
        self.sloads = 0;
        self.sstores = 0;
        self.keccaks = 0;
        self.deducted = 0;
        self.logs.clear();
    }

    fn footprint(&self) -> (u64, u64, u64) {
        (self.sloads, self.sstores, self.keccaks)
    }
}

impl B20Port for CountingPort {
    fn sload(&mut self, address: Address, key: U256) -> Result<U256> {
        if address == self.token {
            self.sloads += 1;
        }
        Ok(self
            .storage
            .get(&(address, key))
            .copied()
            .unwrap_or_default())
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> Result<()> {
        if address == self.token {
            self.sstores += 1;
        }
        self.storage.insert((address, key), value);
        Ok(())
    }

    fn emit_event(&mut self, _address: Address, log: LogData) -> Result<()> {
        self.logs.push(log);
        Ok(())
    }

    fn has_code(&mut self, _address: Address) -> Result<bool> {
        Ok(true)
    }

    fn deduct_gas(&mut self, gas: u64) -> Result<()> {
        if gas != RECOVER_GAS {
            self.keccaks += 1;
        }
        self.deducted += gas;
        Ok(())
    }

    fn caller(&self) -> Address {
        self.caller
    }
    fn call_value(&self) -> U256 {
        U256::ZERO
    }
    fn chain_id(&self) -> u64 {
        CHAIN_ID
    }
    fn timestamp(&self) -> U256 {
        self.timestamp
    }
    fn is_static(&self) -> bool {
        false
    }
}

/// Token storage fixture. Setup writes go through a Beryl store, so they never trigger
/// Cobalt's read-before-write, and counters are reset before the measured call.
struct Fixture {
    port: CountingPort,
    is_asset: bool,
}

impl Fixture {
    /// An initialized token, as Base's `fresh()`: name, symbol, max supply cap; the asset
    /// multiplier slot is left physically zero.
    fn fresh(is_asset: bool) -> Self {
        let token = if is_asset { ASSET } else { STABLECOIN };
        let mut fixture = Self {
            port: CountingPort::new(token),
            is_asset,
        };
        fixture.with_store(|s| {
            s.set_name(if is_asset { "Base Asset" } else { "Base USD" })
                .unwrap();
            s.set_symbol(if is_asset { "bASSET" } else { "bUSD" })
                .unwrap();
            s.set_supply_cap(B20_MAX_SUPPLY_CAP).unwrap();
        });
        fixture
    }

    fn with_store<R>(&mut self, f: impl FnOnce(&mut B20Store<'_, CountingPort>) -> R) -> R {
        let token = self.port.token;
        let mut store = B20Store::new(&mut self.port, token, self.is_asset, B20Version::V1);
        f(&mut store)
    }

    fn fund(&mut self, who: Address, amount: u64) {
        self.with_store(|s| {
            let balance = s.balance_of(who).unwrap();
            s.set_balance(who, balance + U256::from(amount)).unwrap();
            let supply = s.total_supply().unwrap();
            s.set_total_supply(supply + U256::from(amount)).unwrap();
        });
    }

    fn give_role(&mut self, role: B256, who: Address) {
        self.with_store(|s| {
            s.set_role(role, who, true).unwrap();
            if role == ids::DEFAULT_ADMIN_ROLE {
                let count = s.admin_count().unwrap();
                s.set_admin_count(count + U256::ONE).unwrap();
            }
        });
    }

    fn set_policy(&mut self, slot: PolicySlot, id: u64) {
        self.with_store(|s| s.set_policy_id(slot, id).unwrap());
    }

    fn set_allowance(&mut self, owner: Address, spender: Address, amount: u64) {
        self.with_store(|s| s.set_allowance(owner, spender, U256::from(amount)).unwrap());
    }

    fn set_pending(&mut self, multiplier: u128, effective_at: u64) {
        self.with_store(|s| s.set_pending(multiplier, effective_at).unwrap());
    }

    /// Marks `policy_id` as created in the registry.
    fn registry_create(&mut self, policy_id: u64) {
        let slot = mapping_slot(ROOT_POLICY_REGISTRY, u64_word(policy_id));
        self.port
            .storage
            .insert((POLICY_REGISTRY, slot), U256::ONE << 255);
    }

    /// Adds `who` to `policy_id`'s member set in the registry.
    fn registry_allow(&mut self, policy_id: u64, who: Address) {
        let outer = mapping_slot(ROOT_POLICY_REGISTRY + U256::from(1u64), u64_word(policy_id));
        let slot = mapping_slot(outer, who.into_word());
        self.port.storage.insert((POLICY_REGISTRY, slot), U256::ONE);
    }

    /// Writes a composite's child list at registry offset 4 (length word + packed data).
    fn registry_children(&mut self, policy_id: u64, children: &[u64]) {
        let len_slot = mapping_slot(ROOT_POLICY_REGISTRY + U256::from(4u64), u64_word(policy_id));
        self.port
            .storage
            .insert((POLICY_REGISTRY, len_slot), U256::from(children.len()));
        let data = U256::from_be_bytes(keccak256(len_slot.to_be_bytes::<32>()).0);
        for (word, chunk) in children.chunks(4).enumerate() {
            let mut value = U256::ZERO;
            for (lane, child) in chunk.iter().enumerate() {
                value |= U256::from(*child) << (lane * 64);
            }
            self.port
                .storage
                .insert((POLICY_REGISTRY, data + U256::from(word)), value);
        }
    }

    /// Runs `calldata` as `caller` against `version`, returning the output or revert bytes.
    fn call(
        &mut self,
        version: B20Version,
        caller: Address,
        calldata: &[u8],
        privileged: bool,
    ) -> core::result::Result<alloy::primitives::Bytes, B20Error> {
        self.port.caller = caller;
        self.port.reset_counters();
        let token = self.port.token;
        let mut store = B20Store::new(&mut self.port, token, self.is_asset, version);
        run(&mut store, calldata, privileged)
    }

    fn slot(&self, slot: U256) -> U256 {
        self.port
            .storage
            .get(&(self.port.token, slot))
            .copied()
            .unwrap_or_default()
    }
}

fn u64_word(value: u64) -> B256 {
    B256::from(U256::from(value))
}

/// Base's `gas()`: a privileged success-path footprint.
fn gas(
    version: B20Version,
    is_asset: bool,
    setup: impl FnOnce(&mut Fixture),
    caller: Address,
    calldata: Vec<u8>,
) -> (u64, u64, u64) {
    let mut fixture = Fixture::fresh(is_asset);
    setup(&mut fixture);
    fixture
        .call(version, caller, &calldata, true)
        .expect("gas-footprint op must succeed");
    fixture.port.footprint()
}

/// Base's `gas_reverting()`: a privileged revert-path footprint.
fn gas_reverting(
    version: B20Version,
    is_asset: bool,
    setup: impl FnOnce(&mut Fixture),
    caller: Address,
    calldata: Vec<u8>,
) -> (u64, u64, u64) {
    let mut fixture = Fixture::fresh(is_asset);
    setup(&mut fixture);
    fixture
        .call(version, caller, &calldata, true)
        .expect_err("gas-footprint op must revert");
    fixture.port.footprint()
}

fn u(value: u64) -> U256 {
    U256::from(value)
}

/// A permit for `owner -> BOB` signed by the anvil key over the token's domain separator.
fn signed_permit(version: B20Version, is_asset: bool) -> IB20::permitCall {
    let signer = PrivateKeySigner::from_bytes(&ANVIL_KEY).unwrap();
    let owner = signer.address();
    let mut fixture = Fixture::fresh(is_asset);
    let domain = {
        let out = fixture
            .call(
                version,
                owner,
                &IB20::DOMAIN_SEPARATORCall {}.abi_encode(),
                false,
            )
            .unwrap();
        B256::from_slice(&out)
    };
    let typehash = keccak256(
        "Permit(address owner,address spender,uint256 value,uint256 nonce,uint256 deadline)",
    );
    let struct_hash = keccak256((typehash, owner, BOB, u(500), U256::ZERO, U256::MAX).abi_encode());
    let mut buf = vec![0x19, 0x01];
    buf.extend_from_slice(domain.as_slice());
    buf.extend_from_slice(struct_hash.as_slice());
    let sig = signer.sign_hash_sync(&keccak256(buf)).unwrap();
    IB20::permitCall {
        owner,
        spender: BOB,
        value: u(500),
        deadline: U256::MAX,
        v: 27 + sig.v() as u8,
        r: sig.r().into(),
        s: sig.s().into(),
    }
}

fn anvil_owner() -> Address {
    PrivateKeySigner::from_bytes(&ANVIL_KEY).unwrap().address()
}

/// The footprints every B20 variant shares, for `version`.
fn common_footprints(version: B20Version, is_asset: bool) -> Vec<(&'static str, (u64, u64, u64))> {
    let g = |setup: &dyn Fn(&mut Fixture), caller, calldata: Vec<u8>| {
        gas(version, is_asset, |f| setup(f), caller, calldata)
    };
    vec![
        (
            "transfer",
            g(
                &|f| f.fund(ALICE, 100),
                ALICE,
                IB20::transferCall {
                    to: BOB,
                    amount: u(30),
                }
                .abi_encode(),
            ),
        ),
        (
            "transfer_from",
            g(
                &|f| {
                    f.fund(ALICE, 100);
                    f.set_allowance(ALICE, BOB, 40);
                },
                BOB,
                IB20::transferFromCall {
                    from: ALICE,
                    to: BOB,
                    amount: u(30),
                }
                .abi_encode(),
            ),
        ),
        (
            "approve",
            g(
                &|_| {},
                ALICE,
                IB20::approveCall {
                    spender: BOB,
                    amount: u(50),
                }
                .abi_encode(),
            ),
        ),
        (
            "mint",
            g(
                &|_| {},
                ADMIN,
                IB20::mintCall {
                    to: BOB,
                    amount: u(100),
                }
                .abi_encode(),
            ),
        ),
        (
            "burn",
            g(
                &|f| {
                    f.fund(ALICE, 100);
                    f.give_role(ids::BURN_ROLE, ALICE);
                },
                ALICE,
                IB20::burnCall { amount: u(40) }.abi_encode(),
            ),
        ),
        (
            "burn_blocked",
            g(
                &|f| {
                    f.fund(ALICE, 100);
                    f.set_policy(PolicySlot::TransferSender, POLICY_ID);
                },
                ADMIN,
                IB20::burnBlockedCall {
                    from: ALICE,
                    amount: u(40),
                }
                .abi_encode(),
            ),
        ),
        (
            "pause",
            g(
                &|_| {},
                ADMIN,
                IB20::pauseCall {
                    features: vec![IB20::PausableFeature::MINT],
                }
                .abi_encode(),
            ),
        ),
        (
            "unpause",
            g(
                &|_| {},
                ADMIN,
                IB20::unpauseCall {
                    features: vec![IB20::PausableFeature::MINT],
                }
                .abi_encode(),
            ),
        ),
        (
            "update_supply_cap",
            g(
                &|_| {},
                ADMIN,
                IB20::updateSupplyCapCall {
                    newSupplyCap: u(1_000),
                }
                .abi_encode(),
            ),
        ),
        (
            "update_name",
            g(
                &|_| {},
                ADMIN,
                IB20::updateNameCall {
                    newName: "New Name".into(),
                }
                .abi_encode(),
            ),
        ),
        (
            "update_symbol",
            g(
                &|_| {},
                ADMIN,
                IB20::updateSymbolCall {
                    newSymbol: "USDX".into(),
                }
                .abi_encode(),
            ),
        ),
        (
            "update_contract_uri",
            g(
                &|_| {},
                ADMIN,
                IB20::updateContractURICall {
                    newURI: "ipfs://x".into(),
                }
                .abi_encode(),
            ),
        ),
        (
            "grant_role",
            g(
                &|_| {},
                ADMIN,
                IB20::grantRoleCall {
                    role: ids::MINT_ROLE,
                    account: ALICE,
                }
                .abi_encode(),
            ),
        ),
        (
            "revoke_role",
            g(
                &|f| f.give_role(ids::MINT_ROLE, ALICE),
                ADMIN,
                IB20::revokeRoleCall {
                    role: ids::MINT_ROLE,
                    account: ALICE,
                }
                .abi_encode(),
            ),
        ),
        (
            "set_role_admin",
            g(
                &|_| {},
                ADMIN,
                IB20::setRoleAdminCall {
                    role: ids::MINT_ROLE,
                    newAdminRole: ids::METADATA_ROLE,
                }
                .abi_encode(),
            ),
        ),
        (
            "update_policy",
            g(
                &|f| f.registry_create(7),
                ADMIN,
                IB20::updatePolicyCall {
                    policyScope: ids::TRANSFER_SENDER_POLICY,
                    newPolicyId: 7,
                }
                .abi_encode(),
            ),
        ),
        (
            "permit",
            g(
                &|_| {},
                anvil_owner(),
                signed_permit(version, is_asset).abi_encode(),
            ),
        ),
    ]
}

fn assert_footprints(
    label: &str,
    actual: &[(&str, (u64, u64, u64))],
    expected: &[(&str, (u64, u64, u64))],
) {
    let actual: HashMap<_, _> = actual.iter().copied().collect();
    for (name, want) in expected {
        let got = actual
            .get(name)
            .unwrap_or_else(|| panic!("{label}: missing case {name}"));
        assert_eq!(got, want, "{label}: {name} (sload, sstore, keccak256)");
    }
}

/// `tests/b20_asset_v1_golden.rs::golden_gas_footprints`.
#[test]
fn asset_v1_footprints_match_base() {
    let v = B20Version::V1;
    let mut actual = common_footprints(v, true);
    actual.extend([
        (
            "update_multiplier",
            gas(
                v,
                true,
                |_| {},
                ADMIN,
                IB20Asset::updateMultiplierCall {
                    newMultiplier: WAD * u(2),
                }
                .abi_encode(),
            ),
        ),
        (
            "batch_mint",
            gas(
                v,
                true,
                |_| {},
                ADMIN,
                IB20Asset::batchMintCall {
                    recipients: vec![BOB, CAROL],
                    amounts: vec![u(30), u(70)],
                }
                .abi_encode(),
            ),
        ),
        (
            "announce",
            gas(
                v,
                true,
                |_| {},
                ADMIN,
                IB20Asset::announceCall {
                    internalCalls: vec![],
                    id: "gas".into(),
                    description: String::new(),
                    uri: String::new(),
                }
                .abi_encode(),
            ),
        ),
        (
            "update_extra_metadata",
            gas(
                v,
                true,
                |_| {},
                ADMIN,
                IB20Asset::updateExtraMetadataCall {
                    key: "category".into(),
                    value: "commodity".into(),
                }
                .abi_encode(),
            ),
        ),
    ]);
    assert_footprints(
        "asset V1",
        &actual,
        &[
            ("transfer", (3, 2, 0)),
            ("transfer_from", (4, 3, 0)),
            ("approve", (0, 1, 0)),
            ("mint", (5, 2, 0)),
            ("burn", (4, 2, 0)),
            ("burn_blocked", (4, 2, 0)),
            ("pause", (1, 1, 0)),
            ("unpause", (1, 1, 0)),
            ("update_supply_cap", (2, 1, 0)),
            ("update_name", (0, 1, 0)),
            ("update_symbol", (0, 1, 0)),
            ("update_contract_uri", (0, 1, 0)),
            ("grant_role", (1, 1, 0)),
            ("revoke_role", (1, 1, 0)),
            ("set_role_admin", (1, 1, 0)),
            ("update_policy", (2, 1, 0)),
            ("update_multiplier", (0, 1, 0)),
            ("batch_mint", (11, 4, 0)),
            ("announce", (1, 1, 0)),
            ("update_extra_metadata", (0, 1, 0)),
            ("permit", (3, 2, 0)),
        ],
    );
}

/// `tests/b20_stablecoin_v1_golden.rs::golden_gas_footprints`, including the
/// old-policy-first `updatePolicy` revert path (Base #4596).
#[test]
fn stablecoin_v1_footprints_match_base() {
    let v = B20Version::V1;
    let mut actual = common_footprints(v, false);
    actual.push((
        "update_policy_reverts_missing_policy",
        gas_reverting(
            v,
            false,
            |_| {},
            ADMIN,
            IB20::updatePolicyCall {
                policyScope: ids::TRANSFER_SENDER_POLICY,
                newPolicyId: 99,
            }
            .abi_encode(),
        ),
    ));
    assert_footprints(
        "stablecoin V1",
        &actual,
        &[
            ("transfer", (3, 2, 0)),
            ("transfer_from", (4, 3, 0)),
            ("approve", (0, 1, 0)),
            ("mint", (5, 2, 0)),
            ("burn", (4, 2, 0)),
            ("burn_blocked", (4, 2, 0)),
            ("pause", (1, 1, 0)),
            ("unpause", (1, 1, 0)),
            ("update_supply_cap", (2, 1, 0)),
            ("update_name", (0, 1, 0)),
            ("update_symbol", (0, 1, 0)),
            ("update_contract_uri", (0, 1, 0)),
            ("grant_role", (1, 1, 0)),
            ("revoke_role", (1, 1, 0)),
            ("set_role_admin", (1, 1, 0)),
            ("update_policy", (2, 1, 0)),
            ("update_policy_reverts_missing_policy", (1, 0, 0)),
            ("permit", (3, 2, 0)),
        ],
    );
}

/// The asset V1 `updatePolicy` checks existence first, so its missing-policy revert reads
/// nothing from the token — the asymmetry Base #4596 restored for the stablecoin only.
#[test]
fn asset_v1_update_policy_missing_policy_reads_nothing() {
    let footprint = gas_reverting(
        B20Version::V1,
        true,
        |_| {},
        ADMIN,
        IB20::updatePolicyCall {
            policyScope: ids::TRANSFER_SENDER_POLICY,
            newPolicyId: 99,
        }
        .abi_encode(),
    );
    assert_eq!(footprint, (0, 0, 0));
}

fn seize_setup(f: &mut Fixture) {
    f.fund(ALICE, 100);
    f.give_role(ids::SEIZE_ROLE, ADMIN);
    f.set_policy(PolicySlot::SeizeExempt, POLICY_ID);
    f.set_policy(PolicySlot::SeizeReceiver, POLICY_ID_2);
    f.registry_allow(POLICY_ID_2, BOB);
}

fn seize_call() -> Vec<u8> {
    IB20::seizeWithMemoCall {
        from: ALICE,
        to: BOB,
        amount: u(40),
        memo: MEMO,
    }
    .abi_encode()
}

/// `tests/b20_asset_v2_golden.rs::golden_gas_footprints`.
#[test]
fn asset_v2_footprints_match_base() {
    let v = B20Version::V2;
    let mut actual = common_footprints(v, true);
    actual.extend([
        ("seize", gas(v, true, seize_setup, ADMIN, seize_call())),
        (
            "update_multiplier",
            gas(
                v,
                true,
                |_| {},
                ADMIN,
                IB20Asset::updateMultiplierCall {
                    newMultiplier: WAD * u(2),
                }
                .abi_encode(),
            ),
        ),
        (
            "update_ui_multiplier",
            gas(
                v,
                true,
                |_| {},
                ADMIN,
                IB20Asset::updateUIMultiplierCall {
                    newMultiplier: WAD * u(2),
                    effectiveAt: u(1_000),
                }
                .abi_encode(),
            ),
        ),
        (
            "cancel_ui_multiplier_update",
            gas(
                v,
                true,
                |f| f.set_pending((WAD * u(2)).to::<u128>(), 1_000),
                ADMIN,
                IB20Asset::cancelUIMultiplierUpdateCall {}.abi_encode(),
            ),
        ),
        (
            "batch_mint",
            gas(
                v,
                true,
                |_| {},
                ADMIN,
                IB20Asset::batchMintCall {
                    recipients: vec![BOB, CAROL],
                    amounts: vec![u(30), u(70)],
                }
                .abi_encode(),
            ),
        ),
        (
            "announce",
            gas(
                v,
                true,
                |_| {},
                ADMIN,
                IB20Asset::announceCall {
                    internalCalls: vec![],
                    id: "gas".into(),
                    description: String::new(),
                    uri: String::new(),
                }
                .abi_encode(),
            ),
        ),
        (
            "update_extra_metadata",
            gas(
                v,
                true,
                |_| {},
                ADMIN,
                IB20Asset::updateExtraMetadataCall {
                    key: "category".into(),
                    value: "commodity".into(),
                }
                .abi_encode(),
            ),
        ),
    ]);
    assert_footprints(
        "asset V2",
        &actual,
        &[
            ("transfer", (3, 2, 0)),
            ("transfer_from", (4, 3, 0)),
            ("approve", (0, 1, 0)),
            ("mint", (5, 2, 0)),
            ("burn", (4, 2, 0)),
            ("burn_blocked", (4, 2, 0)),
            ("seize", (6, 2, 0)),
            ("pause", (1, 1, 0)),
            ("unpause", (1, 1, 0)),
            ("update_supply_cap", (2, 1, 0)),
            ("update_name", (1, 1, 0)),
            ("update_symbol", (1, 1, 0)),
            ("update_contract_uri", (1, 1, 0)),
            ("grant_role", (1, 1, 0)),
            ("revoke_role", (1, 1, 0)),
            ("set_role_admin", (1, 1, 0)),
            ("update_policy", (2, 1, 0)),
            ("update_multiplier", (4, 1, 0)),
            ("update_ui_multiplier", (3, 1, 0)),
            ("cancel_ui_multiplier_update", (3, 1, 0)),
            ("batch_mint", (11, 4, 0)),
            ("announce", (1, 1, 0)),
            ("update_extra_metadata", (1, 1, 0)),
            ("permit", (3, 2, 5)),
        ],
    );
}

/// `tests/b20_stablecoin_v2_golden.rs::golden_gas_footprints`.
#[test]
fn stablecoin_v2_footprints_match_base() {
    let v = B20Version::V2;
    let mut actual = common_footprints(v, false);
    actual.extend([
        ("seize", gas(v, false, seize_setup, ADMIN, seize_call())),
        (
            "transfer_with_memo",
            gas(
                v,
                false,
                |f| f.fund(ALICE, 100),
                ALICE,
                IB20::transferWithMemoCall {
                    to: BOB,
                    amount: u(30),
                    memo: MEMO,
                }
                .abi_encode(),
            ),
        ),
        (
            "transfer_from_with_memo",
            gas(
                v,
                false,
                |f| {
                    f.fund(ALICE, 100);
                    f.set_allowance(ALICE, BOB, 40);
                },
                BOB,
                IB20::transferFromWithMemoCall {
                    from: ALICE,
                    to: BOB,
                    amount: u(30),
                    memo: MEMO,
                }
                .abi_encode(),
            ),
        ),
        (
            "mint_with_memo",
            gas(
                v,
                false,
                |_| {},
                ADMIN,
                IB20::mintWithMemoCall {
                    to: BOB,
                    amount: u(100),
                    memo: MEMO,
                }
                .abi_encode(),
            ),
        ),
        (
            "burn_with_memo",
            gas(
                v,
                false,
                |f| {
                    f.fund(ALICE, 100);
                    f.give_role(ids::BURN_ROLE, ALICE);
                },
                ALICE,
                IB20::burnWithMemoCall {
                    amount: u(40),
                    memo: MEMO,
                }
                .abi_encode(),
            ),
        ),
        (
            "renounce_role",
            gas(
                v,
                false,
                |f| f.give_role(ids::MINT_ROLE, ALICE),
                ALICE,
                IB20::renounceRoleCall {
                    role: ids::MINT_ROLE,
                    callerConfirmation: ALICE,
                }
                .abi_encode(),
            ),
        ),
        (
            "renounce_last_admin",
            gas(
                v,
                false,
                |f| f.give_role(ids::DEFAULT_ADMIN_ROLE, ADMIN),
                ADMIN,
                IB20::renounceLastAdminCall {}.abi_encode(),
            ),
        ),
    ]);
    assert_footprints(
        "stablecoin V2",
        &actual,
        &[
            ("transfer", (3, 2, 0)),
            ("transfer_from", (4, 3, 0)),
            ("approve", (0, 1, 0)),
            ("mint", (5, 2, 0)),
            ("burn", (4, 2, 0)),
            ("burn_blocked", (4, 2, 0)),
            ("seize", (6, 2, 0)),
            ("pause", (1, 1, 0)),
            ("unpause", (1, 1, 0)),
            ("update_supply_cap", (2, 1, 0)),
            ("update_name", (1, 1, 0)),
            ("update_symbol", (1, 1, 0)),
            ("update_contract_uri", (1, 1, 0)),
            ("grant_role", (1, 1, 0)),
            ("revoke_role", (1, 1, 0)),
            ("set_role_admin", (1, 1, 0)),
            ("update_policy", (2, 1, 0)),
            ("transfer_with_memo", (3, 2, 0)),
            ("transfer_from_with_memo", (4, 3, 0)),
            ("mint_with_memo", (5, 2, 0)),
            ("burn_with_memo", (4, 2, 0)),
            ("renounce_role", (1, 1, 0)),
            ("renounce_last_admin", (4, 2, 0)),
            ("permit", (3, 2, 5)),
        ],
    );
}

/// `golden_transfer_unprivileged_zero_receiver_storage_access` (Base #4823): Cobalt rejects a
/// zero receiver after the pause read and before any policy SLOAD.
#[test]
fn v2_zero_receiver_rejects_before_policy_read() {
    let mut f = Fixture::fresh(true);
    f.fund(ALICE, 10);
    let err = f
        .call(
            B20Version::V2,
            ALICE,
            &IB20::transferCall {
                to: Address::ZERO,
                amount: u(1),
            }
            .abi_encode(),
            false,
        )
        .unwrap_err();
    assert_eq!(
        err,
        B20Error::revert(IB20::InvalidReceiver {
            receiver: Address::ZERO
        })
    );
    assert_eq!(f.port.footprint(), (1, 0, 0));
}

/// Cobalt reads the packed transfer-policy slot once; Beryl reads it once per check.
#[test]
fn v2_unprivileged_transfer_reads_policy_slot_once() {
    let call = IB20::transferCall {
        to: BOB,
        amount: u(30),
    }
    .abi_encode();
    let footprint = |version| {
        let mut f = Fixture::fresh(true);
        f.fund(ALICE, 100);
        f.call(version, ALICE, &call, false).unwrap();
        f.port.footprint()
    };
    // pause + policy slot(s) + two balances.
    assert_eq!(footprint(B20Version::V1), (5, 2, 0));
    assert_eq!(footprint(B20Version::V2), (4, 2, 0));
}

/// Cobalt meters permit's cryptography: the domain separator's three keccaks, the signing
/// digest's two, and a flat 3000 for recovery — charged before `v` is checked, so even an
/// invalid signature pays it. Beryl charges none of it.
#[test]
fn v2_permit_meters_hashing_and_recovery() {
    let mut call = signed_permit(B20Version::V2, true);
    call.v = 0;
    let calldata = call.abi_encode();
    for version in [B20Version::V1, B20Version::V2] {
        let mut f = Fixture::fresh(true);
        let err = f.call(version, call.owner, &calldata, false).unwrap_err();
        assert_eq!(
            err,
            B20Error::revert(IB20::InvalidSigner {
                signer: Address::ZERO,
                owner: call.owner
            })
        );
        if version == B20Version::V1 {
            assert_eq!(f.port.deducted, 0);
        } else {
            // name "Base Asset" (1 word) 36 + "1" 36 + domain encoding (5 words) 60
            // + Permit struct (6 words) 66 + digest input (66 bytes, 3 words) 48 + recovery 3000.
            assert_eq!(f.port.deducted, 36 + 36 + 60 + 66 + 48 + 3000);
            assert_eq!(f.port.keccaks, 5);
        }
    }
}

/// `DOMAIN_SEPARATOR()` is metered from Cobalt too: it runs the same three keccaks.
#[test]
fn v2_domain_separator_read_is_metered() {
    let mut f = Fixture::fresh(true);
    f.call(
        B20Version::V2,
        ALICE,
        &IB20::DOMAIN_SEPARATORCall {}.abi_encode(),
        false,
    )
    .unwrap();
    assert_eq!(f.port.deducted, 36 + 36 + 60);
    let mut f = Fixture::fresh(true);
    f.call(
        B20Version::V1,
        ALICE,
        &IB20::DOMAIN_SEPARATORCall {}.abi_encode(),
        false,
    )
    .unwrap();
    assert_eq!(f.port.deducted, 0);
}

// --- Version gating ---

/// Cobalt selectors are unknown on a Beryl block: the revert is the bare selector.
#[test]
fn cobalt_selectors_are_unknown_before_cobalt() {
    let cases: Vec<(bool, Vec<u8>)> = vec![
        (false, seize_call()),
        (false, IB20::SEIZE_ROLECall {}.abi_encode()),
        (false, IB20::SEIZE_EXEMPT_POLICYCall {}.abi_encode()),
        (true, IB20Asset::uiMultiplierCall {}.abi_encode()),
        (
            true,
            IB20Asset::updateUIMultiplierCall {
                newMultiplier: WAD,
                effectiveAt: u(10),
            }
            .abi_encode(),
        ),
        (
            true,
            IB20Asset::supportsInterfaceCall {
                interfaceId: [0x01, 0xff, 0xc9, 0xa7].into(),
            }
            .abi_encode(),
        ),
    ];
    for (is_asset, calldata) in cases {
        let mut f = Fixture::fresh(is_asset);
        let err = f.call(B20Version::V1, ADMIN, &calldata, false).unwrap_err();
        assert_eq!(err, B20Error::Revert(calldata[..4].to_vec().into()));
    }
}

/// The frozen Beryl surface's 3-member `PausableFeature` rejects `SEIZE` at decode; Cobalt
/// accepts it.
#[test]
fn seize_pause_feature_is_rejected_before_cobalt() {
    let calldata = IB20::pauseCall {
        features: vec![IB20::PausableFeature::SEIZE],
    }
    .abi_encode();
    let mut f = Fixture::fresh(true);
    f.give_role(ids::PAUSE_ROLE, ADMIN);
    let err = f.call(B20Version::V1, ADMIN, &calldata, false).unwrap_err();
    assert_eq!(
        err,
        B20Error::Revert(IB20::pauseCall::SELECTOR.to_vec().into())
    );

    let mut f = Fixture::fresh(true);
    f.give_role(ids::PAUSE_ROLE, ADMIN);
    f.call(B20Version::V2, ADMIN, &calldata, false).unwrap();
    let paused = f
        .call(
            B20Version::V2,
            ADMIN,
            &IB20::pausedFeaturesCall {}.abi_encode(),
            false,
        )
        .unwrap();
    let features = <Vec<IB20::PausableFeature>>::abi_decode(&paused).unwrap();
    assert_eq!(features, vec![IB20::PausableFeature::SEIZE]);
}

/// Calldata shorter than a selector reverts as the unknown selector `0x00000000`.
#[test]
fn short_calldata_reverts_with_zero_selector() {
    for version in [B20Version::V1, B20Version::V2] {
        let mut f = Fixture::fresh(true);
        let err = f.call(version, ALICE, &[0xa9, 0x05], false).unwrap_err();
        assert_eq!(err, B20Error::Revert(vec![0u8; 4].into()));
    }
}

// --- Seize semantics ---

#[test]
fn seize_moves_balance_and_emits_transfer_memo_seized() {
    let mut f = Fixture::fresh(false);
    seize_setup(&mut f);
    f.call(B20Version::V2, ADMIN, &seize_call(), false).unwrap();
    let balance = |f: &mut Fixture, who| {
        U256::abi_decode(
            &f.call(
                B20Version::V2,
                who,
                &IB20::balanceOfCall { account: who }.abi_encode(),
                false,
            )
            .unwrap(),
        )
        .unwrap()
    };
    // Supply is untouched: seize is a transfer, not a burn.
    assert_eq!(balance(&mut f, ALICE), u(60));
    assert_eq!(balance(&mut f, BOB), u(40));
    let supply = f.slot(field_slot(ROOT_B20, 3));
    assert_eq!(supply, u(100));

    f.call(B20Version::V2, ADMIN, &seize_call(), false).unwrap();
    let topics: Vec<B256> = f.port.logs.iter().map(|l| l.topics()[0]).collect();
    use alloy::sol_types::SolEvent;
    assert_eq!(
        topics,
        vec![
            IB20::Transfer::SIGNATURE_HASH,
            IB20::Memo::SIGNATURE_HASH,
            IB20::Seized::SIGNATURE_HASH
        ]
    );
}

/// Guard precedence: the holder gate outranks the destination gate, which outranks balance.
#[test]
fn seize_guard_order() {
    // Unset SEIZE_EXEMPT_POLICY is always-allow, so nobody is seizable.
    let mut f = Fixture::fresh(true);
    f.fund(ALICE, 100);
    f.give_role(ids::SEIZE_ROLE, ADMIN);
    let err = f
        .call(B20Version::V2, ADMIN, &seize_call(), false)
        .unwrap_err();
    assert_eq!(
        err,
        B20Error::revert(IB20::AccountNotSeizable { account: ALICE })
    );

    // Seizable holder, but BOB is not an authorized seize receiver.
    let mut f = Fixture::fresh(true);
    f.fund(ALICE, 100);
    f.give_role(ids::SEIZE_ROLE, ADMIN);
    f.set_policy(PolicySlot::SeizeExempt, POLICY_ID);
    f.set_policy(PolicySlot::SeizeReceiver, POLICY_ID_2);
    let err = f
        .call(B20Version::V2, ADMIN, &seize_call(), false)
        .unwrap_err();
    assert_eq!(
        err,
        B20Error::revert(IB20::PolicyForbids {
            policyScope: ids::SEIZE_RECEIVER_POLICY,
            policyId: POLICY_ID_2
        })
    );

    // Self-seize is an invalid receiver, checked before the policies.
    let mut f = Fixture::fresh(true);
    seize_setup(&mut f);
    let calldata = IB20::seizeWithMemoCall {
        from: ALICE,
        to: ALICE,
        amount: u(1),
        memo: MEMO,
    }
    .abi_encode();
    let err = f.call(B20Version::V2, ADMIN, &calldata, false).unwrap_err();
    assert_eq!(
        err,
        B20Error::revert(IB20::InvalidReceiver { receiver: ALICE })
    );

    // Missing role.
    let mut f = Fixture::fresh(true);
    let err = f
        .call(B20Version::V2, ALICE, &seize_call(), false)
        .unwrap_err();
    assert_eq!(
        err,
        B20Error::revert(IB20::AccessControlUnauthorizedAccount {
            account: ALICE,
            neededRole: ids::SEIZE_ROLE
        })
    );
}

// --- Composite policies ---

const UNION_ID: u64 = (2u64 << 56) | 20;
const INTERSECT_ID: u64 = (3u64 << 56) | 21;
/// BLOCKLIST, counter 9.
const BLOCK_9: u64 = 9;

/// A transfer under a composite sender policy: authorized on Cobalt per the composite's live
/// children, while Beryl treats the composite type byte as malformed (unauthorized).
#[test]
fn composite_sender_policy_gates_transfer() {
    let transfer = IB20::transferCall {
        to: BOB,
        amount: u(1),
    }
    .abi_encode();
    let run_with = |sender_policy: u64, setup: &dyn Fn(&mut Fixture), version| {
        let mut f = Fixture::fresh(true);
        f.fund(ALICE, 10);
        f.set_policy(PolicySlot::TransferSender, sender_policy);
        setup(&mut f);
        f.call(version, ALICE, &transfer, false)
    };

    // UNION(allowlist 7, allowlist 8): ALICE is in 8 only -> authorized.
    let union_setup = |f: &mut Fixture| {
        f.registry_children(UNION_ID, &[POLICY_ID, POLICY_ID_2]);
        f.registry_allow(POLICY_ID_2, ALICE);
    };
    assert!(run_with(UNION_ID, &union_setup, B20Version::V2).is_ok());
    assert_eq!(
        run_with(UNION_ID, &union_setup, B20Version::V1).unwrap_err(),
        B20Error::revert(IB20::PolicyForbids {
            policyScope: ids::TRANSFER_SENDER_POLICY,
            policyId: UNION_ID
        })
    );

    // INTERSECT(allowlist 8, blocklist 9): ALICE allowed by 8 but blocked by 9 -> forbidden.
    let intersect_setup = |f: &mut Fixture| {
        f.registry_children(INTERSECT_ID, &[POLICY_ID_2, BLOCK_9]);
        f.registry_allow(POLICY_ID_2, ALICE);
        f.registry_allow(BLOCK_9, ALICE);
    };
    assert_eq!(
        run_with(INTERSECT_ID, &intersect_setup, B20Version::V2).unwrap_err(),
        B20Error::revert(IB20::PolicyForbids {
            policyScope: ids::TRANSFER_SENDER_POLICY,
            policyId: INTERSECT_ID
        })
    );

    // Never-created composites: UNION of nothing authorizes nobody, INTERSECT everybody.
    assert!(run_with(UNION_ID, &|_| {}, B20Version::V2).is_err());
    assert!(run_with(INTERSECT_ID, &|_| {}, B20Version::V2).is_ok());
}

/// Registry reads for a composite: its length word, one data word per four children, then
/// each evaluated child's membership — stopping at the first deciding child.
#[test]
fn composite_evaluation_short_circuits() {
    let registry_reads = |children: &[u64], allow: &[(u64, Address)]| {
        let mut f = Fixture::fresh(true);
        f.registry_children(UNION_ID, children);
        for (policy, who) in allow {
            f.registry_allow(*policy, *who);
        }
        // Count the registry's reads instead of the token's.
        f.port.token = POLICY_REGISTRY;
        f.port.reset_counters();
        let authorized =
            super::policy::is_authorized(&mut f.port, UNION_ID, ALICE, B20Version::V2).unwrap();
        (authorized, f.port.sloads)
    };
    // First child authorizes: length + data word + one membership read.
    assert_eq!(
        registry_reads(&[POLICY_ID, POLICY_ID_2], &[(POLICY_ID, ALICE)]),
        (true, 3)
    );
    // Neither authorizes: both memberships read.
    assert_eq!(registry_reads(&[POLICY_ID, POLICY_ID_2], &[]), (false, 4));
}

// --- ERC-8056 scheduled multiplier ---

fn multiplier_at(f: &mut Fixture, now: u64) -> U256 {
    f.port.timestamp = u(now);
    U256::abi_decode(
        &f.call(
            B20Version::V2,
            ALICE,
            &IB20Asset::multiplierCall {}.abi_encode(),
            false,
        )
        .unwrap(),
    )
    .unwrap()
}

/// A scheduled multiplier flips lazily at `effectiveAt` with no transaction, and the scaled
/// reads follow it. Beryl ignores the schedule entirely.
#[test]
fn scheduled_multiplier_matures_lazily() {
    let mut f = Fixture::fresh(true);
    f.give_role(ids::OPERATOR_ROLE, ADMIN);
    f.fund(ALICE, 1_000);
    f.port.timestamp = u(100);
    f.call(
        B20Version::V2,
        ADMIN,
        &IB20Asset::updateUIMultiplierCall {
            newMultiplier: WAD * u(3),
            effectiveAt: u(200),
        }
        .abi_encode(),
        false,
    )
    .unwrap();

    assert_eq!(multiplier_at(&mut f, 199), WAD);
    assert_eq!(multiplier_at(&mut f, 200), WAD * u(3));

    f.port.timestamp = u(250);
    let ui_balance = U256::abi_decode(
        &f.call(
            B20Version::V2,
            ALICE,
            &IB20Asset::balanceOfUICall { account: ALICE }.abi_encode(),
            false,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(ui_balance, u(3_000));
    let ui_supply = U256::abi_decode(
        &f.call(
            B20Version::V2,
            ALICE,
            &IB20Asset::totalSupplyUICall {}.abi_encode(),
            false,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(ui_supply, u(3_000));

    // Beryl reads only the multiplier slot, which no transaction has written.
    let v1 = U256::abi_decode(
        &f.call(
            B20Version::V1,
            ALICE,
            &IB20Asset::multiplierCall {}.abi_encode(),
            false,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(v1, WAD);
}

#[test]
fn scheduled_multiplier_guards_and_cancel() {
    let mut f = Fixture::fresh(true);
    f.give_role(ids::OPERATOR_ROLE, ADMIN);
    f.port.timestamp = u(100);
    let schedule = |at: u64| {
        IB20Asset::updateUIMultiplierCall {
            newMultiplier: WAD * u(2),
            effectiveAt: u(at),
        }
        .abi_encode()
    };

    assert_eq!(
        f.call(B20Version::V2, ADMIN, &schedule(100), false)
            .unwrap_err(),
        B20Error::revert(IB20Asset::EffectiveAtInPast {
            effectiveAt: u(100)
        })
    );
    f.call(B20Version::V2, ADMIN, &schedule(300), false)
        .unwrap();
    assert_eq!(
        f.call(B20Version::V2, ADMIN, &schedule(400), false)
            .unwrap_err(),
        B20Error::revert(IB20Asset::UIMultiplierUpdateExists {
            effectiveAt: u(300)
        })
    );

    let new_ui = U256::abi_decode(
        &f.call(
            B20Version::V2,
            ALICE,
            &IB20Asset::newUIMultiplierCall {}.abi_encode(),
            false,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(new_ui, WAD * u(2));

    f.call(
        B20Version::V2,
        ADMIN,
        &IB20Asset::cancelUIMultiplierUpdateCall {}.abi_encode(),
        false,
    )
    .unwrap();
    assert_eq!(
        f.call(
            B20Version::V2,
            ADMIN,
            &IB20Asset::cancelUIMultiplierUpdateCall {}.abi_encode(),
            false
        )
        .unwrap_err(),
        B20Error::revert(IB20Asset::UIMultiplierUpdateDoesNotExist {})
    );
    assert_eq!(multiplier_at(&mut f, 1_000), WAD);

    let too_big = IB20Asset::updateMultiplierCall {
        newMultiplier: U256::from(u128::MAX) + U256::ONE,
    }
    .abi_encode();
    assert_eq!(
        f.call(B20Version::V2, ADMIN, &too_big, false).unwrap_err(),
        B20Error::revert(IB20Asset::InvalidMultiplier {})
    );
    // Beryl has no upper bound.
    f.call(B20Version::V1, ADMIN, &too_big, false).unwrap();
}

/// The instant setter supersedes a live schedule: it clears the slot and emits the
/// cancellation, the legacy event, then the ERC-8056 event.
#[test]
fn instant_multiplier_update_cancels_live_schedule() {
    use alloy::sol_types::SolEvent;
    let mut f = Fixture::fresh(true);
    f.give_role(ids::OPERATOR_ROLE, ADMIN);
    f.set_pending((WAD * u(5)).to::<u128>(), 1_000);
    f.port.timestamp = u(10);
    f.call(
        B20Version::V2,
        ADMIN,
        &IB20Asset::updateMultiplierCall {
            newMultiplier: WAD * u(2),
        }
        .abi_encode(),
        false,
    )
    .unwrap();
    let topics: Vec<B256> = f.port.logs.iter().map(|l| l.topics()[0]).collect();
    assert_eq!(
        topics,
        vec![
            IB20Asset::UIMultiplierUpdateCancelled::SIGNATURE_HASH,
            IB20Asset::MultiplierUpdated::SIGNATURE_HASH,
            IB20Asset::UIMultiplierUpdated::SIGNATURE_HASH,
        ]
    );
    let effective_at = U256::abi_decode(
        &f.call(
            B20Version::V2,
            ALICE,
            &IB20Asset::effectiveAtCall {}.abi_encode(),
            false,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(effective_at, U256::ZERO);
    assert_eq!(multiplier_at(&mut f, 2_000), WAD * u(2));
}

#[test]
fn supports_erc165_and_erc8056() {
    let mut f = Fixture::fresh(true);
    for (id, expected) in [
        ([0x01, 0xff, 0xc9, 0xa7], true),
        ([0xa6, 0x0b, 0xf1, 0x3d], true),
        ([0x57, 0x85, 0x4f, 0xc3], true),
        ([0xff, 0xff, 0xff, 0xff], false),
    ] {
        let out = f
            .call(
                B20Version::V2,
                ALICE,
                &IB20Asset::supportsInterfaceCall {
                    interfaceId: id.into(),
                }
                .abi_encode(),
                false,
            )
            .unwrap();
        assert_eq!(bool::abi_decode(&out).unwrap(), expected, "{id:02x?}");
    }
}

// --- Cobalt string tail cleanup ---

/// Shrinking a long name: Beryl leaves the old tail word, Cobalt zeroes it.
#[test]
fn shrinking_a_long_name_clears_the_tail_only_from_cobalt() {
    let long = "A token name that is well over thirty-two bytes long".to_string();
    let data_slot = |f: &Fixture| {
        let head = field_slot(ROOT_B20, 0);
        let base = U256::from_be_bytes(keccak256(head.to_be_bytes::<32>()).0);
        f.slot(base + U256::ONE)
    };
    for (version, tail_cleared) in [(B20Version::V1, false), (B20Version::V2, true)] {
        let mut f = Fixture::fresh(true);
        f.give_role(ids::METADATA_ROLE, ADMIN);
        f.call(
            version,
            ADMIN,
            &IB20::updateNameCall {
                newName: long.clone(),
            }
            .abi_encode(),
            false,
        )
        .unwrap();
        assert!(!data_slot(&f).is_zero());
        f.call(
            version,
            ADMIN,
            &IB20::updateNameCall {
                newName: "Short".into(),
            }
            .abi_encode(),
            false,
        )
        .unwrap();
        assert_eq!(data_slot(&f).is_zero(), tail_cleared, "{version:?}");
        let name = String::abi_decode(
            &f.call(version, ADMIN, &IB20::nameCall {}.abi_encode(), false)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(name, "Short");
    }
}

// --- Wire surface ---

/// Selectors, error selectors and topic hashes published in Base's Cobalt changelog.
#[test]
fn cobalt_selectors_match_the_published_changelog() {
    use alloy::sol_types::{SolError, SolEvent};
    let calls: [([u8; 4], [u8; 4]); 15] = [
        (IB20::seizeWithMemoCall::SELECTOR, [0xf9, 0x16, 0xd8, 0x1b]),
        (IB20::SEIZE_ROLECall::SELECTOR, [0x3c, 0x7e, 0x9b, 0xa5]),
        (
            IB20::SEIZE_EXEMPT_POLICYCall::SELECTOR,
            [0xfe, 0xb3, 0x46, 0xec],
        ),
        (
            IB20::SEIZE_RECEIVER_POLICYCall::SELECTOR,
            [0xb3, 0x1d, 0xa2, 0x7f],
        ),
        (
            IB20Asset::updateUIMultiplierCall::SELECTOR,
            [0x62, 0x8e, 0x60, 0x0f],
        ),
        (
            IB20Asset::newUIMultiplierCall::SELECTOR,
            [0xdc, 0x76, 0x70, 0x07],
        ),
        (
            IB20Asset::effectiveAtCall::SELECTOR,
            [0x97, 0xa4, 0x06, 0x4f],
        ),
        (
            IB20Asset::cancelUIMultiplierUpdateCall::SELECTOR,
            [0x2c, 0x97, 0xa0, 0xf0],
        ),
        (
            IB20Asset::uiMultiplierCall::SELECTOR,
            [0xa6, 0x0b, 0xf1, 0x3d],
        ),
        (
            IB20Asset::toUIAmountCall::SELECTOR,
            [0x32, 0x48, 0xd4, 0xff],
        ),
        (
            IB20Asset::fromUIAmountCall::SELECTOR,
            [0x65, 0xcd, 0x9b, 0x3c],
        ),
        (
            IB20Asset::balanceOfUICall::SELECTOR,
            [0x43, 0x7a, 0x99, 0x58],
        ),
        (
            IB20Asset::totalSupplyUICall::SELECTOR,
            [0x9b, 0xea, 0x64, 0x29],
        ),
        (
            IB20Asset::MAX_UI_MULTIPLIERCall::SELECTOR,
            [0x78, 0x5c, 0x0c, 0xf0],
        ),
        (
            IB20Asset::supportsInterfaceCall::SELECTOR,
            [0x01, 0xff, 0xc9, 0xa7],
        ),
    ];
    for (got, want) in calls {
        assert_eq!(got, want);
    }
    let errors: [([u8; 4], [u8; 4]); 6] = [
        (IB20::AccountNotSeizable::SELECTOR, [0x91, 0xdb, 0xbc, 0x8d]),
        (
            IB20Asset::InvalidMultiplier::SELECTOR,
            [0x6f, 0x12, 0xf3, 0xdc],
        ),
        (
            IB20Asset::EffectiveAtInPast::SELECTOR,
            [0x14, 0x11, 0x9c, 0xf6],
        ),
        (
            IB20Asset::EffectiveAtTooFar::SELECTOR,
            [0x1c, 0xe2, 0x14, 0xfa],
        ),
        (
            IB20Asset::UIMultiplierUpdateExists::SELECTOR,
            [0x44, 0x81, 0xa6, 0x8e],
        ),
        (
            IB20Asset::UIMultiplierUpdateDoesNotExist::SELECTOR,
            [0xa7, 0xd6, 0xa5, 0xca],
        ),
    ];
    for (got, want) in errors {
        assert_eq!(got, want);
    }
    assert_eq!(
        IB20::Seized::SIGNATURE_HASH,
        b256!("a9aec5d8b86e2fa2fd6ac3af62f2622e3dfdab1967d4cbbb56a5df7d74cb887c")
    );
}

/// Base's `v1_selectors_are_a_subset_of_v2`: Cobalt adds exactly 11 asset selectors and the
/// four seize selectors to the common surface, and removes none.
#[test]
fn cobalt_surface_is_a_strict_superset() {
    use super::abi::{IB20AssetV1, IB20V1};
    use alloy::sol_types::SolInterface;
    let v1_asset: Vec<[u8; 4]> = IB20AssetV1::IB20AssetV1Calls::selectors().collect();
    let v2_asset: Vec<[u8; 4]> = IB20Asset::IB20AssetCalls::selectors().collect();
    assert!(v1_asset.iter().all(|s| v2_asset.contains(s)));
    assert_eq!(v2_asset.len() - v1_asset.len(), 11);

    let v1_common: Vec<[u8; 4]> = IB20V1::IB20V1Calls::selectors().collect();
    let v2_common: Vec<[u8; 4]> = IB20::IB20Calls::selectors().collect();
    assert!(v1_common.iter().all(|s| v2_common.contains(s)));
    assert_eq!(v2_common.len() - v1_common.len(), 4);
}
