//! Tempo transaction fee-token resolution.
//!
//! The writer resolves the token once, before executing any call. Keep the
//! transaction-only decision logic here so execution and RPC estimation cannot
//! drift into separate partial implementations.

use alloy::primitives::{Address, TxKind, U256};
use alloy::sol_types::SolCall;

use super::precompile::Result;
use super::{
    hardfork::TempoHardfork,
    precompile::{
        fee_manager::{IFeeManager, TipFeeManager},
        stablecoin_dex::IStablecoinDEX,
        storage_types::Handler,
        tip20::{is_tip20_prefix, TIP20Token, ITIP20},
        DEFAULT_FEE_TOKEN, STABLECOIN_DEX_ADDRESS, TEMPO_GAS_PRICE_SCALING_FACTOR,
        TIP_FEE_MANAGER_ADDRESS,
    },
    tx::TempoTxEnv,
};

/// No TIP-1115 fallback token covers the maximum fee on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InsufficientFallbackBalance {
    /// Fallback token with the largest fee-payer balance; ties keep the earlier token.
    pub(crate) token: Address,
    /// Largest fallback-token balance of the fee payer.
    pub(crate) available: U256,
    /// Maximum fee in TIP-20 base units.
    pub(crate) required: U256,
}

/// Maximum fee in TIP-20 base units, as `calc_gas_balance_spending` in the writer.
pub(crate) fn max_fee_in_token_units(gas_limit: u64, max_fee_per_gas: u128) -> U256 {
    U256::from(gas_limit)
        .saturating_mul(U256::from(max_fee_per_gas))
        .div_ceil(TEMPO_GAS_PRICE_SCALING_FACTOR)
}

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
///
/// `max_fee` is in TIP-20 base units; zero skips the T13 fallback balance reads.
pub(crate) fn resolve_from_storage(
    candidates: FeeTokenCandidates,
    fee_payer: Address,
    spec: TempoHardfork,
    max_fee: U256,
) -> Result<core::result::Result<Address, InsufficientFallbackBalance>> {
    resolve(
        candidates,
        spec,
        max_fee,
        || TipFeeManager::new().user_tokens[fee_payer].read(),
        is_valid_fee_token,
        |token| TIP20Token::from_address_unchecked(token).balances[fee_payer].read(),
    )
}

fn resolve<E>(
    candidates: FeeTokenCandidates,
    spec: TempoHardfork,
    max_fee: U256,
    mut stored_user_token: impl FnMut() -> core::result::Result<Address, E>,
    mut valid_fee_token: impl FnMut(Address) -> core::result::Result<bool, E>,
    mut balance_of: impl FnMut(Address) -> core::result::Result<U256, E>,
) -> core::result::Result<core::result::Result<Address, InsufficientFallbackBalance>, E> {
    if let Some(token) = candidates.explicit {
        return Ok(Ok(token));
    }
    if let Some(token) = candidates.preference_update {
        return Ok(Ok(token));
    }

    let stored = stored_user_token()?;
    if !stored.is_zero() {
        return Ok(Ok(stored));
    }

    if let Some(token) = candidates.tip20_call {
        if valid_fee_token(token)? {
            return Ok(Ok(token));
        }
    }
    if let Some(token) = candidates.dex_input {
        if valid_fee_token(token)? {
            return Ok(Ok(token));
        }
    }

    if !spec.is_t13() || max_fee.is_zero() {
        return Ok(Ok(DEFAULT_FEE_TOKEN));
    }

    // TIP-1115: the first fallback token that covers the whole maximum fee pays it.
    let mut richest = InsufficientFallbackBalance {
        token: DEFAULT_FEE_TOKEN,
        available: U256::ZERO,
        required: max_fee,
    };
    for &token in spec.fallback_fee_tokens() {
        let balance = balance_of(token)?;
        if balance >= max_fee {
            return Ok(Ok(token));
        }
        if balance > richest.available {
            richest.token = token;
            richest.available = balance;
        }
    }
    Ok(Err(richest))
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
        resolve::<core::convert::Infallible>(
            candidates,
            spec,
            U256::ZERO,
            || Ok(stored),
            |token| Ok(valid(token)),
            |_| unreachable!("free transactions do not read fallback balances"),
        )
        .unwrap()
        .unwrap()
    }

    /// Resolves a transaction that reaches the fallback step, recording balance reads.
    fn resolve_fallback(
        spec: TempoHardfork,
        max_fee: u64,
        balances: &[(Address, u64)],
    ) -> (
        core::result::Result<Address, InsufficientFallbackBalance>,
        Vec<Address>,
    ) {
        let caller = Address::random();
        let candidates = FeeTokenCandidates::from_tx(&aa_tx(caller, Vec::new()), caller, spec);
        let mut reads = Vec::new();
        let result = resolve::<core::convert::Infallible>(
            candidates,
            spec,
            U256::from(max_fee),
            || Ok(Address::ZERO),
            |_| Ok(false),
            |token| {
                reads.push(token);
                Ok(balances
                    .iter()
                    .find(|(holder, _)| *holder == token)
                    .map_or(U256::ZERO, |(_, balance)| U256::from(*balance)))
            },
        )
        .unwrap();
        (result, reads)
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

    #[test]
    fn t13_fallback_uses_first_token_covering_max_fee() {
        let [path_usd, ousd] = TempoHardfork::T13.fallback_fee_tokens() else {
            panic!("T13 has two fallback fee tokens");
        };
        let (path_usd, ousd) = (*path_usd, *ousd);

        assert_eq!(
            resolve_fallback(TempoHardfork::T13, 100, &[(path_usd, 100), (ousd, 500)]),
            (Ok(path_usd), vec![path_usd])
        );
        assert_eq!(
            resolve_fallback(TempoHardfork::T13, 100, &[(path_usd, 99), (ousd, 100)]),
            (Ok(ousd), vec![path_usd, ousd])
        );
        // One token must pay the whole fee, even if the balances add up to it.
        assert_eq!(
            resolve_fallback(TempoHardfork::T13, 100, &[(path_usd, 60), (ousd, 70)]),
            (
                Err(InsufficientFallbackBalance {
                    token: ousd,
                    available: U256::from(70),
                    required: U256::from(100),
                }),
                vec![path_usd, ousd]
            )
        );
        // Equal balances keep the earlier token, as the writer does.
        assert_eq!(
            resolve_fallback(TempoHardfork::T13, 100, &[(path_usd, 60), (ousd, 60)]),
            (
                Err(InsufficientFallbackBalance {
                    token: path_usd,
                    available: U256::from(60),
                    required: U256::from(100),
                }),
                vec![path_usd, ousd]
            )
        );
    }

    #[test]
    fn fallback_reads_no_balance_before_t13_or_without_max_fee() {
        assert_eq!(
            resolve_fallback(TempoHardfork::T12, 100, &[]),
            (Ok(DEFAULT_FEE_TOKEN), vec![])
        );
        assert_eq!(
            resolve_fallback(TempoHardfork::T13, 0, &[]),
            (Ok(DEFAULT_FEE_TOKEN), vec![])
        );
    }

    #[test]
    fn earlier_rules_take_precedence_over_t13_fallback() {
        let caller = Address::random();
        let stored = Address::random();
        let candidates =
            FeeTokenCandidates::from_tx(&aa_tx(caller, Vec::new()), caller, TempoHardfork::T13);
        let result = resolve::<core::convert::Infallible>(
            candidates,
            TempoHardfork::T13,
            U256::from(100),
            || Ok(stored),
            |_| Ok(false),
            |_| unreachable!("a stored preference skips the fallback"),
        );
        assert_eq!(result, Ok(Ok(stored)));
    }

    #[test]
    fn max_fee_rounds_up_to_token_units() {
        assert_eq!(max_fee_in_token_units(0, 1_000_000_000_000), U256::ZERO);
        assert_eq!(max_fee_in_token_units(1, 1_000_000_000_000), U256::from(1));
        assert_eq!(max_fee_in_token_units(1, 1_000_000_000_001), U256::from(2));
        assert_eq!(
            max_fee_in_token_units(u64::MAX, u128::MAX),
            (U256::from(u64::MAX) * U256::from(u128::MAX)).div_ceil(TEMPO_GAS_PRICE_SCALING_FACTOR)
        );
    }
}
