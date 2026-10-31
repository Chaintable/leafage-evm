//! ApeChain yield against a step-by-step port of nitro's `stateObject`
//! (go-ethereum-apechain `core/state/state_object.go`): random transfers and
//! yield-mode changes run as transactions, and after each one the committed
//! balances, yield slots and `shareCount` must equal the model's.

use super::ArbitrumEvm;
use crate::arbitrum::arbos_state;
use crate::arbitrum::evm::ArbitrumExecutionContext;
use crate::arbitrum::hardforks::ArbitrumHardfork;
use crate::arbitrum::precompile::{ArbitrumPrecompileEnv, ARB_INFO_ADDRESS};
use crate::arbitrum::tx::ArbitrumTxEnv;
use alloy::sol_types::SolCall;
use leafage_evm_types::ape_yield::{
    ape_shares_and_remainder, ape_yield_slot, ApeYieldFields, APE_SHARE_COUNT_OFFSET,
    APE_SHARE_PRICE_OFFSET, APE_YIELD_ADDRESS, APE_YIELD_AUTOMATIC, APE_YIELD_DELEGATED,
    APE_YIELD_DISABLED,
};
use leafage_evm_types::{BlockEnv, CfgEnv};
use revm::context::{ContextTr, TxEnv};
use revm::database::{in_memory_db::CacheDB, EmptyDB};
use revm::handler::ExecuteCommitEvm;
use revm::primitives::{keccak256, Address, Bytes, TxKind, U256};
use revm::state::AccountInfo;
use std::collections::BTreeMap;

alloy::sol! {
    function configureAutomaticYield() external;
    function configureVoidYield() external;
    function configureDelegateYield(address delegate) external;
}

/// A price with large remainders, so `fixed` is rarely zero.
const PRICE: u64 = 1_112_418_342;

fn negative(word: U256) -> bool {
    word.bit(255)
}

/// nitro's `stateObject` data, one account at a time.
#[derive(Default)]
struct Model {
    accounts: BTreeMap<Address, ApeYieldFields>,
    share_count: U256,
}

impl Model {
    fn price(&self) -> U256 {
        U256::from(PRICE)
    }

    fn get(&self, a: Address) -> ApeYieldFields {
        self.accounts.get(&a).copied().unwrap_or_default()
    }

    /// `Balance`.
    fn balance(&self, a: Address) -> U256 {
        let f = self.get(a);
        let gross = if f.flags == APE_YIELD_AUTOMATIC {
            f.shares * self.price() + f.fixed
        } else {
            f.fixed
        };
        assert!(gross >= f.debt, "negative balance not allowed");
        gross - f.debt
    }

    fn adjust_shares(&mut self, pre: U256, post: U256) {
        self.share_count = self.share_count.wrapping_add(post).wrapping_sub(pre);
    }

    /// `SetBalance`.
    fn set_balance(&mut self, a: Address, amount: U256) {
        let mut f = self.get(a);
        let amount = amount + f.debt;
        let (fixed, shares) = match f.flags {
            APE_YIELD_AUTOMATIC => {
                let (shares, fixed) = ape_shares_and_remainder(self.price(), amount);
                (fixed, shares)
            }
            _ => {
                if f.flags == APE_YIELD_DELEGATED {
                    let delta = amount.wrapping_sub(f.fixed);
                    if !delta.is_zero() {
                        self.adjust_debt(f.delegate, delta);
                    }
                }
                (amount, U256::ZERO)
            }
        };
        self.adjust_shares(f.shares, shares);
        f.fixed = fixed;
        f.shares = shares;
        self.accounts.insert(a, f);
    }

    /// `adjustDebt`, with nitro's two's-complement delta.
    fn adjust_debt(&mut self, a: Address, mut value: U256) {
        let mut f = self.get(a);
        if negative(value.wrapping_add(f.debt)) {
            value = U256::ZERO.wrapping_sub(f.debt);
        }
        f.fixed = f.fixed.wrapping_add(value);
        if f.flags == APE_YIELD_AUTOMATIC {
            let amount = f.shares.wrapping_mul(self.price()).wrapping_add(f.fixed);
            assert!(!negative(amount), "underflow detected in amount");
            let (shares, fixed) = ape_shares_and_remainder(self.price(), amount);
            self.adjust_shares(f.shares, shares);
            f.shares = shares;
            f.fixed = fixed;
        } else {
            assert!(!negative(f.fixed), "underflow detected in fixed");
        }
        f.debt = f.debt.wrapping_add(value);
        self.accounts.insert(a, f);
    }

    /// `SetFlags`.
    fn set_flags(&mut self, a: Address, flags: u8, delegate: Option<Address>) {
        if flags == APE_YIELD_DELEGATED && delegate.is_none_or(|d| d == a) {
            return;
        }
        let mut f = self.get(a);
        if flags == f.flags && (flags != APE_YIELD_DELEGATED || delegate == Some(f.delegate)) {
            return;
        }
        let value = match f.flags {
            APE_YIELD_AUTOMATIC => f.shares * self.price() + f.fixed,
            _ => {
                if f.flags == APE_YIELD_DELEGATED {
                    let balance = f.fixed - f.debt;
                    if !balance.is_zero() {
                        self.adjust_debt(f.delegate, U256::ZERO.wrapping_sub(balance));
                    }
                }
                f.fixed
            }
        };
        let (fixed, shares) = match flags {
            APE_YIELD_AUTOMATIC => {
                let (shares, fixed) = ape_shares_and_remainder(self.price(), value);
                f.delegate = Address::ZERO;
                (fixed, shares)
            }
            APE_YIELD_DISABLED => {
                f.delegate = Address::ZERO;
                (value, U256::ZERO)
            }
            _ => {
                let d = delegate.unwrap();
                f.delegate = d;
                let balance = value - f.debt;
                if !balance.is_zero() {
                    self.adjust_debt(d, balance);
                }
                (value, U256::ZERO)
            }
        };
        self.adjust_shares(f.shares, shares);
        f.flags = flags;
        f.fixed = fixed;
        f.shares = shares;
        self.accounts.insert(a, f);
    }

    fn transfer(&mut self, from: Address, to: Address, value: U256) {
        let from_balance = self.balance(from);
        self.set_balance(from, from_balance - value);
        let to_balance = self.balance(to);
        self.set_balance(to, to_balance + value);
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// Up to ~10^25 wei, so balances span many shares.
    fn amount(&mut self) -> U256 {
        U256::from(self.next()) * U256::from(self.next() % 1_000_000_000)
    }
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Transfer(usize, usize, U256),
    Automatic(usize),
    Void(usize),
    Delegate(usize, usize),
}

/// Five funded EOAs and a sixth that starts empty.
fn accounts() -> Vec<Address> {
    (1..=6u8).map(|i| Address::repeat_byte(0x10 + i)).collect()
}

fn random_op(rng: &mut Rng, model: &Model, accounts: &[Address]) -> Op {
    let a = rng.below(accounts.len());
    match rng.below(10) {
        0..=4 => {
            let balance = model.balance(accounts[a]);
            if balance.is_zero() {
                return Op::Automatic(a);
            }
            // At least 1 wei, so no account the transfer touches ends up empty.
            let value = match rng.below(4) {
                0 => balance,
                1 => U256::from(1),
                _ => rng.amount() % balance + U256::from(1),
            };
            Op::Transfer(a, rng.below(accounts.len()), value.min(balance))
        }
        5 | 6 => Op::Delegate(a, rng.below(accounts.len())),
        7 => Op::Void(a),
        _ => Op::Automatic(a),
    }
}

fn apply(model: &mut Model, accounts: &[Address], op: Op) {
    match op {
        Op::Transfer(from, to, value) => model.transfer(accounts[from], accounts[to], value),
        Op::Automatic(a) => model.set_flags(accounts[a], APE_YIELD_AUTOMATIC, None),
        Op::Void(a) => model.set_flags(accounts[a], APE_YIELD_DISABLED, None),
        Op::Delegate(a, d) => model.set_flags(accounts[a], APE_YIELD_DELEGATED, Some(accounts[d])),
    }
}

fn tx(op: Op, accounts: &[Address]) -> ArbitrumTxEnv {
    let (caller, to, value, data) = match op {
        Op::Transfer(from, to, value) => (accounts[from], accounts[to], value, Vec::new()),
        Op::Automatic(a) => (
            accounts[a],
            ARB_INFO_ADDRESS,
            U256::ZERO,
            configureAutomaticYieldCall {}.abi_encode(),
        ),
        Op::Void(a) => (
            accounts[a],
            ARB_INFO_ADDRESS,
            U256::ZERO,
            configureVoidYieldCall {}.abi_encode(),
        ),
        Op::Delegate(a, d) => (
            accounts[a],
            ARB_INFO_ADDRESS,
            U256::ZERO,
            configureDelegateYieldCall {
                delegate: accounts[d],
            }
            .abi_encode(),
        ),
    };
    ArbitrumTxEnv::new(
        TxEnv {
            caller,
            gas_limit: 1_000_000,
            kind: TxKind::Call(to),
            value,
            data: Bytes::from(data),
            ..Default::default()
        },
        Default::default(),
    )
}

/// The writer's view of `model`: balance 0 is resolved through the yield slots,
/// so the database holds the resolved balances and the raw fields side by side.
fn evm(model: &Model) -> ArbitrumEvm<CacheDB<EmptyDB>, ()> {
    let mut db = CacheDB::new(EmptyDB::default());
    let root = |offset| arbos_state::slot_at(&[], offset);
    for (offset, value) in [
        (arbos_state::ARBOS_VERSION_OFFSET, U256::from(59)),
        (APE_SHARE_PRICE_OFFSET, U256::from(PRICE)),
        (APE_SHARE_COUNT_OFFSET, model.share_count),
    ] {
        db.insert_account_storage(arbos_state::ARBOS_STATE_ADDRESS, root(offset), value)
            .unwrap();
    }
    // `ApeYieldStorage` reports the reserved account with nonce 1.
    db.insert_account_info(
        APE_YIELD_ADDRESS,
        AccountInfo {
            nonce: 1,
            ..Default::default()
        },
    );
    for (&a, f) in &model.accounts {
        db.insert_account_info(
            a,
            AccountInfo {
                balance: model.balance(a),
                nonce: 1,
                ..Default::default()
            },
        );
        let hash = keccak256(a);
        for (i, value) in f.to_slots().into_iter().enumerate() {
            db.insert_account_storage(APE_YIELD_ADDRESS, ape_yield_slot(hash, i), value)
                .unwrap();
        }
    }
    let mut cfg = CfgEnv::new_with_spec(ArbitrumHardfork::Prague);
    cfg.disable_nonce_check = true;
    ArbitrumEvm::new(
        BlockEnv {
            gas_limit: 30_000_000,
            ..Default::default()
        },
        cfg,
        db,
        (),
        ArbitrumPrecompileEnv {
            current_arbos_version: 59,
            ape_yield: true,
            ..Default::default()
        },
        ArbitrumExecutionContext::default(),
    )
}

/// Committed balances, yield slots and `shareCount` must equal the model's.
fn check(
    evm: &mut ArbitrumEvm<CacheDB<EmptyDB>, ()>,
    model: &Model,
    accounts: &[Address],
    step: &str,
) {
    let db = &evm.inner.ctx.db_mut().cache.accounts;
    let slot = |a: Address, i: usize| {
        db.get(&APE_YIELD_ADDRESS)
            .and_then(|acc| acc.storage.get(&ape_yield_slot(keccak256(a), i)).copied())
            .unwrap_or_default()
    };
    for &a in accounts {
        let balance = db.get(&a).map(|acc| acc.info.balance).unwrap_or_default();
        assert_eq!(balance, model.balance(a), "{step}: balance of {a}");
        let fields = ApeYieldFields::from_slots(std::array::from_fn(|i| slot(a, i)));
        assert_eq!(fields, model.get(a), "{step}: yield fields of {a}");
    }
    let count = db
        .get(&arbos_state::ARBOS_STATE_ADDRESS)
        .and_then(|acc| {
            acc.storage
                .get(&arbos_state::slot_at(&[], APE_SHARE_COUNT_OFFSET))
                .copied()
        })
        .unwrap_or_default();
    assert_eq!(count, model.share_count, "{step}: shareCount");
}

fn run(model: &mut Model, ops: &[Op]) {
    let accounts = accounts();
    let mut evm = evm(model);
    for (i, &op) in ops.iter().enumerate() {
        let result = evm.transact_commit(tx(op, &accounts)).expect("execute");
        assert!(result.is_success(), "op {i} {op:?}: {result:?}");
        apply(model, &accounts, op);
        check(&mut evm, model, &accounts, &format!("op {i} {op:?}"));
    }
}

fn funded(seed: u64) -> (Model, Rng) {
    let mut rng = Rng(seed);
    let mut model = Model::default();
    for a in &accounts()[..5] {
        let amount = rng.amount();
        model.set_balance(*a, amount);
    }
    (model, rng)
}

#[test]
fn delegated_spend_from_an_automatic_delegate_matches_nitro() {
    let (mut model, _) = funded(7);
    // Account 1 ends up with twice account 0's balance; it delegates to the
    // empty account 5, then spends a third of it.
    let d = model.balance(accounts()[0]);
    run(
        &mut model,
        &[
            Op::Transfer(0, 1, d),
            Op::Delegate(1, 5),
            Op::Transfer(1, 2, d / U256::from(3)),
            Op::Transfer(2, 1, U256::from(12_345)),
            Op::Automatic(1),
            Op::Delegate(1, 0),
            Op::Void(0),
            Op::Transfer(1, 3, U256::from(1)),
            Op::Automatic(1),
        ],
    );
}

#[test]
fn random_transfers_and_yield_modes_match_nitro() {
    let accounts = accounts();
    for seed in 1..=40u64 {
        let (mut model, mut rng) = funded(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        // Mixed yield modes before the EVM sees the state.
        for _ in 0..12 {
            let op = random_op(&mut rng, &model, &accounts);
            apply(&mut model, &accounts, op);
        }
        let ops: Vec<Op> = {
            let mut shadow = Model {
                accounts: model.accounts.clone(),
                share_count: model.share_count,
            };
            (0..60)
                .map(|_| {
                    let op = random_op(&mut rng, &shadow, &accounts);
                    apply(&mut shadow, &accounts, op);
                    op
                })
                .collect()
        };
        run(&mut model, &ops);
    }
}
