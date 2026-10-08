# Changelog

All notable changes to Kage are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- M1, core domain, configuration and store:
  - `kage-core`: the domain model of spec §5 (proxies, personas, sessions,
    requests, attempts, results, artifacts, jobs, keys, events) with typed
    UUIDv7 ids, `Outcome` and the block report.
  - Domain policies (`policies/domains.yaml`): each field is inherited from
    the exact host, then its parent domains from nearest to furthest, then
    `defaults`; per-outcome cooldowns plus a `short` cooldown; pacing
    resolution request, then domain, then default, with `pacing_locked`.
  - Pacing profiles (`policies/pacing-profiles.yaml`) with seeded, replayable
    samplers: log-normal by median and p95 (shifted, so a `min` leaves neither
    a spike nor an edge at the floor), uniform, and bursts with breaks.
  - Block classifier: signatures as data (`policies/block-signatures.yaml`)
    with matchers on status, headers, cookies, body, URL and browser signals,
    covering Cloudflare, Akamai, DataDome, HUMAN, Kasada, Imperva, AWS WAF,
    F5, reCAPTCHA, hCaptcha, generic statuses and soft blocks. Each rule says
    whether a match is retried, escalated to the browser and how long the
    persona cools down; every verdict lists the markers that fired. A
    vendor's cookie on a good page is a hint, never a block. 44 fixtures
    under `tests/fixtures/blocks/`, all synthetic and marked as such.
  - SSRF guard as pure functions: denied address ranges, name rules, literal
    IP targets behind `security.allow_ip_targets`, and a redirect chain that
    re-checks every hop, records `https` to `http` downgrades and names the
    hop it refuses.
  - URL normalization into a dedupe and cache key (spec §13), including
    RFC 3986 percent-encoding normalization. Requests keep the URL as
    discovered.
  - Configuration: `kage.yaml` layered with `KAGE__SECTION__KEY`, secrets
    only from the environment or `_FILE` variables, and `kage config check`,
    which prints the effective configuration with secrets redacted.
    `deploy/kage.example.yaml` lists every key.
  - Policy hot reload: a changed file is validated and swapped in; an invalid
    one keeps the last good policy and raises an alert, stored as a health
    event.
  - `kage-store`: migrations for the 18 tables of spec §18, repositories with
    compile-time checked queries and committed offline data (`.sqlx/`), and
    the content-addressed artifact store with gzip for text and a retention
    job.
  - `kage migrate` (and `--check`); `kage serve` now requires a reachable,
    migrated database and valid policies. The compose stack runs the
    migrations in a one-shot `migrate` service.
  - `just sqlx-check`, `just sqlx-prepare`, `just config-check`; the coverage
    gate now includes `kage-store`.

- M0, repository scaffold and CI gates:
  - Cargo workspace (edition 2024, Rust 1.99.0, workspace lints) with the
    `kage` binary and eight library crates. `kage serve` exposes `/healthz`;
    `kage healthcheck` probes it for the container healthcheck.
  - Sidecar pnpm workspace (TypeScript strict, ESM) with the `contract`,
    `engine-core`, `engine-chromium`, `engine-firefox` and `server` packages.
    The server exposes `/v1/health`; the `ENGINE` variable selects the engine
    package; both engines are stubs.
  - `justfile` with every dev and CI command; `just ci` runs everything a
    pull request runs.
  - Dockerfiles for `kage` and both sidecar variants, and a compose file with
    Postgres 17, healthchecks and hardened containers.
  - GitHub Actions: `ci.yaml`, `nightly-live.yaml` (skeleton) and
    `release.yaml`.
  - Supply-chain gates: cargo-deny, cargo-audit, pnpm audit, a license policy
    for the sidecar, gitleaks, Trivy, CycloneDX SBOMs, and a check that fails
    when `openssl-sys` is in the Rust dependency graph.
  - Coverage gate: at least 80 % line coverage on `kage-core`, `kage-proxy`,
    `kage-crawl`, `kage-extract` and `engine-core`.
  - README with a responsible-use section, dual MIT / Apache-2.0 license
    files, CODEOWNERS and the repository runbook.
  - Exact dependency pins re-verified and recorded in `docs/versions.md`.
  - ADR 0008 (proposed): AGPL-licensed `ua-parser-js` in the Firefox engine
    dependency tree.

### Changed

- `kage serve` takes its listen address from `server.bind`
  (`KAGE__SERVER__BIND`) or `--bind`, and fails at startup when a required
  secret is missing. `deploy/.env` gained `KAGE_SIDECAR_TOKEN` and
  `KAGE_MASTER_KEY`; `just setup` adds them to an existing file.
