//! Regression coverage for lazy TIP-1115 balance probing in the production handler.
use super::*;
use crate::tempo::precompile::{TIP_FEE_MANAGER_ADDRESS, storage_types::StorageKey};
use crate::tempo::precompile::{fee_manager::IFeeManager, tip20::ITIP20};
use alloy::primitives::{Address, B256, TxKind, address};
use alloy::sol_types::SolCall;
use alloy_evm::EvmEnv;
use revm::{
    context::CfgEnv,
    database::{CacheDB, EmptyDB},
    inspector::NoOpInspector,
    state::{AccountInfo, Bytecode},
};

#[derive(Debug)]
struct InjectedReadError;
impl core::fmt::Display for InjectedReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("injected fallback balance read failure")
    }
}
impl core::error::Error for InjectedReadError {}
impl revm::database_interface::DBErrorMarker for InjectedReadError {}

#[derive(Debug)]
struct FaultDb {
    inner: CacheDB<EmptyDB>,
    fail: Option<(Address, U256)>,
    reads: Vec<(Address, U256)>,
}
impl revm::Database for FaultDb {
    type Error = InjectedReadError;
    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        Ok(self.inner.basic(address).unwrap())
    }
    fn code_by_hash(&mut self, hash: B256) -> Result<Bytecode, Self::Error> {
        Ok(self.inner.code_by_hash(hash).unwrap())
    }
    fn storage(&mut self, address: Address, slot: U256) -> Result<U256, Self::Error> {
        self.reads.push((address, slot));
        if self.fail == Some((address, slot)) {
            return Err(InjectedReadError);
        }
        Ok(self.inner.storage(address, slot).unwrap())
    }
    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        Ok(self.inner.block_hash(number).unwrap())
    }
}

#[test]
fn fee_token_probe_skips_unneeded_balances_and_propagates_needed_errors() {
    let payer = Address::repeat_byte(0x11);
    let selected = address!("20c0000000000000000000000000000000000011");
    let [path, ousd] = *TempoHardfork::T13.fallback_fee_tokens() else {
        panic!()
    };
    let balance_slot = payer.mapping_slot(U256::from(9));
    let mut usd = [0u8; 32];
    usd[..3].copy_from_slice(b"USD");
    usd[31] = 6;
    // mode: explicit, stored preference, or fallback.
    let cases = [
        (
            "explicit/control",
            0,
            None,
            false,
            TempoHardfork::T13,
            1_000_000_000_000u128,
            false,
        ),
        (
            "explicit/path-fault",
            0,
            Some(path),
            false,
            TempoHardfork::T13,
            1_000_000_000_000,
            false,
        ),
        (
            "explicit/ousd-fault",
            0,
            Some(ousd),
            false,
            TempoHardfork::T13,
            1_000_000_000_000,
            false,
        ),
        (
            "preference/ousd-fault",
            1,
            Some(ousd),
            false,
            TempoHardfork::T13,
            1_000_000_000_000,
            false,
        ),
        (
            "fallback/path-sufficient/ousd-fault",
            2,
            Some(ousd),
            false,
            TempoHardfork::T13,
            1_000_000_000_000,
            false,
        ),
        (
            "explicit/balance-check-on",
            0,
            Some(ousd),
            true,
            TempoHardfork::T13,
            1_000_000_000_000,
            false,
        ),
        (
            "explicit/T12",
            0,
            Some(ousd),
            false,
            TempoHardfork::T12,
            1_000_000_000_000,
            false,
        ),
        (
            "explicit/free",
            0,
            Some(ousd),
            false,
            TempoHardfork::T13,
            0,
            false,
        ),
        (
            "inferred/ousd-fault",
            3,
            Some(ousd),
            false,
            TempoHardfork::T13,
            1_000_000_000_000,
            false,
        ),
        (
            "preference-update/ousd-fault",
            4,
            Some(ousd),
            false,
            TempoHardfork::T13,
            1_000_000_000_000,
            false,
        ),
        (
            "fallback/path-needed/path-fault",
            2,
            Some(path),
            false,
            TempoHardfork::T13,
            1_000_000_000_000,
            true,
        ),
        (
            "fallback/path-short/ousd-fault",
            2,
            Some(ousd),
            false,
            TempoHardfork::T13,
            1_000_000_000_000,
            true,
        ),
        (
            "fallback/control",
            2,
            None,
            false,
            TempoHardfork::T13,
            1_000_000_000_000,
            false,
        ),
    ];
    for (name, mode, fail, balance_check, spec, gas_price, expect_error) in cases {
        let mut inner = CacheDB::new(EmptyDB::default());
        for token in [selected, path, ousd] {
            inner
                .insert_account_storage(token, U256::from(4), U256::from_be_bytes(usd))
                .unwrap();
            inner
                .insert_account_storage(token, balance_slot, U256::from(2_000_000))
                .unwrap();
        }
        if name.contains("path-short") {
            inner
                .insert_account_storage(path, balance_slot, U256::ZERO)
                .unwrap();
        }
        if mode == 1 {
            inner
                .insert_account_storage(
                    TIP_FEE_MANAGER_ADDRESS,
                    payer.mapping_slot(U256::ONE),
                    U256::from_be_slice(selected.as_slice()),
                )
                .unwrap();
        }
        let db = FaultDb {
            inner,
            fail: fail.map(|token| (token, balance_slot)),
            reads: vec![],
        };
        let mut cfg = CfgEnv::new_with_spec(spec);
        cfg.chain_id = 4217;
        cfg.disable_balance_check = !balance_check;
        let mut evm = TempoEvm::new(
            EvmEnv::new(cfg, BlockEnv::default()),
            db,
            NoOpInspector,
            false,
        );
        // TempoEvm derives the fork from block timestamp on construction, as in production.
        // This helper-level test selects the fork explicitly, matching existing exec tests.
        evm.ctx_mut().cfg.spec = spec;
        evm.ctx_mut().tx = TempoTxEnv {
            base: revm::context::TxEnv {
                caller: payer,
                kind: TxKind::Call(match mode {
                    3 => selected,
                    4 => TIP_FEE_MANAGER_ADDRESS,
                    _ => Address::repeat_byte(0x33),
                }),
                data: match mode {
                    3 => ITIP20::transferCall {
                        to: Address::repeat_byte(0x22),
                        amount: U256::ONE,
                    }
                    .abi_encode()
                    .into(),
                    4 => IFeeManager::setUserTokenCall { token: selected }
                        .abi_encode()
                        .into(),
                    _ => Default::default(),
                },
                gas_limit: 1_000_000,
                gas_price,
                ..Default::default()
            },
            tempo_fields: (mode == 0).then(|| TempoTxFields {
                fee_token: Some(selected),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = warm_fee_token_balance(&mut evm);
        let db = evm.ctx_mut().journal_mut().db_mut();
        let fallback_reads: Vec<_> = db
            .reads
            .iter()
            .filter(|(a, s)| (*a == path || *a == ousd) && *s == balance_slot)
            .collect();
        println!("{name}: result={result:?}; fallback_reads={fallback_reads:?}");
        assert_eq!(
            matches!(result, Err(EVMError::Database(InjectedReadError))),
            expect_error,
            "{name}"
        );
        if mode != 2 {
            assert!(
                fallback_reads.is_empty(),
                "{name}: selected tokens must not probe fallback balances"
            );
        } else if !expect_error {
            assert!(
                fallback_reads.iter().all(|(token, _)| *token == path),
                "{name}: pathUSD covers the fee"
            );
        }
        if !expect_error {
            result.unwrap();
            assert_eq!(
                evm.ctx().tx.resolved_fee_token,
                Some(if mode == 2 { path } else { selected }),
                "{name}"
            );
        }
    }
}
