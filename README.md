# Kage

Kage is a self-hosted API service that fetches web pages for its operator's
own projects while presenting, to the target site, the same signals a real
person on a household connection would present: a static residential IP, one
long-lived persona per IP, a real browser (patched Chromium or Camoufox) in an
anti-detect sidecar, and a wreq-based HTTP path for lenient sites.

**Status: 1.0.0.** Kage fetches pages through its pool of personas, in
both browsers and on the http path, alone or as batches, scripted flows and
crawls, with extraction, `robots.txt` per site, a response cache, webhooks,
sessions, an admin API, metrics, traces and an optional observability
stack. The contract is [docs/01-spec.md](docs/01-spec.md), the settled
decisions [docs/00-decisions.md](docs/00-decisions.md), and what changed
[CHANGELOG.md](CHANGELOG.md).

## Responsible use

Kage is built for fetching publicly available pages at a human pace for
personal projects. Whoever runs it is responsible for how it is used.

- **Follow the law and the site's terms.** Scraping rules differ by country
  and by site. Check both before pointing Kage at a target, and stop when a
  site asks you to.
- **Stay polite.** Pacing defaults imitate one human visitor per persona.
  Do not raise them to extract more than a person could read.
- **`robots.txt` is the operator's choice, per site.** A reader never asks
  for that file, so asking marks the address that does. Kage therefore
  asks for a site's `robots.txt` only where the domain policy says
  `report` or `enforce`, and then from one address that never reads the
  site. Under `enforce` it obeys the file fully: what is disallowed is
  not fetched, and a `Crawl-delay` is kept to, however long. Where the
  policy says nothing, every response says that the file was not asked
  for. Decide per site, and decide `enforce` where a site's rules are
  what you mean to follow.
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

On a Linux machine (or WSL2) with Docker Engine 27 or newer, Compose v2,
git and curl, and nothing else:

```bash
git clone https://github.com/Serpe-nt/Kage.git kage && cd kage
scripts/dev-secrets.sh
docker compose --file deploy/compose.yaml up --build --detach --wait
curl http://127.0.0.1:8080/healthz
```

`scripts/dev-secrets.sh` writes random local secrets into `deploy/.env`
(ignored by git). The first build compiles Kage and fetches the pinned
Chrome and Camoufox: count 20 to 40 minutes. The stack applies the
database migrations in a one-shot `migrate` service before `kage` starts,
and publishes one port, on loopback only: the API speaks plain HTTP, and a
deployment that serves other machines puts a reverse proxy that terminates
TLS in front of it.

Kage fetches only through proxies. Put `KAGE_WEBSHARE_API_KEY=<key>` into
`deploy/.env` (or give `kage` a static list,
[docs/runbooks/proxies.md](docs/runbooks/proxies.md)) and run the `up`
again: every proxy gets a persona within minutes. Then make a key and fetch
a page of a site you may fetch:

```bash
docker compose --file deploy/compose.yaml exec kage \
    kage key create --name operator --scope admin --scope fetch
curl http://127.0.0.1:8080/v1/fetch \
    --header "Authorization: Bearer <token>" \
    --header 'Content-Type: application/json' \
    --data '{"url": "https://example.org/"}'
```

[docs/runbooks/first-start.md](docs/runbooks/first-start.md) is the whole
first start, [docs/runbooks/api.md](docs/runbooks/api.md) the guide to
keys, limits, jobs, crawls, sessions and the admin endpoints, and
[docs/runbooks/observability.md](docs/runbooks/observability.md) the
dashboards and alerts. The API's contract is `/openapi.json`
([crates/kage-api/openapi.json](crates/kage-api/openapi.json)).

Building and testing Kage itself needs more: rustup, Node.js 24 with
corepack, and [just](https://just.systems) 1.58 or newer (`just setup`
installs the rest; see Development below).

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
kage persona list   # the personas; also `show`, `selftest`, `regenerate` and `retire`
kage selftest http  # compare the http path's fingerprint with the recorded one
kage fetch <url>    # one request through the whole pipeline; see docs/runbooks/fetching.md
kage key create     # an API key; also `list`, `revoke` and `rotate`
kage openapi        # print the OpenAPI document of the public API
kage serve          # the API, the pool, the personas and the scheduler
```

Kage fetches only through proxies. Give it a Webshare API key
(`KAGE__WEBSHARE__API_KEY`, or `KAGE_WEBSHARE_API_KEY` in `deploy/.env`) or a
static list, and it keeps the pool by itself: health checks, backoff, and
replacement within a budget. Without a provider the pool is empty and
nothing is fetched. [docs/runbooks/proxies.md](docs/runbooks/proxies.md) is
the operator's guide. Every usable proxy gets one persona, a long-lived
synthetic household user with its own browser, screen, language and place:
[docs/runbooks/personas.md](docs/runbooks/personas.md). What a request
passes through (pace, retries on another persona, the circuit breaker, the
forwarder that keeps the provider's credentials out of the browsers, the
bandwidth budget) is in
[docs/runbooks/fetching.md](docs/runbooks/fetching.md).

Per-domain behaviour is data in [policies/](policies/): `domains.yaml`
(render mode, pacing, robots, retries, cooldowns), `pacing-profiles.yaml`
and `block-signatures.yaml`. `kage` reloads them when they change; an
invalid file keeps the last good policy in force and raises an alert.

Every YAML file Kage reads (`kage.yaml`, the policies, the YAML form of the
static proxy list) is read as YAML 1.2 with three rules of Kage's own: a
key said twice in one mapping is an error that gives its line and column,
a merge key (`<<`) is an error, and a document nests at most 128 levels,
has at most a million nodes and 16 MiB of text. Only `true` and `false`
are booleans, so `NO` is Norway. **Write in quotes what is text but reads
as a number**: a password of digits, an id with a leading zero, `1_000`
(`password: "0123"`); unquoted, it is refused where text is expected, and
`kage config check` names the key and says so.

## Development

Every dev and CI command is a `just` target. `just` without arguments lists
them all.

| Command | What it does |
|---|---|
| `just setup` | Install toolchains, pnpm dependencies, the pinned Chrome, git hooks, local secrets |
| `just fmt` | rustfmt and prettier |
| `just lint` | clippy with warnings denied, eslint, tsc, the dashboards and the alert rules of the observability profile |
| `just test` | Rust tests (the store tests start Postgres in Docker) and sidecar vitest (the Chromium engine's tests start the real browser; not as root) |
| `just contract-check` | The committed sidecar OpenAPI document is current and breaks no client |
| `just openapi` | Regenerate the sidecar OpenAPI document from its zod schemas |
| `just capture-browser-fingerprint [engine]` | Record what an engine's real browser shows the fingerprint echo (`chromium`, `firefox`) |
| `just sqlx-check` | Migrations apply from empty and are idempotent; `.sqlx/` matches the queries |
| `just sqlx-prepare` | Regenerate `.sqlx/` after a query or a migration changed |
| `just cov` | Coverage reports with thresholds |
| `just deny` | cargo-deny, cargo-audit, pnpm audit, license policy, openssl-sys check |
| `just hardening` | The container checklist on the Dockerfiles and every compose file (`hardening-images` on the built images) |
| `just sbom` | CycloneDX SBOMs |
| `just build-images` | Images for kage and both sidecar variants |
| `just test-e2e` | Compose-based end-to-end suite, with the load test's design A (200 queued fetches under `fast`) |
| `just test-load` | The load test's design B: 200 queued fetches under the test-only `load` profile at the shipped caps, about 12 minutes |
| `just test-live` | Gated live suite through the real proxies; needs `KAGE_LIVE=1` and secrets |
| `just dev` | Postgres and sidecars in compose, `kage serve` natively |
| `just migrate` | Apply the migrations to the development database |
| `just config-check` | Validate the development configuration and print it, secrets redacted |
| `just ci` | Everything a pull request runs |

House rules for contributors and coding agents are in
[CLAUDE.md](CLAUDE.md). Repository settings, CI jobs and the release
procedure are in [docs/runbooks/repo.md](docs/runbooks/repo.md). The
runbooks of [docs/runbooks/](docs/runbooks/) cover the first start, the
proxy pool, the personas, fetching, the API, rotating keys, upgrading an
engine, reading a block report, the fingerprint echo, observability and the
owner's field campaign; the security review is
[docs/security-review.md](docs/security-review.md).

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
