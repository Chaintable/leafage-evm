use super::abi::IArbRetryableTx;
use super::state::ArbStorage;
use super::util::{log_gas, sol_error_revert};
use super::{
    ArbPrecompileInput, ArbitrumContext, ARB_RETRYABLE_TX_ADDRESS, RETRYABLE_LIFETIME_SECONDS,
};
use alloy::primitives::{keccak256, Address, Bytes, Log, B256, U256};
use alloy::sol_types::{SolCall, SolInterface, SolValue};
use alloy_rlp::{BufMut, Encodable, Header, EMPTY_STRING_CODE};
use revm::context::{Cfg, ContextTr, JournalTr};
use revm::precompile::{PrecompileError, PrecompileOutput, PrecompileResult};
use revm::Database;

pub(super) struct ArbRetryableTx;

const ARBITRUM_RETRY_TX_TYPE: u8 = 0x68;
const RETRYABLE_STORAGE_BURN_PER_WORD: u64 = 50;
const RETRYABLE_KEEPALIVE_STORAGE_BURN_PER_WORD: u64 = 200;
const RETRYABLE_REAP_PRICE: u64 = 58_000;
const TX_GAS: u64 = 21_000;
const COPY_GAS: u64 = 3;
const ARBOS_VERSION_3: u64 = 3;
const ARBOS_VERSION_11: u64 = 11;

impl ArbRetryableTx {
    pub(super) fn run<DB: Database>(
        input: ArbPrecompileInput<'_, ArbitrumContext<DB>>,
    ) -> PrecompileResult {
        let gas_limit = input.gas;
        let data = input.data;
        let caller = input.caller;
        let is_static = input.is_static;
        let current_arbos_version = input.current_arbos_version;
        let current_retryable_ticket = input.current_retryable_ticket;
        let current_refund_to = input.current_refund_to;
        let context = input.context;
        let mut storage = ArbStorage::new_with_initial_gas(context, gas_limit, 0);
        let call = match Self::decode_call(data, gas_limit, &mut storage) {
            Ok(call) => call,
            Err(result) => return result,
        };
        match call {
            IArbRetryableTx::IArbRetryableTxCalls::getLifetime(_) => {
                Self::finish_call::<DB, IArbRetryableTx::getLifetimeCall>(
                    gas_limit,
                    &mut storage,
                    U256::from(RETRYABLE_LIFETIME_SECONDS),
                )
            }
            IArbRetryableTx::IArbRetryableTxCalls::getCurrentRedeemer(_) => {
                Self::finish_call::<DB, IArbRetryableTx::getCurrentRedeemerCall>(
                    gas_limit,
                    &mut storage,
                    current_refund_to.unwrap_or_default(),
                )
            }
            IArbRetryableTx::IArbRetryableTxCalls::getTimeout(call) => {
                match storage.retryable_timeout(call.ticketId) {
                    Ok(ret) => Self::finish_call::<DB, IArbRetryableTx::getTimeoutCall>(
                        gas_limit,
                        &mut storage,
                        U256::from(ret),
                    ),
                    Err(error) => Self::handle_precompile_error(
                        gas_limit,
                        &mut storage,
                        current_arbos_version,
                        error,
                    ),
                }
            }
            IArbRetryableTx::IArbRetryableTxCalls::getBeneficiary(call) => {
                match storage.retryable_beneficiary(call.ticketId) {
                    Ok(ret) => Self::finish_call::<DB, IArbRetryableTx::getBeneficiaryCall>(
                        gas_limit,
                        &mut storage,
                        ret,
                    ),
                    Err(error) => Self::handle_retryable_error(
                        gas_limit,
                        &mut storage,
                        current_arbos_version,
                        error,
                    ),
                }
            }
            IArbRetryableTx::IArbRetryableTxCalls::cancel(call) => {
                if is_static {
                    return Self::empty_revert(gas_limit, storage.gas_used);
                }
                match Self::cancel(
                    &mut storage,
                    caller,
                    current_retryable_ticket,
                    call.ticketId,
                ) {
                    Ok(()) => {
                        Self::emit_canceled(&mut storage, call.ticketId)?;
                        Self::finish_call::<DB, IArbRetryableTx::cancelCall>(
                            gas_limit,
                            &mut storage,
                            ().into(),
                        )
                    }
                    Err(error) => Self::handle_retryable_error(
                        gas_limit,
                        &mut storage,
                        current_arbos_version,
                        error,
                    ),
                }
            }
            IArbRetryableTx::IArbRetryableTxCalls::submitRetryable(_) => {
                Self::not_callable(gas_limit, storage.gas_used)
            }
            IArbRetryableTx::IArbRetryableTxCalls::keepalive(call) => {
                if is_static {
                    return Self::empty_revert(gas_limit, storage.gas_used);
                }
                match Self::keepalive(&mut storage, call.ticketId) {
                    Ok(ret) => {
                        Self::emit_lifetime_extended(&mut storage, call.ticketId, ret)?;
                        Self::finish_call::<DB, IArbRetryableTx::keepaliveCall>(
                            gas_limit,
                            &mut storage,
                            U256::from(ret),
                        )
                    }
                    Err(error) => Self::handle_retryable_error(
                        gas_limit,
                        &mut storage,
                        current_arbos_version,
                        error,
                    ),
                }
            }
            IArbRetryableTx::IArbRetryableTxCalls::redeem(call) => {
                if is_static {
                    return Self::empty_revert(gas_limit, storage.gas_used);
                }
                match Self::redeem(
                    &mut storage,
                    caller,
                    current_retryable_ticket,
                    call.ticketId,
                ) {
                    Ok(retry_tx_hash) => Self::finish_call::<DB, IArbRetryableTx::redeemCall>(
                        gas_limit,
                        &mut storage,
                        retry_tx_hash,
                    ),
                    Err(error) => Self::handle_retryable_error(
                        gas_limit,
                        &mut storage,
                        current_arbos_version,
                        error,
                    ),
                }
            }
        }
    }

    fn keepalive<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        ticket_id: B256,
    ) -> Result<u64, PrecompileError> {
        let byte_count = storage.retryable_size_bytes(ticket_id)?;
        if byte_count == 0 {
            return Err(PrecompileError::other("NoTicketWithID"));
        }
        storage.burn(
            RETRYABLE_KEEPALIVE_STORAGE_BURN_PER_WORD.saturating_mul(byte_count.div_ceil(32)),
        )?;
        let timeout = storage.keepalive_retryable(ticket_id)?;
        storage.burn(RETRYABLE_REAP_PRICE)?;
        Ok(timeout)
    }

    fn redeem<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
        current_retryable_ticket: Option<B256>,
        ticket_id: B256,
    ) -> Result<B256, PrecompileError> {
        if current_retryable_ticket == Some(ticket_id) {
            return Err(PrecompileError::other("retryable cannot modify itself"));
        }

        let byte_count = storage.retryable_size_bytes(ticket_id)?;
        storage.burn(RETRYABLE_STORAGE_BURN_PER_WORD.saturating_mul(byte_count.div_ceil(32)))?;

        let info = storage.retryable_redeem_info(ticket_id)?;
        let backlog_update_cost = storage.backlog_update_cost()?;
        let uses_fixed_backlog_update_cost = storage.uses_fixed_backlog_update_cost()?;
        let event_cost = Self::redeem_scheduled_event_gas_cost();
        let future_gas_costs = event_cost
            .saturating_add(COPY_GAS)
            .saturating_add(backlog_update_cost);
        let gas_left = storage.gas_left();
        if gas_left < future_gas_costs {
            storage.burn_out();
            return Err(PrecompileError::OutOfGas);
        }

        let gas_to_donate = gas_left - future_gas_costs;
        if gas_to_donate < TX_GAS {
            return Err(PrecompileError::other(
                "not enough gas to run redeem attempt",
            ));
        }

        let retry_tx_hash = Self::retry_tx_hash(storage, &info, ticket_id, caller, gas_to_donate);
        Self::emit_redeem_scheduled(
            storage,
            ticket_id,
            retry_tx_hash,
            info.nonce,
            gas_to_donate,
            caller,
        )?;
        storage.burn(gas_to_donate)?;
        if uses_fixed_backlog_update_cost {
            storage.burn(backlog_update_cost)?;
        }
        storage.shrink_l2_backlog(gas_to_donate)?;
        Ok(retry_tx_hash)
    }

    fn cancel<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        caller: Address,
        current_retryable_ticket: Option<B256>,
        ticket_id: B256,
    ) -> Result<(), PrecompileError> {
        if current_retryable_ticket == Some(ticket_id) {
            return Err(PrecompileError::other("retryable cannot modify itself"));
        }

        let beneficiary = storage.retryable_beneficiary(ticket_id)?;
        if caller != beneficiary {
            return Err(PrecompileError::other(
                "only the beneficiary may cancel a retryable",
            ));
        }
        storage.delete_retryable(ticket_id, beneficiary)
    }

    fn decode_call<DB: Database>(
        data: &[u8],
        gas_limit: u64,
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
    ) -> Result<IArbRetryableTx::IArbRetryableTxCalls, PrecompileResult> {
        if storage.burn(Self::args_copy_gas(data)).is_err() {
            storage.burn_out();
            return Err(Self::empty_revert(gas_limit, gas_limit));
        }

        match <IArbRetryableTx::IArbRetryableTxCalls as SolInterface>::abi_decode(data) {
            Ok(call) => Ok(call),
            Err(_) => {
                storage.burn_out();
                Err(Self::empty_revert(gas_limit, gas_limit))
            }
        }
    }

    fn finish_call<DB: Database, T: SolCall>(
        gas_limit: u64,
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        ret: T::Return,
    ) -> PrecompileResult {
        let encoded: Bytes = T::abi_encode_returns(&ret).into();
        if storage.burn(Self::copy_gas(encoded.len())).is_err() {
            storage.burn_out();
            return Self::empty_revert(gas_limit, gas_limit);
        }
        Ok(PrecompileOutput::new(storage.gas_used, encoded))
    }

    fn handle_precompile_error<DB: Database>(
        gas_limit: u64,
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        arbos_version: u64,
        error: PrecompileError,
    ) -> PrecompileResult {
        match error {
            PrecompileError::OutOfGas => {
                storage.burn_out();
                Self::empty_revert(gas_limit, gas_limit)
            }
            PrecompileError::Other(reason) => {
                Self::handle_error(gas_limit, storage.gas_used, arbos_version, &reason)
            }
            error => Err(error),
        }
    }

    fn handle_error(
        gas_limit: u64,
        gas_used: u64,
        arbos_version: u64,
        reason: &str,
    ) -> PrecompileResult {
        if reason == "NoTicketWithID" {
            return Self::no_ticket_with_id(gas_limit, gas_used);
        }
        Self::non_solidity_error(gas_limit, gas_used, arbos_version)
    }

    fn handle_retryable_error<DB: Database>(
        gas_limit: u64,
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        arbos_version: u64,
        error: PrecompileError,
    ) -> PrecompileResult {
        match &error {
            PrecompileError::Other(reason)
                if reason == "NoTicketWithID" && arbos_version < ARBOS_VERSION_3 =>
            {
                Self::non_solidity_error(gas_limit, storage.gas_used, arbos_version)
            }
            _ => Self::handle_precompile_error(gas_limit, storage, arbos_version, error),
        }
    }

    fn no_ticket_with_id(gas_limit: u64, gas_used: u64) -> PrecompileResult {
        sol_error_revert(gas_limit, gas_used, IArbRetryableTx::NoTicketWithID {})
    }

    fn not_callable(gas_limit: u64, gas_used: u64) -> PrecompileResult {
        sol_error_revert(gas_limit, gas_used, IArbRetryableTx::NotCallable {})
    }

    fn empty_revert(gas_limit: u64, gas_used: u64) -> PrecompileResult {
        if gas_used > gas_limit {
            return Err(PrecompileError::OutOfGas);
        }
        Ok(PrecompileOutput::new_reverted(gas_used, Bytes::new()))
    }

    fn non_solidity_error(gas_limit: u64, gas_used: u64, arbos_version: u64) -> PrecompileResult {
        if arbos_version < ARBOS_VERSION_11 {
            return Self::empty_revert(gas_limit, gas_limit);
        }
        Self::empty_revert(gas_limit, gas_used)
    }

    fn emit_canceled<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        ticket_id: B256,
    ) -> Result<(), PrecompileError> {
        storage.burn(Self::canceled_event_gas_cost())?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_RETRYABLE_TX_ADDRESS,
            vec![keccak256("Canceled(bytes32)"), ticket_id],
            Bytes::new(),
        ));
        Ok(())
    }

    fn emit_lifetime_extended<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        ticket_id: B256,
        new_timeout: u64,
    ) -> Result<(), PrecompileError> {
        let data = Bytes::from(U256::from(new_timeout).abi_encode());
        storage.burn(Self::lifetime_extended_event_gas_cost())?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_RETRYABLE_TX_ADDRESS,
            vec![keccak256("LifetimeExtended(bytes32,uint256)"), ticket_id],
            data,
        ));
        Ok(())
    }

    fn emit_redeem_scheduled<DB: Database>(
        storage: &mut ArbStorage<'_, ArbitrumContext<DB>>,
        ticket_id: B256,
        retry_tx_hash: B256,
        sequence_num: u64,
        gas_donated: u64,
        gas_donor: Address,
    ) -> Result<(), PrecompileError> {
        storage.burn(Self::redeem_scheduled_event_gas_cost())?;
        storage.context.journal_mut().log(Log::new_unchecked(
            ARB_RETRYABLE_TX_ADDRESS,
            vec![
                keccak256("RedeemScheduled(bytes32,bytes32,uint64,uint64,address,uint256,uint256)"),
                ticket_id,
                retry_tx_hash,
                B256::from(U256::from(sequence_num).to_be_bytes::<32>()),
            ],
            Bytes::from((U256::from(gas_donated), gas_donor, U256::MAX, U256::ZERO).abi_encode()),
        ));
        Ok(())
    }

    fn retry_tx_hash<DB: Database>(
        storage: &ArbStorage<'_, ArbitrumContext<DB>>,
        info: &super::state::RetryableRedeemInfo,
        ticket_id: B256,
        refund_to: Address,
        gas: u64,
    ) -> B256 {
        let chain_id = storage.context.cfg().chain_id();
        let gas_fee_cap = U256::from(storage.current_l2_basefee());
        let max_refund = U256::MAX;
        let submission_fee_refund = U256::ZERO;
        let payload_len = chain_id.length()
            + info.nonce.length()
            + info.from.length()
            + gas_fee_cap.length()
            + gas.length()
            + Self::optional_address_rlp_len(&info.to)
            + info.value.length()
            + info.data.length()
            + ticket_id.length()
            + refund_to.length()
            + max_refund.length()
            + submission_fee_refund.length();

        let mut out = Vec::with_capacity(payload_len + 8);
        out.push(ARBITRUM_RETRY_TX_TYPE);
        Header {
            list: true,
            payload_length: payload_len,
        }
        .encode(&mut out);
        chain_id.encode(&mut out);
        info.nonce.encode(&mut out);
        info.from.encode(&mut out);
        gas_fee_cap.encode(&mut out);
        gas.encode(&mut out);
        Self::encode_optional_address(&info.to, &mut out);
        info.value.encode(&mut out);
        info.data.encode(&mut out);
        ticket_id.encode(&mut out);
        refund_to.encode(&mut out);
        max_refund.encode(&mut out);
        submission_fee_refund.encode(&mut out);
        keccak256(out)
    }

    fn redeem_scheduled_event_gas_cost() -> u64 {
        log_gas(3, 4 * 32)
    }

    fn lifetime_extended_event_gas_cost() -> u64 {
        log_gas(1, 32)
    }

    fn canceled_event_gas_cost() -> u64 {
        log_gas(1, 0)
    }

    fn args_copy_gas(data: &[u8]) -> u64 {
        Self::copy_gas(data.len().saturating_sub(4))
    }

    fn copy_gas(byte_count: usize) -> u64 {
        COPY_GAS.saturating_mul((byte_count as u64).div_ceil(32))
    }

    fn optional_address_rlp_len(address: &Option<Address>) -> usize {
        address.as_ref().map_or(1, |address| address.length())
    }

    fn encode_optional_address(address: &Option<Address>, out: &mut dyn BufMut) {
        match address {
            Some(address) => address.encode(out),
            None => out.put_u8(EMPTY_STRING_CODE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{STORAGE_READ_GAS, STORAGE_WRITE_COST};
    use super::*;
    use crate::arbitrum::arbos_state;
    use crate::arbitrum::evm::ArbitrumExecutionContext;
    use crate::arbitrum::hardforks::ArbitrumHardfork;
    use crate::arbitrum::tx::ArbitrumTxEnv;
    use alloy::sol_types::SolError;
    use leafage_evm_types::{BlockEnv, CfgEnv};
    use revm::context::JournalTr;
    use revm::database::in_memory_db::CacheDB;
    use revm::database::EmptyDB;
    use revm::{Context, MainContext};

    fn context() -> ArbitrumContext<CacheDB<EmptyDB>> {
        context_at(0)
    }

    fn context_at(timestamp: u64) -> ArbitrumContext<CacheDB<EmptyDB>> {
        let mut context = Context::mainnet()
            .with_tx(ArbitrumTxEnv::default())
            .with_block(BlockEnv {
                timestamp: U256::from(timestamp),
                ..Default::default()
            })
            .with_cfg(CfgEnv::new_with_spec(ArbitrumHardfork::Prague))
            .with_db(CacheDB::new(EmptyDB::default()))
            .with_chain(ArbitrumExecutionContext::default());
        context
            .journal_mut()
            .load_account(crate::arbitrum::arbos_state::ARBOS_STATE_ADDRESS)
            .expect("load ArbOS state account");
        context
    }

    fn initialize_retryable(context: &mut ArbitrumContext<CacheDB<EmptyDB>>, ticket_id: B256) {
        initialize_retryable_with(context, ticket_id, 60, RETRYABLE_LIFETIME_SECONDS, 0);
    }

    fn initialize_retryable_with(
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        ticket_id: B256,
        arbos_version: u64,
        timeout: u64,
        windows_left: u64,
    ) {
        let mut storage = ArbStorage::new_with_initial_gas(context, u64::MAX, 0);
        storage
            .write(
                &[],
                arbos_state::ARBOS_VERSION_OFFSET,
                U256::from(arbos_version),
            )
            .expect("write ArbOS version");

        let retryable_key = storage.retryable_key(ticket_id);
        storage
            .write(&retryable_key, 5, U256::from(timeout))
            .expect("write retryable timeout");
        storage
            .write(&retryable_key, 6, U256::from(windows_left))
            .expect("write retryable timeout windows");

        let retryables_key = arbos_state::child_key(&[], arbos_state::RETRYABLE_SUBSPACE);
        let timeout_queue_key = arbos_state::child_key(&retryables_key, &[0]);
        storage
            .write(&timeout_queue_key, 0, U256::from(2))
            .expect("initialize timeout queue nextPut");
        storage
            .write(&timeout_queue_key, 1, U256::from(2))
            .expect("initialize timeout queue nextGet");
    }

    fn get_beneficiary(
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        arbos_version: u64,
        ticket_id: B256,
    ) -> PrecompileOutput {
        let data = IArbRetryableTx::getBeneficiaryCall {
            ticketId: ticket_id,
        }
        .abi_encode();
        ArbRetryableTx::run(ArbPrecompileInput {
            data: &data,
            gas: 10_000_000,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: arbos_version,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context,
        })
        .expect("getBeneficiary")
    }

    fn keepalive(
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        ticket_id: B256,
    ) -> PrecompileOutput {
        keepalive_with_arbos_version(context, ticket_id, 60)
    }

    fn keepalive_with_arbos_version(
        context: &mut ArbitrumContext<CacheDB<EmptyDB>>,
        ticket_id: B256,
        arbos_version: u64,
    ) -> PrecompileOutput {
        let data = IArbRetryableTx::keepaliveCall {
            ticketId: ticket_id,
        }
        .abi_encode();
        ArbRetryableTx::run(ArbPrecompileInput {
            data: &data,
            gas: 10_000_000,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: arbos_version,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: None,
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context,
        })
        .expect("keepalive")
    }

    fn keepalive_expected_gas(ticket_id: B256, storage_reads: u64) -> u64 {
        let input = IArbRetryableTx::keepaliveCall {
            ticketId: ticket_id,
        }
        .abi_encode();
        let retryable_size: u64 = 6 * 32 + 32;
        ArbRetryableTx::args_copy_gas(&input)
            + STORAGE_READ_GAS * storage_reads
            + RETRYABLE_KEEPALIVE_STORAGE_BURN_PER_WORD * retryable_size.div_ceil(32)
            + STORAGE_WRITE_COST * 3
            + RETRYABLE_REAP_PRICE
            + ArbRetryableTx::lifetime_extended_event_gas_cost()
            + ArbRetryableTx::copy_gas(32)
    }

    #[test]
    fn event_gas_costs_match_nitro_generated_events() {
        assert_eq!(ArbRetryableTx::canceled_event_gas_cost(), 1_125);
        assert_eq!(ArbRetryableTx::lifetime_extended_event_gas_cost(), 1_381);
        assert_eq!(ArbRetryableTx::redeem_scheduled_event_gas_cost(), 2_899);
    }

    #[test]
    fn keepalive_storage_burn_per_word_matches_nitro() {
        assert_eq!(RETRYABLE_KEEPALIVE_STORAGE_BURN_PER_WORD, 200);
    }

    #[test]
    fn keepalive_updates_timeout_queue_and_prepays_reap_gas() {
        let ticket_id = B256::from([0x22; 32]);
        let mut context = context();
        initialize_retryable(&mut context, ticket_id);

        let output = keepalive(&mut context, ticket_id);
        let ret = IArbRetryableTx::keepaliveCall::abi_decode_returns(output.bytes.as_ref())
            .expect("decode keepalive return");

        assert!(!output.reverted);
        assert_eq!(ret, U256::from(RETRYABLE_LIFETIME_SECONDS * 2));
        assert_eq!(output.gas_used, keepalive_expected_gas(ticket_id, 7));

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let retryable_key = storage.retryable_key(ticket_id);
        assert_eq!(storage.read(&retryable_key, 6).unwrap(), U256::from(1));

        let retryables_key = arbos_state::child_key(&[], arbos_state::RETRYABLE_SUBSPACE);
        let timeout_queue_key = arbos_state::child_key(&retryables_key, &[0]);
        assert_eq!(storage.read(&timeout_queue_key, 0).unwrap(), U256::from(3));
        assert_eq!(storage.read(&timeout_queue_key, 1).unwrap(), U256::from(2));
        assert_eq!(
            storage.read(&timeout_queue_key, 2).unwrap(),
            U256::from_be_slice(ticket_id.as_slice())
        );
    }

    #[test]
    fn keepalive_honors_v60_timeout_windows_after_raw_timeout() {
        let ticket_id = B256::from([0x55; 32]);
        let now = RETRYABLE_LIFETIME_SECONDS + 10;
        let mut context = context_at(now);
        initialize_retryable_with(&mut context, ticket_id, 60, RETRYABLE_LIFETIME_SECONDS, 1);

        let output = keepalive(&mut context, ticket_id);
        let ret = IArbRetryableTx::keepaliveCall::abi_decode_returns(output.bytes.as_ref())
            .expect("decode keepalive return");

        assert!(!output.reverted);
        assert_eq!(ret, U256::from(RETRYABLE_LIFETIME_SECONDS * 3));
        assert_eq!(output.gas_used, keepalive_expected_gas(ticket_id, 9));

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let retryable_key = storage.retryable_key(ticket_id);
        assert_eq!(storage.read(&retryable_key, 6).unwrap(), U256::from(2));
    }

    #[test]
    fn keepalive_rejects_pre_v60_retryable_after_raw_timeout() {
        let ticket_id = B256::from([0x66; 32]);
        let now = RETRYABLE_LIFETIME_SECONDS + 10;
        let mut context = context_at(now);
        initialize_retryable_with(&mut context, ticket_id, 59, RETRYABLE_LIFETIME_SECONDS, 1);

        let output = keepalive_with_arbos_version(&mut context, ticket_id, 59);

        assert!(output.reverted);
        IArbRetryableTx::NoTicketWithID::abi_decode(output.bytes.as_ref())
            .expect("decode NoTicketWithID");

        let mut storage = ArbStorage::new_with_initial_gas(&mut context, u64::MAX, 0);
        let retryable_key = storage.retryable_key(ticket_id);
        assert_eq!(storage.read(&retryable_key, 6).unwrap(), U256::from(1));
    }

    #[test]
    fn not_found_error_consumes_all_gas_before_arbos_3() {
        let ticket_id = B256::from([0x33; 32]);
        let mut legacy_context = context();
        let legacy = get_beneficiary(&mut legacy_context, 2, ticket_id);
        assert!(legacy.reverted);
        assert!(legacy.bytes.is_empty());
        assert_eq!(legacy.gas_used, 10_000_000);

        let mut current_context = context();
        let current = get_beneficiary(&mut current_context, 3, ticket_id);
        assert!(current.reverted);
        IArbRetryableTx::NoTicketWithID::abi_decode(current.bytes.as_ref())
            .expect("decode NoTicketWithID");
    }

    #[test]
    fn ordinary_error_consumes_all_gas_before_solidity_revert_version() {
        let ticket_id = B256::from([0x77; 32]);
        let input = IArbRetryableTx::redeemCall {
            ticketId: ticket_id,
        }
        .abi_encode();
        let gas_limit = 10_000_000;
        let mut context = context();

        let modern = ArbRetryableTx::run(ArbPrecompileInput {
            data: &input,
            gas: gas_limit,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 11,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: Some(ticket_id),
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("self-modifying redeem");
        assert!(modern.reverted);
        assert!(modern.bytes.is_empty());
        assert!(modern.gas_used < gas_limit);

        let legacy = ArbRetryableTx::run(ArbPrecompileInput {
            data: &input,
            gas: gas_limit,
            caller: Address::ZERO,
            value: U256::ZERO,
            is_static: false,
            is_valid_call_context: true,
            current_arbos_version: 10,
            current_tx_l1_gas_fees: U256::ZERO,
            current_tx_l1_gas_units: 0,
            current_l1_block_number: 0,
            current_retryable_ticket: Some(ticket_id),
            current_refund_to: None,
            allow_debug_precompiles: false,
            current_chain_config: None,
            context: &mut context,
        })
        .expect("legacy self-modifying redeem");
        assert!(legacy.reverted);
        assert!(legacy.bytes.is_empty());
        assert_eq!(legacy.gas_used, gas_limit);
    }

    #[test]
    fn canceled_event_burns_gas_before_log() {
        let ticket_id = B256::from([0x44; 32]);
        let cost = ArbRetryableTx::canceled_event_gas_cost();
        let mut context = context();

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, cost - 1, 0);
            let error = ArbRetryableTx::emit_canceled(&mut storage, ticket_id)
                .expect_err("event should run out of gas");
            assert!(error.is_oog());
        }
        assert!(context.journal_mut().take_logs().is_empty());

        {
            let mut storage = ArbStorage::new_with_initial_gas(&mut context, cost, 0);
            ArbRetryableTx::emit_canceled(&mut storage, ticket_id).expect("emit canceled");
            assert_eq!(storage.gas_used, cost);
        }
        let logs = context.journal_mut().take_logs();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].data.topics()[0], keccak256("Canceled(bytes32)"));
        assert_eq!(logs[0].data.topics()[1], ticket_id);
    }
}
