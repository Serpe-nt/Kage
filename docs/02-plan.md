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
live persona check passed on 2026-10-10 (`m6-live-handover.md`): `kage
serve` ran against the real pool for the first time, twenty personas for
twenty proxies within three minutes, mix 8/8/4 with both engines in every
country, stock language lists as observed by a third-party page, and one
persona per engine scored 100 against the recorded baselines. Not yet
exercised live: a replacement, a failing proxy, a geo change, a warm-up
visit, the profile archive on the real volume.

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

**Status.** Merged as sixteen commits ending `f0bedbf` (`m7-handover.md`).
New crate `kage-scheduler` with the pure rules in `kage_core::schedule`
under mutation testing (91.7 % caught); visits, persona days and start
spacing (D37); escalation, cooldowns by confidence, the half-open breaker
and the 5xx pause (D38); the in-process forwarder counting bytes to the
byte against the proxies' own sockets, adding under a millisecond per
connection, and the budget with its hard stop (D39); `kage fetch`; the
compose end-to-end test through both real browsers with real personas.
Spec §3.2, §8.2, §8.3 and §11 now carry the visit and escalation models.

**Follow-ups closed by M7.** F10 (the forwarder: the provider's credentials
stay in the kage process, bytes are counted per persona and domain; the
command-line self-tests still hand the sidecar the provider's proxy, see
F29), F27 (`kage_persona::Contexts` owns every persona context and the
manager's own browser work asks the scheduler's gate first), and F25 as far
as M7 goes (recorded in `docs/runbooks/fetching.md`; sizing the host stays
with M12).

**Follow-ups raised by M7**
- **F28 (M10):** the later pages of a visit are typed navigations in M7.
  A person follows a link: when the open page has a link to the target,
  click it (the sidecar's `link` entry on the page that is open, not on a
  page loaded for the purpose), else type. Needs a contract addition
  (1.2.0) on both engines.
- **F29 (M8):** an operator's self-test of a persona runs in its own
  process, hands the sidecar the provider's proxy, and meets
  `context_conflict` while the server holds that persona's browser open
  (up to `scheduler.context_idle` after its last page). Run it in the
  server, behind the admin API, through the gate and the forwarder.
- **F30 (with the first engine major that has an http path):** the
  compose end-to-end test (`tests/e2e/fetch.sh`) fetches through the
  browser path only, because no persona of this release has an http
  profile. Add the http path, the http to browser escalation and the cookie
  handoff with real personas to it; until then they are proven one layer
  down (`crates/kage-scheduler/tests/paths.rs`).
- **Question for M13:** should a persona forget a site after a hard block
  (clear that site's cookies and storage from its profile before it comes
  back after the cooldown)? M7 closes the browser and takes nothing of the
  wall into the http jar, but the profile keeps what the site set.

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

**Status.** Merged as eighteen commits ending `e1a3871` (`m8-handover.md`).
The public API v1 with 25 operations and a committed OpenAPI 3.1 document
validated by an outside validator and exercised by a generated TypeScript
client; keys, scopes, limits and the audit trail; sync fetch with the
behaviour floors, request kinds and refusals of D40; sessions that never
move; admin endpoints with self-tests in the server; the SSRF guard on
targets, hops and webhook URLs with the host-side lookup off by default;
`/metrics` finally exported; a listener that cuts silent connections. The
first clean-clone gate failed on a shell race in the new e2e script, fixed;
the second passed every gate. F34 is decided (deny the forwarding headers,
M9 does it); F31 to F37 stand as listed.

**Follow-ups closed by M8.** F6 (the request kind of a `fetch()` and an
`XMLHttpRequest` on the http path and its API field, from the captures of
M4 and M5), F20 (the stand-in of `kage selftest browser` refuses a firefox
language without a region), F29 (a persona's self-tests run in the server
behind the admin API, through the gate and the forwarder, in the context
real work has open).

**Follow-ups raised by M8**
- **F31 (evaluate at M13 with field data; build only if the field data
  wants it):** an in-page `fetch()` made by the browser, for JSON APIs
  behind a pool that has no http path. Known tell: a request the page did
  not make shows in the page's own `performance` entries. Until then the
  way to such data is rendering the page that calls the API with
  `include: ["xhr"]`.
- **F32 (with F30's milestone):** emulate the CORS preflight on the http
  path, so that a script's request a browser would preflight
  (`preflight_required` today) can be sent; and capture what real Chrome
  and Camoufox send for a `fetch()` with headers a script sets
  (`X-Requested-With`, `Authorization`, an `Accept` of its own) to another
  origin, so that more caller headers get a place in a script's request
  (`header_not_placeable` today for everything but `Referer`, `Accept` and
  `Content-Type`).
- **F33 (M9, with the retention work):** the admin purge endpoint spec §19
  names (delete a key's results and artifacts before their retention
  ends). M8 has retention only.
- **F34 (decided, M9 does it):** the forwarding headers
  (`X-Forwarded-For`, `Forwarded`, `X-Real-IP`, `True-Client-IP`,
  `X-Client-IP`, `CF-Connecting-IP`) join the header denylist of §9 with a
  test each; a browser never sends them and a proxy that does is a tell.
- **F35 (M9):** a fetch that outlasts the wall time runs on in a task of
  the server and is lost when the server stops (its result is then
  `result_not_found` after an hour). M9's job takes it over; `job_id` in
  the `202` is null until then.
- **F36 (M11 or M12):** a stored result does not keep `pacing`,
  `behavior`, `console` and `side_effects`; `GET /v1/results/{id}` returns
  them null. Store them if consumers of stored results want them.
- **F37 (M12):** `hickory-resolver` is pinned and unused; decide with the
  webhook sender of M9 and remove the pin if it stays unused. The API
  test client is linted by `tsc --strict` and prettier only; give it
  eslint if it grows.

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

**Status.** Merged as fifteen commits ending `68ab0e9` (`m9-handover.md`).
Jobs under a worker lease (`SELECT … FOR UPDATE SKIP LOCKED`, counters
moved with the item), `batch` and `flow` with idempotency keys, cancel,
paginated items with their results, a deadline and a pause (D41); flows as
one paced browser attempt under contract 1.2.0 with the F21 `back` fix
(D42); webhooks to listed origins only, signed per attempt, five retries on
the jittered schedule, a delivery log (D43); the overflowed sync fetch
carried on as a one-item job under its own request id across a restart
(F35); the audited admin purge and the retention of jobs and of results
that are no job's (F33); the forwarding-header family on the denylist
(F34). `crawl` answers `not_available_yet` until M10. The gate passed on
the fourth clean-clone run: the first failed on an M8 test that is flaky
(F42), the second on example webhook secrets gitleaks took for real ones
(made plain words), the third on the smoke test's table count (22 now).
Coverage: kage-api 93.2 %, kage-scheduler 93.7 %, kage-store 98.6 %.
Measured: 50 pages of the fixture site dealt out freely take 10 min on
3 to 5 addresses with 44 changes of address; one visitor takes about an
hour on one address, 300 pages a day; so one visitor per site is the
default and `visitors` is the caller's trade. Real Chrome clicks 0.95 to
1.19 s after a load and a constant 1.2 s after every click without
pacing; paced, the first click comes about 6 s after the load and the
quiet time after a click is 1.8 to 2.4 s and never the same. `goBack`
with `load` hangs 8 s on Chrome; with `commit` it returns in 11 ms.

**Live check (2026-10-11, `m9-live-handover.md`, merged as four commits
ending `47f7738`).** Jobs ran against the real pool for the first time,
against the two allowed detection pages only, with the shipped `human`
profile and `active_hours: ignore` for those two hosts (it was 23:00 in
the pool's countries and a job never wakes a persona). Four of five steps
passed: a sync fetch per engine, a batch of four read by one visitor per
site in 171 s, three webhooks signed and verified at a loopback receiver
with the first retry after 33.1 s, and an overflowed fetch carried across
a server restart under its request id (the cut attempt unrecorded and the
page fetched twice: F39, seen live). The Firefox flow failed at its human
`scroll`: a scroll on a page too long for its step time was failed by
that time after finishing its flick and rest (Chrome passed by 74 ms,
Firefox missed by 2 ms). Fixed in the sidecar without a contract change:
the scrolling ends two seconds before the step's time and sits out no
rest past it, so a tall page is scrolled as far as the time allows and
the step succeeds (spec §10.5). The fix was verified in both real browsers
without keys, then on 2026-10-12 against the real pages on the owner's
word (`KAGE_LIVE_JOBS_ONLY=flows`, midday in the pool's countries, shipped
policies untouched): both flows succeeded 10 of 10 steps in one attempt,
70 s in Chrome and 69 s in Firefox, the scroll ending inside its 30 s
(about 28 s of scrolling in each), the human `back` 0.4 s beside its dwell,
the flow's webhook delivered on its retry after 26.1 s. Every step of the
live check has passed (`m9-evidence/live/run-2/`). Measured: a tall page takes 46 to 107 s
under the shipped behaviour (53 wheel notches, dwell 5 to 7 s), so the
default 60 s attempt budget would have cut the Chrome fetch (F44); the
next page of a sitting begins 0.1 s after the persona left the one before,
because the profile's gap is start-to-start and a minute's read uses it
up (F43); eleven page loads cost 1.59 MB through the forwarder, about
145 kB a page, and Webshare's own counter moved 1.79 MB because the
health probes and geo lookups of twenty proxies do not pass the
forwarder; four of twenty personas were used, because the scheduler kept
each page with the persona that had read its site. Still never exercised
live: a replacement, a blocked page inside a job, a job paused for a
closed site, a webhook to a public receiver, the http path (F30).

**Follow-ups closed by M9.** F21 (`goBack` on commit, the sidecar's own
look at the address, `wait_for url` with its own matcher), F33 (the purge),
F34 (the whole forwarding family, not only the six named), F35 (the
overflow as a job).

**Follow-ups raised by M9**
- **F38 (M10, in the sidecar):** a human `back` with no history still
  reports success after its 5 s watch while the driver's `back` fails
  after 1 s; and a human `back` to a history entry with the same URL,
  served from the cache, is not detected as having left the page. Make
  both report what happened.
- **F39 (M12):** a carried fetch's attempt that is under way when the
  server stops is cut and not recorded; the job is taken up after its
  lease (`jobs.lease`, 1 min) and the page is fetched again, so the site
  sees it twice. Record the cut attempt, or wait for it on shutdown.
- **F40 (when a consumer asks):** no `GET /v1/jobs` (a key's jobs) and no
  `kage job` at the command line; the API is the only way and a job is
  found by its id. `GET /v1/jobs/{id}/items` reads each item's result with
  three queries (a page of 200 items is 600); a join when it matters.
- **F41 (M13, measure):** Chrome sends one `mousemove` of its own about
  110 ms after a load in a paced flow (the pointer resting over a fresh
  page); Kage sends nothing before the dwell ends. Compare with a person's
  trace before deciding whether the first wander should start earlier.
- **F42 (M10, first commit):** the M8 test
  `keys_are_made_rotated_and_revoked_and_their_tokens_shown_once` in
  `crates/kage-api/tests/admin.rs` takes `token.rsplit('_').next()` as
  the secret; a base64url secret may contain or end in `_`, so about 3 %
  of runs fail. Take everything after the second underscore
  (`token.splitn(3, '_').nth(2)`).
- **F43 (M10, with F28, consult first):** a hand's rest between leaving a
  page and asking for the next one of the same sitting. The profile's gap
  (median 4.5 s, floor 1.5 s) is start-to-start, so after a long read the
  next typed navigation leaves 0.1 s after the last; a person who has
  finished reading still takes a moment to type or to find the link. A
  seeded thinking time after the dwell (the flow pacer already draws one
  between steps), never counted against the gap. F28's click on the open
  page gets the same thinking time before the click.
- **F44 (M10, consult first):** the attempt budget against the shipped
  behaviour. `timeout_ms` (60 s by default) covers the load and the whole
  visit of the page, and a tall page under `human` takes up to 107 s, so
  the default cuts it with `timeout`, the retry goes to another persona
  and the site sees the page twice for nothing. Options to weigh: the
  behaviour's time separate from the load's (`timeout_ms` for the load, a
  policy `max_dwell` for the visit); or the budget derived from the depth
  asked; or a higher default with the runbook saying that a tall page
  takes far longer than the table's median. A crawl of a real site meets
  tall pages constantly, so M10 decides this before its first crawl.
- **F37 stands** (M12): the webhook sender uses the system resolver;
  `hickory-resolver` is still pinned and unused.

---

## M10 — Crawler

**Scope.** §13.

**Starting point (after M9).** A job already has its lease, items,
counters, visitors, cancel, pause, deadline, retention and webhooks
(`crates/kage-api/src/jobs/`, `crates/kage-store/src/repo/jobs.rs`); the
planner already holds a job to its visitors (`Work.visitors`, D41); a
`crawl` job is refused with `not_available_yet`. M10 adds the frontier
(`crawl_frontier`, §18), discovery and dedupe in `kage-crawl` with URL
normalization in `kage-core`, and the `crawl` job type on top of what M9
built; it does not build a second worker. F28 (a click on a link of the
open page as the way from one page to the next in the browser, so that a
crawl looks like reading; its contract addition is 1.3.0 now that M9 took
1.2.0) is M10's, and so are F43 (the rest before the next page) and F44
(the attempt budget on a tall page), both decided with the planner before
the crawler is built, because a crawl of a real site is a long sitting of
typed or clicked pages, many of them tall. F38 and F42 are M10's first
commits. Read `m9-live-handover.md` for what the real pool showed.

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

**Status.** Merged as seventeen commits ending `ee3444f`
(`m10-handover.md`). `kage-crawl` (page reader, SimHash, site, sitemaps,
scope, the frontier) and the `crawl` job on M9's worker; contract 1.3.0
(a patience per page, an arrival by a click from the page that is open
with up to three keyboard backs, the links of a document with whether
each is rendered); migration 0014 (frontier columns, `jobs.crawl`,
`attempts.patience_ms`); the patience of a page and the rest between two
pages (D44); the reader's order, the scout for sitemaps, the dedupe and
the normalization defaults (D45). The gate passed on a clean clone in 43
min: 1320 Rust tests, 651 sidecar tests in both browsers, five e2e suites
including a crawl by real personas of both engines; `kage-crawl` 99.66 %
of lines, every gated crate above 92 %; mutation score of `kage-core`
92.1 % (2213 mutants, run with 24 jobs as M7 did, see F47). Measured
before the change: the 60 s default never timed a tall page out, it ended
the visit `ok` at exactly 56.0 s in six of six runs in both browsers, so
every tall page of a crawl would have been left at the same instant; the
next page of a sitting was asked for 1 to 3 ms after the last; a
breadth-first crawl types 80 to 96 % of its addresses (no `Referer`,
`Sec-Fetch-Site: none`, no activation on the page that is left), the
reader's order with text links within three backs before menu links 0.3
to 8 %. In the compose e2e the fixture's reading room of 21 pages is typed
4, clicked 5 and reached by a back and a click 12, in both engines; the
page that is left sees a trusted click with user activation 9 to 14 ms
before `pagehide`; a hidden link is never asked for; the sitemap is read
by a Firefox scout at another address than the reader's; under a profile
with a rest the next page comes 1.4 s later at the median than without.
Contract and OpenAPI regenerated, client extended, runbook "Crawls". The
day's disk incident (C: full, Docker Desktop down) was the owner's to
clear; the session deleted nothing. No live check was run.

**Follow-ups closed by M10.** F28 (the click from the open page, both
engines, contract 1.3.0), F38, F42, F43 (`rest_ms`), F44 (the patience).

**Follow-ups raised by M10**
- **F45 (M12):** move the YAML reading off `serde_yaml` and
  `unsafe-libyaml` (RUSTSEC-2024-0449, ignored in `deny.toml` with the
  review date 2026-12-31).
- **F46 (M11, first commit):** `stored_spec` in
  `crates/kage-api/src/jobs/request.rs` puts a webhook's `progress_every`
  at the top of the stored spec, and the worker parses a flow's spec back
  into `FlowJob`, which denies unknown fields; a flow job with
  `webhook.progress_every` is accepted and then fails in the worker.
  Found by reading, not tested: write the test first, fix it, check the
  batch and the crawl take the same road.
- **F47 (M11, first commit):** `just mutants` as written runs one mutant
  at a time, about seven hours since M10; M7 and M10 ran its command with
  `--jobs 24`, each job in its own build directory, and the recipe's own
  score check after. Give the recipe a `jobs` parameter (default 1, the
  comment naming what the two runs used and that a busy machine turns
  slow tests into timeouts), and fix the surviving mutant on M10's lines
  (`config/crawl.rs:166`, the 16-letter bound of an ending) with a test
  at the boundary.
- **F48 (M12):** the sidecar's older `link` and `search_engine` entry
  (`enter()`) reports `ttfb_ms` 0, a download as `arrived` with the start
  page's document, and misses a link wrapped over two lines in Chrome;
  the new arrival from the open page has none of these. A page that
  answers a click itself (`pushState`) is `link_not_found` and then typed,
  so on client-routed sites every page is a click and a typed load.
  `visible` does not see overflow clipping; links in shadow roots are not
  listed.
- **F49 (M12):** a page reached while a crawl changed hands between its
  stored result and its item is recorded as read without its links.
- **M13, observations:** in Camoufox the window gets its first `focus`
  only at the first press, so a page that is only read never has the
  focus; Chrome fires `blur` before `pagehide` on a typed navigation; a
  page gone back to sees two `keyup` and no `keydown`; the pointer stays
  still during the rest. The runbook's line that a crawl costs what a
  batch of as many pages costs is inferred, not measured.

---

## M11 — Extraction, robots, cache

**Scope.** §14, §15.

**Starting point (after M10).** The links and the main text of a page are
already read by `kage_crawl::html` (`scraper` 0.27.0, `html5ever`), which
`kage-extract` may take over or call; `extract` in `include` is refused
until M11 and a crawl's items take `include` through `defaults`. A crawl
does not read `robots.txt`: the frontier has `robots_denied` among its
reasons, `kage_crawl::sitemap::parse` takes whatever `Sitemap:` lines
name, and who asks for `robots.txt` is M11's first consult, because a
reader never requests it (M10's scout rule, a persona that never reads
the site, is the proposal to weigh; the verdict cache in Postgres means
one request per site per TTL, never one per page). The response cache
(§15) is keyed on the normalized URL (`kage_core::urlnorm`) and the
include set, and a cache hit is a result with `cached: true` and no visit
at all: it must never shorten or skip a visit that a persona is in the
middle of, and a crawl's dedupe by address happens before the cache. F36
(stored results keep `pacing`, `behavior`, `console`, `side_effects`) is
M11's or M12's; F46 and F47 are M11's first commits.

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

**Status.** Merged as fourteen commits ending `4adc07e`
(`m11-handover.md`). `kage-extract` (Readability's scoring ported onto
the tree the crawler already keeps, one parse per page, the text and the
markdown from one walk, metadata and times, a robots.txt parser and
matcher of Kage's own); `robots.txt` asked for only where a site's policy
says so and then by the site's one scout (D46, which changes D15); the
response cache that answers without a visit (D47); extraction from the
captured document at the end of the attempt (D48); migration 0015
(`site_scouts`, 23 tables). The gate passed on the third clean-clone run
in 44 min (the first fell to the secrets scanner on an invented key-shaped
value in a fixture page, the second to the smoke test's table count):
1435 Rust tests, 651 sidecar tests, six e2e suites with the new
`extract.sh` by real personas of both engines, `kage-extract` 98.86 % of
lines, mutation score of `kage-core` 92.5 %. Measured: the port reads a
mean F1 of 97.0 on the ten tuned fixture pages and 97.7 on three pages
written afterwards by a subagent that saw neither the rules nor the
fixtures, against 89.4 and 78.2 for the best crate; a typed `robots.txt`
in both browsers is one document navigation of a person who typed the
address, the file there in under 100 ms and the visit 3 to 16 s; a crawl
served page by page from a half-warm cache types 19 to 45 % of what it
still asks for, so a crawl fills the cache and never reads it; in the
e2e the cache answered in 13 to 15 ms against a visit of 0.8 to 1.2 s,
the reader never asked for `robots.txt`, the scout asked once from an
address that read nothing, a crawl under `enforce` never asked for the
denied page, and the sitemaps came from the address the file named. Two
bugs found on the way and fixed: a crawl never sent `job.progress` (with
F46), and a navigation a browser turns into a download (a gzipped
sitemap in Chrome) was classified as a soft block and cooled the persona
for hours. The `robotstxt` crate panics on input a site controls and is
the test oracle only. The session removed its container and volumes;
image `kage-dev:m11` stays for M12. No live check was run.

**Follow-ups closed by M11.** F46, F47.

**Follow-ups raised by M11**
- **F50 (M12, first commit):** a test for the value of
  `MAX_ROBOTS_BYTES` (the one surviving mutant on M11's lines), and
  `kage fetch` at the command line gets `--extract`, `--cache-ttl` and
  `--robots` and shows what they return.
- **F51 (M12, decide):** a site every persona has read has no scout for
  good, and a scout that is not ready (its proxy down, the persona
  cooling) closes an enforced site until it is back or retired. Give the
  rule an end (a persona that has not read the site for
  `robots.scout_quarantine`, 30 days, may scout) and an admin call that
  gives a site another scout. The scout's preference for a Firefox can
  take a site's only Firefox reader; `readers_needed` counts readers,
  not engines.
- **F52 (M12):** two identical requests in flight are both fetched; join
  the second onto the first inside one process. A whole crawl asked for
  again inside the cache's lifetime visits every page again; an
  all-or-nothing replay of an identical crawl is the stealth-neutral way
  to serve it.
- **F53 (M13, observe):** `Crawl-delay` is a floor per persona and host,
  not per site; two visitors of one site may land closer than the delay
  from two addresses. A redirect's target is not checked against
  `robots.txt`. A fetch that ends `robots_denied` costs a page of the
  daily budget. Thin content keeps a page out of the cache only when
  `extract` was asked for. Under `report`, a page already queued when
  the scout was chosen can still go to the scout (closed under
  `enforce`).
- **F36 stands** (M12): a cache hit, like every stored result, has
  `pacing`, `behavior`, `console` and `side_effects` null.

---

## M12 — Observability, hardening, docs, release 1.0

**Scope.** §20, §22, §19 review, §21 nightly.

**Starting point (after M11).** Every feature of the spec is built
(M0 to M11); what remains is to make the whole observable, hard and
releasable. The verification image `kage-dev:m11` is on the host with
the pinned browsers inside; each session builds its own container on
fresh volumes and removes them when done. The gate takes 44 min on a
clean clone and the mutation run 93 min with 24 jobs; nothing has ever
run on GitHub Actions because nothing is pushed (D30): M12 is where the
repository goes to origin, the PR workflow starts, and the nightly live
workflow runs for the first time with the owner's secrets documented and
never committed. Carried into M12: F36, F37 (the unused resolver pin),
F39 (a carried fetch's attempt cut on shutdown), F45 (the YAML parser
behind the ignored advisory, review by 2026-12-31), F48, F49, F50, F51,
F52, and the Camoufox 156 line as a fingerprint-critical change of its
own that removes the ADR 0010 backport (D32). The security review of §19
must cover the SSRF guard on every road a URL enters (targets, hops,
sitemaps, robots.txt, webhooks, redirects) and the secrets that exist
(`KAGE_MASTER_KEY`, the sidecar token, the provider key, API keys, the
webhook secrets).

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

**Status.** Done. **1.0.0 was released on 2026-10-18**
(https://github.com/Serpe-nt/Kage/releases/tag/v1.0.0,
`m12-handover.md`). The first push took `origin/main` to `adb8ba3` (M0
to M11, no pull request, as D30 said); the milestone's 55 commits came
in as PR #1 by rebase, two fixes found by the merged `main`'s own run as
PR #2, and the released digests as PR #3; `main` is `6308a36` with
`ci-ok` green. The tag is annotated and signed with an SSH release key
the owner made for it, listed in `.github/allowed_signers`, which
`release.yaml` checks before it builds; the two published images
(`ghcr.io/serpe-nt/kage`, `ghcr.io/serpe-nt/kage-browser-chromium`) are
signed keylessly by the workflow's identity with SLSA provenance and a
CycloneDX SBOM attested to each digest; the Firefox image is not
published (ADR 0008). Built in the milestone: the carried follow-ups
F36, F37, F39 (a stop waits up to `jobs.wind_down`, 150 s, and writes
what it cuts), F45 (YAML by `serde-saphyr` behind a provider of Kage's,
the advisory's ignore gone), F48, F49, F50, F51, F52 and F54 (a batch no
longer stands still for one visitor's long break); traces end to end
(OTLP/HTTP through wreq, the sidecar's spans under Kage's `traceparent`
on the compose network only), the `observability` profile with the core
collector, Jaeger v2, Prometheus and Grafana at pinned digests, eight
dashboards and the alert rules, a redaction audit over the e2e logs; the
container checklist as a gate, no setuid file in the images, the
sidecars on an internal network with kage alone; `docs/security-review.md`
with its fixes (a sidecar's file taken only as described, IPv4-translated
addresses, https-only provider roots, a 128 MiB bound on a sidecar's
answer, the command-line self-test through a forwarder of its own); the
runbooks; the README quick start proven on a Docker-only machine (the
stack healthy in 563 s from empty); the load test (design A, 200 pages
in 68 s, in every PR; design B before a release, 582 s with the
20-a-minute cap reached on Kage's clock, D49); the three workflows. The
local gate was green on a clean clone (47 min) and the mutation score
92.6 %. GitHub's 4-vCPU runners then found what no local gate had: on a
slow host a fresh document draws no frames for about half a second, so
the hand lost its wheel and read every page as one that does not scroll
(a product fix: the hand waits for two frames and turns the wheel once
more before it gives up, reproduced with a cgroup holding four CPUs to
0.6 of one), a Chrome screenshot right after a load could fail, a
scheduler test flaky since M7, a coverage report counting a cached older
build, and six assumptions of a fast host or a daytime run. The nightly
workflow's `load` job runs by schedule; its live job waits for the
owner's secrets and the `KAGE_LIVE_ENABLED` variable. One slip reported:
an early crates.io API call carried the owner's e-mail address in its
User-Agent.

**Still the owner's, after 1.0.** The repository settings of
`docs/runbooks/repo.md` (branch protection with `ci-ok` required,
Actions restricted and SHA-pinned, read-only token, fork approval,
secret scanning with push protection, e-mail privacy); the two Actions
secrets and the first run of `nightly-live.yaml`; deleting the three
merged branches on GitHub.

**Follow-ups raised by M12** (after 1.0; each a pull request of its own)
- **F55 (fixed by [Serpe-nt/Kage#32](https://github.com/Serpe-nt/Kage/pull/32)):**
  drop the `zip` ignore when `utoipa-swagger-ui` is on `zip` 9.
- **F56 (M13, observe):** count `domain_rpm` at the moment a page's
  request leaves, if a site is ever seen to count by the minute.
- **F57 (security review):** with `resolve_on_host` on, the forwarder
  resolves the names it relays and refuses denied ranges, and `kage
  config check` warns about a static proxy list without it (G1); the
  http path refuses the browsers' unsafe ports (G3); the forwarder relays
  the authority it checked, not the client's text (G5); a per-key limit
  of open jobs and queued items (L2); a bound on the distinct `domain`
  label values (L3); a volume of the sidecars' own for their files (V1).
- **F58 (CI):** the report-only mutants job reaches 78 of 2375 mutants
  in its 30 min on a runner; run the mutants of the pull request's diff
  (`cargo mutants --in-diff`) or a weekly job on a larger runner. The
  release workflow pushes an image before Trivy scans it; scan before
  the push.
- **F59 (M13, measure):** whether `min_dwell` should bound a persona's
  own dwell too (a draw of 600 ms was seen on a page that fit its
  window); whether a first look before the hand moves belongs in the
  motor profile (the hand moves 100 to 400 ms after a load, two frames
  since the fix); what Firefox's select looked like to the page on the
  runners (the `parity-notes` artifact).
- **The Camoufox 156 line** as a fingerprint-critical milestone of its
  own: removes the ADR 0010 backport, re-checks `camoufox.cfg` against
  stock Firefox defaults, new baselines, a live self-test on the owner's
  word (D32). **Done, 2026-10-19:** merged as
  [Serpe-nt/Kage#5](https://github.com/Serpe-nt/Kage/pull/5), thirteen
  commits (`camoufox156-handover.md`), released in 1.0.1. Measured
  before the change: the 1.0.0 Firefox personas had four tells no stock
  Firefox has, each one line of script for a page (the `chrome://juggler`
  worker hCaptcha probes for, unpartitioned cross-site storage, a popup
  without activation, fifteen `set…` globals); 156 removes all four with
  no launcher or driver pin moved, since the driver already runs it and
  the parity suite passed 31 of 31 without the backport. 156 brought
  three regressions, two fixed in the pull request (`Intl` and the
  browser's messages in English for a German persona; Global Privacy
  Control claimed without its header) and one accepted (the host's
  canvas for every Firefox persona, security review T1). The audit of
  `camoufox.cfg` (411 rows, `docs/evidence/camoufox-156-prefs-audit.tsv`)
  set two preferences to stock and documented the rest as known tells.
  Existing personas move at their next open and keep their cookies; a
  profile is one-way, so a copy is kept seven days. The live self-test
  scored 100 of 100 for a Windows and a macOS persona and recorded
  `firefox-156-live.json`. The same parity check found Chrome's one tell:
  unpartitioned third-party storage from patchright's default switches
  (T2), left for the next milestone. Follow-ups raised: **F60** the
  Chrome launch-switch audit (started 2026-10-19 as its own milestone,
  with the Chromium "paces a flow" test that fails under the slow cgroup
  on main); **F61 (M13, measure)** how rare the host's canvas hash is
  among real Windows and macOS Firefox users; **F62 (data needed)**
  speech voices, EME/Widevine, the macOS colour gamut, `maxTouchPoints`
  for 13 % of Windows devices, WebGPU's null adapter; **F63 (a milestone
  of its own if wanted)** the official `camoufox` launcher as a second
  generator (fpgen, per-identity fonts, voices, media labels; brings
  ua-parser-js 2.x and python-random, ADR 0008 again). Firefox is at
  158; a Camoufox build for it is a routine fingerprint-critical pull
  request when upstream publishes one.
- **The Chrome launch-switch audit (F60). Done, 2026-10-19:** merged
  as [Serpe-nt/Kage#8](https://github.com/Serpe-nt/Kage/pull/8), five
  commits, ADR 0014 (`chrome-switches-handover.md`), released in 1.0.2.
  Measured against a stock Chrome 155 that nothing drives: every
  Chromium persona of 1.0.0 and 1.0.1 showed a page three things no
  stock Chrome does, all from the driver's default switches: a yellow
  "unsupported command-line flag" bar over the page on every start
  (the viewport 56 px short of a real Chrome's), unpartitioned
  third-party storage, and no https-first attempt on a bare host. Kage
  now owns the switch list (D53): thirteen of the driver's defaults are
  left out, twelve kept with a reason each, `--test-type` removes the
  bar, and three features stay off as known tells (the built-in AI
  models, which would download gigabytes through the pool; media
  routing, which would multicast; background networking to Google,
  measured live at 89 kB per persona-hour with it off against 139 kB
  with it on). The wire did not move and no fixture changed. The live
  self-test scored 100 of 100 for two Chromium personas. Follow-ups:
  **F64 (M14 or later, level 3)** a typed navigation made by the hand
  through the address bar, since the driver's navigation leaves the
  address bar focused; **F65 (M14, first commit)** the server-sync test
  of the proxy CLI suite is racy on a slow runner (the proxy is healthy
  before its `proxy_added` event is written): wait for the events, and
  check the order the pool writes them in.

---

## M13 — Field validation campaign (ops, repeated)

**Goal.** Measure and tune against real protected sites chosen by the
owner, never in CI.

**Starting point (after 1.0).** Nothing of the field campaign can begin
until the owner names the sites; the plan does not choose them. Before
the first report the owner's to-dos above should be done, so that the
nightly live suite (the echo and detection pages, the real pool) runs
and the released build has been seen on the real proxies at least once.
A campaign session runs the released images on the owner's host, never
in CI, with the owner's keys under the owner's typed approval, one site
at a time, with `report` for robots.txt unless the owner says `enforce`
for that site, and writes `docs/field-reports/<date>.md`. What it may
change: `policies/domains.yaml` (pacing profile, cooldowns, breaker,
`robots`, `cache`, `driver_input`), block signatures with new sanitized
fixtures, the persona mix; what it may not change without a milestone
of its own: anything fingerprint-critical (`docs/versions.md`). F31, F41,
F53, F56 and F59 wait for its data.

**Deliverables**
- `docs/field-reports/<date>.md`: per site, vendor detected, outcome rates
  per engine and render mode, pacing used, policy changes made.
- Policy updates in `policies/domains.yaml`, signature updates with new
  fixtures, persona mix adjustments.

**Acceptance**
- Each report includes before and after outcome rates and the exact
  configuration diff.

**Status (round 1).** Run from 2026-10-24 to 2026-10-29 by session M13,
started on the owner's typed "start M13" and run against twenty sites the
planner picked and the owner approved by name in that session (the list,
the two controls, `robots: enforce`, the `Claude-User` groups, the budget
and the stops, and that the sites' terms were not read). Twenty EU retail,
classifieds, property, rail-information, price-comparison and publishing
sites, four per country in AT, DE, FR, GB and IT, one new persona per site
behind a Webshare static residential address (9 Chrome, 11 Firefox by a
seeded draw), level 3 only, the pages kept inside the stack and the
committed report naming a site only by index, country, category and
vendor. The owner changed the plan on the second day ("the caller must
know it himself"): from then on each site got one crawl of 40 pages at
depth 3, five sites at a time and one per country, no page cap per site
or per round, pass 2 four hours or more after a block. Result, 442 pages
asked and 851 MB (half of it warm-up), 0.3 % of the proxy plan's term:
fifteen sites read cleanly for tens of pages, among them every Akamai
site given another hour of the day, three of the four DataDome sites and
a Kasada site nobody had expected; three were refused at the first page
on the last evening and had no second pass (two Akamai, one DataDome);
one Cloudflare site refused twice (a managed challenge, then a bare 403
to the scout's robots.txt) and is "refuses a gentle visitor"; one site
closed its robots.txt to the first scout and read 38 pages for a second.
The round found four product bugs, one false positive of the classifier
and the round's main product finding: a consent dialog that covers the
page stops the hand, and the persona then types every address (under
Didomi no click landed; under Usercentrics 8 of 9 did; without a dialog
24 of 40 pages were reached by click). Merged as
[Serpe-nt/Kage#24](https://github.com/Serpe-nt/Kage/pull/24): the
runner `tests/field/` with `just field`, its unit tests and an e2e
against the fixture site, `max_body_bytes` 16 MiB in the shipped
signatures (F88) with a classifier test, the rule
`akamai-reference-error` with a synthetic fixture, the `patient` pacing
profile, the runbook's "A session runs it" and the report
`docs/field-reports/2026-10-29.md`. Decided from its data: F59 (19 of
439 dwells under the policy's 2 s `min_dwell`, the floor must bound the
persona's own draw too) and F53 (a redirect's target is checked by
neither robots.txt nor a crawl's `deny`); F31, F41, F56, F61 and F62 got
no data. Raised: F83, F85 (fixed in 2.0.0), F86, F88, F89 and F90 (the
list under "After 2.0.0"); the merge review of the runner raised F94,
its own debts. A second round, when the owner wants it, F94 fixed
first: return visits for the clean sites, a second pass for the three
refused at page 1, and the consent answer once F89 is built. Lessons: the
machine must stay on for the round's days (it went off twice and two
slices never ran); a session's background launcher does not survive a
shutdown; keep a batch's record even when it is cut.

---

## Phase 2.0 — levels, egress, providers, the public face

Decided by the owner on 2026-10-19 (D50 to D52, ADR 0012 and ADR 0013)
after 1.0.1. Three milestones, in this order, one pull request each on
the PR workflow; the phase releases as **2.0.0** (the required `level`
breaks every 1.x request on purpose). Before M14 lands: the Chrome
launch-switch audit (F60, its own fingerprint-critical pull request,
released as 1.0.2). The field campaign (M13) runs whenever the owner
names sites, on whichever release is current.

### M14 — Camouflage levels and egress (2.0 core)

**Scope.** Spec §25, ADR 0012, D50; §1, §3.1, §8.1, §16.4, §17 and §19
updated to it.

**Deliverables**
- `level` required on every request and in a job's `defaults`
  (`level_required`, `level_below_policy`, `level_unavailable`,
  `session_needs_persona`); the policy's `min_level`; the OpenAPI, the
  TypeScript client, `kage fetch --level`, the results' `level`, the
  metrics' `level` label.
- Level 0: the plain client in `kage-http` (wreq's own TLS, the default
  `User-Agent`, the caller's headers and body, no jar), no persona,
  egress per `egress.plain`, through the SSRF guard, the key's limits and
  the daily budget; the pool used as plain proxies in turn when
  `egress.plain: proxies`.
- Level 2: the sidecar's plain load (contract 1.5.0, additive: a
  behaviour mode with no scrolling, no wander, no patience, the dwell at
  `min_dwell`), the scheduler keeping the persona's pacing, visits and
  nights.
- Level 1: the path exists and is refused with `level_unavailable` while
  `VERIFIED_MAJORS` is empty (F30), tested with a fixture major.
- Egress `direct`: the host persona generated from the host's observed
  geo or the declared country and locale, the forwarder with no upstream,
  the pool untouched; `kage config check` refuses a mixed configuration.
- The security review's new threats (direct mode, level 0), the runbook's
  table of what each level costs and what a site can tell at each.

**Consult first.** Level 0's TLS and header defaults (what wreq sends
untouched, and whether a `kage/<version>` User-Agent is right for a
"plain" client or a caller must always name one); what pacing level 0
gets per domain (none beyond the key's limits, or a `plain_rpm` in the
policy); level 2's dwell and whether the pointer should move at all; the
host persona's generation when the host is behind a carrier NAT or a VPN;
how a crawl behaves at level 0 (links from raw HTML only, no clicks).

**Acceptance.** Compose e2e: the same fixture page fetched at every level
with the site's own log proving what differed (no persona and the plain
User-Agent at 0; the persona's browser without scrolling at 2; the full
behaviour at 3); a request without a level refused; a policy floor
enforced; `egress: direct` end to end with the host persona on the
compose network (the world's "host" address) and nothing through the
pool; `just ci` green; the mutation gate on `kage-core`.

**Status.** Merged on 2026-10-20 as
[Serpe-nt/Kage#11](https://github.com/Serpe-nt/Kage/pull/11), four
commits (F65 first, the feature, the docs, the tests that catch the
mutants on its lines); the hand-over is the pull request's body
(`m14-consult-answers.md` holds the decisions). Built: `level` on every
request with the codes `level_required`, `level_below_policy`,
`level_unavailable`, `level_needs_http`, `level_kind_conflict`,
`level_include_conflict`, `session_needs_persona`; `render` gone from
requests and from the policy file, escalation removed; the plain client
(`kage-http`'s `plain.rs`, wreq's own TLS recorded as
`tests/fixtures/fingerprints/plain.json`, a minimal well-formed header
set with `kage/<version>` unless `egress.plain_user_agent` or the caller
names one, in-chain cookies only, caller `Cookie` and `Authorization`
at level 0 only); level 2 as the sidecar's still visit (contract 1.5.0,
`behavior.mode: still`, no input, a seeded dwell above `min_dwell`);
the host persona on a synthetic proxy row (provider `host`, declared
values winning, a lone host persona its own scout, `egress.direct.private_names`);
`egress.plain` `host | pool` with both crossings refused; the site's
scout's address as the one plain exit under the pool; migration 0017;
the runbook `levels-and-egress.md`; the security review's E-series.
Measured: untouched wreq has a fingerprint no common client has, sends
no `Accept` and puts `Host` last, hence the header set; a stock browser
shows a page zero input for a short visit, hence level 2's stillness;
the gate green on a clean clone in 50 min with all nine e2e scripts
(a new `direct.sh`), the mutation score 92.6 % with the twelve survivors
on M14's lines caught. Known limit, documented: level 0 under the pool
needs a site scout that has read none of the site, so a small pool that
has read a site answers `plain_exit_unavailable` for it until one
qualifies. Follow-ups: **F66 (M16, first commit)** the crawl e2e's "a
page that is left" fails about once in four runs under load (a hover
that does not settle on the link while the page still scrolls makes the
hand fall back to typing; 1.x code); **F67 (M16)** refusals made in the
pipeline are not counted in `kage_scheduler_refusals_total`; **F68
(M15)** under `direct` with a provider configured the idle pool is
still probed; **F69 (M16)** the `AttemptView` nullables of level 0.
The planner amended D50, ADR 0012 and `CLAUDE.md` to what was built
(the scout's address as the plain exit, `egress.plain` `host | pool`,
up to four host personas, the credential sentence).

### M15 — Provider-agnostic pool and declared lists

**Scope.** ADR 0013, D51; spec §6.1, §7.1, §7.4, §17 updated.

**Deliverables**
- `ProxyProvider::capabilities()`; the pool, the CLI and the health page
  showing unsupported operations as such; the conformance suite in
  `crates/kage-proxy/tests/provider_conformance.rs` run against the
  Webshare mock and the static list; `CONTRIBUTING.md`'s provider guide
  (a template module, the mock, the suite, the versions row).
- Static entries with `country`, `locale`, `city`; personas generated
  from them; an entry without a country usable at level 0 only, with the
  warning; verification by geo lookup with `geo_mismatch`.
- `proxies.health.echo_url` with the public default for providers
  without an echo, the runbook's note on what the echo's operator sees,
  and the operator's own echo as the alternative.
- `docs/runbooks/proxies.md` rewritten for three ways to run: Webshare,
  a declared list, no proxies (direct).

**Consult first.** Whether a second built-in provider should ship as the
worked example of the guide (a "list URL" provider that fetches
`host:port:user:pass:country` lines from an authenticated URL, which
several vendors offer), and the exact YAML shape of an entry.

**Acceptance.** A declared list of the e2e world's proxies with countries
and locales produces personas whose languages and time zones match the
declarations; a geo mismatch is alerted and the proxy degraded; the
conformance suite passes for both providers and fails for a deliberately
broken mock; `kage proxy health` on a list without a provider shows live
and dead proxies from the echo; `just ci` green.

**Status.** Merged on 2026-10-20 as
[Serpe-nt/Kage#14](https://github.com/Serpe-nt/Kage/pull/14), three
commits (`m15-handover.md`). Built: provider capabilities with the
optional operations shown as unsupported by the pool, the CLI and the
admin view; the conformance suite with a deliberately broken provider
that makes every rule report; `CONTRIBUTING.md` with the provider guide;
declared entries (YAML with a `url` form, or the line form with country
and locale) that build personas, verified by the geo lookup, a mismatch
degraded and alerted with the declared identity kept, a corrected
declaration regenerating the persona; a declared `timezone`; unknown
countries refused at load; shared exits detected; entries without a
country as the pool's plain exits, one per site and kept, which closes
M14's known limit; the echo defaults put right (a provider's echo only
for that provider, the public echo for a list, the probe reading the
operator's own `kage echo serve`, the host's address from a second
public echo); F68 (no probe of the idle pool under `direct`); migration
0018; the runbook `proxies.md` rewritten for three ways to run. Gate
green on a clean clone with ten e2e scripts (a new `declared.sh`),
mutation score 93.4 % with no new survivor. Found and fixed on the way:
1.x probed Webshare's echo through every proxy of any vendor's list and
read the host's address from it. The list-URL provider stays a
follow-up written from the guide. The planner amended D50, D51, ADR 0012
and `CLAUDE.md` for the plain-exit rule.

**Follow-ups raised by M15** (after 2.0.0 unless said)
- **F70:** the `list_url` provider, the first written from the guide (a
  URL per source with its declared country, locale and city; plain wreq,
  https only, no redirects, a size cap, the URL never logged; list-only
  capabilities); whoever writes it notes where the guide fell short.
- **F71 (M16, first commit):** a proxy that never answered gets a
  persona once its backoff ends, because `backoff_expired` makes it
  `degraded` and usable; generate only for a proxy with one successful
  probe.
- **F72:** shrinking a declared list by more than half trips the
  `provider_list_suspect` guard; a declared list is the operator's own
  word, so trust the file (guarding an empty one) or add `kage proxy
  retire`.
- **F73:** IP-authorised entries without a username and password.
- **F74:** a plain exit later given a country releases its sites and
  carries a persona; quarantine those sites for that persona as the
  scout rule does.
- **F75 (M16, first commit):** the `--language` error of the browser
  self-test command prints a literal line break.

### M16 — The public face, and 2.0.0

**Starting point (after M15).** Every feature of 2.0 is built and the
contract is what main holds: the levels (§25), the egress modes, the
providers and declared lists, the Chrome and Camoufox engines of 1.0.2.
M16 makes the public face and cuts the release: the README, the badges
from Kage's own data, the community files, the quick start re-proven on
a Docker-only machine, F66, F71 and F75 as its first commits, the host
persona's live self-test on the owner's word (the only live run of the
phase), and 2.0.0 per the release runbook with the breaking changes
first in the changelog. The repository settings of `docs/runbooks/repo.md`
are still unset and the nightly live suite still has no secrets; both
are the owner's and the README must not claim either.

**Scope.** D52; the README, the community files, the badges, the
release.

**Deliverables**
- The README rewritten (D52): the badges with Kage's own data (a CI job
  writing the coverage JSON to `gh-pages`, shields.io endpoint badges),
  the light ninja voice, the audiences, the levels in plain words, the
  quick start re-verified on a Docker-only machine, the responsible-use
  section kept.
- `CONTRIBUTING.md`, `SECURITY.md`, `CODE_OF_CONDUCT.md`, the issue and
  pull-request templates, `CODEOWNERS`.
- The 2.0.0 release per the runbook: the version bump, the changelog
  with the breaking change first, the owner's signed tag, the digests
  pull request.

**Acceptance.** Every badge renders from the repository's own data on a
fresh fork with no secret; the quick start transcript; `just ci` green;
the release published with the attestations verified.

**Status.** Merged on 2026-10-21 as
[Serpe-nt/Kage#16](https://github.com/Serpe-nt/Kage/pull/16), eleven
commits (`m16-handover.md`); the release steps follow on the owner's
words (the bump pull request, the load job by hand with the nightly's new
`suite` input, the signed tag `v2.0.0`, the digests pull request). Built:
the README of D52 (the shinobi voice in five places, the levels, the
egress modes, the three ways to run the pool, a result explained,
"Measured, not promised" with the live scores quoted as they are and the
known tells named, nothing claimed that is not set up); the five badges
from the repository's own data (a `coverage-badge` job on pushes to
`main` only, `contents: write` on that job alone, one orphan commit of
one JSON file on `gh-pages`, shields.io rendering it; a lint check that
holds the static badges to `rust-toolchain.toml` and `Cargo.toml`);
`SECURITY.md` (GitHub's private advisory form, the latest line
supported, tells as documented limits), `CODE_OF_CONDUCT.md`
(Contributor Covenant 2.1), the issue forms, the pull-request template,
`CODEOWNERS`, `CONTRIBUTING.md` polished with the gates and the provider
guide; the compose pass-throughs for `KAGE_EGRESS_MODE` and
`KAGE_STATIC_LIST_FILE`; the nightly's `workflow_dispatch` input `suite`;
F66 (the hand goes to an element once the page rests from the wheel:
not reproduced on `main` in eight crawl runs and thirty parity runs
under the slow cgroup, but the suspected mechanism reproduced one layer
down and fixed there, then twenty crawl runs green), F69, F71, F75, and
a command-line forwarder fix that a host persona needed. Measured: the
quick start proven word for word on an empty Docker machine (up in
346 s, level 0 in 84 ms, level 3 by the host persona's Chrome in 38 s);
the host persona's live self-test from the owner's home connection, 100
of 100 for a Chrome and a Firefox persona, every check as in the pool's
baselines, so no baseline of its own; the gate green on a clean clone
with ten e2e scripts, the workspace's line coverage 95.7 % (the badge's
figure), kage-core 99.5 %. The owner turned private vulnerability
reporting on before the merge.

**The release.** Six pull requests after #16, all rebase-merged on the
owner's words: #18 and #20 (`fix(deps)`, advisories that appeared during
the release gates, with two test fixes beside them), #19 (`chore(release):
2.0.0`), #21 (F85, found by the running field campaign: every warm-up
visit was judged a soft block because the visit captured no document, in
every 1.x too), #22 (`fix(deps)`: `zstd` 0.14.3, `zstd-safe` 8.2.0,
`zstd-sys` 2.1.2, and the 2.0.1 bump), #23 (the digests of 2.0.1 in
`deploy/compose.yaml`). **2.0.0 was published on 2026-10-27** on the
owner's signed tag at `6b94d80`; RustSec advisories against both `zstd`
lines had been published between its last load run and its tag, and by
reading the code the one against legacy frames (RUSTSEC-2026-0403) is
reachable at level 0 and in the proxy probes, where a site's compressed
body is decoded by the Rust process; not reproduced. Published tags do
not move, so 2.0.0 stands with a security note at the top of its release
page and **2.0.1, the version to run, was published on 2026-10-29** at
`552008b`, verified like 2.0.0 (the tag's signature, cosign on both
images, the SLSA provenance and CycloneDX SBOM attestations, the SBOM
listing the fixed crates). The older `zstd` 0.13 under `wreq-util`
decodes only TLS certificates with a fresh decoder each and is accepted
in `deny.toml` and `.cargo/audit.toml` until 2026-12-31 (F91). The load
test's two failures on the way (59,689 and 59,893 ms against a 59,900 ms
threshold, with every later binding window's median at 60,003 ms) were
the measurement's own noise, not pacing drift (F84). Lessons for the
next release: re-audit right before the tag whenever days pass after the
gate; clear the nested Docker build cache before a release gate (a cached
Debian layer kept an old `libbrotli1`); a hold must reach the owner
before the tag.

**Follow-ups raised by M16** (after 2.0.0)
- **F76:** the 56 schemas whose always-sent response members are
  optional in the OpenAPI document (220 nullable, not required,
  properties; request fields among them where optional is right).
- **F77:** the sidecar's self-test report of a page that could not be
  read says `passed: true, score: 100` beside the verdict `inconclusive`;
  a report that read no page should not say it passed.
- **F78:** nothing tests that the issue forms render; GitHub shows a
  broken form as plain text, seen on the first new issue.
- **F79:** `kage persona list` under `docker compose exec` prints a pool
  INFO line on stdout before the table.
- **F67 stands** (after 2.0.0): pipeline refusals in
  `kage_scheduler_refusals_total`.
- **F80:** `setxkbmap`'s 10 s timeout on a loaded runner (a longer
  timeout or one retry for the layout step).
- **F81:** a Firefox parity test read a button hold of 19 ms on a loaded
  runner.
- **F82 (fixed by [Serpe-nt/Kage#31](https://github.com/Serpe-nt/Kage/pull/31)):**
  the `fast-uri` override to 3.1.9 / 4.2.3 in the sidecar and
  the API client (four advisories, moderate and low, below the `pnpm
  audit` gate; 2.0.0 and 2.0.1 ship with them, their changelogs do not
  name them).
- **F84:** the load test measures from the grant time (or the gate's own
  clock), not from a start recorded after a database read, with its
  tolerance argued against the measurement's noise; the site-side count
  (21 to 23 arrivals in a busiest minute) is its other question.
- **F86:** `observe_page` reads an absent body as empty, so a page
  captured without a document is judged on nothing (how F85 hid); a page
  with neither `dom_html` nor `raw_body` should say so, not look blank.
- **F91:** drop the three `zstd` 0.13 ignores (RUSTSEC-2026-0391, -0398,
  -0403) with the first `wreq-util` on `zstd` 0.14, or review them by
  2026-12-31.
- **F92 (fixed by [Serpe-nt/Kage#25](https://github.com/Serpe-nt/Kage/pull/25)):**
  the README's CI badge read "failing" while `ci-ok` was green, because
  the report-only mutants job reached its 30-minute cap and
  `continue-on-error` does not cover a timeout, so every run on `main`
  ended `cancelled`; the run is bounded inside the job (`timeout
  --signal=INT --kill-after=1m 26m just mutants`, exit 124 accepted as
  the budget spent) and the runbook's jobs table says so.
- **F93 (fixed by [Serpe-nt/Kage#32](https://github.com/Serpe-nt/Kage/pull/32)):**
  five advisories against `zip` <= 9.0.4 (CVE-2026-115905 to
  -115909) are on GitHub and will reach RustSec; `zip` 8.6.0 is build-only
  under `utoipa-swagger-ui` (F55's reasoning): extend F55's exception or
  move `utoipa-swagger-ui` when they do.
- **F94:** the field runner's merge review (`tests/field/`, at #24,
  nothing blocking, for the session that runs the second round): the
  watch loop of `run()` has no retry around its API calls, so one
  transient error leaves the crawl unwatched with its session open
  (retry a few times, then cancel the crawl and close the session);
  `afterBlock` and `COOLDOWN_MS` copy the domain policy's cooldowns by
  hand, without their jitter (the admin persona view says
  `cooling_reason` but not `cooling_until`; expose it and read it);
  `SITE_BYTES` counts from `date_trunc('hour', from)`, so a batch
  started mid-hour is charged the hour's earlier traffic; `record()`
  aborts when a consent artifact is gone (`GET /v1/artifacts/{id}`
  expects only 200; a 404 is "no consent read"); `consentIn`'s "unnamed"
  heuristic counts any cookie or consent class with accept or agree
  anywhere in the page; `entry` rebuilds `site-${index}.example` instead
  of calling `anonymousKey()`, without its lowercase; `leaks()` takes
  redirect hosts as `extra` that `report()` never passes; the
  hour-rounding regex is written twice (`summary()`, `renderVoid()`).
  F88's two notes are under "After 2.0.0".
- **F95 (fixed by [Serpe-nt/Kage#29](https://github.com/Serpe-nt/Kage/pull/29)):**
  the browser self-test retries an unreachable detection page once; the
  first nightly live run of 2026-10-30 failed on one timed-out load of
  bot.sannysoft.com through one proxy while the other three self-tests
  passed. The sidecar loads a page whose navigation failed (a timeout, a
  proxy or a network error) a second time after a fixed pause of five
  seconds, through the same context, proxy and persona, and judges it by
  that load; a page that fails twice stays `unreachable`, a page that
  arrived is never loaded again. The page report says how many loads a
  page took (`attempts`, contract 1.6.0, additive), the live check prints
  it, and its rule that a verdict rests on all checks is unchanged.

---

## After 2.0.0

2.0.1 (2026-10-29) is the version to run; the field campaign's first
round is done (M13 above). What remains, in the order the planner would
take it, each a pull request of its own on the PR workflow, with the
owner's word for every merge:

1. **The public face and the gates:** F91 by 2026-12-31 (F82, `fast-uri`,
   F93 and F55, `zip`, and F92, the CI badge, are fixed).
2. **What the field campaign found, the stealth items first:** F89 (the
   behaviour engine answers a consent dialog by hand before its first
   scroll, reject non-essential by default or accept where a site's policy
   says so, the answer kept in the persona's profile so the next visit
   sees no dialog, the result saying what it saw and answered; F64's
   typed address stays the fallback); F90 (a 304 to the robots.txt scout
   keeps the stored file and refreshes its TTL; with no stored file the
   scout asks once more bypassing the HTTP cache; only that failing is
   unreachable); F88 (rules that count visible text or body bytes abstain
   on a body that was cut, the sidecar's rendered-text measure
   preferred, `max_body_bytes` bounding the regex scan only; until then
   every body rule scans up to the 16 MiB the shipped signatures read,
   and the classifier test
   `a_page_whose_head_outgrows_a_mebibyte_is_judged_by_its_body` pins
   the 1 MiB soft-block verdict F88 replaces); F86; F59
   (the dwell floor bounds the persona's own draw); F53 (a redirect's
   target checked by robots.txt and by `deny`); F83 (the shipped warm-up
   list without `Special:Random` and without the pages that cost tens of
   megabytes a visit; the round spent 400 MB of 851 on warm-up).
3. **What the caller is told (the owner's ruling of 2026-10-25: no limits
   per persona per query, the caller decides, and is told):** F87, an
   advisory in the accept response of a fetch, job or crawl whose asked
   volume exceeds what its visitors read in a day at the pacing in
   force (how many days it will take, or that `visitors` or a custom
   pacing changes that), and the README and the OpenAPI document saying
   in plain words what a person's day on one site looks like and that
   more from one address looks like a bot; the daily cap of the pacing
   profile stays a default the caller may replace (D9), never a wall.
4. **The rest of the phase's list:** F84, F70 (the `list_url` provider
   written from the guide), F76, F67, F77, F57 (the security review's
   six), F58 (the mutants job on a larger runner or the diff only), F72
   to F74, F78, F79, F80, F81, the Camoufox and Chrome majors as upstream
   publishes them.

A second field round runs whenever the owner wants it, on the released
images, by a session the owner starts, with F94 (the runner's review)
fixed first: return visits for the clean sites, a second pass for the
three refused at page 1, and the consent answer once F89 is built; F31,
F41, F56, F61 and F62 still wait for data. The
owner's own items stay the owner's: the repository settings of
`docs/runbooks/repo.md` (branch protection, Actions restrictions, the
description and topics, wiki and projects off; private vulnerability
reporting is on), the two Actions secrets and the nightly live suite,
and the sentence in `SECURITY.md` that promises an answer within a week.

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
