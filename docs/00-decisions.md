# Kage: decision record from discovery

Date: 2026-10-08. Source: structured Q&A with the project owner. Every row here
is a settled decision unless marked "open". Agents must not re-litigate these
without an ADR that supersedes them (see `docs/adr/`).

## Mission

Kage is a self-hosted API service that fetches web pages for the owner's
personal projects while being indistinguishable from a real human user at
every layer software can control: network egress, TCP/TLS/HTTP fingerprints,
browser fingerprint, JavaScript environment, and input behavior. It must run
excellently on any site it is pointed at; there is no fixed target list.

## Decisions

| # | Area | Decision |
|---|------|----------|
| D1 | Browser | A real browser is core, not a fallback. It runs in an anti-detect browser sidecar (TypeScript on Node, Playwright-compatible). Rust talks to it over an internal HTTP/JSON API. |
| D2 | Engines | Two engines behind one contract: patched Chromium (patchright, real Chrome channel, CDP leaks hidden) for Chrome personas and Camoufox (Firefox fork with C++-level fingerprint injection) for Firefox personas. Persona mix is Chrome-weighted to follow real market share. |
| D3 | Display | Headful under a virtual display (Xvfb or Camoufox's built-in virtual display) in Linux containers. Never new-headless in production. |
| D4 | Fetch path | Per-domain policy decides `http` (wreq) or `browser`. Unknown or protected domains default to browser. Automatic escalation http → browser on block. Cookies earned by the browser may be reused by wreq only with a matching fingerprint profile. |
| D5 | Proxies | Webshare Static Residential (ISP) proxies: a fixed list of IPs managed through the Webshare list and replacement API. Plan bought 2026-10-09: **Shared** exclusivity (the IPs are used by other Webshare customers too; Private and Dedicated were priced out), **250 GB/month** bandwidth, 20 IPs, **No Refreshes** (never a scheduled rotation), **50 manual replacements per month**, **High IP Reputation** add-on. Kage's automatic budget `proxies.replacements_per_month` stays small (code default 5; set it to 10 in the deployed `kage.yaml`) because an automatic replacement also retires the persona; the remaining replacements are for the owner's manual `kage proxy replace` when reputation or usage-type alerts fire. Countries: DE 4, IT 3, FR 3, GB 2, AT 2, BE 2, RO 2, SE 2. Consequences: reputation lookups, per-domain cooldowns learned from outcomes and the replacement budget are the defence against shared-IP history; Kage must track bandwidth against the 250 GB cap and warn at 80 % (scheduled for M7 with the scheduler, alert routing in M12). |
| D6 | Personas | One long-lived persona per proxy IP (1:1). A persona owns a generated real-device fingerprint, engine, OS, locale, timezone and geolocation coherent with the IP, a persistent browser profile (cookies, storage, history), and a matching wreq emulation profile for the http path. |
| D7 | Fingerprints | Generated from a real-device fingerprint database (Apify fingerprint-suite for Chromium, Camoufox's generator for Firefox), pinned for the persona's lifetime, re-applied deterministically on every session. |
| D8 | Behavior | Full human emulation: Bezier mouse paths with overshoot, scroll with reading pauses, dwell from a human-like distribution, per-key typing cadence, idle periods. |
| D9 | Pacing | Human-like defaults per persona and per domain, with per-domain policy overrides. The caller may fully control pacing per request unless the domain policy locks it. |
| D10 | Blocks | Detect and classify by vendor and type. Let the browser pass silent challenges natively. No external CAPTCHA solvers, no manual solving. Retry with another persona up to N, cool the blocked persona down for that domain, per-domain circuit breaker, then report the classified block. |
| D11 | API | REST/JSON with OpenAPI 3.1 generated from the Rust types. Sync `POST /v1/fetch`, async jobs for batches, navigation flows and crawls. |
| D12 | Output | Everything is available and the caller selects via an `include` list: rendered DOM HTML, raw network body, headers and status, extracted text/markdown/metadata/links, captured XHR/fetch responses, screenshots, PDFs. |
| D13 | Interactions v1 | Page loads, scripted navigation flows (click, type, scroll, paginate, wait), screenshots and PDFs. No logins with owner accounts in v1. |
| D14 | Crawling | Frontier-based crawler is in v1: seeds, allow/deny, depth, page caps, sitemaps, dedupe, politeness via pacing. |
| D15 | robots.txt | Fetch and cache, report the verdict on every response, enforce only where the domain policy says so. |
| D16 | API auth | Per-consumer API keys with scopes and rate limits, stored hashed. |
| D17 | Direct mode | Never. If no usable proxy exists the request fails. The host IP never touches a target. |
| D18 | Replacement | A burned IP is replaced automatically through the Webshare API after sustained failure and backoff, bounded by a monthly replacement budget. Its persona is retired and a new one is generated. |
| D19 | Health | Reachability, latency, leak check, country match; fingerprint self-tests against detection test sites; IP reputation lookup on a slow timer; Webshare account sync via API. |
| D20 | Storage | PostgreSQL via sqlx with in-repo migrations. No ORM, no Redis. |
| D21 | Observability | Structured JSON logs (tracing), Prometheus metrics, OpenTelemetry traces. Dashboards and alerting are optional compose profiles. |
| D22 | QA gates | Every PR: rustfmt, clippy with warnings denied, unit and integration tests, sidecar lint/typecheck/tests, cargo-deny, cargo-audit, npm audit, secret scanning, SBOM, image scan, coverage threshold. Mutation testing reported on core crates. Live fingerprint regression suite runs nightly and before releases, not per PR. |
| D23 | Layout | Monorepo: Cargo workspace under `crates/`, Node pnpm workspace under `sidecar/`, `deploy/`, `docs/`, `policies/`. |
| D24 | Runtime | Linux server or VPS with Docker Compose. Development on Windows through WSL2 or a devcontainer. |
| D25 | Process | One milestone per agent session, one PR per milestone through the CI gates, ADRs for architecture decisions, `CLAUDE.md` with house rules. |
| D26 | License | Public repository, dual MIT / Apache-2.0, with a responsible-use section in the README. |
| D27 | Go project | `CarPriceScanner` is reference only. Nothing from it is binding. |
| D28 | AGPL dependency | ADR 0008 accepted with option B then C: M5 overrides `ua-parser-js` to the MIT 1.x line under the engine parity suite; if behavior differs, the Firefox image is not published and is built locally by operators. |
| D29 | Camoufox pin | M5 moves the Camoufox browser build to `v152.0.4-beta.30`, the only build `camoufox-js` 0.12.1 accepts with `playwright-core` 1.62.1, as the fingerprint-critical bump inside the M5 PR. |
| D30 | Merging | Until the repository is pushed, milestone branches are fast-forwarded into local `main` by the owner and deleted; the PR workflow in D25 starts once `main` is on origin. |

## Open items (owner to confirm when relevant)

| # | Item | Default until answered |
|---|------|------------------------|
| O1 | Exact Webshare countries purchased | Answered, see D5. Personas inherit country from the observed geo of their IP. |
| O2 | Host sizing (RAM, CPU) for the browser pool | Spec assumes 16 GB RAM, 4 vCPU, 6 concurrent browser sessions. |
| O3 | IP reputation provider and API key | Spec defines a provider trait with AbuseIPDB first; disabled until a key exists. |
| O4 | Webhook delivery targets | HMAC-signed webhooks are built; none configured by default. |

## Honest limits (from `bot-detection-landscape.md`)

Even a perfect browser cannot pass hardware-rooted device attestation (Private
Access Tokens, PACT, Play Integrity), and some vendors specifically probe the
automation transport (CDP, patched runtimes). The design therefore treats
"indistinguishable" as a measured property: a fingerprint and behavior test
suite runs continuously against public detection test pages and the service
reports its own detection score per persona and engine. Regressions in that
score block releases.
