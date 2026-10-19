# Kage technical specification

Version 1.0 draft, 2026-10-08. Companion documents: `00-decisions.md`
(settled decisions), `02-plan.md` (milestones), `versions.md` (pins),
`adr/` (architecture decision records), `../bot-detection-landscape.md`
(research; kept out of git, ask the owner for a copy).

This document is the contract for implementing agents. Where it says MUST,
tests enforce it. Where it says SHOULD, a PR may deviate with a stated reason.
Defaults in tables are configuration defaults, not hard-coded constants.

---

## 1. Purpose, goals, non-goals

**Purpose.** A self-hosted HTTP API that fetches web pages on behalf of the
owner's projects while presenting, to the target site, the same signals a
real human on a real household connection would present.

Since 2.0 (§25, ADR 0012) the caller says on every request how much of
that it wants, by a **camouflage level** from 0 (a plain HTTP client, no
persona) to 3 (everything below), and the operator says whether the
traffic leaves through the proxy pool or the host's own connection
(`egress.mode`). The goals below describe level 3 under `egress: proxies`;
the lower levels are Kage being a well-behaved client, not a person, and
a site's policy may refuse them (`min_level`).

**Goals**

1. Indistinguishability at every software-controllable layer: egress IP class
   and geo, TCP/TLS/HTTP fingerprints, browser fingerprint, JavaScript
   environment, input behavior, navigation rhythm, returning-visitor state.
2. One API for all consumers: single fetch, batches, scripted flows, crawls,
   with selectable outputs.
3. Measurable stealth: the service continuously measures its own detection
   score and refuses to release on regression.
4. Operability: one `docker compose up`, clear metrics, safe defaults.
5. Industry-standard engineering: typed contracts, tests at every boundary,
   supply-chain hygiene, least privilege.

**Non-goals (v1)**

- External CAPTCHA solving services and manual solving.
- Logins with the owner's accounts, payments, or any write action on targets.
- Passing hardware-rooted attestation (Private Access Tokens, PACT).
- Multi-tenant SaaS features (billing, org management).
- Web Bot Auth / declared-bot mode (future module).
- HTTP/3.

---

## 2. Glossary

| Term | Meaning |
|---|---|
| Proxy | One static residential IP with credentials from Webshare. |
| Persona | A long-lived synthetic household user bound 1:1 to a proxy: engine, fingerprint, locale, timezone, geolocation, persistent browser profile, wreq profile. |
| Engine | A browser implementation in the sidecar: `chromium` (patchright + real Chrome) or `firefox` (Camoufox). |
| Session | A caller-visible handle that pins subsequent requests to one persona for a TTL. |
| Render mode | `http` (wreq, no JavaScript) or `browser` (sidecar). `auto` lets policy decide. |
| Domain policy | Per-domain configuration: render mode, pacing, robots, countries, engines, retries, locks. |
| Outcome | Classified result of an attempt: `ok`, `challenge_passed`, `challenge_failed`, `captcha`, `rate_limited`, `forbidden`, `soft_block`, `provider_error`, `timeout`, `ssrf_denied`, `robots_denied`. |
| Job | Asynchronous unit: `batch`, `flow`, or `crawl`. |
| Sidecar | The Node service that owns browsers. One container per engine, same codebase. |
| Self-test | A persona loading detection test pages and asserting its expected identity. |

---

## 3. Architecture

### 3.1 Components

```
consumers ──HTTPS/JSON──▶ kage (Rust, axum)
                           ├─ api        : auth, OpenAPI, validation, SSRF guard, rate limits
                           ├─ scheduler  : queue, pacing, persona selection, retries, circuit breakers
                           ├─ forwarder  : the one proxy every persona is given; adds the provider's credentials, counts bytes
                           ├─ policy     : domain policies, robots, block signatures (hot reload)
                           ├─ http path  : wreq clients per persona (TLS/H2/header emulation)
                           ├─ browser    : client for the sidecar contract
                           ├─ personas   : generation, lifecycle, state export/import
                           ├─ proxies    : Webshare provider, pool state machine, health, replacement
                           ├─ crawl      : frontier, sitemaps, dedupe
                           ├─ extract    : text/markdown/metadata/links/JSON-LD
                           └─ store      : sqlx/Postgres repositories, artifacts on disk
                                 │
            internal HTTP/JSON + shared secret, compose network only
                                 ▼
          browser-chromium (Node, patchright + Chrome, Xvfb)
          browser-firefox  (Node, camoufox-js + Camoufox, virtual display)
                                 │  per-context proxy: kage's forwarder, under the persona's own credential
                                 ▼
                 kage forwarder ──▶ Webshare static residential IPs ──▶ target sites

postgres (jobs, personas, proxies, results, keys, audit)
otel-collector, jaeger, prometheus, grafana (optional compose profiles)
```

### 3.2 Data flow for one fetch

1. API validates the request, authenticates the key, checks scopes and the
   key's rate limit, runs the SSRF guard on the URL.
2. Policy resolves the effective domain policy and the render decision.
   A request that named a cache lifetime is answered from the response
   cache when the cache holds its page (§15): none of the steps below
   happen. Where the site's policy has its `robots.txt` asked for, what
   the file says of the address is looked up, and under `enforce` waited
   for and kept to (§14); the file is asked for by the site's scout, never
   by the persona of step 3.
3. Scheduler picks a persona: session-pinned if a session is given, otherwise
   a persona already visiting the domain whose slot is free, otherwise the
   best-scored eligible persona (§8.2), preferring one that is awake in its
   own day; it waits for the persona's pacing slot and spaces starts of
   different personas apart (§11).
4. Executor runs the attempt on the http path or the browser path, every
   byte through the in-process forwarder (§7.5).
5. Classifier labels the outcome from status, headers, cookies, body and
   browser signals.
6. On a block a browser cures (`challenge_failed`, `soft_block`, a rule
   marked `escalate` that is not a refusal) under `auto`, the same persona
   makes the browser attempt once, and the verdict on the persona comes
   from that attempt. On a refusal (`forbidden`, `captcha`, `rate_limited`)
   or a browser block, the persona cools for the domain, its visit ends and
   its context is closed, and another persona on another network retries
   after a seeded pause, up to the retry budget.
7. What the request asked to have extracted is read from the document
   the visit captured (§14). Result and artifacts are stored, and the
   cache is filled when the request named a lifetime; the response is
   returned or the job record is updated and webhooks fire.

### 3.3 Deployment topology

One Compose project. Services: `kage`, `browser-chromium`, `browser-firefox`,
`postgres`. Optional profile `observability`: `otel-collector`, `jaeger`,
`prometheus`, `grafana`. Volumes: `personas` (browser profile directories),
`artifacts`, `pgdata`. Only `kage` publishes a port, and only the API's:
the forwarder (§7.5) listens on the compose network alone.

---

## 4. Repository layout

```
kage/
  Cargo.toml                 workspace
  rust-toolchain.toml
  justfile                   every dev and CI command has a just target
  CLAUDE.md                  house rules for agents
  crates/
    kage/                    binary: CLI (serve, migrate, fetch, selftest, persona, proxy, key)
    kage-core/               domain types, outcomes, policy model, pacing math, classifier
    kage-store/              sqlx repositories, migrations/, artifact store
    kage-proxy/              provider trait, Webshare client, pool, health, replacement
    kage-http/               wreq client factory, cookie jar persistence, fingerprint self-test (http)
    kage-browser/            sidecar contract types (generated from sidecar OpenAPI) and client
    kage-persona/            persona manager: generation, lifecycle, sessions, per-domain state, cookie handoff, warm-up, self-tests, profile archive, the owner of persona contexts
    kage-scheduler/          scheduler over the rules in kage-core (`schedule`): grants, the fetch pipeline, the attempt executor over both paths, verdicts, the forwarder, the bandwidth budget
    kage-crawl/              frontier, sitemap, dedupe (url normalization lives in kage-core)
    kage-extract/            html → text/markdown/metadata/links/jsonld, robots parsing
    kage-api/                axum router, handlers, OpenAPI, auth, webhooks
  sidecar/
    package.json, pnpm-workspace.yaml, .nvmrc
    packages/
      contract/              zod schemas, OpenAPI emitter, shared types
      engine-core/           persona application, behavior engine, flow executor, capture, geo
      engine-chromium/       patchright launcher (own playwright pin)
      engine-firefox/        camoufox-js launcher (own playwright-core pin)
      server/                fastify app; ENGINE env selects the engine package
  policies/
    domains.yaml             per-domain policies (hot reload)
    block-signatures.yaml    vendor detection rules (hot reload)
    pacing-profiles.yaml     named pacing distributions
  deploy/
    compose.yaml, compose.observability.yaml, compose.e2e.yaml (the world of the end-to-end fetch test)
    docker/kage.Dockerfile, docker/sidecar.Dockerfile
    grafana/, otel/
  docs/                      this spec, plan, ADRs, versions, runbooks
  tests/
    fixtures/                recorded block pages, sample sites, robots files
    e2e/                     compose-based end-to-end suite
    live/                    network tests gated by KAGE_LIVE=1
  .github/workflows/         ci.yaml, nightly-live.yaml, release.yaml
```

---

## 5. Domain model

All ids are UUIDv7. Timestamps are UTC.

### Proxy

| Field | Type | Notes |
|---|---|---|
| id | uuid | |
| provider | enum | `webshare`, `static` |
| provider_ref | text | Webshare proxy id |
| host, port | text, int | |
| username, password | bytea (both ciphertext, see §19) | |
| scheme | enum | `http` (CONNECT). SOCKS5 unsupported in v1. |
| country, city, asn | text | from provider; the observed geo record in `last_health` is authoritative for personas, the provider country is the fallback |
| state | enum | `new`, `healthy`, `degraded`, `backoff`, `replacing`, `retired` |
| consecutive_failures, backoff_until, backoff_count, failing_since | int, ts, int, ts | `backoff_count` is k; `failing_since` starts the unbroken failure run |
| replacement_requested_at, replacement_reason, replacement_ticket | ts, enum (`unreachable`, `reputation`, `operator`), text | the replacement Kage requested; the monthly budget counts rows by request time |
| last_health | jsonb | failure kind, latency_ms, exit_ip, leak_detected, geo (observed record: country, city, asn, timezone), geo_mismatches, reputation_score, usage_type, reputation_checked_at, reputation_bad_lookups |
| created_at, retired_at | ts | `(provider, provider_ref)` is unique among non-retired rows only |

### Persona

| Field | Type | Notes |
|---|---|---|
| id | uuid | |
| proxy_id | uuid | 1:1: unique among the personas that are not retired, so that `regenerate` can replace one behind the same proxy |
| engine | enum | `chromium`, `firefox` |
| claimed_os | enum | `windows`, `macos`, `linux` |
| browser_major | int | from the engine's bundled browser at generation |
| seed | bytea | 32 bytes derived from the id; the fingerprint, the screen class, the position, the pacing factor, the warm-up plan and the sidecar's motor profile hang on it; never changes |
| fingerprint | jsonb | full generated fingerprint, re-applied on every session |
| user_agent, hardware_concurrency | text, int | what the generated browser sends and reports |
| wreq_profile | text nullable | wreq-util profile name; null disables the http path |
| country | text nullable | observed country of the address, the provider's while no lookup has succeeded |
| locale, languages | text, text[] | the country-carrying locale tag and the stock language list of that browser in that country (§6.1) |
| timezone | text | IANA, from the observed geo record |
| geolocation | jsonb nullable | lat/lon/accuracy near the lookup's coordinates |
| screen | jsonb | width, height, dpr, color depth |
| tables_version | text | version of the locale and screen tables the persona was drawn from |
| profile_dir | text | `<engine>/<id>` under the personas volume |
| state, cooling_reason | enum, enum nullable | `active`, `cooling`, `retired`; while cooling: `proxy_backoff`, `selftest_failed` or `geo_changed` |
| warmup_visits, warmed_at | int, ts nullable | warm-up progress; the scheduler uses a persona only once `warmed_at` is set (§6.1 step 5) |
| created_at, retired_at, retire_reason | ts, ts, text | |
| profile_archived_at, profile_archive, profile_deleted_at | ts, text, ts | the archive of the retired persona's profile directory (§6.3) |

### PersonaDomainState

| Field | Notes |
|---|---|
| persona_id, domain | PK |
| cooldown_until | set on block |
| requests_today, counter_day, last_request_at | pacing counters; `counter_day` is the calendar day in the persona's timezone that `requests_today` counts, so the cap resets at local midnight |
| last_outcome | |
| block_count_24h, block_window_started_at | blocks in the 24 hours that began with the first of them; feeds circuit breaker |

### Session

| Field | Notes |
|---|---|
| id | caller-supplied (≤ 64 chars, `[A-Za-z0-9_.-]`) or server-minted; unique per API key, primary key is `(api_key_id, id)` |
| api_key_id | owner |
| persona_id | pinned persona: the ready persona with the fewest open sessions when the session is opened; it never moves |
| created_at, last_used_at, expires_at | idle TTL default 10 min, max lifetime default 6 h (`sessions.idle_ttl`, `sessions.max_lifetime`); ended at once when the persona is retired; an id whose session has ended names a new session |

### FetchRequest (API) — see §16.3

### Attempt

| Field | Notes |
|---|---|
| id, request_id, n | |
| persona_id, engine, render | |
| started_at, finished_at | |
| status, final_url | |
| outcome, block | classifier output |
| timing | dns, connect, tls, ttfb, dom_content_loaded, load, total |

### Result

Stored per request: outcome, status, headers, final_url, redirect chain,
body reference(s), extract jsonb, xhr jsonb, artifacts list, persona summary,
attempts, robots verdict, cached flag.

### Job, JobItem, CrawlFrontier, ApiKey, AuditEvent — see §18.

---

## 6. Personas and fingerprints

### 6.1 Generation

A persona is generated when a proxy is usable (`healthy` or `degraded`) and
has none: on the pool's events, and at every sweep of the persona manager
(`kage-persona`) for whatever no event announced. Its id is new, its seed
derives from the id, and every draw below is seeded by it.

1. **Geo.** Everything derives from the observed geo record of the proxy
   (§7.4); the provider's country is the fallback only while no lookup has
   succeeded. Timezone is the lookup's, else the country's primary zone.
   The locale is the country's primary language with the country as its
   region (`de-AT`), and the language list is the one a stock installation
   of that browser ships with there, which differs by browser
   (`kage_core::persona::locale`, a table per country with overrides for
   regions that speak another language): Chrome in Austria
   `de-DE, de, en-US, en`, Firefox `de, en-US, en`. A country without an
   entry gets `en-US, en` and a health event. Geolocation is a seeded point
   0.5 to 4 km from the lookup's coordinates, or null without them.
2. **Engine and OS.** Seeded draw from `personas.mix`, weighted by what the
   pool is short of: among the entries the persona's own country has fewer
   of than its share, the one the whole pool is shortest of is likeliest,
   so that twenty proxies get 8, 9 and 3 give or take one (two at the
   most, in fewer than one pool in a hundred) and no country is all one
   browser. `engines.chromium.allow_cross_os_claims` is read at
   generation and at `kage config check`. A chromium
   persona claims the system its Chrome runs on, Linux in the shipped image
   (ADR 0009). A firefox persona claims Windows or macOS: M5 measured
   Camoufox under the same worker-inclusive coherence bar and both pass
   (§6.2), and the Camoufox 156 milestone measured both again on the 156
   build. The default mix is: chromium on linux 40%, firefox on windows
   45%, firefox on macos 15%. Firefox on linux stays possible by
   configuration but is not in the default: its fonts are Tor Browser's
   bundle, not a Linux desktop's (`docs/runbooks/engine-firefox.md`).
   The mix is configuration, and `kage persona list` reports the actual
   pool distribution. A common and coherent Chrome-on-Windows persona needs
   a real Windows engine host (risk R3), a candidate milestone after M5.
3. **Fingerprint.** Kage draws a screen class from a seeded table per
   engine and system (`kage_core::persona::screen`, version in
   `tables_version`) and asks for it by exact bounds: the generators' own
   screen data is narrow, and twenty personas must not share one screen.
   The sidecar generates it from constraints (engine, OS,
   locale, screen class, browser major equal to the engine's bundled browser)
   using fingerprint-generator (chromium) or Camoufox's generator (firefox),
   returns the full fingerprint JSON, and Kage persists it. The same JSON is
   sent back on every session open. The sidecar MUST apply it
   deterministically, and applies it as far as the engine's browser reads
   it: on firefox the keys the pinned build declares `removed` and the two
   privacy signals are left out (§10.3), so that a document generated for
   one build opens unchanged on the next.
4. **wreq profile.** Kage maps (engine, browser_major, claimed_os) to a
   wreq-util profile (`kage_http::profile::resolve`). An exact match is the
   profile of the same major, among the newest eight majors wreq-util ships
   per browser family; older profiles are not offered. A major newer than
   every profile may be served by the newest profile with `User-Agent` and
   `sec-ch-ua` written for the real major, but only once that major is in
   the verified-majors table: a capture of the real browser against the
   fingerprint echo (§9) showed the same JA4, the same HTTP/2 fingerprint
   and the same navigation header set and order. The table is empty at M3;
   M4 and M5 fill it. Otherwise `wreq_profile` is null and the persona is
   browser-only (alert `http_profile_missing`).
   `engines.chromium.browser_major` and `engines.firefox.browser_major`
   name each engine's bundled major (unset until M4 and M5); a major that
   resolves to no profile fails `kage config check` and a test.
5. **Profile directory.** Created empty. Cookies, storage and history accrue
   over the persona's life. A persona warms up by visiting a small set
   of popular neutral sites during its first hours (`personas.warmup`,
   configurable list, on by default and off in every test setup), so
   that its profile is not empty on first contact with a target: 6 to 10
   visits in its own browser through its own proxy with the behavior
   engine on, the first 3 to 20 minutes after generation and then
   log-normal gaps (median 25 min), only between 07:00 and 23:30 of its
   own time zone, one visit of the manager's per engine at a time. A
   visit counts toward the persona's state on that site; a blocked one is
   recorded, cools that site for that persona and is never repeated. The
   persona is ready for real work (`warmed_at`) after
   `personas.warmup.min_visits` (3) visits or `personas.warmup.max_wait`
   (12 h), whichever is first; with warm-up off, at once.

### 6.2 Coherence rules (tested by the self-test suite)

- UA string, Client Hints (`Sec-CH-UA*`), `navigator.userAgentData`,
  `navigator.platform`, `navigator.oscpu`, WebGL vendor and renderer, fonts,
  screen and timezone MUST all describe the same OS and browser version.
- `Accept-Language`, `navigator.languages`, `Intl` locale and timezone MUST
  agree with the proxy's country.
- TLS JA4, HTTP/2 fingerprint and header order on the http path MUST match
  the persona's claimed browser. The in-repo echo compares every mapped
  profile against its recorded fixture on every PR; a derived profile for a
  browser major that wreq-util does not ship is enabled only after a capture
  of the real browser against the same echo equals the profile's JA4, HTTP/2
  string and navigation header set and order (`VERIFIED_MAJORS` in
  `kage-http`). Until then that persona is browser-only.
- A chromium persona claims the operating system its Chrome runs on: Linux
  in the shipped image (ADR 0009). M4 measured the alternative on Chrome
  155: every way of claiming another system leaves dedicated and blob
  workers, and the `Sec-CH-UA-Platform` of their requests, saying Linux, so
  one page load shows both systems. The chromium engine is therefore
  restricted to `claimed_os = linux` (`engines.chromium.allow_cross_os_claims`
  and the sidecar's `SIDECAR_ALLOW_CROSS_OS_CLAIMS`, default false; a
  persona that claims another system is refused with
  `cross_os_claim_disabled`). The identity is made by how the browser is
  launched (display, time zone, languages, CPU affinity, WebGL through
  Mesa), with no script in any page (`inject: none`).
- The coherence bar of a chromium persona, tested on the fixture site and
  by the self-test probe: `User-Agent` and `Sec-CH-UA-Platform` on the
  navigation, an iframe navigation, a window `fetch()`, the worker script
  request and a worker `fetch()`; `navigator.userAgent`,
  `navigator.platform`, `userAgentData.platform` and the high-entropy
  values in the window, a same-origin, a `srcdoc` and an `about:blank`
  iframe, a dedicated worker and a blob worker; native getters; navigation
  header names and order equal to the recorded capture of the real browser;
  no `Pragma` or `Cache-Control` on a plain navigation; a second visit
  served from cache.
- A firefox persona claims Windows or macOS and is made inside the
  browser (Camoufox), from a configuration that is generated once from the
  persona's seed, persisted whole in the fingerprint document and applied
  unchanged on every open. M5 measured it on Camoufox 152.0.4-beta.30
  under the same bar, extended by what only measuring shows: the font
  families a line of text measures differently in, and the WebGL strings
  and parameters, read in the window, every kind of iframe and an
  `OffscreenCanvas` of a dedicated and of a blob worker. Every value is
  the claimed system's and equal in every scope; `oscpu` and `buildID`
  exist in windows only, as in every Firefox. The probe judges fonts
  (`fonts.own`, `fonts.foreign`, `coherence.fonts`), WebGL (`webgl.os`,
  `coherence.webgl`) and the screen for both engines since. The Camoufox
  156 milestone measured the bar on `v156.0.1-beta.39` with Windows and
  macOS personas and added three things that build made visible, judged
  hard for both engines: `Intl` speaks the language of
  `navigator.language` (`coherence.intl_language`), what a script reads of
  `globalPrivacyControl` and `doNotTrack` is what the navigation sent
  (`headers.navigation.gpc`, `headers.navigation.dnt`), and a page finds
  no trace of the driver (`automation.driver_worker`: a
  `new Worker("chrome://juggler/…")` throws as in stock Firefox;
  `automation.extra_globals`: no `set…` function beyond the browser's
  own).
- What surrounds the browser is part of the claim: a work area smaller
  than the screen (chromium: a desktop's top bar announced on the display
  and the window placed below it; firefox: what Firefox's baseline
  fingerprinting protection reports of any screen since the 156 build,
  the screen less a taskbar of the claimed system, 48 px on Windows and
  76 px on macOS), media
  devices and an `AudioContext` with channels (a sound server in both
  images). The probe reports their absence as warnings
  (`environment.*`), never as failures.
- Never rotate any identity attribute inside a persona's lifetime. Browser
  major may advance only when the engine image is upgraded, and then as a
  real browser's update does: at the persona's next open the engine
  refuses its document (`browser_major_mismatch`), the sidecar moves it to
  the engine's major (`POST /v1/personas/upgrade`: the browser version and
  the `User-Agent`, nothing of the device, the same answer for the same
  document), and the manager stores it with its `browser_major`,
  `user_agent` and `wreq_profile` together, records
  `persona_browser_upgraded` and opens the context as the moved persona,
  whose next self-test tests it. Its sessions, its cookies, its http jar
  and its per-domain state continue. A major never goes back: a sidecar of
  an older major refuses the moved document, and a Firefox profile does
  not start under an older major, so before its first start under the new
  one the firefox engine copies it to `.upgrade/<persona id>` on the
  personas volume, which Kage deletes after `personas.upgrade_backup_days`
  (7). The way back within that window is
  `docs/runbooks/upgrading-an-engine.md`; `--allow-downgrade` is never
  used.

### 6.3 Lifecycle

| Event | Effect |
|---|---|
| Proxy enters `backoff` or `replacing` | Persona `cooling` (`proxy_backoff`); no new sessions; existing sessions finish. Active again when the proxy is usable. |
| Proxy `retired` (replaced, by Kage or by the provider itself) | Persona `retired`: its sessions end and its http cookies are deleted at once; `personas.archive_after` (10 min) later its profile directory is moved to `.archive/<persona id>` on the personas volume with a `persona.json` (ids, engine, system, the proxy's address without credentials, reason, times), and the archive is deleted after `personas.retention_days` (default 7). The row, the fingerprint, the per-domain state and the self-test runs stay. New persona generated for the replacement proxy; it inherits nothing. |
| Geo record of the address changes | Same country: time zone and position follow, event `persona_geo_updated` at info. Another country (or a region of another language): locale and languages follow too, the event is a warning and the persona is `cooling` (`geo_changed`) until a self-test passes. Seed, fingerprint, screen and profile never change. |
| Engine image bundles a newer browser major | At the persona's next open its document is moved to that major and stored, event `persona_browser_upgraded` at info (§6.2). Seed, device, place, profile, sessions and cookies stay. A firefox profile is first copied to `.upgrade/<persona id>`; the copy is deleted after `personas.upgrade_backup_days` (7). |
| Block on domain D | `PersonaDomainState.cooldown_until = now + cooldown(D, block_type)`; defaults: `rate_limited` 2 h, `forbidden`/`captcha`/`challenge_failed` 24 h, `soft_block` 6 h. A rule may carry a cooldown class instead: `none`, `short` (policy key `short`, default 30 min, never longer than the outcome's own cooldown) or `long` (the outcome's cooldown). M7 scales the duration by the classification confidence. |
| Daily cap reached on D | Persona ineligible for D until local midnight in the persona's timezone. |
| Self-test failure | Persona `cooling` (`selftest_failed`) until a self-test passes: the next scheduled one (retried after a twelfth of the interval) or an operator's `kage persona selftest`. A run that could not read its test site is inconclusive and changes nothing. |
| Operator | `kage persona retire` retires a persona; `kage persona regenerate` replaces it with a new one behind the same proxy. |

### 6.4 State export and import (http ↔ browser)

After a browser attempt, the sidecar can export cookies and the relevant
storage for the origin. Kage stores them on the persona and loads cookies
into that persona's wreq jar. The http path MAY reuse them only when
`wreq_profile` is non-null and the persona's engine major equals the profile
major. Cookies set by the http path are imported back into the browser
context before the next browser attempt for that origin.

Each persona has its own http jar (table `http_cookies`, keyed by proxy and
persona, §9). It dies with the persona and with the proxy: a replacement
never inherits cookies, because a new address presenting the old cookies
links the two. Both directions of the handoff
(`PersonaManager::browser_to_http`, `http_to_browser`) are refused when the
persona has no profile or its stored profile is not the one its engine
major resolves to; only the cookies a browser would send to the origin
move.

---

## 7. Proxy pool

### 7.1 Provider trait

```rust
#[async_trait]
pub trait ProxyProvider: Send + Sync {
    fn kind(&self) -> ProviderKind;                    // the `provider` column
    async fn list(&self) -> Result<Vec<ProviderProxy>>;
    async fn replace(&self, want: ReplaceSpec) -> Result<ReplaceTicket>;   // address + country
    async fn replace_status(&self, ticket: &ReplaceTicket) -> Result<ReplaceState>;
    async fn replaced(&self, since: DateTime<Utc>) -> Result<Vec<ProxyReplacement>>; // the replaced-proxy log
    async fn account(&self) -> Result<AccountInfo>;   // subscription + plan + config flags + stock
    async fn stats(&self, since: DateTime<Utc>) -> Result<Vec<ProxyActivity>>;
}
```

Implementations: `WebshareProvider` and `StaticListProvider`. The static
list is a secret (`KAGE__PROXIES__STATIC_LIST`, env or `_FILE`, never the
config file) because it holds passwords; it accepts `host:port:user:pass[:country]`
lines or a YAML document with a `proxies:` list. `proxies.provider` is
`auto | webshare | static | none`; `auto` picks Webshare when its key is
set, else the static list, else none. Without a provider `kage serve` still
starts, warns, and keeps health-checking stored proxies (a leak must not go
unseen because a key went missing) but never syncs or replaces.

Since 2.0 (ADR 0013, M15): the trait has `capabilities()`, and
`replace`, `replaced`, `account` and `stats` are optional (a provider
that lacks one answers `Unsupported`, which the pool, the CLI and the
health page show as such); a provider is one module under
`crates/kage-proxy/src/provider/`, its configuration section, its pins,
and a pass of the conformance suite
(`crates/kage-proxy/tests/provider_conformance.rs`) against a mock of
its API, as `CONTRIBUTING.md` describes. A static entry carries
`country` and may carry `locale` (BCP 47) and `city`; a persona is
generated from the declared values as from an observed geo record
(§6.1), `locale` overriding the country's primary language; an entry
without a country gets no persona and serves level 0 only (§25), with a
warning at start and in `kage config check`. Where a geo lookup is
possible (§7.4) it verifies the declaration: a mismatch on two
consecutive lookups is `geo_mismatch`, the proxy `degraded` with an
alert, the declared identity kept until the operator resolves it.

### 7.2 Webshare specifics

Verified against <https://apidocs.webshare.io> on 2026-10-08 (M2). The API
root is `https://proxy.webshare.io/api/`; endpoints live under `v2/` or
`v3/`, and the root is configuration (`webshare.base_url`). Every request
carries `Authorization: Token …`.

| Purpose | Method and path | Notes |
|---|---|---|
| Proxy list | `GET v2/proxy/list/` | `mode=direct` is required (`backbone` is for rotating plans, whose proxies have no address); paging with `page` and `page_size`. The country filter is `country_code__in` (comma-separated); Kage does not use it, it reads the whole list. |
| Request a replacement | `POST v3/proxy/replace/` | Body `{"to_replace": {"type": "ip_address", "ip_addresses": [ip]}, "replace_with": [{"type": "country", "country_code": cc, "count": 1}], "dry_run": false}`. Errors as HTTP 400 with a code: `not_enough_replacements_in_subscription`, `active_replacement` (one at a time), `invalid`. |
| Replacement status | `GET v3/proxy/replace/{id}/` | `state` is `validating`, `validated`, `processing`, `completed` or `failed`. |
| Replaced proxies | `GET v2/proxy/list/replaced/` | Old and new address, time, and `reason`: `proxy_replaced` for an API request, `auto_invalidated`, `auto_out_of_rotation`, `auto_low_country_confidence`, `auto_site_check`, `auto_deleted` or `list_updated` when Webshare did it. |
| Subscription | `GET v2/subscription/` | Names the active plan; term end, paused, throttled. |
| Plan | `GET v2/subscription/plan/{id}/` | Proxy count and countries, `proxy_replacements_total/used/available`. |
| Proxy config | `GET v3/proxy/config?plan_id=` | The four `auto_replace_*` settings. |
| List stats | `GET v3/proxy/list/stats?plan_id=` | `available_countries`: what a replacement can draw from. |
| Hourly traffic | `GET v2/stats/?timestamp__gte=&timestamp__lte=` | Not paged; at most 90 days back. |

`plan_id` is an optional query parameter on the `v2` endpoints and required
on the `v3` config and stats endpoints; Kage uses `webshare.plan_id` when
set, else the plan the subscription names.

Observed against the live API on 2026-10-09 (M2 live check, 20 of 20
proxies passed): each listed proxy also carries `asn_name`, `asn_number`
(number) and `high_country_confidence`, which Kage reads where useful; the
plan carries `high_quality_ips_only` (the High IP Reputation add-on) and a
30-day subscription term with start and end; no response carries a
rate-limit header, so limits are learned only from a 429; the proxy config
response includes `proxy_list_download_token`, a secret Kage never reads or
logs; one credential pair serves every proxy on the plan. Webshare counts
manual replacements per subscription term, not per calendar month: when the
plan reports a term, Kage's budget window follows that term
(`Pool::budget_window`: the term that contains now, rolled forward by whole
terms when the reported start is stale), and the UTC month otherwise.

- Rate limits: general 240/min, proxy list endpoints 60/min; after a 429
  Webshare asks to wait 60 s. The client keeps its own token buckets below
  both (`webshare.requests_per_minute` 120, `webshare.list_requests_per_minute`
  30), honors `Retry-After`, and never sends a request that changes
  something twice after a failure that may have applied it.
- The Webshare client MUST be built on wreq (no second TLS stack). It is
  plain wreq without browser emulation and sends `User-Agent: kage/<version>`:
  Webshare is a vendor, not a target.
- Webshare replaces proxies by itself unless the four `auto_replace_*`
  settings are off. Kage reports them and alerts when one is on that
  `webshare.accepted_auto_replace` does not name; it never
  changes account settings. The owner switches the others off in the
  dashboard, so that an address (and its persona) only changes when Kage
  decided it. The default accepts `auto_replace_invalid_proxies` (D31):
  while it is on Kage records `provider_auto_replace_accepted` at info, and
  a swap Webshare made for that reason is recorded as
  `provider_replaced_proxy` at info instead of a warning. Whoever replaced a
  proxy, the path is the same: the old proxy is retired, its persona with
  it, and the new address gets a new persona (§6.3).
- Sync: the pool reconciles with the list every `proxies.sync_interval`
  (default 10 min): new IPs → `new`. A missing IP is `retired` at once only
  when the replaced-proxy list confirms it, otherwise after
  `proxies.missing_syncs` (default 3) syncs in a row without it, and never on
  the word of a list that is empty or shorter than half the pool.

### 7.3 Pool state machine

```
new ──health ok──▶ healthy ──failure──▶ degraded ──N failures──▶ backoff ──expiry──▶ degraded
                     ▲                                              │
                     └──────────── health ok ───────────────────────┘
backoff ──M cycles AND replace_after of unbroken failure, or bad reputation──▶ replacing ──provider done──▶ retired
new | healthy ──leak, or N failures before a first success──▶ backoff
new | healthy | degraded ──bad reputation, or operator──▶ replacing
replacing ──provider failed or refused──▶ backoff
new | healthy | degraded | backoff ──gone from the provider's list──▶ retired
```

The complete list is `ALLOWED_TRANSITIONS` in `kage-proxy/src/machine.rs`
(17 edges); property tests prove no event sequence leaves it. Defaults:
N = 3 failures (a failure counts only when the primary echo and the
confirmation echo both fail), backoff = 1 min × 2^k capped at 1 h and
jittered into `[0.5 d, d]`, M = 3 consecutive backoff cycles AND
`proxies.replace_after` (default 6 h) of unbroken failure. A proxy leaving
backoff is probed at once; success resets everything, failure returns it
with k + 1.

Leak (exit IP equals host IP): `backoff` from any state, critical alert,
never counts toward replacement, and sets the pool-wide `egress_unsafe`
flag. While the flag is set the scheduler (M7) MUST refuse every dispatch;
it clears when a probe through the same proxy shows a distinct exit IP, or
when an operator runs `kage proxy egress --clear`.

Pool-wide outage guard: while more than half of a live pool of 4 or more
fails its latest probe, failures are recorded but advance no counter, and
`pool_outage_suspected` fires. `pool_capacity_low` fires when a quarter or
more of such a pool is in `backoff` or `replacing`.

Replacement: budget `proxies.replacements_per_month` (default 5) counts
requests in the current budget window (the provider's subscription term
when the account reports one, else the UTC month; §7.2), automatic and
operator alike, enforced
atomically in Postgres; Kage never requests when the plan's own quota shows
0 available; the longest-failing proxy goes first; one replacement at a time
(the provider allows one); same country as the proxy replaced, else no
request and an alert. When the budget is exhausted the proxy stays in
`backoff`, keeps probing at the cap, recovers by itself if the IP returns,
and `replacement_budget_exhausted` fires once. Bad reputation with no
budget leaves the proxy usable as `degraded` with an alert.

Retirement from the provider list: at once only when the replaced-proxy log
confirms it; otherwise after `proxies.missing_syncs` (default 3) syncs in a
row without it; never when the list is empty or shorter than half the pool.

### 7.4 Health checks

| Check | Default interval | Method |
|---|---|---|
| Reachability, latency, exit IP | 5 min × U(0.75, 1.25) per proxy from its seed; first probes spread over 2 min after boot; at most 3 in flight; real traffic through a proxy postpones its next probe (`Pool::report_traffic_ok`) | Through-proxy GET of the provider's own echo (`https://ipv4.webshare.io/`), so no third party sees the pool; a failed probe is retried once against the confirmation echo (`https://checkip.amazonaws.com/`) before it counts. Client from the `ProbeClientFactory` seam: the client of the persona behind the proxy when it has an http path (its profile, `User-Agent`, brand list and languages, on a fresh connection without its cookies), else one wreq-util profile (`proxies.health.profile`, default `chrome_149` on Windows). |
| Leak | with reachability | Exit IP (from echo) ≠ host public IP; host IP learned at boot by a direct GET of the same echo, refreshed hourly. Leak → `backoff` + `egress_unsafe` (§7.3). |
| Geo (country, city, ASN, timezone) | first check of a `new` proxy, on exit IP change, and every 24 h jittered | `https://ipinfo.io/json` through the proxy (not Cloudflare-fronted). The observed record is persisted as authoritative; a change is a health event. Mismatch with the provider country on two consecutive lookups → `degraded` and alert, proxy stays usable. Lookup failures are not health failures (retry after an hour). |
| Fingerprint self-test (http) | per PR in CI over loopback for every mapped profile; on demand (`kage selftest http --proxy --profile`) and two proxies per nightly live run against `selftest.http.third_party_url`; every 24 h × U(0.75, 1.25) per persona from its seed (`selftest.http.interval`; first run 2 to 30 min after generation; `kage selftest http --persona`, `kage persona selftest`), only against the operator's own echo (`selftest.http.echo_url`) and never against a third party on a timer; without an echo of the operator's nothing is scheduled | JA4, HTTP/2 Akamai string, header order, HTTP version and `User-Agent` equal the fixture recorded for the profile. The fingerprint is a function of the pins and the header code, not of the proxy, so the live runs only confirm that a real tunnel changes nothing. |
| Fingerprint self-test (browser) | per PR on the fixture site's coherence probe; on demand (`kage selftest browser`) and two proxies per nightly live run against the third-party pages; every 24 h × U(0.75, 1.25) per persona from its seed (`selftest.browser.interval`), in the persona's own context, only against the operator's own pages (`selftest.browser.pages`: the probe and, when configured, the fingerprint echo), never a third party on a timer; without such pages nothing is scheduled. Every run is a row in `selftest_runs` | The sidecar loads the page list given in the request. Default: our own probe and echo. Third parties (`bot.sannysoft.com`, CreepJS) only on demand and in the live suite; BrowserScan and Pixelscan never in a default. Findings `{page, check, status, observed, expected, hard}`; hard failures (any automation flag, an identity mismatch against the persona, a transport mismatch on the echo) fail the run; score `100 × (1 − (fails + ½ warns) / known)` with `min_score` 90; an unreachable page yields `unknown`, changes no score and sets `complete: false`, so a test-site outage never cools a persona down. Page text and a screenshot are stored as artifacts. |
| IP reputation | 24 h, one proxy at a time spread over the day, asked from the host directly | Provider trait with AbuseIPDB first; disabled without a key. Bad = `abuseConfidenceScore` ≥ `reputation.bad_score` (default 75) on two consecutive lookups → `replacing` when the budget allows, else `degraded` + alert. `usageType` recorded; Data Center or Hosting on a residential IP raises `reputation_usage_type_suspicious`, never an automatic replacement. A 429 pauses lookups for as long as it says, or 24 h. |
| Account sync | 10 min | Subscription, plan, counts, replacement quota, the four `auto_replace_*` flags (alert while any is on, never changed by Kage), available countries. |

All health traffic obeys pacing and never targets real target sites.

Since 2.0 (ADR 0013): `proxies.health.echo_url` names the page the
reachability probe fetches and the leak check compares with. For a
provider with an echo of its own (Webshare) it defaults to that echo;
for a declared list it defaults to the public echo above
(`https://checkip.amazonaws.com/`), whose operator then sees every proxy
address on the probe's schedule, as the runbook says; the operator's own
echo (`selftest.http.echo_url`, O5) is the private alternative. The
confirmation echo stays as it is.

### 7.5 The forwarder and the bandwidth budget

**Forwarder (follow-up F10).** `kage serve` runs one HTTP proxy of its own
(`forwarder.bind`, default `0.0.0.0:8899`; `forwarder.advertise`, default
`http://kage:8899`, is what the sidecars are told). Every persona's browser
context and http client is given the forwarder as its proxy, under a
credential that only names the persona: its id as the user name and a
token derived from the master key (HMAC-SHA-256, 128 bits) as the password,
the same after a restart. The forwarder then

- looks up that persona's own proxy and connects to it and nowhere else,
  adding the provider's credentials, so those never enter a browser or a
  sidecar container. A persona that is unknown, cooling or retired, or
  whose proxy is not usable, is refused (`PoolRoutes`, cached for 5 s);
- relays `CONNECT host:port` and absolute-form `http://` requests only,
  and refuses every target the SSRF guard refuses by name or by range
  (loopback, private, link-local, metadata, single-label and local names,
  port 0, credentials in the URL). A public address by number is relayed:
  whether a request may *name* one is decided where requests are accepted
  (`security.allow_ip_targets`). The operator's own self-test hosts
  (`selftest.browser.pages`, `selftest.http.echo_url`) pass by name and
  port;
- counts every byte in both directions per persona and per domain being
  fetched (the attempt in flight for that persona; `(idle)` outside one);
- relays no new connection while a leak is on record (`egress_unsafe`) or
  the bandwidth budget is used up, and the open browsers are closed when
  such a stop begins.

A refusal looks like a network that does not answer: a refused tunnel gets
a bare `403`, a refused plain request is closed without an answer, so no
page ever reads a response Kage wrote. A missing or wrong credential gets
`407`, as from any proxy. Health probes do not pass the forwarder: they
present the provider's credentials themselves. A CLI command that opens a
context in its own process (`kage fetch`, `kage persona selftest`, `kage
selftest http|browser --persona`) binds a forwarder of its own on a free
port. The stand-in self-tests of a proxy (`kage selftest http|browser
--proxy`) have no persona the forwarder could name and hand the sidecar
the provider's proxy: an operator's command, on demand, against the pages
it names (the one exception; `docs/security-review.md`).

**Bandwidth budget (decision D5).** The counts are written to
`bandwidth_usage` by the hour (`bandwidth.flush_interval`, default 30 s).
The budget is `bandwidth.monthly_cap_gb` (default 250) per billing term:
the provider's subscription term once the account was read, else the UTC
calendar month. It is judged on the larger of Kage's own count and the
provider's (`stats`, asked at most every ten minutes), because the provider
also bills what does not pass the forwarder. At `bandwidth.warn_at`
(default 0.8) `bandwidth_budget_warning` is raised once per term with the
day the cap would be reached at the rate so far. At the cap
`bandwidth_budget_exhausted` is raised once per term and, with
`bandwidth.hard_stop` (default on), nothing is fetched until the term ends
or the cap is raised: the scheduler answers every request with
`provider_error` and reason `bandwidth_exhausted`, the forwarder relays
nothing new, the open browsers are closed, and the manager's warm-up
visits and scheduled self-tests wait. An operator's own
`kage persona selftest` is not held by it.

---

## 8. Fetch pipeline

### 8.1 Render decision

```
request.render == http | browser   → as requested (http requires wreq_profile)
request.render == auto             → policy.render if set, else:
                                     domain known lenient (policy)   → http
                                     otherwise                        → browser
escalation: an http attempt with outcome in {challenge_*, captcha, forbidden, soft_block}
            and render == auto → next attempt uses browser
```

Since 2.0 the request's `level` (§25) comes first: level 0 is the plain
client and no persona, level 1 is `render: http` with the persona's
verified profile, levels 2 and 3 are `render: browser` (2 a plain load,
3 the behaviour engine); `render` is derived from the level and is no
longer a request field, and escalation from level 1 to 2 under `auto`
happens only when the request's level allows it (a level is a maximum
the caller chose as much as a minimum the policy set).

### 8.2 Persona selection score

Eligible = `active` and warmed (`Persona::is_ready()`), proxy `healthy` or
`degraded`, not cooling for domain, daily cap not reached, engine ∈
policy.engines, country ∈ policy.countries (if set), for http:
`wreq_profile` non-null. A persona already visiting the domain keeps its
work while its visit lasts (§11); the score chooses a new visitor only
when work waits and fewer than `domain_concurrency` personas are visiting.
Score = country preference match (+3) + no visit to this domain in the last
hour (+2) + low load (+1) + previous `ok` on this domain (+1); an awake
persona is always taken before a sleeping one (`active_hours: prefer`);
after a block, never the blocked persona's /24 while another is eligible
and minus 2 for its ASN. Ties seeded.

### 8.3 Retry policy

`max_retries` default 2 (3 attempts). Retryable outcomes: `rate_limited`,
`challenge_failed`, `captcha`, `forbidden`, `soft_block`, `provider_error`,
`timeout`. Each retry after a refusal or a browser block uses a different
persona, after a seeded pause (shifted log-normal, at least 2 s, median
6 s, p95 20 s, scaled by the persona's factor); the one exception is the
same persona's single browser escalation of a curable http block (§3.2).
Cooldown = `Cooldowns::resolve(outcome, class)` × confidence, at least
5 min, ±10 % seeded per persona. Non-retryable: `ssrf_denied`,
`robots_denied`, 4xx other than 403/429 (returned as `ok` with the
status). Three 5xx or timeouts in a row from one domain pause it for 30 s,
doubling to 10 min, with no cooldown and no breaker count. Circuit breaker
per domain: if `block_count_1h` over all personas ≥
`policy.breaker_threshold` (default 5), the domain is `paused` for
`policy.breaker_pause` (default 30 min); requests return `provider_error`
with `reason: domain_paused` immediately. The breaker is half-open: when
the pause ends one trial request goes through on one persona; success
closes it, a block reopens it with the pause doubled (at most 6 h) and a
success resets the length. Blocks of real work and warm-up count; server
errors, an http block cured by its own escalation, a passed challenge and
self-tests never do.

### 8.4 Timeouts

Per attempt: `timeout_ms` default 60 000 for browser, 30 000 for http; hard
cap 300 000. On the browser path it is the time to **reach** the page (the
navigation, its waits, the challenge wait) and bounds nothing after it: how
long the persona stays is the patience drawn for the page and its dwell
(§10.4, F44), and the attempt is waited for as long as those three and a
margin of 30 s add up to, a bound of its own for every page. Before M10 the
scrolling shared `timeout_ms`, and every page longer than that was left at
the same instant: 56.0 s after it was asked for, six times of six, in both
browsers. Sync `/v1/fetch` total wall time default 180 s, after which the
request is carried on as a job and `202` returned with the job id: a batch
of one item under the request's own id (`origin: sync_overflow`), held by
the server that accepted the fetch while it runs. From then on the fetch
waits where a job waits (a paused site, a persona that sleeps, the
bandwidth term) instead of ending with an error, and a server that stops
gives the job back: after a restart the job worker takes the request up
again under the same id, with the attempts that were made in its record
and counted against its retries. The result is at `GET /v1/results/{id}`
either way (F35).

---

## 9. HTTP path (wreq)

- **Clients.** `kage-http` builds one `wreq::Client` per (proxy, profile,
  claimed OS) from a wreq-util profile, with that proxy (`Proxy::all` with
  credentials) as its only route, HTTP/2 preferred and HTTP/3 disabled.
  wreq's redirect handling, cookie store and default-header merge are off:
  kage-http does all three itself. A client is asked for with a
  profile name, an OS and a `ProxyEndpoint`; the persona manager asks with
  the persona (`kage_persona::identity::http_spec`) and
  passes its exact `User-Agent` and `sec-ch-ua` through the identity
  override, which API callers cannot reach. A Firefox profile that claims
  Linux is refused without that override, because wreq-util's own value
  names a distribution.
- **Headers.** wreq-util supplies TLS, the HTTP/2 settings and the base
  values (`User-Agent`, client hints, `Accept`, `Accept-Encoding`);
  kage-http owns the complete ordered header list. One table per browser
  family (Chrome, Firefox) and request kind (document, style, script, image)
  fixes the names and their order. On HTTP/2 they go out in lower case, on
  HTTP/1.1 in the browser's spelling with `Connection: keep-alive`. A plain
  `http://` origin is treated as the browser treats it: no client hints, no
  fetch metadata, `Accept-Encoding: gzip, deflate`. `Accept-Language` is
  written in the family's own format from the client's language list
  (the persona's; `en-US, en` for a client without one).
- **Caller headers** are checked against a denylist, and a denied header is
  a typed error that names it, never a silent drop. Denied: `User-Agent`,
  `Accept-Language`, `Accept-Encoding`, `Cookie`, `Origin`, every `Sec-*`
  (so `Sec-CH-*` and `Sec-Fetch-*`), every `Proxy-*` and
  `Access-Control-Request-*`, `Upgrade-Insecure-Requests`, `Priority`,
  `Host`, and the connection and framing headers. An allowed header takes
  the slot the browser family gives it; a name the tables do not know goes
  just before `cookie`. At most 64.
- **Fetch metadata.** A typed navigation sends `Dest: document`, `Mode:
  navigate`, `Site: none`, `User: ?1`. A request with a `Referer` sends
  `Site: same-origin`, `same-site` or `cross-site`, schemeful and decided by
  the Public Suffix List; over a redirect chain the furthest hop decides.
  The `Referer` that goes out follows `strict-origin-when-cross-origin`. A
  request is a navigation (GET, HEAD or POST), one of the subresource
  kinds below, or a script's request.
- **A script's request** (`request_kind: fetch | xhr` of the API, F6): a
  `fetch()` or an `XMLHttpRequest` goes out as Chrome 155 and Firefox 152
  and 156 were captured sending one (`tests/fixtures/fingerprints/browser/`): its
  own header order, `Dest: empty`, `Mode: cors`, the `Accept` of a script
  (`*/*`), Chrome's `Priority: u=1, i`, Firefox's `u=4` for a fetch and
  none for an XHR, no `Upgrade-Insecure-Requests` and no `Sec-Fetch-User`.
  It names the page that makes it as its `Referer`, which the caller must
  give: Kage never invents the page. `Origin` goes out when the method is
  not GET or HEAD or the target is another origin, and is `null` after a
  redirect chain that crossed origins twice. Cookies go to the page's own
  origin only (credentials `same-origin`, the default of `fetch()`).
  A request a browser would preflight is refused: preflights are not
  emulated. Of a caller's headers only those with a captured place are
  taken (`Referer`, `Accept`, `Content-Type`): Chrome orders the headers
  a script sets by a hash of their names, so no capture says where an
  unknown one goes, and a wrong place is a tell. No subresources are
  emulated after a script's request.
- **Redirects** are followed by kage-http, at most `http.max_redirects`
  (default 10), every hop through the redirect chain of the SSRF guard
  (§19), which names the hop it refuses and records an `https` to `http`
  downgrade. 301 and 302 turn a POST into a GET, 303 turns everything but a
  HEAD into a GET, 307 and 308 repeat the request. Host names are resolved
  by the proxy, never by Kage; resolved addresses are checked on every hop
  only when a resolver is injected (`HopResolver`), and none is by default.
- **Cookies.** One jar per persona (§6.4; per proxy for a client without a persona), with
  the rules browsers apply (RFC 6265bis): `SameSite` with `Lax` as the
  default, `Secure`, the `__Secure-` and `__Host-` prefixes, no cookie for a
  public suffix, no replacing a `Secure` cookie from an insecure origin, 400
  days at most, 180 cookies per site and 3000 in all. The jar is stored in
  Postgres (`http_cookies`, one row per cookie), loaded when the proxy's
  first client is built and saved after every fetch that changed it. A
  retired or unknown proxy has no jar. Session cookies are stored too and
  dropped when the jar was idle for `http.session_cookie_idle` (default
  12 h).
- **Capture.** Status, final URL, HTTP version, response headers, the
  decoded body up to `http.max_body_bytes` (default 10 MB; a flag says when
  it was cut), every hop, and timing per phase: `connect_ms` (TCP to the
  proxy plus the CONNECT tunnel), `tls_ms`, `ttfb_ms`, `total_ms`. `dns_ms`
  is always null because the proxy resolves, and a request on a reused
  connection has neither `connect_ms` nor `tls_ms`. The health probe (§7.4)
  uses the same client and records both per proxy.
- **Subresource emulation** (`http.fetch_subresources`, default off): after
  a `200` HTML document fetched with GET, up to `http.subresources.max`
  (default 12) stylesheets, scripts and preloads of the document's own site,
  never another site's, in document order, at most 6 in flight, starting 5
  to 40 ms after the body arrived, then the favicon 100 to 600 ms after the
  last of them. The pauses come from a recorded seed. Each request carries
  its kind's `Sec-Fetch-Dest` and header table. An asset the client already
  holds fresh is not fetched again and a stale one is revalidated. Bodies
  are read to the end and dropped (2 MB each, 8 MB per page); a failing
  asset never fails the fetch; counts and bytes are reported.
- **Fingerprint self-test.** kage-http contains its own echo (`kage echo
  serve`; compose profile `echo`, off by default). It terminates TLS and
  reports the JA4 of the ClientHello, the Akamai HTTP/2 string and the
  request headers in wire order: the HEADERS frame on HTTP/2, the raw lines
  on HTTP/1.1. The expected values per profile and OS are files in
  `tests/fixtures/fingerprints/`. `just ci` compares every mapped profile
  with them over loopback, so a pin bump that moves a fingerprint fails on
  that change. `kage selftest http --proxy <id> --profile <name>` makes the
  same comparison through a real proxy, against `selftest.http.echo_url`
  when set and else, on demand only, against the third-party echo
  `selftest.http.third_party_url`. Kage never contacts a third-party echo on
  a timer; the scheduled check of §7.4 uses the operator's
  own echo. See `docs/runbooks/fingerprint-echo.md`.
- The http path never executes JavaScript and never pretends to; a page that
  requires it will surface as a block or as `ok` with thin content, which
  the extractor flags (`extract.thin_content = true`) so policy can learn.

---

## 10. Browser sidecar

### 10.1 Service shape

Fastify server, `ENGINE=chromium|firefox`, listens on the compose network
only, requires `X-Kage-Sidecar-Token` (shared secret, at least 24
characters, compared in constant time) on every route, health and metrics
included: the container healthcheck sends it too. Up to
`SIDECAR_MAX_CONTEXTS` concurrent persona contexts per container (default
3; scale by running more containers). Each persona context uses its own
persistent profile directory (`launchPersistentContext` or engine
equivalent) and its own authenticated proxy. On chromium a persistent
profile is a browser process of its own, so a container runs one Chrome per
open context, each on its own X display.

Opening is idempotent per persona: the same request again answers with the
open context (`reused: true`), a different request for a persona whose
context is open answers `context_conflict`, one context more than the cap
answers `context_limit`, and a second operation on a context that is busy
answers `context_busy`. Requests and responses are validated against the
contract in both directions; errors are `{code, message}` with the stable
codes of the contract.

### 10.2 Contract (v1)

OpenAPI is emitted from zod schemas in `sidecar/packages/contract` and
checked into `sidecar/packages/contract/openapi.json`; `kage-browser`
generates Rust types from it at build time and a contract test fails if the
file and the Rust types drift.

| Method and path | Purpose |
|---|---|
| `GET /v1/health` | engine, browser version, contexts in use, uptime |
| `GET /v1/metrics` | Prometheus |
| `POST /v1/personas/generate` | input: constraints; output: fingerprint JSON and summary |
| `POST /v1/personas/upgrade` | input: a stored fingerprint; output: the same persona moved to the engine's browser major (deterministic), its summary, and whether it changed (1.4.0) |
| `POST /v1/contexts` | open a persona context: persona id, fingerprint, proxy, locale, timezone, geolocation, screen, profile dir; returns context id |
| `DELETE /v1/contexts/{id}` | close, flush profile |
| `POST /v1/contexts/{id}/navigate` | url, wait, behavior, capture → page result |
| `POST /v1/contexts/{id}/flow` | actions[] → NDJSON stream of step results, final page result |
| `POST /v1/contexts/{id}/state/export` | cookies and storage for an origin |
| `POST /v1/contexts/{id}/state/import` | cookies for an origin |
| `POST /v1/contexts/{id}/selftest` | run the detection page set, return findings and score |

Page result: status and headers of the main navigation, final URL and
redirect chain (every hop with its `Location`), DOM HTML after settle, raw
main body, captured XHR/fetch responses (filtered by content-type and size),
console entries, timing, challenge signals (vendor scripts seen,
interstitial detected and whether it auto-resolved within
`challenge_wait_ms`), screenshots and PDFs as artifact references (sidecar
writes them to the shared artifacts volume, below `sidecar/<id>/`, and
returns their paths, sizes and SHA-256). Kage takes such a file only as it
is described: below `sidecar/`, with a real path that stays there, a plain
file (no link at its last step, no pipe, device or directory), at most
128 MiB, of the size and the SHA-256 the sidecar named; anything else is
a warning in the log and no artifact. A sidecar that was taken over
cannot make Kage read a file of its own host into a result, since it does
not know the content of Kage's environment or secrets (security review
S1). A navigation that fails is a page result too, with a typed `error`
(`timeout`, `navigation_failed`, `proxy_failed`, `aborted`) and whatever
was captured until then.

Captures must not change what the page experiences. Bodies come from
network events, never from request interception; the DOM is read in an
isolated world. A capture a page can notice is opt-in per request, off by
default, and named in `side_effects`: a full-page screenshot (`resize`)
and a PDF (`beforeprint`, `afterprint`).

Reading a page grants it nothing. Both drivers mark an evaluation as a
user gesture (patchright sends `userGesture: true`; the Juggler of the
Camoufox 152 build called `setHandlingUserInput` and
`notifyUserGestureActivation`), and M5 measured the result: a page nobody
touched read `navigator.userActivation` as active from its first read on,
and Chrome let an `AudioContext` run. Since M5.1:

- On chromium every read the engine makes of a page (title, text, DOM,
  selector waits, storage, the element boxes and page metrics of the
  behavior engine) is evaluated over the engine's own debugger session
  with `Runtime.evaluate`, in an isolated world of its own and without the
  gesture flag. The world is marked by a script registered for every new
  document, and every expression checks the mark and the top frame first,
  because an execution context id is reused by the next renderer process.
  The driver keeps the launch, navigation, network events, and the input
  of `human: false` steps. Its `goBack` is asked to wait for the commit
  only, and neither its wait for a load state after a restore from the
  cache nor its `waitForURL` is used: both hang there (F21). `back` and
  `wait_for url` look at the address and at `document.readyState`
  themselves.
- On firefox the pinned 156 build has upstream's fix natively (Camoufox
  pull request 779): an evaluation lifts the popup blocker through
  `driverPopupsAllowed` and grants no activation. M5.1 had backported it
  into the 152 build (ADR 0010, superseded by ADR 0011).
- The coherence probe compares `navigator.userActivation` with the trusted
  input events it received: `automation.user_activation` is a hard failure
  when the page is or has been active without an event that grants
  activation, unless it was reached from a page of its own origin (Chrome
  carries activation over a same-origin link click).
  `automation.audio_autoplay` warns for sound that may start by itself
  under the same conditions: an `AudioContext` that runs, or a browser
  that answers `allowed` to `navigator.getAutoplayPolicy()`. The engine parity suite asserts all of it on an
  untouched page at `load`, 300 ms and 4 s later, with the captures taken.

On chromium the console capture
is the browser's log domain (network and security messages); `console.*`
calls and uncaught errors are not captured, because reading them needs
`Runtime.enable`, and the result says which source was used
(`console_capture`).

The post-hoc SSRF check of the redirect chain (§19) runs in
`kage-browser` on the reported chain, with `kage-core`'s functions; a
denied hop is reported as `ssrf_denied` and the captures are dropped.

`state/export` and `state/import` speak the fields of an `http_cookies`
row (§6.4), so a cookie moves between the http path and a browser without
translation. `selftest` takes its page list from the request and defaults
to the sidecar's own pages (the coherence probe and the fingerprint echo);
third-party detection pages are loaded only when a request names them
(§7.4).

`navigate` takes an optional `behavior` object (§10.4): a seed, an entry
strategy, a scroll depth, where the dwell time comes from, and whether an
idle hand wanders. Without it a navigation sends no input at all. The
page result then carries `behavior`: the seed that was used (the
request's, or one the sidecar drew), the id of the persona's motor
profile, what the entry did, and counts of what was sent. `flow` answers
with NDJSON: one `step` line per finished step as it finishes, then one
`final` line with the page as the flow left it. What refuses a flow
before its first step is an ordinary error answer; a flow that breaks
after lines were sent ends without its final line.

The contract is at 1.4.0: everything M5 added (1.1.0) is optional in
requests and additional in responses, and so is what M9 added (1.2.0: a
flow's `behavior.pace` and the `paced_ms` of a step, §10.5) and what M10
added: `behavior.patience_ms` and what the scrolling came to
(`behavior.patience_ms`, `scroll_reached`, `scroll_ended_by` in the
result), an entry by a link of a page the persona has been reading
(`entry.from_open_page`, `entry.max_backs`, and `backs` in the entry's
report), and the links of the document (`capture.links`, `links` and
`links_truncated` in the result), each with whether a reader can see it
and whether it stands in what the site repeats on every page (§10.4).
The Camoufox 156 milestone added an endpoint (1.4.0:
`POST /v1/personas/upgrade`), which no earlier client calls.

A breaking change of the contract needs a new major of the contract
version; `just contract-check` compares the committed document with the
one on the base branch and fails otherwise.

### 10.3 Engines

**chromium.** patchright driving real Chrome stable, installed in the image
at a pinned version and started by its path (what `channel: "chrome"`
resolves to) through a wrapper that sets the persona's CPU affinity.
Headful, one Xvfb display per context sized from the persona's screen, no
viewport override, Chrome's sandbox on. The persona is made by launch
parameters: the display, `TZ`, `--lang` and `LANG`, and two preferences
merged into the profile before launch (`intl.accept_languages`, and
`webrtc.ip_handling_policy = disable_non_proxied_udp` so that no UDP
leaves outside the proxy); WebGL comes from Mesa llvmpipe through ANGLE, or
from a GPU of the host when one is mapped in (`SIDECAR_GL`). Proxy with
credentials via context options and the bypass rule `<-loopback>`, so that
loopback targets leave through the proxy as well (§19). No HTTP/3.

The fingerprint is generated with fingerprint-generator from a 64-hex seed
(its randomness replaced by a seeded generator for the duration of the
call, so the same seed gives the same document) and persisted as a small
versioned document: generator and seed, claimed OS, the screen and CPU
count drawn, the list of values that belong to the host and not to the
persona, and the injection payload when there is one. The browser version
is always read from the installed Chrome, never from the generator. With
the default `inject: none` nothing is injected. `inject: window`
(cross-OS claims, off by default, §6.2) applies fingerprint-injector's
script deterministically from the persisted document through a CDP
`User-Agent` override and one script per document.

Rules from patchright's guidance, kept by tests: no custom UA through
Playwright options, no `addInitScript` and no request interception from
application code, no `Runtime.enable`-dependent APIs.

**firefox.** Camoufox at a pinned build (`v156.0.1-beta.39`, Firefox
156.0.1, fetched by URL and checksum, never by a launcher's own download;
the engine refuses any other build and any other `omni.ja`, ADR 0011), driven through its own playwright-core, one browser per
context on its own Xvfb display, started through a wrapper that sets the
persona's CPU affinity. The fingerprint is generated from a 64-hex seed
with the parts of camoufox-js (the real-device generator under the seeded
swap, the mapping to the browser's configuration, the font list and a
graphics card of the claimed system drawn from the launcher's table) and
persisted as the complete browser configuration; opening is a pure launch
plan from that document. The plan sends the build only what it reads: the
keys its `properties.json` declares `removed` are left out, and
`navigator.doNotTrack` and `navigator.globalPrivacyControl` are never
applied (the generator draws them, and a stock Firefox sends neither
signal unless its user turns it on). Its preferences reach the build at
startup through `CAMOU_PREFS` and through the driver: the persona's
Firefox UI locale (`intl.locale.requested`, from a table per country
checked against the build's locales, so that `Intl` and the browser's own
messages speak the persona's language), WebGPU where Firefox 156 has it
(Windows, and macOS claims whose graphics card is Apple's), and the memory
cache and compression dictionaries as in stock. The launcher's `launchOptions` is not called: it
is not reproducible, points fontconfig at a directory the pinned build
does not have, and depends on ua-parser-js (ADR 0008). Language, time
zone and position come from the open request (`geoip` is never used),
`humanize` stays off (the shared behavior engine drives input), no addon
is loaded, and autoplay is Firefox's own default (the 156 build ships it;
the engine sets it as well, so that a build that allows autoplay, as the
152 build did and a page reads with `navigator.getAutoplayPolicy()`, is
still answered as Firefox answers). The persona's fontconfig
file is the build's own for the claimed system with an absolute font
directory and, for Windows and macOS claims, hinting off. The proxy with
credentials goes through the driver's option; loopback is sent through it
(`network.proxy.allow_hijacking_localhost`), and WebRTC gathers nothing
that would leave around it (`media.peerconnection.ice.proxy_only_if_behind_proxy`).
The same captures as chromium, with three differences that the contract
names or the runbook explains: the console holds the page's own calls, a
full-page screenshot has no side effect, and a PDF is printed with the
keyboard shortcut into a file, always A4 portrait.

**Fallback (risk R1).** If camoufox-js proves unstable, `engine-firefox` is
replaced by a Python package implementing the same contract with the
upstream `camoufox` launcher. The contract does not change.

### 10.4 Behavior engine (engine-core)

Input is sent through the mouse and keyboard of the context's own X
display (the XTEST extension), by one implementation for both engines, and
never through a driver: a driver's synthetic input is distinguishable (an
empty `pointerType` on Firefox, wheel jumps without the browser's own
delta and smooth scrolling, capitals without `Shift`, key holds equal to
the key interval). The display's keyboard layout is the persona's
(`setxkbmap` by locale), so `KeyboardEvent.code` relates to `key` as on
that country's keyboard.

A persona has a motor profile, drawn once from its seed
(`motor-profile-v1`, sixteen traits inside human bounds), so twenty
personas have twenty hands; a request has its own stream, from the
persona's seed and the request's seed, which the result records.

- Mouse: Bezier curves with overshoot and correction, variable velocity,
  micro-jitter, occasional idle wander; click after a short hover.
- Scroll: wheel events in chunks with reading pauses proportional to visible
  text density; sometimes scroll back up; reach `behavior.scroll_depth`, or
  stop when `behavior.patience_ms` is used up, whichever comes first. The
  result says how far down the document the page was and which of the two
  ended it (`depth`, `patience`, or `page` for a page that does not go
  further). The hand acts on a page once it draws (two animation frames,
  waited for at most 2 s, read where the page cannot see it): a browser
  hands a page its pointer moves and its wheel with its frames and loses
  the wheel turned while it draws none, which a new document on a slow
  machine does for half a second and more. A flick that moves nothing is
  turned once more after the page has drawn before the page counts as
  one that does not go further. Kage draws the patience anew for every
  page from the attempt's seed (`browser.patience`: a shifted log-normal,
  floor 6 s, median 38 s, p95 110 s, redrawn above 4 min, scaled by the
  persona's pace) and sends it with every navigation it makes: a fetch,
  an item of a batch, a page of a crawl, a warm-up visit. With it, `timeout_ms` bounds reaching the page
  only and the dwell follows in full. Nobody's request sets it (F44): a
  consumer that needs the bottom of a tall page uses a flow's `scroll`
  step.
- Dwell: `behavior.dwell_ms` drawn from a log-normal (median 6 s, cap 45 s)
  unless the caller fixes it.
- Typing: per-key delay 60–220 ms, occasional typo plus backspace when
  `behavior.typos` is on.
- Entry: the first navigation in a context for a new origin SHOULD arrive
  with a plausible `Referer` strategy from policy: `direct`, `search_engine`
  (visit the engine results page first, then click), or `none`. `none`
  and `direct` are equal on the wire: a typed navigation without a
  `Referer`. `link` loads a page to start from and clicks its link to the
  target; `search_engine` clicks the first result that leads to the
  target's host on a results URL that Kage builds from a policy template.
  A `Referer` is never forged: it is what the browser sends for the click.
  Without a link or result the outcome says so and nothing else is loaded,
  unless the request asks for the fallback to a typed navigation. No
  search template is enabled by default. With `entry.from_open_page` the
  page to start from is one the persona has been reading and nothing is
  loaded for it: its link is clicked on the page that is open, or after
  the persona went back to it with the keyboard, at most `max_backs`
  entries of its tab (the browser's own way back: no request when the
  browser kept the page); the report says how many (`backs`). A tab that
  does not hold the page, or a page without the link, and `fallback`
  decides. This is how a crawl reaches its next page (§13, F28).
  Every entry clicks as a reader does, on a page it loaded or on the page
  that is open (F48): only a link a reader can see (as `capture.links`
  says it) that leads on in this tab (no `download` attribute, no other
  window), aimed at the largest of its boxes (a link over two lines is
  clicked on one of them), and not clicked when the pointer comes to rest
  on something that lies over it. The times of the result count from the
  click. A click the browser answers by saving a file is no page: the
  result is `aborted` with no entry report, and nothing is typed after it.
  A page that answers the click itself (`history.pushState`, a
  client-routed site) asks for no document: the wait for one ends about
  1.5 s after the click, the outcome is `link_not_found`, and with
  `fallback: none` the address is typed. On such a site every page costs
  a click and a typed load; this is known and not solved in 1.0. Not
  seen as hidden: a link clipped by an ancestor's `overflow`; not found
  at all: links inside shadow roots.
- Links: with `capture.links` the result lists the links of the document
  to `http` and `https` addresses in document order, read in the isolated
  world like the document, each with `visible` (rendered, with a box, on
  the canvas of the document: not `display: none`, not `visibility:
  hidden`, not fully transparent, nor inside anything that is, nor moved
  off the page) and `furniture` (inside `nav`, `header`, `footer` or
  `aside`, or an element with the ARIA role of one). A link that is not
  visible is one nobody can click.
- All randomness seeded per persona and request for reproducibility in tests.
- The sidecar does nothing unless asked: a navigation without `behavior`
  is a bare page load. Kage's domain policy for a browser render MUST ask
  for human behavior (a human dwell, a scroll depth around 0.6 to 0.9,
  wander on); the scheduler and the API apply it (M7, M8).

### 10.5 Flow actions (v1 DSL)

`goto`, `click`, `type`, `press`, `scroll`, `wait_for` (selector | url |
network_idle | ms), `select`, `hover`, `back`, `extract` (capture point),
`screenshot`, `pdf`, `assert` (selector exists | text contains). Every
action has an optional `human: true|false` (default true) and `timeout_ms`.
Each step reports success, duration and a capture when requested. A human
`scroll` goes on as long as its step has time, less two seconds for the
flick that is under way: a page too long for the step is scrolled as far as
that and the step succeeds (a longer `timeout_ms` scrolls further). A human
step is done by the behavior engine through the display: a `select` is
opened with a click and chosen with the arrow keys and `Enter`, `back` is
`Alt` and the left arrow. A step with `human: false` is done by the
driver and is visible to a page as such. The first step that fails ends
the flow; a flow's `behavior` gives the seed and whether typing makes
typos.

**Pacing (contract 1.2.0).** With `behavior.pace` a flow paces itself the
way a person does. After every step that arrived on a page (a `goto`, or a
`click`, `press` or `back` after which a new document or another address
was seen) the persona dwells there with a wandering pointer: around the
median `pace.dwell_ms` when it is given (log-normal, the 95th percentile
at twice the median), the persona's own human dwell otherwise, and never
below `pace.min_dwell_ms` (a shorter draw is drawn again, so nothing piles
up at the floor). Before every step that acts on a page the persona is
already on it takes a thinking time of its own (log-normal, a median of
0.4 to 0.9 s that is the persona's, 80 ms to 5 s). The five steps that only
read (`wait_for`, `assert`, `extract`, `screenshot`, `pdf`) are never
delayed and send no input. Every time comes from the flow's seed, so a
flow replays; a pause never outlasts the flow's budget less a reserve for
the captures, and counts in its step's `duration_ms` and `paced_ms`, not
against the step's `timeout_ms`. Measured on Chrome 155 and Camoufox 152:
without pacing the first click came 0.95 to 1.19 s after the load and
every click was followed by a constant 1.2 s; with it the first click
comes about 6 s after the load and no two gaps are alike. Without `pace` a
flow runs its steps one right after the other; that is for the sidecar's
own tests. **Kage always sends `pace`** for a flow of the API, with the
policy's `min_dwell` as the floor and the request's `behavior.dwell_ms`
(never below that floor, ignored where the policy locks behavior) as the
median.

**Going back (F21, F38).** `back` waits for the address, the response or
the document to change, whoever went back: the driver, after its commit,
or the persona's keys. The page itself says which document it shows, so a
way back to an entry with the address of the page that was left is seen as
one. With no history the step fails after a second ("there is no page to
go back to") instead of reporting success; keys that made the browser ask
for the page before are waited for as long as the step has. `wait_for url`
matches the address with a matcher of its own (`**` anything, `*`
anything but a `/`; the fragment only when the pattern names one) and
then waits for the document to be complete. A capture after `back`
reports the document that came back, with the status and headers it was
first loaded with when the flow saw it arrive.

**A flow in Kage** is one browser attempt of one persona under one grant
of the scheduler (`POST /v1/jobs` with `type: flow`, §16.6). It starts
with a `goto`; every `goto` passes the SSRF guard when the job is
accepted, and every page a step captured and the page the flow ended on
pass the redirect-chain check of §19 afterwards: one refused hop and
nothing of the flow is kept. A `human: false` step is refused
(`driver_input_denied`) unless the site's policy has `driver_input: true`.
A wall met inside a flow is judged as a wall: the persona cools down, its
window is closed, and another persona runs the script again from its first
step inside the retry budget. A step that failed on a page that is no wall
is the script's failure (`flow_step_failed`) and is not tried again,
unless the page did not arrive at all (a timeout, a failed connection). A
flow in a session is never moved to another persona.

### 10.6 Resource rules

`shm_size` ≥ 1 GB for Chromium, memory and process limits per container
(`pids_limit` 4096 for chromium: three browsers are a few hundred threads
each), hard context cap. Kage owns the closing of a persona's context
(`kage_persona::Contexts`, follow-up F27): it is taken as an exclusive
lease by whatever works with it (an attempt, a warm-up visit, a
self-test), stays open when the lease is given back so that the persona's
next page finds its session, and is closed after a block, when it was not
used for `scheduler.context_idle` (default 5 min), when another persona
needs the engine's slot and none is free (the least recently used context
that is not held, one whose persona is not in the middle of a visit before
one whose persona is), and at a stop (leak, bandwidth cap) and at shutdown.
The sidecar's own `SIDECAR_CONTEXT_IDLE_MS` (compose default 6 min) is the
backstop for a context Kage lost track of; a context it closed is reopened
on demand (profile persists). The manager's warm-up visits and scheduled
self-tests ask the scheduler for the persona and a browser session before
they take a lease (`BrowserGate`): they come after synchronous requests
and jobs, and leave the last free session to a request that waits for one.
An operator's self-test counts as a synchronous request. All of them close
a context only when they opened it. A browser process is recycled after
`SIDECAR_MAX_PAGES_PER_BROWSER` (default 500) navigations: the context is
reopened from the same request before the next operation, which the
metrics count. A context whose browser died is closed and reported, never
left in the table.

---

## 11. Pacing and scheduling

Pacing profiles live in `policies/pacing-profiles.yaml`:

```yaml
human:
  gap_ms:        { dist: lognormal, median: 4500, p95: 15000, min: 1500 }
  rest_ms:       { dist: lognormal, median: 2200, p95: 7000, min: 700 }
  burst_pages:   { min: 4, max: 14 }
  burst_break_ms: { dist: lognormal, median: 60000, p95: 240000 }
  daily_cap_per_persona_per_domain: 300
  domain_concurrency: 3
  domain_rpm: 20
fast:
  gap_ms:        { dist: uniform, min: 300, max: 1200 }
  burst_pages:   { min: 50, max: 50 }
  burst_break_ms: { dist: uniform, min: 0, max: 0 }
  daily_cap_per_persona_per_domain: 5000
  domain_concurrency: 10
  domain_rpm: 300
```

A `lognormal` with `min` is a shifted log-normal: `min` plus a log-normal
fitted so the configured median and p95 hold exactly, density zero at `min`
and rising smoothly (no spike, no cliff). `max` is enforced by bounded
resampling, never clamping; samples are whole milliseconds. The sampler takes
its parameters as a value plus a seed, and median and p95 are scaled
per persona by a fixed factor from the persona seed, log-uniform in
0.8 to 1.25 (`kage_core::persona::pacing_factor`,
`PacingProfile::for_persona`; M7 applies it), so that
twenty personas never share one identical gap distribution. A wait that ends
a burst is `gap_ms` plus `burst_break_ms`, so the gap floor holds everywhere.

Resolution order: request `pacing` → domain policy `pacing` → global default
`human`. A domain policy may set `pacing_locked: true`, which makes the
request value ignored (the response reports `pacing.applied`).
`domain_concurrency` and `domain_rpm` hold on the scheduler's own clock:
never more than `domain_rpm` starts within a minute as Kage dispatches
them. A site can see a request or two more within a sliding minute,
because between the start and the request a persona types the address
and loads the page in a time of its own (measured in M12's load test at
20 a minute: 21 starts in a row never within 60 s at Kage, within 57.3 s
at the site). Global caps:
`scheduler.max_browser_sessions` (default 6), `scheduler.max_http_inflight`
(default 20). The scheduler is a priority queue (sync requests and an
operator's self-test, then jobs, then the manager's own warm-up and
self-test work, FIFO within) with per-domain and per-persona gates; it
never busy-waits.

**Visits.** A persona that fetched a page from a domain keeps that
domain's work for `visit_bursts` bursts (default 2 to 5), then leaves for
`visit_break_ms` (log-normal, median 25 min, p95 90 min, in place of the
last burst break). Other personas join only while fewer than
`domain_concurrency` are visiting, so a site sees a few households
reading, not twenty addresses taking turns. A sync request never waits for
a visitor's break; a session-pinned request waits for its persona. On the
browser path the gap counts from the start of the previous page and is a
floor under the behaviour engine's own time on the page; a page is dwelt on
once. A persona's browser stays open for its whole visit, breaks included,
and `scheduler.context_idle` (5 min) after its last page; Kage closes it
(the sidecar's idle limit, a minute behind, is the backstop).

**The rest between two pages (F43).** The gap counts from the start of a
page, so a page that was read for longer than the gap would be followed by
the next one the instant the persona is done: measured at 1 to 3 ms after
the result came back, in both browsers. A profile with `rest_ms` puts a
seeded pause between the end of a page and the request for the next one
of the same sitting (`human`: floor 700 ms, median 2.2 s, p95 7 s, scaled
by the persona's pace; `fast` has none): the next page is not asked for
before the later of the gap from the start and the rest from the end. It
is the planner's (`kage_core::schedule`), drawn from a stream of its own,
so the gaps of a persona are the same with and without it.

**A persona's day.** Each persona has its own waking hours from its seed
(up 06:30 to 08:30, to bed 22:30 to 00:30 local time, drifting up to 20
min day to day). `active_hours: prefer` (the default) takes an awake
persona first, never wakes a sleeper for a job, and lets a sync request
wake one for a single page; `enforce` fetches nothing at night; `ignore`
ignores the hour.

**Never in lockstep.** Every sampler is seeded from the persona, the domain
and the visit number and scaled by the persona's factor; nothing dispatches
on a shared timer; starts that a persona's own slot did not time are kept
apart from the previous such start of another persona by a seeded pause
(at least 150 ms, median 700 ms, p95 2.5 s).

---

## 12. Block detection and classification

Signatures are data (`policies/block-signatures.yaml`) with tests against
recorded fixtures in `tests/fixtures/blocks/`. Each rule has `vendor`,
`type`, and matchers on status, headers, cookies, body regex, URL, and
browser signals (script hosts, iframe hosts, interstitial text). Initial
rule set MUST cover: Cloudflare (managed challenge, Turnstile, `cf-mitigated`,
`cf_clearance`/`__cf_bm`, 403/503 "Just a moment"), Akamai (`_abck`,
`bm_sz`, "Access Denied" reference pages), DataDome (`datadome` cookie,
captcha-delivery.com), HUMAN/PerimeterX (`_px*`, "Press & Hold"), Kasada
(`x-kpsdk-*`, ips.js/p.js), Imperva/Incapsula (`incap_ses`, `visid_incap`),
AWS WAF (`aws-waf-token`, challenge.js), F5/Shape (heuristic), reCAPTCHA and
hCaptcha iframes, generic 403/429/5xx, and soft blocks (200 with
empty/boilerplate body, login wall, consent wall without content).

Classifier output:

```json
{ "outcome": "challenge_failed", "vendor": "cloudflare", "type": "managed_challenge",
  "confidence": 0.95, "rule": "cloudflare-just-a-moment",
  "retryable": true, "escalate": true, "cooldown": "long",
  "vendors": ["cloudflare"],
  "evidence": ["status:503", "body:/Just a moment/", "cookie:__cf_bm"] }
```

Every verdict, including `ok`, carries `evidence` (the markers that fired)
and `vendors` (vendor hints seen on the page). `retryable`, `escalate` and
`cooldown` are scheduler hints: defaults follow the outcome, a rule may
override them. Confidence bands are meaningful: a vendor marker yields 0.9 or
higher, a bare 403 or 429 without a marker about 0.6 with cooldown `short`.
Vendor cookies and scripts on an otherwise good page never make a block by
themselves.

On the browser path, a detected interstitial is given up to
`browser.challenge_wait_ms` (default 15 s) to self-resolve with behavior
running; if the page proceeds, outcome is `challenge_passed`.

---

## 13. Crawler

Job type `crawl`: `seeds[]`, `allow[]`/`deny[]` (regex on the normalized
URL), `max_depth`, `max_pages`, `same_site_only`, `use_sitemaps`,
`defaults` (what every page is fetched with: any field of a fetch request
but `url`, `session_id`, `method`, `body` and `request_kind`, so `include`,
`pacing`, `render` and `behavior` are a crawl's as they are a fetch's),
`visitors`, `session_id`, `webhook`. It needs the `crawl` scope besides
`jobs`. (`personas` of the first draft is `visitors`: a count, as in a
batch. No request names a persona.)

**The reader.** A crawl reads a site the way a reader goes through it,
not breadth-first. Its first page is typed. From then on the next page is
(1) a link in the text of the page that is open, in document order; (2)
when it has none left, a link in the text of a page at most
`crawl.back_limit` (3) entries back in the tab, the nearest first, which
the persona goes back to and clicks there: a reader who has finished an
article goes back to the listing for the next one; (3) a link of the open
page's menu (what the site repeats on every page: navigation, header,
footer, side bar); (4) a link of a menu within those entries back; and (5)
only when the tab holds nothing more an address is typed: the oldest that
waits, of the least depth. On the http path, which has no tab, the page that holds the
link is the request's `Referer`, as the click would make it, and a typed
address names none. A link is followed under the address its own page
gives: a page is one entry however it is spelled, and the crawl keeps how
each page spells the links it holds, so the browser finds the link it is
told to click and the site is asked for what its own page says. Measured
with the frontier itself on tables of links, inside three links of the
home page (`cargo run -p kage-crawl --example reader_order`):
breadth-first types 96 % of the addresses of a shop and 79 % of a blog's,
each a navigation without a `Referer` and with `Sec-Fetch-Site: none`;
this order types 8 % of the shop, one address of the blog's 344, and one
of the first hundred pages of either. Taking a menu link of the open page
before going back (the first version of the order) typed 9 % and 7 %
there, and 11 of the 18 pages of a listing of ten articles with a menu on
every page, where this order types one: the menu leads away from the
listing before it is exhausted. A link no reader can see
(§10.4: not rendered, transparent, off the page; on the http path what
the markup itself hides) is never followed and is counted as `hidden`:
the trap for crawlers. In a browser every page is scrolled and dwelt on
like any other, for as long as the persona's patience with it lasts, and
the persona rests before the next one (§11).

**One visitor per site.** A site is a registrable domain (the public
suffix list that `kage-http` pins). One persona reads it, and every page
goes through the scheduler like any other fetch: the same pacing, visits,
breaks, waking hours, breaker and cooldowns, and another persona takes
over only when that one cannot go on. `visitors` asks for more, up to the
`domain_concurrency` of the pacing in force, refused beyond; each has a
tab of its own. A job-level `session_id` pins every page to the session's
persona. With `same_site_only: false` a link to any site is followed
inside the same limits, each site with a visitor of its own; every address
passes the SSRF guard before it is asked for, and one it refuses is
counted (`ssrf_denied`) and never requested.

**Scope.** `max_depth` (default `crawl.default_max_depth`, 3, at most 32)
is the least number of links from a seed by the shortest way the crawl
finds: a page found deep and later linked from nearer is lifted, with what
it links to, and read if that brings it inside. `max_pages` (default 100,
at most `jobs.max_items`) counts pages asked of the sites, a page that
failed included. `allow` and `deny` (at most 32 patterns of at most 512
bytes each) match the normalized URL; a seed is read whatever `allow`
says, and `deny` wins everywhere: a seed it matches is refused with the
job. An address that ends like a file (`crawl.skipped_extensions`) is not
a page. The crawl keeps at most `crawl.max_frontier` (50 000) addresses;
what it finds beyond is counted (`dropped`) and forgotten.

**URL normalization** (`kage_core::urlnorm`, pure, property-tested):
scheme and host lower-cased, default ports removed, the fragment removed,
tracking parameters removed by a configurable list (36 by default), the
query sorted, the trailing slash kept as written. `www.` is not folded and
`http` and `https` are two keys: they are two addresses to a site.

**Duplicates.** A crawl holds one entry per normalized URL and never asks
for one twice; the end of a redirect counts, so a page that another
address redirected to is not asked for again under its own (`already_read`).
What a page says is its text without what the site repeats on every page
(`nav`, `header`, `footer`, `aside` and their ARIA roles), white space
collapsed: its SHA-256 is the content hash, and a 64-bit SimHash over
three-word shingles is its fingerprint (none under 24 words). A page with
the hash of a page read before is a duplicate (`duplicate_of`); its own
links are not followed from there, they are the original's. A page whose
fingerprint differs from an earlier one's in at most
`crawl.near_duplicate_bits` (6) of 64 bits is flagged
(`near_duplicate_of`, with the bits) and read on from. Both are fetched
and stored like any page. On the fixture site the near pair is 5 bits
apart and the closest unrelated pair 22 (median 31); a page with 1 % of
its words changed is within 6 bits 95 times of 100.

**Sitemaps.** Off unless `use_sitemaps`: a reader does not ask for
`/sitemap.xml`, and a site that watches for the request sees it. When on,
the sitemaps of the seeds' sites are read before the first page of each
site by the site's **scout** (§14): the one persona that asks a site for
what no reader asks for, is not the site's reader and never becomes it, a
Firefox persona when one is free (Chrome saves a gzipped sitemap as a
file and shows nothing of it: `sitemap_unreadable`; a file a browser
saved and showed nothing of is no empty page, and turns nobody away).
Each file is a typed navigation through the scheduler in the scout's
browser, `/sitemap.xml` and what an index names of the same site, at most
`crawl.max_sitemaps` (10) files per site and `crawl.max_sitemap_urls`
(50 000) addresses per file, in the scout's own window (the sidecar
returns the file as the server sent it, `capture.raw`). What they name are places to start from at
depth 0, inside the scope and `max_pages`; `lastmod` is kept and shown
and orders nothing. Where no persona but the reader could ask (`no_scout`)
nobody asks, and a scout the site turned away is not waited for through
its cooldown: the crawl reads on without the file.

**robots.txt.** Where the policy of a seed's site has its `robots.txt`
asked for (`report` or `enforce`, §14) and the crawl reads sitemaps, the
file is read first, by the same scout, and listed with the sitemaps: the
`Sitemap:` lines of the same site take the place of the guess
`/sitemap.xml`, which is asked for only when the file names none or
there is no file. Where a site's file is enforced, an address the file is
known to disallow never becomes a page of the crawl: no item, no request,
no page of the budget, counted as `skipped.robots_denied`. The first page
of an origin whose file is not known yet waits for it in the pipeline
like any fetch, and a file that cannot be had holds the site
(`robots_unavailable`). Every page's result carries the verdict. A crawl's
`defaults` may say `robots` like a fetch.

**The job.** A crawl has no items when it is accepted; an item is added
for every page it asks for, in the order it asks, and paginated like a
batch's. An item's `crawl` says the page's depth, how the crawl came to
know it (`seed`, `sitemap`, `link`) and on which page, how it was reached
(`typed`, `click`, `back_click` with how many entries back, or `referer`
on the http path), where a redirect ended, its content hash, what it
repeats, how many links it held and how many were new, and for a browser
page how its scrolling ended. A page the site answers with an error
status is a fetch that was made, stored like any other, and no page of
the crawl: nothing is taken from what an error page says, and the crawl
counts it as `failed`. The job's `crawl` counts what it discovered,
fetched, failed, skipped by reason, found repeated, how its pages were
reached, its sitemaps (and the `robots.txt` that named them), and why it
stopped reading (`completed`,
`max_pages`, `cancelled`, `expired`). Cancel, pause, deadline and
idempotency are a batch's (§16.6); a cancel finishes the page that is
being read and asks for no other.

**Storage.** The frontier is `crawl_frontier`, one row per address, kept
in memory by the one worker that holds the job's lease and written
through before it acts. A page is claimed for its item with `SELECT …
FOR UPDATE SKIP LOCKED` before the item is added, so no page is asked for
twice even when a job changed hands, and a crawl that changes hands goes
on where it stands. What the crawl takes from a page (where it ended, its
links with whether a reader can see each, its text) is stored with the
page's result (`results.reading`) and forgotten once the crawl has
recorded the page: a crawl that changed hands between the two takes the
page's links from there and reads on from it (F49). There is no second worker: the job worker of §16.6
works crawls.

---

## 14. Extraction and robots

**Extraction.** `include: ["extract"]` returns what the page says, read
from the document the visit captured (the DOM in a browser, the body on
the http path when it is HTML) at the end of the attempt that got the
page, never by a second request. `extract` is null for a fetch that did
not get its page and for a response that is no HTML document. One tree is
parsed per page: a crawl reads its links and its text from the tree
extraction works on (`kage_crawl::html::Document`).

The object: `title` (the document's `<title>`), `description`,
`canonical` (absolute), `lang`, `published_at` and `modified_at` (from
JSON-LD, then `article:*` metadata, then other metadata, then a `<time>`
of the content; in UTC when the page named a zone, as written when not),
`meta`, `opengraph` and `twitter` (a name to the list of its values, in
document order), `jsonld` (every block that parses, a top-level array as
its members, at most 64; `jsonld_skipped` counts the rest), `links`
(`url` absolute, `text`, `rel`, `same_site`; what a reader can see, at
most 5000, with `links_truncated`), `headings` (`level`, `text`, `id`),
`text` and `markdown` (the main content), `word_count`, `thin_content`.
The request's `extract: { markdown, links }`, both true unless said,
leaves either out.

The main content is Readability's scoring (the algorithm of Firefox's
reader view) ported onto that tree in `kage-extract`, with three rules of
Kage's own: what the markup names as furniture (`nav`, `aside`, `footer`,
the ARIA roles of the same, dialogs) is never content, and a page without
a candidate has no content; bylines are metadata, not content; where the
chosen element is one of three or more alike (the posts of a thread) the
others are content too, and a figure between two parts of the content
belongs to it. The markdown is written by the same walk as the text:
headings, nested lists, fenced code with its language, GFM tables with a
header row, absolute links. `thin_content` is true when the main content
has fewer words than `extract.thin_words` (80): the document gave a
reader next to nothing, which on the http path usually means scripts or
a wall. The result does not guess which.

Measured word for word against text marked by hand: on the ten fixture
pages of ten kinds (`tests/fixtures/extract/`, written, not recorded;
each has a golden file that `just goldens` regenerates and `just test`
compares) the mean F1 is 97.0, against 76.2 for the furniture rule of §13
alone; on three further pages written afterwards and never tuned on
(`tests/fixtures/extract-heldout/`) it is 97.7.

**robots.txt.** A reader never asks for `/robots.txt`, so the request
marks the address that makes it. Whether a site's file is asked for at
all is therefore its policy's: `robots: ignore` (the default: nobody
asks, and every result says `{ "fetched": false, "allowed": true }`),
`report` (ask, report the verdict, fetch either way) or `enforce` (ask,
never fetch what the file disallows, keep to its `Crawl-delay`).

- **Who asks.** The site's *scout*: one persona per site (registrable
  domain), which asks for the `robots.txt` of each of the site's origins
  and for its sitemaps (§13) and never reads a page of the site; the
  pipeline keeps it out of every other fetch of the site. It is
  remembered (`site_scouts`), so that one address is marked and no other.
  The scout is, in this order: the persona that asked before; a ready
  persona the site's policy keeps from reading anyway (another country or
  engine), which costs the site no reader; a ready persona that has not
  read a page of the site for `robots.scout_quarantine` (30 days), as
  long as the readers the fetch needs stay, first one whose engine keeps
  another persona that may read the site (ready or not), so that a site's
  only Firefox stays its reader while a Chrome can ask (F51); nobody.
  Never a persona that read the site within the quarantine: after it, one
  address that both read the site and asked it is the price of a site
  that would otherwise have no scout for good once every persona has read
  it. Among equals a Firefox goes first, and then the site's name draws.
  A scout that is not ready (its proxy down, the persona cooling) closes
  a site that enforces its file until it is back or retired;
  `POST /v1/admin/sites/{site}/scout` gives the site another one, chosen
  by the same rule for one reader and never the one it had (`409
  no_matching_persona`, and the site keeps its scout, when nobody else
  may ask).
- **How.** Through the scheduler like any page, in the scout's browser,
  the address typed, once (no retry from another address), with the dwell
  of any visit; the request is Kage's own and no key's. Measured in
  Chrome 155 and Camoufox 152: what the site sees is one document
  navigation of a person who typed the address (Firefox also asks for the
  favicon), the file is there in under 100 ms, and the visit lasts 3 to
  16 s, 8 s at the median. A scout that is blocked cools down for the
  site and counts on its breaker like any persona.
- **What is kept.** One answer per origin (`robots_cache`), for
  `robots.ttl` (24 h, moved by up to a tenth either way so that files are
  not asked for on a grid), and asked for again only when a request needs
  it. A `4xx` that is no block is "no file": everything is allowed. A
  server error, a timeout or a block is "unreachable" for
  `robots.error_ttl` (1 h). An attempt that failed before the site had a
  say (the proxy, the sidecar) is nothing: nothing is kept, and it is
  tried again a minute later. At most `robots.max_bytes` (512 KiB, what
  Google reads) of a file are read.
- **The verdict** is Kage's own reading of the file by Google's rules
  (RFC 9309: the longest match, `Allow` on a tie, `*` and `$`; the group
  of `*`, and before it any group that names a token of `robots.agents`,
  `["kage"]`, so that a site owner can address Kage):
  `{ fetched, allowed, matched_rule, crawl_delay }` on every result. The
  parser and the matcher are Kage's; the `robotstxt` crate (a port of
  Google's parser) is the test oracle only, because it fails on a long
  line with characters of more than one byte, loses the first line of a
  file with a byte order mark, and matches `*` wrongly in one case.
- **`report`**: nothing waits. A fetch reports what is known; when
  nothing is, its result says `fetched: false` and the scout is sent
  after it, at job priority, so that the next result of the origin has
  the verdict. Whoever read the page is not chosen as the scout.
- **`enforce`**: the first fetch of an origin waits for the scout.
  Nothing of an origin is fetched without a verdict. A disallowed address
  is `robots_denied` with the rule, and nothing is asked of the site. An
  unreachable file means disallowed (RFC 9309 §2.3.1.4): `robots_denied`
  with `reason: robots_unavailable` and `retry_at` for a caller that
  waits, while a job's page waits (`paused`, `robots_unavailable`) up to
  `jobs.max_age`; `robots_no_scout` when nobody can ask. A scout the
  scheduler refuses (the site is paused, the scout sleeps) is the page's
  own refusal. `Crawl-delay` is a floor under the gap from the start of
  one page to the start of the next for each persona on the host, with up
  to a fifth added so that no two gaps are alike, and no cap but 366
  days: a site that asks for an hour gets an hour, and `report` is the
  way out. Under `report` it is reported only.
- **A request** (`robots: policy | report | enforce | ignore`) may ask
  for more care than the policy takes and never for less, and never
  makes Kage ask a site its policy leaves alone: `enforce` where the
  policy reports is honoured, with the same file; `report` or `ignore`
  where it enforces is refused (`robots_enforced`); `enforce` where it
  ignores is refused (`robots_ignored`: no verdict will ever exist);
  `report` where it ignores answers `fetched: false`.
- **Not covered**: a redirect. The verdict is about the address that was
  asked for; where a redirect leads is the site's own doing.

---

## 15. Caching

Opt-in per request (`cache_ttl_s`) or per site (the policy's
`cache.ttl`). A request whose key holds a result that is young enough is
answered with it: no persona, no visit, no pacing, nothing asked of the
site and no page of the key's daily budget, and so also while the site is
paused or the pool sleeps. It counts against the key's requests per
minute like any call.

- **The key** is everything that changes what a result holds, and
  nothing that only changes when or how politely it is fetched: the
  normalized URL (as a crawl normalizes it, §13), the method, the body,
  the request kind, the caller's headers, `render` as asked (`auto`,
  `http` and `browser` are three keys: which path `auto` takes is the
  policy's, and may change), the include set (a result with more parts is
  not cut down for a request for fewer), `wait`, `behavior`, the
  `screenshot`, `pdf`, `xhr` and `extract` options, `max_body_bytes`, the
  persona hints as asked (never the country of whoever happened to
  fetch), and the API key, unless the site's policy says `cache.shared`.
  Not in it: `cache_ttl_s`, `timeout_ms`, `max_retries`, `pacing`,
  `robots`.
- **Age.** An entry lives as long as the request that wrote it asked for,
  and is served to a later request only while it is also no older than
  that request's own lifetime: nobody gets a page older than they asked
  for. `cache_ttl_s: 0` is never. More than the site's `cache.max_ttl`
  (the configuration's, 24 h and never beyond the retention of results,
  unless the policy sets a shorter one) is refused with
  `cache_ttl_too_long`, not cut.
- **What is kept**: a result with the outcome `ok` or `challenge_passed`
  and a `2xx` status whose extraction, when there was one, is not thin.
  Never a block, a failure, a refused redirect or an error status.
- **Who reads and who fills.** A fetch and an item of a batch read and
  fill. A page of a crawl fills and never reads: a page that is not
  visited is not open in the tab, and what it links to cannot be clicked
  from it. Measured on the fixture sites in the reader's order: with half
  of a site cached, a fifth to a half of the pages still asked for would
  arrive from nowhere (no `Referer`, `Sec-Fetch-Site: none`), where the
  reader's order has 0.3 to 8 %. A flow has no part in the cache; a
  request in a session neither reads nor fills it (what a session sees
  depends on its cookies).
- **A hit** is a request and a result of the key that asked, with
  `cached: true`, `cached_at` (when the page was fetched), no attempts,
  the stored extraction and robots verdict, artifact rows of its own that
  point at the stored bodies, and the persona handle only for the key
  that had the page fetched. What the persona did on the page, the
  browser's log and what a capture fired in it are the stored visit's;
  the pacing is the hit's own, which waited for nobody (F36). Purge and
  retention of one key take nothing from another.
- **Storage.** `response_cache`; an entry goes with the result it serves
  and is swept when it expires.
- **Two at once (F52).** Of two requests with one key under way at the
  same moment in one process, the second waits for the first and is
  answered from the cache with what the first one stored, the moment it
  stored it: one visit. When the first stored nothing (a block, a
  failure, thin content) the second makes its own visit. It waits as
  long as the request waits for anything: a job's fetch stops with its
  job, a synchronous one that outlasts the wall time is carried on as a
  job and still waits. A crawl's page holds its key for others and
  waits for nobody. Two processes each fetch.
- **Not built**: the same crawl asked for again inside the lifetime
  visits every page again; serving it would be all or nothing (the
  earlier job's pages as a whole), which needs an identity of crawls and
  items that point at another job's results.

---

## 16. Public API

### 16.1 Conventions

- Base path `/v1`. JSON only: a body is `application/json` or the request
  is `415 unsupported_media_type`. A field the API does not know is an
  error (`422 validation_failed`), never ignored.
- Errors follow RFC 9457 problem details (`application/problem+json`) with
  a stable `code`, the request's id as `request_id`, and what a client
  needs to act as further members (`field`, `header`, `retry_after_s`,
  `constraint`). A problem never carries an address, a persona's id or a
  sidecar's words. The codes are the `ErrorCode` enumeration of the OpenAPI
  document.
- A fetch that was made answers `200` whatever the site did: a block is
  the result's `outcome`, not an HTTP error. Only a request that cannot be
  made as asked, or that the API cannot serve, is a problem.
- Every response has `X-Request-Id` (a UUIDv7 the server makes; one a
  client sends is not taken over); `Idempotency-Key` is honored on
  `POST /v1/jobs` for 24 h (`jobs.idempotency_ttl`; §16.6).
- OpenAPI 3.1 at `/openapi.json` (no key: it is the same for every
  deployment), generated from the Rust types and committed as
  `crates/kage-api/openapi.json`; `kage openapi` prints it. Swagger UI at
  `/docs` (admin scope; `server.docs: false` removes it).
- Liveness `/healthz` (no dependency), readiness `/readyz` (the database
  answers, at least one persona is ready behind a usable proxy, at least
  one sidecar answers; `503` with the three checks otherwise, and from the
  moment the server is told to stop), both without a key. Metrics
  `/metrics` needs the admin scope on the API; `server.metrics_bind` opens
  a second listener that serves `/metrics` and `/healthz` without a key,
  for a Prometheus on the same private network.
- The API listens on `127.0.0.1:8080` unless `server.bind` says otherwise,
  speaks plain HTTP/1.1 (a reverse proxy in front speaks TLS and HTTP/2 to
  its clients), and sends no CORS header unless
  `server.cors_allowed_origins` names origins (§19).

### 16.2 Authentication and authorization

`Authorization: Bearer kage_<keyid>_<secret>`. The key id is the key's
UUID without hyphens; the secret is 32 random bytes, base64url. Stored as
SHA-256 of the secret with the key id as lookup, compared in constant
time; shown once at creation and again only by a rotation. A token that
does not parse, a key that does not exist and a wrong secret are one
answer (`401 invalid_key`); only the holder of the secret learns that a
key was disabled (`401 key_disabled`).

Scopes: `fetch`, `jobs`, `crawl`, `sessions`, `admin`. They are
independent: `admin` implies no other, and `session_id` in a fetch needs
`sessions` beside `fetch`. A missing scope is `403 scope_missing` and names
it.

Per-key limits: requests per minute (a token bucket; default 60),
requests in flight (default 4; a fetch that outlasts the wall time gives
its slot back with its `202`), and an optional daily page budget (every
fetch a key starts is a page, whatever came of it; counted per UTC day
from the stored requests, so a restart forgets nothing). The API as a
whole has `server.requests_per_minute` (600), counted before a key is
looked at, so guessing keys is limited too. All are `429` with
`Retry-After` and a code that says which (`rate_limited`,
`concurrency_limited`, `budget_exhausted`).

Keys are made, listed, disabled and rotated through the admin endpoints or
`kage key create|list|revoke|rotate`, which works on the store and needs no
running server: the first admin key of a deployment is made there. Every
admin call through the API, whatever came of it (a refusal for a missing
scope included), and every change at the command line is written to
`audit_events` with the key, the action (`admin.persona.retire`,
`key.create`), its target and its outcome; never a token or a hash.

### 16.3 Endpoints

| Method and path | Scope | Purpose |
|---|---|---|
| `POST /v1/fetch` | fetch | Synchronous fetch. One that outlasts `scheduler.sync_wall_time` (180 s) is carried on as a job (§8.4) and is answered `202` with the request's id, the job's id (`job_id`), `Location` and `Retry-After`; its result is stored when it ends |
| `POST /v1/jobs` | jobs / crawl | Create a `batch` or a `flow` job (`crawl` with M10): `201` with the job, `200` with `Idempotency-Replayed: true` for the job an `Idempotency-Key` made before, `409 idempotency_key_reused` for another request under that key. A job is accepted whole or refused whole (§16.6) |
| `GET /v1/jobs/{id}` | jobs | State, counters, why and until when it is paused, its deadline, and the delivery log of its webhook. A key reads the jobs it made; another key's is `404 job_not_found` |
| `GET /v1/jobs/{id}/items?cursor=&limit=&results=` | jobs | The items in the job's order, 50 a page unless said and at most 200, each with its stored result as `GET /v1/results/{id}` shows it without the document and the body (which are artifacts); `results=false` leaves the results out |
| `POST /v1/jobs/{id}/cancel` | jobs | Cooperative cancel: what waits is cancelled at once, a page that is being fetched is finished, so at most one more item per visitor ends. A job that has ended is answered as it ended |
| `GET /v1/results/{id}` | fetch | The result of a fetch by the request's id (the `id` of the result, `X-Request-Id` of the fetch): `200` once it ended, `202` while it runs. A key reads what it fetched; another key's is `404` |
| `GET /v1/artifacts/{id}` | fetch | What was kept of a page (document, raw body, screenshot, PDF, the body of a captured XHR response), streamed; served with `nosniff` and a sandboxing CSP, gzip passed through to a client that takes it |
| `POST /v1/sessions` | sessions | Open a session pinned to a persona matching the hints and a `domain`'s policy: `201` new, `200` when the id is open already, `409 no_matching_persona` |
| `GET /v1/sessions/{id}` / `DELETE` | sessions | Inspect (not a use: the idle time does not start again) or end |
| `GET /v1/policies/domains/{domain}` | fetch | Effective policy preview: the policy as it resolves, the pacing profile, the first render, a pause in force |
| `GET /v1/admin/personas[?state=&all=]`, `GET …/{id}[?fingerprint=true]`, `POST …/{id}/retire`, `POST …/{id}/regenerate`, `POST …/{id}/selftest` | admin | The self-test runs in the server: the http path when the persona has one and an echo is configured, the browser path in the persona's own context through the scheduler's gate and the forwarder, so it never meets `context_conflict` with real work (F29) |
| `GET /v1/admin/proxies[?all=]`, `POST …/{id}/replace`, `POST …/{id}/health` | admin | Never a proxy's user name or password |
| `GET /v1/admin/pool` | admin | Pool and bandwidth summary |
| `POST /v1/admin/policies/reload` | admin | Force reload; an invalid file changes nothing and is `422` |
| `POST /v1/admin/sites/{site}/scout` | admin | Give a site another scout (§14); `409 no_matching_persona` when nobody else may ask |
| `GET/POST /v1/admin/keys`, `DELETE …/{id}`, `POST …/{id}/rotate` | admin | Key management; a token is in the answer of a creation and of a rotation only, with `Cache-Control: no-store` |
| `GET /v1/admin/audit?cursor=&limit=` | admin | Newest first, at most 200 a page |
| `POST /v1/admin/purge` | admin | `{ key_id, before? }`: deletes what a key fetched before a moment (now unless said): its finished jobs with everything of theirs, and every other result, its attempts and its artifacts, before their retention ends. What is still somebody's work stays: a request that has not ended, and the request an open item of a job waits on. Answers with how many requests, jobs and artifacts went; on the audit log as `admin.data.purge` (F33) |
| `GET /metrics` | admin | §20 |

Sessions (§5): a session is opened by `POST /v1/sessions` and by nothing
else. A fetch that names one that is not open is `404 session_not_found`,
so a caller always knows when the persona behind an id is a new one. A
session never moves: a fetch whose persona hints, or whose domain's
policy, exclude the session's persona is `409 session_persona_conflict`
naming the constraint, and nothing is fetched.

### 16.4 FetchRequest

```json
{
  "url": "https://example.org/listing?page=2",
  "method": "GET",
  "headers": {},
  "body": null,
  "render": "auto",
  "request_kind": "navigation",
  "include": ["dom", "raw", "headers", "extract", "xhr", "screenshot"],
  "session_id": "proj-a-42",
  "persona": { "countries": ["DE", "AT"], "engines": ["chromium"] },
  "pacing": { "profile": "human" },
  "wait": { "until": "network_idle", "selector": null, "extra_ms": 0 },
  "behavior": { "scroll_depth": 0.8, "dwell_ms": null, "referer_strategy": "direct" },
  "screenshot": { "full_page": true, "format": "webp", "quality": 80 },
  "pdf": null,
  "timeout_ms": 60000,
  "max_retries": 2,
  "cache_ttl_s": 600,
  "robots": "policy",
  "extract": { "markdown": true, "links": true },
  "xhr": { "content_types": ["application/json"], "max_bytes": 2000000 },
  "max_body_bytes": 10000000
}
```

A `Referer`, caller headers, a method other than `GET`, a body or a
`request_kind` other than `navigation` need `render: http` and are refused
under `browser` or `auto` (`render_needs_http`, `render_kind_conflict`);
arriving from a page in a browser is `behavior` (a click on a link, F28),
never a header. `behavior.dwell_ms` is a median Kage draws around per
attempt, never below the policy's `min_dwell`; `timeout_ms` is at least
15 000 on a browser render (`timeout_too_short`). The forwarding headers
are on the denylist (F34): `X-Forwarded-For`, `Forwarded`, `X-Real-IP`,
`True-Client-IP`, `X-Client-IP`, `CF-Connecting-IP`, and the rest of the
family (every `X-Forwarded-*`, `X-Originating-IP`, `X-Cluster-Client-IP`,
`Fastly-Client-IP`). A browser never sends them, and a request that does
says that a proxy is in the way.

Validation: `url` http(s) only, ≤ 8 KB, SSRF guard (§19); `headers` ≤ 32
entries, denylist enforced (`header_denied`, naming the header); `include`
⊆ {`dom`,`raw`,`headers`,`extract`,`xhr`,`screenshot`,`pdf`,`console`},
by default `dom` and `headers`, or `raw` and `headers` with `render:
http`; `render: http` with an `include` only a browser produces is
rejected with code `render_include_conflict`. There is no `locale`: a
persona's languages are its own. Since 2.0 every request also names
`level` (§25), which replaces `render` (derived from the level) and is
required (`level_required`).

What only the http path can send is refused elsewhere instead of being
dropped: a `method` other than `GET` (`GET`, `HEAD`, `POST` exist), a
`body` and caller `headers` need `render: http`
(`render_needs_http`); a browser render is a plain navigation. Arriving
from a page in a browser is `behavior.referer_strategy`, not a header.

`request_kind` (`navigation`, `fetch`, `xhr`; default `navigation`, never
guessed from the URL) says what kind of request the http path sends
(§9). A `fetch` or `xhr` needs `render: http` (`render_kind_conflict`)
and a `Referer` header that names the page making it
(`request_kind_needs_referer`); `Origin` and the `Sec-Fetch-*` headers are
derived and cannot be set. Of a caller's headers a script's request takes
`Referer`, `Accept` and `Content-Type`: another header has no captured
place in one (`header_not_placeable`). A request a browser would preflight
(cross-origin with a body type or header that is not simple) is
`preflight_required` until preflights are emulated (with F30's milestone).

`behavior` is what a request may change of what the persona does on a
page, in a browser: `scroll_depth` from 0 (no scrolling) to 1, honoured as
given; `dwell_ms`, the median of a log-normal the attempt's seed draws
from (p95 at twice the median), never below the domain policy's
`min_dwell` (2 s unless set) and at most 60 000; `referer_strategy`. A
policy with `behavior_locked` ignores all three, and the result says so.
The pointer always wanders; no field turns it off. How long the persona
goes on scrolling a page is no field either: it is the patience Kage
draws for every page (§10.4), and the result says what it was. `timeout_ms`
is the time to reach the page, and in a browser bounds nothing after it;
it is at
least 15 000 when a browser may make the attempt and 1 000 with `render:
http`, at most 300 000; a shorter one is `timeout_too_short`, not clamped:
a budget that only fits a bare load would make every visit one. `pacing`
is D9's: a profile's name or a custom profile, ignored and reported when
the policy locks it.

`include: ["extract"]` returns what the page says (§14), on either path;
`extract` (`markdown`, `links`, both true unless said) leaves a part out
and is refused without it. `cache_ttl_s` is for how many seconds the
result may be served from the response cache (§15): left out, the site's
policy decides (`cache.ttl`, never unless set); 0 is never; more than the
site's results may be served for is `cache_ttl_too_long`; in a session it
is refused. `robots` (`policy`, `report`, `enforce`, `ignore`) is what
the request says about the site's `robots.txt`: it may ask for more care
than the policy takes and never for less, and never makes Kage ask a site
its policy leaves alone (`robots_enforced`, `robots_ignored`; §14). The
code `not_available_yet` is gone: nothing answers it any more.

### 16.5 FetchResult

```json
{
  "id": "0192…", "outcome": "ok", "status": 200,
  "final_url": "https://example.org/listing?page=2",
  "redirects": [], "headers": { "...": "..." },
  "dom_html": "<!doctype html>…", "raw_body": null, "raw_body_artifact": "art_…",
  "extract": { "title": "…", "markdown": "…", "links": [ ... ], "thin_content": false },
  "xhr": [ { "url": "…/api/items", "status": 200, "content_type": "application/json", "body": "…" } ],
  "artifacts": { "screenshot": "art_…" },
  "timing": { "ttfb_ms": 410, "load_ms": 2210, "total_ms": 9800 },
  "persona": { "id": "…", "engine": "chromium", "claimed_os": "windows", "country": "DE" },
  "attempts": [ { "n": 1, "render": "browser", "outcome": "ok", "status": 200, "total_ms": 9800 } ],
  "block": null,
  "robots": { "fetched": true, "allowed": true, "matched_rule": null, "crawl_delay": null },
  "pacing": { "applied": "human", "waited_ms": 3900 },
  "cached": false, "cached_at": null
}
```

Blocked example: `outcome: "challenge_failed"`, `block: { vendor, type,
confidence, evidence }`, `attempts` lists each persona tried, `status` is
the last status.

As built (the OpenAPI document is the contract):

- `id` is the request's id; `state` is `finished` (a `202` carries `state:
  "running"`, `job_id` and `result`, the path of the result).
- `headers` is a list of `{ name, value }` in the order received (a name
  may repeat). The values of `Set-Cookie` are `[redacted]`, with the
  cookie's name and attributes kept, unless `security.expose_set_cookie`
  is on: a session a persona holds with a site is not a caller's to take.
- `dom_html` and `raw_body` are inline when `include` asks; the http path
  has no document, so `include: ["dom"]` returns its body as `raw_body`.
  What `include` asked for is also kept as artifacts, named in `artifacts`
  by kind (`dom_html`, `raw_body`, `screenshot`, `pdf`) as `art_…` ids.
- `xhr` is `{ responses, dropped }`; a response's body is inline up to 256
  kB of text and an artifact (`body_artifact`) beyond.
- `persona.id` is a handle (`p_` and 16 hex digits) for every key without
  the admin scope: stable for one key, different for another, an HMAC of
  the key's and the persona's ids under a key derived from the master key.
  `engine`, `claimed_os` and `country` are shown. No response to such a key
  names a proxy, an address or an attempt's seed. A key with the admin
  scope sees the persona's id and the seeds.
- `attempts[].persona`, `reason`, `retry_at`, `escalated`, `session_id`,
  `side_effects`, `created_at`.
- `pacing` (`applied`, `request_ignored`, `waited_ms`) and `behavior`
  (`applied`: `policy` or `request`, `request_ignored`, `scroll_depth`,
  `scroll_reached`, `scroll_ended_by`: `depth`, `patience` or `page`,
  `patience_ms`, `dwell_ms`, `referer_strategy`, `moves`, `notches`) say
  what applied.
  They, `console` and `side_effects` are kept with the result (F36): the
  answer of `POST /v1/fetch`, `GET /v1/results/{id}` and an item's result
  in a job say the same. A result keeps at most the first 1000 entries of
  a browser's log, each text cut at 4096 bytes; the seed in `behavior` is
  the last attempt's and for a key with the admin scope only.
- `extract` is the object of §14, with `include: ["extract"]`; null for a
  fetch that did not get its page and for a response that is no HTML
  document. `robots` is `{ fetched, allowed, matched_rule, crawl_delay }`
  on every result that got as far as the site's policy; `fetched: false`
  says that nothing is known of the file (the default: nobody asks).
- `outcome: "robots_denied"` is a result like a block: `200`, no attempts,
  nothing asked of the site. Its `reason` is null when a rule of the file
  disallows the address (`robots.matched_rule` names it),
  `robots_unavailable` when the file is enforced and could not be had
  (`retry_at` says when it is asked for again), `robots_no_scout` when
  nobody can ask for it, and `robots_enforced` or `robots_ignored` when a
  policy changed under a job's request.
- `cached: true` says the result was served from the response cache:
  `cached_at` is when its page was fetched, `attempts` is empty, and
  `persona` is shown only to the key that had the page fetched. The page
  is the one that visit got, so `behavior`, `console` and `side_effects`
  are that visit's; `pacing` is the request's own, with `waited_ms: 0`:
  it waited for nobody. Its `artifacts` are its own.
- A result and its artifacts are a key's own: another key gets `404`, a
  key with the admin scope reads all. Both live for
  `artifacts.retention_days`.

### 16.6 Jobs and webhooks

Extraction, the cache and `robots.txt` in a job (§14, §15): a batch's
`defaults` and items take `include: ["extract"]`, `extract`,
`cache_ttl_s` and `robots` like a fetch, and an item the cache holds is
answered from it (no visit, no page of the budget). A flow's `include`
and its `extract` step take `extract`; a flow never reads or fills the
cache. A crawl's `defaults` take all four; its pages fill the cache and
are never answered from it (§15), and what an enforced `robots.txt`
disallows never becomes one of its pages (§13). A page whose site's
enforced file cannot be had pauses the job with the reason
`robots_unavailable` until the file is asked for again.

Job states: `queued`, `running`, `paused`, `succeeded`, `partial`,
`failed`, `cancelled`. A job ends `succeeded` when every item succeeded,
`failed` when none did, `partial` otherwise, and `cancelled` when a cancel
ended it before every item ran.

**Acceptance.** `POST /v1/jobs` takes `{ "type": "batch", ... }`,
`{ "type": "flow", ... }` or `{ "type": "crawl", ... }` (§13; every seed
passes the checks of a fetch, the problem names it as `item`). A job is
accepted whole or refused whole, and
nothing is stored for one that is refused: every item of a batch passes
the checks of a synchronous fetch (the problem names it as `item`), every
`goto` of a flow passes the SSRF guard (`step`), a `session_id` must be
open and its persona one every item may use, and a webhook's receiver must
be one the operator listed. At most `jobs.max_items` items (10 000), at
most 100 steps. An `Idempotency-Key` (1 to 255 visible ASCII characters)
names the job it made for `jobs.idempotency_ttl` (a day) per API key: the
same request again, whatever the order of its members, is answered with
that job; another request under the key is `409 idempotency_key_reused`.
What is compared is a keyed fingerprint of the body, because the body
holds the webhook's secret.

**A batch** is `urls` (fetched as `defaults` says) and `items` (fetch
requests of their own, with whatever they set in place of the default), in
that order. Its items are worked site by site, sites side by side. Per
site a batch has **one visitor**: one persona reads its pages of that site
in the order given, in sittings, with the pauses a person makes, and comes
back after its breaks and its nights; another persona takes over only when
that one cannot go on (blocked there, at its daily cap, retired, its proxy
unusable), and whoever gets through is the visitor from then on. A site
therefore sees one household reading, not a list dealt out to several
addresses. Measured with the planner under the `human` profile: 50 pages
of one site take about an hour on one address (a median of 61 minutes) and
300 pages a working day, which is the daily cap of one persona; with three
visitors 50 pages take about 20 minutes on three to four addresses, and
dealing them out freely (three at a time, any persona) took 10 minutes
with 44 changes of address. `visitors` asks for more than one, up to the
`domain_concurrency` of the pacing in force for the site; more is refused,
never reduced. That is the way to trade stealth for time. A job-level
`session_id` pins every item to the session's persona under a fetch's
session rules, and the job counts as use of the session while it has items
left. A batch is planned by the same planner as everything else (D37): it
never wakes a sleeper and has the priority of a job.

**An item that cannot be attempted now is not failed.** A paused site, a
visitor that sleeps or is on a break, the bandwidth term, the key's daily
page budget: the item goes back into the queue with the time to come back,
its site is left alone until then, and a job whose every remaining item
waits is `paused` in the store (`paused.reason`, `paused.until`) and in
nobody's hands until that time. An item waits in the scheduler for its
visitor for up to five minutes and is told to come back only when nobody
it may have could start in that time: a visitor in the middle of a page
counts from its slot (the end of the page and the rest after it are not
known before the page ends), and one that an earlier page waits for counts
too. A site left alone for want of a persona (`no_persona`) is asked again
as soon as one of its pages ends, which can change the answer; the other
reasons keep their time. With several visitors a site therefore never
stands still for the long break of one of them while another reads
(follow-up F54: 200 pages under `human` with three visitors took a median
of 146 minutes in the simulation of the release's load test, and take
61, as long as when every page may wait as long as it must). A job may wait `jobs.max_age` (7 days)
from its acceptance; past that deadline it ends with what it has, and the
items left fail with `expired: <what they waited for>`.

**A flow** (§10.5) is one unit: one browser attempt of one persona. Its
items are its steps, in order, and one last item for the page it ended on.
A step's item says whether it ran and succeeded, how long it took and how
much of that was pacing; a step that captured the page (`extract`,
`screenshot`, `pdf`) names a result of its own, read, kept and deleted
like any other; the last item is the flow's own request with its attempts.
Steps after one that failed are `failed` with `not_run`.

**The worker.** Jobs are claimed with `SELECT ... FOR UPDATE SKIP LOCKED`
under a lease (`jobs.lease`, renewed while the job is worked), at most
`jobs.max_running` at once and `jobs.max_items_in_flight` items of all
jobs in the scheduler. A job whose worker stopped is claimed again when
its lease ran out, by the same server after a restart or by another one,
and goes on where its items stand. An item names the request that carries
it out before that request is written, so a request is never begun twice.
A cancel is read with every renewal of the lease and at once on the server
that works the job: what waits is cancelled, an item that waits for its
persona is taken out of the scheduler's queue, and an attempt that is
under way is finished, never cut.

**A server that stops (F39).** A page that is being read when the server
is told to stop is a request the site has seen. Cut without a word, the
record says nothing of it, the request is taken up again after the
restart and the site sees the page twice: measured live in M9 for a fetch
that was carried on, and true of every item of a job whose page took
longer than the 20 s the worker then waited. So, from the moment of the
signal: the scheduler grants nothing more (what waits for a persona is
told `shutting_down`, a job's item is given back with its time), and the
server waits for the attempts that are under way, whoever they are for
(an item, a page of a crawl, a fetch that is carried on, a caller that
still waits), for at most `jobs.wind_down` (150 s: a visit under `human`
takes about 130 s at the 95th percentile). Each is finished and written
down like any attempt, and its request ends or goes on as it would have.
What is still under way when the wait is over is cut and written down as
cut: an attempt with the outcome `provider_error` and the reason
`interrupted`, which cools nobody, counts on no breaker and counts toward
the request's retries. A job's request stays open and is taken up again
after the restart, with that attempt in its record; a fetch whose caller
still waited ends with it. `jobs.wind_down: 0s` waits for nothing. The
container must be given the time: `stop_grace_period` of the `kage`
service is 200 s (§22). A process that is killed writes nothing; its
pages are fetched again when the lease has run out.

**Webhooks.** `webhook: { url, secret, progress_every? }` on a job. The
receiver must be an origin the operator listed in
`webhooks.allowed_origins` (none by default: no job may have a webhook
then); another one is `422 webhook_origin_not_allowed`. A webhook is the
one request Kage makes from its own address, and under any other rule a
key with the `jobs` scope could make that address post a signed request at
any public site, a target among them (D17). A listed origin passes the
SSRF guard like a target, unless it is also in `webhooks.private_origins`:
those are the operator's own (the compose network, the LAN, the host) and
may resolve to a private or loopback address; link-local, metadata and
multicast addresses are refused for everyone.

Events: `job.completed` or `job.failed` when the job ends, and
`job.progress` whenever `progress_every` more items are over. The body is
thin: `{ id, type, created_at, job }` with the job as `GET /v1/jobs/{id}`
shows it to its key, and nothing of items, URLs, personas or proxies.
`X-Kage-Signature: sha256=<hex>` is the HMAC-SHA-256, under the job's
secret, of `<X-Kage-Timestamp>.<body>` (the timestamp in Unix seconds, the
body exactly as sent); `X-Kage-Event` names the event and
`X-Kage-Delivery` its id, the same on every attempt, so a receiver drops a
repeat. Every attempt is signed anew. A receiver recomputes the signature,
compares in constant time and refuses a timestamp older than a few
minutes. The secret is 16 to 256 bytes, sealed at rest bound to its job,
and never returned; a job shows only its receiver's host.

Delivery: plain wreq without emulation and with an honest `User-Agent`
(`kage/<version>`), never through a proxy and never one named in the
environment. The sender resolves the receiver's name itself, refuses a
name any of whose addresses it may not post to, connects to the addresses
it checked and to no other, and follows no redirect; all of that again on
every attempt. Anything but a `2xx` is a failure: one attempt and five
retries (`webhooks.max_retries`) after 30 s, 2 min, 8 min, 32 min and
2 h 8 min, each moved by up to a fifth either way by the event's own seed;
a `Retry-After` is honoured up to an hour; `410` ends the retries; 10 s
per attempt. One delivery is in flight per receiver, and while a
receiver's last attempt failed its other events wait behind the next probe
without spending attempts of their own. A `job.progress` that still waits
when a newer event of its job is queued is dropped (`superseded`). Every
attempt is a row of the delivery log (`webhook.deliveries` on the job),
and what waits survives a restart.

**Retention.** A finished job, its items, their requests, results and
artifacts are deleted `jobs.retention_days` (14) after it ended; a result
that is no job's goes when it is as old as `artifacts.retention_days`. The
admin purge (§16.3) does the same for one key before that time.

---

## 17. Configuration

`kage.yaml` layered with env `KAGE__SECTION__KEY=value` (figment, with
`serde-saphyr` reading the YAML: a repeated key and a merge key are errors
with line and column, depth 128, a million nodes and 16 MiB of scalars at
most, only `true` and `false` are booleans, and text that reads as a number
is written in quotes where text is expected; F45). Secrets
MUST come only from env or files mounted via Docker secrets:
`KAGE__WEBSHARE__API_KEY`, `KAGE__DATABASE__URL`, `KAGE__SIDECAR__TOKEN`,
`KAGE__SECRETS__MASTER_KEY` (for encrypting proxy credentials at rest;
exactly 32 bytes as 64 hex characters, validated at load),
`KAGE__REPUTATION__ABUSEIPDB_KEY`, `KAGE__PROXIES__STATIC_LIST`. Every
secret may also be given as `<NAME>_FILE`; an empty variable does not
conflict with its `_FILE` form. Required for every command except
`healthcheck`: database URL, sidecar token, master key; the others are
optional. Startup fails on a missing required secret. `kage config check`
validates and prints the effective config as JSON with secrets redacted.
Sections at this point: `server`, `database`, `secrets`, `sidecar`,
`policies`, `artifacts`, `proxies` (with `proxies.health`), `webshare`,
`reputation`, `http` (with `http.subresources`), `selftest` (`selftest.http`,
`selftest.browser` with its own pages and the third-party pages, each with
the `interval` of its scheduled run), `personas` (`dir`, `mix`,
`retention_days`, `upgrade_backup_days`, `archive_after`, `sweep_interval`,
`warmup`), `sessions`
(`idle_ttl`, `max_lifetime`, `sweep_interval`),
`engines` (`engines.<engine>.browser_major`: 155 for the chromium image of
M4, 156 for the firefox image since the Camoufox 156 milestone (152 in
M5); a set major must be served by a profile,
verified or recorded as browser-only, or `kage config check` fails;
`engines.chromium.allow_cross_os_claims`, default false), `scheduler`
(the caps `max_browser_sessions` 6, `max_http_inflight` 20 and
`max_contexts_per_engine` 3, which is `SIDECAR_MAX_CONTEXTS`;
`context_idle`, `sync_wall_time`, `visitor_wait`, `visit_idle`,
`visit_start_spacing`, `retry_pause`, `cooldown_floor`,
`cooldown_jitter`, `server_errors`, `breaker_max_pause`,
`breaker_trial_timeout`, `active_hours`), `browser` (`timeout`,
`challenge_wait_ms`, `scroll_depth`, and `patience`: `min` 6 s, `median`
38 s, `p95` 110 s, `max` 4 min, the distribution a page's patience is
drawn from, §10.4), `crawl` (`default_max_depth` 3, `default_max_pages`
100, `max_seeds` 100, `max_frontier` 50 000, `back_limit` 3,
`near_duplicate_bits` 6, `max_sitemaps` 10, `max_sitemap_urls` 50 000,
`skipped_extensions`; §13), `extract` (`thin_words` 80; §14), `robots`
(`ttl` 24 h, `error_ttl` 1 h, `max_bytes` 512 KiB, `agents` `["kage"]`,
`scout_quarantine` 30 d; §14), `telemetry` (`otlp_endpoint` none,
`service_name` `kage`, `export_timeout` 10 s; §20), `cache` (`max_ttl` 24 h, never longer than
`artifacts.retention_days`; §15), `bandwidth` and `forwarder` (§7.5).
`kage config check` reports the scheduler's caps, the budget and the
forwarder. `server` holds what the API's listener enforces (§19): `bind`
(`127.0.0.1:8080`), `metrics_bind` (unset), `max_body_bytes` (1 MiB),
`max_header_bytes` (16 KiB), `max_headers` (64), `max_uri_bytes` (8 KiB),
`header_read_timeout` (10 s), `body_read_timeout` (30 s),
`request_timeout` (30 s, for everything but a fetch, a self-test and a
proxy probe), `shutdown_grace` (30 s), `max_connections` (1024),
`requests_per_minute` (600 over all keys; 0 for none),
`cors_allowed_origins` (none) and `docs` (true). `security` holds
`allow_ip_targets`, `resolve_on_host` and `expose_set_cookie`, all false.
`jobs` holds `max_items` (10 000), `max_running` (8), `max_items_in_flight`
(24), `max_age` (7 d), `retention_days` (14), `lease` (1 m),
`poll_interval` (2 s), `idempotency_ttl` (1 d) and `wind_down` (150 s, at
most 15 m: how long a server that stops waits for the pages that are
being read, §16.6). `webhooks` holds
`allowed_origins` and `private_origins` (both empty: no job may have a
webhook; a private origin must also be an allowed one), `timeout` (10 s),
`max_retries` (5), `first_retry` (30 s), `backoff_factor` (4), `jitter`
(0.2), `max_retry_after` (1 h) and `poll_interval` (1 s) (§16.6).
`sidecar` holds
the token, each engine's URL, the timeouts and `connect_retries`;
every section rejects unknown keys and every non-secret key is listed in
`deploy/kage.example.yaml` (a test compares them).

Domain policy (`policies/domains.yaml`):

```yaml
defaults:
  render: auto
  pacing: human
  robots: ignore
  retries: 2
  breaker_threshold: 5
  breaker_pause: 30m
domains:
  "example.org":
    render: browser
    countries: ["DE", "AT"]
    engines: ["chromium", "firefox"]
    pacing: human
    pacing_locked: false
    robots: enforce
    cache: { shared: false, max_ttl: 6h, ttl: 10m }
    referer_strategy: search_engine
    min_dwell: 2s
    behavior_locked: false
    cooldowns: { rate_limited: 2h, forbidden: 24h }
  "api.lenient.example":
    render: http
    pacing: fast
```

Resolution cascades per field: exact host, then each parent domain from
nearest to furthest (suffix on label boundaries), then `defaults`, then the
built-in defaults; each field takes the value from the first level that sets
it, and `pacing_locked`, `min_dwell` (between 500 ms and 45 s: the floor
below which a request cannot shorten a persona's stay on a page),
`behavior_locked`, the per-outcome cooldowns and each member of `cache`
cascade the same way. `robots` is `ignore` unless set (§14: asking for
the file marks an address, so the operator says where a verdict is worth
one). `cache.shared` (false) says that the site shows every key the same
and one key's page may serve another's; `cache.max_ttl` bounds how long
its results are served, inside the configuration's; `cache.ttl` is what a
request gets that does not name `cache_ttl_s` (nothing unless set).
Reloaded on file change with typed validation (unknown keys rejected,
semantic checks, every named pacing profile must exist); an invalid file
keeps the last good policy and raises an alert.

---

## 18. Storage schema (Postgres)

Tables: `proxies`, `personas`, `persona_domain_state`, `sessions`,
`requests`, `attempts`, `results`, `artifacts`, `jobs`, `job_items`,
`crawl_frontier`, `robots_cache`, `site_scouts`, `response_cache`,
`api_keys`,
`audit_events`, `health_events`, `selftest_runs`, `domain_breakers`
(with how often the breaker opened since the domain last answered well,
`trips`, and when its one trial was let through, `trial_started_at`),
`http_cookies` (one row per cookie, keyed by proxy and persona; rows die
with the persona and with the proxy), `bandwidth_usage` (bytes up and down
through the forwarder per hour, persona and domain; §7.5),
`webhook_deliveries` and `webhook_attempts` (one row per event of a job on
its way to the job's webhook, and one per attempt at it; §16.6). A job
keeps its lease (`worker`, `lease_until`), its deadline, why and until
when it is paused, the keyed fingerprint of the request its
`Idempotency-Key` named and the visitors of each of its sites; an item its
site, the time before which it is not to be taken up, how often it was
taken up, its outcome and what a flow's step reported. `job_items.request_id`
is not a foreign key: an item names its request before the request is
written (§16.6). An attempt
stores its seed (`attempts.seed`: the persona's, the request's id and the
attempt's number, from which its behavior and its pauses are drawn), the
patience it had with its page (`patience_ms`, so that a visit replays
whatever the configuration says by then) and,
like a result, the `reason` of an outcome the site did not give. A crawl
keeps its frontier in `crawl_frontier` (per address: its position in the
crawl, site, depth, state and why it is not read, how and on which page it
was found, the positions it links to, where a redirect ended, content hash
and SimHash, what it repeats, a sitemap's `lastmod`, and the item that
carries its page) and, in `jobs.crawl`, its counters, its sitemaps and the
tab of each of its visitors; nothing of who reads is ever returned.
`robots_cache` holds one answer per origin (the file, or that there is
none, or that it could not be had; its site, when it was asked for and
when it is due again) and is never swept by age: a row that is due says
"ask again". `site_scouts` names the one persona per site (registrable
domain) that asks for what no reader asks for; the row goes with the
persona. `response_cache` points from a cache key to the result it
serves, with the normalized URL, the API key the entry is for (null where
a policy shares the site's pages), when the page was fetched and when the
entry expires; an entry goes with its result and is swept when it
expires. A result served from the cache has `cached` and `cached_at`
(both or neither), and artifact rows of its own that point at the stored
bodies; the daily page budget counts requests whose result is not
`cached`. A result keeps what its answer says of the visit besides the
page in `results.visit`, one document with a member for each part that
has something to say: `pacing`, `behavior`, `console`, `side_effects`
(§16.5; null for a result that keeps none of it), and, for a page of a
crawl until the crawl has recorded it, what the crawl took from the page
in `results.reading` (§13).
Migrations in `crates/kage-store/migrations`, applied by `kage migrate` and
checked in CI (`sqlx migrate info`, offline query data committed). Bodies and
artifacts live on disk under `artifacts/` keyed by content hash with a
retention job (`artifacts.retention_days`, default 14). Large text bodies
are gzip-compressed on disk.

---

## 19. Security

- **SSRF guard.** Every target, every redirect hop, every page a flow
  visited and every webhook URL is checked: non-http(s) schemes, credentials in the URL, port 0, names
  that never name a public site (`localhost`, `*.local`, `*.internal`, a
  name of one label, …) and literal addresses in loopback, link-local, RFC
  1918, CGNAT, multicast, IPv6 ULA and the cloud metadata ranges are
  denied, as is any literal address unless `security.allow_ip_targets`. A
  denied target is `422 ssrf_denied` (or `invalid_url`) before anything is
  fetched or stored; a denied hop ends the fetch with the outcome
  `ssrf_denied` and nothing read from it is returned or kept.

  By default Kage looks no name up: the proxy resolves every target, and a
  lookup from the Kage host would show the operator's resolver to the name
  servers of whatever a consumer asks for. With `security.resolve_on_host`
  the target of a fetch, the redirect hops of the http path and webhook
  hosts are also resolved with the system's resolver, and a name with any
  address in a denied range, or with none, is refused. Turn it on when the
  proxies could reach the operator's own networks (a static list of
  self-hosted proxies); a residential provider's exits cannot.

  A webhook is the one request that leaves from the host's own address, so
  the guard alone is not its rule: it goes only to an origin the operator
  listed (`webhooks.allowed_origins`), and its sender always resolves the
  name itself, checks every address, connects to a checked address and to
  no other, and follows no redirect (§16.6). An origin that is also in
  `webhooks.private_origins` may resolve to a private or loopback address;
  `kage config check` refuses a private origin that is not listed as
  allowed.
- **Secrets.** Proxy usernames and passwords both encrypted at rest with
  XChaCha20-Poly1305 under `KAGE__SECRETS__MASTER_KEY` (random 24-byte nonce
  per value; version byte, nonce, ciphertext and tag; associated data binds
  each value to its proxy id and field name so a value copied into another
  row or column fails to open); `kage proxy list --reveal-username`
  decrypts on demand and writes an audit event. API key secrets hashed;
  sidecar token constant-time compared; never logged (tracing field
  redaction tested).
- **Isolation.** Sidecars are on an internal network of their own
  (`browsers`) with kage and nothing else: no way out but kage's
  forwarder, no Postgres, no host, nothing published (`just hardening`
  checks the compose files, the smoke test the running stack); browsers run
  as non-root with every capability dropped, `no-new-privileges`, read-only
  root FS except profile and artifact mounts and `/tmp`; Chrome sandbox
  stays enabled (no `--no-sandbox`); the Kage container has no browser.
  The chromium sidecar runs under `deploy/seccomp/chromium.json`: Docker's
  default profile plus the four system calls Chrome's namespace sandbox
  needs (`chroot`, `clone`, `setns`, `unshare`), which the default
  refuses to a container without capabilities. The firefox sidecar keeps
  the default profile. The stack smoke test checks all of it on a running
  container of each engine with a context open.
- **Browser path.** Loopback is sent through the proxy (`<-loopback>`), so
  a page cannot reach the sidecar's own network. The redirect chain of a
  navigation is checked after the fact with the same functions as the http
  path; a denied hop is `ssrf_denied` and nothing read from it is returned.
  The request to such a hop has left through the proxy by then; that is the
  limit of a check that does not intercept requests. WebRTC sends no UDP
  outside the proxy.
- **Forwarder.** The provider's proxy credentials never leave the kage
  process (but for the stand-in self-tests of a proxy, §7.5): browsers and
  sidecars hold a per-persona credential that opens
  only that persona's own proxy, through the forwarder, which refuses what
  the SSRF guard refuses (§7.5). It is the one place where a browser's own
  background traffic and a page's redirect to the compose network would
  pass, and it is never published on the host.
- **Input limits.** A body of at most `server.max_body_bytes` on every
  endpoint (`413 payload_too_large`, whether it says its length or not)
  that arrives within `server.body_read_timeout` (`408`); a head of at
  most `server.max_header_bytes` with at most `server.max_headers` headers
  (`431`, by the HTTP layer) that arrives within
  `server.header_read_timeout`, which runs from the moment a connection is
  accepted and so also ends one that sends nothing or trickles its headers
  (slowloris); a request target of at most
  `server.max_uri_bytes` (`414`); at most `server.max_connections`
  connections; `server.request_timeout` for every endpoint that does not
  wait for the network. At most `jobs.max_items` items per job (10 000)
  and 100 steps per flow. A caller's headers pass a denylist: what is the persona's identity or the
  transport's (`User-Agent`, `Accept-Language`, `Accept-Encoding`,
  `Cookie`, `Origin`, `Host`, `Sec-*`, `Proxy-*` and the rest of §9's
  list) cannot be set.
- **Rate limiting.** Per key and global; `429` with `Retry-After` (§16.2).
- **The API's surface.** Loopback by default; plain HTTP, so a deployment
  that serves other machines puts a TLS-terminating reverse proxy in
  front. No CORS header unless origins are named, never credentials, never
  a wildcard. `/metrics` needs the admin scope, or its own listener on a
  private network. An artifact is served as what a site sent, never as a
  page of the API's origin (`nosniff`, a sandboxing CSP).
- **What a consumer sees of the pool.** A key without the admin scope sees
  a persona as a handle of its own, never a persona's id, a proxy, an
  address or a seed; the values of the cookies a site set are redacted for
  every key unless `security.expose_set_cookie` (§16.5). A problem
  document says what the caller can change and nothing of what failed
  inside.
- **Supply chain.** Exact pins, committed lockfiles, `cargo-deny` (licenses,
  advisories, duplicates, sources), `cargo-audit`, `pnpm audit`, SBOM
  (CycloneDX) per image, Trivy on images, gitleaks on every PR, signed
  release tags, images pinned by digest in compose.
- **Data hygiene.** Results may contain personal data from target pages;
  retention defaults are short (`artifacts.retention_days`,
  `jobs.retention_days`); `POST /v1/admin/purge` deletes what a key
  fetched before that time, with an audit event (§16.3); the README
  states responsible-use expectations.
- **Audit.** Every admin API call and CLI mutation writes an audit event
  with key id, action, target, and outcome; an admin call whose event
  cannot be written is not answered as done.

---

## 20. Observability

- Logs: JSON via tracing, one event per attempt with request id, persona
  id, domain, render, outcome, vendor, timing. Sidecar logs via pino in the
  same shape; both carry the trace id.
- Metrics (Prometheus): `kage_requests_total{domain,render,outcome,vendor}`,
  `kage_attempts_total{domain,render,outcome,vendor}`,
  `kage_attempt_duration_seconds{render,outcome}`, `kage_persona_state{state}`,
  `kage_proxy_state{state}`, `kage_scheduler_wait_seconds{kind}`,
  `kage_queue_depth{kind}`, `kage_scheduler_in_flight{render}`,
  `kage_scheduler_refusals_total{reason}`, `kage_breaker_open{domain}`,
  `kage_policy_breaker_threshold{domain}` (the blocks per hour that pause a
  domain under its policy, set at each attempt there),
  `kage_bandwidth_bytes_total{direction}`, `kage_bandwidth_budget_ratio`
  (the scheduler's are recorded through the `metrics` facade since M7 and
  exported with the API's since M8),
  `kage_selftest_score{persona,engine}`, `kage_replacements_total`,
  `sidecar_contexts_open{engine}`, `sidecar_navigation_duration_seconds`.
  The API adds `kage_api_requests_total{route,method,status}` (the route
  is the pattern, never the path a client sent),
  `kage_api_request_duration_seconds{route,method}`,
  `kage_api_requests_in_flight` and `kage_api_refusals_total{code}`.
  Jobs add `kage_jobs{job_type,state}`, `kage_job_items{state}` (what
  waits and what is being worked) and `kage_webhook_deliveries_pending`,
  read from the store at the scrape, and `kage_jobs_accepted_total{job_type}`,
  `kage_jobs_finished_total{job_type,state}`,
  `kage_job_items_total{job_type,state}` and
  `kage_webhook_deliveries_total{event,result}` (`delivered`, `retry`,
  `failed`) from the process. A crawl is a job (`job_type="crawl"`) and
  adds `kage_crawl_pages_total{reached}` (`typed`, `click`, `back_click`,
  `referer`), `kage_crawl_skipped_total{reason}` and
  `kage_crawl_duplicates_total{kind}` (`exact`, `near`). Extraction,
  `robots.txt` and the cache add `kage_extract_duration_seconds` (reading
  a page's document: its links, its text, its extraction),
  `kage_robots_requests_total{result}` (what came of asking for a file:
  `file`, `missing`, `unreachable`, `not_asked`, `no_scout`),
  `kage_robots_denied_total{reason}` (`rule`, `robots_unavailable`,
  `robots_no_scout`, `robots_enforced`, `robots_ignored`) and
  `kage_cache_requests_total{result}` (`hit`, `miss`, `stored`,
  `bypass` for a result the cache does not keep, and `joined` for a
  request that waited for an identical one under way). The scheduler's `kage_queue_depth{kind}` and
  `kage_scheduler_wait_seconds{kind}` have a `job` kind for what jobs have
  in the queue.
  `/metrics` is one exposition: the process's recorder (histograms of
  seconds with fixed buckets, so they aggregate), `kage_persona_state` and
  `kage_selftest_score` read from the store at the scrape, and each
  sidecar's own `/v1/metrics` merged in by family; a sidecar that does not
  answer is missing from the scrape. No label is a secret, a token or a
  proxy's address.
- Traces: OTLP over HTTP with protobuf from both processes, to the
  collector `telemetry.otlp_endpoint` names (Kage, through wreq, never
  through a proxy) and `SIDECAR_OTLP_ENDPOINT` (a sidecar); without one
  nothing is sent. One trace per request: `api.request` (the route as its
  pattern, the method, the status, the key's id) → `pipeline.fetch` (the
  request's id, its domain, the render asked for, how it ended) →
  `scheduler.wait` → `attempt` (its number, its render, the persona's id,
  outcome and vendor) → `sidecar.navigate` or `sidecar.flow`, on Kage's side
  as the client and on the sidecar's as the server, joined by the W3C
  `traceparent` of Kage's own request to the sidecar. A job's page is a
  trace of its own from `job.item`; a webhook attempt is `webhook.deliver`.
  No span holds a URL's path or query, an address, a seed, a token, a
  fingerprint or a body. Every JSON line of Kage and of a sidecar written
  inside a span carries `trace_id` and `span_id`, also when nothing is
  sent.
- Dashboards (optional profile): overview, per-domain block rate, persona
  health, pool, sidecar resources.

---

## 21. Testing and quality gates

| Layer | Tooling | What |
|---|---|---|
| Rust unit | cargo test | pacing math, classifier rules, policy resolution, URL normalization, SSRF guard, persona scoring, state machines |
| Rust integration | wiremock + a test CONNECT proxy + testcontainers Postgres | API handlers, provider client against a mock Webshare, store repositories, scheduler behavior with fake clock |
| Sidecar unit | vitest | behavior engine determinism, contract schemas, capture filters |
| Sidecar integration | vitest + Playwright against `tests/fixtures/site` | navigate, flow DSL, state export/import, artifact writing |
| Contract | both | sidecar OpenAPI ↔ Rust generated types, breaking-change check |
| E2E | compose + fixture site + mock Webshare | fetch, job, crawl, webhook, admin flows |
| Live (gated) | `KAGE_LIVE=1`, real proxies | fingerprint self-tests for every persona and engine, echo-service equality, detection page scores; nightly and pre-release |
| Static | rustfmt, clippy `-D warnings`, eslint, tsc, cargo-deny, cargo-audit, pnpm audit, gitleaks, trivy, SBOM | every PR |
| Coverage | cargo-llvm-cov, vitest coverage | ≥ 80 % lines on `kage-core`, `kage-store`, `kage-proxy`, `kage-http`, `kage-browser`, `kage-persona`, `kage-scheduler`, `kage-crawl`, `kage-extract`, `engine-core`; reported elsewhere |
| Mutation | cargo-mutants on `kage-core` | score reported; ≥ 70 % required from M7 on |

Branch protection on `main`: PRs only, required checks, linear history,
conventional commits enforced by a check. Release: semver tags, changelog
generated, images pushed with digests recorded in `deploy/compose.yaml`.

---

## 22. Deployment

- `deploy/docker/kage.Dockerfile`: multi-stage; builder has cmake, clang,
  nasm, perl, golang (BoringSSL build deps); runtime distroless or
  debian-slim non-root; `cargo auditable` build.
- `deploy/docker/sidecar.Dockerfile`: Node LTS base, build arg `ENGINE`;
  chromium variant installs Chrome stable at a pinned version, verified by
  the checksum of Google's package (`CHROME_VERSION`, `CHROME_SHA256`),
  Xvfb, Mesa and the font set of the distribution's desktop; firefox
  variant fetches the pinned Camoufox build by URL and checksum
  (`CAMOUFOX_VERSION`, `CAMOUFOX_RELEASE`, `CAMOUFOX_SHA256`); both install
  PulseAudio, which the sidecar starts with one output and one input that
  go nowhere (`SIDECAR_AUDIO=pulse`), and record versions under
  `/etc/kage/` (both: `engine`, `node-version`, `browser-version`,
  `display`, `audio`; chromium: `chrome-version`, `fonts`; firefox:
  `camoufox-version`). The Firefox image is built locally and not
  published (ADR 0008).
  `scripts/setup-chrome.sh` and `scripts/setup-camoufox.sh` install the
  same browsers on a CI runner or a developer machine from the same lines.
- `deploy/compose.yaml`: services, healthchecks, resource limits,
  `shm_size`, read-only FS, secrets, volumes, digests. The sidecars reach
  kage's forwarder at `http://kage:8899` on the compose network. `kage`
  has a `stop_grace_period` of 200 s: `jobs.wind_down` (150 s, §16.6) and
  50 s for the rest of a stop, because Docker kills a container 10 s
  after it was told to stop unless the file says otherwise.
- Runbooks in `docs/runbooks/`: first start, adding proxies, replacing an
  IP, rotating keys, upgrading an engine, reading a block report.
- Local dev: `just dev` brings up Postgres and sidecars in compose and runs
  `kage serve` natively (Linux/WSL2) with hot reload of policies.

---

## 23. Risks and mitigations

| Id | Risk | Mitigation |
|---|---|---|
| R1 | Camoufox and camoufox-js are labeled not production-stable; releases lag Firefox. | Firefox engine isolated behind the contract; Python fallback documented; persona mix can be set to 100 % chromium by config. |
| R2 | patchright and camoufox-js need different playwright-core versions. | Separate pnpm packages with their own pins; never hoist. |
| R3 | Chromium on Linux claiming Windows via injection may be caught on Client Hints coherence. | Self-test gate (§6.2); `allow_cross_os_claims` switch; future option: run the chromium engine on a real Windows host. |
| R4 | 20 IPs is a small pool; a few burns stall everything. | Conservative pacing, per-domain cooldowns, breaker, replacement budget, alerting at 25 % pool unavailable. |
| R5 | Vendors change checks weekly. | Signatures as data, nightly live suite, score history, fast pin bumps. |
| R6 | BoringSSL build fragility and OpenSSL conflicts. | CI check that no `openssl-sys` is linked; builder image pinned. |
| R7 | wreq-util has no profile for the engine's exact browser major. | Persona becomes browser-only automatically; alert so an upgrade is scheduled. |
| R8 | Residential ISP IPs are shared with other Webshare customers' history. | Reputation checks, replacement budget, per-domain cooldown learned from outcomes. |
| R9 | Legal and ethical exposure from scraping. | robots verdicts, responsible-use README, no credentialed scraping, short retention, purge endpoint. |

---

## 24. Future (v2 candidates)

Web Bot Auth signing module; pay-per-crawl (HTTP 402) handling; a Windows
engine host for native Chrome personas; more providers; a small web UI over
the admin API; per-site learned policies from outcome history; Redis-backed
scheduler if a second Kage instance is ever needed.

---

## 25. Camouflage levels and egress (2.0)

Decided by the owner on 2026-10-19 (D50, ADR 0012); built by M14. This
section is normative; the sections it touches (§1, §3.1, §6.1, §8.1,
§16.4, §17, §19) are updated by M14 to point here.

**The level.** Every request names `level`, an integer 0 to 3. A request
without one is refused with `level_required`; a job's `defaults.level`
applies to its items and a crawl's to its pages; a flow names 2 or 3.
The result says the level applied, and the metrics of fetches, jobs and
attempts carry a `level` label.

| Level | Name | What runs | Persona | Pacing | A site sees |
|---|---|---|---|---|---|
| 0 | plain | wreq with its own TLS and HTTP/2 settings; `User-Agent: kage/<version>` unless the caller sets one; the caller's headers and body as given; no cookie jar (a `Cookie` header is the caller's); redirects as a plain client follows them, through the SSRF chain check | none; `persona: null` | none of a persona's; the key's limits, the daily budget and the policy's robots and cache rules apply | a well-behaved HTTP client that says who it is |
| 1 | http-emulated | the persona's http client (§9): its verified browser profile, header tables, cookie jar, request kinds | yes | the persona's | a browser's wire fingerprint with no page behaviour; refused with `level_unavailable` while no verified major exists (F30) |
| 2 | browser-fast | the persona's real browser context, a plain navigation: no scrolling, no wandering pointer, no patience, the dwell at the site policy's `min_dwell`; captures, screenshots and extraction as at 3 | yes | the persona's pacing, visits and nights (the address's rhythm, D37) | a real browser that loads a page and leaves |
| 3 | human | everything of 1.0: the behaviour engine, the patience and the rest, the reader's order, the floors of D40 and D44 | yes | the persona's | a person |

**The floor.** A site's policy may set `min_level` (0 to 3, default 0;
the shipped policy file sets 3 for the detection pages). A request below
the floor is refused with `level_below_policy` and never raised. The
level is also the request's ceiling: escalation (§8.1) never goes above
it, and a block at level 0 or 1 is reported, not retried in a browser.

**Egress.** `egress.mode` is `proxies` (the pool, as in 1.0; the default)
or `direct` (the host's own connection). Under `direct`, one persona
stands for the host (`egress.direct.personas`, default 1, at most a
small number): its country, time zone, locale and languages come from
the host's observed geo through the pool's lookup (§7.4), or from
`egress.direct.country` and `egress.direct.locale` when declared; it is
generated, warmed, paced and self-tested like any persona; the forwarder
fronts every client and browser with no upstream, so the SSRF guard and
the byte count hold. Level 0's egress is `egress.plain`: `host` (the
default under `direct`) or `pool` (the pool's usable proxies in turn, as
plain proxies, no persona binding; the default under `proxies`). There
is no mixed mode and `kage config check` refuses one: a site that sees a
visitor move from a residential address to the host's has seen a bot.

**What a level never changes.** Persona handles, sessions (a session
needs a persona: `session_needs_persona` at level 0), jobs, crawls,
flows, webhooks, extraction, robots.txt, the cache (its key includes the
level) and the shape of a result. The SSRF guard, the key's scopes and
rate limits and the daily page budget apply at every level.

**Costs, said plainly** (the runbook's table, the OpenAPI description
and the README repeat it): below 3 Kage is not pretending to be a
person. Level 0 is for pages that can be read with `curl`; level 2 for
sites that need a browser but not a reader; level 1 waits for a
verified major. A request's level is the caller's responsibility and a
site's floor is the operator's.
