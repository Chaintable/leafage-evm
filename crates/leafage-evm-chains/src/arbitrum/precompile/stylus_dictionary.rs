/// ArbOS 11 Stylus program dictionary bytes used for Brotli-compressed Stylus
/// contract payloads.
///
/// This is vendored as a binary asset because the bytes must match Nitro
/// exactly. Do not regenerate it at build time.
///
/// Source: Nitro `crates/brotli/src/dicts/stylus-program-11.lz` at
/// `fabfd479919d8df2aef4a9a5e95a2fba50ae7b02`.
pub(super) const PROGRAM_DICTIONARY_BYTES: &[u8] = include_bytes!("assets/stylus-program-11.lz");
pub(super) const PROGRAM_DICTIONARY_ID: u8 = 1;

pub(super) fn program_dictionary_owned() -> Vec<u8> {
    PROGRAM_DICTIONARY_BYTES.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    const PROGRAM_DICTIONARY_SHA256: &str =
        "9681d04f40f0960dbc44f57fdd523ad5b829b43b9f65878fb9e80f964e273672";

    #[test]
    fn program_dictionary_bytes_match_expected_asset() {
        assert_eq!(PROGRAM_DICTIONARY_ID, 1);
        assert_eq!(PROGRAM_DICTIONARY_BYTES.len(), 112_640);
        assert_eq!(
            format!("{:x}", Sha256::digest(PROGRAM_DICTIONARY_BYTES)),
            PROGRAM_DICTIONARY_SHA256
        );
    }
}
