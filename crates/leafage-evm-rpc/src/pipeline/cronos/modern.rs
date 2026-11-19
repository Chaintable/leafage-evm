//! Cronos v1.7 installs the history contract before BeginBlock. Ethermint writes
//! the current header hash directly to its storage. The bytecode and keeper use
//! different default moduli: the deployed code reads modulo 8191, while the
//! v1.7 keeper writes modulo 8192 when history_serve_window is zero.
//! https://github.com/crypto-org-chain/cronos/blob/v1.7.0/app/upgrades.go
//! https://github.com/crypto-org-chain/ethermint/blob/9e97913655b0/x/evm/keeper/keeper.go
use super::CronosArchiveRead;
use alloy::primitives::{address, bytes, Address, B256, U256};
use anyhow::{ensure, Context, Result};
use revm::{
    bytecode::Bytecode,
    context_interface::{Cfg, ContextTr, JournalTr, Transaction},
    database::CacheDB,
    handler::{EthFrame, EvmTr, EvmTrError, Handler, MainnetHandler},
    interpreter::{interpreter::EthInterpreter, interpreter_action::FrameInit, GasTracker},
    primitives::hardfork::SpecId,
    state::EvmState,
    DatabaseRef,
};

/// Ethermint invokes geth EVM.Call directly after authorizations. Its top-level
/// delegation code lookup does not warm the delegate. REVM's initial frame
/// construction does. Preserve warmth from the access list, precompiles,
/// beneficiary and successfully recovered authorization authorities.
pub(super) fn first_frame_input<E, Error>(
    evm: &mut E,
    gas: &mut GasTracker,
) -> Result<Option<FrameInit>, Error>
where
    E: EvmTr<
        Context: ContextTr<Journal: JournalTr<State = EvmState>>,
        Frame = EthFrame<EthInterpreter>,
    >,
    Error: EvmTrError<E>,
{
    let mut cold_delegate = None;
    if evm.ctx_ref().cfg().spec().into() == SpecId::PRAGUE {
        let (tx, journal) = evm.ctx_mut().tx_journal_mut();
        if let Some(to) = tx.kind().to() {
            let target = journal
                .load_account_with_code(*to)?
                .info
                .code
                .as_ref()
                .and_then(Bytecode::eip7702_address);
            if let Some(delegate) = target {
                cold_delegate = journal.load_account(delegate)?.is_cold.then_some(delegate);
            }
        }
    }

    let frame = MainnetHandler::<E, Error, E::Frame>::default().first_frame_input(evm, gas)?;
    if let Some(delegate) = cold_delegate {
        evm.ctx_mut()
            .journal_mut()
            .evm_state_mut()
            .get_mut(&delegate)
            .expect("initial frame loaded the delegate")
            .mark_cold();
    }
    Ok(frame)
}

const HISTORY_ADDRESS: Address = address!("0000f90827f1c53a10cb7a02335b175320002935");
const INSTALL_HEIGHT: u64 = 58_825_800;

/// Pallene migrated to SDK v0.50, whose staking HistoricalInfo holds a
/// stripped header without validators_hash. Its Comet Header.Hash() is nil.
/// Before v1.7 began filling the EVM ring, BeginBlock deleted the old height
/// key for exactly H-256. Other in-range heights still hit the retained key.
/// Heights predating Pallene have full staking headers even after migration.
pub(super) fn prepare_block_hash<D: DatabaseRef>(db: &mut CacheDB<D>, height: u64) {
    let Some(oldest) = height.checked_sub(256) else {
        return;
    };
    if (17_155_000..INSTALL_HEIGHT).contains(&oldest) {
        db.cache.block_hashes.insert(U256::from(oldest), B256::ZERO);
    }
}

fn history_code() -> Bytecode {
    Bytecode::new_legacy(bytes!(
        "3373fffffffffffffffffffffffffffffffffffffffe14604657602036036042575f35600143038111604257611fff81430311604257611fff9006545f5260205ff35b5f5ffd5b5f35611fff60014303065500"
    ))
}

pub(super) fn prepare_history<D: DatabaseRef>(
    db: &mut CacheDB<D>,
    height: u64,
    header_hash: B256,
    history_serve_window: u64,
) -> Result<()> {
    let account = db
        .load_account(HISTORY_ADDRESS)
        .context(CronosArchiveRead)?;
    if height == INSTALL_HEIGHT {
        ensure!(
            account.info.nonce == 0 && account.info.is_empty_code_hash(),
            "history preinstall address already has code or nonce"
        );
        let info = account.info.clone().with_code(history_code());
        db.insert_account_info(HISTORY_ADDRESS, info);
    } else if account.info.is_empty_code_hash() {
        // Without a contract the keeper falls back to its Cosmos KV store.
        return Ok(());
    }
    let window = match history_serve_window {
        0 => 8192,
        n => n,
    };
    db.insert_account_storage(
        HISTORY_ADDRESS,
        U256::from(height % window),
        U256::from_be_bytes(header_hash.0),
    )
    .context(CronosArchiveRead)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::b256;
    use revm::{database::EmptyDB, state::AccountInfo, Database};

    #[test]
    fn oldest_hash_exposes_zero_only_for_the_pruned_header_range() {
        for (height, pruned) in [
            (17_155_255, false),
            (17_155_256, true),
            (INSTALL_HEIGHT, true),
            (INSTALL_HEIGHT + 255, true),
            (INSTALL_HEIGHT + 256, false),
        ] {
            let mut db = CacheDB::new(EmptyDB::default());
            let oldest = U256::from(height - 256);
            prepare_block_hash(&mut db, height);
            assert_eq!(
                db.cache.block_hashes.get(&oldest),
                pruned.then_some(&B256::ZERO)
            );
            assert_eq!(db.cache.block_hashes.len(), usize::from(pruned));
        }
    }

    #[test]
    fn top_level_delegation_preserves_the_implementation_address_access_cost() {
        use super::super::{EthermintHandler, Warm};
        use revm::{context::TxEnv, primitives::TxKind, MainBuilder, MainContext};
        use std::marker::PhantomData;

        let authority = Address::with_last_byte(0x20);
        let delegate = Address::with_last_byte(0x21);
        for (already_warm, expected) in [(false, 23_605), (true, 21_105)] {
            let mut db = CacheDB::new(EmptyDB::default());
            db.insert_account_info(
                authority,
                AccountInfo::default().with_code(Bytecode::new_eip7702(delegate)),
            );
            // PUSH1 delegate, BALANCE, POP, STOP.
            db.insert_account_info(
                delegate,
                AccountInfo::default().with_code(Bytecode::new_legacy(bytes!("6021315000"))),
            );
            let mut evm = revm::Context::mainnet()
                .with_db(db)
                .modify_cfg_chained(|cfg| cfg.set_spec_and_mainnet_gas_params(SpecId::PRAGUE))
                .build_mainnet();
            evm.ctx.tx = TxEnv::builder()
                .caller(Address::with_last_byte(0xff))
                .kind(TxKind::Call(authority))
                .gas_limit(100_000)
                .gas_price(0)
                .build()
                .unwrap();
            let mut warm = Warm::default();
            if already_warm {
                warm.entry(delegate).or_default();
            }
            let mut handler = EthermintHandler {
                warm,
                marker: PhantomData,
            };
            let result = handler.run(&mut evm).unwrap();
            assert!(result.is_success());
            // Both cases charge 50,000 on mainnet with a 0.5 multiplier.
            // Checking execution gas catches an otherwise invisible cold access.
            assert_eq!(result.gas().total_gas_spent(), expected);
        }
    }

    #[test]
    fn upgrade_installs_code_and_writes_current_hash_at_keeper_index() {
        // eth_getStorageAt(H % 8192, H) and eth_getCode at H=58,825,800.
        let hash = b256!("6a0197a54ce2dee70298dfd1c0e327c4c2c0618442831ba74d1a46eee7031e10");
        let mut db = CacheDB::new(EmptyDB::default());
        let balance = U256::from(11);
        db.insert_account_info(
            HISTORY_ADDRESS,
            AccountInfo::default().with_balance(balance),
        );
        prepare_history(&mut db, INSTALL_HEIGHT, hash, 0).unwrap();
        let info = db.basic(HISTORY_ADDRESS).unwrap().unwrap();
        assert_eq!(info.code_hash, history_code().hash_slow());
        assert_eq!(info.nonce, 0);
        assert_eq!(info.balance, balance);
        assert_eq!(
            db.storage(HISTORY_ADDRESS, U256::from(INSTALL_HEIGHT % 8192))
                .unwrap(),
            U256::from_be_bytes(hash.0)
        );
        assert_eq!(
            db.storage(HISTORY_ADDRESS, U256::from(INSTALL_HEIGHT % 8191))
                .unwrap(),
            U256::ZERO
        );
    }

    #[test]
    fn stored_window_overrides_default_and_absent_contract_uses_cosmos_store() {
        let mut db = CacheDB::new(EmptyDB::default());
        let height = INSTALL_HEIGHT + 1;
        let hash = B256::repeat_byte(0x55);
        let index = U256::from(height % 123);
        prepare_history(&mut db, height, hash, 123).unwrap();
        assert_eq!(db.storage(HISTORY_ADDRESS, index).unwrap(), U256::ZERO);
        db.insert_account_info(
            HISTORY_ADDRESS,
            AccountInfo::default().with_code(history_code()),
        );
        prepare_history(&mut db, height, hash, 123).unwrap();
        assert_eq!(
            db.storage(HISTORY_ADDRESS, index).unwrap(),
            U256::from_be_bytes(hash.0)
        );
    }
}
