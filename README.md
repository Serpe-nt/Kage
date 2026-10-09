# Kage

Kage is a self-hosted API service that fetches web pages for its operator's
own projects while presenting, to the target site, the same signals a real
person on a household connection would present: a static residential IP, one
long-lived persona per IP, a real browser (patched Chromium or Camoufox) in an
anti-detect sidecar, and a wreq-based HTTP path for lenient sites.

**Status: foundations (milestone M1).** The domain model, configuration,
policy files, block classifier, SSRF guard and the Postgres store exist and
pass every quality gate, but nothing fetches a page yet. `kage` serves
`/healthz`, each sidecar serves `/v1/health`, and the engines are stubs. The
contract is in [docs/01-spec.md](docs/01-spec.md) and the road to 1.0 in
[docs/02-plan.md](docs/02-plan.md).

## Responsible use

Kage is built for fetching publicly available pages at a human pace for
personal projects. Whoever runs it is responsible for how it is used.

- **Follow the law and the site's terms.** Scraping rules differ by country
  and by site. Check both before pointing Kage at a target, and stop when a
  site asks you to.
- **Stay polite.** Pacing defaults imitate one human visitor per persona.
  Do not raise them to extract more than a person could read. Kage fetches
  `robots.txt`, reports its verdict on every response, and enforces it
  wherever the domain policy says so.
- **No access you were not given.** Kage does not solve CAPTCHAs through
  external services, does not log in with accounts, does not perform write
  actions on targets, and never connects to a target from the host's own IP.
  These are design decisions, not missing features; please do not send
  patches that add them.
- **Handle personal data with care.** Fetched pages can contain personal
  data. Retention defaults are short. Store only what your project needs and
  delete it when you are done.
- **Do no harm.** Do not use Kage to overload a site, to evade a ban that was
  placed on you for abuse, to commit fraud, or to collect data about people
  for harassment or surveillance.

The settled decisions behind these limits are in
[docs/00-decisions.md](docs/00-decisions.md) (D10, D13, D15, D17) and
[docs/adr/0006-no-solvers-no-direct-mode.md](docs/adr/0006-no-solvers-no-direct-mode.md).

## Quick start

Kage is developed and run on Linux. On Windows, work inside WSL2.

Prerequisites:

- Docker Engine 27 or newer with Compose v2
- [rustup](https://rustup.rs) (the toolchain is pinned in `rust-toolchain.toml`)
- Node.js 24 LTS (see `sidecar/.nvmrc`) with corepack
- [just](https://just.systems) 1.58 or newer
- To compile outside Docker: `cmake`, `clang`, `libclang-dev`, `nasm`, `perl` and `go`,
  which the BoringSSL build inside wreq needs

```bash
just setup
docker compose --file deploy/compose.yaml up --build --wait
curl http://127.0.0.1:8080/healthz
```

`just setup` installs the pinned toolchains and tools, the sidecar
dependencies, the pinned Chrome with Xvfb for the sidecar's browser tests
(through apt, with sudo; Debian or Ubuntu) and the git hooks, and creates
`deploy/.env` with random local secrets. The stack applies the database migrations in a one-shot `migrate`
service before `kage` starts, and publishes one port, on loopback only,
because the API has no authentication until milestone M8.

## Configuration

`kage` reads `kage.yaml` (or the file named by `--config` / `KAGE_CONFIG`)
and lets environment variables of the form `KAGE__SECTION__KEY` override it.
[deploy/kage.example.yaml](deploy/kage.example.yaml) lists every key with its
default. Secrets are never read from the file: they come from the environment
or from a file named by the same variable with `_FILE` appended, and `kage`
refuses to start when a required one is missing.

```bash
kage config check   # validate, then print the effective configuration, secrets redacted
kage migrate        # apply the database schema; safe to run again
kage proxy list     # the proxy pool; also `health`, `replace` and `egress`
kage selftest http  # compare the http path's fingerprint with the recorded one
kage serve
```

Kage fetches only through proxies. Give it a Webshare API key
(`KAGE__WEBSHARE__API_KEY`, or `KAGE_WEBSHARE_API_KEY` in `deploy/.env`) or a
static list, and it keeps the pool by itself: health checks, backoff, and
replacement within a monthly budget. Without a provider the pool is empty and
nothing is fetched. [docs/runbooks/proxies.md](docs/runbooks/proxies.md) is
the operator's guide.

Per-domain behaviour is data in [policies/](policies/): `domains.yaml`
(render mode, pacing, robots, retries, cooldowns), `pacing-profiles.yaml`
and `block-signatures.yaml`. `kage` reloads them when they change; an
invalid file keeps the last good policy in force and raises an alert.

## Development

Every dev and CI command is a `just` target. `just` without arguments lists
them all.

| Command | What it does |
|---|---|
| `just setup` | Install toolchains, pnpm dependencies, the pinned Chrome, git hooks, local secrets |
| `just fmt` | rustfmt and prettier |
| `just lint` | clippy with warnings denied, eslint, tsc |
| `just test` | Rust tests (the store tests start Postgres in Docker) and sidecar vitest (the Chromium engine's tests start the real browser; not as root) |
| `just contract-check` | The committed sidecar OpenAPI document is current and breaks no client |
| `just openapi` | Regenerate the sidecar OpenAPI document from its zod schemas |
| `just capture-browser-fingerprint [engine]` | Record what an engine's real browser shows the fingerprint echo (`chromium`, `firefox`) |
| `just sqlx-check` | Migrations apply from empty and are idempotent; `.sqlx/` matches the queries |
| `just sqlx-prepare` | Regenerate `.sqlx/` after a query or a migration changed |
| `just cov` | Coverage reports with thresholds |
| `just deny` | cargo-deny, cargo-audit, pnpm audit, license policy, openssl-sys check |
| `just sbom` | CycloneDX SBOMs |
| `just build-images` | Images for kage and both sidecar variants |
| `just test-e2e` | Compose-based end-to-end suite |
| `just test-live` | Gated live suite through the real proxies; needs `KAGE_LIVE=1` and secrets |
| `just dev` | Postgres and sidecars in compose, `kage serve` natively |
| `just migrate` | Apply the migrations to the development database |
| `just config-check` | Validate the development configuration and print it, secrets redacted |
| `just ci` | Everything a pull request runs |

House rules for contributors and coding agents are in
[CLAUDE.md](CLAUDE.md). Repository settings, CI jobs and the release
procedure are in [docs/runbooks/repo.md](docs/runbooks/repo.md); running the
proxy pool is in [docs/runbooks/proxies.md](docs/runbooks/proxies.md), and
the fingerprint echo and the http self-test are in
[docs/runbooks/fingerprint-echo.md](docs/runbooks/fingerprint-echo.md), and
the Chromium engine, its container and the browser self-test are in
[docs/runbooks/engine-chromium.md](docs/runbooks/engine-chromium.md).

## Layout

| Path | Contents |
|---|---|
| `crates/` | Rust workspace: the `kage` binary and its library crates |
| `sidecar/` | pnpm workspace: contract, engine-core, both engines, server |
| `deploy/` | Dockerfiles, compose files and the example configuration |
| `policies/` | Domain policies, pacing profiles and block signatures (data, hot-reloaded) |
| `docs/` | Decisions, specification, plan, ADRs, pinned versions, runbooks |
| `scripts/` | Gate scripts used by `just` and CI |
| `tests/` | End-to-end and live suites, fixtures |

## Security

Please report vulnerabilities privately through GitHub's "Report a
vulnerability" button on the repository's Security tab, not in a public
issue.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution you
intentionally submit for inclusion in this work, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.

Third-party components keep their own licenses. Two things about the Firefox
sidecar need saying, both recorded in
[docs/adr/0008-agpl-ua-parser-js-in-firefox-engine.md](docs/adr/0008-agpl-ua-parser-js-in-firefox-engine.md):

- `camoufox-js` asks for `ua-parser-js` 2.x, which is AGPL-3.0-or-later. The
  workspace uses the MIT-licensed 1.0.x line instead; the dependency tree has
  no AGPL package.
- **The Firefox sidecar image is not published.** It is built from Camoufox,
  whose build bundles Microsoft's and Apple's font files (they are what makes
  a Windows or macOS persona's fonts right), and a public image of this
  project would redistribute them. Build it where it runs:
  `just build-images`. This is the project's cautious default, not legal
  advice.
