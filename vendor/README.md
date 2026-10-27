# Stable dependency baseline

Pinned source snapshots of the crates used by the upstream lockfile. The source
under each `src/` directory is unchanged from its crates.io release. Stable
feature branches can maintain small, reviewable patches against this baseline
while reusing the existing execution machinery.

| Package | Version |
| --- | --- |
| revm-context | 43.0.3 |
| revm-context-interface | 43.0.2 |
| revm-handler | 43.0.3 |
| alloy-evm | 0.39.0 |
| revm-inspectors | 0.44.0 |

The normalized manifests have a local `[workspace]` to keep vendored packages
outside Leafage's workspace members. Only source, manifests, README and licenses
are retained; the omitted inspector benchmark target is removed from its manifest.
Docker copies these packages before cargo-chef prepares and cooks its recipe.
