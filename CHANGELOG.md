# Changelog

All notable changes to Kage are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- M6, the persona manager and session binding:
  - A new crate, `kage-persona`. A proxy that is usable gets one persona,
    generated from the observed geo record of its address: time zone,
    a position near the lookup's coordinates, the country-carrying locale
    and the language list a stock installation of that browser ships with
    in that country (which differs between Chrome and Firefox), an entry
    of `personas.mix` drawn by what the pool and the country are short of,
    and a screen class from a seeded table that the generator is asked
    for by exact bounds. The sidecar's fingerprint is stored whole. The
    persona's seed, in its own column, is what every draw hangs on.
  - A persona follows its proxy: cooling in backoff, a new time zone and
    position when the geo record moves, new locale and languages and a
    cooling until a self-test passes when it moves across a border, and
    retirement with the proxy, whoever replaced it. A retired persona's
    sessions end and its http cookies are deleted at once; its profile
    directory is archived after ten minutes and the archive deleted
    after `personas.retention_days`. `kage serve` runs the manager.
  - Sessions: ids unique per API key, pinned to the least loaded ready
    persona, with an idle and a maximum lifetime (`sessions.*`).
  - Per-domain state on the persona's own calendar: cooldowns after a
    block, the daily cap until local midnight, block counters.
  - The http cookie jar belongs to the persona (`http_cookies.persona_id`),
    and cookies move between its browser context and its http jar in both
    directions, unless its profile is not its browser's major.
  - Warm-up (`personas.warmup`, on by default): a new persona visits a few
    popular neutral sites in its own browser during its first hours and is
    given real work only after three of them or twelve hours.
  - Self-tests per persona, every 24 hours with a jitter of its own, only
    against the operator's own echo and pages; a failure cools the persona
    and raises `http_selftest_failed` or `browser_selftest_failed`, a run
    that cannot read its test site changes nothing. A health probe now
    presents as the persona behind the proxy it checks.
  - `kage persona list|show|retire|selftest|regenerate`, and `--persona`
    on `kage selftest http` and `kage selftest browser`.
  - Pacing: a persona's own factor, log-uniform in 0.8 to 1.25 from its
    seed, for the median and the 95th percentile of its gaps.
  - The replacement budget counts per subscription term when the provider
    reports one. `webshare.accepted_auto_replace` names the automatic
    replacement settings that raise no alert (default
    `auto_replace_invalid_proxies`). The geo record keeps the lookup's
    coordinates.

- M5.1, page reads without user activation:
  - The Chromium engine reads a page over its own debugger session:
    title, text, DOM, selector waits, storage and the measurements of the
    behavior engine are evaluated with `Runtime.evaluate` in an isolated
    world of the engine's own, without the user-gesture flag the driver
    sends. The driver keeps the launch, navigation and its waits, and the
    input of `human: false` steps.
  - The pinned Camoufox build runs with upstream's Juggler fix backported
    into its `omni.ja` (`scripts/patch-camoufox-juggler.mjs`, ADR 0010),
    in the image and after `just setup-camoufox`.
  - The coherence probe compares `navigator.userActivation` and a new
    `AudioContext` with the trusted input events it received:
    `automation.user_activation`, a hard failure, and
    `automation.audio_autoplay`, a warning.
  - The fixture site's activation recorder (`/activation/`) and a parity
    test on both engines: an untouched page is not active and its
    `AudioContext` is suspended at `load`, 300 ms and 4 s later, with the
    captures taken; a click activates it.

- M5, the Firefox engine and the behavior engine:
  - The Firefox engine: Camoufox `v152.0.4-beta.30`, fetched by URL and
    checksum, one browser per persona on its own X display with a
    persistent profile and an authenticated proxy. A Firefox persona claims
    Windows or macOS; the claim was measured coherent in the window, every
    kind of iframe and both kinds of worker, fonts and WebGL included
    (`docs/runbooks/engine-firefox.md`). The fingerprint is generated once
    from the persona's seed and persisted as the browser's whole
    configuration; an open generates nothing. The graphics card is drawn
    from the launcher's table of real devices without the rows that name
    another system's renderer or a software one.
  - The behavior engine, shared by both engines: mouse paths (curved, timed
    by Fitts' law, with tremor, overshoot and correction), wheel scrolling
    in flicks with reading pauses proportional to the text in view, dwell
    from a log-normal, typing with per-key timing, modifier keys and
    optional typos, and an idle hand that wanders. Each persona has its own
    motor profile from its seed; each request its own recorded seed.
  - Input goes through the mouse and keyboard of the context's own X
    display (XTEST), not through a browser driver, on both engines, with
    the keyboard layout of the persona's locale.
  - `navigate` takes `behavior`: scroll depth, human dwell, wander, and the
    entry strategies `none`, `direct`, `link` and `search_engine`, which
    arrive by a real click with the browser's own `Referer`.
  - The flow endpoint: the v1 actions (`goto`, `click`, `type`, `press`,
    `scroll`, `wait_for`, `select`, `hover`, `back`, `extract`,
    `screenshot`, `pdf`, `assert`) streamed as NDJSON; `SidecarClient::flow`
    in `kage-browser`.
  - An engine parity suite: the same contract, capture and behavior tests
    run against both engines.
  - Both images announce a desktop to their browsers: a work area smaller
    than the screen and a sound server with an output and an input
    (follow-ups F9 and F12 of M4).
  - The coherence probe measures fonts and WebGL in every scope and
    reports what surrounds the browser (work area, media devices, audio).
  - `just capture-browser-fingerprint firefox`, the capture
    `tests/fixtures/fingerprints/browser/firefox-152.json`, and Firefox 152
    recorded as browser-only on the http path (its TLS offers one cipher
    suite fewer than the newest profile; a stock Firefox 152.0.4 measures
    the same as the Camoufox build).
  - `just setup-camoufox`, `kage selftest browser --engine firefox`,
    `engines.firefox.browser_major`.
  - The live browser self-test covers the Firefox engine, with its
    recorded baseline `tests/fixtures/selftest/firefox-152-live.json`. The
    judge of bot.sannysoft.com reads that page's Chrome row by the browser
    the page saw: a Firefox has no `window.chrome`.

- M4, the browser sidecar and its Chromium engine:
  - `@kage/contract`: zod schemas for every endpoint of the sidecar
    (health, metrics, persona generation, contexts, navigate, flow, state
    export and import, self-test), the OpenAPI document emitted from them
    and committed (`sidecar/packages/contract/openapi.json`), and a check
    that fails a change which breaks clients without a new major of the
    contract (`just contract-check`, part of `just ci`).
  - The sidecar server: every route behind the shared secret, bodies
    validated in and out, Prometheus metrics, a context manager with a hard
    cap, idempotent opens, idle close and browser recycling, and artifacts
    written to the shared volume with their checksum.
  - The Chromium engine: real Chrome stable, pinned by version and package
    checksum, headful on one X display per persona, with a persistent
    profile and an authenticated proxy. Loopback goes through the proxy and
    WebRTC sends nothing around it. Chrome's sandbox stays on.
  - A chromium persona claims the system its Chrome runs on, Linux, and is
    made by how the browser is launched, with no script in any page
    (ADR 0009). `engines.chromium.allow_cross_os_claims` exists and is off.
  - `navigate` with waits and captures that a page cannot notice: DOM, raw
    body, headers, redirect chain, XHR and fetch bodies, the browser's log,
    timing and challenge signals. Screenshots and PDFs are files on the
    artifacts volume; a capture with a visible side effect is opt-in and
    named in the result.
  - `state/export` and `state/import` in the shape of an `http_cookies`
    row, so a session moves between the http path and a browser.
  - The self-test endpoint and its pages: a coherence probe (window,
    frames, workers, request headers, cache), the fingerprint echo, and
    readers for bot.sannysoft.com and CreepJS. Third-party pages are loaded
    only on request.
  - `kage-browser`: Rust types generated from the committed OpenAPI
    document at build time, a client with timeouts and retries on
    connection errors only, the post-hoc SSRF check of a navigation's
    redirect chain, and a contract test that fails when the document and
    the client drift apart.
  - `kage selftest browser`: runs the self-test for one proxy through the
    sidecar. `selftest.browser` in the configuration names the pages.
    `--idle-before` leaves the context untouched first, and the gated live
    check uses it to measure what an idle Chrome sends by itself
    (`KAGE_LIVE_IDLE_SECONDS`).
  - The fingerprint echo serves `/browser`, a page that makes a real
    browser show its navigation, `fetch()` and `XMLHttpRequest` requests;
    `just capture-browser-fingerprint` records them. Chrome 155 is recorded
    and is browser-only on the http path: its TLS fingerprint is newer than
    any emulation profile (`BROWSER_ONLY_MAJORS`).
  - The Chromium sidecar image and its hardening: non-root, read-only root
    filesystem, no capabilities, and a seccomp profile derived from
    Docker's default that lets Chrome's sandbox work. The stack smoke test
    checks it with a context open.
  - Runbook `docs/runbooks/engine-chromium.md`.

### Fixed

- One Firefox persona in thirty that claims Windows had no
  `navigator.oscpu` in its generated configuration and showed the engine
  host's `Linux x86_64` in its window. The generator now writes the stock
  value of the claimed system when the device record has none.
- The stand-in persona of `kage selftest browser --proxy` took its
  languages from the caller, and the live suite passed `de-DE, de, en` for
  both engines, a list no stock Firefox sends. Its time zone, locale and
  languages are now derived from the proxy's geo record, like a persona's.

- Reading a page no longer grants it user activation on either engine
  (spec §10.2): both drivers marked every evaluation as a user gesture, so
  a page nobody touched read `navigator.userActivation` as active about
  300 ms after `load` and Chrome let an `AudioContext` run.
- `kage-proxy/tests/reputation.rs` no longer fails by chance (follow-up
  F11): its windows were shorter than the spread of the first lookups for
  about one set of random proxy ids in four. The windows now cover the
  spread and the assertions are per address.

### Changed

- The Firefox engine refuses a Camoufox build whose `omni.ja` is not the
  one with the Juggler backport, and removes a profile's `startupCache`
  when the profile last started under another archive (a note,
  `omni.sha256`, in each profile directory). An existing developer
  install needs `just setup-camoufox` once.
- A Firefox persona answers `navigator.getAutoplayPolicy()` as a stock
  Firefox does (`allowed-muted`, `disallowed`) and keeps an `AudioContext`
  suspended until its user acts: the launch plan sets
  `media.autoplay.default` to Firefox's default, where the Camoufox build
  ships "allow". The probe's `automation.audio_autoplay` warns otherwise.
- A self-test against a probe page older than M5.1 reports
  `automation.user_activation` as a warning: update the copy on your echo
  host from `tests/fixtures/site/probe/`.
- The sidecar contract is 1.1.0: `behavior` on navigate and flow requests
  and on their results. Nothing was removed.
- `ua-parser-js` is overridden to its MIT 1.0.x line and the license
  exception is gone (ADR 0008). The release workflow does not publish the
  Firefox image: the Camoufox build bundles Microsoft's and Apple's fonts.
- The default persona mix is chromium on linux 40 %, firefox on windows
  45 %, firefox on macos 15 % (spec §6.1).
- A chromium persona's window sits below a 32 px top bar that its display
  announces, so `screen.availHeight` is smaller than `screen.height`.
- The sidecars need the shared secret on every route, the health endpoint
  included; the container healthcheck sends it.
- `sidecar` in the configuration gained each engine's URL, the timeouts
  and the connection retries.
- The JA4 the echo computes leaves GREASE out of the signature algorithms
  as well, which Chrome 155 now sends there.

- M3, the http path:
  - `kage-http`: fetches a page through a proxy with wreq, looking like the
    browser a profile names. A client factory keeps one client per proxy and
    profile; wreq-util supplies TLS and the HTTP/2 settings, and Kage writes
    the complete header list itself, in the browser's order and spelling,
    for Chrome and Firefox, over HTTP/2 and HTTP/1.1, for a page and for its
    stylesheets, scripts and images. `Sec-Fetch-Site` follows the referrer
    over the whole redirect chain, and the `Referer` that goes out follows
    the browsers' default policy.
  - Caller headers are checked against a denylist. A header that belongs to
    the browser's identity (`User-Agent`, `Accept-Language`,
    `Accept-Encoding`, `Cookie`, every `Sec-*` and others) is refused with
    an error that names it, never dropped silently.
  - Redirects are followed by Kage, ten at most by default, and every hop
    passes the SSRF guard again; the refused hop is named. The body is read
    up to `http.max_body_bytes`, and the result says when it was cut.
  - A cookie jar with the rules browsers apply (`SameSite`, `Secure`, the
    `__Secure-` and `__Host-` prefixes, the Public Suffix List, limits and
    eviction), stored in Postgres per proxy (migration 0009,
    `http_cookies`). It survives a restart, is never handed to a
    replacement proxy, and forgets session cookies after
    `http.session_cookie_idle`.
  - Timing per phase: tunnel (`connect_ms`), TLS handshake (`tls_ms`), first
    byte and total. The health probe records the first two per proxy, and
    `kage proxy health` shows them, so a proxy that is slow to set up a
    connection can be told from one that is slow to answer.
  - Subresource emulation (`http.fetch_subresources`, off by default): after
    a page, its own site's stylesheets, scripts and icon are fetched the way
    a first paint would, with seeded pauses and a freshness cache. Assets of
    other sites are never fetched.
  - The profile mapping of spec §6.1: (engine, browser major, OS) to a
    wreq-util profile, for the newest eight majors of Chrome and Firefox. A
    test enumerates wreq-util's profiles and fails when a configured engine
    major has none. `engines.chromium.browser_major` and
    `engines.firefox.browser_major` name the engines' majors.
  - A fingerprint echo inside `kage-http`: a TLS server that reports the JA4
    of the ClientHello, the HTTP/2 fingerprint and the request headers in
    wire order. `kage echo serve` runs it; the compose profile `echo` adds
    it to the stack.
  - The fingerprint gate: the JA4, the HTTP/2 fingerprint and the header
    order of all 48 mapped profiles are recorded in
    `tests/fixtures/fingerprints/` and compared on every change in
    `just ci`, over loopback. A dependency bump that moves one fails there.
  - `kage selftest http --proxy <id> --profile <name>`: the same comparison
    through a real proxy, against the operator's echo
    (`selftest.http.echo_url`) or, on demand only, a third-party one. The
    server never asks a third-party echo on a timer.
  - New operator alerts: `http_selftest_failed`, `http_profile_missing`.
  - Configuration: new sections `http`, `selftest` and `engines`.
  - `docs/runbooks/fingerprint-echo.md`; a gated live check of the
    fingerprint through two real proxies (`tests/live/http-selftest.sh`).
  - The coverage gate now includes `kage-http`.

- M2, proxy provider, pool, health and replacement:
  - `kage-proxy`: the `ProxyProvider` trait with two implementations. The
    Webshare client (on wreq, no second TLS stack) reads the proxy list page
    by page, requests and follows replacements, reads the log of replaced
    proxies, the account (plan, replacement quota, automatic-replacement
    settings, stock per country) and hourly traffic. It keeps its own request
    rate below Webshare's limits, waits out a 429, and never sends a request
    that changes something twice. The static list provider serves a fixed
    list from `KAGE__PROXIES__STATIC_LIST`, for tests and other vendors.
  - The pool: stored proxies are kept in step with the provider every
    `proxies.sync_interval`. A proxy missing from the list is retired at once
    only when the provider's own log confirms it, otherwise after three
    syncs, and never on the word of an empty or far too short list.
  - The pool state machine of spec §7.3 as pure functions with property
    tests: three failed checks send a proxy into backoff (1 min doubling to
    1 h, jittered), one probe follows every backoff, and a proxy is replaced
    only after three backoff cycles and six hours of failing without a break
    (`proxies.replace_after`), within `proxies.replacements_per_month`. The
    budget is enforced in Postgres, so the server and the CLI cannot overspend
    it together. Replacements ask for the same country.
  - Health checks, each proxy on its own jittered schedule: reachability,
    latency and exit address through the provider's own echo (a second echo
    is tried once before a failure counts), a leak check against the host's
    own address, a geo lookup of the exit address on the first check, on an
    address change and once a day, and optional IP reputation lookups
    (AbuseIPDB, off without a key). When more than half of the pool fails at
    once the failure counters freeze: the cause is on the path.
  - A proxy that lets the host's own address through is parked and sets the
    pool-wide `egress_unsafe` flag, which later milestones must honor before
    dispatching anything. It clears when a probe through that proxy shows a
    distinct exit address, or with `kage proxy egress --clear`.
  - Proxy user names and passwords are encrypted at rest with
    XChaCha20-Poly1305 under `KAGE__SECRETS__MASTER_KEY`, each value bound to
    its proxy and its field.
  - `kage proxy list`, `kage proxy health`, `kage proxy replace` and
    `kage proxy egress`, as tables or with `--json`; they may run next to the
    server. `kage serve` syncs the pool and runs the health checks when a
    provider is configured; proxies stored from an earlier provider are
    still health-checked when none is.
  - New operator alerts, stored as health events: proxy leak, unsafe egress,
    geo mismatch, bad reputation, suspicious usage type, replacement budget
    exhausted, replacement failed, pool outage suspected, pool capacity low,
    suspect provider list, provider replaced a proxy, provider
    auto-replacement enabled, provider account, provider sync failed, host
    address unknown.
  - Configuration: new `proxies` section, more keys under `webshare` and
    `reputation`, the secret `KAGE__PROXIES__STATIC_LIST`. The provider is
    chosen by `proxies.provider` (`auto` by default: Webshare when its key is
    set, else the static list, else none).
  - `docs/runbooks/proxies.md`; a gated live check of the real proxy list
    (`just test-live`, `tests/live/proxy-health.sh`).
  - The compose stack and `just dev` pass `KAGE_WEBSHARE_API_KEY` and
    `KAGE_ABUSEIPDB_KEY` from `deploy/.env` on when they are set. The smoke
    test never uses them.

- M1, core domain, configuration and store:
  - `kage-core`: the domain model of spec §5 (proxies, personas, sessions,
    requests, attempts, results, artifacts, jobs, keys, events) with typed
    UUIDv7 ids, `Outcome` and the block report.
  - Domain policies (`policies/domains.yaml`): each field is inherited from
    the exact host, then its parent domains from nearest to furthest, then
    `defaults`; per-outcome cooldowns plus a `short` cooldown; pacing
    resolution request, then domain, then default, with `pacing_locked`.
  - Pacing profiles (`policies/pacing-profiles.yaml`) with seeded, replayable
    samplers: log-normal by median and p95 (shifted, so a `min` leaves neither
    a spike nor an edge at the floor), uniform, and bursts with breaks.
  - Block classifier: signatures as data (`policies/block-signatures.yaml`)
    with matchers on status, headers, cookies, body, URL and browser signals,
    covering Cloudflare, Akamai, DataDome, HUMAN, Kasada, Imperva, AWS WAF,
    F5, reCAPTCHA, hCaptcha, generic statuses and soft blocks. Each rule says
    whether a match is retried, escalated to the browser and how long the
    persona cools down; every verdict lists the markers that fired. A
    vendor's cookie on a good page is a hint, never a block. 44 fixtures
    under `tests/fixtures/blocks/`, all synthetic and marked as such.
  - SSRF guard as pure functions: denied address ranges, name rules, literal
    IP targets behind `security.allow_ip_targets`, and a redirect chain that
    re-checks every hop, records `https` to `http` downgrades and names the
    hop it refuses.
  - URL normalization into a dedupe and cache key (spec §13), including
    RFC 3986 percent-encoding normalization. Requests keep the URL as
    discovered.
  - Configuration: `kage.yaml` layered with `KAGE__SECTION__KEY`, secrets
    only from the environment or `_FILE` variables, and `kage config check`,
    which prints the effective configuration with secrets redacted.
    `deploy/kage.example.yaml` lists every key.
  - Policy hot reload: a changed file is validated and swapped in; an invalid
    one keeps the last good policy and raises an alert, stored as a health
    event.
  - `kage-store`: migrations for the 18 tables of spec §18, repositories with
    compile-time checked queries and committed offline data (`.sqlx/`), and
    the content-addressed artifact store with gzip for text and a retention
    job.
  - `kage migrate` (and `--check`); `kage serve` now requires a reachable,
    migrated database and valid policies. The compose stack runs the
    migrations in a one-shot `migrate` service.
  - `just sqlx-check`, `just sqlx-prepare`, `just config-check`; the coverage
    gate now includes `kage-store`.

- M0, repository scaffold and CI gates:
  - Cargo workspace (edition 2024, Rust 1.99.0, workspace lints) with the
    `kage` binary and eight library crates. `kage serve` exposes `/healthz`;
    `kage healthcheck` probes it for the container healthcheck.
  - Sidecar pnpm workspace (TypeScript strict, ESM) with the `contract`,
    `engine-core`, `engine-chromium`, `engine-firefox` and `server` packages.
    The server exposes `/v1/health`; the `ENGINE` variable selects the engine
    package; both engines are stubs.
  - `justfile` with every dev and CI command; `just ci` runs everything a
    pull request runs.
  - Dockerfiles for `kage` and both sidecar variants, and a compose file with
    Postgres 17, healthchecks and hardened containers.
  - GitHub Actions: `ci.yaml`, `nightly-live.yaml` (skeleton) and
    `release.yaml`.
  - Supply-chain gates: cargo-deny, cargo-audit, pnpm audit, a license policy
    for the sidecar, gitleaks, Trivy, CycloneDX SBOMs, and a check that fails
    when `openssl-sys` is in the Rust dependency graph.
  - Coverage gate: at least 80 % line coverage on `kage-core`, `kage-proxy`,
    `kage-crawl`, `kage-extract` and `engine-core`.
  - README with a responsible-use section, dual MIT / Apache-2.0 license
    files, CODEOWNERS and the repository runbook.
  - Exact dependency pins re-verified and recorded in `docs/versions.md`.
  - ADR 0008 (proposed): AGPL-licensed `ua-parser-js` in the Firefox engine
    dependency tree.

### Changed

- The health probe is built by `kage-http`. `proxies.health.profile` must
  name a desktop Chrome or Firefox profile (Edge, Safari, OkHttp and the
  rest are refused), and Firefox on Linux is refused; `kage config check`
  says so. The default (`chrome_149` on Windows) is unchanged.
- `kage proxy health` shows the time each proxy took to open the tunnel and
  to finish the TLS handshake, next to the total.
- `kage config check` validates the http path too and reports the profiles
  this build serves under `http`.
- `ProxyEndpoint` moved from `kage-proxy` to `kage-core`; `kage-proxy`
  still exports it.
- `KAGE__SECRETS__MASTER_KEY` must be 32 random bytes written as 64
  hexadecimal characters; any other value now fails at startup. `just setup`
  has always generated it in this form.
- The `proxies.username` column holds ciphertext (migration 0008). The
  migration adds the columns the pool state machine needs and lets a retired
  proxy share its provider id with its successor.
- `kage config check` also validates the proxy pool settings (the probe's
  emulation profile must exist in this build) and reports which proxy
  provider is in force.
- A secret variable that is set but empty no longer clashes with its `_FILE`
  form: the file is used.
- `kage serve` takes its listen address from `server.bind`
  (`KAGE__SERVER__BIND`) or `--bind`, and fails at startup when a required
  secret is missing. `deploy/.env` gained `KAGE_SIDECAR_TOKEN` and
  `KAGE_MASTER_KEY`; `just setup` adds them to an existing file.
