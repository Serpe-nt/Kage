# Security review for 1.0.0

Kage's own security review before its first release (milestone M12, spec
§19 and §22), made by reading the code at the commits named here and
running the tests that hold each claim. It covers every road a request
can take into a network, every secret, the limits of the API, the
forwarder, the sidecars and the operator on the same host. Each finding
says what became of it: fixed (with its commit), accepted (with the
reason), or a follow-up for after 1.0, numbered by the planner.

Reviewed on 2026-10-17 at the M12 branch; the fixes are commits of the
same branch. Severities: **high**, Kage's master key or another party's
data within reach of a hostile page that takes over a browser;
**medium**, another promise of §19 broken once a component is taken
over; **low**, defence in depth; **info**, a decision written down.

## Who is trusted with what

| Party | Trusted with | Not trusted with |
|---|---|---|
| The operator (shell on the host, `deploy/.env`, `docker`) | everything: the master key, the database, the provider key, the CLI | — |
| An API consumer (a key) | the scopes of its key, its own requests, results and jobs | other keys' data, the admin endpoints without `admin`, any address the SSRF guard refuses, the identity headers of a persona |
| A page Kage fetches | nothing; its content is hostile by default | anything but the exit of its persona's proxy |
| A sidecar | the persona profiles (the `personas` volume), the files it writes, one shared token | the provider's credentials, the master key, Postgres, any way out but kage's forwarder |
| The proxy provider and its exits | the traffic of the personas (TLS is end to end) | the operator's own networks |

The sidecar is the component a hostile page is most likely to take over
(a browser exploit), so much of this review asks what a sidecar that was
taken over can do.

## SSRF, road by road

The guard is `kage_core::ssrf`: scheme `http`/`https`, no credentials in
the URL, not port 0, no name that never names a public site
(`localhost`, `*.local`, `*.internal`, one label, …), no literal address
in a denied range (loopback, link-local, RFC 1918, CGNAT, multicast, ULA,
cloud metadata, the reserved and documentation networks, and an IPv4
address embedded in an IPv6 one), and no literal address at all unless
`security.allow_ip_targets`. By default Kage resolves no name: the proxy
does (§19). With `security.resolve_on_host` the names below marked
"resolved" are also looked up on the host and refused when any address
is in a denied range.

| Road | Checked by | Where | Shown by |
|---|---|---|---|
| A fetch's URL (`POST /v1/fetch`, a job's item, a crawl's seed, `kage fetch`) | the guard at acceptance (`422 ssrf_denied` / `invalid_url`), resolved | `kage-api/src/resolve.rs::guard`, `kage-api/src/fetch/request.rs` | `kage-api/src/resolve.rs` tests, `tests/fetch.rs` |
| Any request of the pipeline: pages, `robots.txt`, sitemaps, scouts | the guard again before anything is sent | `kage-scheduler/src/pipeline.rs::run` | `kage-scheduler/tests/pipeline.rs` |
| A redirect on the http path | every hop before it is asked for, resolved | `RedirectChain`, `kage-http` with the host resolver | `kage-http` and `kage-api` tests |
| A redirect in a browser | the whole chain after the navigation; a refused hop is `ssrf_denied` and its captures are dropped | `kage-browser/src/ssrf.rs::check_chain` | `kage-browser/tests/client.rs`, `a_redirect_to_a_denied_address_ends_the_fetch_and_returns_nothing_of_it` |
| A page a flow visited | the same check on every captured page and on the last one | `check_capture` | `kage-scheduler/tests/flows.rs` |
| A link a crawl follows | the guard before the page is asked for, resolved; a refused page is `ssrf_denied` in the frontier | `kage-api/src/jobs/worker/crawl.rs` | `kage-api/tests/crawl.rs` |
| Everything a browser asks for by itself (subresources, scripts' requests, a redirect before the check) | the forwarder, on every `CONNECT` and absolute-form request, by URL | `kage-scheduler/src/forwarder.rs::check_target` | `kage-scheduler/tests/forwarder.rs` |
| A browser that tries to leave another way (direct, WebRTC, the compose network) | there is no other way: the sidecars are on an internal network with kage alone, loopback goes through the proxy (`<-loopback>`), WebRTC sends no UDP outside the proxy | `deploy/compose.yaml`, the engines | smoke test, `tests/e2e/fetch.sh`, `just hardening` |
| A webhook | `webhooks.allowed_origins` first; then the sender resolves the name itself, checks every address, connects to a checked address, follows no redirect | `kage-api/src/jobs/webhook.rs` | `kage-api/tests/jobs.rs` |
| The provider's API, AbuseIPDB, the health probes, the echo, the OTLP collector | the operator's configuration; the provider and AbuseIPDB roots must be `https` (plain `http` only to loopback); the OTLP endpoint takes no credentials or query | `kage-core/src/config/` | config tests |

What the guard cannot do, by design (§19): a browser's redirect has left
through the proxy before its check runs, and what the check prevents is
that anything read there is returned or stored. Names are resolved by
the proxy; `resolve_on_host` is for proxies that could reach the
operator's own networks, and it does not reach a browser's own requests
(G1 below).

## Every secret

| Secret | Lives in | Protected by | Rotation |
|---|---|---|---|
| The master key | `deploy/.env` → kage's environment (`KAGE__SECRETS__MASTER_KEY`, or `_FILE`) | never logged (`Secret`), redacted by `kage config check`; seals proxy credentials and webhook secrets (XChaCha20-Poly1305, AD bound to row and field); derives the forwarder's token key (HMAC) so the key itself signs nothing | `docs/runbooks/rotating-keys.md` |
| An API key's secret | its holder; the `api_keys` table holds its SHA-256 | compared in constant time (`subtle`); shown once | `kage key rotate` |
| The sidecar token | `deploy/.env` → kage and both sidecars (`SIDECAR_TOKEN` or `SIDECAR_TOKEN_FILE`) | compared in constant time (`timingSafeEqual` over digests); the header goes to the sidecars only | runbook |
| A persona's forwarder credential | the persona's browser and http client | HMAC of the persona id under the derived key, compared without an early exit; opens that persona's proxy only | follows the master key |
| Proxy credentials (provider or static list) | sealed in Postgres; in kage's memory | never leave the kage process (the forwarder adds them), except for the stand-in self-tests of a proxy (accepted below) | runbook |
| The provider key, the AbuseIPDB key | kage's environment | sent only to their `https` roots | runbook |
| A webhook secret | sealed in Postgres | signs deliveries (HMAC-SHA256); never in an answer of the API | a new job |
| The database password | Postgres's secret file and kage's `DATABASE_URL` | the database is on the stack's own network | runbook |
| The Grafana admin password | Grafana's own database | Grafana on loopback only | runbook |

No secret reaches a log line, a span or a metric label: the observability
end-to-end test greps every line and every span of a running stack for
ten secrets of the run and found none (`tests/e2e/observability.sh`, the
redaction audit: 2121 lines, 10 secrets, no finding). The `Secret` type
zeroes its memory on drop; the 32 bytes of the master key are copied into plain arrays where the
cipher and the token key are made, and those copies are not zeroed (S5).

## The API's limits

On every endpoint: a body of at most `server.max_body_bytes` (1 MiB) read
within `server.body_read_timeout` (30 s), a head of at most 16 KiB and 64
headers read within 10 s (slowloris), a request target of at most 8 KiB,
at most 1024 connections, `server.request_timeout` (30 s) for every
endpoint that does not wait for the network, 600 requests a minute for
the whole API and each key's own rate, concurrency and daily page budget.
At most 10 000 items a job and 100 steps a flow. A caller cannot set the
identity or transport headers of a persona. Keys have scopes; the admin
endpoints need `admin` and every admin call is in the audit log. The API
listens on loopback; beyond it the connection is plain HTTP and needs a
TLS-terminating proxy in front (the server warns at start). CORS is off
until origins are named. Every error is a problem document with a stable
code.

## The forwarder

Every persona's traffic, browser and http alike, leaves through kage's
forwarder: it checks the persona's credential, looks up that persona's
own proxy and no other, refuses what the guard refuses, adds the
provider's credentials, counts every byte, and relays nothing while a
leak is on record or the budget is spent. A refusal looks like a network
that does not answer. It listens on `0.0.0.0:8899` inside kage's
container and is never published; on the compose networks it is reached
by kage's name. A forwarder bound on a host is reachable from that host's
network, and opens nothing without a persona's credential (L5).

## The sidecars

A sidecar that was taken over

- **cannot** reach Postgres, the host or the internet: it is on the
  internal `browsers` network with kage alone (fixed, L4);
- **cannot** learn the provider's credentials: its browsers hold a
  per-persona credential of the forwarder (fixed for the command-line
  self-test of a persona, G4);
- **cannot** make Kage read a file of Kage's own container into a result:
  Kage takes a file only below `sidecar/`, as a plain file, with the size
  and SHA-256 the sidecar named (fixed, S1);
- **cannot** exhaust Kage's memory with an answer: at most 128 MiB is read
  (fixed, S4);
- **can** read and change every persona's profile (the `personas` volume
  is the sidecars' by design) and read or delete what the artifacts
  volume holds, the stored results of other requests included (V1,
  accepted with a follow-up);
- **can** lie in its answers: a page, a verdict input or a self-test
  report. Kage judges blocks from what it is told; a lying sidecar can
  make a persona look blocked or healthy. That is the trust a browser
  host needs.

The images run as 65532 with a read-only root, no capabilities,
`no-new-privileges`, Chrome's sandbox on (with a seccomp profile of four
added calls) and Firefox's own seccomp filter, and carry no setuid or
setgid file (`just hardening`, `just hardening-images`, the smoke test).

## The operator on the same host

The operator is trusted with everything; what this review asks is that
nothing is handed to whoever is not. `deploy/.env` is mode 600 and
ignored by git, and the gate's secret scan covers the whole history. A
secret given as an environment variable is visible to whoever may run
`docker inspect`, which is as much as root (S2; every Kage secret also
takes a `_FILE` form). `kage proxy list --reveal-username` writes an audit
event. Grafana is published on loopback only, with its own admin
password, and its plugins are not downloaded. `/metrics` and `/healthz`
without a key are on the compose network only, in the observability
profile; they hold no secret (the redaction audit).

## Images and dependencies

Every dependency is pinned exactly, the lockfiles are committed, there is
no `openssl-sys` in the graph, and `just deny` runs cargo-deny,
cargo-audit, pnpm audit and the license policy. Every image is pinned by
digest. Trivy finds no fixed critical or high vulnerability in the three
images Kage builds (`just scan-images`, in `just ci`): none on
2026-10-17, with the Trivy database of that day. The first scan of that
day found three in `libcurl4t64` of the Chromium image (CVE-2026-8286,
-8927, -92392, fixed in Debian's 8.14.1-2+deb13u7): the sidecar images take
their Debian packages from the archive when they are built, and that local
build had reused layers from before the update. Rebuilt without the
layer cache, all three images are clean. CI builds without one; locally,
`docker compose build --no-cache` before a release build. CycloneDX
SBOMs of the workspace, the sidecar and each image are made by `just
sbom` and `just sbom-images` (in `just ci` too), and the release
workflow attaches them to the release and binds each image's SBOM to its
digest. The release workflow pushes an image before it scans it: an image
that fails the scan is in the registry, unsigned, without attestations,
and in no release.

## Findings

| ID | Severity | Finding | Outcome |
|---|---|---|---|
| S1 | high | Kage read whatever path below the artifacts directory a sidecar named, followed links and ignored the declared size and SHA-256: a sidecar that was taken over could have Kage store its own `/proc/self/environ` as a screenshot (shown by `a_sidecar_cannot_hand_over_a_file_outside_its_part_of_the_artifacts_directory` against the old code), read it back from the shared volume and, before L4, send it anywhere | fixed in `d0e11da` |
| G4 | medium | `kage persona selftest` and `kage selftest … --persona` handed the persona's browser the provider's proxy with its password | fixed in `ebe019c` |
| L4 | medium | the sidecars were on the stack's default network: a browser that was taken over could reach Postgres and the internet past the forwarder | fixed in `7923f05`, `f1b0c4f` |
| S4 | medium | an answer of a sidecar was read whole, without a bound | fixed in `f3c944f` |
| G6 | low | an IPv4-translated address (`::ffff:0:a.b.c.d`) was not judged by its IPv4 address | fixed in `72a2ffe` |
| C1 | low | the provider's and AbuseIPDB's API roots accepted plain `http` to any host, which would send their keys in clear | fixed in `a04983c` |
| G1 | low | with `resolve_on_host`, a browser's own requests (and the sitemaps and `robots.txt` a scout reads in one) are judged by URL only: a name that resolves into the operator's network passes the forwarder | follow-up after 1.0: the forwarder resolves names when `resolve_on_host` is on, and `kage config check` warns about a static list of proxies without it |
| G2 | info | between the host's lookup and the proxy's own, a name can change its answer | accepted: the proxy resolves where it connects, and only it could close this; with a residential provider the operator's networks are out of the exit's reach |
| G3 | low | any port but 0 may be asked for: an API consumer can send an HTTP request through an exit to a port that serves something else | follow-up after 1.0: the http path refuses the ports the browsers themselves refuse (their "unsafe ports") |
| G5 | low | the forwarder checks its own parse of a request's target but relays the target as the client wrote it; a proxy that parses it otherwise could connect elsewhere | follow-up after 1.0: relay the authority that was checked. Low because the `url` crate is stricter than proxies at recognising an address, and names are the proxy's to resolve either way |
| S2 | info | secrets are environment variables in compose | accepted: `docker inspect` needs root's access anyway; every secret has a `_FILE` form for Docker secrets |
| S3 | — | no test showed that logs and traces hold no secret | resolved: the redaction audit of the observability end-to-end test |
| S5 | info | copies of the master key's bytes are not zeroed | accepted: they live as long as the process, which holds the key anyway |
| L1 | info | the API's global rate is spent before the key is checked | accepted, by design: it bounds the key lookups an unauthenticated flood causes; a deployment that serves other machines limits clients at its reverse proxy |
| L2 | low | jobs have global limits only: one key can queue many jobs | follow-up after 1.0: a per-key limit of open jobs and items |
| L3 | low | the `domain` label of the request and attempt metrics grows with every domain asked for | follow-up after 1.0: a bound on distinct domains, the rest as `other`; until then, drop the label in Prometheus' relabelling where many domains are fetched |
| L5 | info | the forwarder binds `0.0.0.0` by default | accepted: it opens nothing without a persona's credential and is never published; a bare `kage serve` on a host names `forwarder.bind` |
| D1 | info | the database connection has no TLS by default | accepted: compose's Postgres is on the stack's own network; a remote database takes `sslmode=verify-full` in `database.url` (sqlx over rustls with the webpki roots; a private CA is named with `sslrootcert`) |
| V1 | low | the artifacts volume is shared: a sidecar that was taken over can read and delete stored artifacts | follow-up after 1.0: a volume of their own for the sidecars' files, mounted below `sidecar/` in kage |

## Accepted exceptions

- **RUSTSEC-2026-0367, -0368, -0369 (`zip` 8.6.0)** are ignored in
  `deny.toml` and `.cargo/audit.toml` until 2026-12-31 or the first
  `utoipa-swagger-ui` on `zip` 9, whichever comes first. `zip` unpacks only
  the vendored Swagger UI archive at build time, never an archive from
  outside. Follow-up F55 drops the ignore.
- **The stand-in self-tests of a proxy** (`kage selftest http|browser
  --proxy`) hand the sidecar the provider's proxy with its credentials:
  there is no persona yet for the forwarder to name. An operator's
  command, on demand, against the pages it names (§7.5).
- **The browser's redirect check is after the fact** (§19): the request to
  a refused hop has left through the proxy; nothing read from it is
  returned or stored.
- **The `domain` metric label** is unbounded until its follow-up (L3).

## Known tells

Added by the Camoufox 156 milestone (2026-10-18), T4 to T7 by the Chrome
switch audit (2026-10-19). A tell is not a breach
of §19, but it is what the product exists to avoid, and one that every
persona of a host or of an engine shares is a risk for the whole pool:
it can link personas that live behind different addresses. The tells of
one persona and one engine are listed in the engine's runbook
(`engine-firefox.md` and `engine-chromium.md`, "Known tells"); these are
the pool-level ones.

| ID | Engine | Tell | Outcome |
|---|---|---|---|
| T1 | both | Canvas pixels and text metrics are the host's (Mesa and FreeType): every persona of a host draws the same canvas while each names its own graphics card. A vendor that knows which canvas hash goes with which card, or that clusters hashes across addresses, can link them. Since Camoufox 156 this holds for Firefox as it did for Chromium (the 152 build varied the canvas per persona, which upstream removed as a tell of its own) | accepted (ADR 0011); the field campaign measures how rare the hash is; if an answer is needed it is a real GPU or per-persona rendering, never noise |
| T2 | chromium | A cross-site frame read its site's first-party `localStorage` and IndexedDB, and an `http` address of a public host was never tried over https: patchright's default switches turned `ThirdPartyStoragePartitioning` and `HttpsUpgrades` off, where a Chrome nobody configured has both | fixed in 1.0.2 by the Chrome switch audit (ADR 0014): Kage names every switch Chrome gets and leaves the rest to Chrome; the parity suite states partitioned storage for both engines, an integration test the upgrade, and another the browser's whole command line |
| T4 | chromium | Every Chromium persona of 1.0.0 and 1.0.1 showed a page a bar over it on every start ("an unsupported command-line flag: `--disable-blink-features=AutomationControlled`"): the viewport 56 px shorter than the window's (`outerHeight − innerHeight` 143 where a stock Chrome shows 87). The switch stays, since the driver's pipe would make `navigator.webdriver` true without it | fixed in 1.0.2 (ADR 0014): `--test-type` suppresses the bar and changed nothing a page reads; an integration test holds the height |
| T5 | chromium | Google's properties (search, YouTube, reCAPTCHA's frames) see a Chrome 155 that never fetched a variations seed or a component and sends no `X-Client-Data` header, persona after persona: `--disable-background-networking` | accepted (ADR 0014): measured live in the switch audit, an idle persona-hour costs about 89 kB with the switch and 139 kB without, which adds the variations seed and Safe Browsing to the hosts asked for (`docs/runbooks/engine-chromium.md`); no target sees the background traffic |
| T6 | chromium | The built-in AI APIs answer `unavailable` (`LanguageDetector`, `Summarizer`, `LanguageModel`) where a stock Chrome 155 says `downloadable`: `OptimizationHints` stays off | accepted (ADR 0014): otherwise a page's `create()` after a click makes the persona download a model through the pool, gigabytes for `LanguageModel`; the field campaign looks for sites that probe the APIs |
| T7 | chromium | WebGL comes from Mesa's llvmpipe through `--ignore-gpu-blocklist`; a stock Chrome on such a machine refuses llvmpipe and has no WebGL at all, so the configuration is one no stock Chrome reaches by itself, the same for every persona of a host | accepted (ADR 0009, ADR 0014); the answer is the host's GPU (`SIDECAR_GL=host`), like T1's |
| T3 | firefox | The 1.0.0 Firefox personas (Camoufox 152) showed four things stock Firefox does not: a page could construct `new Worker("chrome://juggler/…")`, a cross-site frame read first-party cookies and storage, `window.open()` without activation opened a window, and pages had 15 extra `set…` functions | fixed by the move to Camoufox 156 (ADR 0011): each persona at its next open after the deploy |
