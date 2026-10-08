# Pinned versions and verification log

Policy: every dependency is pinned to an exact version in `Cargo.toml`
(`=x.y.z`) and `package.json` (no caret, no tilde), lockfiles are committed,
and bumps are deliberate PRs that re-run the fingerprint self-test suite.
Renovate or Dependabot may open bump PRs but must not auto-merge anything in
the "fingerprint-critical" set.

Re-verified on 2026-10-08 during M0 against crates.io, npm, nodejs.org, the
container registries and GitHub releases. A newer patch release is
acceptable when a pin is bumped; a newer minor or major requires a note in
the PR.

The **State** column says how far a pin has got:

- **locked**: in a committed lockfile, compiled or installed in CI, covered
  by the gates.
- **pinned**: exact version declared in `[workspace.dependencies]`; it enters
  `Cargo.lock` when a crate first uses it.
- **planned**: verified on the registry, not declared anywhere yet.

## Fingerprint-critical (bump together, re-test, never auto-merge)

| Package | Ecosystem | Version | State | Notes |
|---|---|---|---|---|
| wreq | crates.io | 0.16.1 (2026-08-27) | locked (`kage-http`) | Newest stable. The `6.0.0-rc.*` line is abandoned (last: rc.31, 2026-08-18); the project re-versioned to stable 0.16.x on 2026-08-22. Links BoringSSL through `btls` 0.5.6. No HTTP/3. Pulls the pre-release `wreq-rt` 0.2.2-rc.4 transitively. MSRV 1.98. |
| wreq-util | crates.io | 0.2.0 (2026-08-22) | locked (`kage-http`) | Emulation profiles. Must contain a profile whose browser major matches each engine's bundled browser, else the http path is disabled for that persona. |
| patchright | npm | 1.63.0 | locked (`engine-chromium`) | Newest. Pins `patchright-core 1.63.0`. Chromium engine. Use `channel: "chrome"` and headful. |
| camoufox-js | npm | 0.12.1 | locked (`engine-firefox`) | Newest. Experimental JS port of the Python launcher. Peer `playwright-core < 1.63.0`. Depends on `fingerprint-generator ^2.1.86` (resolved 2.1.88), `ua-parser-js ^2.0.10` (AGPL-3.0-or-later, see ADR 0008) and the native addon `better-sqlite3` 13.0.3, whose install script is not run until M5. |
| playwright-core (firefox engine package only) | npm | 1.62.1 | locked (`engine-firefox`) | The highest below 1.63.0, as camoufox-js requires. npm latest is 1.64.0. Lives only in `sidecar/packages/engine-firefox`; a test fails if another package can resolve it. |
| Camoufox browser binary | GitHub daijro/camoufox | v152.0.4-beta.27 (2026-07-16) | planned (M5) | **Pin under review, see below.** Fetched by camoufox-js at build time. Maintainer: "may not be suitable for stable production use". See risk R1 in the spec. |
| fingerprint-generator | npm | 2.1.88 | locked (transitive, through camoufox-js) | Apify fingerprint-suite. Becomes a direct dependency for Chromium persona generation in M4. |
| fingerprint-injector | npm | 2.1.88 | planned (M4) | Same suite, same version as the generator. |
| Chrome (stable channel, in the Chromium engine image) | Google | pinned by image digest at M4 | planned (M4) | Playwright's `channel: "chrome"`. The image build records the Chrome version in `/etc/kage/chrome-version`. |

### Camoufox browser binary: the pin does not fit the other two

`camoufox-js` 0.12.1 only accepts browser builds in a fixed range
(`dist/__version__.js`): below `beta.32`, and, when the installed
`playwright-core` is 1.61 or newer, at least `beta.30`. With the pinned
`playwright-core` 1.62.1 that leaves exactly one published build,
**v152.0.4-beta.30** (2026-09-01). The pinned `v152.0.4-beta.27` would be
rejected at launch. The newer `v156.0.1` line (beta.32 to beta.36, latest
2026-10-06) is outside the range as well.

M0 does not fetch the browser, so nothing is broken yet, and M0 did not
change this pin: a fingerprint-critical bump is its own PR.

Owner decision (2026-10-08, D29 in `00-decisions.md`): M5 moves the pin to
**v152.0.4-beta.30** as the fingerprint-critical bump inside its PR and
verifies it with the live self-test. `playwright-core` stays at 1.62.1.

## Rust core

| Crate | Version | State | Notes |
|---|---|---|---|
| tokio | 1.53.2 | locked | `default-features = false`; each crate enables what it needs. |
| axum | 0.8.9 (2026-04-14) | locked | |
| tower | 0.5.3 | locked | |
| tower-http | 0.7.1 | pinned | timeout, limit, request-id, trace, cors, compression. Same `http` 1 and `tower` 0.5 stack as axum 0.8. |
| utoipa | 6.0.0 (2026-09-22) | locked | Compatibility verified in M0: `utoipa-axum` 0.3.0 and `utoipa-swagger-ui` 10.0.1 both require `utoipa ^6.0.0` and `axum ^0.8.4`, and the three compile together. Pinned together. |
| utoipa-axum | 0.3.0 (2026-09-22) | locked | `kage-api` registers its route through it. |
| utoipa-swagger-ui | 10.0.1 (2026-09-23) | locked | Features `axum` and `vendored`. Without `vendored` the build script downloads the Swagger UI bundle at compile time. |
| sqlx | 0.9.0 (2026-05-21) | locked (`kage-store`) | `postgres`, `runtime-tokio`, `tls-rustls-ring-webpki`, `chrono`, `uuid`, `json`, `migrate`, `macros`. Never `tls-native-tls` (OpenSSL conflict with BoringSSL). MSRV 1.94. |
| serde, serde_json | 1.0.229, 1.0.151 | locked | |
| tracing, tracing-subscriber | 0.1.44, 0.3.23 | locked | |
| tracing-opentelemetry | 0.34.0 | pinned | |
| opentelemetry, opentelemetry_sdk, opentelemetry-otlp | 0.33.0 | pinned | Default features off: the OTLP transport is chosen in M12 and must use rustls, never OpenSSL. |
| metrics, metrics-exporter-prometheus | 0.24.6, 0.18.3 | pinned | |
| figment | 0.10.19 | pinned | YAML + env layering. |
| clap | 4.6.7 | locked | `derive`, `env`. |
| thiserror, anyhow | 2.0.21, 1.0.104 | locked | `anyhow` only in the binary crate. |
| async-trait | 0.1.92 | pinned | |
| governor | 0.10.4 | pinned | Token-bucket limits per key and per domain. |
| notify | 8.2.0 | pinned | Policy hot reload. 9.0.0 is still a release candidate. License CC0-1.0: add it to `deny.toml` when first used. |
| url, ipnet, hickory-resolver | 2.5.8, 2.12.2, 0.26.3 | url and ipnet locked (transitive), hickory-resolver pinned | SSRF guard needs rustls DNS-over-TCP/UDP only. |
| sha2, hmac, rand, secrecy, uuid | 0.11.0, 0.13.0, 0.10.3, 0.10.3, 1.27.0 | pinned (sha2, hmac, rand, uuid also locked transitively) | |
| scraper, html5ever | 0.27.0, 0.39.0 | pinned | Extraction. html5ever is held at the version scraper 0.27.0 is built against so their types match; 0.40.1 exists. |
| robotstxt | 0.3.0 | pinned | Google robots.txt matcher port. Last release 2021. |
| quick-xml | 0.42.0 | pinned | Sitemaps. |
| chrono | 0.4.45 | locked (transitive) | |
| http-body-util | 0.1.5 | locked | Test only. |
| wiremock | 0.6.5 | pinned | Test only. |
| testcontainers, testcontainers-modules | 0.27.3, 0.15.0 | pinned | Test only. testcontainers 0.28.0 exists, but testcontainers-modules 0.15.0 requires `^0.27`; the two move together. |

M0 resolved all 44 external pins together in a scratch workspace: 492
packages, one version of every HTTP-stack crate, TLS through `ring`, `rustls`
and `btls-sys` only, and no `openssl-sys`, `openssl`, `native-tls` or
`reqwest`. That check caught the two held-back pins above.

Hard rule: `cargo tree -i openssl-sys` must be empty. `just deny` enforces it
three ways: `scripts/check-no-openssl.sh` (lockfile and `cargo tree`), and a
ban on `openssl-sys`, `openssl` and `native-tls` in `deny.toml`. `Cargo.lock`
contains `openssl-macros`; that is a proc-macro crate used by `btls` and links
nothing.

## Node sidecar

| Package | Version | State | Notes |
|---|---|---|---|
| Node.js | 24.21.0 (Active LTS "Krypton") | locked | `.nvmrc`, `engines` and the image tag pin it. 26.x is the Current line and not LTS yet. |
| pnpm | 10.34.6 | locked | Latest 10.x, pinned with its hash in `packageManager`. 11.x and 12.x exist; moving is a deliberate PR. `hoist: false` keeps the two playwright versions apart. |
| TypeScript | 5.9.3 | locked | Latest 5.x. 6.0.3 and 7.0.2 exist; `typescript-eslint` 8.71.1 supports `<6.1.0`, so 7.x is not usable yet. `strict: true`. |
| fastify | 5.12.5 | locked | Internal HTTP server. |
| zod | 4.6.5 | locked | Contract schemas; OpenAPI emitted from them from M4 on. |
| pino | 10.4.0 | locked (transitive, through fastify) | Direct dependency from M4. |
| prom-client | 15.1.3 | planned (M4) | |
| @opentelemetry/sdk-node | 0.223.0 | planned (M12) | |
| vitest, @vitest/coverage-v8 | 5.0.3 | locked | |
| vite | 8.3.3 | locked | Required peer of vitest. 8.3.4 was published on the verification day and is held back by `minimumReleaseAge`. |
| eslint, @eslint/js | 10.12.0, 10.0.1 | locked | |
| typescript-eslint | 8.71.1 | locked | |
| eslint-config-prettier, prettier | 10.1.8, 3.9.9 | locked | |
| globals, @types/node | 17.13.0, 24.19.1 | locked | `@types/node` follows the Node major. |

## Toolchain

| Tool | Version | Pinned in |
|---|---|---|
| Rust | 1.99.0 (2026-09-28), edition 2024 | `rust-toolchain.toml`, builder tag in `deploy/docker/kage.Dockerfile` |
| just | 1.58.0 | `.github/actions/setup-just/action.yml` |
| cargo-deny | 0.20.2 | `justfile` |
| cargo-audit | 0.22.2 | `justfile` |
| cargo-llvm-cov | 0.9.1 | `justfile` |
| cargo-cyclonedx | 0.5.9 | `justfile` |
| cargo-mutants | 27.1.0 | `justfile` |
| cargo-auditable | 0.7.7 | `deploy/docker/kage.Dockerfile` |
| Docker Engine / Compose | 27+ / v2 or newer | Verified with Engine 29.8.2 and Compose v5.6.0. |
| PostgreSQL | 17.11 | `deploy/compose.yaml` |
| gitleaks | v8.30.1 | `justfile` (image digest) |
| trivy | 0.75.0 | `justfile` (image digest) |
| syft | v1.54.1 | `justfile` (image digest) |

## Container images

Every image is pinned by tag and digest.

| Image | Digest | Used by |
|---|---|---|
| `rust:1.99.0-trixie` | `sha256:6ff07edce8775d0f64be7aba9197229407301bddf2054d62c27b541a6238a181` | kage builder |
| `gcr.io/distroless/cc-debian13:nonroot` | `sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2` | kage runtime |
| `node:24.21.0-trixie-slim` | `sha256:173f125896c3b47ddf056734c7ea789d04595a6a08769a8f78e0df642781fb66` | sidecar build and runtime |
| `postgres:17.11-trixie` | `sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3` | compose |
| `ghcr.io/gitleaks/gitleaks:v8.30.1` | `sha256:c00b6bd0aeb3071cbcb79009cb16a60dd9e0a7c60e2be9ab65d25e6bc8abbb7f` | `just secrets` |
| `ghcr.io/aquasecurity/trivy:0.75.0` | `sha256:af6acf9a6b85dfe389a1941505c0ce9efef52a4719635e1a962f022a3d855daa` | `just scan-images` |
| `ghcr.io/anchore/syft:v1.54.1` | `sha256:3eb5379ba7b409c3f4069b686110527af0c47df993fa5c10d13e7cf34f49b1aa` | `just sbom`, `just sbom-images` |

## GitHub Actions

Pinned by commit SHA in `.github/`.

| Action | Version | Commit |
|---|---|---|
| actions/checkout | v7.0.1 | `3d3c42e5aac5ba805825da76410c181273ba90b1` |
| actions/setup-node | v7.1.0 | `949feb2413d6458794dcd2491c4babbbce0c15c1` |
| actions/upload-artifact | v7.0.2 | `cf430e030ddbb5b0abf93d22962f4752f3646cd9` |
| actions/download-artifact | v8.0.2 | `9000827ccba6bdab643e8b6fd33ac0654aef8333` |
| pnpm/action-setup | v6.1.0 | `ea17c68df8912ef543352723c149a84f56e3d413` |
| Swatinem/rust-cache | v2.9.2 | `6323deb102c322ba6fcbdcafc7e3dddab59af2b6` |
| taiki-e/install-action | v2.87.26 | `f7e5d7c961414b23f5b25b2da9294395d08513ad` |
| docker/setup-buildx-action | v4.4.1 | `f87e5991a6d7451dcb8d9637bfbc97413f497069` |
| docker/login-action | v4.6.0 | `dbcb813823bdd20940b903addbd779551569679f` |
| docker/metadata-action | v6.2.0 | `dc802804100637a589fabce1cb79ff13a1411302` |
| docker/build-push-action | v7.4.0 | `c3c9e263c25d99ce0380d002d59b67737d91b0dc` |
| anchore/sbom-action | v0.24.3 | `66cbf4bc1f1c0d2edc94016e65bc221b6bb0ad6c` |

## Verification log

| Date | Who | What changed |
|---|---|---|
| 2026-10-08 | spec author | Initial pins from registry lookups. |
| 2026-10-08 | M0 session | Re-verified every pin. Confirmed wreq 0.16.1, wreq-util 0.2.0, patchright 1.63.0, camoufox-js 0.12.1, axum 0.8.9, sqlx 0.9.0, utoipa 6.0.0. Resolved the open ones: playwright-core 1.62.1, utoipa-axum 0.3.0, utoipa-swagger-ui 10.0.1, Rust 1.99.0, Node 24.21.0, pnpm 10.34.6, TypeScript 5.9.3 and every "latest at M0" entry. Two "latest" pins are held one release back because the full pin set does not resolve otherwise: testcontainers 0.27.3 and html5ever 0.39.0. Added the State column, tool, image and action pins. Newer majors left alone on purpose: TypeScript 6 and 7, pnpm 11 and 12, playwright-core 1.63 and 1.64 (camoufox-js peer range), Node 26 (not LTS). Two findings for the owner: the Camoufox browser pin does not fit playwright-core 1.62.1 (section above), and camoufox-js brings an AGPL-3.0-or-later dependency (ADR 0008). |
