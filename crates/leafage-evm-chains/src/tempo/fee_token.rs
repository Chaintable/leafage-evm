//! Tempo transaction fee-token resolution.
//!
//! The writer resolves the token once, before executing any call. Keep the
//! transaction-only decision logic here so execution and RPC estimation cannot
//! drift into separate partial implementations.

use alloy::primitives::{Address, TxKind};
use alloy::sol_types::SolCall;

use super::precompile::Result;
use super::{
    hardfork::TempoHardfork,
    precompile::{
        fee_manager::{IFeeManager, TipFeeManager},
        stablecoin_dex::IStablecoinDEX,
        storage_types::Handler,
        tip20::{is_tip20_prefix, TIP20Token, ITIP20},
        DEFAULT_FEE_TOKEN, STABLECOIN_DEX_ADDRESS, TIP_FEE_MANAGER_ADDRESS,
    },
    tx::TempoTxEnv,
};

/// State-independent candidates, in the precedence order used by the writer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FeeTokenCandidates {
    explicit: Option<Address>,
    preference_update: Option<Address>,
    tip20_call: Option<Address>,
    dex_input: Option<Address>,
}

impl FeeTokenCandidates {
    pub(crate) fn from_tx(tx: &TempoTxEnv, fee_payer: Address, spec: TempoHardfork) -> Self {
        let explicit = tx.fee_token();

        let preference_update = if !tx.is_aa() && fee_payer == tx.base.caller {
            tx.calls().next().and_then(|(kind, input)| {
                (kind.to() == Some(&TIP_FEE_MANAGER_ADDRESS))
                    .then(|| IFeeManager::setUserTokenCall::abi_decode(input).ok())
                    .flatten()
                    .map(|call| call.token)
            })
        } else {
            None
        };

        let tip20_call = tx
            .calls()
            .next()
            .and_then(|(kind, _)| kind.to().copied())
            .filter(|target| {
                (!tx.is_aa() || fee_payer == tx.base.caller)
                    && tx.calls().all(|(kind, input)| {
                        kind.to() == Some(target) && is_tip20_fee_inference_call(spec, input)
                    })
            });

        let mut calls = tx.calls();
        let dex_input = calls.next().and_then(|(kind, input)| {
            if kind != TxKind::Call(STABLECOIN_DEX_ADDRESS)
                || (tx.is_aa() && calls.next().is_some())
            {
                return None;
            }

            IStablecoinDEX::swapExactAmountInCall::abi_decode(input)
                .map(|call| call.tokenIn)
                .or_else(|_| {
                    IStablecoinDEX::swapExactAmountOutCall::abi_decode(input)
                        .map(|call| call.tokenIn)
                })
                .ok()
        });

        Self {
            explicit,
            preference_update,
            tip20_call,
            dex_input,
        }
    }
}

/// Resolves candidates using the currently installed precompile storage context.
pub(crate) fn resolve_from_storage(
    candidates: FeeTokenCandidates,
    fee_payer: Address,
) -> Result<Address> {
    resolve(
        candidates,
        || TipFeeManager::new().user_tokens[fee_payer].read(),
        is_valid_fee_token,
    )
}

fn resolve<E>(
    candidates: FeeTokenCandidates,
    mut stored_user_token: impl FnMut() -> core::result::Result<Address, E>,
    mut valid_fee_token: impl FnMut(Address) -> core::result::Result<bool, E>,
) -> core::result::Result<Address, E> {
    if let Some(token) = candidates.explicit {
        return Ok(token);
    }
    if let Some(token) = candidates.preference_update {
        return Ok(token);
    }

    let stored = stored_user_token()?;
    if !stored.is_zero() {
        return Ok(stored);
    }

    if let Some(token) = candidates.tip20_call {
        if valid_fee_token(token)? {
            return Ok(token);
        }
    }
    if let Some(token) = candidates.dex_input {
        if valid_fee_token(token)? {
            return Ok(token);
        }
    }

    Ok(DEFAULT_FEE_TOKEN)
}

fn is_valid_fee_token(token: Address) -> Result<bool> {
    if !is_tip20_prefix(token) {
        return Ok(false);
    }

    let currency = &TIP20Token::from_address_unchecked(token).currency;
    Ok(currency.len()? == 3 && currency.read()?.as_str() == "USD")
}

/// Returns true for TIP-20 calls from which Tempo permits fee-token inference.
fn is_tip20_fee_inference_call(spec: TempoHardfork, input: &[u8]) -> bool {
    input.first_chunk::<4>().is_some_and(|selector| {
        matches!(
            *selector,
            ITIP20::transferCall::SELECTOR | ITIP20::transferWithMemoCall::SELECTOR
        ) || (!spec.is_t7() && *selector == ITIP20::distributeRewardCall::SELECTOR)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempo::{
        precompile::PATH_USD_ADDRESS,
        tx::{TempoCall, TempoTxFields},
    };
    use alloy::primitives::{address, Bytes, U256};

    fn base_tx(caller: Address, to: Address, input: Bytes) -> TempoTxEnv {
        TempoTxEnv {
            base: revm::context::TxEnv {
                caller,
                kind: TxKind::Call(to),
                data: input,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn aa_tx(caller: Address, calls: Vec<TempoCall>) -> TempoTxEnv {
        TempoTxEnv {
            base: revm::context::TxEnv {
                caller,
                ..Default::default()
            },
            tempo_fields: Some(TempoTxFields {
                aa_calls: calls,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn call(to: Address, input: Vec<u8>) -> TempoCall {
        TempoCall {
            to: TxKind::Call(to),
            value: U256::ZERO,
            input: input.into(),
        }
    }

    fn resolve_for_test(
        tx: &TempoTxEnv,
        payer: Address,
        spec: TempoHardfork,
        stored: Address,
        valid: impl Fn(Address) -> bool,
    ) -> Address {
        let candidates = FeeTokenCandidates::from_tx(tx, payer, spec);
        resolve::<core::convert::Infallible>(candidates, || Ok(stored), |token| Ok(valid(token)))
            .unwrap()
    }

    #[test]
    fn explicit_token_has_highest_precedence() {
        let caller = Address::random();
        let explicit = Address::random();
        let stored = Address::random();
        let mut tx = aa_tx(caller, Vec::new());
        tx.tempo_fields.as_mut().unwrap().fee_token = Some(explicit);

        assert_eq!(
            resolve_for_test(&tx, caller, TempoHardfork::T11, stored, |_| false),
            explicit
        );
    }

    #[test]
    fn non_aa_set_user_token_applies_immediately() {
        let caller = Address::random();
        let token = Address::random();
        let input = IFeeManager::setUserTokenCall { token }.abi_encode().into();
        let tx = base_tx(caller, TIP_FEE_MANAGER_ADDRESS, input);

        assert_eq!(
            resolve_for_test(&tx, caller, TempoHardfork::T11, Address::ZERO, |_| false),
            token
        );
        assert_eq!(
            resolve_for_test(
                &tx,
                Address::random(),
                TempoHardfork::T11,
                Address::ZERO,
                |_| false
            ),
            DEFAULT_FEE_TOKEN
        );
    }

    #[test]
    fn stored_preference_precedes_call_inference() {
        let caller = Address::random();
        let stored = Address::random();
        let input = ITIP20::transferCall {
            to: Address::random(),
            amount: U256::ONE,
        }
        .abi_encode()
        .into();
        let tx = base_tx(caller, PATH_USD_ADDRESS, input);

        assert_eq!(
            resolve_for_test(&tx, caller, TempoHardfork::T11, stored, |_| true),
            stored
        );
    }

    #[test]
    fn tip20_inference_requires_matching_calls_and_unsponsored_aa() {
        let caller = Address::random();
        let other_token = address!("0x20C0000000000000000000000000000000000001");
        let transfer = ITIP20::transferCall {
            to: Address::random(),
            amount: U256::ONE,
        }
        .abi_encode();
        let memo_transfer = ITIP20::transferWithMemoCall {
            to: Address::random(),
            amount: U256::ONE,
            memo: Default::default(),
        }
        .abi_encode();

        let matching = aa_tx(
            caller,
            vec![
                call(PATH_USD_ADDRESS, transfer.clone()),
                call(PATH_USD_ADDRESS, memo_transfer),
            ],
        );
        assert_eq!(
            resolve_for_test(&matching, caller, TempoHardfork::T11, Address::ZERO, |_| {
                true
            }),
            PATH_USD_ADDRESS
        );
        assert_eq!(
            resolve_for_test(
                &matching,
                Address::random(),
                TempoHardfork::T11,
                Address::ZERO,
                |_| true
            ),
            DEFAULT_FEE_TOKEN
        );

        let mixed = aa_tx(
            caller,
            vec![
                call(PATH_USD_ADDRESS, transfer.clone()),
                call(other_token, transfer),
            ],
        );
        assert_eq!(
            resolve_for_test(&mixed, caller, TempoHardfork::T11, Address::ZERO, |_| true),
            DEFAULT_FEE_TOKEN
        );
    }

    #[test]
    fn reward_inference_stops_at_t7() {
        let caller = Address::random();
        let input = ITIP20::distributeRewardCall { amount: U256::ONE }
            .abi_encode()
            .into();
        let tx = base_tx(caller, PATH_USD_ADDRESS, input);

        assert_eq!(
            resolve_for_test(&tx, caller, TempoHardfork::T6, Address::ZERO, |_| true),
            PATH_USD_ADDRESS
        );
        assert_eq!(
            resolve_for_test(&tx, caller, TempoHardfork::T7, Address::ZERO, |_| true),
            DEFAULT_FEE_TOKEN
        );
    }

    #[test]
    fn single_dex_swap_uses_valid_input_token() {
        let caller = Address::random();
        let token_out = address!("0x20C0000000000000000000000000000000000001");
        let input = IStablecoinDEX::swapExactAmountInCall {
            tokenIn: PATH_USD_ADDRESS,
            tokenOut: token_out,
            amountIn: 100,
            minAmountOut: 90,
        }
        .abi_encode();
        let tx = aa_tx(caller, vec![call(STABLECOIN_DEX_ADDRESS, input.clone())]);
        assert_eq!(
            resolve_for_test(&tx, caller, TempoHardfork::T11, Address::ZERO, |token| {
                token == PATH_USD_ADDRESS
            }),
            PATH_USD_ADDRESS
        );

        let multi = aa_tx(
            caller,
            vec![
                call(STABLECOIN_DEX_ADDRESS, input.clone()),
                call(STABLECOIN_DEX_ADDRESS, input),
            ],
        );
        assert_eq!(
            resolve_for_test(&multi, caller, TempoHardfork::T11, Address::ZERO, |_| true),
            DEFAULT_FEE_TOKEN
        );
    }
}
