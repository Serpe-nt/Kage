# Changelog

All notable changes to Kage are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- M2, proxy provider, pool, health and replacement:
  - `kage-proxy`: the `ProxyProvider` trait with two implementations. The
    Webshare client (on wreq, no second TLS stack) reads the proxy list page
    by page, requests and follows replacements, reads the log of replaced
    proxies, the account (plan, replacement quota, automatic-replacement
    settings, stock per country) and hourly traffic. It keeps its own request
    rate below Webshare's limits, waits out a 429, and never sends a request
    that changes something twice. The static list provider serves a fixed
    list from `KAGE__PROXIES__STATIC_LIST`, for tests and other vendors.
  - The pool: stored proxies are kept in step with the provider every
    `proxies.sync_interval`. A proxy missing from the list is retired at once
    only when the provider's own log confirms it, otherwise after three
    syncs, and never on the word of an empty or far too short list.
  - The pool state machine of spec §7.3 as pure functions with property
    tests: three failed checks send a proxy into backoff (1 min doubling to
    1 h, jittered), one probe follows every backoff, and a proxy is replaced
    only after three backoff cycles and six hours of failing without a break
    (`proxies.replace_after`), within `proxies.replacements_per_month`. The
    budget is enforced in Postgres, so the server and the CLI cannot overspend
    it together. Replacements ask for the same country.
  - Health checks, each proxy on its own jittered schedule: reachability,
    latency and exit address through the provider's own echo (a second echo
    is tried once before a failure counts), a leak check against the host's
    own address, a geo lookup of the exit address on the first check, on an
    address change and once a day, and optional IP reputation lookups
    (AbuseIPDB, off without a key). When more than half of the pool fails at
    once the failure counters freeze: the cause is on the path.
  - A proxy that lets the host's own address through is parked and sets the
    pool-wide `egress_unsafe` flag, which later milestones must honor before
    dispatching anything. It clears when a probe through that proxy shows a
    distinct exit address, or with `kage proxy egress --clear`.
  - Proxy user names and passwords are encrypted at rest with
    XChaCha20-Poly1305 under `KAGE__SECRETS__MASTER_KEY`, each value bound to
    its proxy and its field.
  - `kage proxy list`, `kage proxy health`, `kage proxy replace` and
    `kage proxy egress`, as tables or with `--json`; they may run next to the
    server. `kage serve` syncs the pool and runs the health checks when a
    provider is configured; proxies stored from an earlier provider are
    still health-checked when none is.
  - New operator alerts, stored as health events: proxy leak, unsafe egress,
    geo mismatch, bad reputation, suspicious usage type, replacement budget
    exhausted, replacement failed, pool outage suspected, pool capacity low,
    suspect provider list, provider replaced a proxy, provider
    auto-replacement enabled, provider account, provider sync failed, host
    address unknown.
  - Configuration: new `proxies` section, more keys under `webshare` and
    `reputation`, the secret `KAGE__PROXIES__STATIC_LIST`. The provider is
    chosen by `proxies.provider` (`auto` by default: Webshare when its key is
    set, else the static list, else none).
  - `docs/runbooks/proxies.md`; a gated live check of the real proxy list
    (`just test-live`, `tests/live/proxy-health.sh`).
  - The compose stack and `just dev` pass `KAGE_WEBSHARE_API_KEY` and
    `KAGE_ABUSEIPDB_KEY` from `deploy/.env` on when they are set. The smoke
    test never uses them.

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

- `KAGE__SECRETS__MASTER_KEY` must be 32 random bytes written as 64
  hexadecimal characters; any other value now fails at startup. `just setup`
  has always generated it in this form.
- The `proxies.username` column holds ciphertext (migration 0008). The
  migration adds the columns the pool state machine needs and lets a retired
  proxy share its provider id with its successor.
- `kage config check` also validates the proxy pool settings (the probe's
  emulation profile must exist in this build) and reports which proxy
  provider is in force.
- A secret variable that is set but empty no longer clashes with its `_FILE`
  form: the file is used.
- `kage serve` takes its listen address from `server.bind`
  (`KAGE__SERVER__BIND`) or `--bind`, and fails at startup when a required
  secret is missing. `deploy/.env` gained `KAGE_SIDECAR_TOKEN` and
  `KAGE_MASTER_KEY`; `just setup` adds them to an existing file.
