# Pinned versions and verification log

Policy: every dependency is pinned to an exact version in `Cargo.toml`
(`=x.y.z`) and `package.json` (no caret, no tilde), lockfiles are committed,
and bumps are deliberate PRs that re-run the fingerprint self-test suite.
Renovate or Dependabot may open bump PRs but must not auto-merge anything in
the "fingerprint-critical" set.

Verified on 2026-10-08 against crates.io, npm and GitHub. Agents starting M0
must re-verify and update the "verified" column; a newer patch release is
acceptable, a newer minor or major requires a note in the PR.

## Fingerprint-critical (bump together, re-test, never auto-merge)

| Package | Ecosystem | Version | Notes |
|---|---|---|---|
| wreq | crates.io | 0.16.1 (2026-08-27) | The `6.0.0-rc.*` line is abandoned; the project re-versioned to stable 0.16.x on 2026-08-22. BoringSSL-linked. No HTTP/3. |
| wreq-util | crates.io | 0.2.0 (2026-08-22) | Emulation profiles. Must contain a profile whose browser major matches each engine's bundled browser, else the http path is disabled for that persona. |
| patchright | npm | 1.63.0 | Pins `patchright-core 1.63.0`. Chromium engine. Use `channel: "chrome"` and headful. |
| camoufox-js | npm | 0.12.1 | Experimental JS port of the Python launcher. Peer `playwright-core < 1.63.0`. Depends on `fingerprint-generator ^2.1.86`. Firefox engine. |
| playwright-core (firefox engine package only) | npm | 1.62.x, the highest below 1.63.0 | Required by camoufox-js. Lives only in `sidecar/packages/engine-firefox`. |
| Camoufox browser binary | GitHub daijro/camoufox | v152.0.4-beta.27 | Fetched by camoufox-js at build time. Maintainer: "may not be suitable for stable production use". See risk R1 in the spec. |
| fingerprint-generator | npm | 2.1.88 | Apify fingerprint-suite. Chromium persona generation. |
| fingerprint-injector | npm | 2.1.88 | Same suite, same version as the generator. |
| Chrome (stable channel, in the Chromium engine image) | Google | pinned by image digest at M4 | Playwright's `channel: "chrome"`. The image build records the Chrome version in `/etc/kage/chrome-version`. |

## Rust core

| Crate | Version | Notes |
|---|---|---|
| tokio | 1.x latest at M0 | `full` feature set in the binary, minimal in libs. |
| axum | 0.8.9 (2026-04-14) | |
| tower-http | latest compatible with axum 0.8 | timeout, limit, request-id, trace, cors, compression. |
| utoipa | 6.0.0 (2026-09-22) | New major; M0 verifies `utoipa-axum` and `utoipa-swagger-ui` compatibility and pins all three together. |
| sqlx | 0.9.0 (2026-05-21) | `postgres`, `runtime-tokio`, `tls-rustls`, `chrono`, `uuid`, `json`, `migrate`. Never `tls-native-tls` (OpenSSL conflict with BoringSSL). |
| serde, serde_json | latest at M0 | |
| tracing, tracing-subscriber, tracing-opentelemetry, opentelemetry-otlp | latest at M0 | OTLP over gRPC or HTTP; the OTLP crate must use rustls, never OpenSSL. |
| metrics, metrics-exporter-prometheus | latest at M0 | |
| figment | latest at M0 | YAML + env layering. |
| clap | 4.x | |
| thiserror, anyhow | latest at M0 | `anyhow` only in the binary crate. |
| governor | latest at M0 | Token-bucket limits per key and per domain. |
| notify | latest at M0 | Policy hot reload. |
| url, ipnet, hickory-resolver | latest at M0 | SSRF guard needs rustls DNS-over-TCP/UDP only. |
| sha2, hmac, rand, secrecy, uuid | latest at M0 | |
| scraper, html5ever | latest at M0 | Extraction. |
| robotstxt | latest at M0 | Google robots.txt matcher port. |
| quick-xml | latest at M0 | Sitemaps. |
| chrono | 0.4.x | |
| wiremock | latest at M0 | Test only. |

Hard rule: `cargo tree -i openssl-sys` must be empty. CI enforces it.

## Node sidecar

| Package | Version | Notes |
|---|---|---|
| Node.js | current Active LTS at M0 (24.x as of writing) | `.nvmrc` and `engines` pin the major. |
| pnpm | latest 10.x | Workspace with per-package isolation (needed for the two playwright-core versions). |
| TypeScript | 5.x latest | `strict: true`. |
| fastify | 5.x latest | Internal HTTP server. |
| zod | 3.x or 4.x latest | Contract schemas; OpenAPI emitted from them. |
| pino, prom-client, @opentelemetry/sdk-node | latest at M0 | |
| vitest, eslint, prettier | latest at M0 | |

## Toolchain

| Tool | Version |
|---|---|
| Rust | stable, pinned in `rust-toolchain.toml` at M0; edition 2024 |
| cargo-deny, cargo-audit, cargo-llvm-cov, cargo-mutants, cargo-cyclonedx | latest at M0 |
| Docker Engine / Compose | 27+ / v2 |
| PostgreSQL | 17 |
| gitleaks, trivy, syft | latest at M0 |

## Verification log

| Date | Who | What changed |
|---|---|---|
| 2026-10-08 | spec author | Initial pins from registry lookups. |
