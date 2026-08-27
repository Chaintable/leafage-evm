//! TIP-1070 current committee precompile (T8+).

use alloy::primitives::{Address, B256, U256};
use alloy::sol_types::{SolError, SolInterface};
use revm::precompile::{PrecompileError, PrecompileResult};

use super::error::{Result, TempoPrecompileError};
use super::storage::{ContractStorage, StorageCtx};
use super::storage_types::{Handler, Slot, VecHandler};
use super::{
    dispatch_call, input_cost, mutate_void, unknown_selector, view, Precompile,
    CURRENT_COMMITTEE_ADDRESS,
};

alloy::sol! {
    #[derive(Debug, PartialEq, Eq)]
    interface ICurrentCommittee {
        error Unauthorized();

        function getCommitteeMembers()
            external
            view
            returns (uint64 epoch, bytes32[] memory publicKeys);

        function setCommitteeMembers(uint64 epoch, bytes32[] calldata publicKeys) external;
    }
}

pub struct CurrentCommittee {
    pub epoch: Slot<u64>,
    pub public_keys: VecHandler<B256>,
    pub address: Address,
    pub storage: StorageCtx,
}

impl CurrentCommittee {
    pub fn new() -> Self {
        let address = CURRENT_COMMITTEE_ADDRESS;
        Self {
            epoch: Slot::new(U256::ZERO, address),
            public_keys: VecHandler::new(U256::from(1), address),
            address,
            storage: StorageCtx::default(),
        }
    }

    pub fn get_committee_members(&self) -> Result<ICurrentCommittee::getCommitteeMembersReturn> {
        Ok(ICurrentCommittee::getCommitteeMembersReturn {
            epoch: self.epoch.read()?,
            publicKeys: self.public_keys.read()?,
        })
    }

    pub fn set_committee_members(
        &mut self,
        msg_sender: Address,
        call: ICurrentCommittee::setCommitteeMembersCall,
    ) -> Result<()> {
        if msg_sender != Address::ZERO {
            return Err(TempoPrecompileError::Revert(
                ICurrentCommittee::Unauthorized {}.abi_encode().into(),
            ));
        }

        // This is a system-only update: it must neither mint nor consume TIP-1060 credits.
        self.storage.set_tip1060_storage_credits(false);
        self.epoch.write(call.epoch)?;
        self.public_keys.write(call.publicKeys)?;
        Ok(())
    }
}

impl ContractStorage for CurrentCommittee {
    fn address(&self) -> Address {
        self.address
    }

    fn storage(&self) -> &StorageCtx {
        &self.storage
    }

    fn storage_mut(&mut self) -> &mut StorageCtx {
        &mut self.storage
    }
}

impl Precompile for CurrentCommittee {
    fn call(&mut self, calldata: &[u8], msg_sender: Address) -> PrecompileResult {
        if !self.storage.spec().is_t8() {
            let selector = calldata
                .get(..4)
                .and_then(|s| s.try_into().ok())
                .unwrap_or([0; 4]);
            return unknown_selector(selector, 0);
        }

        self.storage
            .deduct_gas(input_cost(calldata.len()))
            .map_err(|_| PrecompileError::OutOfGas)?;

        dispatch_call(
            calldata,
            |data| {
                ICurrentCommittee::ICurrentCommitteeCalls::abi_decode_with_config(
                    data,
                    super::abi_decoder_config(),
                )
            },
            |call| match call {
                ICurrentCommittee::ICurrentCommitteeCalls::getCommitteeMembers(call) => {
                    view(call, |_| self.get_committee_members())
                }
                ICurrentCommittee::ICurrentCommitteeCalls::setCommitteeMembers(call) => {
                    mutate_void(call, msg_sender, |sender, call| {
                        self.set_committee_members(sender, call)
                    })
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::sol_types::SolCall;

    use crate::tempo::hardfork::TempoHardfork;
    use crate::tempo::precompile::test_utils::TestStorageProvider;

    #[test]
    fn storage_layout_matches_writer() {
        let committee = CurrentCommittee::new();
        assert_eq!(committee.epoch.slot(), U256::ZERO);
        assert_eq!(committee.public_keys.len_slot(), U256::from(1));
    }

    #[test]
    fn activation_gate_is_t8() {
        assert!(!TempoHardfork::T7.is_t8());
        assert!(TempoHardfork::T8.is_t8());
        assert!(TempoHardfork::T10.is_t8());
    }

    #[test]
    fn only_system_caller_can_replace_committee() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T8);
        let call = ICurrentCommittee::setCommitteeMembersCall {
            epoch: 42,
            publicKeys: vec![B256::repeat_byte(0x11), B256::repeat_byte(0x22)],
        };

        StorageCtx::enter(&mut provider, || {
            let mut committee = CurrentCommittee::new();
            let unauthorized = committee
                .call(&call.abi_encode(), Address::repeat_byte(1))
                .unwrap();
            assert!(unauthorized.reverted);
            ICurrentCommittee::Unauthorized::abi_decode(&unauthorized.bytes).unwrap();

            let system = committee.call(&call.abi_encode(), Address::ZERO).unwrap();
            assert!(!system.reverted);

            let members = committee.get_committee_members()?;
            assert_eq!(members.epoch, call.epoch);
            assert_eq!(members.publicKeys, call.publicKeys);
            Result::<()>::Ok(())
        })
        .unwrap();
    }

    #[test]
    fn replacement_clears_removed_public_keys() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T8);

        StorageCtx::enter(&mut provider, || {
            let mut committee = CurrentCommittee::new();
            committee.set_committee_members(
                Address::ZERO,
                ICurrentCommittee::setCommitteeMembersCall {
                    epoch: 1,
                    publicKeys: vec![B256::repeat_byte(1), B256::repeat_byte(2)],
                },
            )?;
            committee.set_committee_members(
                Address::ZERO,
                ICurrentCommittee::setCommitteeMembersCall {
                    epoch: 2,
                    publicKeys: vec![B256::repeat_byte(3)],
                },
            )?;

            let members = committee.get_committee_members()?;
            assert_eq!(members.epoch, 2);
            assert_eq!(members.publicKeys, vec![B256::repeat_byte(3)]);
            assert_eq!(
                super::super::storage_credits::StorageCredits::new()
                    .balance_of(CURRENT_COMMITTEE_ADDRESS)?,
                0,
            );
            Result::<()>::Ok(())
        })
        .unwrap();

        let data_slot =
            U256::from_be_bytes(alloy::primitives::keccak256(U256::from(1).to_be_bytes::<32>()).0);
        assert_eq!(
            provider.storage(CURRENT_COMMITTEE_ADDRESS, data_slot + U256::ONE),
            U256::ZERO,
        );
    }

    #[test]
    fn dispatch_rejects_calls_before_t8() {
        let mut provider = TestStorageProvider::new(TempoHardfork::T7);
        let call = ICurrentCommittee::getCommitteeMembersCall {};

        let output = StorageCtx::enter(&mut provider, || {
            CurrentCommittee::new().call(&call.abi_encode(), Address::ZERO)
        })
        .unwrap();

        assert!(output.reverted);
    }
}
