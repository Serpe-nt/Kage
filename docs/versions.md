# Pinned versions and verification log

Policy: every dependency is pinned to an exact version in `Cargo.toml`
(`=x.y.z`) and `package.json` (no caret, no tilde), lockfiles are committed,
and bumps are deliberate PRs that re-run the fingerprint self-test suite.
Renovate or Dependabot may open bump PRs but must not auto-merge anything in
the "fingerprint-critical" set.

Re-verified on 2026-10-08 during M0 against crates.io, npm, nodejs.org, the
container registries and GitHub releases. A newer patch release is
acceptable when a pin is bumped; a newer minor or major requires a note in
the PR.

The **State** column says how far a pin has got:

- **locked**: in a committed lockfile, compiled or installed in CI, covered
  by the gates.
- **pinned**: exact version declared in `[workspace.dependencies]`; it enters
  `Cargo.lock` when a crate first uses it.
- **planned**: verified on the registry, not declared anywhere yet.

## Fingerprint-critical (bump together, re-test, never auto-merge)

| Package | Ecosystem | Version | State | Notes |
|---|---|---|---|---|
| wreq | crates.io | 0.16.1 (2026-08-27) | locked (`kage-http`, `kage-proxy`, `kage-api` since M9) | Newest stable. The `6.0.0-rc.*` line is abandoned (last: rc.31, 2026-08-18); the project re-versioned to stable 0.16.x on 2026-08-22. Links BoringSSL through `btls` 0.5.6. No HTTP/3. Pulls the pre-release `wreq-rt` 0.2.2-rc.4 transitively. MSRV 1.98. Since M2 the features `gzip`, `brotli`, `deflate` and `zstd` are on (`kage-proxy`): a client that advertises an encoding like the browser it emulates must be able to decode it. They pull `tower-http` 0.6.11 and `async-compression` 0.4.50 in transitively. wreq reads `HTTP_PROXY`-style variables unless a client sets a proxy or calls `no_proxy()`; every client in the workspace does one or the other. Since M3 `kage-http` also turns on `cookies`: not wreq's jar, which stays unused, but the per-request cookie hook, which lets Kage's own jar answer once wreq knows whether the connection is HTTP/2. wreq's redirect handling and default-header merge are off in every client `kage-http` builds. |
| wreq-util | crates.io | 0.2.0 (2026-08-22) | locked (`kage-http`) | Emulation profiles. Must contain a profile whose browser major matches each engine's bundled browser, else the http path is disabled for that persona. Newest profiles in this release: `chrome_149`, `edge_148`, `firefox_151`, `safari_26.4`. Since M2 the features `emulation-compression` (the profile sends `Accept-Encoding` as the real browser does; without it the header is missing) and `emulation-serde` (profile names such as `chrome_149` parse from configuration) are on. The default of `proxies.health.profile` is the newest Chrome profile; a test fails when the pin moves and the default does not. Since M3 only `kage-http` names it (`kage-proxy` builds its probe client through `kage-http`). `kage-http` offers the newest eight majors per family (Chrome 142 to 149, Firefox 144 to 151, each on Windows, macOS and Linux) and takes TLS, the HTTP/2 settings and the base header values from the profile; the header list and its order are Kage's own. The JA4, the HTTP/2 fingerprint and the header order of each are recorded in `tests/fixtures/fingerprints/` and compared in `just ci`, so a bump of this pin or of wreq that moves one fails there and must re-record them on purpose (`docs/runbooks/fingerprint-echo.md`). No profile exists for Chrome 150 or newer, nor for Firefox 152 or newer: see spec §6.1 step 4. |
| wreq-proto | crates.io | 0.2.6 | locked (`kage-http`) | Added in M3 as a direct dependency; it was already in the lockfile as the protocol crate under wreq 0.16.1 and moves with it. `kage-http` names it for one hook (`ext::on_preserve_header`), which decides the order and the spelling of request headers when they are written to the wire. |
| patchright | npm | 1.63.0 | locked (`engine-chromium`) | Newest. Pins `patchright-core 1.63.0`. Chromium engine, headful. Since M4 it drives the pinned Chrome by its path (what `channel: "chrome"` resolves to) through the CPU-affinity wrapper, with `chromiumSandbox: true`: the driver passes `--no-sandbox` otherwise. What M4 measured about this version and builds on: every page is intercepted by the driver itself, scripts run in an isolated world by default, an init script through the driver switches the HTTP cache off (so the engine never uses one), and the console domain is never enabled. A bump re-runs the engine's integration tests, which pin each of these. |
| camoufox-js | npm | 0.12.1 | locked (`engine-firefox`) | Newest. Experimental JS port of the Python launcher. Peer `playwright-core < 1.63.0`. Since M5 the Firefox engine uses its parts and not its entry point: `dist/fingerprints.js` (the real-device generator and the mapping to the browser's configuration), `dist/mappings/fonts.config.js` and `dist/data-files/webgl_data.db`. `launchOptions` is never called (it is not reproducible, sets a fontconfig path the pinned build does not have, downloads an addon at run time, and reads the claimed system through ua-parser-js) and the launcher's `fetch` is never used (it does not install a pinned build, see below). Depends on `fingerprint-generator ^2.1.86` (resolved 2.1.88), `ua-parser-js ^2.0.10` (overridden to the MIT 1.0.41, ADR 0008) and `better-sqlite3`. A bump changes what a pinned seed generates (a unit test with literal values fails then, on purpose) and must re-read `dist/utils.js` for new uses of ua-parser-js; `docs/runbooks/engine-firefox.md`. |
| playwright-core (firefox engine package only) | npm | 1.62.1 | locked (`engine-firefox`) | The highest below 1.63.0, as camoufox-js requires. npm latest is 1.64.0. Lives only in `sidecar/packages/engine-firefox`; a test fails if another package can resolve it. Since M5 it drives Camoufox (persistent context, proxy, screenshots, cookies, console); human input does not go through it. |
| Camoufox browser binary | GitHub daijro/camoufox | v152.0.4-beta.30 (2026-09-01), `camoufox-152.0.4-beta.30-lin.x86_64.zip`, sha256 `5720d45b894ce1770543de024c6f10d514b38be560fa2dc3226b3d8586caf672` | locked (`deploy/docker/sidecar.Dockerfile`: `CAMOUFOX_VERSION`, `CAMOUFOX_RELEASE`, `CAMOUFOX_SHA256`; `engine-firefox/src/host.ts`) | Firefox 152.0.4. Fetched by exact URL and verified by checksum in the image build and by `scripts/setup-camoufox.sh`; `version.json` is written by us; the engine refuses any other build at start. Maintainer: "may not be suitable for stable production use" (risk R1). 663 MB, 1.2 GB unpacked; it bundles the font files of Windows and macOS (why the Firefox image is not published, ADR 0008). `engines.firefox.browser_major` is 152. Its capture against the echo is `tests/fixtures/fingerprints/browser/firefox-152.json`; it is browser-only on the http path. A bump: `docs/runbooks/engine-firefox.md`. |
| Juggler backport inside that build (`omni.ja`) | this repository, `scripts/patch-camoufox-juggler.mjs` (the change is upstream's, Camoufox pull request 779) | `omni.ja` of the release: sha256 `269da6dadc2dd93b5b0baafecccc8de1849020d04889d4fdaaa5423bf7e8545e`; with the backport: sha256 `ad3799d8c96fa80ba26a58cb1aaa28f525801d9ecc727a3e2be9262318ccbf5c` | locked (`deploy/docker/sidecar.Dockerfile`: `CAMOUFOX_OMNI_SHA256`, `CAMOUFOX_OMNI_PATCHED_SHA256`; `engine-firefox/src/host.ts`: `CAMOUFOX_OMNI_SHA256`, the second value) | Added in M5.1 (ADR 0010). The driver inside the build ran every evaluation of a page as user input, so an untouched page read user activation; the four statements are overwritten in place (same length, 260 bytes differ, nothing repacked). The tool refuses any other archive, the engine refuses any other result, and a test keeps the Dockerfile and the engine at the same value. Gone when the build moves to one from `v156.0.1-beta.32` on, which has the fix. |
| fingerprint-generator | npm | 2.1.88 | locked (`engine-chromium`, and transitive through camoufox-js) | Apify fingerprint-suite. Direct dependency since M4 for the screen and CPU draws of a chromium persona. It has no seed parameter: the engine swaps `Math.random` for a seeded generator during the synchronous call. Its data stops at Chrome 147, so the browser version is never taken from it. A bump changes what a pinned seed generates; a unit test with a pinned seed fails then, on purpose. |
| fingerprint-injector | npm | 2.1.88 | locked (`engine-chromium`) | Same suite, same version as the generator. Used only for `inject: window` (cross-OS claims, off by default, ADR 0009): the engine takes its script and applies it itself; the injector's own attach (header rewriting through a route, forced dark mode, random `history.length`) is not used. |
| Chrome (stable channel, in the Chromium engine image) | Google (`dl.google.com/linux/chrome/deb`) | 155.0.8059.39, package sha256 `c58aa0f2cd66179c9f050e062c882d27aa9b9f8c2b7c73fee3498560b5ed0b38` | locked (`deploy/docker/sidecar.Dockerfile`: `CHROME_VERSION`, `CHROME_SHA256`) | The `.deb` of that exact version, verified by checksum at build; Google's apt source and update job are removed, so the image never moves by itself. `scripts/setup-chrome.sh` installs the same build for tests from the same two lines, and an integration test fails when the installed Chrome is another one. The image records it in `/etc/kage/chrome-version`. `engines.chromium.browser_major` is 155. Its capture against the echo is `tests/fixtures/fingerprints/browser/chromium-155.json`; it is browser-only on the http path (`BROWSER_ONLY_MAJORS`, `docs/runbooks/fingerprint-echo.md`). A bump: `docs/runbooks/engine-chromium.md`. |
| Docker default seccomp profile (base of `deploy/seccomp/chromium.json`) | GitHub moby/profiles | commit `6fe7deb1b9fb7c0397a4593480d7d22b9ee8caef` | locked (derived file committed) | Added in M4. The Chromium sidecar's profile is this file plus one rule (`chroot`, `clone`, `setns`, `unshare`), written by `scripts/derive-chromium-seccomp.mjs`. Not a fingerprint: listed here because Chrome does not start without it and it must move together with the Docker default. |
| Xvfb, `x11-xkb-utils`, Mesa (`libgl1-mesa-dri`, `libegl1`), PulseAudio (both engine images); desktop fonts (Chromium image); GTK 3 and Firefox's runtime libraries (Firefox image) | Debian 13 (trixie), from the base image's distribution | the versions of the pinned base image's release; recorded per build in `/etc/kage/display` and `/etc/kage/fonts` | locked by the base image digest and the build date | Added in M4, extended in M5. They decide what WebGL and font enumeration report on chromium; `setxkbmap` gives a display the persona's keyboard layout and PulseAudio gives the browsers an output and an input (both engines, M5). Debian packages are not pinned one by one: a rebuild takes the distribution's current point release, and the files under `/etc/kage/` say what an image has. The chromium font set is the one `task-gnome-desktop` installs on Debian 13. |

### Camoufox browser binary: one pin, fetched by us

`camoufox-js` 0.12.1 only accepts browser builds in a fixed range
(`dist/__version__.js`): below `beta.32`, and, when the installed
`playwright-core` is 1.61 or newer, at least `beta.30`. The owner decided
on 2026-10-08 (D29) to move the pin to **v152.0.4-beta.30** in M5, which
this milestone did.

M5 found that the range holds a second build: `camoufox-152.0.4-beta.31`,
attached to the GitHub release `font-bundle-v1` (2026-09-24), whose notes
say "Build input, not a browser download. Nothing here is meant to be
installed." The launcher's own `fetch` installs the first non-prerelease
release whose asset is in range, which is that one. Kage therefore never
uses the launcher's download: the image build and
`scripts/setup-camoufox.sh` fetch beta.30 by its URL and checksum, and the
engine refuses to start on any other build. The `v156.0.1` line (beta.32
to beta.36, latest 2026-10-06) is outside the launcher's range.

M5.1 found that upstream fixed, in exactly those builds, what M5 had
measured in this one: Juggler granting user activation for every
evaluation (pull request 779, merged 2026-09-25). The fix is backported
into the pinned build's `omni.ja` by `scripts/patch-camoufox-juggler.mjs`
(ADR 0010, the row above): the build stays the pin, and what the engine
runs of it is pinned by a second checksum. Moving to the v156 line is a
fingerprint-critical bump of its own and removes the backport.

Measured in M5 for the record: a stock Mozilla Firefox 152.0.4
(`firefox-152.0.4.tar.xz`, sha256
`cfbe0a66269f220c69809210eda55fee72f4798427e78181b7d0da14df44882f`) shows
the fingerprint echo the same JA4, HTTP/2 fingerprint and header order as
this Camoufox build. It is not a dependency: it was downloaded once for
the comparison.

## Rust core

| Crate | Version | State | Notes |
|---|---|---|---|
| tokio | 1.53.2 | locked | `default-features = false`; each crate enables what it needs. |
| axum | 0.8.9 (2026-04-14) | locked | |
| tower | 0.5.3 | locked | Since M3 also `kage-http`: `Layer` and `Service` for the layer that times connection setup. |
| tower-http | 0.7.1 | locked (`kage-api`, first use in M8) | Default features off; `catch-panic` (a handler that panics is a `500` problem, and the connection lives on) and `cors` (only when `server.cors_allowed_origins` names origins). The body limit is axum's, the request id, the timeouts and the request log are the API's own layers, and nothing is compressed by the API. Same `http` 1 and `tower` 0.5 stack as axum 0.8. wreq still pulls 0.6.11 in for its decoders; the two do not meet. |
| hyper, hyper-util | 1.12.0, 0.1.21 | locked (`kage-api`, direct since M8) | Both were in the lockfile through axum. hyper with `server` and `http1`, hyper-util with `server-graceful`, `service` and `tokio`: the listener of the API, because `axum::serve` has no switch for the limits of spec §19 (the size and count of headers, a deadline for the head of a request, a connection cap). HTTP/1.1 only, on purpose: hyper-util's protocol-detecting server (`server-auto`) waits for a connection's first bytes without a deadline, which a silent client can hold for ever; hyper's own HTTP/1 server starts the header deadline when the connection is accepted. |
| utoipa | 6.0.0 (2026-09-22) | locked | Compatibility verified in M0: `utoipa-axum` 0.3.0 and `utoipa-swagger-ui` 10.0.1 both require `utoipa ^6.0.0` and `axum ^0.8.4`, and the three compile together. Pinned together. |
| utoipa-axum | 0.3.0 (2026-09-22) | locked | `kage-api` registers its route through it. |
| utoipa-swagger-ui | 10.0.1 (2026-09-23) | locked | Features `axum` and `vendored`. Without `vendored` the build script downloads the Swagger UI bundle at compile time. |
| sqlx | 0.9.0 (2026-05-21) | locked (`kage-store`) | `postgres`, `runtime-tokio`, `tls-rustls-ring-webpki`, `chrono`, `uuid`, `json`, `migrate`, `macros`. Never `tls-native-tls` (OpenSSL conflict with BoringSSL). MSRV 1.94. |
| serde, serde_json | 1.0.229, 1.0.151 | locked | |
| tracing, tracing-subscriber | 0.1.44, 0.3.23 | locked | |
| tracing-opentelemetry | 0.34.0 | pinned | |
| opentelemetry, opentelemetry_sdk, opentelemetry-otlp | 0.33.0 | pinned | Default features off: the OTLP transport is chosen in M12 and must use rustls, never OpenSSL. |
| metrics, metrics-exporter-prometheus | 0.24.6, 0.18.3 | locked (metrics: `kage-proxy`, `kage-scheduler`, `kage-api`; exporter: `kage-api`, first use in M8) | The exporter has default features off: no HTTP listener and no push gateway of its own, so no second HTTP stack; `kage-api` renders its handle at `/metrics`. Brings `metrics-util` 0.20.4, `quanta` 0.12.6, `sketches-ddsketch` 0.3.1, `evmap` 11.0.0 with `left-right` 0.11.8 and `hashbag` 0.1.13, `crossbeam-epoch` 0.9.21, `raw-cpuid` 11.6.0, and, in the lockfile only (they build under `cfg(loom)` or for wasm), `loom` 0.7.2, `generator` 0.8.10 and `web-sys`. |
| figment | 0.10.19 | locked (`kage-core`) | Features `yaml` and `env`: `kage.yaml`, the policy files and `KAGE__SECTION__KEY` layering. Its YAML parser is `serde_yaml` 0.9.34+deprecated (no longer developed upstream) over `unsafe-libyaml` 0.2.11, which RUSTSEC-2024-0449 names as unmaintained since October 2026; both only read operator-controlled files. The advisory is ignored in `deny.toml` with that reason and a review date of 2026-12-31 (M10, decided with the planning session); follow-up F45 (M12) moves the YAML reading to a maintained parser. |
| clap | 4.6.7 | locked | `derive`, `env`. |
| thiserror, anyhow | 2.0.21, 1.0.104 | locked | `anyhow` only in the binary crate. |
| async-trait | 0.1.92 | locked (`kage-proxy`, `kage-http`) | The provider, prober and reputation traits are used as trait objects; so are the cookie backend and the hop resolver of `kage-http`. |
| governor | 0.10.4 | locked (`kage-api`, first use in M8) | Default features off, `std` on: the direct (unkeyed) limiter only, one per API key and one for the whole API. The keyed stores (`dashmap`) and the jitter (`rand`) are not built. Brings `nonzero_ext` 0.3.0, `spinning_top` 0.3.0 and `futures-timer` 3.0.4. The pace per domain is the scheduler's own (M7), not governor's. |
| notify | 8.2.0 | locked (`kage-core`) | Policy hot reload. 9.0.0 is still a release candidate. License CC0-1.0, allowed in `deny.toml` since M1. |
| url, ipnet, hickory-resolver | 2.5.8, 2.12.2, 0.26.3 | url and ipnet locked (`kage-core`, url also `kage-proxy` and `kage-api`), hickory-resolver pinned and still unused | SSRF guard. The guard itself is pure functions over resolved addresses (M1). The host-side lookup of M8 (`security.resolve_on_host`) asks the operating system's resolver through tokio (`getaddrinfo`) and not hickory: it follows the host's own configuration, and hickory with the system configuration would have brought 27 more crates into the lockfile for a lookup that is off by default. The pin stays for the webhook sender of M9 to decide on; remove it there if it is still unused. |
| sha2, hmac, rand, secrecy, uuid | 0.11.0, 0.13.0, 0.10.3, 0.10.3, 1.27.0 | sha2, rand, secrecy, uuid locked (`kage-core`, sha2 also `kage-store` and `kage-scheduler`); hmac locked (`kage-scheduler`, first use in M7: the token a persona's clients present to the forwarder) | `rand` has default features off; `kage-core` enables only `chacha` for the value-stable ChaCha8 generator behind recorded seeds. |
| base64, subtle, getrandom | 0.23.1, 2.6.1, 0.4.3 | locked (`kage-api`, base64 also `kage-scheduler`; direct since M8) | All three were in the lockfile already. An API key's secret is 32 bytes from the operating system (`getrandom`), written base64url without padding (`base64`), and its SHA-256 is compared in constant time (`subtle`). `kage-scheduler` decodes the bodies a sidecar returns base64-encoded before it stores them as artifacts. |
| chacha20poly1305 | 0.11.0 | locked (`kage-proxy`) | Added in M2. XChaCha20-Poly1305 for proxy credentials at rest (spec §19). RustCrypto, pure Rust, no OpenSSL. Features `alloc`, `getrandom` (nonces from the operating system) and `zeroize`. Brings `aead` 0.6.1, `cipher` 0.5.2, `poly1305` 0.9.1, `universal-hash` 0.6.1. |
| zeroize | 1.9.1 | locked (`kage-proxy`) | Added in M2 as a direct dependency (it was already in the lockfile through `secrecy`). Wipes the master key bytes and decrypted credentials. |
| regex | 1.13.1 | locked (`kage-core`, `kage-crawl`) | Added in M1. Block signature patterns. Linear-time matching, so a signature cannot hang the classifier on a hostile page. Since M10 also the `allow` and `deny` patterns of a crawl: a pattern a caller sends cannot hang the worker either, and one too large to compile is refused with the job. |
| flate2 | 1.1.10 | locked (`kage-store`, `kage-crawl`) | Added in M1. gzip for text bodies in the artifact store. Default pure-Rust backend. Since M10 also gzipped sitemaps, unpacked up to a byte limit. |
| scraper, html5ever | 0.27.0, 0.39.0 | locked (`kage-crawl`, first use in M10) | The tree of a page: a crawl reads its links and its text from it (`kage_crawl::html`), on the http path and wherever a browser did not bring the links itself. html5ever is held at the version scraper 0.27.0 is built against so their types match (0.40.1 exists) and is not named by any crate of ours. Behind them: `selectors` 0.38.0, `cssparser` 0.37.0 with `cssparser-macros` 0.7.1, `markup5ever` 0.39.0, `web_atoms` 0.2.6, `string_cache` 0.9.0 with `string_cache_codegen` 0.6.1, `tendril` 0.5.1, `ego-tree` 0.11.0, `servo_arc` 0.4.3, `phf` 0.13.1 with `phf_codegen`, `phf_generator`, `phf_macros` and `phf_shared` 0.13.1 (a second `phf` line beside `chrono-tz`'s 0.12.1), `derive_more` 2.1.1 with `derive_more-impl`, `dtoa` 1.0.11 with `dtoa-short` 0.3.5, `precomputed-hash` 0.1.1, `new_debug_unreachable` 1.0.6, and at build time `rustc_version` 0.4.1 with `semver` 1.0.28, `getopts` 0.2.24 and `unicode-width` 0.2.2. All MIT, Apache-2.0, MPL-2.0 or BSD-3-Clause, which `deny.toml` allowed already; no TLS, no network, nothing unsafe of ours. Extraction (M11) will use the same pin. |
| robotstxt | 0.3.0 | pinned | Google robots.txt matcher port. Last release 2021. |
| quick-xml | 0.42.0 | locked (`kage-crawl`, first use in M10) | Sitemaps and sitemap indexes, read as a stream with a limit on entries; no DTD is loaded and no entity is expanded beyond the five of XML. |
| chrono | 0.4.45 | locked (`kage-core`, `kage-store`, `kage`, `kage-persona`) | |
| chrono-tz | 0.10.4 | locked (`kage-core`) | Added in M6. The IANA time zone database, compiled in (no file read, no network): a persona's calendar day and local midnight for the daily cap, and its waking hours for warm-up visits. Default features. A newer database is a deliberate bump. Brings `phf` 0.12.1, `phf_shared` 0.12.1 and `siphasher` 1.0.4 into the lockfile. MIT or Apache-2.0. |
| http-body-util | 0.1.5 | locked (`kage-http`) | Since M3 a real dependency: `kage-http` reads a response body frame by frame so that the size cap can stop it. |
| http, bytes | 1.5.0, 1.12.1 | locked (`kage-http`) | Added in M3 as direct dependencies at the versions already in the lockfile, so that `kage-http` names the same `http::Request` and `Bytes` wreq does. |
| futures-util | 0.3.34 | locked (`kage-http`) | Added in M3 as a direct dependency (already locked). Default features off, `std` only: a bounded number of subresource requests at once. |
| psl | 2.1.239 (2026-10-03) | locked (`kage-http`, `kage-crawl`) | Added in M3. The Public Suffix List, compiled in; no network, no file read. It decides what a site is: `Sec-Fetch-Site`, `SameSite` cookies, and which `Domain` a cookie may name. The crate is re-released whenever the list changes, several times a week; 2.1.241 (2026-10-08) existed on the verification day and the pin is a release a few days old on purpose. A newer list is a deliberate bump like any other. Brings `psl-types` 2.0.11. Since M10 also what a site is to a crawl (`same_site_only`, one visitor per site): the same list at the same version, so a crawl and a cookie never disagree about a site. |
| rustls, tokio-rustls | 0.23.45, 0.26.6 | locked (`kage-http`) | Added in M3 as direct dependencies at the versions sqlx already locks. The fingerprint echo terminates TLS with them. Default features off; provider `ring` and `tls12`, never `aws-lc-rs` (one more C library next to BoringSSL). Outbound HTTP stays on wreq. |
| webpki-root-certs | 1.0.9 | locked (`kage-http`) | Added in M3 as a direct dependency at the version wreq resolves: the Mozilla roots wreq trusts by default, so that the self-test client can trust them plus the operator's own authority (`selftest.http.echo_ca_file`). |
| wiremock | 0.6.5 | locked (`kage-proxy`, `kage-http`, `kage`) | Test only. The mock Webshare API, the mock AbuseIPDB, and the target sites of the http path's tests. |
| rcgen | 0.14.10 | locked (`kage-http`, `kage`) | Added in M3. Test only. Makes a throwaway certificate authority and a server certificate for the tests that run the fingerprint echo, at run time; no key is committed. Brings `x509-parser` 0.18.1, `yasna` 0.6.0, `pem` 4.0.0 and their ASN.1 crates into the lockfile, none into a release build. |
| proptest | 1.11.0 | locked (`kage-proxy`, `kage-core`) | Added in M2. Test only. Property tests of the pool state machine. Default features off (`std` only): no process forking. Since M10 also the properties of URL normalization in `kage-core` (spec §13). |
| testcontainers, testcontainers-modules | 0.27.3, 0.15.0 | locked (`kage-store`, `kage-http`, `kage`) | Test only. testcontainers 0.28.0 exists, but testcontainers-modules 0.15.0 requires `^0.27`; the two move together. Default features off: the Docker socket needs no TLS stack. |
| tempfile | 3.27.0 | locked (`kage-core`, `kage-store`, `kage`) | Added in M1. Test only. |

M2 added 27 packages to `Cargo.lock` (427 to 454): the cipher and its
traits, the response decoders behind wreq, wiremock and proptest with their
dependencies. `brotli` and `zstd` are now in the lockfile in two versions
each (wreq-util's certificate compression and tower-http's decoders ask for
different ones); `cargo deny` reports duplicates as warnings.

M3 added 16 packages to `Cargo.lock` (454 to 470): `psl` and `psl-types`,
`cookie` 0.18.2 (through wreq's `cookies` feature), and `rcgen` with its
ASN.1 and X.509 dependencies, which only tests link. Every other new direct
dependency was in the lockfile already.

M10 added 28 packages to `Cargo.lock` (497 to 525): `scraper` 0.27.0 and
`quick-xml` 0.42.0, both pinned since M0 and used for the first time by
`kage-crawl`, and the 26 packages behind `scraper` that the table names.
`phf` and `phf_shared` are in the lockfile in two versions each (0.12.1
through `chrono-tz`, 0.13.1 through `selectors`); `cargo deny` reports
duplicates as warnings. The openssl checks and the license policy pass
unchanged.

M0 resolved all 44 external pins together in a scratch workspace: 492
packages, one version of every HTTP-stack crate, TLS through `ring`, `rustls`
and `btls-sys` only, and no `openssl-sys`, `openssl`, `native-tls` or
`reqwest`. That check caught the two held-back pins above.

Advisories: `cargo deny` fails on every advisory against the lockfile,
"unmaintained" included. One is ignored, with its reason and a review date
in `deny.toml`: RUSTSEC-2024-0449 (`unsafe-libyaml`, through `figment` and
`serde_yaml`; review by 2026-12-31; follow-up F45 for M12). An ignore is a
decision recorded there and here, never a way to get a gate past a
vulnerability.

Hard rule: `cargo tree -i openssl-sys` must be empty. `just deny` enforces it
three ways: `scripts/check-no-openssl.sh` (lockfile and `cargo tree`), and a
ban on `openssl-sys`, `openssl` and `native-tls` in `deny.toml`. `Cargo.lock`
contains `openssl-macros`; that is a proc-macro crate used by `btls` and links
nothing.

## Node sidecar

| Package | Version | State | Notes |
|---|---|---|---|
| Node.js | 24.21.0 (Active LTS "Krypton") | locked | `.nvmrc`, `engines` and the image tag pin it. 26.x is the Current line and not LTS yet. |
| pnpm | 10.34.6 | locked | Latest 10.x, pinned with its hash in `packageManager`. 11.x and 12.x exist; moving is a deliberate PR. `hoist: false` keeps the two playwright versions apart. |
| TypeScript | 5.9.3 | locked | Latest 5.x. 6.0.3 and 7.0.2 exist; `typescript-eslint` 8.71.1 supports `<6.1.0`, so 7.x is not usable yet. `strict: true`. |
| fastify | 5.12.5 | locked (`server`) | Internal HTTP server. Since M4 with request and response validation against the contract. |
| zod | 4.6.5 | locked (`contract`, `engine-core`, `server`) | Contract schemas. Since M4 the OpenAPI document is emitted from them (`z.toJSONSchema` over the registry) and committed as `sidecar/packages/contract/openapi.json`; `kage-browser` generates its Rust types from that file. A bump can change the emitted document: `just contract-check` shows it. |
| pino | 10.4.0 | locked (`server`) | Direct dependency since M4: the server's and the engine's log, with the field names of the Rust side. |
| @prometheus-io/client | 0.16.1 | locked (`server`) | Added in M4 in place of the planned `prom-client` 15.1.3, which npm marks deprecated in favour of this package (the same library under the Prometheus organisation, the same API). `/v1/metrics`. |
| @opentelemetry/sdk-node | 0.223.0 | planned (M12) | |
| vitest, @vitest/coverage-v8 | 5.0.3 | locked | |
| vite | 8.3.3 | locked | Required peer of vitest. 8.3.4 was published on the verification day and is held back by `minimumReleaseAge`. |
| eslint, @eslint/js | 10.12.0, 10.0.1 | locked | |
| typescript-eslint | 8.71.1 | locked | |
| eslint-config-prettier, prettier | 10.1.8, 3.9.9 | locked | |
| globals, @types/node | 17.13.0, 24.19.1 | locked | `@types/node` follows the Node major. |
| better-sqlite3 | 13.0.3 | locked (`engine-firefox`) | Direct dependency since M5 at the version camoufox-js already resolved: the engine reads the launcher's WebGL table with it to draw a persona's graphics card from its seed. MIT. Ships its prebuilt binaries inside the package; no install script runs. |
| ua-parser-js (override) | 1.0.41 | locked (`overrides` in `sidecar/pnpm-workspace.yaml`) | The MIT line in place of the AGPL 2.x camoufox-js asks for (ADR 0008). Never loaded by the engine; a test asserts it. With it the launcher's own `launchOptions` reads a macOS User-Agent as Linux: re-verify on every camoufox-js bump. |

### The API test client (`tests/api-client`)

A pnpm project of its own with its own lockfile, so that nothing here can
move `sidecar/pnpm-lock.yaml`: resolving these packages inside the sidecar
workspace changed how `debug` links in the engines' own tree. Nothing of
it is in an image.

| Package | Version | State | Notes |
|---|---|---|---|
| @seriousme/openapi-schema-validator | 2.11.0 | locked | Validates `crates/kage-api/openapi.json` against the JSON Schema of the OpenAPI 3.1 specification: the validator that is not Kage's (M8 acceptance). MIT. Brings `ajv` 8.20.0, `ajv-draft-04`, `ajv-formats`, `yaml`. |
| openapi-typescript | 7.13.0 | locked | Generates `src/schema.ts` from the document. MIT. Brings `@redocly/openapi-core` 1.34.20 and, through `js-yaml`, `argparse` 2.0.1 (Python-2.0, allowed in `tests/api-client/license-policy.json` only). Peer `typescript ^5`. |
| openapi-fetch | 0.17.0 | locked | The runtime of the generated client: `fetch` typed by the generated paths. MIT, 6 kB, one dependency (`openapi-typescript-helpers`, types only). |
| typescript, @types/node, prettier | 5.9.3, 24.19.1, 3.9.9 | locked | The sidecar's versions. No eslint and no vitest here: the client is type-checked (`strict`), formatted, and tested with `node --test`. |

## Toolchain

| Tool | Version | Pinned in |
|---|---|---|
| Rust | 1.99.0 (2026-09-28), edition 2024 | `rust-toolchain.toml`, builder tag in `deploy/docker/kage.Dockerfile` |
| just | 1.58.0 | `.github/actions/setup-just/action.yml` |
| cargo-deny | 0.20.2 | `justfile` |
| cargo-audit | 0.22.2 | `justfile` |
| cargo-llvm-cov | 0.9.1 | `justfile` |
| cargo-cyclonedx | 0.5.9 | `justfile` |
| cargo-mutants | 27.1.0 | `justfile` |
| sqlx-cli | 0.9.0 | `justfile` (`setup-sqlx`). Built from source with `--no-default-features --features rustls,postgres`, so the tool does not link OpenSSL either. Same version as the `sqlx` crate. |
| cargo-auditable | 0.7.7 | `deploy/docker/kage.Dockerfile` |
| Docker Engine / Compose | 27+ / v2 or newer | Verified with Engine 29.8.2 and Compose v5.6.0. |
| PostgreSQL | 17.11 | `deploy/compose.yaml` |
| gitleaks | v8.30.1 | `justfile` (image digest) |
| trivy | 0.75.0 | `justfile` (image digest) |
| syft | v1.54.1 | `justfile` (image digest) |

## Container images

Every image is pinned by tag and digest.

| Image | Digest | Used by |
|---|---|---|
| `rust:1.99.0-trixie` | `sha256:6ff07edce8775d0f64be7aba9197229407301bddf2054d62c27b541a6238a181` | kage builder |
| `gcr.io/distroless/cc-debian13:nonroot` | `sha256:e792ab3d241a468a4fd7519ddbbebe66b49b5f365771716ea688ad40b6c6f1c2` | kage runtime |
| `node:24.21.0-trixie-slim` | `sha256:173f125896c3b47ddf056734c7ea789d04595a6a08769a8f78e0df642781fb66` | sidecar build and runtime |
| `postgres:17.11-trixie` | `sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3` | compose |
| `ghcr.io/gitleaks/gitleaks:v8.30.1` | `sha256:c00b6bd0aeb3071cbcb79009cb16a60dd9e0a7c60e2be9ab65d25e6bc8abbb7f` | `just secrets` |
| `ghcr.io/aquasecurity/trivy:0.75.0` | `sha256:af6acf9a6b85dfe389a1941505c0ce9efef52a4719635e1a962f022a3d855daa` | `just scan-images` |
| `ghcr.io/anchore/syft:v1.54.1` | `sha256:3eb5379ba7b409c3f4069b686110527af0c47df993fa5c10d13e7cf34f49b1aa` | `just sbom`, `just sbom-images` |

## GitHub Actions

Pinned by commit SHA in `.github/`.

| Action | Version | Commit |
|---|---|---|
| actions/checkout | v7.0.1 | `3d3c42e5aac5ba805825da76410c181273ba90b1` |
| actions/setup-node | v7.1.0 | `949feb2413d6458794dcd2491c4babbbce0c15c1` |
| actions/upload-artifact | v7.0.2 | `cf430e030ddbb5b0abf93d22962f4752f3646cd9` |
| actions/download-artifact | v8.0.2 | `9000827ccba6bdab643e8b6fd33ac0654aef8333` |
| pnpm/action-setup | v6.1.0 | `ea17c68df8912ef543352723c149a84f56e3d413` |
| Swatinem/rust-cache | v2.9.2 | `6323deb102c322ba6fcbdcafc7e3dddab59af2b6` |
| taiki-e/install-action | v2.87.26 | `f7e5d7c961414b23f5b25b2da9294395d08513ad` |
| docker/setup-buildx-action | v4.4.1 | `f87e5991a6d7451dcb8d9637bfbc97413f497069` |
| docker/login-action | v4.6.0 | `dbcb813823bdd20940b903addbd779551569679f` |
| docker/metadata-action | v6.2.0 | `dc802804100637a589fabce1cb79ff13a1411302` |
| docker/build-push-action | v7.4.0 | `c3c9e263c25d99ce0380d002d59b67737d91b0dc` |
| anchore/sbom-action | v0.24.3 | `66cbf4bc1f1c0d2edc94016e65bc221b6bb0ad6c` |

## Verification log

| Date | Who | What changed |
|---|---|---|
| 2026-10-08 | spec author | Initial pins from registry lookups. |
| 2026-10-08 | M0 session | Re-verified every pin. Confirmed wreq 0.16.1, wreq-util 0.2.0, patchright 1.63.0, camoufox-js 0.12.1, axum 0.8.9, sqlx 0.9.0, utoipa 6.0.0. Resolved the open ones: playwright-core 1.62.1, utoipa-axum 0.3.0, utoipa-swagger-ui 10.0.1, Rust 1.99.0, Node 24.21.0, pnpm 10.34.6, TypeScript 5.9.3 and every "latest at M0" entry. Two "latest" pins are held one release back because the full pin set does not resolve otherwise: testcontainers 0.27.3 and html5ever 0.39.0. Added the State column, tool, image and action pins. Newer majors left alone on purpose: TypeScript 6 and 7, pnpm 11 and 12, playwright-core 1.63 and 1.64 (camoufox-js peer range), Node 26 (not LTS). Two findings for the owner: the Camoufox browser pin does not fit playwright-core 1.62.1 (section above), and camoufox-js brings an AGPL-3.0-or-later dependency (ADR 0008). |
| 2026-10-08 | M1 session | First use of figment, notify, url, ipnet, sha2, rand, secrecy, uuid, chrono, testcontainers and testcontainers-modules: moved from pinned to locked. Three new pins, each the version already in the lockfile or the newest on crates.io: regex 1.13.1, flate2 1.1.10, tempfile 3.27.0 (test only). `rand` now has default features off in the workspace manifest. New tool pin: sqlx-cli 0.9.0. `deny.toml` allows CC0-1.0 for notify. No fingerprint-critical pin changed. `cargo deny`, `cargo audit` and the `openssl-sys` check pass on the new graph. |
| 2026-10-09 | M2 session | No pin moved and no fingerprint-critical version changed. First use of wreq and wreq-util outside the `kage-http` scaffold (`kage-proxy`), with new features on both: wreq `gzip`, `brotli`, `deflate`, `zstd`; wreq-util `emulation-compression`, `emulation-serde`. They change what an emulated client sends (`Accept-Encoding` is now present, as in the real browser), which M3 must keep in mind when it records fingerprint fixtures. First use of async-trait, metrics and wiremock: moved from pinned to locked. Three new pins, each the newest on crates.io: chacha20poly1305 0.11.0, zeroize 1.9.1 (already locked through secrecy), proptest 1.11.0 (test only). `Cargo.lock` grew from 427 to 454 packages. `cargo deny`, `cargo audit` and the `openssl-sys` check pass on the new graph. |
| 2026-10-09 | M3 session | No pin moved and no fingerprint-critical version changed. wreq gained the feature `cookies` in `kage-http` (the per-request cookie hook; wreq's own jar is not used). `wreq-proto` 0.2.6, the protocol crate under wreq, is now named directly for the header-order hook and is listed with the fingerprint-critical set because it moves with wreq. `kage-proxy` no longer depends on wreq-util. New direct dependencies at the versions already locked: http 1.5.0, bytes 1.12.1, futures-util 0.3.34, rustls 0.23.45, tokio-rustls 0.26.6, webpki-root-certs 1.0.9; http-body-util is no longer test only. New pins: psl 2.1.239 and rcgen 0.14.10 (test only), both verified on crates.io. `Cargo.lock` grew from 454 to 470 packages. The fingerprints of the 48 mapped profiles (Chrome 142 to 149 and Firefox 144 to 151 on three operating systems) were recorded against the in-repo echo with this pin set; Chrome and Firefox each show one JA4 and one HTTP/2 fingerprint across their eight majors. `cargo deny`, `cargo audit` and the `openssl-sys` check pass on the new graph. |
| 2026-10-09 | M4 session | No existing pin moved. Fingerprint-critical additions: Chrome stable 155.0.8059.39 by version and package checksum (was "planned, by image digest"), `fingerprint-generator` 2.1.88 as a direct dependency and `fingerprint-injector` 2.1.88 (both already the planned versions). `prom-client` was not taken: it is deprecated on npm; `@prometheus-io/client` 0.16.1 is its successor. pino 10.4.0 became a direct dependency at the locked version. `kage-browser` uses crates that were already locked (wreq, serde, serde_json, chrono, uuid, url, thiserror, tracing, tokio; wiremock for tests) and adds none; its types are generated by its own build script, so no code generator joined the tree. New non-registry pins: the moby/profiles commit the seccomp profile is derived from, and the Debian packages of the Chromium image (recorded per build). Camoufox stays as it was (M5). |
| 2026-10-09 | M5 session | Fingerprint-critical: the Camoufox browser build moved from the planned v152.0.4-beta.27 to **v152.0.4-beta.30** (D29), now locked by URL and checksum; camoufox-js 0.12.1 and playwright-core 1.62.1 are in use for the first time and unchanged. Found: the launcher's own download would install a beta.31 that upstream marks as not to be installed, so it is never used. `ua-parser-js` overridden to 1.0.41 (MIT) and the license exception removed (ADR 0008). `better-sqlite3` 13.0.3 became a direct dependency of `engine-firefox` at the locked version. No Rust dependency was added. New in the images: `x11-xkb-utils` and PulseAudio in both, GTK 3 and Firefox's libraries in the Firefox one. Captured against the echo: Camoufox 152.0.4 and, for comparison, a stock Firefox 152.0.4; both show JA4 `t13d1617h2_86a278354501_3cbfd9057e0d`, one cipher suite fewer than `firefox_151`, so Firefox 152 is browser-only. `just deny`, `pnpm audit` and the license policy pass on the new tree. |
| 2026-10-10 | M5.1 session | No pin moved and no dependency was added. Fingerprint-critical: the pinned Camoufox build now runs with upstream's Juggler fix backported into its `omni.ja` (ADR 0010); the archive's checksum before and after is pinned, the engine refuses any other. Found: Camoufox pull request 779 fixed it upstream for the v156 builds, which camoufox-js 0.12.1 does not accept. patchright 1.63.0 is unchanged; the Chromium engine no longer uses its evaluation for reading pages. |
| 2026-10-10 | M6 session | No existing pin moved and no fingerprint-critical version changed. One new crate: `chrono-tz` 0.10.4 (`kage-core`), with `phf` 0.12.1, `phf_shared` 0.12.1 and `siphasher` 1.0.4 behind it; `cargo deny` and the openssl-sys check pass with it. The new crate `kage-persona` uses only crates that were already locked (`async-trait`, `wiremock`, `testcontainers`, `tempfile`, `rcgen`, `axum` and `http` as test dependencies at their pinned versions). No npm dependency changed: the `navigator.oscpu` fix in `engine-firefox` is in Kage's own generation code. |
| 2026-10-10 | M7 session | No pin moved, no fingerprint-critical version changed, no new crate in the lockfile. `hmac` 0.13.0 moved from pinned to locked with its first use: the new crate `kage-scheduler` derives the token a persona's clients present to the forwarder with it (HMAC-SHA-256 under a key derived from the master key). `kage-scheduler` otherwise uses crates that were already locked (`async-trait`, `bytes`, `chrono`, `http`, `metrics`, `serde`, `serde_json`, `sha2`, `thiserror`, `tokio`, `tracing`, `url`, `uuid`; in tests `sqlx`, `tempfile`, `testcontainers`, `wiremock`), and the `kage` binary names `uuid` as a direct dependency. The forwarder is plain `tokio` sockets: no HTTP or TLS library was added, and it terminates no TLS. `cargo deny` and the openssl-sys check pass. |
| 2026-10-11 | M8 session | No existing pin moved and no fingerprint-critical version changed; `sidecar/pnpm-lock.yaml` is untouched. First uses in `kage-api`: `governor` 0.10.4 (default features off, `std`), `metrics-exporter-prometheus` 0.18.3 (default features off) and `tower-http` 0.7.1 (`catch-panic`, `cors`), each moved from pinned to locked; with them 18 crates entered `Cargo.lock`, all behind the exporter and governor (`metrics-util`, `quanta`, `evmap`, `left-right`, `loom` under `cfg(loom)` only, and others, listed with their rows). Direct dependencies at versions that were locked already: `hyper-util` 0.1.21, `base64` 0.23.1, `subtle` 2.6.1, `getrandom` 0.4.3. `hickory-resolver` stays pinned and unused: the host-side lookup of the SSRF guard asks the system's resolver. New, outside the sidecar workspace: `tests/api-client` with `@seriousme/openapi-schema-validator` 2.11.0, `openapi-typescript` 7.13.0 and `openapi-fetch` 0.17.0, its own lockfile and its own license policy (the sidecar's allowlist and Python-2.0 for `argparse`, a build-time dependency of the generator). |
| 2026-10-11 | M9 session | No pin moved, no fingerprint-critical version changed, and no crate or package entered a lockfile; `sidecar/pnpm-lock.yaml` and `tests/api-client/pnpm-lock.yaml` are untouched. First use of `wreq` 0.16.1 in `kage-api`, at the locked version: the webhook sender, a plain client without emulation and without a proxy (`no_proxy`, no redirects, `resolve_to_addrs` to the addresses it checked itself), which is the only outbound request of the API crate; `Cargo.lock` gained that one dependency edge. The sidecar contract is at 1.2.0 (additive: `FlowBehavior.pace`, `FlowStepEvent.paced_ms`); `kage-browser` generates its types from the committed document as before. `hickory-resolver` stays pinned and unused: the webhook sender resolves a receiver with the system's resolver, like the host-side lookup of the SSRF guard (follow-up F37 may remove the pin in M12). |
| 2026-10-12 | M10 session | No existing pin moved and no fingerprint-critical version changed; `sidecar/pnpm-lock.yaml` and `tests/api-client/pnpm-lock.yaml` are untouched. First uses, both in the new logic of `kage-crawl`: `scraper` 0.27.0 (with `html5ever` 0.39.0 behind it) and `quick-xml` 0.42.0, at the versions pinned in M0; `Cargo.lock` grew from 497 to 525 packages, all of them behind `scraper`. `psl`, `regex`, `flate2` and `sha2` gained `kage-crawl` as a user and `proptest` gained `kage-core` (test only), each at its locked version. The sidecar contract moved to 1.3.0, additive (`behavior.patience_ms`, an entry from the open page, `capture.links`); `just contract-check` passes against the base. `cargo deny` fails on main since RUSTSEC-2024-0449 was published against `unsafe-libyaml`: ignored in `deny.toml` with a reason and a review date (2026-12-31), follow-up F45. Verified: `cargo deny check`, `scripts/check-no-openssl.sh`, both license checks. |
