use crate::api_impl::core::StateOverrideEndpoint;
use crate::error::{internal_rpc_err, invalid_params_rpc_err};
use alloy::primitives::{keccak256, Address};
use alloy::rpc::types::state::{AccountOverride, StateOverride};
use jsonrpsee::core::RpcResult;
use leafage_evm_types::Bytecode;
use revm::database::{CacheDB, DatabaseRef};
use revm::state::{Account, AccountStatus, EvmStorageSlot};
use revm::{Database, DatabaseCommit};
use std::collections::HashMap;

pub(super) fn apply<DB>(
    endpoint: StateOverrideEndpoint,
    overrides: StateOverride,
    db: &mut CacheDB<DB>,
) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    for (account, account_override) in overrides {
        apply_account(endpoint, account, account_override, db)?;
    }
    Ok(())
}

fn apply_account<DB>(
    endpoint: StateOverrideEndpoint,
    account: Address,
    account_override: AccountOverride,
    db: &mut CacheDB<DB>,
) -> RpcResult<()>
where
    DB: DatabaseRef,
{
    let mut info = db
        .basic(account)
        .map_err(|error| match endpoint {
            StateOverrideEndpoint::EthCall => internal_rpc_err(error.to_string()),
            StateOverrideEndpoint::DebankCall => {
                internal_rpc_err("Failed to get basic account info")
            }
        })?
        .unwrap_or_default();

    if let Some(nonce) = account_override.nonce {
        info.nonce = nonce;
    }
    if let Some(code) = account_override.code {
        info.code_hash = keccak256(&code);
        info.code = Some(Bytecode::new_raw_checked(code).map_err(|error| {
            let message = match endpoint {
                StateOverrideEndpoint::EthCall => format!("Invalid bytecode: {error}"),
                StateOverrideEndpoint::DebankCall => format!("Invalid bytecode {error}"),
            };
            invalid_params_rpc_err(message)
        })?);
    }
    if let Some(balance) = account_override.balance {
        info.balance = balance;
    }

    let mut account_state = Account {
        info: info.clone(),
        original_info: Box::new(info),
        status: AccountStatus::Touched,
        storage: HashMap::default(),
        transaction_id: 0,
    };

    let storage_diff = match (account_override.state, account_override.state_diff) {
        (Some(_), Some(_)) => {
            return Err(invalid_params_rpc_err(format!(
                "account {:?} has both 'state' and 'stateDiff'",
                account
            )))
        }
        (None, None) => None,
        (Some(state), None) => {
            db.commit(HashMap::from_iter([(
                account,
                Account {
                    status: AccountStatus::SelfDestructed | AccountStatus::Touched,
                    ..Default::default()
                },
            )]));
            account_state.mark_created();
            Some(state)
        }
        (None, Some(state)) => {
            if account_state.info.is_empty() && !state.is_empty() {
                account_state.mark_created();
            }
            Some(state)
        }
    };

    if let Some(state) = storage_diff {
        for (slot, value) in state {
            account_state.storage.insert(
                slot.into(),
                EvmStorageSlot {
                    original_value: (!value).into(),
                    present_value: value.into(),
                    transaction_id: 0,
                    is_cold: false,
                },
            );
        }
    }

    db.commit(HashMap::from_iter([(account, account_state)]));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::{Bytes, U256};
    use revm::database::EmptyDB;
    use revm::database_interface::DBErrorMarker;
    use revm::state::AccountInfo;

    #[derive(Debug, thiserror::Error)]
    #[error("injected state override database failure")]
    struct OverrideDbError;
    impl DBErrorMarker for OverrideDbError {}

    #[derive(Debug)]
    struct FailingOverrideDb;

    impl DatabaseRef for &FailingOverrideDb {
        type Error = OverrideDbError;

        fn basic_ref(&self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Err(OverrideDbError)
        }

        fn code_by_hash_ref(
            &self,
            _code_hash: alloy::primitives::B256,
        ) -> Result<Bytecode, Self::Error> {
            Err(OverrideDbError)
        }

        fn storage_ref(&self, _address: Address, _index: U256) -> Result<U256, Self::Error> {
            Err(OverrideDbError)
        }

        fn block_hash_ref(&self, _number: u64) -> Result<alloy::primitives::B256, Self::Error> {
            Err(OverrideDbError)
        }
    }

    #[test]
    fn updates_code_hash_and_preserves_endpoint_errors() {
        let address = Address::with_last_byte(1);
        let code = Bytes::from_static(&[0x60, 0x2a, 0x5f, 0x52, 0x60, 0x20, 0x5f, 0xf3]);
        let mut overrides = StateOverride::default();
        overrides.insert(address, AccountOverride::default().with_code(code.clone()));

        for endpoint in [
            StateOverrideEndpoint::EthCall,
            StateOverrideEndpoint::DebankCall,
        ] {
            let mut db = CacheDB::new(EmptyDB::default());
            apply(endpoint, overrides.clone(), &mut db).unwrap();
            let info = db.basic(address).unwrap().unwrap();
            assert_eq!(info.code_hash, keccak256(&code));
            assert_eq!(info.code.unwrap().original_bytes(), code);
        }

        let mut balance_override = StateOverride::default();
        balance_override.insert(address, AccountOverride::default().with_balance(U256::ONE));

        let mut db = CacheDB::new(&FailingOverrideDb);
        let error = apply(
            StateOverrideEndpoint::EthCall,
            balance_override.clone(),
            &mut db,
        )
        .unwrap_err();
        assert_eq!(error.code(), -32603);
        assert_eq!(error.message(), "injected state override database failure");

        let mut db = CacheDB::new(&FailingOverrideDb);
        let error =
            apply(StateOverrideEndpoint::DebankCall, balance_override, &mut db).unwrap_err();
        assert_eq!(error.code(), -32603);
        assert_eq!(error.message(), "Failed to get basic account info");
    }

    #[test]
    fn keeps_endpoint_specific_invalid_bytecode_prefixes() {
        let address = Address::with_last_byte(1);
        let mut overrides = StateOverride::default();
        overrides.insert(
            address,
            AccountOverride::default().with_code(Bytes::from_static(&[0xef, 0x01])),
        );
        let mut db = CacheDB::new(EmptyDB::default());
        let error = apply(StateOverrideEndpoint::EthCall, overrides.clone(), &mut db).unwrap_err();

        assert_eq!(error.code(), jsonrpsee::types::error::INVALID_PARAMS_CODE);
        assert!(error.message().starts_with("Invalid bytecode: "));

        let mut db = CacheDB::new(EmptyDB::default());
        let error = apply(StateOverrideEndpoint::DebankCall, overrides, &mut db).unwrap_err();

        assert_eq!(error.code(), jsonrpsee::types::error::INVALID_PARAMS_CODE);
        assert!(error.message().starts_with("Invalid bytecode "));
        assert!(!error.message().starts_with("Invalid bytecode: "));
    }
}
