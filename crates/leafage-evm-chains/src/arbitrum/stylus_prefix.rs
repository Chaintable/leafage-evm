//! ArbOS-aware Stylus bytecode prefix checks.
//!
//! Nitro distinguishes deployable programs (which may execute) from Stylus
//! components (which may be returned by CREATE). From ArbOS 60 a fragment is a
//! valid component, but it is never a directly executable program.

pub(crate) const ARBOS_VERSION_STYLUS: u64 = 30;
pub(crate) const ARBOS_VERSION_STYLUS_CONTRACT_LIMIT: u64 = 60;

pub(crate) const STYLUS_CLASSIC_PREFIX: &[u8] = &[0xef, 0xf0, 0x00];
pub(crate) const STYLUS_FRAGMENT_PREFIX: &[u8] = &[0xef, 0xf0, 0x01];
pub(crate) const STYLUS_ROOT_PREFIX: &[u8] = &[0xef, 0xf0, 0x02];

pub(crate) fn has_stylus_prefix(code: &[u8], prefix: &[u8]) -> bool {
    code.len() > prefix.len() && code.starts_with(prefix)
}

pub(crate) fn is_stylus_classic(code: &[u8]) -> bool {
    has_stylus_prefix(code, STYLUS_CLASSIC_PREFIX)
}

pub(crate) fn is_stylus_fragment(code: &[u8]) -> bool {
    has_stylus_prefix(code, STYLUS_FRAGMENT_PREFIX)
}

pub(crate) fn is_stylus_root(code: &[u8]) -> bool {
    has_stylus_prefix(code, STYLUS_ROOT_PREFIX)
}

/// Nitro `IsStylusDeployableProgramPrefix`.
pub(crate) fn is_stylus_deployable(code: &[u8], arbos_version: u64) -> bool {
    if arbos_version < ARBOS_VERSION_STYLUS {
        return false;
    }
    if arbos_version < ARBOS_VERSION_STYLUS_CONTRACT_LIMIT {
        return is_stylus_classic(code);
    }
    is_stylus_classic(code) || is_stylus_root(code)
}

/// Nitro `IsStylusComponentPrefix`, used only by CREATE's EIP-3541 exception.
pub(crate) fn is_stylus_component(code: &[u8], arbos_version: u64) -> bool {
    is_stylus_deployable(code, arbos_version)
        || (arbos_version >= ARBOS_VERSION_STYLUS_CONTRACT_LIMIT && is_stylus_fragment(code))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASSIC: &[u8] = &[0xef, 0xf0, 0x00, 0x01];
    const FRAGMENT: &[u8] = &[0xef, 0xf0, 0x01, 0x01];
    const ROOT: &[u8] = &[0xef, 0xf0, 0x02, 0x01];

    #[test]
    fn prefix_matrix_matches_nitro_arbos_versions() {
        for version in [0, 29] {
            assert!(!is_stylus_deployable(CLASSIC, version));
            assert!(!is_stylus_component(CLASSIC, version));
        }

        for version in [30, 59] {
            assert!(is_stylus_deployable(CLASSIC, version));
            assert!(is_stylus_component(CLASSIC, version));
            assert!(!is_stylus_deployable(ROOT, version));
            assert!(!is_stylus_component(ROOT, version));
            assert!(!is_stylus_deployable(FRAGMENT, version));
            assert!(!is_stylus_component(FRAGMENT, version));
        }

        for version in [60, 61] {
            assert!(is_stylus_deployable(CLASSIC, version));
            assert!(is_stylus_deployable(ROOT, version));
            assert!(!is_stylus_deployable(FRAGMENT, version));
            assert!(is_stylus_component(CLASSIC, version));
            assert!(is_stylus_component(ROOT, version));
            assert!(is_stylus_component(FRAGMENT, version));
        }
    }

    #[test]
    fn prefix_requires_a_body_byte() {
        assert!(!is_stylus_classic(STYLUS_CLASSIC_PREFIX));
        assert!(!is_stylus_fragment(STYLUS_FRAGMENT_PREFIX));
        assert!(!is_stylus_root(STYLUS_ROOT_PREFIX));
    }
}
