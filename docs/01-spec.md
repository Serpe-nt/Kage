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
3. Scheduler picks a persona: session-pinned if a session is given, otherwise
   the best-scored eligible persona for the domain (country match, engine
   match, not cooling down, lowest recent load), and waits for the pacing
   slot.
4. Executor runs the attempt on the http path or the browser path.
5. Classifier labels the outcome from status, headers, cookies, body and
   browser signals.
6. On a retryable block, the scheduler cools the persona down for that
   domain, picks another persona, and repeats up to the retry budget. On
   `http` blocks with `auto` render, the next attempt escalates to `browser`.
7. Result and artifacts are stored; the response is returned or the job
   record is updated and webhooks fire.

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
   (§6.2). The default mix is: chromium on linux 40%, firefox on windows
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
   deterministically.
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
  `coherence.webgl`) and the screen for both engines since.
- What surrounds the browser is part of the claim: a work area smaller
  than the screen (chromium: a desktop's top bar announced on the display
  and the window placed below it; firefox: the fingerprint's), media
  devices and an `AudioContext` with channels (a sound server in both
  images). The probe reports their absence as warnings
  (`environment.*`), never as failures.
- Never rotate any identity attribute inside a persona's lifetime. Browser
  major may advance only when the engine image is upgraded; then every
  persona of that engine gets its `browser_major` and `wreq_profile` updated
  together and re-tested.

### 6.3 Lifecycle

| Event | Effect |
|---|---|
| Proxy enters `backoff` or `replacing` | Persona `cooling` (`proxy_backoff`); no new sessions; existing sessions finish. Active again when the proxy is usable. |
| Proxy `retired` (replaced, by Kage or by the provider itself) | Persona `retired`: its sessions end and its http cookies are deleted at once; `personas.archive_after` (10 min) later its profile directory is moved to `.archive/<persona id>` on the personas volume with a `persona.json` (ids, engine, system, the proxy's address without credentials, reason, times), and the archive is deleted after `personas.retention_days` (default 7). The row, the fingerprint, the per-domain state and the self-test runs stay. New persona generated for the replacement proxy; it inherits nothing. |
| Geo record of the address changes | Same country: time zone and position follow, event `persona_geo_updated` at info. Another country (or a region of another language): locale and languages follow too, the event is a warning and the persona is `cooling` (`geo_changed`) until a self-test passes. Seed, fingerprint, screen and profile never change. |
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
context in its own process (`kage fetch`) binds a forwarder of its own on
a free port; `kage persona selftest` and the stand-in self-tests hand the
sidecar the provider's proxy as before M7.

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

### 8.2 Persona selection score

Eligible = `active`, proxy `healthy` or `degraded`, not cooling for domain,
daily cap not reached, engine ∈ policy.engines, country ∈ policy.countries
(if set), for http: `wreq_profile` non-null. Score = country preference
match (+3) + no visit to this domain in the last hour (+2) + low load (+1) +
previous `ok` on this domain (+1). Ties random.

### 8.3 Retry policy

`max_retries` default 2 (3 attempts). Retryable outcomes: `rate_limited`,
`challenge_failed`, `captcha`, `forbidden`, `soft_block`, `provider_error`,
`timeout`. Each retry uses a different persona. Non-retryable:
`ssrf_denied`, `robots_denied`, 4xx other than 403/429 (returned as `ok`
with the status). Circuit breaker per domain: if `block_count_1h` over all
personas ≥ `policy.breaker_threshold` (default 5), the domain is `paused`
for `policy.breaker_pause` (default 30 min); requests return
`provider_error` with `reason: domain_paused` immediately.

### 8.4 Timeouts

Per attempt: `timeout_ms` default 60 000 for browser, 30 000 for http; hard
cap 300 000. Sync `/v1/fetch` total wall time default 180 s, after which the
request is converted into a job and `202` returned with the job id.

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
  request is a navigation (GET, HEAD or POST) or one of the subresource
  kinds below; fetch()/XHR as a kind waits for the browser captures of M4
  and the API field of M8.
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
writes them to the shared artifacts volume and returns paths). A navigation
that fails is a page result too, with a typed `error` (`timeout`,
`navigation_failed`, `proxy_failed`, `aborted`) and whatever was captured
until then.

Captures must not change what the page experiences. Bodies come from
network events, never from request interception; the DOM is read in an
isolated world. A capture a page can notice is opt-in per request, off by
default, and named in `side_effects`: a full-page screenshot (`resize`)
and a PDF (`beforeprint`, `afterprint`).

Reading a page grants it nothing. Both drivers mark an evaluation as a
user gesture (patchright sends `userGesture: true`; the Juggler of the
pinned Camoufox build calls `setHandlingUserInput` and
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
  The driver keeps the launch, navigation, the waits for a load state and
  a URL, network events, and the input of `human: false` steps.
- On firefox the pinned build runs with upstream's fix of its Juggler
  backported into `omni.ja` (ADR 0010); the engine refuses a build without
  it and clears a profile's startup cache when the profile last started
  under another archive.
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

The contract is at 1.1.0: everything M5 added is optional in requests and
additional in responses.

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

**firefox.** Camoufox at a pinned build (`v152.0.4-beta.30`, fetched by
URL and checksum, never by the launcher's own download; the engine refuses
any other build, and this one without the Juggler backport of ADR 0010,
§10.2), driven through its own playwright-core, one browser per
context on its own Xvfb display, started through a wrapper that sets the
persona's CPU affinity. The fingerprint is generated from a 64-hex seed
with the parts of camoufox-js (the real-device generator under the seeded
swap, the mapping to the browser's configuration, the font list and a
graphics card of the claimed system drawn from the launcher's table) and
persisted as the complete browser configuration; opening is a pure launch
plan from that document. The launcher's `launchOptions` is not called: it
is not reproducible, points fontconfig at a directory the pinned build
does not have, and depends on ua-parser-js (ADR 0008). Language, time
zone and position come from the open request (`geoip` is never used),
`humanize` stays off (the shared behavior engine drives input), no addon
is loaded, the build's window theming is off, and autoplay is Firefox's
own default (the build's allows it, which a page reads with
`navigator.getAutoplayPolicy()`). The persona's fontconfig
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
  text density; sometimes scroll back up; reach `behavior.scroll_depth`.
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
  search template is enabled by default.
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
step is done by the behavior engine through the display: a `select` is
opened with a click and chosen with the arrow keys and `Enter`, `back` is
`Alt` and the left arrow. A step with `human: false` is done by the
driver and is visible to a page as such. The first step that fails ends
the flow; a flow's `behavior` gives the seed and whether typing makes
typos.

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
they take a lease (`BrowserGate`); they close a context only when they
opened it. A browser process is recycled after
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
request value ignored (the response reports `pacing.applied`). Global caps:
`scheduler.max_browser_sessions` (default 6), `scheduler.max_http_inflight`
(default 20). The scheduler is a priority queue (sync requests before jobs,
then FIFO) with per-domain and per-persona gates; it never busy-waits.

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

Job type `crawl`: `seeds[]`, `allow[]`/`deny[]` (regex on normalized URL),
`max_depth`, `max_pages`, `same_site_only`, `use_sitemaps`, `include[]` per
page, `pacing`, `personas` (count or explicit ids). Frontier rows in
Postgres with `SELECT … FOR UPDATE SKIP LOCKED` workers. URL normalization:
scheme/host lowercase, default ports removed, fragment removed, tracking
params removed by configurable list, trailing-slash policy, sorted query.
Dedupe by normalized URL and by content hash (SimHash near-duplicate flag).
Politeness uses the same pacing as everything else; a crawl SHOULD stay on
one persona per site to look like one visitor, with `personas` > 1 only
when the caller asks. Discovered links come from the rendered DOM (browser
path) or the raw HTML (http path). Results stream into `job_items` and are
paginated through the API.

---

## 14. Extraction and robots

`include: ["extract"]` yields: `title`, `description`, `canonical`, `lang`,
`meta` (name/property map), `jsonld[]`, `opengraph`, `links[]`
(absolute, rel, text, same_site), `text` (readability-style main content),
`markdown`, `word_count`, `thin_content` flag, `headings[]`.

robots.txt: fetched per host via the http path with the persona in use,
cached 24 h, parsed with the Google matcher. Verdict object on every result:
`{ "fetched": true, "allowed": true, "matched_rule": "...", "crawl_delay": null }`.
Enforcement only when policy `robots: enforce`; `ignore` skips fetching.

---

## 15. Caching

Opt-in via `cache_ttl_s`. Key = normalized URL + method + body hash +
render + include set + persona country. Served only for `ok` outcomes.
Response carries `cached: true` and `cached_at`. Stored in Postgres with
bodies in the artifact store.

---

## 16. Public API

### 16.1 Conventions

- Base path `/v1`. JSON only. `Content-Type: application/json; charset=utf-8`.
- Errors follow RFC 9457 problem details with a stable `code`.
- Every response has `X-Request-Id`; `Idempotency-Key` is honored on
  `POST /v1/jobs` for 24 h.
- OpenAPI 3.1 at `/openapi.json`, Swagger UI at `/docs` (admin scope).
- Liveness `/healthz`, readiness `/readyz` (DB, at least one healthy
  persona, at least one sidecar), metrics `/metrics` (internal network or
  admin scope).

### 16.2 Authentication and authorization

`Authorization: Bearer kage_<keyid>_<secret>`. Secret is 32 random bytes,
base64url. Stored as SHA-256 of the secret with the key id as lookup;
shown once at creation. Scopes: `fetch`, `jobs`, `crawl`, `sessions`,
`admin`. Per-key limits: requests per minute, concurrent in-flight, daily
page budget. Keys can be disabled and rotated via admin endpoints or the
CLI. All admin actions are written to `audit_events`.

### 16.3 Endpoints

| Method and path | Scope | Purpose |
|---|---|---|
| `POST /v1/fetch` | fetch | Synchronous fetch (may return 202 + job on wall-time overflow) |
| `POST /v1/jobs` | jobs / crawl | Create `batch`, `flow` or `crawl` job |
| `GET /v1/jobs/{id}` | jobs | Status, counters, timing |
| `GET /v1/jobs/{id}/items?cursor=&limit=` | jobs | Paginated results |
| `POST /v1/jobs/{id}/cancel` | jobs | Cooperative cancel |
| `GET /v1/results/{id}` | fetch | A stored result by id |
| `GET /v1/artifacts/{id}` | fetch | Screenshot, PDF, raw body stream |
| `POST /v1/sessions` | sessions | Mint a session pinned to a persona matching hints |
| `GET /v1/sessions/{id}` / `DELETE` | sessions | Inspect or end |
| `GET /v1/policies/domains/{domain}` | fetch | Effective policy preview |
| `GET /v1/admin/personas`, `POST …/{id}/retire`, `POST …/{id}/selftest` | admin | |
| `GET /v1/admin/proxies`, `POST …/{id}/replace`, `POST …/{id}/health` | admin | |
| `GET /v1/admin/pool` | admin | Pool and bandwidth summary |
| `POST /v1/admin/policies/reload` | admin | Force reload |
| `GET/POST/DELETE /v1/admin/keys` | admin | Key management |
| `GET /v1/admin/audit?cursor=` | admin | |

### 16.4 FetchRequest

```json
{
  "url": "https://example.org/listing?page=2",
  "method": "GET",
  "headers": { "Referer": "https://example.org/" },
  "body": null,
  "render": "auto",
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
  "cache_ttl_s": 0,
  "robots": "policy",
  "extract": { "markdown": true, "links": true },
  "xhr": { "content_types": ["application/json"], "max_bytes": 2000000 },
  "max_body_bytes": 10000000
}
```

Validation: `url` http(s) only, ≤ 8 KB, SSRF guard (§19); `headers` ≤ 32
entries, denylist enforced; `include` ⊆ {`dom`,`raw`,`headers`,`extract`,
`xhr`,`screenshot`,`pdf`,`console`}; `render: http` with `include: dom`
is rejected (no DOM without a browser) with code `render_include_conflict`.

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
  "robots": { "fetched": true, "allowed": true },
  "pacing": { "applied": "human", "waited_ms": 3900 },
  "cached": false
}
```

Blocked example: `outcome: "challenge_failed"`, `block: { vendor, type,
confidence, evidence }`, `attempts` lists each persona tried, `status` is
the last status.

### 16.6 Jobs and webhooks

Job states: `queued`, `running`, `paused`, `succeeded`, `partial`,
`failed`, `cancelled`. Webhook: `webhook: { url, secret }` on job creation;
events `job.completed`, `job.failed`, `job.progress` (every N items);
body signed with `X-Kage-Signature: sha256=<hmac>` over timestamp + body;
retries with exponential backoff up to 5. Webhook URLs pass the same SSRF
guard as targets.

---

## 17. Configuration

`kage.yaml` layered with env `KAGE__SECTION__KEY=value` (figment). Secrets
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
`retention_days`, `archive_after`, `sweep_interval`, `warmup`), `sessions`
(`idle_ttl`, `max_lifetime`, `sweep_interval`),
`engines` (`engines.<engine>.browser_major`: 155 for the chromium image of
M4, 152 for the firefox image of M5; a set major must be served by a profile,
verified or recorded as browser-only, or `kage config check` fails;
`engines.chromium.allow_cross_os_claims`, default false), `scheduler`
(the caps `max_browser_sessions` 6, `max_http_inflight` 20 and
`max_contexts_per_engine` 3, which is `SIDECAR_MAX_CONTEXTS`;
`context_idle`, `sync_wall_time`, `visitor_wait`, `visit_idle`,
`visit_start_spacing`, `retry_pause`, `cooldown_floor`,
`cooldown_jitter`, `server_errors`, `breaker_max_pause`,
`breaker_trial_timeout`, `active_hours`), `browser` (`timeout`,
`challenge_wait_ms`, `scroll_depth`), `bandwidth` and `forwarder` (§7.5).
`kage config check` reports the scheduler's caps, the budget and the
forwarder. `sidecar` holds
the token, each engine's URL, the timeouts and `connect_retries`;
every section rejects unknown keys and every non-secret key is listed in
`deploy/kage.example.yaml` (a test compares them).

Domain policy (`policies/domains.yaml`):

```yaml
defaults:
  render: auto
  pacing: human
  robots: report
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
    referer_strategy: search_engine
    cooldowns: { rate_limited: 2h, forbidden: 24h }
  "api.lenient.example":
    render: http
    pacing: fast
```

Resolution cascades per field: exact host, then each parent domain from
nearest to furthest (suffix on label boundaries), then `defaults`, then the
built-in defaults; each field takes the value from the first level that sets
it, and `pacing_locked` and the per-outcome cooldowns cascade the same way.
Reloaded on file change with typed validation (unknown keys rejected,
semantic checks, every named pacing profile must exist); an invalid file
keeps the last good policy and raises an alert.

---

## 18. Storage schema (Postgres)

Tables: `proxies`, `personas`, `persona_domain_state`, `sessions`,
`requests`, `attempts`, `results`, `artifacts`, `jobs`, `job_items`,
`crawl_frontier`, `robots_cache`, `response_cache`, `api_keys`,
`audit_events`, `health_events`, `selftest_runs`, `domain_breakers`
(with how often the breaker opened since the domain last answered well,
`trips`, and when its one trial was let through, `trial_started_at`),
`http_cookies` (one row per cookie, keyed by proxy and persona; rows die
with the persona and with the proxy), `bandwidth_usage` (bytes up and down
through the forwarder per hour, persona and domain; §7.5). An attempt
stores its seed (`attempts.seed`: the persona's, the request's id and the
attempt's number, from which its behavior and its pauses are drawn) and,
like a result, the `reason` of an outcome the site did not give.
Migrations in `crates/kage-store/migrations`, applied by `kage migrate` and
checked in CI (`sqlx migrate info`, offline query data committed). Bodies and
artifacts live on disk under `artifacts/` keyed by content hash with a
retention job (`artifacts.retention_days`, default 14). Large text bodies
are gzip-compressed on disk.

---

## 19. Security

- **SSRF guard.** Resolve every target and webhook host before connecting;
  deny loopback, link-local, RFC 1918, CGNAT, multicast, IPv6 ULA and the
  cloud metadata ranges; re-check on every redirect hop; deny non-http(s)
  schemes and literal IP targets unless `security.allow_ip_targets`. The
  proxy performs the connection, but the guard still runs on the resolved
  name because the proxy could reach the operator's own networks.
- **Secrets.** Proxy usernames and passwords both encrypted at rest with
  XChaCha20-Poly1305 under `KAGE__SECRETS__MASTER_KEY` (random 24-byte nonce
  per value; version byte, nonce, ciphertext and tag; associated data binds
  each value to its proxy id and field name so a value copied into another
  row or column fails to open); `kage proxy list --reveal-username`
  decrypts on demand and writes an audit event. API key secrets hashed;
  sidecar token constant-time compared; never logged (tracing field
  redaction tested).
- **Isolation.** Sidecars listen only on the compose network; browsers run
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
  process: browsers and sidecars hold a per-persona credential that opens
  only that persona's own proxy, through the forwarder, which refuses what
  the SSRF guard refuses (§7.5). It is the one place where a browser's own
  background traffic and a page's redirect to the compose network would
  pass, and it is never published on the host.
- **Input limits.** Body size limits on all endpoints, header count and
  size limits, URL length, items per job (default 10 000), artifact size.
- **Rate limiting.** Per key and global; `429` with `Retry-After`.
- **Supply chain.** Exact pins, committed lockfiles, `cargo-deny` (licenses,
  advisories, duplicates, sources), `cargo-audit`, `pnpm audit`, SBOM
  (CycloneDX) per image, Trivy on images, gitleaks on every PR, signed
  release tags, images pinned by digest in compose.
- **Data hygiene.** Results may contain personal data from target pages;
  retention defaults are short; an admin purge endpoint exists; the README
  states responsible-use expectations.
- **Audit.** Every admin API call and CLI mutation writes an audit event
  with key id, action, target, and outcome.

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
  `kage_bandwidth_bytes_total{direction}`, `kage_bandwidth_budget_ratio`
  (the scheduler's are recorded through the `metrics` facade since M7 and
  exported with the API's in M8),
  `kage_selftest_score{persona,engine}`, `kage_replacements_total`,
  `sidecar_contexts_open{engine}`, `sidecar_navigation_duration_seconds`.
- Traces: OTLP from both processes; one trace per request spanning API →
  scheduler → attempt → sidecar navigate.
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
  kage's forwarder at `http://kage:8899` on the compose network.
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
