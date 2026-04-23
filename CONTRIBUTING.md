# Contributing

Thanks for your interest in contributing to `leafage-evm`.

This project provides a lightweight EVM executor across multiple chains. Contributions should prioritize correctness, determinism, and performance.

---

## Getting Started

### Requirements

* Rust (stable)
* cargo
* rustfmt
* clippy

Install toolchain:

```bash
rustup default stable
rustup component add rustfmt clippy
```

---

## Development Workflow

1. Fork the repository
2. Create a branch from `main`
3. Make changes
4. Run local checks
5. Open a PR

Keep PRs small and focused.

---

## Local Checks (must pass)

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --all
```

---

## Code Guidelines

### General

* Prefer simple and explicit logic
* Avoid unnecessary abstractions
* Keep dependencies minimal
* Favor readability over cleverness

---

### EVM Correctness (Critical)

Changes affecting execution must ensure:

* Deterministic execution
* Correct gas accounting
* Correct opcode semantics
* Proper error / revert handling
* Cross-chain compatibility (if applicable)

---

### Performance

* Avoid unnecessary allocations
* Minimize copies in hot paths
* Be cautious with async / concurrency overhead
* Benchmark critical paths when relevant

---

### Errors

* Use explicit error types
* Avoid panics in library code
* Preserve context when propagating errors

---

## Testing

All changes must include tests.

### Run tests

```bash
cargo test --all
```

---

### What to cover

* Opcode behavior
* Edge cases (overflow, underflow, invalid input)
* Revert / failure paths
* Gas correctness
* Chain-specific differences (if applicable)

---

### Integration tests

If a test depends on external components:

* Place it under `tests/`
* Gate with feature flag or conditional execution

---

## Formatting & Lint

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
```

---

## Pull Requests

Before submitting:

* CI must pass
* Tests added or updated
* Behavior changes clearly explained

PRs should include:

* Summary
* Motivation
* Testing details
* Compatibility impact

---

## Compatibility Policy

* Public APIs should remain stable
* Breaking changes must be clearly documented
* Avoid unnecessary API surface expansion

---

## Commit Guidelines

* Use clear, descriptive messages

Example:

```
evm: fix gas accounting for SSTORE under EIP-2200
```

---

## Reporting Issues

Please include:

* Rust version
* OS / environment
* Reproduction steps
* Expected vs actual behavior

---

## Security

Do not disclose vulnerabilities publicly.

See `SECURITY.md` for reporting instructions.

---

## License

By contributing, you agree that your contributions are licensed under the project's license.

