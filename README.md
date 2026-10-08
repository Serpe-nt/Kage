# Kage

Kage is a self-hosted API service that fetches web pages for its operator's
own projects while presenting, to the target site, the same signals a real
person on a household connection would present: a static residential IP, one
long-lived persona per IP, a real browser (patched Chromium or Camoufox) in an
anti-detect sidecar, and a wreq-based HTTP path for lenient sites.

**Status: scaffold (milestone M0).** The repository builds, tests and ships
images through every quality gate, but no fetching feature exists yet. `kage`
serves `/healthz`, each sidecar serves `/v1/health`, and the engines are
stubs. The contract is in [docs/01-spec.md](docs/01-spec.md) and the road to
1.0 in [docs/02-plan.md](docs/02-plan.md).

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
dependencies and the git hooks, and creates `deploy/.env` with random local
secrets. The stack publishes one port, on loopback only, because the API has
no authentication until milestone M8.

## Development

Every dev and CI command is a `just` target. `just` without arguments lists
them all.

| Command | What it does |
|---|---|
| `just setup` | Install toolchains, pnpm dependencies, git hooks, local secrets |
| `just fmt` | rustfmt and prettier |
| `just lint` | clippy with warnings denied, eslint, tsc |
| `just test` | Rust tests and sidecar vitest |
| `just cov` | Coverage reports with thresholds |
| `just deny` | cargo-deny, cargo-audit, pnpm audit, license policy, openssl-sys check |
| `just sbom` | CycloneDX SBOMs |
| `just build-images` | Images for kage and both sidecar variants |
| `just test-e2e` | Compose-based end-to-end suite |
| `just dev` | Postgres and sidecars in compose, `kage serve` natively |
| `just ci` | Everything a pull request runs |

House rules for contributors and coding agents are in
[CLAUDE.md](CLAUDE.md). Repository settings, CI jobs and the release
procedure are in [docs/runbooks/repo.md](docs/runbooks/repo.md).

## Layout

| Path | Contents |
|---|---|
| `crates/` | Rust workspace: the `kage` binary and its library crates |
| `sidecar/` | pnpm workspace: contract, engine-core, both engines, server |
| `deploy/` | Dockerfiles and compose files |
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

Third-party components keep their own licenses. One of them needs attention:
the Firefox sidecar image contains `ua-parser-js` 2.x, a dependency of
`camoufox-js` that is licensed AGPL-3.0-or-later. How the project deals with
it is an open decision, recorded in
[docs/adr/0008-agpl-ua-parser-js-in-firefox-engine.md](docs/adr/0008-agpl-ua-parser-js-in-firefox-engine.md).
