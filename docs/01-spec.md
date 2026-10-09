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
                                 │  per-context authenticated proxy
                                 ▼
                     Webshare static residential IPs ──▶ target sites

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
`artifacts`, `pgdata`. Only `kage` publishes a port.

---

## 4. Repository layout

```
kage/
  Cargo.toml                 workspace
  rust-toolchain.toml
  justfile                   every dev and CI command has a just target
  CLAUDE.md                  house rules for agents
  crates/
    kage/                    binary: CLI (serve, migrate, selftest, persona, proxy, key)
    kage-core/               domain types, outcomes, policy model, pacing math, classifier
    kage-store/              sqlx repositories, migrations/, artifact store
    kage-proxy/              provider trait, Webshare client, pool, health, replacement
    kage-http/               wreq client factory, cookie jar persistence, fingerprint self-test (http)
    kage-browser/            sidecar contract types (generated from sidecar OpenAPI) and client
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
    compose.yaml, compose.observability.yaml
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
| proxy_id | uuid | 1:1, unique |
| engine | enum | `chromium`, `firefox` |
| claimed_os | enum | `windows`, `macos`, `linux` |
| browser_major | int | from the engine's bundled browser at generation |
| fingerprint | jsonb | full generated fingerprint, re-applied on every session |
| wreq_profile | text nullable | wreq-util profile name; null disables the http path |
| locale, languages | text, text[] | derived from proxy country |
| timezone | text | IANA, derived from proxy geo |
| geolocation | jsonb nullable | lat/lon/accuracy near the proxy city |
| screen | jsonb | width, height, dpr, color depth |
| profile_dir | text | path under the personas volume |
| state | enum | `active`, `cooling`, `retired` |
| created_at, retired_at | ts | |

### PersonaDomainState

| Field | Notes |
|---|---|
| persona_id, domain | PK |
| cooldown_until | set on block |
| requests_today, counter_day, last_request_at | pacing counters; `counter_day` is the calendar day in the persona's timezone that `requests_today` counts, so the cap resets at local midnight |
| last_outcome | |
| block_count_24h | feeds circuit breaker |

### Session

| Field | Notes |
|---|---|
| id | caller-supplied (≤ 64 chars, `[A-Za-z0-9_.-]`) or server-minted; unique per API key, primary key is `(api_key_id, id)` |
| api_key_id | owner |
| persona_id | pinned persona |
| created_at, last_used_at, expires_at | idle TTL default 10 min, max lifetime default 6 h |

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

A persona is generated when a proxy enters `healthy` for the first time.

1. **Geo.** Country and city come from the provider record and are verified
   by the health check (exit IP geo lookup). Timezone comes from the city when
   known, else the country's primary zone. Locale is the country's primary
   language, with `en` as a secondary at lower q. Geolocation is a random
   point within a few kilometers of the city centroid, or null for the
   persona if the city is unknown.
2. **Engine and OS.** Weighted draw from `personas.mix` (default: chromium
   on windows 55%, chromium on macos 15%, firefox on windows 15%, firefox on
   macos 10%, firefox on linux 5%). The mix is configuration, and the health
   subsystem reports the actual pool distribution.
3. **Fingerprint.** The sidecar generates it from constraints (engine, OS,
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
   over the persona's life. A persona SHOULD warm up by visiting a small set
   of popular neutral sites during its first hours (configurable list), so
   that its profile is not empty on first contact with a target.

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
- The platform claimed by a chromium persona on a Linux host is produced by
  fingerprint injection. If the chromium self-test fails the Client Hints
  coherence check on the current Chrome build, the chromium engine MUST be
  restricted to `claimed_os = linux` personas until fixed (configuration
  flag `engines.chromium.allow_cross_os_claims`, default true, flipped by
  the operator or by the nightly suite opening an issue).
- Never rotate any identity attribute inside a persona's lifetime. Browser
  major may advance only when the engine image is upgraded; then every
  persona of that engine gets its `browser_major` and `wreq_profile` updated
  together and re-tested.

### 6.3 Lifecycle

| Event | Effect |
|---|---|
| Proxy enters `backoff` | Persona `cooling`; no new sessions; existing sessions finish. |
| Proxy `retired` (replaced) | Persona `retired`, profile dir archived then deleted after `personas.retention_days` (default 7). New persona generated for the replacement proxy. |
| Block on domain D | `PersonaDomainState.cooldown_until = now + cooldown(D, block_type)`; defaults: `rate_limited` 2 h, `forbidden`/`captcha`/`challenge_failed` 24 h, `soft_block` 6 h. A rule may carry a cooldown class instead: `none`, `short` (policy key `short`, default 30 min, never longer than the outcome's own cooldown) or `long` (the outcome's cooldown). M7 scales the duration by the classification confidence. |
| Daily cap reached on D | Persona ineligible for D until local midnight in the persona's timezone. |
| Self-test failure | Persona `cooling` until an operator or the next self-test clears it. |

### 6.4 State export and import (http ↔ browser)

After a browser attempt, the sidecar can export cookies and the relevant
storage for the origin. Kage stores them on the persona and loads cookies
into that persona's wreq jar. The http path MAY reuse them only when
`wreq_profile` is non-null and the persona's engine major equals the profile
major. Cookies set by the http path are imported back into the browser
context before the next browser attempt for that origin.

Until M6 gives each persona its jar, the http path keeps one jar per proxy
(table `http_cookies`, §9). It dies with the proxy: a replacement never
inherits cookies, because a new address presenting the old cookies links the
two.

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
plan reports a term, Kage's budget window MUST follow that term (follow-up
F1 in `02-plan.md`).

- Rate limits: general 240/min, proxy list endpoints 60/min; after a 429
  Webshare asks to wait 60 s. The client keeps its own token buckets below
  both (`webshare.requests_per_minute` 120, `webshare.list_requests_per_minute`
  30), honors `Retry-After`, and never sends a request that changes
  something twice after a failure that may have applied it.
- The Webshare client MUST be built on wreq (no second TLS stack). It is
  plain wreq without browser emulation and sends `User-Agent: kage/<version>`:
  Webshare is a vendor, not a target.
- Webshare replaces proxies by itself unless the four `auto_replace_*`
  settings are off. Kage reports them and alerts when one is on; it never
  changes account settings. The owner switches them off in the dashboard, so
  that an address (and its persona) only changes when Kage decided it.
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
requests in the current UTC month, automatic and operator alike, enforced
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
| Reachability, latency, exit IP | 5 min × U(0.75, 1.25) per proxy from its seed; first probes spread over 2 min after boot; at most 3 in flight; real traffic through a proxy postpones its next probe (`Pool::report_traffic_ok`) | Through-proxy GET of the provider's own echo (`https://ipv4.webshare.io/`), so no third party sees the pool; a failed probe is retried once against the confirmation echo (`https://checkip.amazonaws.com/`) before it counts. Client from the `ProbeClientFactory` seam: one wreq-util profile (`proxies.health.profile`, default `chrome_149` on Windows) until M6 substitutes the persona's own client. |
| Leak | with reachability | Exit IP (from echo) ≠ host public IP; host IP learned at boot by a direct GET of the same echo, refreshed hourly. Leak → `backoff` + `egress_unsafe` (§7.3). |
| Geo (country, city, ASN, timezone) | first check of a `new` proxy, on exit IP change, and every 24 h jittered | `https://ipinfo.io/json` through the proxy (not Cloudflare-fronted). The observed record is persisted as authoritative; a change is a health event. Mismatch with the provider country on two consecutive lookups → `degraded` and alert, proxy stays usable. Lookup failures are not health failures (retry after an hour). |
| Fingerprint self-test (http) | per PR in CI over loopback for every mapped profile; on demand (`kage selftest http --proxy --profile`) and two proxies per nightly live run against `selftest.http.third_party_url`; from M6 every 24 h jittered per persona, only against the operator's own echo (`selftest.http.echo_url`) and never against a third party on a timer | JA4, HTTP/2 Akamai string, header order, HTTP version and `User-Agent` equal the fixture recorded for the profile. The fingerprint is a function of the pins and the header code, not of the proxy, so the live runs only confirm that a real tunnel changes nothing. |
| Fingerprint self-test (browser) | 24 h, staggered | Sidecar loads the detection test page set (configurable list, default: CreepJS, BrowserScan, pixelscan, bot.sannysoft, a JA4/H2 echo) and asserts: no automation flags, OS/browser/timezone/locale as claimed, JA4 as expected. Produces a per-persona score. |
| IP reputation | 24 h, one proxy at a time spread over the day, asked from the host directly | Provider trait with AbuseIPDB first; disabled without a key. Bad = `abuseConfidenceScore` ≥ `reputation.bad_score` (default 75) on two consecutive lookups → `replacing` when the budget allows, else `degraded` + alert. `usageType` recorded; Data Center or Hosting on a residential IP raises `reputation_usage_type_suspicious`, never an automatic replacement. A 429 pauses lookups for as long as it says, or 24 h. |
| Account sync | 10 min | Subscription, plan, counts, replacement quota, the four `auto_replace_*` flags (alert while any is on, never changed by Kage), available countries. |

All health traffic obeys pacing and never targets real target sites.

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
  kage-http does all three itself. Until M6 a client is asked for with a
  profile name, an OS and a `ProxyEndpoint`; M6 asks with the persona and
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
  (`en-US, en` until M6 supplies the persona's).
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
- **Cookies.** One jar per proxy until M6 keys it by persona (§6.4), with
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
  a timer; the scheduled check of §7.4 comes with M6 and uses the operator's
  own echo. See `docs/runbooks/fingerprint-echo.md`.
- The http path never executes JavaScript and never pretends to; a page that
  requires it will surface as a block or as `ok` with thin content, which
  the extractor flags (`extract.thin_content = true`) so policy can learn.

---

## 10. Browser sidecar

### 10.1 Service shape

Fastify server, `ENGINE=chromium|firefox`, listens on the compose network
only, requires `X-Kage-Sidecar-Token` (shared secret). One browser process
per engine container with up to `SIDECAR_MAX_CONTEXTS` concurrent persona
contexts (default 3 per container; scale by running more containers). Each
persona context uses its own persistent profile directory
(`launchPersistentContext` or engine equivalent) and its own authenticated
proxy.

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
redirect chain, DOM HTML after settle, raw main body, captured XHR/fetch
responses (filtered by content-type and size), console errors, timing,
challenge signals (vendor scripts seen, interstitial detected and whether it
auto-resolved), screenshots and PDFs as artifact references (sidecar writes
them to the shared artifacts volume and returns paths).

### 10.3 Engines

**chromium.** patchright, `channel: "chrome"` (real Chrome stable installed in
the image), headful under Xvfb, no default viewport override (screen size
comes from the Xvfb screen matching the persona), fingerprint via
fingerprint-injector built from the persisted fingerprint, proxy via context
options. Rules from patchright's guidance: no custom UA through Playwright
options, no `addInitScript` from application code outside the injector, no
`Runtime.enable`-dependent APIs.

**firefox.** camoufox-js with the persisted fingerprint (`fingerprint` option),
`geoip` disabled in favor of explicit `locale`, `timezone`, `geolocation`
from Kage (deterministic), `virtualDisplay` or Xvfb, `humanize` off
(the shared behavior engine drives input instead so both engines behave the
same), proxy via options.

**Fallback (risk R1).** If camoufox-js proves unstable, `engine-firefox` is
replaced by a Python package implementing the same contract with the
upstream `camoufox` launcher. The contract does not change.

### 10.4 Behavior engine (engine-core)

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
  (visit the engine results page first, then click), or `none`.
- All randomness seeded per persona and request for reproducibility in tests.

### 10.5 Flow actions (v1 DSL)

`goto`, `click`, `type`, `press`, `scroll`, `wait_for` (selector | url |
network_idle | ms), `select`, `hover`, `back`, `extract` (capture point),
`screenshot`, `pdf`, `assert` (selector exists | text contains). Every
action has an optional `human: true|false` (default true) and `timeout_ms`.
Each step reports success, duration and a capture when requested.

### 10.6 Resource rules

`shm_size` ≥ 1 GB for Chromium, memory limit per container, hard context
cap, contexts are closed after `SIDECAR_CONTEXT_IDLE_MS` (default 2 min) and
reopened on demand (profile persists). A browser process is recycled after
`SIDECAR_MAX_PAGES_PER_BROWSER` (default 500) navigations.

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
its parameters as a value plus a seed, so M6/M7 can perturb median and p95
per persona (a fixed factor from the persona seed, roughly 0.8 to 1.25) and
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
`reputation`, `http` (with `http.subresources`), `selftest` (`selftest.http`),
`engines` (`engines.<engine>.browser_major`, unset until M4 and M5 pin the
browsers; a set major that no profile serves fails `kage config check`);
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
`audit_events`, `health_events`, `selftest_runs`, `domain_breakers`,
`http_cookies` (one row per cookie, keyed by proxy until M6 adds the
persona; rows die with the proxy).
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
  as non-root with seccomp default profile, `no-new-privileges`, read-only
  root FS except profile and artifact mounts; Chrome sandbox stays enabled
  (no `--no-sandbox`); the Kage container has no browser.
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
  `kage_attempt_duration_seconds`, `kage_persona_state{state}`,
  `kage_proxy_state{state}`, `kage_scheduler_wait_seconds`,
  `kage_queue_depth{kind}`, `kage_breaker_open{domain}`,
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
| Coverage | cargo-llvm-cov, vitest coverage | ≥ 80 % lines on `kage-core`, `kage-store`, `kage-proxy`, `kage-crawl`, `kage-extract`, `engine-core`; reported elsewhere |
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
  chromium variant installs Chrome stable and Xvfb; firefox variant fetches
  the pinned Camoufox build; both record versions under `/etc/kage/`.
- `deploy/compose.yaml`: services, healthchecks, resource limits,
  `shm_size`, read-only FS, secrets, volumes, digests.
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
