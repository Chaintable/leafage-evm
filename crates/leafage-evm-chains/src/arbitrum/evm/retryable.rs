use crate::arbitrum::arbos_state::{
    self, ARBOS_STATE_ADDRESS, NETWORK_FEE_ACCOUNT_OFFSET, RETRYABLE_SUBSPACE,
};
use crate::arbitrum::precompile::{
    retryable_redeem_scheduled_log, retryable_ticket_created_log, ArbitrumContext,
    ARB_RETRYABLE_TX_ADDRESS,
};
use crate::arbitrum::tx::{ArbitrumRetryTx, ArbitrumSubmitRetryableTx};
use alloy::primitives::{keccak256, Address, Log, B256, U256};
use alloy::sol_types::SolValue;
use revm::context::{Block, Cfg, ContextTr, JournalTr};
use revm::context_interface::{
    journaled_state::account::JournaledAccountTr,
    result::{EVMError, HaltReason},
};
use revm::Database;

const MIN_TRANSACTION_GAS: u64 = 21_000;
const RETRYABLE_LIFETIME_SECONDS: u64 = 7 * 24 * 60 * 60;
const MAX_RETRYABLE_CALLDATA_SIZE: u64 = 256 * 1024;
const RETRYABLE_TIMEOUT_QUEUE_SUBSPACE: &[u8] = &[0];
const RETRYABLE_CALLDATA_SUBSPACE: &[u8] = &[1];

pub(super) enum SubmitRetryableOutcome {
    Success {
        ticket_id: B256,
        gas_used: u64,
        scheduled: Option<ArbitrumRetryTx>,
    },
    Halt {
        reason: HaltReason,
    },
}

pub(super) struct ArbRetryableState<'a, DB: Database> {
    context: &'a mut ArbitrumContext<DB>,
}

fn custom_error<DB: Database>(message: impl Into<String>) -> EVMError<DB::Error> {
    EVMError::Custom(message.into())
}

fn address_word(address: Address) -> U256 {
    U256::from_be_slice(address.as_slice())
}

fn address_from_word(value: U256) -> Address {
    let bytes = value.to_be_bytes::<32>();
    Address::from_slice(&bytes[12..])
}

fn optional_address_word(address: Option<Address>) -> U256 {
    address
        .map(address_word)
        .unwrap_or_else(|| U256::from(1u8) << 255)
}

fn optional_address_from_word(value: U256) -> Option<Address> {
    (value != (U256::from(1u8) << 255)).then(|| address_from_word(value))
}

fn retryable_escrow_address(ticket_id: B256) -> Address {
    let hash = keccak256([b"retryable escrow".as_slice(), ticket_id.as_slice()].concat());
    Address::from_slice(&hash.as_slice()[12..])
}

fn retryable_storage_key(ticket_id: B256) -> [u8; 32] {
    arbos_state::child_key(
        &arbos_state::child_key(&[], RETRYABLE_SUBSPACE),
        ticket_id.as_slice(),
    )
}

fn take_funds(pool: &mut U256, amount: U256) -> U256 {
    let taken = (*pool).min(amount);
    *pool -= taken;
    taken
}

impl<'a, DB: Database> ArbRetryableState<'a, DB> {
    pub(super) fn new(context: &'a mut ArbitrumContext<DB>) -> Self {
        Self { context }
    }

    fn read_storage(
        &mut self,
        storage_key: &[u8],
        offset: u64,
    ) -> Result<U256, EVMError<DB::Error>> {
        let slot = arbos_state::slot_at(storage_key, offset);
        self.context
            .journal_mut()
            .load_account(ARBOS_STATE_ADDRESS)
            .map_err(EVMError::Database)?;
        self.context
            .journal_mut()
            .sload(ARBOS_STATE_ADDRESS, slot)
            .map(|value| value.data)
            .map_err(EVMError::Database)
    }

    fn write_storage(
        &mut self,
        storage_key: &[u8],
        offset: u64,
        value: U256,
    ) -> Result<(), EVMError<DB::Error>> {
        let slot = arbos_state::slot_at(storage_key, offset);
        self.context
            .journal_mut()
            .load_account(ARBOS_STATE_ADDRESS)
            .map_err(EVMError::Database)?;
        self.context
            .journal_mut()
            .sstore(ARBOS_STATE_ADDRESS, slot, value)
            .map(|_| ())
            .map_err(EVMError::Database)
    }

    fn read_bytes(&mut self, storage_key: &[u8]) -> Result<Vec<u8>, EVMError<DB::Error>> {
        let size = self.read_storage(storage_key, 0)?.wrapping_to::<u64>();
        if size > MAX_RETRYABLE_CALLDATA_SIZE {
            return Err(custom_error::<DB>(format!(
                "retryable calldata size {size} exceeds maximum {MAX_RETRYABLE_CALLDATA_SIZE}"
            )));
        }
        let mut bytes = Vec::with_capacity(size as usize);
        let mut bytes_left = size;
        let mut offset = 1;
        while bytes_left >= 32 {
            bytes.extend_from_slice(&self.read_storage(storage_key, offset)?.to_be_bytes::<32>());
            bytes_left -= 32;
            offset += 1;
        }
        if bytes_left > 0 {
            let word = self.read_storage(storage_key, offset)?.to_be_bytes::<32>();
            bytes.extend_from_slice(&word[32 - bytes_left as usize..]);
        }
        Ok(bytes)
    }

    fn write_bytes(&mut self, storage_key: &[u8], value: &[u8]) -> Result<(), EVMError<DB::Error>> {
        self.write_storage(storage_key, 0, U256::from(value.len()))?;
        let mut offset = 1;
        let mut chunks = value.chunks_exact(32);
        for chunk in &mut chunks {
            self.write_storage(storage_key, offset, U256::from_be_slice(chunk))?;
            offset += 1;
        }
        self.write_storage(storage_key, offset, U256::from_be_slice(chunks.remainder()))
    }

    fn account_balance(&mut self, address: Address) -> Result<U256, EVMError<DB::Error>> {
        self.context
            .journal_mut()
            .load_account(address)
            .map(|account| account.data.info.balance)
            .map_err(EVMError::Database)
    }

    fn mint(&mut self, address: Address, amount: U256) -> Result<(), EVMError<DB::Error>> {
        if amount.is_zero() {
            return Ok(());
        }
        let mut account = self
            .context
            .journal_mut()
            .load_account_mut(address)
            .map_err(EVMError::Database)?
            .data;
        if account.incr_balance(amount) {
            Ok(())
        } else {
            Err(custom_error::<DB>("retryable balance overflow"))
        }
    }

    fn transfer(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<(), EVMError<DB::Error>> {
        if self.try_transfer(from, to, amount)? {
            Ok(())
        } else {
            Err(custom_error::<DB>("retryable balance transfer failed"))
        }
    }

    fn try_transfer(
        &mut self,
        from: Address,
        to: Address,
        amount: U256,
    ) -> Result<bool, EVMError<DB::Error>> {
        if amount.is_zero() || from == to {
            return Ok(true);
        }
        match self
            .context
            .journal_mut()
            .transfer(from, to, amount)
            .map_err(EVMError::Database)?
        {
            None => Ok(true),
            Some(_) => Ok(false),
        }
    }

    fn refund(
        &mut self,
        refund_from: Address,
        retryable: &ArbitrumRetryTx,
        max_refund: &mut U256,
        amount: U256,
    ) -> Result<(), EVMError<DB::Error>> {
        let to_refund_address = take_funds(max_refund, amount);
        let _ = self.try_transfer(refund_from, retryable.refund_to, to_refund_address)?;
        let _ = self.try_transfer(refund_from, retryable.from, amount - to_refund_address)?;
        Ok(())
    }

    fn create_retryable(
        &mut self,
        submit: &ArbitrumSubmitRetryableTx,
        ticket_id: B256,
    ) -> Result<(), EVMError<DB::Error>> {
        let retryables_key = arbos_state::child_key(&[], RETRYABLE_SUBSPACE);
        let retryable_key = retryable_storage_key(ticket_id);
        let timeout = self
            .context
            .block()
            .timestamp()
            .wrapping_to::<u64>()
            .saturating_add(RETRYABLE_LIFETIME_SECONDS);

        for (offset, value) in [
            (0, U256::ZERO),
            (1, address_word(submit.from)),
            (2, optional_address_word(submit.retry_to)),
            (3, submit.retry_value),
            (4, address_word(submit.beneficiary)),
            (5, U256::from(timeout)),
            (6, U256::ZERO),
        ] {
            self.write_storage(&retryable_key, offset, value)?;
        }
        let calldata_key = arbos_state::child_key(&retryable_key, RETRYABLE_CALLDATA_SUBSPACE);
        self.write_bytes(&calldata_key, &submit.retry_data)?;

        let timeout_queue_key =
            arbos_state::child_key(&retryables_key, RETRYABLE_TIMEOUT_QUEUE_SUBSPACE);
        let next_put = self
            .read_storage(&timeout_queue_key, 0)?
            .wrapping_to::<u64>();
        let next_get = self
            .read_storage(&timeout_queue_key, 1)?
            .wrapping_to::<u64>();
        let next_put = if next_put == 0 { 2 } else { next_put };
        let next_get = if next_get == 0 { 2 } else { next_get };
        let next_put_after = next_put
            .checked_add(1)
            .ok_or_else(|| custom_error::<DB>("retryable timeout queue overflow"))?;
        self.write_storage(&timeout_queue_key, 0, U256::from(next_put_after))?;
        self.write_storage(&timeout_queue_key, 1, U256::from(next_get))?;
        self.write_storage(
            &timeout_queue_key,
            next_put,
            U256::from_be_slice(ticket_id.as_slice()),
        )
    }

    pub(super) fn submit_retryable(
        &mut self,
        submit: &ArbitrumSubmitRetryableTx,
    ) -> Result<SubmitRetryableOutcome, EVMError<DB::Error>> {
        let ticket_id = submit.ticket_id();
        let network_fee_account =
            address_from_word(self.read_storage(&[], NETWORK_FEE_ACCOUNT_OFFSET)?);
        let escrow = retryable_escrow_address(ticket_id);
        let basefee = U256::from(self.context.block().basefee());

        self.mint(submit.from, submit.deposit_value)?;
        let mut available_refund = submit.deposit_value;
        take_funds(&mut available_refund, submit.retry_value);

        let balance_after_mint = self.account_balance(submit.from)?;
        if balance_after_mint < submit.max_submission_fee {
            return Ok(SubmitRetryableOutcome::Halt {
                reason: HaltReason::OutOfFunds,
            });
        }

        let submission_fee =
            ArbitrumSubmitRetryableTx::submission_fee(submit.retry_data.len(), submit.l1_base_fee);
        if submit.max_submission_fee < submission_fee {
            return Ok(SubmitRetryableOutcome::Halt {
                reason: HaltReason::PrecompileErrorWithContext(format!(
                    "max submission fee {} is less than the actual submission fee {}",
                    submit.max_submission_fee, submission_fee
                )),
            });
        }

        if !self.try_transfer(submit.from, network_fee_account, submission_fee)? {
            return Ok(SubmitRetryableOutcome::Halt {
                reason: HaltReason::OutOfFunds,
            });
        }
        let withheld_submission_fee = take_funds(&mut available_refund, submission_fee);
        let excess_submission_fee = submit.max_submission_fee - submission_fee;
        let submission_fee_refund = take_funds(&mut available_refund, excess_submission_fee);
        let _ = self.try_transfer(
            submit.from,
            submit.fee_refund_addr,
            submission_fee_refund,
        )?;
        if !self.try_transfer(submit.from, escrow, submit.retry_value)? {
            let _ = self.try_transfer(network_fee_account, submit.from, submission_fee)?;
            let _ = self.try_transfer(
                submit.from,
                submit.fee_refund_addr,
                withheld_submission_fee,
            )?;
            return Ok(SubmitRetryableOutcome::Halt {
                reason: HaltReason::OutOfFunds,
            });
        }
        self.create_retryable(submit, ticket_id)?;
        self.context
            .journal_mut()
            .log(retryable_ticket_created_log(ticket_id));

        let max_gas_cost = submit.gas_fee_cap.saturating_mul(U256::from(submit.gas));
        if self.account_balance(submit.from)? < max_gas_cost
            || submit.gas < MIN_TRANSACTION_GAS
            || submit.gas_fee_cap < basefee
        {
            let gas_cost_refund = take_funds(&mut available_refund, max_gas_cost);
            let _ = self.try_transfer(
                submit.from,
                submit.fee_refund_addr,
                gas_cost_refund,
            )?;
            return Ok(SubmitRetryableOutcome::Success {
                ticket_id,
                gas_used: 0,
                scheduled: None,
            });
        }

        let gas_cost = basefee.saturating_mul(U256::from(submit.gas));
        let mut network_cost = gas_cost;
        if self.context.chain().current_arbos_version() >= 11 {
            let infra_fee_account =
                address_from_word(self.read_storage(&[], arbos_state::INFRA_FEE_ACCOUNT_OFFSET)?);
            if infra_fee_account != Address::ZERO {
                let l2_pricing_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
                let min_base_fee =
                    self.read_storage(&l2_pricing_key, arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET)?;
                let infra_cost = min_base_fee
                    .min(basefee)
                    .saturating_mul(U256::from(submit.gas))
                    .min(network_cost);
                if !self.try_transfer(submit.from, infra_fee_account, infra_cost)? {
                    return Ok(SubmitRetryableOutcome::Success {
                        ticket_id,
                        gas_used: 0,
                        scheduled: None,
                    });
                }
                network_cost -= infra_cost;
            }
        }
        if !self.try_transfer(submit.from, network_fee_account, network_cost)? {
            return Ok(SubmitRetryableOutcome::Success {
                ticket_id,
                gas_used: 0,
                scheduled: None,
            });
        }
        let withheld_gas_funds = take_funds(&mut available_refund, gas_cost);
        let gas_price_refund = submit
            .gas_fee_cap
            .saturating_sub(basefee)
            .saturating_mul(U256::from(submit.gas));
        let gas_price_refund = take_funds(&mut available_refund, gas_price_refund);
        let _ =
            self.try_transfer(submit.from, submit.fee_refund_addr, gas_price_refund)?;

        let max_refund = available_refund
            .saturating_add(withheld_gas_funds)
            .saturating_add(withheld_submission_fee);
        let retryable_key = retryable_storage_key(ticket_id);
        self.write_storage(&retryable_key, 0, U256::ONE)?;
        let scheduled = submit.retry_tx(ticket_id, 0, basefee, max_refund, submission_fee);
        self.context
            .journal_mut()
            .log(retryable_redeem_scheduled_log(
                ticket_id,
                scheduled.hash(),
                scheduled.nonce,
                scheduled.gas,
                scheduled.refund_to,
                scheduled.max_refund,
                scheduled.submission_fee_refund,
            ));

        Ok(SubmitRetryableOutcome::Success {
            ticket_id,
            gas_used: submit.gas,
            scheduled: Some(scheduled),
        })
    }

    pub(super) fn prepare_redeem(
        &mut self,
        retryable: &ArbitrumRetryTx,
    ) -> Result<(), EVMError<DB::Error>> {
        let retryable_key = retryable_storage_key(retryable.ticket_id);
        let timeout = self.read_storage(&retryable_key, 5)?.wrapping_to::<u64>();
        let current_timestamp = self.context.block().timestamp().wrapping_to::<u64>();
        let effective_timeout = if self.context.chain().current_arbos_version() >= 60 {
            let windows_left = self.read_storage(&retryable_key, 6)?.wrapping_to::<u64>();
            timeout.saturating_add(windows_left.saturating_mul(RETRYABLE_LIFETIME_SECONDS))
        } else {
            timeout
        };
        if timeout == 0 || effective_timeout < current_timestamp {
            return Err(custom_error::<DB>(format!(
                "retryable with ticketId: {:?} not found",
                retryable.ticket_id
            )));
        }

        self.transfer(
            retryable_escrow_address(retryable.ticket_id),
            retryable.from,
            retryable.value,
        )?;
        let prepaid =
            U256::from(self.context.block().basefee()).saturating_mul(U256::from(retryable.gas));
        self.mint(retryable.from, prepaid)
    }

    pub(super) fn finish_redeem(
        &mut self,
        retryable: &ArbitrumRetryTx,
        success: bool,
        gas_left: u64,
        multi_gas_refund: U256,
    ) -> Result<(), EVMError<DB::Error>> {
        let gas_left = gas_left.min(retryable.gas);
        let gas_refund = retryable.gas_fee_cap.saturating_mul(U256::from(gas_left));
        let _ = self
            .context
            .journal_mut()
            .load_account_mut(retryable.from)
            .map_err(EVMError::Database)?
            .data
            .decr_balance(gas_refund);

        let network_fee_account =
            address_from_word(self.read_storage(&[], NETWORK_FEE_ACCOUNT_OFFSET)?);
        let mut max_refund = retryable.max_refund;
        if success {
            self.refund(
                network_fee_account,
                retryable,
                &mut max_refund,
                retryable.submission_fee_refund,
            )?;
        } else {
            take_funds(&mut max_refund, retryable.submission_fee_refund);
        }

        let gas_used = retryable.gas.saturating_sub(gas_left);
        let single_gas_cost = retryable.gas_fee_cap.saturating_mul(U256::from(gas_used));
        take_funds(&mut max_refund, single_gas_cost);

        let mut network_refund = gas_refund;
        if self.context.chain().current_arbos_version() >= 11 {
            let infra_fee_account =
                address_from_word(self.read_storage(&[], arbos_state::INFRA_FEE_ACCOUNT_OFFSET)?);
            if infra_fee_account != Address::ZERO {
                let l2_pricing_key = arbos_state::child_key(&[], arbos_state::L2_PRICING_SUBSPACE);
                let min_base_fee =
                    self.read_storage(&l2_pricing_key, arbos_state::L2_MIN_BASE_FEE_WEI_OFFSET)?;
                let infra_refund = min_base_fee
                    .min(retryable.gas_fee_cap)
                    .saturating_mul(U256::from(gas_left))
                    .min(network_refund);
                self.refund(infra_fee_account, retryable, &mut max_refund, infra_refund)?;
                network_refund -= infra_refund;
            }
        }
        self.refund(
            network_fee_account,
            retryable,
            &mut max_refund,
            network_refund,
        )?;
        self.refund(
            network_fee_account,
            retryable,
            &mut max_refund,
            multi_gas_refund,
        )?;

        if !success {
            return self.transfer(
                retryable.from,
                retryable_escrow_address(retryable.ticket_id),
                retryable.value,
            );
        }

        let retryable_key = retryable_storage_key(retryable.ticket_id);
        let beneficiary = address_from_word(self.read_storage(&retryable_key, 4)?);
        let calldata_key = arbos_state::child_key(&retryable_key, RETRYABLE_CALLDATA_SUBSPACE);
        let calldata_size = self.read_storage(&calldata_key, 0)?.wrapping_to::<u64>();
        for offset in 0..=6 {
            self.write_storage(&retryable_key, offset, U256::ZERO)?;
        }
        for offset in 0..=calldata_size.div_ceil(32) {
            self.write_storage(&calldata_key, offset, U256::ZERO)?;
        }
        let escrow = retryable_escrow_address(retryable.ticket_id);
        let remaining = self.account_balance(escrow)?;
        self.transfer(escrow, beneficiary, remaining)
    }

    pub(super) fn scheduled_retryables_from_logs(
        &mut self,
        logs: &[Log],
    ) -> Result<Vec<ArbitrumRetryTx>, EVMError<DB::Error>> {
        let event_id =
            keccak256("RedeemScheduled(bytes32,bytes32,uint64,uint64,address,uint256,uint256)");
        let mut scheduled = Vec::new();
        for log in logs {
            let topics = log.data.topics();
            if log.address != ARB_RETRYABLE_TX_ADDRESS || topics.len() != 4 || topics[0] != event_id
            {
                continue;
            }
            let ticket_id = topics[1];
            let sequence_num = U256::from_be_slice(topics[3].as_slice()).wrapping_to::<u64>();
            let (gas_donated, gas_donor, max_refund, submission_fee_refund) =
                <(U256, Address, U256, U256)>::abi_decode(log.data.data.as_ref()).map_err(
                    |error| custom_error::<DB>(format!("invalid RedeemScheduled log: {error}")),
                )?;

            let retryable_key = retryable_storage_key(ticket_id);
            if self.read_storage(&retryable_key, 5)?.is_zero() {
                continue;
            }
            let calldata_key = arbos_state::child_key(&retryable_key, RETRYABLE_CALLDATA_SUBSPACE);
            let retryable = ArbitrumRetryTx {
                chain_id: U256::from(self.context.cfg().chain_id()),
                nonce: sequence_num,
                from: address_from_word(self.read_storage(&retryable_key, 1)?),
                gas_fee_cap: U256::from(
                    self.context
                        .chain()
                        .current_l2_basefee()
                        .unwrap_or_else(|| self.context.block().basefee()),
                ),
                gas: gas_donated.wrapping_to::<u64>(),
                to: optional_address_from_word(self.read_storage(&retryable_key, 2)?),
                value: self.read_storage(&retryable_key, 3)?,
                data: self.read_bytes(&calldata_key)?.into(),
                ticket_id,
                refund_to: gas_donor,
                max_refund,
                submission_fee_refund,
            };
            scheduled.push(retryable);
        }
        Ok(scheduled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::Context;
    use revm::database::{in_memory_db::CacheDB, EmptyDB};
    use revm::MainContext;

    #[test]
    fn redeem_scheduled_log_reconstructs_retry_transaction() {
        let mut cfg = CfgEnv::new_with_spec(ArbitrumHardfork::Prague);
        cfg.chain_id = 42161;
        let mut chain = ArbitrumExecutionContext::default();
        chain.set_current_l2_context(U256::ZERO, 100);
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv {
                basefee: 100,
                timestamp: U256::from(1_000),
                ..Default::default()
            })
            .with_cfg(cfg)
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(chain);
        let ticket_id = B256::with_last_byte(1);
        let from = Address::with_last_byte(2);
        let to = Address::with_last_byte(3);
        let donor = Address::with_last_byte(4);
        let retryable_key = retryable_storage_key(ticket_id);
        let mut retryable_state = ArbRetryableState::new(&mut context);
        for (offset, value) in [
            (1, address_word(from)),
            (2, optional_address_word(Some(to))),
            (3, U256::from(7)),
            (4, address_word(Address::with_last_byte(5))),
            (5, U256::from(2_000)),
        ] {
            retryable_state
                .write_storage(&retryable_key, offset, value)
                .unwrap();
        }
        let calldata_key = arbos_state::child_key(&retryable_key, RETRYABLE_CALLDATA_SUBSPACE);
        retryable_state
            .write_bytes(&calldata_key, &[0xaa, 0xbb])
            .unwrap();
        let log = retryable_redeem_scheduled_log(
            ticket_id,
            B256::with_last_byte(6),
            9,
            50_000,
            donor,
            U256::from(123),
            U256::from(456),
        );

        let scheduled = retryable_state
            .scheduled_retryables_from_logs(&[log])
            .unwrap();

        assert_eq!(scheduled.len(), 1);
        let retryable = &scheduled[0];
        assert_eq!(retryable.chain_id, U256::from(42161));
        assert_eq!(retryable.nonce, 9);
        assert_eq!(retryable.from, from);
        assert_eq!(retryable.gas_fee_cap, U256::from(100));
        assert_eq!(retryable.gas, 50_000);
        assert_eq!(retryable.to, Some(to));
        assert_eq!(retryable.value, U256::from(7));
        assert_eq!(retryable.data.as_ref(), &[0xaa, 0xbb]);
        assert_eq!(retryable.ticket_id, ticket_id);
        assert_eq!(retryable.refund_to, donor);
        assert_eq!(retryable.max_refund, U256::from(123));
        assert_eq!(retryable.submission_fee_refund, U256::from(456));
    }
}
