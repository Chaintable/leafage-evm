# Security Policy

## Supported Versions

We provide security updates for the latest `main` branch and recent releases.

| Version | Supported |
|---------|----------|
| main    | ✅       |
| <latest release> | ✅ |
| older versions   | ❌ |

---

## Reporting a Vulnerability

If you discover a security issue, **do not open a public issue**.

Please report it privately:

- Email: bugbounty@debank.com
- Or GitHub Security Advisory (preferred)

Include:

- Description of the issue
- Impact / severity assessment
- Steps to reproduce
- Proof of concept (if available)

---

## Response Process

We aim to:

- Acknowledge within **72 hours**
- Provide initial assessment within **3–5 days**
- Fix and release as soon as possible depending on severity

---

## Disclosure Policy

- We follow **responsible disclosure**
- Fixes may be developed privately before public release
- Credit will be given unless you request anonymity

---

## Scope

Typical security-relevant areas include:

- Data integrity (block ordering, duplication, corruption)
- Resource exhaustion (memory / goroutine leaks)
- Denial of service vectors
- Incorrect error handling leading to inconsistent state
- Unsafe concurrency patterns

---

## Notes
This project is a EVM executor built with alloy and revm. It focuses on **state queries**.
Security issues may propagate to downstream systems — please report anything suspicious.
