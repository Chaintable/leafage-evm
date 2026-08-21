//! Gas-accounting tests for the B20 precompile port, checked against Base mainnet.
//!
//! Reference numbers were measured on Base mainnet by binary-searching `eth_call`'s `gas`
//! parameter for the smallest limit that does not run out of gas, then subtracting the
//! intrinsic transaction cost. That minimum is what this test reproduces — deliberately,
//! rather than "gas spent", because the two differ: EIP-2200 requires more than the 2300
//! call stipend to remain *before* an `SSTORE` even though the write does not spend it. A
//! call whose last `SSTORE` is followed by little work is therefore bounded by that reserve,
//! not by its own total. `transfer` is exactly such a call (11,019 required vs 10,574 spent),
//! which is why the reserve has to be modelled and not just summed.
//!
//! Every primitive below was pinned independently on-chain: a pure constant read costs
//! 106 (`DEFAULT_ADMIN_ROLE`), a cold SLOAD 2100, a warm SLOAD 100, a first-touch no-op
//! SSTORE 2200, a real 0→1 SSTORE +19,900, and a 3-topic/32-byte LOG 1756.

use std::collections::{HashMap, HashSet};

use alloy::primitives::{address, b256, Address, LogData, B256, U256};
use alloy::sol_types::SolCall;
use leafage_evm_chains::base::b20::{
    dispatch, is_asset_variant, B20Error, B20Outcome, B20Port, Result as B20Result, IB20, IB20Asset,
};
use revm::context_interface::cfg::GasParams;
use revm::interpreter::gas::{Gas, LOG};
use revm::primitives::hardfork::SpecId;

const TOKEN: Address = address!("0xb200000000000000000000B8d3746D2E56596578");
const SENDER: Address = address!("0xAeD1FFe46F5B6e14AfdEf764dE436c38D38Cd93f");
const RECIPIENT: Address = address!("0x5de27f402f7eb08f767ff0b794e7e118d2bc1fe6");
const TRANSFER_SENDER_POLICY: B256 =
    b256!("b81736c875ab819dd97f59f2a6542cfb731ad52b4ae15a6f24df2fb02b0327f5");

/// Mock port mirroring `MeteredB20Port`, including the EIP-2200 stipend guard.
struct MockPort {
    storage: HashMap<(Address, U256), U256>,
    warm_slots: HashSet<(Address, U256)>,
    warm_accounts: HashSet<Address>,
    code_accounts: HashSet<Address>,
    gas: Gas,
    params: GasParams,
}

impl MockPort {
    /// Builds a port over the reference token's live state at Base block ~50.1M: the token
    /// has marker bytecode, its account is already warm (it is the call's `to`), and every
    /// slot this suite touches reads zero — `paused`, the packed policy IDs (all
    /// ALWAYS_ALLOW), both balances, and the allowance.
    fn new(gas_limit: u64) -> Self {
        let mut port = Self {
            storage: HashMap::new(),
            warm_slots: HashSet::new(),
            warm_accounts: HashSet::new(),
            code_accounts: HashSet::new(),
            gas: Gas::new(gas_limit),
            params: GasParams::new_spec(SpecId::PRAGUE),
        };
        port.warm_accounts.insert(TOKEN);
        port.code_accounts.insert(TOKEN);
        port
    }

    fn charge(&mut self, cost: u64) -> B20Result<()> {
        if self.gas.record_cost(cost) {
            Ok(())
        } else {
            Err(B20Error::OutOfGas)
        }
    }
}

impl B20Port for MockPort {
    fn sload(&mut self, address: Address, key: U256) -> B20Result<U256> {
        let is_cold = self.warm_slots.insert((address, key));
        self.charge(self.params.warm_storage_read_cost())?;
        if is_cold {
            self.charge(self.params.cold_storage_additional_cost())?;
        }
        Ok(self.storage.get(&(address, key)).copied().unwrap_or_default())
    }

    fn sstore(&mut self, address: Address, key: U256, value: U256) -> B20Result<()> {
        if self.gas.remaining() <= self.params.call_stipend() {
            return Err(B20Error::OutOfGas);
        }
        let is_cold = self.warm_slots.insert((address, key));
        let present = self.storage.get(&(address, key)).copied().unwrap_or_default();
        let result = revm::context_interface::context::SStoreResult {
            original_value: present,
            present_value: present,
            new_value: value,
        };
        self.charge(self.params.sstore_static_gas())?;
        self.charge(self.params.sstore_dynamic_gas(true, &result, is_cold))?;
        self.gas.record_refund(self.params.sstore_refund(true, &result));
        self.storage.insert((address, key), value);
        Ok(())
    }

    fn emit_event(&mut self, _address: Address, log: LogData) -> B20Result<()> {
        let cost = LOG + self.params.log_cost(log.topics().len() as u8, log.data.len() as u64);
        self.charge(cost)
    }

    fn has_code(&mut self, address: Address) -> B20Result<bool> {
        let is_cold = self.warm_accounts.insert(address);
        self.charge(self.params.warm_storage_read_cost())?;
        if is_cold {
            self.charge(self.params.cold_account_additional_cost())?;
        }
        Ok(self.code_accounts.contains(&address))
    }

    fn deduct_gas(&mut self, gas: u64) -> B20Result<()> {
        self.charge(gas)
    }

    fn caller(&self) -> Address {
        SENDER
    }
    fn call_value(&self) -> U256 {
        U256::ZERO
    }
    fn chain_id(&self) -> u64 {
        8453
    }
    fn timestamp(&self) -> U256 {
        U256::from(1_786_968_339u64)
    }
    fn is_static(&self) -> bool {
        false
    }
}

/// Whether the call completes without running out of gas. A business revert counts as
/// completing — it is a normal outcome that consumed only the gas it needed, which is
/// exactly how the on-chain probe distinguished the two.
fn completes(calldata: &[u8], gas_limit: u64) -> bool {
    let mut port = MockPort::new(gas_limit);
    !matches!(
        dispatch(&mut port, TOKEN, is_asset_variant(&TOKEN), calldata),
        Err(B20Error::OutOfGas)
    )
}

/// Smallest gas limit at which the call completes — the quantity measured on Base.
fn min_gas(calldata: &[u8]) -> u64 {
    let (mut lo, mut hi) = (0u64, 200_000u64);
    assert!(completes(calldata, hi), "call must complete at the upper bound");
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if completes(calldata, mid) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    hi
}

fn check(name: &str, calldata: Vec<u8>, expected: u64) {
    let got = min_gas(&calldata);
    assert_eq!(got, expected, "{name}: minimum gas must match Base mainnet");
}

/// Each expectation is the precompile gas measured on Base mainnet:
/// `min eth_call gas` minus that call's intrinsic cost.
#[test]
fn gas_matches_base_mainnet_across_the_call_surface() {
    // Pure constant: 1 calldata word (6) + the initialization account read (100).
    check("DEFAULT_ADMIN_ROLE", IB20::DEFAULT_ADMIN_ROLECall {}.abi_encode(), 106);

    // One cold SLOAD on top of a 2-word preamble.
    check("balanceOf", IB20::balanceOfCall { account: SENDER }.abi_encode(), 2212);
    check(
        "policyId",
        IB20::policyIdCall { policyScope: TRANSFER_SENDER_POLICY }.abi_encode(),
        2212,
    );

    // Reverts after the pause read (InvalidReceiver).
    check(
        "transfer->0x0",
        IB20::transferCall { to: Address::ZERO, amount: U256::ZERO }.abi_encode(),
        2218,
    );
    // Reverts after both policy checks and the sender balance read (InsufficientBalance).
    check(
        "transfer insufficient",
        IB20::transferCall { to: RECIPIENT, amount: U256::ONE }.abi_encode(),
        6518,
    );

    // First-touch no-op SSTORE (2200) + Approval log (1756).
    check(
        "approve(0)",
        IB20::approveCall { spender: RECIPIENT, amount: U256::ZERO }.abi_encode(),
        4074,
    );
    // Same, but the write actually sets the slot: +19,900 (SSTORE_SET without load).
    check(
        "approve(1)",
        IB20::approveCall { spender: RECIPIENT, amount: U256::ONE }.abi_encode(),
        23974,
    );

    // The reported bug. Bounded by the stipend reserve, not the 10,574 it spends.
    check(
        "transfer",
        IB20::transferCall { to: RECIPIENT, amount: U256::ZERO }.abi_encode(),
        11019,
    );
    // Self-transfer: the recipient balance read hits the already-warm sender slot, -2000.
    check(
        "transfer to self",
        IB20::transferCall { to: SENDER, amount: U256::ZERO }.abi_encode(),
        9019,
    );
    // The extra Memo log pushes the post-SSTORE work above the stipend, so this one is
    // bounded by its own sum again.
    check(
        "transferWithMemo",
        IB20::transferWithMemoCall {
            to: RECIPIENT,
            amount: U256::ZERO,
            memo: B256::with_last_byte(0x42),
        }
        .abi_encode(),
        12080,
    );
    // Allowance write-back is the final SSTORE, so the stipend reserve binds again.
    check(
        "transferFrom",
        IB20::transferFromCall { from: SENDER, to: RECIPIENT, amount: U256::ZERO }.abi_encode(),
        14981,
    );
}

/// `transfer` must resolve at all — the reported bug was that leafage reverted it, because
/// the selector was absent from its view-only interface.
#[test]
fn transfer_succeeds_and_returns_true() {
    let calldata = IB20::transferCall { to: RECIPIENT, amount: U256::ZERO }.abi_encode();
    let mut port = MockPort::new(100_000);
    match dispatch(&mut port, TOKEN, true, &calldata).expect("must not fail fatally") {
        B20Outcome::Return(out) => {
            assert_eq!(out.len(), 32);
            assert_eq!(out[31], 1, "transfer returns true");
        }
        B20Outcome::Revert(out) => panic!("transfer reverted: 0x{}", alloy::hex::encode(out)),
    }
}

/// A self-transfer must re-read the recipient balance after debiting the sender, or the
/// second write would restore the pre-debit value and mint tokens.
#[test]
fn self_transfer_does_not_inflate_balance() {
    let mut port = MockPort::new(1_000_000);
    // Seed a balance by minting through the port's own slot derivation.
    let probe = IB20::balanceOfCall { account: SENDER }.abi_encode();
    let _ = dispatch(&mut port, TOKEN, true, &probe);
    let slot = *port.storage.keys().next().unwrap_or(&(TOKEN, U256::ZERO));
    let _ = slot;

    // Run a zero self-transfer first so the balance slot is materialised, then seed it.
    let zero = IB20::transferCall { to: SENDER, amount: U256::ZERO }.abi_encode();
    let _ = dispatch(&mut port, TOKEN, true, &zero);
    let balance_slot = *port
        .storage
        .keys()
        .find(|(addr, _)| *addr == TOKEN)
        .expect("balance slot written");
    port.storage.insert(balance_slot, U256::from(100u64));

    let calldata = IB20::transferCall { to: SENDER, amount: U256::from(50u64) }.abi_encode();
    let outcome = dispatch(&mut port, TOKEN, true, &calldata).expect("self-transfer");
    assert!(matches!(outcome, B20Outcome::Return(_)), "self-transfer must succeed");
    assert_eq!(
        port.storage.get(&balance_slot).copied().unwrap_or_default(),
        U256::from(100u64),
        "a self-transfer must leave the balance unchanged"
    );
}

/// The asset variant must expose `WAD_PRECISION`, and the multiplier must default to WAD
/// when its slot is unset — Base's generated accessor does this, and reading the raw zero
/// instead would make `scaledBalanceOf` return 0 and `toRawBalance` divide by zero.
#[test]
fn unset_multiplier_defaults_to_wad() {
    let mut port = MockPort::new(100_000);
    let calldata = IB20Asset::multiplierCall {}.abi_encode();
    match dispatch(&mut port, TOKEN, true, &calldata).expect("multiplier") {
        B20Outcome::Return(out) => {
            let value = U256::from_be_slice(&out);
            assert_eq!(value, U256::from(10u64).pow(U256::from(18u64)), "unset multiplier is WAD");
        }
        B20Outcome::Revert(_) => panic!("multiplier reverted"),
    }
}
