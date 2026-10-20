# Changelog

All notable changes to Kage are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

The next release is 2.0.0: camouflage levels and egress (spec §25, ADR
0012, decision D50), and a provider-agnostic pool with declared lists
(spec §7.1, ADR 0013, decision D51). **Drain the job queues before
upgrading**, and **give every entry of a static list its country** (below):
an item
of 1.x still queued names no level and fails at take-up with
`level_required`; nothing is decided for the caller.

### Breaking

- **Every request names `level`**, 0 to 3, and a request without one is
  refused with `422 level_required`: `POST /v1/fetch`, every item of a
  batch (its own or `defaults.level`), a crawl's `defaults.level`, a
  flow's `level` (2 or 3), and `kage fetch --level`. There is no default.
  0 is a plain HTTP client that says it is Kage (`User-Agent:
  kage/<version>` unless the request names one), with no persona; 1 a
  persona's http client (what `render: http` was; refused with
  `level_unavailable` while no browser version of the installation has a
  verified profile, which is the case for the shipped Chrome 155 and
  Camoufox 156); 2 a persona's browser that loads the page, sends no input
  and leaves; 3 a persona's browser as a person (what 1.x did in a
  browser). Below 3 Kage is not pretending to be a person.
- **`render` is gone**: from requests (an unknown field now), from `kage
  fetch` (`--render` is refused; `--level` is required), from the domain
  policy file (a file that still has `render`, including the 1.x default
  `render: auto`, is refused with a message that names `min_level`: remove
  the key), from the cache key, and from the labels of
  `kage_requests_total`, `kage_attempts_total` and
  `kage_attempt_duration_seconds`, which carry `level` (`"0"` to `"3"`).
  The shipped dashboards and alert tests follow, and the block-rate alert
  counts levels 1 to 3 only: a block at level 0 is no breaker's.
- **Nothing escalates.** A block on the http path is never retried in a
  browser: the level is the request's ceiling. Results no longer have
  `escalated`; a block signature with `escalate` is refused.
- **Error codes renamed**: `render_needs_http` → `level_needs_http`,
  `render_kind_conflict` → `level_kind_conflict`, `render_include_conflict`
  → `level_include_conflict`. New: `level_required`, `level_below_policy`
  (a site's `min_level`), `level_unavailable`, `session_needs_persona` (a
  session at level 0).
- The response cache's key is at version 2 (the level replaces the
  render): no entry of 1.x is served after the upgrade.
- An attempt's `persona` and `engine` are null at level 0, which says its
  `exit` (`host` or `pool`) instead; `attempts.persona_id` and
  `attempts.engine` became nullable in the store (migration 0017).
- The policy preview says `min_level` in place of `first_render`.
- **A static list's country is a declaration** (ADR 0013): the persona of
  an entry follows the country, locale and zone the entry declares, and a
  geo lookup only verifies them (`proxy_geo_mismatch` after two that
  disagree, the persona's identity kept). In 1.x the lookup's country won.
  At the first 2.0 start a persona whose lookup had placed it in another
  country or language than its entry declares is made again
  (`declaration_changed`), and **an entry without a country loses its
  persona** (`undeclared`) and serves as a plain exit for level 0 only.
  Add a country to every entry that should carry a persona.
- A declared country the browser tables do not know (they know AT, BE,
  CA, CH, DE, ES, FR, GB, IE, IT, LU, NL, PL, PT, RO, SE, US) is refused at
  load unless the entry also declares a locale in a language they know;
  1.x gave such a persona `en-US, en`. The same rule holds for
  `egress.direct.country`.
- **The probes of a static list read the public echo**
  `https://checkip.amazonaws.com/` and the host's own address
  `https://api.ipify.org/`, where 1.x read Webshare's echo whatever the
  vendor; `proxies.health.echo_url` and `host_ip_url` are unset by default
  and resolve by provider (Webshare's own echo for Webshare). Set them to
  keep 1.x's endpoints, or to your own echo.
- Entries of a static list that share a host and port (a gateway's
  sessions) need an `id` each; their default ids collide and the list is
  refused.
- The `ProxyProvider` trait has `capabilities()`, and `replace`,
  `replace_status`, `replaced`, `account` and `stats` answer
  `Unsupported` by default; the static list now answers `Unsupported` for
  all of them (in 1.x it returned an empty log, an account and no stats).

### Added

- `level` on every result, and the still visit of level 2: the dwell is
  the site policy's `min_dwell` drawn upward (median one and a half times,
  95 % under three times), with no scrolling, no wandering pointer and no
  patience. Sidecar contract 1.5.0 (additive): `behavior.mode` (`human`,
  `still`) and a flow's `pace.wander`.
- Level 0, the plain client: wreq with its own TLS and HTTP/2, a minimal
  header set in a fixed order (recorded as
  `tests/fixtures/fingerprints/plain.json` and compared in the gate), the
  caller's headers and body as given (`Cookie` and `Authorization` too:
  sent to the request's origin only, never stored or logged), cookies a
  redirect chain sets kept for that chain only. It waits for the site's
  own limits (concurrency, requests per minute, the breaker's pause, the
  pause after server errors) and nothing of a persona's; a block is the
  answer at once and cools nobody. Under the pool it leaves through the
  site's scout's address and no other (`plain_exit_unavailable` while that
  proxy cannot carry traffic).
- `min_level` in the domain policy (0 by default); the shipped file sets 3
  for the detection pages (`bot.sannysoft.com`,
  `abrahamjuliot.github.io`, `browserscan.net`, `pixelscan.net`,
  `browserleaks.com`, `fingerprint.com`, `demo.fingerprint.com`).
- `egress.mode: direct`: Kage fetches from the host's own connection, with
  host personas (`egress.direct.personas`, 1 to 4) placed by the host's own
  geo lookup or by `egress.direct.country` and `egress.direct.locale`, on
  the engines `egress.direct.engines` names; the forwarder resolves and
  checks every target itself (`egress.direct.private_names` for names the
  operator allows to be private). Nothing leaves through the pool then,
  and under `egress.mode: proxies` nothing leaves from the host's address:
  `kage config check` and the start refuse both crossings, and the check
  reports `egress` with its warnings. `egress.plain_user_agent` sets the
  plain client's name.
- The bandwidth is counted by exit (`kage_bandwidth_bytes_total{egress}`,
  `bandwidth_usage.egress`); only the pool's counts against the
  provider's budget. `kage_jobs_accepted_total` and `kage_job_items_total`
  carry `level`.
- **Declared lists** (ADR 0013, M15): an entry declares `country`, and may
  declare `locale` (BCP 47, overriding the country's language) and
  `timezone` (IANA), with `city` informational; the YAML form also takes a
  proxy as `url: http://user:pass@host:port`, and the line form a sixth
  field, `host:port:username:password[:country[:locale]]`. A list given as
  `KAGE__PROXIES__STATIC_LIST_FILE` is read again at every sync; an edit
  that does not parse keeps the last good list (`static_list_invalid`). A
  corrected country or language makes a new persona; a corrected zone moves
  the one there is. `egress.direct.timezone` declares the host personas'
  zone by the same rule.
- **Plain exits**: a declared entry without a country carries no persona
  and is level 0's preferred exit under the pool, one per site, kept
  (`site_exits`, migration 0018; the declared `locale` and `timezone` on
  `proxies` too).
- **Provider capabilities**: `kage proxy list`, `kage proxy health`,
  `GET /v1/admin/pool` (`provider`) and `kage config check` say what the
  provider does not offer; a proxy due for a replacement its provider
  cannot make stays in backoff with one `replacement_unsupported` event.
  The admin proxy view and `kage proxy list --json` show `locale`,
  `timezone`, `declared`, `carries_persona` and `plain_exit`.
- **The provider conformance suite**
  (`crates/kage-proxy/tests/provider_conformance.rs`) and the provider
  guide in `CONTRIBUTING.md`.
- `kage proxy health` ends with how many proxies are live and how many
  dead (`4 checked: 3 live, 1 dead`; `live` and `dead` in its JSON), then
  the pool's line on a line of its own (`pool: …`). `kage config check` reports the
  health endpoints in force, the list's plain exits, and the
  `proxies.health` keys unused under `egress.mode: direct`.
- A proxy whose probe sees an older proxy's exit address carries nothing
  until it leaves by its own (`proxy_exit_shared`).
- The probe reads Kage's own echo (`kage echo serve`, its `peer`) and an
  address written with its port, so the operator's echo can be
  `proxies.health.echo_url`.
- **The public face** (decision D52): the README rewritten for 2.0 (what
  Kage is, who it is for, responsible use for the levels, the quick start,
  the levels and the egress modes in plain words, the three ways to run
  the pool, what a result holds, what is measured and the honest limits);
  `SECURITY.md` (the private advisory form, the latest release line only,
  what is and is not in scope); `CODE_OF_CONDUCT.md` (the Contributor
  Covenant 2.1); issue forms for a bug, a feature and a proxy provider;
  a pull-request template that is the definition of done; `CODEOWNERS`
  naming the release's files and the public ones; `CONTRIBUTING.md` with
  every `just` target and the definition of done.
- **Badges from the repository's own data**: CI's `coverage-badge` job
  writes the Rust workspace's line coverage, as the gate counts it, to the
  `gh-pages` branch on every push to `main` (one file, one commit, with the
  job's own token and `contents: write` alone), which shields.io draws;
  `just lint-docs` holds the toolchain and licence badges to
  `rust-toolchain.toml` and `Cargo.toml`; `just test-scripts` runs the
  gate scripts' own tests (`docs/runbooks/repo.md`, "Badges").
- `deploy/compose.yaml` takes where requests leave from and a declared list
  from `deploy/.env`: `KAGE_EGRESS_MODE` (`proxies` or `direct`) and
  `KAGE_STATIC_LIST_FILE`, a file the `kage` service mounts read-only.
- `nightly-live.yaml` runs by hand the `suite` it is given (`load`, the
  default, needing no secret; `live`; `both`), so that a release's load
  test no longer starts the live job.
- `tests/live/host-persona.sh` (`KAGE_LIVE=1 KAGE_LIVE_HOST=1`, never in
  the nightly run): the host persona of each engine under
  `egress.mode: direct`, placed by the lookup of the machine's own address
  and judged on the two detection pages with the engines' baselines.

### Fixed

- The proxy pool wrote a proxy and the health event that records it in
  two transactions (a new proxy and `proxy_added`, a probe's state change
  and its `proxy_state_changed` and `proxy_geo_observed`, a retirement, a
  replacement requested or finished), so a reader in between saw the
  proxy or its new state without the event; a slow runner's CLI test read
  a `healthy` proxy and no `proxy_added` (F65). The row and its events are
  now one write.
- Under `egress.mode: direct` an operator's `kage proxy health` and the
  admin probe still probed the idle pool; both refuse now ("the pool is
  idle", F68).
- The hand found a link it had to scroll to while the page still glided
  from the wheel, and aimed where the link was, not where it came to rest:
  a page that glides for longer than the hand's pause after a flick (a
  slow or loaded host) moves the link from under the pointer, and after
  two tries the hand gives up and a crawl types the address it meant to
  click (seen once in four gate runs of M14, F66). After the wheel the
  hand now goes to an element only once the page rests, and a link it
  does not come to rest on is logged with the reason.
- A proxy that never answered a probe got a persona when its backoff
  ended, because `degraded` is usable: a generation spent and a profile
  made for an address nothing was ever seen through (F71). A persona is
  made only for a proxy that has answered at least once; the host's own
  rows, which are never probed, always count as answered.
- `kage selftest browser --language` refused a Firefox locale without a
  region with a line break and a run of spaces in the middle of its
  message (F75).
- The OpenAPI document had an attempt's `status`, `persona`, `engine`,
  `exit` and `reason` as optional, though every attempt carries them,
  null where they do not apply (level 0 has no persona, a persona's
  attempt no exit). They are required and nullable now, so a client
  generated from the document may rely on them (F69); only `seed`, which
  an admin key alone sees, may be absent.
- Under `egress.mode: direct`, `kage persona selftest` and `kage selftest
  browser --persona` refused every host persona: the command's own
  forwarder took the pool's routes, which carry no host row, so the
  browser's self-test could reach no page. A command's forwarder now takes
  the server's routes, and the direct end-to-end test runs the host
  persona's self-test through it.
- An empty `KAGE__…_FILE` variable was read as a file named by nothing and
  stopped the start; it now counts as not set, like an empty secret
  variable, which is how compose passes an optional one.

## [1.0.2] - 2026-10-19

### Fixed

- **Every Chromium persona of 1.0.0 and 1.0.1 showed a page three things a
  stock Chrome 155 does not**, all from the driver's default switches
  (patchright's, Playwright's `chromiumSwitches`), measured against a
  Chrome nobody configured: a bar over the page on every start ("You are
  using an unsupported command-line flag:
  `--disable-blink-features=AutomationControlled`"), which made the
  viewport 56 px shorter than the window's; third-party storage not
  partitioned (a cross-site frame read its site's first-party
  `localStorage` and IndexedDB); and no HTTPS upgrade (an `http` address of
  a public host went to http directly, where Chrome asks it on 443 first
  and falls back by itself). The engine now names every switch Chrome is
  started with and leaves the rest to Chrome (ADR 0014): it keeps 12 of the
  driver's 25 for the driver, the hand or the container, leaves 13 out
  (`ThirdPartyStoragePartitioning`, `HttpsUpgrades`, `PaintHolding`,
  `Translate` and the other disabled features, the background throttling
  switches, `--force-color-profile=srgb` among them), and adds
  `--test-type`, which suppresses the bar and changed nothing a page reads.
  Each persona moves at its next context open. The wire is unchanged (the
  capture against the echo equals the committed one).
- The Chromium parity test of a paced flow failed on a slow host (0.6 of a
  CPU, as on GitHub's runners): the unpaced click came after the paced
  floor because the page drew its first frames 0.9 to 1.3 s after `load`,
  and the hand waits for them. The input recorder notes those frames, and
  the click is held to the floor from there.

### Changed

- Known tells kept on purpose and written down
  (`docs/runbooks/engine-chromium.md`, security review T5 to T7): the
  built-in AI APIs answer `unavailable` (`OptimizationHints` stays off, so
  a page cannot make a persona download a model through its proxy), Google's
  properties see no variations seed and no `X-Client-Data`
  (`--disable-background-networking` stays), Cast discovery stays off, and
  WebGL from llvmpipe is reached past Chrome's own blocklist. A result of a
  navigation that Chrome upgraded and that fell back to http shows Chrome's
  internal `307` from the https address as the one hop of its chain.
- The live browser check measures Chromium's idle traffic in two windows
  of its own, with Kage's switches and without
  `--disable-background-networking`, each with the bytes per persona-hour
  and the hosts asked for (about 89 kB and 139 kB, measured on
  2026-10-19), and the Chromium baseline
  (`tests/fixtures/selftest/chromium-155-live.json`) is recorded anew with
  the new switches: 100 of 100, every check as on 2026-10-09, CreepJS's
  "like headless" 38 % (44 % before).

## [1.0.1] - 2026-10-19

### Changed

- The Firefox engine runs Camoufox **156.0.1-beta.39** (Firefox 156.0.1),
  pinned by URL and checksum with its `omni.ja` by a second checksum, in
  place of 152.0.4-beta.30 and its Juggler backport (ADR 0011, which
  supersedes ADR 0010). It removes four things a page could read of every
  1.0.0 Firefox persona and of no stock Firefox: a page's
  `new Worker("chrome://juggler/…")` worked, a cross-site frame read the
  persona's first-party cookies and storage, `window.open()` without
  activation opened a window, and pages had 15 extra `set…` functions.
  Reading a page still grants it no user activation, now by the build
  itself. `engines.firefox.browser_major` is 156; Firefox 156 is
  browser-only on the http path (JA4
  `t13d1517h2_8daaf6152771_3cbfd9057e0d`, as a stock Firefox 156.0.1), and
  its capture replaces the 152 one. The Firefox image grows by about
  1.2 GB (the build bundles 2.1 GB of fonts).
- The launch plan speaks the persona's language at startup: its Firefox
  UI locale (`intl.locale.requested`, a table per country checked against
  the build's language packs) and the plan's other preferences reach the
  build through `CAMOU_PREFS` before the first page, so that `Intl` and the
  browser's own messages are the persona's (on 152 the messages were
  English). The engine leaves out of the configuration the keys the build
  declares `removed` and never applies `navigator.doNotTrack` or
  `navigator.globalPrivacyControl`, which the generator draws for nearly
  every device and 156 would have shown without the `Sec-GPC` header.
  WebGPU is on for Windows claims and for macOS claims with an Apple
  graphics card, and the memory cache and compression dictionaries are on,
  as in stock Firefox 156.

### Added

- `POST /v1/personas/upgrade` (sidecar contract 1.4.0, additive): a stored
  persona moved to the browser major its engine runs, the same bytes for
  the same document. At a persona's first open under a newer major the
  manager has it moved, stores it with its new `browser_major`,
  `user_agent` and `wreq_profile`, and records `persona_browser_upgraded`
  with what the persona was before; its sessions, cookies, http jar and
  per-domain state continue. Personas of 1.0.0 move to Firefox 156 this
  way, at their next open.
- Before a profile's first start under a newer Firefox major the engine
  copies it to `.upgrade/<persona id>/` on the personas volume; kage
  deletes a copy after `personas.upgrade_backup_days` (7). The rollback
  within that window is `docs/runbooks/upgrading-an-engine.md`, "Going
  back", and a test runs its database statement.
- Probe checks, hard for both engines: `coherence.intl_language` (the
  language of `Intl` against `navigator.language`),
  `headers.navigation.gpc` and `headers.navigation.dnt` (what a script
  reads against what the navigation sent), `automation.driver_worker` (a
  `chrome://juggler` worker) and `automation.extra_globals` (a `set…`
  function the browser does not have). The parity suite states what a
  page reads of a persona's document, of a moved persona, of third-party
  storage and of an unrequested popup, per engine. On Chromium the storage
  statement records a tell it found: a cross-site frame reads its site's
  first-party `localStorage`, because patchright's default switches turn
  third-party storage partitioning off (security review T2). Chrome is
  unchanged here; the audit of its launch switches is the next milestone.
- `scripts/audit-camoufox-prefs.mjs`: the preference audit of a Camoufox
  build against a stock Firefox of the same version, for the next bump;
  its table for 156 is `docs/evidence/camoufox-156-prefs-audit.tsv`.

### Removed

- `scripts/patch-camoufox-juggler.mjs`, its two checksums and the
  startup-cache clearing of the engine (ADR 0010). The `omni.sha256` notes
  in existing profiles are no longer read.

## [1.0.0] - 2026-10-17

The first release. Kage fetches web pages through a pool of static
residential proxies, one long-lived persona per address, in a real
browser (Chrome patched by patchright, or Camoufox, each in a hardened
sidecar) or on the http path with a browser's own TLS and HTTP/2
fingerprint; one page at a time or as batches, scripted flows and crawls,
with extraction, `robots.txt` per site, a response cache, webhooks,
sessions and an admin API, and with metrics, traces and an optional
observability stack. What follows is the record of the milestones M0 to
M12 that made it, newest first within each kind of change.

### Added

- The load test of the release (`tests/e2e/load.sh`, `tests/load/batch.js`):
  200 queued fetches, one batch job of three visitors, of the fixture site,
  driven by k6 from its pinned image with its usage report off. Under
  `fast` (design A, in `just test-e2e`) it must end in 45 to 240 s;
  under the test-only `load` profile, `human`'s shape at a fifth of its
  times at the shipped caps (design B, `just test-load`, before a release
  and in the nightly), in 540 to 720 s with the cap of 20 starts a minute
  reached and never exceeded. Both: every item `ok` with 200 at its first
  attempt in a browser and not from the cache, every page asked for once,
  by at most three addresses and without a forwarding header, no persona
  on two pages at once, the job never `paused`; B also at least 100 ms
  from the end of a page to the persona's next.
- M12, the follow-ups carried into the release:
  - `kage fetch` takes `--extract`, `--cache-ttl <duration>` and
    `--robots policy|report|enforce|ignore` and shows what they return:
    what the site's `robots.txt` says of the address, whether the answer
    came from the response cache and when its page was fetched, and the
    title, the words and the links of what was extracted (`robots`,
    `cached`, `cached_at`, `cache_ttl_s` and `extract` with `--json`).
    A wish the site's policy does not allow is refused before anything
    starts, as the API refuses it. The command line's cache entries are
    its own, and a fetch that may fill the cache keeps its document, so
    that `--output` writes the stored page for an answer from the cache
    (F50).
  - A test holds the bound on how much of a `robots.txt` is ever read
    (`MAX_ROBOTS_BYTES`, 4 MiB) to its value: the one mutant that
    survived on M11's lines of `kage-core` (F50).
  - A stored result keeps what its answer said of the visit besides the
    page: `pacing`, `behavior`, `console` and `side_effects` are the same
    in the answer of `POST /v1/fetch`, in `GET /v1/results/{id}` and in
    an item's result of a job, where they were null before (F36;
    migration 0016, `results.visit`). An answer from the response cache
    shows what the persona did on the page, the browser's log and what a
    capture fired as the visit that got the page had them, and its own
    pacing with `waited_ms: 0`. A result keeps at most the first 1000
    entries of a browser's log, each text cut at 4096 bytes.
  - A page of a crawl that changed hands between its stored result and
    its item keeps its links (F49). What the crawl takes from a page
    (where it ended, its links with whether a reader can see each, its
    text) is stored with the result (`results.reading`, migration 0016)
    and forgotten once the crawl has recorded the page; a worker that
    takes the job up reads on along those links instead of recording the
    page as read without any. The rule that turns a page into what the
    crawl takes moved into the pipeline, so that a page read now and one
    taken up later are read alike.
  - A server that stops finishes the pages that are being read (F39).
    Until now a fetch that was carried on as a job ran in a task nobody
    waited for, the job worker waited 20 s for pages that take one to two
    minutes, and Docker killed the container after 10 s: the attempt was
    cut without a record and the page fetched again after the restart,
    so the site saw it twice. Now the scheduler grants nothing more from
    the moment of the signal, the server waits for every attempt that is
    under way for at most `jobs.wind_down` (new, 150 s, at most 15 m),
    and what outlasts the wait is cut and written down as an attempt
    with the reason `interrupted`, which cools nobody and counts toward
    the request's retries. `kage` has a `stop_grace_period` of 200 s in
    `deploy/compose.yaml`. A request that waited for a persona when the
    signal came is answered `shutting_down` at once instead of waiting
    out the grace.
  - The `link` and `search_engine` entries that load a page to start
    from click as the entry from the open page does (F48). Their result
    said `ttfb_ms` 0 and counted the hand's way to the link into the
    times; a link over two lines was missed in Chrome (the hand aimed at
    the rectangle around both lines); a link under something else was
    clicked through it in Firefox; a link for another tab was clicked; a
    click answered by a page that draws itself waited 15 s; and a file
    the browser saved was `arrived` with the start page's document under
    the file's address. Now the times count from the click, only a link
    a reader can see that leads on in this tab is clicked, on the largest
    of its boxes and only when the pointer rests on it, a page that
    answers itself ends the wait after 1.5 s, and a file is no page
    (`aborted`). A client-routed site still costs a click and a typed
    load per page: documented in spec §10.4, not solved.
  - Traces (spec §20): every request is a trace, `api.request` →
    `pipeline.fetch` → `scheduler.wait` → `attempt` → `sidecar.navigate` or
    `sidecar.flow`, and every page of a job one from `job.item`; webhook
    attempts are `webhook.deliver`. With `telemetry.otlp_endpoint` set the
    spans go to that collector as OTLP over HTTP with protobuf, through
    wreq and never a proxy; without it nothing leaves the process. Every
    JSON log line written inside a span carries `trace_id` and `span_id`,
    and a request to a sidecar its `traceparent`. No span holds a URL's
    path or query, an address, a seed, a token or a fingerprint.
    `deny.toml` bans `reqwest`, which `opentelemetry-otlp` brings with its
    default features.
  - The sidecars' half of the traces: a navigation and a flow are spans of
    their own, the children of the span Kage names in the `traceparent` of
    its request, sent as OTLP over HTTP with protobuf when
    `SIDECAR_OTLP_ENDPOINT` is set (`SIDECAR_SERVICE_NAME`, default
    `kage-sidecar-<engine>`). The five `@opentelemetry/*` packages are
    loaded only then, and patch nothing. Every line of a request of Kage's
    carries its `trace_id`, and a line written during a navigation or a
    flow its `span_id`. `@opentelemetry/sdk-node`, planned since M0, is not
    used.
  - The `observability` profile (`deploy/compose.observability.yaml`): an
    OpenTelemetry Collector, Jaeger, Prometheus and Grafana at pinned
    digests, hardened like the rest of the stack, with Grafana alone
    published, on loopback. Seven dashboards (pool and personas, pacing
    and visits, blocks and breakers, jobs and crawls, webhooks, bandwidth
    against the plan's cap, cache and robots.txt), written by
    `scripts/grafana-dashboards.mjs`; seven alert rules (Kage down, no
    usable proxy, 80 % of the bandwidth budget, a breaker open, a domain's
    blocks at its policy's threshold, a persona retired, a webhook failing
    for good) with `promtool` tests. `just lint` checks the dashboards and
    the rules; `tests/e2e/observability.sh` reads one trace from the API
    to the browser back from Jaeger and greps every log line and span for
    the stack's secrets. Runbooks: `observability.md`, `block-report.md`,
    `field-campaign.md` (the owner's M13 campaign).
  - Two identical requests under way at the same moment are one visit
    (F52): of two fetches or batch items with one cache key in one
    process, the second waits for the first and is answered from the
    cache with what it stored; when the first kept nothing, the second
    makes its own visit. `kage_cache_requests_total{result="joined"}`
    counts the waits. The same crawl asked for again inside the lifetime
    still visits every page again.
  - A site's scout has an end to its rule and the operator a way to give
    a site another one (F51). A persona that read a site is kept from
    scouting it for `robots.scout_quarantine` (new, 30 days, 1 to 3650
    days), no longer for good, so a site whose every persona had read it
    once gets a scout again. Among the site's own personas the scout is
    first one whose engine keeps another reader of the site, so the
    preference for a Firefox no longer takes a site's only Firefox while
    a Chrome can ask. `POST /v1/admin/sites/{site}/scout` gives a site
    whose scout is not ready (and which, enforcing its file, is closed
    until the scout is back) another one, chosen by the same rule and
    never the one it had; the audit trail names the site, the new scout
    and the one before.
  - The pin of `hickory-resolver`, which no crate ever used, is gone
    (F37), and `just deny` now fails on a pin in
    `[workspace.dependencies]` that no crate of the workspace uses
    (`scripts/check-workspace-pins.mjs`; what is declared on purpose is
    listed there with its reason). The workspace entry of `html5ever`,
    which no crate of ours names, is gone too: it comes through `scraper`
    at 0.39.0, held by `Cargo.lock`.

- M11, extraction, `robots.txt` and the response cache:
  - `include: ["extract"]` returns what a page says: `title`,
    `description`, `canonical`, `lang`, `published_at`, `modified_at`,
    `meta`, `opengraph`, `twitter`, `jsonld`, `links`, `headings`, and
    its main content as `text` and `markdown` with `word_count` and
    `thin_content`. It is read from the document the visit captured,
    never by a second request, in a fetch, a batch's items, a flow's
    captures and a crawl's pages. `extract: { markdown, links }` leaves a
    part out.
  - The main content is Readability's rules (Firefox's reader view)
    ported onto the tree a crawl reads its links from, so a page is
    parsed once, with three rules of Kage's own (furniture is never
    content and no candidate means no content; bylines are metadata;
    siblings of the same shape are content). Mean F1 against text marked
    by hand: 97.0 on ten written pages of ten kinds, 97.7 on three more
    that were never tuned on; the furniture rule alone reaches 76.2, the
    best crate measured (`dom_smoothie`) 89.4. The markdown is written by
    the same walk as the text: fenced code with its language, GFM tables,
    absolute links, nested lists. Golden files per fixture page,
    regenerated by `just goldens`.
  - `robots.txt`. A reader never asks for the file, so asking marks the
    address that does: a site's file is asked for only where its policy
    says `report` or `enforce`, by the site's scout (one persona per
    site, which reads none of its pages and also asks for its sitemaps),
    once a day at most. Every result carries
    `robots: { fetched, allowed, matched_rule, crawl_delay }`. Under
    `enforce` what the file disallows is `robots_denied` and never asked
    for, a file that cannot be had closes the origin until it is asked
    for again, and `Crawl-delay` is a floor under each persona's gap on
    the host. A crawl never makes a disallowed address a page
    (`skipped.robots_denied`) and takes its sitemaps from the file.
    Parser and matcher are Kage's own, by Google's rules.
  - The response cache. `cache_ttl_s` (or the policy's `cache.ttl`): a
    request whose key holds a result that is young enough is answered
    with it, with `cached: true` and `cached_at`, without a visit and
    without a page of the daily budget. The key is everything that
    changes what a result holds, the include set and the API key among
    it; a policy may share a site's pages between keys (`cache.shared`).
    A crawl fills the cache and never reads it; a flow and a session stay
    out of it.
  - Configuration: `extract.thin_words`, `robots.ttl`,
    `robots.error_ttl`, `robots.max_bytes`, `robots.agents`,
    `cache.max_ttl`; the policy's `cache` section. Error codes
    `robots_enforced`, `robots_ignored`, `cache_ttl_too_long`. Metrics
    `kage_extract_duration_seconds`, `kage_robots_requests_total`,
    `kage_robots_denied_total`, `kage_cache_requests_total`. Migration
    0015.
  - The API test client reads what a result gained (`extractOf`,
    `cachedAt`, `robotsDenial`); `tests/e2e/extract.sh` runs all of the
    above by real personas of both engines and checks it against the
    fixture site's own request log: the essay asked for once per fetch
    and not at all while the cache answered, `robots.txt` asked for once
    by an address that read no page, the page it disallows never asked
    for, the sitemap it names read in place of the guess.

- M10, the crawler:
  - `POST /v1/jobs` takes `type: "crawl"` (the `crawl` scope besides
    `jobs`): `seeds`, `allow` and `deny` patterns on the normalized
    address, `max_depth`, `max_pages`, `same_site_only`, `use_sitemaps`,
    `defaults` for every page, `visitors`, `session_id` and `webhook`. A
    crawl is accepted whole or not at all, with `Idempotency-Key`, cancel,
    pause and deadline as a batch has them. It has no items when it is
    accepted: one is added for every page it asks for, with the page's
    depth, where its link was found, how it was reached and what it
    repeats, and the job shows how far the crawl has come and why it
    stopped reading.
  - A crawl reads like a reader, not breadth-first: the next page is a
    link in the text of the page that is open, else in the text of a page
    up to three entries back in the tab (a reader goes back to the
    listing for the next article), else a link of a menu within the same
    reach, and an address is typed only when the tab leads nowhere new. In a browser the persona clicks
    its way (sidecar contract 1.3.0: an entry by a link of a page it has
    been reading, with the keyboard's way back; F28); on the http path
    the page that holds the link is the `Referer`. Measured with the
    frontier itself (`cargo run -p kage-crawl --example reader_order`):
    breadth-first types 96 % of the addresses of a shop and 79 % of a
    blog's, this order 8 % and one address of 344; on the fixture's
    listing of ten articles it types 1 of 18 where taking the open page's
    menu first typed 11.
  - One persona reads one site (its registrable domain) through the
    scheduler like every other fetch: pacing, visits, waking hours,
    breaker and cooldowns are the same. `visitors` asks for more, up to
    the pacing's `domain_concurrency`.
  - Links no reader can see are never followed (`capture.links` of
    contract 1.3.0 says of every link whether it is rendered, has a box,
    is on the page and is not transparent; on the http path what the
    markup itself hides) and are counted as `hidden`.
  - Duplicates: one entry per normalized URL, the end of a redirect
    included; a page with the content hash of an earlier one is flagged
    and its links are not followed from there; a page whose 64-bit SimHash
    is at most 6 bits from an earlier one's is flagged as a near-duplicate
    and read on from. What is compared is the text without what the site
    repeats on every page.
  - Sitemaps are off by default: no reader asks for `/sitemap.xml`. With
    `use_sitemaps` a scout reads them first: a persona that is not the
    site's reader and never becomes it, a Firefox persona when one is free
    (Chrome saves a gzipped sitemap as a file). Where nobody but the
    reader could ask, nobody asks.
  - `kage-crawl`: the frontier (the reader's order, depth by the shortest
    way with lifting, the page cap, duplicates), the page reader (links
    with whether the markup hides them and whether they stand in the
    site's furniture, the main text), SimHash, sitemaps (index, gzip,
    `lastmod`), and the scope of a crawl. URL normalization in `kage-core`
    is property-tested, with twelve more tracking parameters.
  - A crawl section in the configuration (`crawl`), the frontier in
    `crawl_frontier` (migration 0014), a page claimed for its item with
    `SELECT ... FOR UPDATE SKIP LOCKED`, and a crawl that changes hands
    goes on where it stands. Metrics `kage_crawl_pages_total{reached}`,
    `kage_crawl_skipped_total{reason}` and
    `kage_crawl_duplicates_total{kind}`.
  - The TypeScript test client drives a crawl (`createCrawl`,
    `crawlProgress`, `readCrawlPages`); `tests/e2e/crawl.sh` reads the
    fixture site with real personas of both engines.
- A persona rests between leaving a page and asking for the next one of
  the same sitting (F43): `rest_ms` of a pacing profile (`human`: floor
  700 ms, median 2.2 s, p95 7 s; `fast`: none), applied by the planner.
  Before, a page that was read for longer than the gap was followed by the
  next one 1 to 3 ms after it was left, in both browsers.
- A fetch's `behavior` says the patience the persona had with the page
  (`patience_ms`), how far down it came (`scroll_reached`) and what ended
  its scrolling (`scroll_ended_by`: `depth`, `patience` or `page`); the
  patience is kept with the attempt (`attempts.patience_ms`).

- M9, jobs and webhooks:
  - `POST /v1/jobs` with `batch` and `flow` jobs, accepted whole or not at
    all, with `Idempotency-Key` for a day; `GET /v1/jobs/{id}`,
    `GET /v1/jobs/{id}/items` (paginated, each item with its result) and
    `POST /v1/jobs/{id}/cancel` (cooperative: the page under way is
    finished, nothing more is begun). A worker claims jobs with
    `SELECT ... FOR UPDATE SKIP LOCKED` under a lease, and a job whose
    worker stopped is taken up again where its items stand.
  - A batch has one visitor per site: one persona reads the site's pages
    in order and another takes over only when it cannot go on. `visitors`
    asks for more, up to the pacing's `domain_concurrency`. An item that
    cannot be attempted now waits instead of failing, and a job whose
    every remaining item waits is `paused` until the time the scheduler
    named, for at most `jobs.max_age`.
  - A flow is one browser attempt of one persona, always paced like a
    person (sidecar contract 1.2.0: a dwell on every page it arrives on, a
    thinking time before every action). Its steps are items, each capture
    a result of its own. Every `goto` passes the SSRF guard at acceptance
    and everything a flow visited is checked afterwards. `human: false`
    steps need the site policy's new `driver_input`.
  - Webhooks: `job.completed`, `job.failed` and `job.progress` as thin
    events signed with HMAC-SHA-256 over `<timestamp>.<body>`
    (`X-Kage-Signature`), to receivers the operator listed
    (`webhooks.allowed_origins`, none by default; `webhooks.private_origins`
    for those on the operator's own network), tried again five times on a
    jittered schedule, with a delivery log on the job.
  - A synchronous fetch that outlasts its wall time is carried on as a job
    of one item under its own request id (F35): the `202` has a `job_id`,
    and the result is reachable after a restart of the server.
  - `POST /v1/admin/purge` deletes what a key fetched before its retention
    ends, on the audit log (F33); finished jobs are deleted after
    `jobs.retention_days`.
  - Metrics for jobs, their items and webhook deliveries (spec §20).
  - `tests/e2e/jobs.sh`: a batch of 50 pages, a flow in each engine, a
    cancel, the webhook's signature at a receiver, a fetch carried across
    a restart, and the purge, on the compose stack; the TypeScript test
    client drives the jobs.

- M8, the public API v1 and the admin API:
  - `POST /v1/fetch` over the pipeline of M7: one page fetched while the
    caller waits, answered `200` with its result whatever the site did (a
    block is the result's `outcome`), and `202` with the request's id when
    it outlasts `scheduler.sync_wall_time`; `GET /v1/results/{id}` returns
    a result again and `GET /v1/artifacts/{id}` streams what was kept of a
    page (the document, the raw body, a screenshot, a PDF, the bodies of
    captured XHR responses).
  - OpenAPI 3.1 generated from the Rust types, served at `/openapi.json`,
    committed as `crates/kage-api/openapi.json`, printed by `kage openapi`,
    with Swagger UI at `/docs` for the admin scope. Errors are RFC 9457
    problem documents with a stable `code`; every answer has
    `X-Request-Id`.
  - API keys `kage_<keyid>_<secret>`: 32 random bytes of which only the
    SHA-256 is stored, five independent scopes, and per key requests per
    minute, requests in flight and a daily page budget, beside a limit of
    requests per minute for the whole API. `kage key create | list | revoke
    | rotate` and the same through `/v1/admin/keys`. Every admin call and
    every change at the command line is in `audit_events`.
  - What a request may ask of a persona's behaviour: `behavior.scroll_depth`
    (0 is no scrolling), `behavior.dwell_ms` (the median of what the
    persona draws from, never below the policy's new `min_dwell`) and
    `behavior.referer_strategy`; a policy's new `behavior_locked` ignores
    them. A `timeout_ms` too short for a visit is refused, not clamped.
    The result's `behavior` and `pacing` say what applied.
  - `request_kind: fetch | xhr` on the http path (follow-up F6): a request
    sent as a page's script sends one, with the header order, `Sec-Fetch-*`
    values, `Accept` and `Priority` Chrome 155 and Firefox 152 were
    captured sending, the page that makes it as `Referer`, and an `Origin`
    where a browser sends one. What a browser would preflight is refused.
  - Sessions through the API: opened by `POST /v1/sessions` and by nothing
    else, pinned to one persona that hints and a domain's policy choose,
    never moved; a fetch whose constraints exclude its session's persona
    is `409 session_persona_conflict` and nothing is fetched.
  - The admin endpoints: personas (list, show, retire, regenerate), proxies
    (list, probe, replace), the pool with the bandwidth budget, a policy
    reload, the audit trail. A persona's self-tests run in the server,
    through the scheduler's gate and the forwarder, in the browser context
    real work has open (follow-up F29).
  - What a consumer sees: a persona as a handle of the key's own, never a
    persona's id, a proxy, an address or a seed; the cookies a site set
    without their values (`security.expose_set_cookie` returns them).
  - The SSRF guard on every target, with `security.resolve_on_host` for a
    lookup on the Kage host (off by default: the proxy resolves), and a
    guard for the webhook URLs of M9.
  - A listener with the limits `axum::serve` has no switch for: the size
    and count of headers, a deadline for the head of a request (slowloris),
    a deadline for its body, a connection cap, a graceful stop; body and
    request-target limits; CORS off unless origins are named.
  - `/metrics`: the Prometheus exporter over the series the scheduler and
    the pool recorded since M7, the API's own, the personas by state, the
    latest self-test scores, and both sidecars' series merged in; behind
    the admin scope, or on `server.metrics_bind` without a key.
    `/readyz` checks the database, a ready persona and a sidecar.
  - `tests/api-client`: the document checked by an outside validator and a
    TypeScript client generated from it, which the new compose end-to-end
    test (`tests/e2e/api.sh`) fetches a page with through both engines.
  - `docs/runbooks/api.md`.
- M7, the scheduler, pacing, block handling and escalation:
  - A new crate, `kage-scheduler`, over rules in `kage-core` that take
    time as an argument (`kage_core::schedule`) and are mutation-tested.
    A request waits for a persona and its slot: synchronous work before
    jobs, then first come first served, under the caps
    `scheduler.max_browser_sessions` and `scheduler.max_http_inflight`
    and the domain's own concurrency and requests per minute.
  - Pace: a persona that fetched a page from a domain keeps that domain's
    work for its visit, in bursts and breaks at its own factor, with a long
    break after a few bursts (`visit_bursts`, `visit_break_ms` in the
    pacing profiles); the daily cap turns at the persona's own midnight;
    each persona has a day of its own, and `active_hours: enforce | prefer
    | ignore` per domain says what it means; no two personas start in the
    same moment.
  - Persona selection by the score of spec §8.2 over the personas that may
    serve, away from the network of a persona that was just blocked.
  - Retries on another persona after a drawn pause; cooldowns from the
    policy scaled by the confidence of the signature that matched, with a
    floor and a jitter; a pause for a domain after server errors in a row;
    the circuit breaker in `domain_breakers` with one trial after its
    pause and a doubling pause; `domain_paused` as an alert.
  - Under `render: auto` a block on the http path moves the request to
    the browser exactly once: the same persona's for a block a browser
    cures, another persona's after a refusal.
  - One attempt on either path (`PathExecutor`): the http path with the
    persona's client, cookies and the previous page as `Referer`; the
    browser path with a human dwell, a scroll depth from the attempt's
    seed and a wandering pointer, cookies handed over before and after,
    the redirect chain checked against the SSRF guard, a detected
    interstitial given `browser.challenge_wait_ms` to resolve, and the
    browser closed after a block. Every attempt stores its seed.
  - The forwarder (follow-up F10): the one proxy every persona's browser
    and http client is given. The provider's credentials stay in the kage
    process, every byte is counted per persona and domain
    (`bandwidth_usage`), and it relays nothing the SSRF guard refuses,
    nothing for a persona that may not send, and nothing new while a leak
    is on record or the budget is used up.
  - The bandwidth budget (decision D5): `bandwidth.monthly_cap_gb` (250)
    per billing term, judged on the larger of Kage's count and the
    provider's; `bandwidth_budget_warning` at 80 % with the projected
    day, `bandwidth_budget_exhausted` and a hard stop at the cap.
  - One owner of persona contexts (follow-up F27): a persona's browser
    stays open through its visit and `scheduler.context_idle` after it,
    and warm-up visits and self-tests ask the scheduler first.
  - `kage fetch <url>`: one request through the whole pipeline, in
    sentences or as JSON. `kage serve` runs the scheduler, the forwarder
    and the budget; `kage config check` reports them.
  - Metrics of the scheduler and of attempts (spec §20), recorded through
    the `metrics` facade.
  - A compose end-to-end test of the fetch pipeline with real browsers on
    a world of its own (`tests/e2e/fetch.sh`, `deploy/compose.e2e.yaml`).

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

### Security

- Kage takes a file a sidecar wrote only as the sidecar describes it
  (security review S1, `docs/security-review.md`). Until now it read
  whatever path below the artifacts directory a sidecar named, following
  links, and ignored the size and the SHA-256 the sidecar sent with it: a
  sidecar that was taken over could name a link to Kage's own
  `/proc/self/environ` or a secret file, and Kage stored that file as the
  request's screenshot. Now the file must lie below `sidecar/` with a real
  path that stays there, be a plain file (opened without following a link
  and without waiting on a pipe), hold at most 128 MiB, and have the size
  and the SHA-256 named; anything else is not taken. `libc` is a direct
  dependency of `kage-scheduler` on Unix for the two flags.
- The SSRF guard judges an IPv4-translated IPv6 address
  (`::ffff:0:a.b.c.d`, RFC 2765) by the IPv4 address inside it, as it
  already judged the IPv4-mapped, IPv4-compatible, NAT64 and 6to4 forms:
  `http://[::ffff:0:7f00:1]/` is loopback (security review G6).
- `webshare.base_url` and `reputation.base_url`, the roots the provider's
  and AbuseIPDB's keys are sent to, must use `https`; plain `http` is
  accepted only to `localhost` and the loopback addresses.
- Kage reads at most 128 MiB of a sidecar's answer (`MAX_ANSWER_BYTES`):
  one that says it is longer is refused unread, one that does not say is
  cut off at the limit, and the call fails as an answer the contract does
  not describe. Until now an answer was read whole, so a sidecar that was
  taken over could fill Kage's memory.
- `kage persona selftest` and `kage selftest http|browser --persona`
  bind a forwarder of their own, as `kage fetch` does (security review
  G4). They handed the persona's browser the provider's proxy with its
  password, which then sat in the sidecar for the run. The stand-in
  self-tests of a proxy (`--proxy`), which have no persona the forwarder
  could name, remain the one exception.
- The sidecars are on an internal network of their own, `browsers`, with
  kage alone (security review L4). They were on the stack's default
  network, where a browser that was taken over could reach Postgres and,
  past the forwarder, the internet. Docker gives an internal network no
  gateway: a browser's only way out is kage's forwarder. `just hardening`
  refuses a sidecar on a network with a way out, the smoke test checks
  the running stack, the observability profile puts its collector on both
  networks, and `deploy/compose.dev.yaml` (kage on the host) keeps the
  sidecars on the default network too.

### Fixed

- On a slow machine a persona scrolls pages again. A browser hands a
  page its pointer moves and its wheel with the frames it draws, and a
  new document draws none for a while when the software renderer
  compiles its shaders: a quarter of a second on a workstation, more
  than half a second on a four-core cloud runner. The hand turned the
  wheel in that time, saw the page not move, and took it for a page that
  does not scroll: every page ended after its first flick with nothing
  read, and the page saw a pointer path of four moves. Now the hand acts
  on a page once it has drawn two frames (waited for at most 2 s, from
  the reader's world), and a flick that moves nothing is turned once
  more after the page draws before the page counts as one that does not
  go further. Found by the engines' parity suites on GitHub's runners;
  reproduced here with the browser's CPUs held to 0.6 of one. For the
  same reason a Chrome screenshot taken right after a load could fail
  with "Unable to capture screenshot": it now asks again for up to 2 s
  while the page has no frame to give.
- A batch or a crawl with several visitors no longer stands still for
  the long break of one of them while another reads (F54). The planner
  told a page that would rather be refused than wait five minutes to
  come back when the resting visitor did, as if the visitors in the
  middle of a page were not there, and the job worker then left the
  whole site alone until that time. Now a visitor that is reading counts
  from its slot, one that an earlier page waits for counts too, and a
  page is refused only when nobody it may have could start in time; a
  site left alone for `no_persona` is asked again when one of its pages
  ends. In the simulation of the load test, 200 pages under `human` with
  three visitors took a median of 146 minutes and take 61, as long as
  when every page may wait as long as it must; 50 pages took up to 79
  minutes (p95) and take 14. Outside batches the same rule holds: a
  request that would rather be refused waits for a persona that is in
  the middle of a page.
- The sidecar images keep no setuid or setgid file (Chrome's
  `chrome-sandbox`, the base image's `su`, `passwd`, `mount` and the
  rest). `no-new-privileges` already kept them from raising anything;
  they are no longer there. `just hardening` checks the Dockerfiles and
  every compose file against the container checklist, `just
  hardening-images` the built images, both in `just ci`.
- The sidecars' process metrics (`process_*`, `nodejs_*`) carry their
  `engine`: Kage merges both sidecars' metrics into one exposition, where
  each of these series was there twice, once from each sidecar, with
  nothing to tell them apart.
- RUSTSEC-2026-0367, -0368 and -0369 against `zip` 8.6.0 are ignored in
  `deny.toml` and `.cargo/audit.toml` until 2026-12-31 or the first
  `utoipa-swagger-ui` on `zip` 9: `zip` only unpacks the vendored Swagger
  UI bundle, pinned by checksum, when Kage is compiled, and the advisories
  are about extracting an attacker's archive.
- `rustls` 0.23.46 in place of 0.23.45 (RUSTSEC-2026-0346 to -0349,
  published 2026-10-14): the TLS of the fingerprint echo and of the
  database connection. Outbound requests to targets are wreq's and never
  used it.
- A job with `webhook.progress_every` was accepted and then failed in the
  worker with `validation_failed` when it was a flow, because the worker
  read the stored job back as a request and the stored job carries what
  the request did not (F46). The same test showed that a crawl never
  reported its progress at all: `job.progress` was queued only while
  other items of the job were open, and a crawl's list grows one page at
  a time. One function is now the only way a worker reads a stored job
  back, and a crawl reports like a batch.
- A file a browser saved instead of showing it (a gzipped sitemap in
  Chrome) was judged as a page with nothing on it: a soft block, which
  cooled the persona that asked for it down for the site and counted on
  the site's breaker. A navigation that was aborted and left neither a
  document nor a body is no page, and turns nobody away. Until M11 a
  second persona then asked for the next sitemap; a site has one scout
  now.
- `just mutants` takes the number of jobs (`just mutants 24`; one
  mutant at a time unless said, with what a parallel run needs written
  into the recipe), and the boundary of `crawl.skipped_extensions` that
  a mutant survived has its test (F47).

- A human `back` step of a flow (the keyboard's `Alt` and left arrow)
  reported success when the tab had no page to go back to, after watching
  five seconds of nothing, where the driver's `back` failed after one; and
  it did not see that it had left the page when the entry before had the
  address of the page it left and the browser had kept that document
  (measured on Chrome 155: a page that takes a campaign tag out of its own
  address). Both ways back now go through one watch that asks the page
  itself which document it shows: keys that led nowhere fail the step with
  "there is no page to go back to" within about a second, and a page that
  came back is arrived on, whatever its address (F38).
- A human `scroll` step of a flow on a page too long for the step's time
  scrolled until that time, finished the flick and the rest it was in, and
  was then failed by the same time ("scroll did not finish within 30000
  ms"), found by the live check of jobs on bot.sannysoft.com. It now stops
  two seconds before its step's time and sits out no rest past it: the
  page is scrolled as far as the time allows and the step succeeds. Give
  the step a longer `timeout_ms` to scroll further.
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

- The YAML of `kage.yaml`, the policy files and the static proxy list is
  read by `serde-saphyr` 1.3.0 instead of `serde_yaml` over
  `unsafe-libyaml` (F45; RUSTSEC-2024-0449, which `deny.toml` no longer
  ignores). A file that loaded before can fail now, and the message says
  why: a key said twice in one mapping is an error with its line and
  column (the last one won before, silently); a merge key `<<` is an error;
  unquoted text that YAML reads as a number (`0123456`, `007`, `1_000`) is
  refused where text is expected, and `kage config check` names the key and
  says to write it in quotes; `1e400` and `.inf` are refused. A byte order
  mark is accepted now. A document nests at most 128 levels and holds at
  most a million nodes and 16 MiB of text, and a hostile one is refused in
  milliseconds. On a hot reload an invalid file keeps the last good policy
  and the alert gives the line. The shipped files read exactly as before.
- The shipped default of a domain's `robots` is `ignore` (it was
  `report`, which nothing acted on before M11). A reader never asks for
  `/robots.txt`, so with `report` as the default every site Kage is ever
  pointed at would have been asked for it. Set `robots: report` or
  `robots: enforce` for the sites whose verdict is worth one marked
  address; `defaults: { robots: report }` restores the old meaning for
  all of them.
- `cache_ttl_s` of a fetch request is optional (it was a number that had
  to be 0): left out, the site's policy decides; 0 is "never from the
  cache". `extract` of a fetch request is `{ markdown, links }` (it was
  free-form and refused). The error code `not_available_yet` is gone:
  nothing answers it any more.
- A crawl's sitemaps are asked for by the site's scout, which is
  remembered across crawls (`site_scouts`) and kept out of every fetch
  of the site, not by a persona chosen for each crawl. A scout the site
  turned away is not waited for and not replaced: the files it could not
  ask for are `no_scout`.
- A key's daily page budget counts the requests that were fetched: one
  the response cache answered is no page.

- `timeout_ms` of a browser attempt is the time to reach the page (the
  navigation, its waits, the challenge wait) and bounds nothing after it
  (F44). How long a persona goes on scrolling is a patience Kage draws
  anew for every page from the attempt's seed (`browser.patience`: floor
  6 s, median 38 s, p95 110 s, redrawn above 4 min, at the persona's pace)
  and sends with every navigation it makes: a fetch, an item of a batch, a
  page of a crawl, a warm-up visit (sidecar contract 1.3.0,
  `behavior.patience_ms`). The dwell follows in full, and the attempt is
  waited for as long as the three and a margin add up to. Before, the
  scrolling shared `timeout_ms`, and every page longer than that was left
  at the same instant: 56.0 s after it was asked for, six times of six, in
  both browsers. A consumer that needs the bottom of a tall page uses a
  flow's `scroll` step.
- The sidecar client waits for a navigation as long as that navigation may
  take and not as long as every call may (`navigation_bound`).

- `cargo deny` ignores RUSTSEC-2024-0449 (`unsafe-libyaml` is unmaintained)
  until 2026-12-31: the crate comes in through `figment` and `serde_yaml`
  and reads the operator's own configuration and policy files only. The
  advisory was published in October 2026 and failed the gate for every
  checkout. Moving the YAML reading to a maintained parser is follow-up
  F45 (M12).
- The forwarding headers (`X-Forwarded-For`, `Forwarded`, `X-Real-IP`,
  `True-Client-IP`, `X-Client-IP`, `CF-Connecting-IP` and the rest of the
  family) are on the caller-header denylist (F34).
- A flow's `back` and `wait_for url` no longer hang when the page comes
  back from the browser's cache, and a capture after `back` reports the
  page that came back (F21). A driver `back` with no history fails.
- `job_items.request_id` is no longer a foreign key (migration 0013).
- `server.bind` is `127.0.0.1:8080` unless set (was `0.0.0.0:8080`): the
  API listens on loopback by default (spec §19). The compose file sets
  `0.0.0.0:8080` inside the container and publishes the port on the host's
  loopback, as before. A `kage serve` outside compose that other machines
  reach needs `server.bind`, and a reverse proxy that terminates TLS.
- A stored result names the cookies a site set without their values
  (`[redacted]`) unless `security.expose_set_cookie` is on; the cookies
  themselves are in the persona's jar as before.
- Every request to the API but `/healthz`, `/readyz` and `/openapi.json`
  needs an API key.
- `scripts/check-npm-licenses.mjs` takes the project to check as an
  argument (the sidecar without one) and reads the `license-policy.json`
  next to it.
- The sidecars' `SIDECAR_CONTEXT_IDLE_MS` is 6 minutes in compose (was 2):
  Kage closes a persona's context itself, and the sidecar's limit is the
  backstop.
- Mutation testing of `kage-core` is a gate: `just mutants` fails under
  a score of 70 % (`scripts/check-mutation-score.mjs`). It takes about
  two hours and is not part of `just ci`.
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
