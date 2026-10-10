//! Classic `stdlib/biguint.mini` edge cases after Ethereum's resource checks.
use alloy::primitives::{Bytes, U256};
use revm::precompile::utilities::right_pad_with_offset;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Rules {
    Legacy,
    ExponentSize,
    UintFastPath,
}

/// Preserve the Mini implementation's zero exponent and empty-array behavior.
/// Called only after the stock MODEXP succeeds, keeping its bounded allocation
/// and gas checks before doing any additional work.
pub(super) fn adjust_output(
    input: &[u8],
    output: &mut Bytes,
    gas_limit: u64,
    rules: Rules,
) -> Result<(), ()> {
    if output.is_empty() {
        return Ok(());
    }
    let length = |offset| {
        usize::try_from(U256::from_be_slice(
            right_pad_with_offset::<32>(input, offset).as_ref(),
        ))
        .map_err(|_| ())
    };
    let base_len = length(0)?;
    let exp_len = length(32)?;
    let mod_len = length(64)?;
    let exp_start = 96usize.checked_add(base_len).ok_or(())?;
    let mod_start = exp_start.checked_add(exp_len).ok_or(())?;
    let is_zero = |start: usize, len: usize| {
        input
            .get(start.min(input.len())..start.saturating_add(len).min(input.len()))
            .unwrap_or_default()
            .iter()
            .all(|byte| *byte == 0)
    };
    if is_zero(mod_start, mod_len) {
        return Ok(());
    }
    let set_one = |output: &mut Bytes| {
        let mut bytes = vec![0; output.len()];
        *bytes.last_mut().unwrap() = 1;
        *output = bytes.into();
    };
    if rules == Rules::Legacy {
        // The pre-ArbOS-49 loop used the BASE's effective byte length as
        // its exponent bound (biguint.mini before 039c03b3e/149bb2fc4).
        // A zero/empty base therefore returns one without any reduction.
        let base_size = input
            .get(96..exp_start.min(input.len()))
            .unwrap_or_default()
            .iter()
            .position(|byte| *byte != 0)
            .map_or(0, |first| base_len - first);
        if base_size == 0 {
            set_one(output);
        } else if exp_len > base_size && !is_zero(exp_start, exp_len - base_size) {
            // Ignore high exponent bytes but retain all declared lengths and
            // virtual right-padding. The original Ethereum call has already
            // enforced the gas/resource bound; clearing bits cannot increase it.
            let mut normalized = input.to_vec();
            let end = (exp_start + exp_len - base_size).min(normalized.len());
            normalized[exp_start.min(end)..end].fill(0);
            *output = revm::precompile::modexp::berlin_run(&normalized, gas_limit)
                .map_err(|_| ())?
                .bytes;
        }
        return Ok(());
    }
    let fits_uint = |start, len: usize| len <= 32 || is_zero(start, len - 32);
    // biguint_toUint indexes x[0] for an empty array. The modulus is tested
    // first, then the base, then the exponent; the wide-number path skips it.
    if rules == Rules::UintFastPath
        && fits_uint(mod_start, mod_len)
        && (base_len == 0 || (fits_uint(96, base_len) && exp_len == 0))
    {
        return Err(());
    }
    // In the wide-number path a nonzero exponent multiplies by the base;
    // biguint_mul also indexes an empty base array and throws.
    if base_len == 0 && !is_zero(exp_start, exp_len) {
        return Err(());
    }
    // Classic returns one without reducing it modulo m when e == 0,
    // including m == 1. Keep the declared modulus width and zero padding.
    if is_zero(exp_start, exp_len) {
        set_one(output);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reproduces_each_historical_biguint_implementation() {
        // Expected values from the native archive at heights 1107013,
        // 2965603 and 4198902, respectively.
        type Case<'a> = (&'a [u8], &'a [u8], &'a [u8], [Option<&'a [u8]>; 3]);
        let padded_base = [vec![0; 32], vec![2]].concat();
        let cases: &[Case<'_>] = &[
            (&[2], &[1, 0], &[13], [Some(&[1]), Some(&[3]), Some(&[3])]),
            (
                &padded_base,
                &[1, 0],
                &[13],
                [Some(&[1]), Some(&[3]), Some(&[3])],
            ),
            (&[0], &[1], &[13], [Some(&[1]), Some(&[0]), Some(&[0])]),
            (&[], &[1], &[13], [Some(&[1]), None, None]),
            (&[2], &[], &[13], [Some(&[1]), Some(&[1]), None]),
            (&[2], &[0], &[1], [Some(&[0]), Some(&[1]), Some(&[1])]),
            (&[], &[0], &[1], [Some(&[1]), Some(&[1]), None]),
            (&[], &[], &[0], [Some(&[0]), Some(&[0]), Some(&[0])]),
        ];
        for (b, e, m, expected) in cases {
            let input = [
                U256::from(b.len()).to_be_bytes::<32>().as_slice(),
                U256::from(e.len()).to_be_bytes::<32>().as_slice(),
                U256::from(m.len()).to_be_bytes::<32>().as_slice(),
                b,
                e,
                m,
            ]
            .concat();
            for (rules, expected) in [Rules::Legacy, Rules::ExponentSize, Rules::UintFastPath]
                .into_iter()
                .zip(expected)
            {
                let mut output = revm::precompile::modexp::berlin_run(&input, 1_000_000)
                    .unwrap()
                    .bytes;
                let result = adjust_output(&input, &mut output, 1_000_000, rules);
                assert_eq!(result.is_ok(), expected.is_some());
                if let Some(expected) = expected {
                    assert_eq!(output.as_ref(), *expected);
                }
            }
        }
    }
}
