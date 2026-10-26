//! Calldata decoding and returndata encoding.
//!
//! Port of go-bsc `core/vm/cas20_abi.go`. Decoding is strict in the way Solidity's external
//! decoder is: a word with dirty high bytes for a narrower type, or an offset/length with dirty
//! high bits, is a malformed encoding and reverts with empty returndata.

use alloy::primitives::{Address, B256, U256};

use super::errors::{Cas20Result, REVERTED};

pub(crate) fn read_word(args: &[u8], i: usize) -> Cas20Result<B256> {
    let off = i * 32;
    if args.len() < off + 32 {
        return Err(REVERTED);
    }
    Ok(B256::from_slice(&args[off..off + 32]))
}

/// `wordFitsIn`: every byte above the low `n` is zero.
pub(crate) fn word_fits_in(w: &B256, n: usize) -> bool {
    w[..32 - n].iter().all(|b| *b == 0)
}

pub(crate) fn u64_from_word(w: &B256) -> Option<u64> {
    word_fits_in(w, 8).then(|| u64::from_be_bytes(w[24..].try_into().unwrap()))
}

pub(crate) fn address_from_word(w: &B256) -> Option<Address> {
    word_fits_in(w, 20).then(|| Address::from_slice(&w[12..]))
}

pub(crate) fn read_address(args: &[u8], i: usize) -> Cas20Result<Address> {
    address_from_word(&read_word(args, i)?).ok_or(REVERTED)
}

pub(crate) fn read_u64(args: &[u8], i: usize) -> Cas20Result<u64> {
    u64_from_word(&read_word(args, i)?).ok_or(REVERTED)
}

pub(crate) fn read_u256(args: &[u8], i: usize) -> Cas20Result<U256> {
    Ok(U256::from_be_bytes(read_word(args, i)?.0))
}

/// `isEnumWord`.
pub(crate) fn is_enum_word(w: &B256, max: u8) -> bool {
    word_fits_in(w, 1) && w[31] <= max
}

/// `readStrictUint8`.
pub(crate) fn read_strict_u8(args: &[u8], i: usize) -> Cas20Result<u8> {
    let w = read_word(args, i)?;
    if !is_enum_word(&w, 0xff) {
        return Err(REVERTED);
    }
    Ok(w[31])
}

pub(crate) fn enc_u256(v: U256) -> Vec<u8> {
    v.to_be_bytes::<32>().to_vec()
}

pub(crate) fn enc_bool(b: bool) -> Vec<u8> {
    let mut out = vec![0u8; 32];
    if b {
        out[31] = 1;
    }
    out
}

pub(crate) fn enc_string(s: &[u8]) -> Vec<u8> {
    encode_tuple(vec![abi_string(s)])
}

/// One head entry of an ABI tuple: a static word, or a dynamic part with its tail.
pub(crate) enum AbiPart {
    Word(B256),
    Dynamic(Vec<u8>),
}

pub(crate) fn abi_word(w: B256) -> AbiPart {
    AbiPart::Word(w)
}

pub(crate) fn abi_bytes(b: &[u8]) -> AbiPart {
    let padded = b.len().div_ceil(32) * 32;
    let mut tail = vec![0u8; 32 + padded];
    tail[..32].copy_from_slice(&U256::from(b.len()).to_be_bytes::<32>());
    tail[32..32 + b.len()].copy_from_slice(b);
    AbiPart::Dynamic(tail)
}

pub(crate) fn abi_string(s: impl AsRef<[u8]>) -> AbiPart {
    abi_bytes(s.as_ref())
}

pub(crate) fn abi_word_array(words: &[B256]) -> AbiPart {
    let mut tail = Vec::with_capacity(32 * (words.len() + 1));
    tail.extend_from_slice(&U256::from(words.len()).to_be_bytes::<32>());
    for w in words {
        tail.extend_from_slice(w.as_slice());
    }
    AbiPart::Dynamic(tail)
}

pub(crate) fn encode_tuple(parts: Vec<AbiPart>) -> Vec<u8> {
    let tail_start = 32 * parts.len();
    let mut head = Vec::with_capacity(tail_start);
    let mut tail = Vec::new();
    for part in parts {
        match part {
            AbiPart::Word(w) => head.extend_from_slice(w.as_slice()),
            AbiPart::Dynamic(t) => {
                head.extend_from_slice(&U256::from(tail_start + tail.len()).to_be_bytes::<32>());
                tail.extend(t);
            }
        }
    }
    head.extend(tail);
    head
}

/// `wordU64`: an offset or length word, rejecting dirty high bits.
fn word_u64(args: &[u8], pos: u64) -> Option<u64> {
    let len = args.len() as u64;
    if pos > len || len - pos < 32 {
        return None;
    }
    let pos = pos as usize;
    if args[pos..pos + 24].iter().any(|b| *b != 0) {
        return None;
    }
    Some(u64::from_be_bytes(
        args[pos + 24..pos + 32].try_into().unwrap(),
    ))
}

/// `readStringArg`, also serving `readBytesArg`: the raw bytes of a dynamic argument.
pub(crate) fn read_string_arg(args: &[u8], arg_index: usize) -> Cas20Result<Vec<u8>> {
    let l = args.len() as u64;
    let off = word_u64(args, arg_index as u64 * 32).ok_or(REVERTED)?;
    if off > l || l - off < 32 {
        return Err(REVERTED);
    }
    let n = word_u64(args, off).ok_or(REVERTED)?;
    let data_pos = off + 32;
    if n > l - data_pos {
        return Err(REVERTED);
    }
    Ok(args[data_pos as usize..(data_pos + n) as usize].to_vec())
}

/// `readBytesArray`.
pub(crate) fn read_bytes_array(args: &[u8], arg_index: usize) -> Cas20Result<Vec<Vec<u8>>> {
    let l = args.len() as u64;
    let base = word_u64(args, arg_index as u64 * 32).ok_or(REVERTED)?;
    if base > l || l - base < 32 {
        return Err(REVERTED);
    }
    let n = word_u64(args, base).ok_or(REVERTED)?;
    let arr_data = base + 32;
    if n > (l - arr_data) / 32 {
        return Err(REVERTED);
    }
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let elem_off = word_u64(args, arr_data + i * 32).ok_or(REVERTED)?;
        // Ordered as go-bsc evaluates it: `elemOff > L-arrData` guards the addition below.
        if elem_off > l - arr_data {
            return Err(REVERTED);
        }
        let pos = arr_data + elem_off;
        if pos > l || l - pos < 32 {
            return Err(REVERTED);
        }
        let elem_len = word_u64(args, pos).ok_or(REVERTED)?;
        let start = pos + 32;
        if elem_len > l - start {
            return Err(REVERTED);
        }
        out.push(args[start as usize..(start + elem_len) as usize].to_vec());
    }
    Ok(out)
}

/// `readWordArray`.
pub(crate) fn read_word_array(args: &[u8], arg_index: usize) -> Cas20Result<Vec<B256>> {
    let l = args.len() as u64;
    let base = word_u64(args, arg_index as u64 * 32).ok_or(REVERTED)?;
    if base > l || l - base < 32 {
        return Err(REVERTED);
    }
    let n = word_u64(args, base).ok_or(REVERTED)?;
    let data_pos = base + 32;
    if n > (l - data_pos) / 32 {
        return Err(REVERTED);
    }
    Ok((0..n)
        .map(|i| {
            let p = (data_pos + i * 32) as usize;
            B256::from_slice(&args[p..p + 32])
        })
        .collect())
}

/// `readUint8Array`: the single `uint8[]` argument of `pause`/`unpause`.
pub(crate) fn read_u8_array(args: &[u8]) -> Cas20Result<Vec<u8>> {
    let l = args.len() as u64;
    let off = word_u64(args, 0).ok_or(REVERTED)?;
    if off > l || l - off < 32 {
        return Err(REVERTED);
    }
    let n = word_u64(args, off).ok_or(REVERTED)?;
    let data_pos = off + 32;
    if n > (l - data_pos) / 32 {
        return Err(REVERTED);
    }
    (0..n)
        .map(|i| {
            // Byte-addressed: the caller-supplied head offset need not be 32-aligned.
            match word_u64(args, data_pos + i * 32) {
                Some(v) if v <= 0xff => Ok(v as u8),
                _ => Err(REVERTED),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_encode_like_solidity() {
        let enc = enc_string(b"abc");
        assert_eq!(enc.len(), 96);
        assert_eq!(enc[31], 0x20);
        assert_eq!(enc[63], 3);
        assert_eq!(&enc[64..67], b"abc");
    }

    #[test]
    fn dirty_high_bytes_are_malformed() {
        let mut word = [0u8; 32];
        word[11] = 1; // above the 20 address bytes
        assert_eq!(read_address(&word, 0), Err(REVERTED));
        word[11] = 0;
        word[12] = 1;
        assert!(read_address(&word, 0).is_ok());
    }

    #[test]
    fn string_arg_round_trips() {
        let mut args = encode_tuple(vec![abi_string("hello")]);
        assert_eq!(read_string_arg(&args, 0).unwrap(), b"hello");
        args.truncate(args.len() - 32);
        assert_eq!(read_string_arg(&args, 0), Err(REVERTED));
    }
}
