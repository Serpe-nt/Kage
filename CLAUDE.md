# CLAUDE.md — house rules for agents working on Kage

Kage is a self-hosted Rust API service that fetches web pages while being
indistinguishable from a real human user: static residential proxies, one
long-lived persona per IP, real browsers (patched Chromium and Camoufox) in
an anti-detect sidecar, and a wreq-based HTTP path for lenient sites.

## Read first, in this order

1. `docs/00-decisions.md` — settled decisions. Do not re-litigate them. If
   one must change, write an ADR in `docs/adr/` and stop for owner review.
2. `docs/01-spec.md` — the contract. Section numbers are referenced
   everywhere.
3. `docs/02-plan.md` — milestones, acceptance criteria, definition of done,
   and the session brief template.
4. `docs/versions.md` — exact pins. Update it whenever you add or bump a
   dependency.
5. `docs/adr/` — why things are the way they are.

The research document `bot-detection-landscape.md` is not committed; ask the
owner if you need it.

## Non-negotiables

- Exact version pins for every dependency; committed lockfiles. Never bump a
  fingerprint-critical package (see `versions.md`) outside a dedicated PR.
- No `openssl-sys` anywhere in the Rust dependency graph. `just deny` checks
  it. Every TLS-capable crate uses rustls; all outbound HTTP in Rust goes
  through wreq.
- Never add a direct (non-proxy) connection path to targets.
- Never add external CAPTCHA solving or credentialed scraping.
- Never run live tests (`KAGE_LIVE=1`) against real target sites. Live tests
  only hit the echo and detection test pages listed in configuration.
- Never commit secrets, real proxy credentials, API keys, or unsanitized
  fixture pages containing personal data.
- Tests are part of every change at the layer `docs/01-spec.md` §21 names.
  New Rust modules in core crates must keep coverage ≥ 80 %.
- No `unwrap`/`expect`/`panic!` in library crates outside tests; use typed
  errors (`thiserror`) in libraries and `anyhow` only in the binary.
- Public types and functions carry doc comments. OpenAPI documents are
  regenerated and committed whenever API or contract types change.
- Conventional commits. One milestone, one PR. Fill the PR with evidence for
  each acceptance criterion.

## Commands (all through `just`; M0 creates them)

```
just setup        install toolchains, pnpm deps, git hooks
just fmt          rustfmt + prettier
just lint         clippy -D warnings, eslint, tsc
just test         rust unit + integration (testcontainers), sidecar vitest
just test-e2e     compose-based end-to-end suite
just test-live    gated live suite, needs KAGE_LIVE=1 and secrets
just cov          coverage reports with thresholds
just deny         cargo-deny, cargo-audit, pnpm audit, openssl-sys check
just sbom         CycloneDX SBOMs
just build-images docker images for kage and both sidecar variants
just dev          postgres + sidecars in compose, kage serve natively
just migrate      apply migrations
just openapi      regenerate public and sidecar OpenAPI files
just ci           everything a PR runs
```

## Layout

- `crates/` Rust workspace (`kage` binary, `kage-core`, `kage-store`,
  `kage-proxy`, `kage-http`, `kage-browser`, `kage-persona`,
  `kage-scheduler`, `kage-crawl`, `kage-extract`, `kage-api`).
- `sidecar/` pnpm workspace (`contract`, `engine-core`, `engine-chromium`,
  `engine-firefox`, `server`). `engine-chromium` and `engine-firefox` pin
  different playwright versions on purpose; never hoist them.
- `policies/` YAML that is data, hot-reloaded: domain policies, block
  signatures, pacing profiles.
- `deploy/` Dockerfiles and compose. `docs/` spec, plan, ADRs, runbooks.
- `tests/` fixtures (sanitized recorded pages, fixture site), e2e, live,
  and `api-client`: the public API's document validated and a TypeScript
  client generated from it, a pnpm project of its own so that it never
  moves the sidecar's lockfile.

## Conventions

- Rust: edition 2024, stable toolchain from `rust-toolchain.toml`, workspace
  lints, `#![forbid(unsafe_code)]` in libraries, `tracing` for logs with
  structured fields (never format secrets into messages), UUIDv7 ids, UTC
  timestamps, `chrono`.
- TypeScript: `strict`, ESM, zod schemas are the source of truth for the
  sidecar contract, pino logs with the same field names as Rust.
- Randomness that affects behavior or fingerprints is seeded and the seed is
  recorded, so tests and incident analysis can reproduce it.
- Config keys use `snake_case`; env overrides use `KAGE__SECTION__KEY`.
- Error codes in the API are stable strings documented in OpenAPI.

## When something in the spec is wrong or impossible

Do the parts that are unaffected, write the discrepancy into the PR
description and, if it changes a decision, an ADR draft. Do not silently
deviate.
