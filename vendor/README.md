# Stable dependency patches

These are source snapshots of the exact crates already in the baseline lockfile,
reused through `[patch.crates-io]`. Keep their package versions and licenses;
review the small changes against the corresponding crates.io release when
upgrading. Only `src`, the normalized package manifest, README and licenses are
vendored. A local `[workspace]` keeps them outside Leafage's workspace members.

| Package | Version | Local change |
| --- | --- | --- |
| revm-context-interface | 43.0.2 | Block::basefee returns u128; gas cost products use U256 |
| revm-context | 43.0.3 | BlockEnv.basefee is u128 |
| revm-handler | 43.0.3 | U256 refunds/rewards; propagate payment overflow instead of panicking |
| alloy-evm | 0.39.0 | Lossless u64-to-u128 conversions at native Alloy header boundaries |
| revm-inspectors | 0.44.0 | Checked conversion for unused optional Alloy trace metadata |

The interpreter and its BASEFEE/GASPRICE instructions are unchanged. The existing
execution machinery consumes the widened environment, avoiding a second fee
value or Stable-specific opcode/handler implementation.

Alloy's consensus `BlockHeader` trait still returns a u64 fee, so Leafage's local
RawHeader intentionally does not implement it. Its only former execution use
needed the parent hash; that internal interface now accepts the hash directly.
