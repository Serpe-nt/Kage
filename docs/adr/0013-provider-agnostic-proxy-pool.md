# ADR 0013: A provider-agnostic proxy pool, personas from a declared list, and health without a provider

Status: accepted, 2026-10-19, by the owner (the question round of the
2.0 phase, `docs/00-decisions.md` D51). Widens D5 (Webshare as the
provider) without changing the Webshare path. Ships in 2.0.0 (M15).

## Context

The pool has a provider trait since M2 (`ProxyProvider`: list, replace,
replaced log, account, stats) with two implementations, Webshare and a
static list given as a secret (`host:port:user:pass[:country]` lines or
a YAML document). A persona's country, time zone and languages derive
from the observed geo of its proxy (§6.1), looked up through the proxy;
reachability is probed against the provider's own echo, with a public
echo as the confirmation. The owner wants: the Webshare API optional, a
plain YAML list as a first-class way to run Kage with live checks that
say which proxies are alive, personas built from the list with a
language and region the operator declares, and a clear road for a
contributor to add another provider's API.

## Decision

1. **The trait is the plugin surface, with capabilities.** `ProxyProvider`
   gains `capabilities()`; `replace`, `replaced`, `account` and `stats`
   are optional and a provider that lacks one says so (`Unsupported`),
   and the pool, the CLI and the health page show what the provider
   cannot do instead of failing. `proxies.provider` names the provider
   (`auto | webshare | static | <name>`); a new provider is one module
   under `crates/kage-proxy/src/provider/`, its configuration section,
   its row in `docs/versions.md` if it adds a pin, and a pass of the
   **conformance suite** (`crates/kage-proxy/tests/provider_conformance.rs`:
   listing, credentials never logged, the state machine's edges, the
   replaced log where supported, rate limits honoured, errors typed),
   run against a mock of its API that the contributor writes from the
   provider's documentation. `CONTRIBUTING.md` walks a contributor
   through it step by step.
2. **A declared list builds personas.** A static entry carries `country`
   (ISO 3166-1 alpha-2) and may carry `locale` (BCP 47) and `city`; a
   persona is generated from the declared values exactly as from an
   observed geo record (§6.1), with `locale` overriding the country's
   primary language. **An entry without a country gets no persona** and
   is usable at level 0 only (ADR 0012), with a warning at start and in
   `kage config check`. The declared values are the claim; where a geo
   lookup is possible (`proxies.health.geo_lookup`, on by default, through
   the proxy as today) Kage verifies it: a mismatch on two consecutive
   lookups is `geo_mismatch`, the proxy `degraded` with an alert, and the
   persona keeps its declared identity until the operator resolves it.
3. **Health without a provider echo.** `proxies.health.echo_url` names
   the page every reachability probe fetches through the proxy and the
   leak check compares with; its default for a provider without an echo
   of its own is the public echo 1.0 already uses as its confirmation
   (`https://checkip.amazonaws.com/`), by the owner's choice, and the
   runbook says plainly that the echo's operator sees every proxy
   address on the probe's schedule and that `selftest.http.echo_url` (the
   operator's own echo, O5) is the private alternative. The probe
   schedule, the backoff and `egress_unsafe` are the pool's as before.
4. **Nothing changes for Webshare**: its echo, its replaced log, its
   account sync and D31 stay; `auto` still prefers it when its key is
   set.

## Consequences

- D5 reads "Webshare, or any provider behind the trait, or a declared
  list"; the plan's and the runbook's figures for the 20-address pool
  stay as the owner's own deployment.
- A declared list with countries is the way to run Kage on proxies of
  any vendor today; a provider module is the way to get replacement and
  account sync for a vendor that offers them.
- The field `country` is required for a persona and the error is loud,
  because an address in one country speaking another's language is a
  measurable tell (§6.1).
