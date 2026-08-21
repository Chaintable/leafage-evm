//! Selector dispatch for both B20 variants.
//!
//! Transcribed from Base reth (`b20_asset/dispatch.rs`, `b20_stablecoin/dispatch.rs`).
//! The preamble order — nonpayable check, calldata gas, initialization check — is charged
//! before any handler runs and is part of every call's cost, so it must stay as Base has it.

use alloy::primitives::{Address, Bytes, U256};
use alloy::sol_types::{SolCall, SolError, SolInterface, SolValue};

use super::abi::{IB20, IB20Asset, IB20Stablecoin};
use super::error::{B20Error, Result};
use super::ids;
use super::layout::{B20Store, WAD};
use super::permit::{self, PermitArgs};
use super::ops;
use super::port::B20Port;

use IB20::IB20Calls as C;
use IB20Asset::IB20AssetCalls as A;

/// Gas charged per 32-byte word of calldata, before dispatch.
const CALLDATA_WORD_GAS: u64 = 6;

/// Outcome of a B20 dispatch that did not fail fatally.
pub enum B20Outcome {
    /// Successful return with ABI-encoded output.
    Return(Bytes),
    /// Revert with ABI-encoded output.
    Revert(Bytes),
}

/// Calldata gas charged by Beryl precompile dispatch: 6 per 32-byte word.
pub const fn calldata_gas_cost(calldata: &[u8]) -> u64 {
    (calldata.len() as u64).div_ceil(32).saturating_mul(CALLDATA_WORD_GAS)
}

/// Runs a B20 call against `address`.
///
/// `Err` is reserved for conditions that are not reverts — out of gas, a static-call
/// violation, or a storage failure. Everything else, including an unknown selector, comes
/// back as [`B20Outcome::Revert`] with the gas metered so far already consumed.
pub fn dispatch<P: B20Port>(
    port: &mut P,
    address: Address,
    is_asset: bool,
    calldata: &[u8],
) -> Result<B20Outcome> {
    // B20 selectors are all nonpayable.
    if !port.call_value().is_zero() {
        return Ok(B20Outcome::Revert(IB20::NonPayable {}.abi_encode().into()));
    }
    port.deduct_gas(calldata_gas_cost(calldata))?;

    let mut store = B20Store::new(port, address, is_asset);

    // An address with no marker bytecode is not a created token.
    if !store.is_initialized()? {
        return Ok(B20Outcome::Revert(Bytes::new()));
    }

    match run(&mut store, calldata, false) {
        Ok(output) => Ok(B20Outcome::Return(output)),
        Err(err) => match err.revert_output() {
            Some(data) => Ok(B20Outcome::Revert(data)),
            None => Err(err),
        },
    }
}

fn selector_of(calldata: &[u8]) -> Option<[u8; 4]> {
    calldata.get(..4).map(|bytes| {
        let mut selector = [0u8; 4];
        selector.copy_from_slice(bytes);
        selector
    })
}

/// Decodes and executes one call. `privileged` skips the checks the factory bypasses
/// during token creation; leafage never creates tokens, so it is always `false` at entry
/// and only becomes `true` inside `batchMint`'s inner mints.
fn run<P: B20Port>(
    store: &mut B20Store<'_, P>,
    calldata: &[u8],
    privileged: bool,
) -> Result<Bytes> {
    let Some(selector) = selector_of(calldata) else {
        return Err(B20Error::Revert(Bytes::new()));
    };

    // Variant-specific selectors take precedence over the inherited IB20 set.
    if store.is_asset() {
        if A::valid_selector(selector) {
            let call = A::abi_decode_validate(calldata)
                .map_err(|_| B20Error::Revert(selector.to_vec().into()))?;
            return run_asset(store, call, privileged);
        }
    } else if IB20Stablecoin::IB20StablecoinCalls::valid_selector(selector) {
        let call = IB20Stablecoin::IB20StablecoinCalls::abi_decode_validate(calldata)
            .map_err(|_| B20Error::Revert(selector.to_vec().into()))?;
        return match call {
            IB20Stablecoin::IB20StablecoinCalls::currency(_) => {
                Ok(store.currency()?.abi_encode().into())
            }
        };
    }

    if !C::valid_selector(selector) {
        return Err(B20Error::Revert(selector.to_vec().into()));
    }
    let call =
        C::abi_decode_validate(calldata).map_err(|_| B20Error::Revert(selector.to_vec().into()))?;
    run_b20(store, call, privileged)
}

fn run_b20<P: B20Port>(
    store: &mut B20Store<'_, P>,
    call: C,
    privileged: bool,
) -> Result<Bytes> {
    let caller = store.port().caller();
    let chain_id = store.port().chain_id();
    let timestamp = store.port().timestamp();

    let encoded: Bytes = match call {
        // --- Pure reads ---
        C::name(_) => store.name()?.abi_encode().into(),
        C::symbol(_) => store.symbol()?.abi_encode().into(),
        C::decimals(_) => U256::from(store.decimals()?).abi_encode().into(),
        C::totalSupply(_) => store.total_supply()?.abi_encode().into(),
        C::balanceOf(c) => store.balance_of(c.account)?.abi_encode().into(),
        C::allowance(c) => store.allowance(c.owner, c.spender)?.abi_encode().into(),
        C::supplyCap(_) => store.supply_cap()?.abi_encode().into(),
        C::nonces(c) => store.nonce(c.owner)?.abi_encode().into(),
        C::contractURI(_) => store.contract_uri()?.abi_encode().into(),

        // --- Role identifiers ---
        C::DEFAULT_ADMIN_ROLE(_) => ids::DEFAULT_ADMIN_ROLE.abi_encode().into(),
        C::MINT_ROLE(_) => ids::MINT_ROLE.abi_encode().into(),
        C::BURN_ROLE(_) => ids::BURN_ROLE.abi_encode().into(),
        C::BURN_BLOCKED_ROLE(_) => ids::BURN_BLOCKED_ROLE.abi_encode().into(),
        C::PAUSE_ROLE(_) => ids::PAUSE_ROLE.abi_encode().into(),
        C::UNPAUSE_ROLE(_) => ids::UNPAUSE_ROLE.abi_encode().into(),
        C::METADATA_ROLE(_) => ids::METADATA_ROLE.abi_encode().into(),

        // --- Policy scope identifiers ---
        C::TRANSFER_SENDER_POLICY(_) => ids::TRANSFER_SENDER_POLICY.abi_encode().into(),
        C::TRANSFER_RECEIVER_POLICY(_) => ids::TRANSFER_RECEIVER_POLICY.abi_encode().into(),
        C::TRANSFER_EXECUTOR_POLICY(_) => ids::TRANSFER_EXECUTOR_POLICY.abi_encode().into(),
        C::MINT_RECEIVER_POLICY(_) => ids::MINT_RECEIVER_POLICY.abi_encode().into(),

        // --- Role reads ---
        C::hasRole(c) => store.has_role(c.role, c.account)?.abi_encode().into(),
        C::getRoleAdmin(c) => store.role_admin(c.role)?.abi_encode().into(),

        // --- Pause reads ---
        C::pausedFeatures(_) => ops::paused_features(store)?.abi_encode().into(),
        C::isPaused(c) => ops::is_paused(store, c.feature)?.abi_encode().into(),

        // --- Policy reads ---
        C::policyId(c) => ops::policy_id(store, c.policyScope)?.abi_encode().into(),

        // --- Domain reads ---
        C::DOMAIN_SEPARATOR(_) => permit::domain_separator(store, chain_id)?.abi_encode().into(),
        C::eip712Domain(_) => {
            let (fields, name, version, chain_id, verifying_contract, salt, extensions) =
                permit::eip712_domain(store, chain_id)?;
            IB20::eip712DomainCall::abi_encode_returns(&IB20::eip712DomainReturn {
                fields,
                name,
                version,
                chainId: chain_id,
                verifyingContract: verifying_contract,
                salt,
                extensions,
            })
            .into()
        }

        // --- ERC-20 mutating ---
        C::transfer(c) => {
            ops::transfer(store, caller, c.to, c.amount, privileged)?;
            true.abi_encode().into()
        }
        C::transferFrom(c) => {
            ops::transfer_from(store, caller, c.from, c.to, c.amount, privileged)?;
            true.abi_encode().into()
        }
        C::approve(c) => {
            ops::approve(store, caller, c.spender, c.amount)?;
            true.abi_encode().into()
        }
        C::transferWithMemo(c) => {
            ops::transfer_with_memo(store, caller, c.to, c.amount, c.memo, privileged)?;
            true.abi_encode().into()
        }
        C::transferFromWithMemo(c) => {
            ops::transfer_from_with_memo(
                store, caller, c.from, c.to, c.amount, c.memo, privileged,
            )?;
            true.abi_encode().into()
        }

        // --- Mint ---
        C::mint(c) => {
            ops::mint(store, caller, c.to, c.amount, privileged)?;
            Bytes::new()
        }
        C::mintWithMemo(c) => {
            ops::mint_with_memo(store, caller, c.to, c.amount, c.memo, privileged)?;
            Bytes::new()
        }

        // --- Burn ---
        // Self-burns are never factory-privileged: during init the caller is the factory,
        // not a token holder.
        C::burn(c) => {
            ops::burn(store, caller, caller, c.amount, false)?;
            Bytes::new()
        }
        C::burnWithMemo(c) => {
            ops::burn_with_memo(store, caller, caller, c.amount, c.memo, false)?;
            Bytes::new()
        }
        C::burnBlocked(c) => {
            ops::burn_blocked(store, caller, c.from, c.amount, privileged)?;
            Bytes::new()
        }

        // --- Pause ---
        C::pause(c) => {
            ops::pause(store, caller, c.features, privileged)?;
            Bytes::new()
        }
        C::unpause(c) => {
            ops::unpause(store, caller, c.features, privileged)?;
            Bytes::new()
        }

        // --- Admin ---
        C::updateSupplyCap(c) => {
            ops::update_supply_cap(store, caller, c.newSupplyCap, privileged)?;
            Bytes::new()
        }
        C::updateName(c) => {
            ops::update_name(store, caller, c.newName, privileged)?;
            Bytes::new()
        }
        C::updateSymbol(c) => {
            ops::update_symbol(store, caller, c.newSymbol, privileged)?;
            Bytes::new()
        }
        C::updateContractURI(c) => {
            ops::update_contract_uri(store, caller, c.newURI, privileged)?;
            Bytes::new()
        }

        // --- Role mutations ---
        C::grantRole(c) => {
            ops::grant_role(store, caller, c.role, c.account, privileged)?;
            Bytes::new()
        }
        C::revokeRole(c) => {
            ops::revoke_role(store, caller, c.role, c.account, privileged)?;
            Bytes::new()
        }
        // Renounce operations are never factory-privileged.
        C::renounceRole(c) => {
            ops::renounce_role(store, caller, c.role, c.callerConfirmation)?;
            Bytes::new()
        }
        C::renounceLastAdmin(_) => {
            ops::renounce_last_admin(store, caller)?;
            Bytes::new()
        }
        C::setRoleAdmin(c) => {
            ops::set_role_admin(store, caller, c.role, c.newAdminRole, privileged)?;
            Bytes::new()
        }

        // --- Policy mutations ---
        C::updatePolicy(c) => {
            ops::update_policy(store, caller, c.policyScope, c.newPolicyId, privileged)?;
            Bytes::new()
        }

        // --- Permit ---
        C::permit(c) => {
            permit::permit(
                store,
                chain_id,
                timestamp,
                PermitArgs {
                    owner: c.owner,
                    spender: c.spender,
                    value: c.value,
                    deadline: c.deadline,
                    v: c.v,
                    r: c.r,
                    s: c.s,
                },
            )?;
            Bytes::new()
        }
    };
    Ok(encoded)
}

fn run_asset<P: B20Port>(
    store: &mut B20Store<'_, P>,
    call: A,
    privileged: bool,
) -> Result<Bytes> {
    let caller = store.port().caller();

    let encoded: Bytes = match call {
        // --- Role / precision constants ---
        A::OPERATOR_ROLE(_) => ids::OPERATOR_ROLE.abi_encode().into(),
        A::WAD_PRECISION(_) => WAD.abi_encode().into(),

        // --- Multiplier reads ---
        A::multiplier(_) => store.multiplier()?.abi_encode().into(),
        A::toScaledBalance(c) => ops::to_scaled_balance(store, c.rawBalance)?.abi_encode().into(),
        A::toRawBalance(c) => ops::to_raw_balance(store, c.scaledBalance)?.abi_encode().into(),
        A::scaledBalanceOf(c) => ops::scaled_balance_of(store, c.account)?.abi_encode().into(),

        // --- Announcement / metadata reads ---
        A::isAnnouncementIdUsed(c) => {
            store.is_announcement_id_used(c.id.as_str())?.abi_encode().into()
        }
        A::extraMetadata(c) => store.extra_metadata(c.key.as_str())?.abi_encode().into(),

        // --- Mutations ---
        A::updateMultiplier(c) => {
            ops::update_multiplier(store, caller, c.newMultiplier, privileged)?;
            Bytes::new()
        }
        A::updateExtraMetadata(c) => {
            ops::update_extra_metadata(store, caller, c.key, c.value, privileged)?;
            Bytes::new()
        }
        A::batchMint(c) => {
            ops::batch_mint(store, caller, c.recipients, c.amounts, privileged)?;
            Bytes::new()
        }
        A::announce(c) => {
            announce(store, c, privileged)?;
            Bytes::new()
        }
    };
    Ok(encoded)
}

/// Posts an announcement and atomically executes its `internalCalls` by self-dispatch.
///
/// Base deliberately routes each inner call as a direct Rust call rather than a
/// DELEGATECALL: the native precompile pays for each sub-call's storage work but not for
/// EVM call-frame overhead. Reproducing that structure is what keeps the gas equal.
fn announce<P: B20Port>(
    store: &mut B20Store<'_, P>,
    call: IB20Asset::announceCall,
    privileged: bool,
) -> Result<()> {
    let caller = store.port().caller();
    if !privileged {
        ops::ensure_role(store, caller, ids::OPERATOR_ROLE)?;
    }

    let id = call.id;
    if store.is_announcement_id_used(id.as_str())? {
        return Err(B20Error::revert(IB20Asset::AnnouncementIdAlreadyUsed { id }));
    }
    store.mark_announcement_id_used(id.as_str())?;

    let address = store.address();
    store.port().emit_event(
        address,
        alloy::sol_types::SolEvent::encode_log_data(&IB20Asset::Announcement {
            caller,
            id: id.clone(),
            description: call.description,
            uri: call.uri,
        }),
    )?;

    for inner in &call.internalCalls {
        let bytes: &[u8] = inner.as_ref();
        if bytes.len() < 4 {
            return Err(B20Error::revert(IB20Asset::InternalCallMalformed {
                call: inner.clone(),
            }));
        }
        if bytes[..4] == IB20Asset::announceCall::SELECTOR {
            return Err(B20Error::revert(IB20Asset::AnnouncementInProgress {}));
        }
        run(store, bytes, privileged).map_err(|err| match err {
            // System errors propagate unchanged; ordinary reverts are wrapped.
            B20Error::OutOfGas | B20Error::StaticCallViolation | B20Error::Fatal(_) => err,
            B20Error::UnderOverflow => err,
            B20Error::Revert(_) => {
                B20Error::revert(IB20Asset::InternalCallFailed { call: inner.clone() })
            }
        })?;
    }

    let address = store.address();
    store.port().emit_event(
        address,
        alloy::sol_types::SolEvent::encode_log_data(&IB20Asset::EndAnnouncement { id }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calldata_gas_is_six_per_word_rounded_up() {
        assert_eq!(calldata_gas_cost(&[]), 0);
        assert_eq!(calldata_gas_cost(&[0u8; 1]), 6);
        assert_eq!(calldata_gas_cost(&[0u8; 32]), 6);
        assert_eq!(calldata_gas_cost(&[0u8; 33]), 12);
        // transfer(address,uint256): 4-byte selector + two words.
        assert_eq!(calldata_gas_cost(&[0u8; 68]), 18);
    }

    /// `transfer` must resolve through the inherited IB20 set for both variants —
    /// the bug this port fixes was that it resolved through neither.
    #[test]
    fn transfer_selector_is_recognized() {
        let selector = IB20::transferCall::SELECTOR;
        assert_eq!(selector, [0xa9, 0x05, 0x9c, 0xbb]);
        assert!(C::valid_selector(selector));
        assert!(!A::valid_selector(selector));
    }
}
