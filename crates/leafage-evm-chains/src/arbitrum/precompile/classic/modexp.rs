//! Classic `stdlib/biguint.mini` edge cases after Ethereum's resource checks.
use alloy::primitives::{Bytes, U256};
use revm::precompile::utilities::right_pad_with_offset;

/// Preserve the Mini implementation's zero exponent and empty-array behavior.
/// Called only after the stock MODEXP succeeds, keeping its bounded allocation
/// and gas checks before doing any additional work.
pub(super) fn adjust_output(input: &[u8], output: &mut Bytes) -> Result<(), ()> {
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
    let fits_uint = |start, len: usize| len <= 32 || is_zero(start, len - 32);
    // biguint_toUint indexes x[0] for an empty array. The modulus is tested
    // first, then the base, then the exponent; the wide-number path skips it.
    if fits_uint(mod_start, mod_len) && (base_len == 0 || (fits_uint(96, base_len) && exp_len == 0))
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
        let mut bytes = vec![0; output.len()];
        *bytes.last_mut().unwrap() = 1;
        *output = bytes.into();
    }
    Ok(())
}
