# Runbook: the Firefox engine

How the Firefox sidecar makes a persona, what was measured before it was
allowed to claim Windows and macOS, what its container needs, how to move
it to a new Camoufox build, and what a replacement of the engine must
implement. Spec §10.3 to §10.5 and ADR 0008 are the contract; this page is
the practice. The Chromium engine's page is `engine-chromium.md`; what both
engines share (the self-test, captures, the behavior engine) is described
there or below and holds for both.

## What a persona is made of

A Firefox persona is Camoufox, a Firefox whose fingerprint is set inside
the browser's own code, headful on its own X display, with a persistent
profile and an authenticated proxy. No script of Kage's runs in any page.

| What a page can read | Where it comes from |
|---|---|
| operating system, `User-Agent`, `platform`, `oscpu`, `appVersion` | the persisted fingerprint: Windows or macOS (spec §6.1); Linux only when configured |
| screen, work area, window size and position | the fingerprint, a real device's values with its quirks (a taskbar, a menu bar, a maximized window a few pixels larger than its screen, a second monitor) |
| time zone, languages, `Accept-Language`, `Intl` locale, position | the open request, always from Kage; nothing is looked up from the proxy's address |
| `hardwareConcurrency` | the fingerprint; the browser also runs under a CPU mask of that size, at most the host's |
| WebGL vendor, renderer, parameters, extensions | a graphics card of the claimed system, drawn from the launcher's table by the persona's seed |
| fonts | the claimed system's own font files, which the build ships, through a fontconfig file Kage writes per persona |
| canvas, audio and text-metric variation | the build's own, from three seeds drawn from the persona's seed: one stable device per persona, twenty devices for twenty personas |
| media devices, `AudioContext` | one microphone, a camera on two personas in three, 48 or 44.1 kHz stereo: what a Firefox lists before any permission |
| whether sound may start by itself (`navigator.getAutoplayPolicy()`, an `AudioContext` before any input) | Firefox's own default, set by the launch plan: `allowed-muted` for media, `disallowed` for an `AudioContext`. The build's own default says `allowed` |
| cookies, storage, history | the persona's profile directory |
| WebRTC | host candidates as names (`<uuid>.local`), nothing from a STUN server: no packet leaves outside the proxy |

What stays the image's whatever the persona claims, recorded in the
fingerprint as `host_bound`: pixels are drawn by Mesa (`webgl_rendering`)
and text is rasterized by FreeType (`text_rendering`).

### How it is launched, and why not by the launcher

The launcher, camoufox-js, supplies the parts: real-device fingerprints
(the same generator the Chromium engine uses), the mapping from a
fingerprint to the browser's configuration, the font lists and the WebGL
table. Kage does not call its `launchOptions`:

- It draws six random values on every call and is asynchronous, so the
  seeded swap that makes generation reproducible cannot wrap it. A persona
  opened twice would be two devices.
- With the pinned build it points fontconfig at a directory that does not
  exist (`fontconfig/win`; the build has `fontconfig/windows`). No
  fontconfig file is read then, and `serif`, `sans-serif` and `monospace`
  all become one font, which no browser shows.
- It decides the claimed system by parsing the `User-Agent` with
  ua-parser-js; see ADR 0008 and "The ua-parser-js override" below.
- By default it downloads uBlock Origin from addons.mozilla.org at the
  first launch, from the sidecar's own address, and loads it.

Instead, `POST /v1/personas/generate` builds the complete configuration
once (`generateFingerprint`, engine-firefox) and stores it in the
fingerprint document, and every open is a pure function of that document
and the request (`launchPlan`): the `CAMOU_CONFIG_<n>` environment
variables, a few preferences, the proxy, and the persona's fontconfig file.

The fontconfig file is the build's own for the claimed system with two
changes: the font directory is absolute, and for Windows and macOS claims
hinting is off, because those systems lay text out with the fonts'
fractional advances and whole-pixel advances are FreeType's signature. Not
verified against a real Windows Firefox: that comparison belongs to the
Windows engine-host milestone (follow-up F14 of the plan).

Four more settings differ from the launcher's or the build's defaults,
each for a measured reason: no addon is loaded; the build's own window
theming is off (`disableTheming`: with it the toolbar is 51 px high, a
stock Firefox has 85, and a page reads the difference as
`outerHeight - innerHeight`); `history.length` and the size of the page
area are never set, they are the tab's and the window's own; and sound
waits for the user (`media.autoplay.default` is 1, Firefox's default).

The last one was found in M5.1. The build's `camoufox.cfg` sets
`media.autoplay.default` to 0, "allow autoplay". Measured with nothing
driving the browsers (each started with an address on its command line,
with a sound server): on an untouched page a stock Mozilla Firefox 152.0.4
answers `navigator.getAutoplayPolicy("mediaelement")` with `allowed-muted`
and `("audiocontext")` with `disallowed` and keeps an `AudioContext`
suspended; the build as it ships answers `allowed` twice and lets an
`AudioContext` run 300 ms after `load`. A person can choose that in
Firefox's settings, so it is no impossible value, but it is not what a
Firefox says that nobody configured, and every Firefox persona would say
it. With Firefox's default in the profile's preferences a persona
answers as the stock browser does in every value measured, and a click
through the display lets an `AudioContext` run. The parity suite asserts
the answer (it needs no sound device) and the probe warns for it
(`automation.audio_autoplay`).

### What was measured (M5, 2026-10-09)

Camoufox v152.0.4-beta.30 on Linux, claiming Windows and claiming macOS,
against the coherence probe (`tests/fixtures/site/probe/`):

- `userAgent`, `platform`, `appVersion`, languages, `Intl` locale, time
  zone and `hardwareConcurrency` are equal in the window, a same-origin, a
  `srcdoc` and an `about:blank` iframe, a dedicated worker and a blob
  worker; `oscpu` and `buildID` exist in windows only, as in every Firefox.
- `User-Agent` and `Accept-Language` are the claimed ones on the
  navigation, an iframe navigation, a `fetch()`, the worker script request
  and a worker `fetch()`.
- WebGL vendor, renderer, version, shading language, `MAX_TEXTURE_SIZE`
  and the extension count are equal in the window and in an
  `OffscreenCanvas` of both workers, and name the claimed system.
- The font families found by measuring text are equal in every scope,
  workers included, are the claimed system's (Segoe UI, Calibri, Consolas
  …; Helvetica Neue, Menlo, Lucida Grande …) and include no Linux family.
- `navigator.webdriver` is false, getters are native, the console is not
  read in a way a page can notice.
- TLS, HTTP/2 and the header order are those of a stock Mozilla Firefox
  152.0.4 measured against the same echo (`docs/runbooks/fingerprint-echo.md`).
- One fingerprint opened twice shows the same canvas pixels, audio hash,
  fonts, WebGL and geometry.

The parity suite keeps all of it (`engine parity: firefox`, "passes the
coherence probe …", "shows the same browser on every open …").

A Linux-claiming Firefox persona is possible but not in the default mix:
the build's Linux fonts are Tor Browser's bundle (Arimo, Tinos, Cousine,
Noto per script) and nothing a Linux desktop ships, so the persona looks
like Tor Browser, not like Firefox on Ubuntu.

### What the generator's data is like

The fingerprint and the graphics card are drawn from the launcher's tables
of real devices, and the tables have their own shape and their own noise.
Measured in M5 over 200 seeds per system:

- Every document is different, but the choice is narrow: 6 screen sizes
  for Windows (1376x774 for 38 % of the personas, 1920x1080 for 31 %), 8
  for macOS, 5 to 7 graphics cards per system.
- 9 % of the Windows personas and 11 % of the macOS ones have a work area
  as large as their screen (no taskbar, no menu bar). The probe reports
  those as a warning (`environment.work_area`), never as a failure: such
  machines exist.
- The graphics-card table offers a Direct3D renderer for macOS (0.85 % of
  the draws) and a SwiftShader string for Windows (0.27 %). Direct3D exists
  on Windows only, and SwiftShader is Chrome's software fallback, nothing a
  Firefox reports. The engine never draws a renderer that names another
  system or that is a program (`plausibleRenderer`, engine-firefox).

M6 decides whether a pool of twenty personas needs more variety than this
(a screen constraint per persona is the lever that exists).

### The Juggler backport (ADR 0010)

The driver inside the pinned build (Juggler, in
`chrome/juggler/content/content/Runtime.js` of the build's `omni.ja`) runs
every evaluation of a page as user input: `setHandlingUserInput(true)` and
`notifyUserGestureActivation()`. Every navigation reads the page at least
once, so a page nobody touched read `navigator.userActivation.isActive`
and `hasBeenActive` as true about 300 ms after `load`, with no input event
before it (measured in M5). Upstream took both calls out in Camoufox pull
request 779; the builds from `v156.0.1-beta.32` on have the fix, the
pinned Firefox 152 build does not.

Kage applies the same change to the pinned build:

```bash
node scripts/patch-camoufox-juggler.mjs /opt/camoufox \
    --from <CAMOUFOX_OMNI_SHA256> --to <CAMOUFOX_OMNI_PATCHED_SHA256>
```

The image build and `just setup-camoufox` run it with the two checksums of
`deploy/docker/sidecar.Dockerfile`. The file is stored uncompressed in the
archive, so the four statements are overwritten by statements of the same
length that do nothing and the entry's checksum is rewritten: nothing is
repacked, 260 bytes differ, and the tool refuses any archive but the
pinned one. Measured on the result: either call alone still grants
activation, both gone grant none; an untouched page is not active at
`load`, 300 ms, 1.5 s and 4 s later; a click through the display
activates it from `mousedown` on, as for a person; the parity suite is
green.

Two rules follow from it, and their messages are what an operator sees:

- **The engine runs the patched archive only.** At start it hashes
  `omni.ja`; anything else answers `browser_launch_failed`, "does not hold
  the Juggler backport". Run `just setup-camoufox`, or rebuild the image.
- **A profile is told when the archive changed.** Firefox keeps the
  compiled scripts of its own code in the profile (`startupCache/`) and
  trusts them while version and install directory are the same. Measured:
  a profile first opened under the unpatched build went on granting
  activation after the build was patched in place, on every open. Each
  profile directory holds a note of the archive it last started under
  (`omni.sha256`, next to `user-data`); when it differs, the engine
  removes that profile's `startupCache` before the launch and logs it.
  The first start after that compiles the scripts again.

What the backport leaves out: upstream's switch that lets an evaluation
open a window without activation (it needs the browser's C++). Kage opens
no window through an evaluation.

The probe's check for all of this, and the cases it must not be tightened
to, are in `engine-chromium.md` ("The probe's check").

### Known tells (not solved here)

- No speech synthesis voices (`speechSynthesis.getVoices()` is empty); a
  Windows or macOS Firefox has the system's.
- Pixels come from Mesa and glyph shapes from FreeType under a Windows or
  macOS claim. Canvas and WebGL image hashes are therefore not those of
  any real Windows machine; the build's seeded variation makes them one
  unknown device per persona instead of one known Linux value.
- A printed PDF is always A4, portrait, with backgrounds (below).
- Every PNG a canvas exports carries a 16-byte `deBG` chunk that differs
  per browser session. A stock Firefox 152 writes it too: it is Firefox's
  own, not a mark of this build.

## Input: the display's mouse and keyboard

Neither engine sends human input through its driver. The behavior engine
(engine-core, spec §10.4) uses the XTEST extension of the context's own X
display, so the browser receives a mouse and a keyboard the way it does
from hardware. Measured on a page that records every event: input the
Firefox driver synthesizes has an empty `pointerType` on every pointer
event; both drivers scroll in one instant jump with one wheel event of the
requested size, type capitals without a `Shift` key, and hold every key
exactly as long as the pause to the next. Through the display none of that
shows: `pointerType` is `mouse`, a wheel notch is the browser's own (114 px
in Firefox, 120 in Chrome) with its smooth scrolling, modifiers are keys.

- The keyboard layout of the display is the persona's: `setxkbmap` with
  the layout of the request's locale (`de`, `at`, `fr`, `gb`, `it`, …), so
  `KeyboardEvent.code` relates to `key` as on that country's keyboard. A
  character the layout cannot produce is typed through a spare key that
  is mapped to it for the moment.
- A persona has a motor profile, drawn once from its seed
  (`motor-profile-v1`): how fast it aims (Fitts' law), where its speed
  peaks, to which side its paths bend, how often it overshoots, how long
  it rests before a click, its typing cadence, reading speed and dwell.
  A request has its own stream from the persona's seed and the request's;
  the result returns that seed and the profile's id (`behavior.seed`,
  `behavior.profile_id`), with which a run can be replayed.
- A navigation without `behavior` sends no input at all. Kage's policy
  must ask for human behavior on a browser render (`dwell: "human"`, a
  scroll depth, wander): a page load with no scroll and no dwell is not a
  person's.
- Flow steps with `human: false` are done by the driver and are visible to
  a page as such; they are for pages known not to look. One more way to
  see it since the Juggler backport: at the `mousedown` of a driver's
  click the page still reads `isActive` as false, and as true at `click`;
  for a click through the display it is true at `mousedown`. And an
  `AudioContext` made after a driver's click stays suspended, where one
  made after a real click runs.

Entry strategies (`behavior.entry`): `none` and `direct` are a typed
navigation, with no `Referer`. `link` loads `from_url` as a typed
navigation, finds the link to the target and clicks it. `search_engine`
does the same on a results page and clicks the first result that leads to
the target's host. The `Referer` is whatever the browser sends for the
click; none is ever forged. No link or result: the outcome says so and
nothing else is loaded, unless the request asked for `fallback: "none"`. A
wall on the page to start from is `blocked`; nothing is typed into it. No
search template is enabled by default: a search visit is made from the
persona's address, costs about 0.5 to 2 MB, leaves address and query in
the search engine's log, and Google answers automated-looking visits with
its own wall. Which engine to use is a policy decision for field
validation (M13).

## Captures on Firefox

| Capture | How | A page can notice |
|---|---|---|
| DOM, title, text, storage | read in the driver's isolated world | nothing: with the Juggler backport a read grants no user activation |
| raw body, headers, redirects, XHR and fetch bodies | network events | nothing |
| console | the page's `console.*` calls and uncaught errors, reported by the browser to its driver | nothing (the probe's console check stays at zero) |
| screenshot, viewport or full page | drawn from the browser side | nothing, also for a full page |
| screenshot as WebP | Firefox encodes PNG and JPEG only: a PNG is written and the artifact says `image/png` | nothing |
| PDF | `Ctrl+P` on the display's keyboard; the profile prints silently into a file | `beforeprint`, `afterprint`, and the two keys of the shortcut, as when a person prints |

Paper size and orientation of a PDF are the profile's (A4, portrait,
backgrounds on): Firefox takes them from preferences, not per print, so
the request's `format` and `landscape` are not honored on this engine.

## Settings

| Variable (compose) | Sidecar variable | Default | Meaning |
|---|---|---|---|
| `KAGE_SIDECAR_TOKEN` | `SIDECAR_TOKEN` or `SIDECAR_TOKEN_FILE` | none, required | The shared secret. |
| `KAGE_SIDECAR_MAX_CONTEXTS` | `SIDECAR_MAX_CONTEXTS` | 3 | Contexts, and browsers, at once. |
| `KAGE_SIDECAR_CONTEXT_IDLE_MS` | `SIDECAR_CONTEXT_IDLE_MS` | 120000 | An unused context is closed after this. |
| `KAGE_SIDECAR_MAX_PAGES_PER_BROWSER` | `SIDECAR_MAX_PAGES_PER_BROWSER` | 500 | A browser is restarted after this many navigations. |
| `KAGE_SIDECAR_AUDIO` | `SIDECAR_AUDIO` | `pulse` in the images, `none` elsewhere | The sidecar runs a PulseAudio with one output and one input that go nowhere, so that browsers find devices. Both engines. |
| | `SIDECAR_CAMOUFOX_DIR` | `/opt/camoufox` | Where the pinned build is unpacked. |
| `KAGE_SIDECAR_SELFTEST_PROBE_URL`, `…_ECHO_URL` | the same with `SIDECAR_` | unset | The self-test's own pages. |

On the kage side: `sidecar.firefox_url`, and `engines.firefox.browser_major:
152`. Firefox 152 is browser-only on the http path (its TLS offers one
cipher suite fewer than the newest profile, `firefox_151`): a Firefox
persona has no `wreq_profile` until wreq-util ships a matching one.

The container: user 65532, read-only root filesystem, every capability
dropped, `no-new-privileges`, Docker's default seccomp profile, writable
`/tmp`, the personas and the artifacts volume, 1 GB of `/dev/shm`. The
stack smoke test checks it on a running container with a context open
(`tests/e2e/firefox-hardening.mjs`).

**The Firefox image is not published.** The Camoufox build bundles
Microsoft's and Apple's font files; they are what makes the font check
pass, and a public image of ours would redistribute them. Build it where
it runs: `just build-images`. (This is the project's default, not legal
advice; it is one matrix entry in `.github/workflows/release.yaml`.)

## The ua-parser-js override (ADR 0008)

camoufox-js asks for ua-parser-js 2.x, which is AGPL-3.0-or-later. The
workspace overrides it to the MIT 1.0.x line
(`sidecar/pnpm-workspace.yaml`). One function of the launcher behaves
differently with it: `launchOptions` asks whether the parsed system name
starts with `macOS`; the 1.x line says `Mac OS`, so a macOS persona would
be taken for a Linux one (Linux fonts, Linux WebGL). Kage never calls that
function, and the engine never loads the package at all:
`engine-firefox/src/index.test.ts` ("ua-parser-js (ADR 0008)") generates
and plans all three systems and then asserts that the module was never
loaded, and pins the differing answer. **Anyone who calls `launchOptions`
in this tree gets macOS wrong.** Re-run that test and read the launcher's
`utils.js` on every camoufox-js bump.

## Moving to a new Camoufox build

A bump is a dedicated change (`docs/versions.md`, fingerprint-critical).
The launcher accepts a range of builds and its own `fetch` installs the
newest in it, which at the time of M5 was a build attached to a release
that says "nothing here is meant to be installed". Kage therefore never
uses that fetch.

1. Take the version, the release and the SHA-256 of the Linux x86_64 zip
   from the build's GitHub release: `CAMOUFOX_VERSION`, `CAMOUFOX_RELEASE`,
   `CAMOUFOX_SHA256` in `deploy/docker/sidecar.Dockerfile`, and the two
   constants in `engine-firefox/src/host.ts`. The image build and
   `scripts/setup-camoufox.sh` read the Dockerfile lines; the engine
   refuses any build but the one its constants name.
2. The Juggler backport belongs to one build. `just setup-camoufox` stops
   at the new archive ("neither the pinned build's archive nor the patched
   one"), which is the reminder. A build from `v156.0.1-beta.32` on has
   the fix itself: delete `scripts/patch-camoufox-juggler.mjs`, its two
   `CAMOUFOX_OMNI_*` lines in the Dockerfile, its steps in the Dockerfile
   and in `scripts/setup-camoufox.sh`, and set `CAMOUFOX_OMNI_SHA256` in
   `engine-firefox/src/host.ts` to the new build's own `omni.ja` (the
   engine keeps refusing any other archive). The parity test "reads a
   page nobody touched without granting it user activation" then says
   whether the build keeps the promise by itself.
3. `just setup-camoufox`, then `just test-node`. The pinned-seed test
   shows what changed in generated fingerprints; the parity suite re-checks
   the coherence bar, the captures and the input path on the new build.
   Look at `fontconfig/*/fonts.conf` and `properties.json` of the new
   build: `personaFontconfig` and `validateConfig` fail loudly when they
   no longer fit.
4. Read the build's own preference defaults (`camoufox.cfg` in the build
   directory, a few hundred `defaultPref` lines) for values that differ
   from a stock Firefox and that a page can read. They are the build's
   choices, not the persona's, so every Firefox persona shares them: a
   tell for the whole pool. The example is `media.autoplay.default`
   (above): the build says 0, Firefox ships 1, a page reads it with
   `navigator.getAutoplayPolicy()`. Check that this line is still what the
   launch plan corrects, and measure anything new against a stock Firefox
   of the same version that nothing drives; a preference set in
   `basePreferences` (engine-firefox, `launch-plan.ts`) with its reason is
   the remedy.
5. `just capture-browser-fingerprint firefox`, then "A new browser major"
   in `docs/runbooks/fingerprint-echo.md`.
6. Set `engines.firefox.browser_major` in the deployment.
7. Run the self-test against your own pages, and the live suite once with
   `KAGE_LIVE_RECORD=1` after reviewing its findings.
8. Update `docs/versions.md` and the changelog.

A persona's fingerprint document holds the browser's whole configuration
for the build it was generated with. After a bump to another Firefox major
the sidecar refuses the old document (`browser_major_mismatch`); generate
it again from its seed.

## Fallback: a Python engine behind the same contract (risk R1)

camoufox-js is labeled experimental. If it becomes unusable, the Firefox
engine is replaced by a Python service that uses the upstream `camoufox`
launcher and Playwright for Python. Kage and the Chromium sidecar do not
change: the replacement is a container that speaks the same HTTP contract,
`sidecar/packages/contract/openapi.json` (contract 1.1.0), on the same
port, with the same shared secret.

It must implement exactly these endpoints, with the schemas of that
document validated in both directions and every error as `{code, message}`
with the document's codes:

| Endpoint | What it must do |
|---|---|
| `GET /v1/health` | `engine: "firefox"`, the browser version, contexts in use, `contract_version`. |
| `GET /v1/metrics` | The metrics of spec §20 that start with `sidecar_`. |
| `POST /v1/personas/generate` | A fingerprint document from a 64-hex seed: the same seed and constraints give the same bytes. The document is opaque to Kage; it must hold everything needed to start the same browser again. |
| `POST /v1/contexts` | One browser per persona on its own X display, the persisted fingerprint applied, language, time zone and position from the request (`geoip` off), the proxy with credentials, loopback through the proxy, WebRTC confined, a persistent profile. Idempotent per persona; `context_conflict`, `context_limit`. |
| `DELETE /v1/contexts/{id}` | Close and flush the profile. |
| `POST /v1/contexts/{id}/navigate` | The page result of spec §10.2 with every capture, the `behavior` options and the entry strategies. |
| `POST /v1/contexts/{id}/flow` | The v1 actions of spec §10.5 as NDJSON: one `step` line per finished step, then one `final` line. |
| `POST /v1/contexts/{id}/state/export`, `…/import` | Cookies as rows of `http_cookies`, storage of the page's origin. |
| `POST /v1/contexts/{id}/selftest` | The page kinds `probe`, `echo`, `sannysoft`, `creepjs`, `generic`, judged as engine-core judges them. |

What a replacement must keep, because Kage relies on it and tests it:

- Determinism: generation from the seed only, and an open that generates
  nothing.
- Input through the display (XTEST), never through the driver, with the
  persona's keyboard layout; the upstream launcher's `humanize` stays off.
- The fontconfig rule above (the Python launcher sets its own path: check
  that generic families resolve to three different fonts).
- No addon, no theming of the build, no download at run time, and
  Firefox's own default for autoplay (the Python launcher starts the same
  build with the same `camoufox.cfg`).
- Reads that grant no user activation. Playwright for Python drives the
  same Juggler, so the same rule about the build holds: the backport above,
  or a build that has the fix. The probe fails hard otherwise
  (`automation.user_activation`).
- The parity suite's expectations. Its tests are TypeScript, but they only
  need an `Engine`; the simplest proof for a Python engine is to run them
  against it through a thin `Engine` that calls its HTTP endpoints.
- The license review of its own dependency tree (the Python launcher's is
  not the one reviewed here).
