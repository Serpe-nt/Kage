# Kage build plan

Companion to `01-spec.md`. Each milestone is sized for one focused agent
session and lands as exactly one PR into `main` through the CI gates. Section
numbers (§) refer to the spec.

## How agents work this plan

1. Read `CLAUDE.md`, `00-decisions.md`, the spec sections the milestone
   names, and `versions.md`. Do not re-open settled decisions; write an ADR
   if a decision must change and stop for owner review.
2. Start from `main`, branch `m<N>-<slug>`, conventional commits.
3. Definition of done for every milestone, in addition to its own criteria:
   - all new code has tests at the layer the spec names in §21;
   - `just ci` passes locally; CI green;
   - no new `unwrap`/`expect` in library crates outside tests;
   - public types documented; OpenAPI regenerated and committed if touched;
   - `docs/versions.md` updated for any new dependency;
   - a short entry in `CHANGELOG.md` under Unreleased;
   - the PR description lists how each acceptance criterion was verified.
4. If a milestone turns out larger than one session, split it into
   `M<N>a`, `M<N>b` with the same acceptance criteria divided, and say so in
   the PR.
5. Live tests (`KAGE_LIVE=1`) need real proxies and the owner's Webshare
   key. Agents never run them against real target sites, only against the
   echo and detection test pages listed in configuration.

## Dependency graph

```
M0 ─▶ M1 ─▶ M2 ─▶ M3 ─▶ M6 ─▶ M7 ─▶ M8 ─▶ M9 ─▶ M10 ─▶ M12 ─▶ M13
            │      ▲            ▲                 ▲
            └─▶ M4 ┴───▶ M5 ───┘           M11 ──┘
```

M4 and M5 (sidecar) can run in parallel with M2 and M3 in separate sessions
because they touch disjoint directories. M11 (extraction) can run in parallel
with M9 and M10.

---

## M0 — Repository scaffold and CI

**Goal.** An empty but fully gated monorepo that builds, tests and ships
images.

**Scope.** §4, §21, §22, `versions.md`.

**Deliverables**
- Cargo workspace with all crates from §4 as empty libraries with a smoke
  test each; `rust-toolchain.toml`; edition 2024; workspace lints (clippy
  `all` + selected `pedantic`, `unsafe_code = "forbid"` in libs).
- Sidecar pnpm workspace with the five packages, TypeScript strict, eslint,
  prettier, vitest, a `/v1/health` endpoint, `ENGINE` switch stub.
- `justfile` with targets: `setup`, `fmt`, `lint`, `test`, `test-e2e`,
  `test-live`, `cov`, `deny`, `audit`, `sbom`, `build-images`, `dev`,
  `ci` (runs everything a PR runs), `migrate`, `openapi`.
- Dockerfiles for kage and both sidecar variants (engines may be stubs);
  compose with Postgres 17 and healthchecks; `.dockerignore`.
- GitHub Actions: `ci.yaml` (rust, node, security, images), `nightly-live.yaml`
  (skeleton that fails clearly without secrets), `release.yaml` (tags →
  images with digests). Branch protection documented in `docs/runbooks/repo.md`.
- `cargo-deny` config with license allowlist (MIT, Apache-2.0, BSD, ISC,
  MPL-2.0 for Camoufox pieces), `cargo-audit`, `gitleaks`, `trivy`,
  CycloneDX SBOM artifacts.
- `cargo tree -i openssl-sys` check script that fails if non-empty.
- `README.md` with purpose, responsible-use section, quick start; `LICENSE-MIT`,
  `LICENSE-APACHE`; `CHANGELOG.md`; `CODEOWNERS`.
- Pin verification: update `versions.md` with the exact versions resolved.

**Acceptance**
- `just ci` passes on a clean clone in WSL2 and in CI.
- `docker compose up` starts kage (serving `/healthz`), both sidecars
  (serving `/v1/health`) and Postgres.
- A PR without tests for a new Rust module fails the coverage gate (verify by
  a throwaway PR, then close it).

**Out of scope.** Any real feature.

---

## M1 — Core domain, configuration, store

**Scope.** §5, §17, §18, §12 (signature data model only).

**Deliverables**
- `kage-core`: all domain types and enums, `Outcome`, `Block`, policy model
  with resolution order and suffix matching, pacing profile model and
  samplers (seeded RNG), classifier rule model with matchers, SSRF guard
  (pure functions over resolved addresses), URL normalization (§13).
- Configuration via figment with env layering, secrets from env/files,
  `kage config check`, redaction tested.
- `policies/*.yaml` initial files with schema validation and hot reload
  (notify) keeping last-good on error.
- `kage-store`: migrations for every table in §18, repositories with
  sqlx offline data, testcontainers-based integration tests, artifact store
  (content-addressed, gzip for text, retention job).
- Block signature fixtures: at least one recorded page per vendor in §12 in
  `tests/fixtures/blocks/` (sanitized) with classifier tests.

**Acceptance**
- Classifier identifies every fixture with the expected vendor and type.
- Policy resolution tests cover exact, suffix, defaults, lock, invalid file.
- Migrations apply from empty and are idempotent; `sqlx migrate info` clean.
- SSRF guard tests cover every denied range and redirect re-check semantics.
- Coverage ≥ 80 % on `kage-core` and `kage-store`.

---

## M2 — Proxy provider, pool, health, replacement

**Scope.** §7, §19 (credential encryption).

**Deliverables**
- `ProxyProvider` trait, `WebshareProvider` on wreq with token buckets and
  429 backoff, `StaticListProvider`.
- Mock Webshare server for tests (wiremock) covering list paging,
  replacement lifecycle, replaced list, account, stats, 429s.
- Pool state machine with persistence, reconciliation loop, backoff math
  with jitter, replacement budget and alert event.
- Health checker: reachability/latency, leak, country match, IP reputation
  trait with AbuseIPDB implementation (disabled without key), account sync.
- Credential encryption at rest (XChaCha20-Poly1305) with key from env.
- CLI: `kage proxy list|health|replace`.

**Acceptance**
- Verified Webshare endpoint paths and parameters recorded in the PR with
  links; any deviation from §7.2 noted in the spec.
- State machine property tests: no transition outside the diagram; backoff
  never exceeds the cap; budget never exceeded.
- Health checks run against a local echo in integration tests; leak and
  geo mismatch produce the right states.
- Live (gated): `kage proxy health` against the owner's real list reports
  all 20 proxies with latency and country.

**Status.** Merged as `6b28ca2`. Live check passed on 2026-10-09 (20 of 20,
no leak, observed geo equals provider geo; `m2-live-handover.md` in the
planning session's notes). Follow-ups it raised, each small and owned by the
milestone named:
- **F1 (M6, kage-proxy):** count the replacement budget per provider
  subscription term when the account reports one, else per UTC month.
- **F2 (M3):** the probe reports one latency for CONNECT plus TLS plus GET;
  measure connect and request time separately, because the four GB proxies
  (one ASN, one /16) take 3.2 to 4.0 s per fresh tunnel against 0.2 to 0.6 s
  elsewhere, and a slow connection setup shows on every new connection a
  persona opens.
- **F3 (owner):** several exits sit in carrier or business ASNs rather than
  consumer access networks; an AbuseIPDB key (free tier) in `deploy/.env`
  turns on the usage-type lookups that classify them.
- **F4 (owner):** `auto_replace_invalid_proxies` was still on at the live
  check; the other three were off.

---

## M3 — HTTP path (wreq)

**Scope.** §9, §6.1 step 4, §7.4 http self-test.

**Notes from M2.** All outbound HTTP already goes through wreq; replace the
body of `kage_proxy::health::probe::EmulatedClientFactory` with kage-http's
client builder and keep the `ProbeClientFactory` seam. wreq-util's
`emulation-compression` feature is on, so an emulated client sends the
browser's `Accept-Encoding`; record fingerprint fixtures with it on. Do F2
above. A live probe through a real proxy is possible now (`KAGE_LIVE=1`
with the owner's key in `deploy/.env`), but only against the echo and
fingerprint test endpoints listed in configuration.

**Deliverables**
- `kage-http`: client factory per persona from `wreq_profile`, proxy with
  credentials, persistent cookie jar (store-backed), header coherence rules
  and denylist, `Sec-Fetch-*` for navigation vs in-session requests,
  per-phase timing, raw capture, redirect limit, body cap.
- Profile mapping table (engine, major, os) → wreq-util profile with a test
  that enumerates wreq-util's available profiles and fails loudly when a
  configured engine major has no profile.
- Subresource emulation (off by default).
- Fingerprint self-test (http): fetch an echo endpoint and compare JA4,
  HTTP/2 Akamai string, header order with expected values stored per
  profile in `tests/fixtures/fingerprints/`.
- CLI: `kage selftest http --proxy <id> --profile <name>` (the `--persona`
  form arrives with M6).

**Acceptance**
- Integration tests through a local CONNECT proxy to wiremock verify
  proxy use, cookie persistence, header order and casing, timing fields.
- Live (gated): self-test passes for at least one Chrome and one Firefox
  profile through a real proxy; expected values recorded as fixtures.
- `cargo tree -i openssl-sys` still empty.

**Status.** Merged as `b330822`. Beyond the list above it shipped the
in-repo fingerprint echo (`kage echo serve`, compose profile `echo`) that
makes the per-PR fingerprint gate possible, kage-http-owned header tables
per browser family and request kind, the gated profile derivation
(`VERIFIED_MAJORS`, empty until M4 and M5 record captures), the cookie jar
with browser rules, subresource emulation, and `connect_ms`/`tls_ms` timing
(F2). Live self-test passed on 2026-10-09 (`m3-live-handover.md`):
`chrome_149` and `firefox_151` through two real proxies against
`tls.peet.ws` equal their committed fixtures on HTTP version, JA4, HTTP/2
string, header order and User-Agent; a real CONNECT tunnel changes nothing.
Follow-ups:
- **F5 (M4, M5):** capture the real browsers' navigation and fetch()
  requests against the echo; set `engines.<engine>.browser_major`; add the
  verified majors or leave the engine browser-only and say why.
- **F6 (M8):** the fetch()/XHR request kind and its API field, from the
  M4 captures; `security.resolve_on_host` as an opt-in host-side resolver.
- **F7 (M6):** `--persona` on the self-test, the scheduled 24 h run against
  the operator's echo only, the persona-keyed jar, persisting the asset
  freshness cache per persona.
- **F8 (any):** a `kage cookies` operator command (list, clear per proxy or
  domain).

---

## M4 — Browser sidecar: contract, server, Chromium engine

**Scope.** §10.1–10.3 (chromium), §10.6, §6.2 Client Hints check.

**Deliverables**
- `contract`: zod schemas for every endpoint in §10.2, OpenAPI emitted and
  committed, breaking-change check script.
- `server`: fastify, token auth, metrics, health, context manager with
  caps and idle close, artifact writing to the shared volume.
- `engine-chromium`: patchright with real Chrome channel, headful under
  Xvfb (screen set per persona), persistent profile dirs, authenticated
  proxy per context, fingerprint generation via fingerprint-generator and
  application via fingerprint-injector from a persisted fingerprint,
  `navigate` with waits and full capture (DOM, raw, headers, XHR, console,
  screenshot, PDF, timing, challenge signals), `state/export|import`,
  `selftest` endpoint running the detection page set and returning findings.
- `kage-browser` Rust crate: types generated from the OpenAPI file, client
  with timeouts and retries on connection errors, contract test.
- Sidecar Dockerfile for the chromium variant with Chrome version recorded.

**Acceptance**
- vitest integration: navigate the fixture site through a local proxy with
  auth; captured XHR JSON present; screenshots and PDFs written; two opens of
  the same persona reuse cookies.
- Determinism: generating a persona with a seed twice yields identical
  fingerprints; applying the persisted fingerprint yields identical
  `navigator` and screen values across context restarts.
- Client Hints coherence test on the fixture site: UA, `Sec-CH-UA-Platform`,
  `navigator.userAgentData.platform`, `navigator.platform` agree for a
  Windows-claiming persona; if this fails on the pinned Chrome, document it
  and set `allow_cross_os_claims` default to false in config with an ADR.
- Live (gated): selftest on bot.sannysoft and CreepJS shows no automation
  flags; findings stored as a fixture for regression.
- Container runs non-root, Chrome sandbox enabled, read-only root FS.

**Status.** Merged as `b30e87a` + `2e126ed`. Measured outcome, recorded in
ADR 0009: no way of claiming Windows from a Linux Chrome is coherent in
web workers, and the injection library costs the HTTP cache, so a chromium
persona claims Linux and its identity is set by launch parameters (own
Xvfb display, `TZ`, language preferences, CPU affinity, Mesa llvmpipe,
a real distribution's font set) with no script in the page;
`allow_cross_os_claims` defaults to false. Chrome 155 is browser-only
(`BROWSER_ONLY_MAJORS`): its TLS differs from the `chrome_149` profile in
one extension and three signature algorithms. The sidecar runs real Chrome
with its sandbox under its own seccomp profile. Live self-test passed on
2026-10-09 (`m4-live-handover.md`): two Chromium contexts through two real
proxies, bot.sannysoft all rows pass, CreepJS headless 0 % and stealth 0 %,
score 100, worker scope coherent with the window; the baseline is
`tests/fixtures/selftest/chromium-155-live.json`. Chrome's own idle traffic
measured at about 92 kB per persona-hour (roughly 1.1 GB a month for twenty
personas), because the driver starts Chrome with background networking
disabled. Remaining container tells CreepJS still sees are F9 and F12.
Follow-ups:
- **F9 (M5 or M12):** a dummy audio sink in the image so media devices and
  `AudioContext` exist.
- **F10 (M7):** a local forwarder per context (credentials out of Chrome,
  bytes per persona).
- **F11 (any):** fix the flaky M2 test
  `kage-proxy/tests/reputation.rs::a_lookup_that_fails_is_tried_again_an_hour_later`
  (random per-proxy offsets can exceed the 65-minute window; use fixed ids
  or a longer window).
- **F12 (M5):** a minimal window manager or panel so the work area is
  smaller than the screen; link navigation with a real `Referer` captured
  for the http path's followed-link table.
- **F13 (M12):** an e2e test running the compose `echo` profile with a
  browser self-test; drop the setuid bit of `chrome-sandbox` after checking
  Chrome still picks the namespace sandbox.
- **F14 (candidate milestone after M5):** a Windows engine host running the
  same sidecar for common and coherent Chrome personas (risk R3).
- **F15 (M6):** read `engines.chromium.allow_cross_os_claims` on the Rust
  side when generating personas; apply the §6.1 mix.
- **F16 (M6, kage-proxy):** D31 keeps `auto_replace_invalid_proxies` on;
  add `webshare.accepted_auto_replace` (default
  `[auto_replace_invalid_proxies]`) so an accepted flag logs an
  informational event instead of raising `provider_auto_replace_enabled`,
  and make sure a Webshare-initiated swap (`reason: auto_invalidated` in
  the replaced list) retires the persona and generates a new one through
  the same `PoolEvent` path as a Kage-requested replacement.

---

## M5 — Firefox engine and behavior engine

**Scope.** §10.3 (firefox), §10.4, §10.5.

**Deliverables**
- `engine-firefox`: camoufox-js with its own playwright-core pin,
  deterministic fingerprint from persisted JSON, explicit locale, timezone,
  geolocation, virtual display, proxy per context, the same capture as
  chromium; Dockerfile variant fetching the pinned Camoufox build.
- `engine-core` behavior engine: mouse, scroll, dwell, typing, idle, referer
  strategies; seeded; used by both engines; unit tests on trajectory
  statistics (no straight lines, velocity profile, overshoot frequency).
- Flow executor with the v1 action DSL and NDJSON streaming; shared by
  both engines.
- Engine parity test suite: the same contract tests run against both
  engines.

**Acceptance**
- Parity suite green on both engines.
- Behavior engine produces input events visible to a fixture page that
  records them; statistics within configured bounds.
- Live (gated): Firefox persona selftest shows claimed OS, timezone and
  locale; no automation flags.
- Python fallback for the Firefox engine documented in `docs/runbooks/engine-firefox.md`
  with the exact contract endpoints it must implement.

**Status.** Merged as thirteen commits ending `b3d2911`
(`m5-handover.md`). Measured outcomes: Camoufox `152.0.4-beta.30` passes
the worker-inclusive coherence bar for Windows and macOS claims, so the
§6.1 mix is chromium/linux 40 %, firefox/windows 45 %, firefox/macos 15 %;
the engine opens through its own launch plan (camoufox-js's launcher
mis-points fontconfig, is unseeded and downloads an addon); all human input
goes through XTEST on the context's display because driver input is
distinguishable on both engines; `link` entry and a never-forged
`search_engine` entry exist; Firefox 152 is browser-only (one cipher suite
fewer than `firefox_151`); ADR 0008 option B applied with the Firefox
image not published; F9 and F12 done (PulseAudio null devices, a work area
below the screen). Live Firefox self-test pending the owner's in-session
approval when merged. Follow-ups:
- **F17 (M5.1, next):** page reads grant user activation on both engines
  (spec §10.2 known violation). Chromium: read title, text, DOM, selector
  waits and element boxes over the engine's own CDP session in an isolated
  world without the gesture flag. Firefox: raise it upstream with Camoufox
  and, if needed, repack `omni.ja` with our own checksum. Probe: report
  `userActivation` against recorded trusted input so the self-test catches
  a regression. Also fix F11 for good (the flaky reputation test fails
  about four gate runs in nine): fixed ids in the test rig or a window
  longer than the spread.
- **F18 (M3 owner, small):** `Accept-Language` weights for Firefox http
  profiles: Firefox 152 sends `en-US,en;q=0.9`, kage-http builds `q=0.5`;
  measure 144 to 151 before changing the table.
- **F19 (M6):** the Firefox generator's screen data is narrow (six sizes,
  38 % on one) and one persona in ten has no taskbar; constrain per persona
  when generating twenty.
- **F20 (M8):** `kage selftest browser --engine firefox` should refuse a
  language without a region before calling the sidecar; the nightly live
  workflow fails for firefox until a baseline is recorded
  (`KAGE_LIVE_ENGINES=chromium` narrows it meanwhile).

---

## M5.1 — Page reads without user activation, and the flaky test

**Scope.** F17 and F11 above; spec §10.2 known violation; §21.

**Deliverables**
- Chromium: an adapter in `engine-chromium` that performs every page read
  the engine makes (title, page text, DOM capture, selector waits, element
  boxes for the behavior engine) through the engine's own CDP session with
  `Runtime.evaluate` in an isolated world and no `userGesture`, leaving
  patchright's driver for launch and navigation only.
- Firefox: an issue filed upstream with the measurement; a documented
  repack of the Juggler `Runtime.js` inside `omni.ja` with our own
  checksum, applied in the image only if the measurement after the repack
  shows no activation and the parity suite stays green; otherwise the
  runbook records the open tell.
- Probe: `navigator.userActivation.isActive` and `hasBeenActive` and a new
  `AudioContext` state reported next to the count of trusted input events
  the page saw; a hard failure when activation exists without input.
- `kage-proxy/tests/reputation.rs`: deterministic proxy ids or a window
  longer than the lookup spread, so the two listed tests never flake.

**Acceptance**
- On chromium an untouched page reports `isActive` false,
  `hasBeenActive` false and a `suspended` `AudioContext` at `load`, 300 ms
  later and 4 s later, with the DOM and text captures still taken.
- The same on firefox, or the exact remaining state documented in the
  runbook and the probe finding marked `warn` for firefox with the reason.
- The parity suite and `just ci` green; 24 consecutive runs of the
  reputation test binary pass.

**Status.** Merged as six commits ending `b56eb35` (`m5-1-handover.md`),
followed by the three M5 live-check commits (Firefox baseline
`tests/fixtures/selftest/firefox-152-live.json`, score 100 through two
real proxies, and the sannysoft judge reading the Chrome row by the
browser it saw). Both engines now leave an untouched page with no user
activation and a suspended `AudioContext` at every sample, in the hardened
images too; the probe fails hard on activation without input; the
reputation tests passed 84 consecutive runs. Decisions D32 and D33 record
the backport and the read path. Follow-ups:
- **F21 (M8/M9):** patchright's `goBack` and `waitForURL` hang when a page
  comes out of the back/forward cache; flows must not rely on `goBack`
  until handled.
- **F22 (Camoufox v156 milestone, or earlier if cheap):** audit every
  `defaultPref` in `camoufox.cfg` against a stock Firefox with the
  reference method (both browsers started with nothing driving them); the
  autoplay line was found by accident and any page-readable difference is
  a pool-wide tell.
- **F23 (M6 or M12):** CreepJS `like headless` warning threshold per engine
  (Chrome sits at 44 %, Firefox at 6 %).
- **F24 (live suite):** measure a Firefox context's idle traffic as M4 did
  for Chrome (`KAGE_LIVE_IDLE_SECONDS`).
- **Camoufox v156 line (after M6, fingerprint-critical):** new browser
  build, camoufox-js and playwright-core pins, removes the ADR 0010
  backport, re-measures coherence, re-records the capture and the live
  baseline, re-checks `camoufox.cfg`.

---

## M6 — Persona manager and session binding

**Scope.** §6, §5 (Persona, Session, PersonaDomainState).

**Deliverables**
- Persona generation on proxy health (geo → timezone/locale/geolocation,
  engine/OS mix, sidecar generate call, wreq profile mapping, profile dir).
- Lifecycle handlers for proxy state changes, retirement with archive and
  retention, warm-up visits (configurable list, off in tests).
- Sessions: mint, pin, idle and max TTL, caller ids, cleanup job.
- Domain state: cooldowns, daily caps in persona timezone, block counters.
- State export/import between browser and http path with the profile-major
  rule.
- CLI: `kage persona list|show|retire|selftest|regenerate`.

**Acceptance**
- Pool of N healthy proxies yields N personas with the configured mix
  within tolerance over repeated runs (statistical test).
- Replacing a proxy retires its persona and generates a new one; archived
  profile removed after retention (fake clock).
- Cookie handoff test: cookie set in the browser path is sent by the http
  path and vice versa; blocked when majors differ.

**Status.** Merged as eight commits ending `151f45b` (`m6-handover.md`).
New crate `kage-persona`; versioned identity tables (screens, locales,
mix) in `kage-core`; personas derived from the observed geo record;
warm-up on by default gating real work on `warmed_at`; sessions, per-domain
state, cookie handoff with the major rule, profile archive on the personas
volume, scheduled self-tests against the operator's own pages only; the
Finding A `oscpu` defect fixed at generation. Decisions D34 to D36 record
the tables, the warm-up gate and the self-test hosting question (O5). The
optional live persona check (`KAGE_LIVE_PERSONAS=1`) was not run.

**Follow-ups closed by M6.** F1 (budget per subscription term), F7 except
the persisted asset cache (see F26), F15 (the cross-OS rule on the Rust
side and the §6.1 mix), F16 (`webshare.accepted_auto_replace`, one path for
a swap whoever made it), F19 (screen classes by exact bounds from Kage's
own tables).

**Follow-ups raised by M6**
- **F25 (M7 or M12):** a chromium persona's processor count is clamped to
  the engine host's CPUs when its context opens (ADR 0009), so twenty
  chromium personas on a small host report the same count whatever the
  generator drew. Size the host, or record the accepted spread.
- **F26 (with the first engine major that has an http path):** persist the
  asset freshness cache of the http path per persona. It is per persona
  in memory already (one client per persona); both engines of this
  release are browser-only, so nothing uses it yet.
- **F27 (M7):** one owner per persona context. The manager's own browser
  work (warm-up visits, scheduled self-tests) and the scheduler's real
  work open the same context of a persona; M7 must route both through one
  gate per persona so that neither closes the context under the other.
- **Next to F22 (Camoufox v156 milestone):** a stock localized Firefox
  reports the bare language as its `Intl` locale (`de`) where Camoufox
  with the persona's locale reports the regional tag (`de-DE`, `de-AT`).
  Measured on Camoufox only; compare with a stock Firefox when the
  defaults are audited.

---

## M7 — Scheduler, pacing, block handling, escalation

**Scope.** §8, §11, §12 (runtime), §3.2.

**Notes from M2 to M6.** Honour `Pool::egress_unsafe()` before every
dispatch and call `Pool::report_traffic_ok` after a success (M2); add the
politeness pause after consecutive 5xx and scale cooldowns by
classification confidence through `Cooldowns::resolve` (M1); use
`Persona::is_ready()` (active and warmed) and `DomainStates::eligibility`,
apply `PacingProfile::for_persona(persona::pacing_factor(&persona.seed))`,
route browser attempts through `PersonaManager::browser_to_http` and
`http_to_browser`, and take the proxy from `route(&persona)` (M6); F27: one
gate per persona context shared with the manager's warm-up and self-test
work; F10: the local forwarder per context for bandwidth accounting; the
domain-policy default for a browser render MUST request human behaviour
(`dwell: "human"`, scroll depth around 0.6 to 0.9, wander on) because a
bare load is the http path's signature (M5); track bandwidth against the
250 GB cap and warn at 80 % (D5).

**Deliverables**
- Queue with priorities, per-domain and per-persona gates, pacing samplers,
  global caps, fake-clock testable.
- Persona selection scoring, retry with persona switch, cooldowns, circuit
  breaker with persistence, http → browser escalation, timeout handling and
  sync-to-job conversion hook.
- Attempt executor abstraction over http and browser paths, classifier
  integration, challenge self-resolve wait on the browser path.
- Metrics from §20 for scheduler and attempts.

**Acceptance**
- Simulation tests with fake clock: pacing distributions match profiles;
  bursts and breaks occur; daily caps respected; breaker opens and closes.
- Retry tests: three distinct personas used across three blocked attempts;
  escalation happens exactly once for `auto`.
- Mutation score ≥ 70 % on `kage-core`.

---

## M8 — Public API v1 and admin

**Scope.** §16 (fetch, sessions, admin, keys), §19, §20 (API metrics).

**Deliverables**
- axum router with utoipa OpenAPI 3.1, Swagger UI behind admin scope,
  problem-details errors, request ids, body limits, timeouts, CORS off by
  default.
- API keys: create/rotate/disable, hashing, scopes, per-key limits with
  governor, audit events.
- `POST /v1/fetch` end to end, `GET /v1/results/{id}`, artifacts streaming,
  sessions endpoints, policy preview, admin endpoints from §16.3.
- SSRF guard wired to targets and redirects.
- CLI: `kage key create|list|revoke`, `kage serve`.

**Acceptance**
- OpenAPI validates with an external validator in CI; generated client
  (any language) can call `/v1/fetch`.
- Auth tests: missing, wrong, disabled, out-of-scope, rate-limited keys.
- E2E: fetch through compose against the fixture site via both engines and
  the http path; artifacts retrievable; audit rows written for admin calls.
- Security tests: SSRF cases, header denylist, oversize bodies, slowloris
  timeout.

---

## M9 — Jobs: batch and flow, webhooks

**Scope.** §16.6, §10.5 via API.

**Deliverables**
- Job tables and worker loop (SKIP LOCKED), idempotency keys, progress
  counters, cancel, pagination of items, retention.
- `batch` and `flow` job types; sync-to-job conversion on wall-time overflow.
- Webhooks with HMAC signatures, retries, SSRF guard, delivery log.

**Acceptance**
- E2E: batch of 50 URLs completes with per-item results; flow job runs a
  multi-step DSL on the fixture site and returns captures per step.
- Idempotent re-submit returns the same job; cancel stops within one item.
- Webhook signature verified by a test receiver; retry schedule observed
  with fake clock.

---

## M10 — Crawler

**Scope.** §13.

**Deliverables**
- Frontier, sitemap discovery and parsing, allow/deny, depth, caps,
  same-site rule, dedupe by URL and content hash, link discovery from DOM
  and raw HTML, one-persona-per-site default.
- `crawl` job type wired into jobs and the API.

**Acceptance**
- Fixture site crawl yields the expected page set and depth; sitemap-only
  pages discovered; duplicates and near-duplicates flagged.
- Politeness verified with fake clock; crawl respects breaker and
  cooldowns.

---

## M11 — Extraction, robots, cache

**Scope.** §14, §15.

**Deliverables**
- `kage-extract`: metadata, JSON-LD, OpenGraph, links, readability text,
  markdown, headings, thin-content heuristic; golden tests on fixtures.
- robots.txt fetch/cache/verdict with the Google matcher; policy modes.
- Response cache with key rules and TTL.

**Acceptance**
- Golden extraction tests on ten varied fixture pages.
- robots verdict correct for allow, disallow, wildcard and crawl-delay
  cases; enforcement only under `enforce`.
- Cache hit returns `cached: true`; different include sets miss.

---

## M12 — Observability, hardening, docs, release 1.0

**Scope.** §20, §22, §19 review, §21 nightly.

**Deliverables**
- OTLP traces end to end, dashboards and alert rules in the observability
  profile, log redaction audit.
- Container hardening checklist applied and tested (non-root, read-only,
  seccomp, no-new-privileges, digests).
- Security review document in `docs/security-review.md` following the
  spec's threat list; fixes applied.
- Runbooks complete; README quick start verified from a clean machine.
- Nightly live workflow fully wired with secrets documented; release
  workflow produces signed tags and images.

**Acceptance**
- A single trace shows API → scheduler → sidecar spans in Jaeger.
- Trivy: no critical or high findings; SBOMs attached to the release.
- Load test (k6 or oha) of 200 queued fetches on the fixture site completes
  within pacing expectations without errors.
- Tag `v1.0.0` published.

---

## M13 — Field validation campaign (ops, repeated)

**Goal.** Measure and tune against real protected sites chosen by the
owner, never in CI.

**Deliverables**
- `docs/field-reports/<date>.md`: per site, vendor detected, outcome rates
  per engine and render mode, pacing used, policy changes made.
- Policy updates in `policies/domains.yaml`, signature updates with new
  fixtures, persona mix adjustments.

**Acceptance**
- Each report includes before and after outcome rates and the exact
  configuration diff.

---

## Session brief template

```
Milestone: M<N> <title>
Read first: CLAUDE.md, docs/00-decisions.md, docs/01-spec.md §…, docs/versions.md
Branch: m<N>-<slug>
Scope: <copy from plan>
Deliverables: <copy from plan>
Acceptance: <copy from plan>
Out of scope: <copy from plan or "nothing beyond deliverables">
Known constraints: <anything learned in earlier milestones>
Done when: just ci green, PR open with acceptance evidence, versions.md and CHANGELOG updated
```
