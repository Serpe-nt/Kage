# ADR 0007: wreq on its stable 0.16 line, exact pins for fingerprint-critical dependencies, no OpenSSL anywhere

Status: accepted, 2026-10-08

## Context

The original brief assumed wreq was on a 6.0.0 release-candidate line. On
2026-08-22 the project re-versioned to stable 0.16.x and wreq-util to 0.2.x;
the rc line is abandoned. wreq links BoringSSL, which cannot coexist with
`openssl-sys` in one binary. Any dependency bump can alter the emulated
TLS/HTTP/2 fingerprint.

## Decision

Pin `wreq = "=0.16.1"` and `wreq-util = "=0.2.0"` and every other
fingerprint-critical package listed in `docs/versions.md` exactly. Bumps are
dedicated PRs that re-run the live fingerprint self-test suite and are never
auto-merged. CI fails if `cargo tree -i openssl-sys` is non-empty; every
TLS-capable crate uses rustls.

## Consequences

- Engine and client upgrades are scheduled work, not background noise.
- The Webshare API client and every outbound HTTP call in Rust go through
  wreq.
