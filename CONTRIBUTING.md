# Contributing to Kage

Kage is built in milestones, one pull request each, through the same gates
for every change. This page is the short version of the rules and a
step-by-step guide for the change most often asked for: another proxy
provider. `CLAUDE.md` has the house rules in full; `docs/00-decisions.md`
the decisions that are settled; `docs/01-spec.md` the contract.

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md). A
vulnerability is reported privately, as [SECURITY.md](SECURITY.md) says,
never in an issue or a pull request. An issue starts from one of the forms
(a bug, a feature, a proxy provider); a change that re-opens a settled
decision says which one and why, and becomes an ADR draft before it
becomes code.

## The gates

On Linux (or WSL2) with Docker, rustup, Node.js 24 with corepack and
[just](https://just.systems) 1.58 or newer; `just setup` installs the
pinned toolchain, the cargo tools, the pnpm dependencies, the pinned Chrome
and Camoufox, the git hooks and local secrets:

```bash
just setup
```

```bash
just ci
```

`just ci` runs what a pull request runs on GitHub: the commit messages,
rustfmt and prettier, clippy with warnings denied (pedantic included, tests
too), eslint and tsc, the sqlx query data against the migrations, every
Rust and sidecar test (Postgres in Docker through testcontainers), the
coverage gate (80 % of lines on the core crates), cargo-deny, cargo-audit,
pnpm audit, the `openssl-sys` check, gitleaks over the history, the SBOMs,
the images and their scan, and the compose end-to-end tests. Nothing
reaches a real site: live tests (`KAGE_LIVE=1`) run only on the owner's
word, against the echo and detection pages of the configuration.

Every target on its own (`just` without arguments lists them all):

| Command | What it does |
|---|---|
| `just fmt` | rustfmt and prettier |
| `just lint` | clippy with warnings denied, eslint, tsc, the dashboards and the alert rules of the observability profile, the README's badges |
| `just test` | Rust tests (the store tests start Postgres in Docker), sidecar vitest (the Chromium engine's tests start the real browser; not as root), the gate scripts' and the field runner's own tests |
| `just contract-check` | The committed sidecar OpenAPI document is current and breaks no client; the public API's document is valid and its TypeScript client current |
| `just openapi` | Regenerate both OpenAPI documents and the API's TypeScript test client |
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
| `just test-live` | Gated live suite through the real proxies; needs `KAGE_LIVE=1` and secrets, and runs only on the maintainer's word |
| `just field <index>` | One site's batch of a field campaign through the running campaign stack (`docs/runbooks/field-campaign.md`); reads a real site, so it needs `KAGE_FIELD=1` and the round's private files, and runs only on the owner's approval of the list, never in CI |
| `just mutants` | Mutation testing of `kage-core`, 70 % required; before a milestone is merged |
| `just dev` | Postgres and sidecars in compose, `kage serve` natively |
| `just migrate` | Apply the migrations to the development database |
| `just config-check` | Validate the development configuration and print it, secrets redacted |
| `just ci` | Everything a pull request runs |

What a change must keep:

- Exact version pins and committed lockfiles; a new or bumped crate gets
  its row in `docs/versions.md` with its licence. No `openssl-sys` anywhere:
  every TLS-capable crate uses rustls, and all outbound HTTP in Rust goes
  through wreq.
- Tests at the layer spec §21 names, and 80 % line coverage on a new
  module of a core crate.
- No `unwrap`, `expect` or `panic!` in library crates outside tests: typed
  errors (`thiserror`) in libraries, `anyhow` in the binary.
- A doc comment on every public type and function; the OpenAPI document
  and the API test client regenerated (`just openapi`) when an API type
  changes; `deploy/kage.example.yaml` listing every non-secret key (a test
  compares).
- No secret, no real proxy credential, no API key and no unsanitized page
  in a commit, a log line, a test fixture or an error message.

## Pull requests

- Conventional commits (`feat(proxy): …`, `fix(core): …`); the commit check
  refuses a comma in the type or the scope.
- One change, one pull request, from a branch on `main`; `ci-ok` green; a
  rebase merge that keeps the commits. A pull request from a fork may wait
  for the maintainer to approve its CI run.
- A bump of anything in the fingerprint-critical table of
  `docs/versions.md` is a pull request of its own, never part of another
  change and never merged by a bot.
- The description says how each acceptance criterion was verified, with the
  evidence (a test name, a log, a measurement), and lists anything that
  differs from the spec as a discrepancy, with an ADR draft when it changes
  a decision. The template (`.github/PULL_REQUEST_TEMPLATE.md`) asks for
  both.
- A line in `CHANGELOG.md` under Unreleased; breaking changes first.

## The definition of done

A change is done when every line holds (`docs/02-plan.md`, "How agents work
this plan", is the source):

- new code has tests at the layer spec §21 names, and a new module of a
  core crate keeps 80 % line coverage;
- `just ci` passes locally, and `ci-ok` is green on the pull request;
- no new `unwrap`, `expect` or `panic!` in a library crate outside tests;
- public types and functions are documented; the OpenAPI documents and the
  API test client are regenerated and committed when a type of theirs
  changed;
- `docs/versions.md` has a row for every new or bumped dependency, with its
  licence;
- `CHANGELOG.md` has its line under Unreleased;
- the description lists how each acceptance criterion was verified, and
  every discrepancy with the spec.

## Adding a proxy provider

A provider is where the pool's list of proxies comes from, and, when the
vendor offers it, who replaces a burned proxy, keeps a log of replaced
proxies, reports the account and counts the traffic (spec §7.1, ADR 0013).
Kage ships two: `WebshareProvider` (everything) and `StaticListProvider`,
the operator's declared list (lists only). **A vendor with no API needs no
provider**: its proxies run as a declared list
(`docs/runbooks/proxies.md`, "A declared list"). Write a provider when the
vendor's API brings what a list cannot: replacement, a replaced log, the
account, traffic stats.

A provider is four things: one module under
`crates/kage-proxy/src/provider/`, its configuration section, its pins in
`docs/versions.md`, and a pass of the conformance suite against a mock of
its API. Step by step:

### 1. Read the vendor's documentation, and write down what you read

Note every endpoint you will call, with its link, its parameters, its
answer's fields, its error codes and its rate limits. Note what the API
does not say (the M2 hand-over found three undocumented fields and a list
that answered without a trailing slash only by asking). The pull request
carries this table.

### 2. The module

Copy this skeleton to `crates/kage-proxy/src/provider/<name>.rs` and fill
in what the vendor offers; leave out the methods of what it does not (their
default bodies answer `Unsupported`, and `capabilities` must say so):

```rust
//! The <Vendor> API client (spec §7.1). Paths and parameters verified
//! against <documentation link> on <date>.

use async_trait::async_trait;
use kage_core::config::<Vendor>Config;
use kage_core::domain::ProxyProviderKind;

use super::{Capabilities, ProviderError, ProviderProxy, ProxyProvider, Result};

/// What the client calls itself: a vendor is not a target.
const USER_AGENT: &str = concat!("kage/", env!("CARGO_PKG_VERSION"));

/// What <Vendor> offers besides listing, and its own echo, if it runs one
/// (the probes then read it and no third party sees the pool).
pub const CAPABILITIES: Capabilities = Capabilities {
    replacement: false,
    replaced_log: false,
    account: false,
    stats: false,
    echo: None,
};

/// The <Vendor> API.
pub struct <Vendor>Provider {
    client: wreq::Client,
    // the API root, the key as a header value, the token buckets…
}

// Never derive Debug: write it by hand, without the key.
impl std::fmt::Debug for <Vendor>Provider { /* … */ }

impl <Vendor>Provider {
    /// A client for the configured account.
    ///
    /// # Errors
    ///
    /// Fails when the configuration has no key or the client cannot be built.
    pub fn new(config: &<Vendor>Config) -> Result<Self> {
        let client = wreq::Client::builder()
            .no_proxy()                 // directly, never through a proxy
            .user_agent(USER_AGENT)
            .timeout(config.timeout.get())
            .build()
            .map_err(|error| ProviderError::Setup(error.to_string()))?;
        // …
    }
}

#[async_trait]
impl ProxyProvider for <Vendor>Provider {
    fn kind(&self) -> ProxyProviderKind {
        ProxyProviderKind::<Vendor>
    }

    fn capabilities(&self) -> Capabilities {
        CAPABILITIES
    }

    async fn list(&self) -> Result<Vec<ProviderProxy>> {
        // Read every page; map each proxy to a ProviderProxy with a
        // reference that never changes while the proxy lives.
        todo!()
    }
}
```

What the module must hold to:

- **HTTP through wreq only**, plain (no browser emulation), with an honest
  `User-Agent`, never through a proxy, never following a redirect to
  another host, and to an `https` root (plain `http` only to localhost,
  for a mock).
- **The vendor's limits, kept from below**: a `crate::rate::TokenBucket`
  per limit under the documented rate, `Retry-After` honoured on a 429,
  reads retried, and **a request that changes something never sent twice**
  after a failure that may have applied it
  (`ProviderError::OutcomeUnknown`).
- **Typed errors**: `Transport` for no answer, `Unauthorized` for a refused
  key, `RateLimited` once the retries are spent, `Status` with the vendor's
  code, `Decode` for an answer its documentation does not describe,
  `QuotaExhausted` and `Busy` for a replacement the vendor refuses.
  `Unsupported` only for what `capabilities` says is not offered.
- **No credential anywhere it could be read**: not in a log line, a
  `Debug` output or an error's text. `Secret` holds credentials; write
  `Debug` by hand.
- **Identity**: does the vendor *state* where an address is (Kage then
  trusts the geo lookup and the vendor's country is a fallback, as for
  Webshare), or does the operator *declare* it (as for a list)? That is
  `ProxyProviderKind::declares_identity` (step 3).

### 3. Wire it in

- `ProxyProviderKind::<Vendor>` in `crates/kage-core/src/domain/proxy.rs`,
  with its stable name, and its arm in `declares_identity`; a migration
  that adds the name to the `proxies.provider` check constraint
  (`crates/kage-store/migrations/`), then `just sqlx-prepare`.
- `ProviderChoice::<Vendor>` and `ResolvedProvider::<Vendor>` in
  `crates/kage-core/src/config/proxies.rs`, and whether `auto` may pick it.
- The arm in `kage_proxy::provider::capabilities_of` and in
  `Pool::from_config`.
- `pub use` of the provider in `provider/mod.rs` and `lib.rs`.

### 4. Its configuration section

A `<Vendor>Config` in `crates/kage-core/src/config/` with
`#[serde(deny_unknown_fields)]`, a default for every key, and its checks in
the section's `validate` (an API root that carries a key must be `https`).
Its key is a secret: a `SecretSpec` in `SECRETS` (`config.rs`), set only by
`KAGE__<VENDOR>__API_KEY` or `_FILE`, refused in the configuration file.
Every non-secret key goes into `deploy/kage.example.yaml`.

### 5. The mock

A stateful wiremock responder in `crates/kage-proxy/tests/common/`
written from the vendor's documentation, as `MockWebshare` is: the list
(with paging), the replacement lifecycle (requested, pending, done,
failed), the replaced log, the account, the stats, injectable 429s with
`Retry-After` and injectable errors (401, 500). The mock is the
documentation as code: where the documentation is silent, say so in a
comment.

### 6. The conformance suite

Add a subject to `crates/kage-proxy/tests/provider_conformance.rs`: a
`World` that builds your provider against your mock and scripts it (list
proxies, take one away, swap one by itself and log it, finish or fail a
replacement, rate limit the next request, refuse the key, a provider at an
address where nothing answers, the secrets to look for), and a test:

```rust
#[tokio::test(flavor = "current_thread")]
async fn the_<vendor>_provider_conforms() {
    assert_conforms(|| Box::pin(<Vendor>World::start())).await;
}
```

The suite checks six rules (listing, credentials, the pool's state
machine through a real pool on Postgres, the replaced log, rate limits,
typed errors) and prints every violation it finds. Write your own tests
besides for what the suite does not see: paging, how each field maps,
your error codes.

### 7. Pins, docs and the pull request

- A row in `docs/versions.md` for any crate the provider adds (none should
  be needed: wreq, serde and the rest are there), and `just deny`.
- A section in `docs/runbooks/proxies.md` (how to get a key, what to
  switch off in the vendor's dashboard, what its capabilities mean for the
  operator), a sentence in spec §7.1, a section for the vendor's API like
  §7.2, a row in `docs/security-review.md` for its key, and the changelog.
- The pull request: the endpoint table of step 1 with its links, the
  suite's run, and any discrepancy between the documentation and what you
  found. A live check against the vendor's real API happens only on the
  owner's word, read-only, and never against a target site.
