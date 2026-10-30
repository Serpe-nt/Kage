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
| screen, window size and position | the fingerprint, a real device's values with its quirks (a maximized window a few pixels larger than its screen, a second monitor) |
| work area (`availWidth`, `availHeight`) | what Firefox's baseline fingerprinting protection reports of any screen, as a stock Firefox 156 does: the screen less a taskbar of the claimed system (48 px on Windows, 76 px on macOS). The 152 build showed the fingerprint's own |
| time zone, languages, `Accept-Language`, `Intl` locale, position | the open request, always from Kage; nothing is looked up from the proxy's address |
| the browser's own language (`Intl`'s default, form validation messages, the PDF viewer) | the persona's Firefox UI locale, requested at startup: the build carries all of Firefox's language packs |
| `hardwareConcurrency` | the fingerprint; the browser also runs under a CPU mask of that size, at most the host's |
| WebGL vendor, renderer, parameters, extensions | a graphics card of the claimed system, drawn from the launcher's table by the persona's seed |
| fonts | the claimed system's own font files, which the build ships, through a fontconfig file Kage writes per persona |
| audio variation | the build's own, from a seed drawn from the persona's seed: one stable device per persona |
| canvas pixels, text metrics | the host's (Mesa and FreeType), the same for every persona of a host: the 156 build no longer reads the per-persona canvas and font-spacing seeds (a known tell below) |
| media devices, `AudioContext` | one microphone, a camera on two personas in three, 48 or 44.1 kHz stereo: what a Firefox lists before any permission |
| whether sound may start by itself (`navigator.getAutoplayPolicy()`, an `AudioContext` before any input) | Firefox's own default: `allowed-muted` for media, `disallowed` for an `AudioContext`. The 156 build ships it, and the launch plan sets it as well, as a guard (the 152 build said `allowed`) |
| `navigator.doNotTrack`, `navigator.globalPrivacyControl`, `Sec-GPC` | Firefox's defaults (`"unspecified"`, `false`, no header): the engine never applies what the generator draws for them |
| WebGPU (`navigator.gpu`) | present on Windows claims and on macOS claims whose graphics card is Apple's, as in Firefox 156; `requestAdapter()` answers null, as on a machine whose GPU Firefox blocks |
| storage of a cross-site frame | partitioned, as in a stock Firefox (`network.cookie.cookieBehavior` 5; the 152 build had 4, which showed a frame its site's first-party cookies and storage) |
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
fingerprint document, and every open is a pure function of that document,
the request and the build (`launchPlan`): the `CAMOU_CONFIG_<n>`
environment variables with the configuration as far as the build reads it,
the `CAMOU_PREFS_<n>` variables with a few preferences, which the build's
`camoufox.cfg` applies at startup (the driver sets them again once the
browser runs), the proxy, and the persona's fontconfig file. What the
build does not read is left out: the keys its `properties.json` declares
`removed` (ten of the generator's: the canvas and font-spacing seeds and
offsets, `disableTheming`, the WebGL context attributes, and
`navigator.product`, `appCodeName` and `appName`), and the two privacy
signals (`NEVER_APPLIED`, `launch-plan.ts`), which the generator draws
for nearly every device and a stock Firefox sends only when its user turns
them on. A stored document keeps its bytes: what is left out is decided at
every open, so the next build's removals are handled the same way.

The fontconfig file is the build's own for the claimed system with two
changes: the font directory is absolute, and for Windows and macOS claims
hinting is off, because those systems lay text out with the fonts'
fractional advances and whole-pixel advances are FreeType's signature. Not
verified against a real Windows Firefox: that comparison belongs to the
Windows engine-host milestone (follow-up F14 of the plan).

More settings differ from the launcher's or the build's defaults, each
for a measured reason: no addon is loaded; `history.length` and the size
of the page area are never set, they are the tab's and the window's own;
sound waits for the user (`media.autoplay.default` is 1, Firefox's
default); the browser speaks the persona's language (below); WebGPU is on
where Firefox 156 has it; and the memory cache and compression
dictionaries (`browser.cache.memory.enable`,
`network.http.dictionaries.enable`) are on as in stock, where the build
turns them off. The 152 build also needed its window theming off (its
toolbar was 51 px high, a stock Firefox's 85, and a page reads
`outerHeight - innerHeight`); the 156 build's is 85 by itself.

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
(`automation.audio_autoplay`). The 156 build ships Firefox's 1 by
itself; the launch plan keeps setting it, as a guard, so that a later
build that changes it again is still answered as Firefox answers.

**The browser's language.** The 156 build carries all of Firefox's
language packs and reads `intl.locale.requested` at startup only. The
launch plan requests the persona's Firefox UI locale through
`CAMOU_PREFS` (`firefoxUiLocale`: the full tag where Firefox ships one,
`en-GB`, `pt-BR`, `es-MX`, `sv-SE`, …, else the language, `de` for
Germany and Austria; checked against the build's `res/multilocale.txt`,
`en-US` when the build has neither). Measured: set through the driver
alone, `Intl` moved but the browser's own messages stayed English; at
startup both are the persona's. A stock Firefox of a country's language on
a system of that country reports the regional tag (`de-DE`, `fr-FR`,
`it-IT`, `en-GB`), which is the persona's locale; the probe judges
`Intl`'s language against `navigator.language`'s
(`coherence.intl_language`, hard).

### What was measured (M5, 2026-10-09, on the 152 build)

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

### What was measured again (Camoufox 156, 2026-10-18)

Camoufox v156.0.1-beta.39, four personas (two claiming Windows, two
macOS), against the same probe and against a reference page that reads
about 80 values a page can read without input or permission, next to
Mozilla's own Firefox 156.0.1 and 152.0.4 that nothing drives:

- The coherence bar holds as on 152: 0 failures and 0 warnings in the
  window, the three kinds of iframe and both workers, for every persona.
- Four things a page could read of the 152 personas are gone, each now as
  in stock Firefox: a `new Worker("chrome://juggler/content/content/WorkerMain.js")`
  throws (hCaptcha's probe; `automation.driver_worker`), a cross-site
  frame sees no first-party storage (`cookieBehavior` 5), `window.open()`
  without activation is blocked, and no page has the 15 extra `set…`
  functions (`setNavigatorUserAgent`, `setTimezone`, …;
  `automation.extra_globals`). The parity suite states the storage and the
  popup cases.
- Firefox 156 itself: `RTCRtpReceiver.getCapabilities('video')` lists
  H264, and a few new interfaces exist (`IDBRecord`,
  `PictureInPictureEvent`, `RTCIceCandidatePair`), as in stock.
- TLS, HTTP/2 and the header order equal a stock Firefox 156.0.1 against
  the echo (JA4 `t13d1517h2_8daaf6152771_3cbfd9057e0d`: Firefox 156 dropped
  the cipher suite `c00a`); `Accept-Encoding` is `gzip, deflate, br, zstd`
  as in stock.
- Fonts: each of the 107 Windows families of the launcher's list has a file
  in the build; of its 573 macOS families 6 have none (152: 69).
- The built-in add-ons (`webcompat` 156.1.0 among them) are active as in
  stock, so Mozilla's site interventions apply as for a user.
- A 152 persona's profile opened by 156 keeps its cookies and moves its
  `compatibility.ini` forward (Firefox then discards its startup cache by
  itself); opened by 152 again afterwards it gives no context within a
  minute (downgrade protection).

The preference table is `docs/evidence/camoufox-156-prefs-audit.tsv`,
made by `scripts/audit-camoufox-prefs.mjs`; the parity suite keeps what a
test can keep of the rest, and the reference page with its results is in
the milestone's hand-over evidence.

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

### User activation and the driver (ADR 0011)

The driver inside the build (Juggler) used to run every evaluation of a
page as user input (`setHandlingUserInput(true)`,
`notifyUserGestureActivation()`), so that a page nobody touched read
`navigator.userActivation` as active about 300 ms after `load` (measured
in M5). Upstream took both calls out in Camoufox pull request 779; the 156
build lifts the popup blocker for an evaluation through
`docShell.driverPopupsAllowed` instead, which grants no activation, and
since pull request 852 Juggler is no `chrome://` package a page can name.
M5.1 had backported the fix into the 152 build's `omni.ja` (ADR 0010,
superseded); that tool and its two checksums are gone.

- **The engine runs the pinned archive only.** At start it hashes
  `omni.ja` against `CAMOUFOX_OMNI_SHA256`; anything else answers
  `browser_launch_failed`, "is not the pinned build's". Run
  `just setup-camoufox`, or rebuild the image.
- **A profile's startup cache needs nothing from Kage.** Firefox discards
  it when the version a profile last started under changes
  (`compatibility.ini`), which every build change does. Profiles from 1.0.0
  hold an `omni.sha256` note of M5.1's; nothing reads it any more, and it
  may stay.

The parity test "reads a page nobody touched without granting it user
activation" says whether a build keeps the promise. The probe's check, and
the cases it must not be tightened to, are in `engine-chromium.md` ("The
probe's check").

### Known tells (not solved here)

- No speech synthesis voices (`speechSynthesis.getVoices()` is empty); a
  Windows or macOS Firefox has the system's. The 156 build reads a
  `voices` key; it needs voice data per system (a follow-up).
- **The canvas is the host's.** Pixels come from Mesa and glyph shapes from
  FreeType under a Windows or macOS claim, and the 156 build no longer
  varies them per persona: every Firefox persona of a host draws the same
  canvas and measures text the same, as every Chromium persona already
  does. That is a pool-level risk: personas of one host share one canvas
  hash while naming different graphics cards, which a vendor that knows
  which hash goes with which card, or that clusters hashes across
  addresses, can link. Accepted for 156 (ADR 0011); the field campaign
  measures how rare the hash is, and the answer, if one is needed, is a
  real GPU or per-persona rendering, never noise. WebGL pixels were and
  are the host's (`host_bound`).
- No Widevine: `requestMediaKeySystemAccess("com.widevine.alpha")` fails
  with "EME has been preffed off", where a desktop Firefox fetches the
  module at first use (a follow-up: the preference and a download through
  the proxy).
- A macOS persona with a colour depth of 30 does not match
  `(color-gamut: p3)`, which a Mac with such a display does (a follow-up).
- The generator gives 13 % of its Windows devices a `maxTouchPoints` of 1
  or 2; a Windows machine reports 0, or 10 with a touch screen (a
  follow-up, in the generator's data).
- `pageshow.persisted` is never true after back and forward: the driver
  turns the back-forward cache off (upstream's own list of what remains
  visible, row 106), and `camoufox.cfg` does too.
- A printed PDF is always A4, portrait, with backgrounds (below).
- Every PNG a canvas exports carries a 16-byte `deBG` chunk that differs
  per browser session. A stock Firefox 152 writes it too: it is Firefox's
  own, not a mark of this build.

**Preferences a page or a server can see.** The build's `camoufox.cfg`
and Kage's launch plan differ from a stock Firefox 156.0.1 in 24 values a
page or a server can see (of 402 that differ in all; the rest are
the browser's own interface, its background services and what the driver
needs). Each is kept for the reason given, and the whole table, with the
source of every row, is `docs/evidence/camoufox-156-prefs-audit.tsv`:

| Preference | stock | persona | who can see it | why it stays |
|---|---|---|---|---|
| `browser.sessionhistory.max_total_viewers`, `fission.bfcacheInParent` | -1, true | 0, false | a page (`pageshow.persisted`) | the driver turns the cache off anyway |
| `fission.webContentIsolationStrategy` | 1 | 0 | a page, by timing (no out-of-process iframes) | Playwright needs it, per upstream |
| `focusmanager.testmode` | false | true | a page (focus is emulated: a blur into a frame does not happen) | the driver needs it |
| `dom.input_events.security.minNumTicks`, `.minTimeElapsedInMS` | 3, 100 | 0, 0 | a page, only with input within 100 ms of an element appearing | the driver needs it; the hand rests longer than that |
| `webgl.forbid-software`, `webgl.force-enabled` | true, false | false, true | a page | the claimed graphics card must have WebGL on the image's software renderer |
| `media.peerconnection.ice.default_address_only`, `.proxy_only_if_behind_proxy`, `.proxy_only_if_pbmode` | false | true | a page (CreepJS says WebRTC "blocked") | no packet leaves outside the proxy (spec §10.3) |
| `gfx.bundled-fonts.activate` | -1 | 1 | a page (fonts) | the claimed system's fonts |
| `dom.push.connection.enabled`, `dom.push.serverURL` | true, Mozilla's | false, empty | a page, only after a notification permission | no connection to Mozilla's push service from the persona's address |
| `browser.safebrowsing.{malware,phishing,downloads,blockedURIs}.enabled` | true | false | Google only | no list updates or lookups from the persona's address |
| `network.http.http3.enable` | true | false | a server | no UDP outside the proxy; behind an HTTP proxy a stock Firefox does not use HTTP/3 either |
| `print.always_print_silent` | false | true | a page that calls `print()`: no dialog | the PDF capture |
| `intl.accept_languages`, `intl.locale.requested` | `und`, empty | the persona's | a page and a server | the persona's languages |
| `dom.webgpu.enabled`, `gfx.blacklist.webgpu` | Linux's | Windows' (on Windows and Apple-GPU macOS claims) | a page (`navigator.gpu`) | what Firefox 156 has on the claimed system |

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
- A navigation without `behavior` sends no input at all. Kage asks for
  human behavior on every level-3 visit (`dwell: "human"`, a scroll
  depth, wander): a page load with no scroll and no dwell is not a
  person's. A level-2 visit is such a load on purpose (`behavior.mode:
  "still"`, the policy's dwell, no input); the caller asked for it by
  name ([levels-and-egress.md](levels-and-egress.md)).
- Flow steps with `human: false` are done by the driver and are visible to
  a page as such; they are for pages known not to look. One more way to
  see it since M5.1: at the `mousedown` of a driver's
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

## The self-test on Firefox

`kage selftest browser --engine firefox` and the live check work as on
chromium (`engine-chromium.md`, "Checking a persona: the self-test"),
the second load of a page whose navigation failed included: a page run by
others may see the exit address twice in one run, and the page's report
says so (`attempts: 2`). What
the two third-party pages show of a Firefox persona was recorded live on
2026-10-18 on Camoufox 156 through two proxies, one stand-in claiming
Windows and one macOS (`tests/fixtures/selftest/firefox-156-live.json`,
34 checks, all passed for both, score 100; judged first against the 152
baseline M5 recorded on 2026-10-09, which it replaces):

- bot.sannysoft.com paints its row "Chrome (New)" red. The page was
  written for Chrome, and a Firefox has no `window.chrome`: every real
  Firefox user gets that red row. The judge reads the row by the browser
  the page saw: on a Firefox, missing passes and present fails hard.
  Every other row is green, the canvas row (`TRANSPARENT_PIXEL`) included.
- CreepJS: `headless` 0 %, `stealth` 0 %, `like headless` 6 %, for both
  claims. It names no anti-fingerprinting mode (its "resistance" entries
  stay unknown), calls the Windows persona's fonts "Like Windows 11" and
  the macOS persona's "Like undefined" (its classifier has no Mac entry
  that matches; the families are macOS ones), and reads the same
  language, time zone, system and graphics card in a service worker as in
  the window.
- CreepJS prints "blocked" for two things. WebRTC: no host and no STUN
  candidate, which is the confinement of spec §10.3 at work. Speech
  synthesis: no voices, the known tell above.

## Captures on Firefox

| Capture | How | A page can notice |
|---|---|---|
| DOM, title, text, storage | read in the driver's isolated world | nothing: a read grants no user activation |
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

On the kage side: `sidecar.firefox_url`, `engines.firefox.browser_major:
156`, and `personas.upgrade_backup_days` (7, how long a profile's copy
from before a browser upgrade is kept). Firefox 156 is browser-only on the
http path (its TLS offers two cipher suites fewer than the newest profile,
`firefox_151`): a Firefox persona has no `wreq_profile` until wreq-util
ships a matching one.

The container: user 65532, read-only root filesystem, every capability
dropped, `no-new-privileges`, Docker's default seccomp profile, writable
`/tmp`, the personas and the artifacts volume, 1 GB of `/dev/shm`. The
stack smoke test checks it on a running container with a context open
(`tests/e2e/firefox-hardening.mjs`).

**The Firefox image is not published.** The Camoufox build bundles
Microsoft's and Apple's font files (2.1 GB of the 156 build's 2.4); they are what makes the font check
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
Kage never uses a launcher's download: the image build and
`scripts/setup-camoufox.sh` fetch the pinned build by URL and checksum,
and the engine refuses any other build.

**Learning of a new build.** Builds are published on the GitHub releases
of `daijro/camoufox` (watch its releases). Since its pull request 810
upstream pre-releases every tested merge and promotes by tag, so the
pre-release flag says nothing about a build: read the pull requests a
build contains instead, for what a page can read (the line's `LEAKS`
list, its changelog). Firefox itself moves every two weeks; a Camoufox
build follows it by weeks, and a persona claims the build's version. No
launcher of camoufox-js accepts the 156 builds, and the engine does not
need one to.

1. Take the version, the release and the SHA-256 of the Linux x86_64 zip
   from the build's GitHub release (GitHub shows the asset's digest):
   `CAMOUFOX_VERSION`, `CAMOUFOX_RELEASE`, `CAMOUFOX_SHA256` in
   `deploy/docker/sidecar.Dockerfile`, and the same three constants in
   `engine-firefox/src/host.ts`. `just setup-camoufox` (with
   `KAGE_CAMOUFOX_ARCHIVE=<path>` it keeps the archive for the next run),
   then set `CAMOUFOX_OMNI_SHA256` in `host.ts` to the new build's own
   `omni.ja` (`sha256sum /opt/camoufox/omni.ja`). A test keeps the
   Dockerfile and `host.ts` at the same values.
2. `just test-node`. The pinned-seed test shows what changed in generated
   fingerprints; the parity suite re-checks the coherence bar, the
   captures, the input path, user activation, a moved persona (a document
   of the previous major, moved and opened on a profile the previous
   major left), third-party storage and popups on the new build. Read
   `properties.json` of the new build: keys it declares `removed` are
   left out by themselves, new live keys are the generator's to fill or
   not; `personaFontconfig` and `validateConfig` fail loudly when the
   fontconfig files or the property types no longer fit.
3. **Audit the preferences.** Download Mozilla's Linux build of the same
   Firefox version (`archive.mozilla.org/pub/firefox/releases/<version>/`,
   checked against its `SHA256SUMS`), unpack it, and run, as a user that
   is not root:

   ```bash
   node scripts/audit-camoufox-prefs.mjs --stock <dir>/firefox --out prefs-audit.tsv
   ```

   It starts the stock browser with nothing driving it and a persona of
   Kage's engine, each from a copy of its build with a preference dump,
   and writes every preference that differs twelve seconds after start,
   with where the persona's value comes from and what the last audit
   (`docs/evidence/camoufox-<major>-prefs-audit.tsv`) said about it. Every
   row marked `new: review` gets a decision: invisible to pages and
   servers (the browser's interface, its background services, what the
   driver needs), or visible, and then either set as in stock in
   `basePreferences` (`launch-plan.ts`) with its reason, or kept and
   listed above under "Preferences a page or a server can see". Commit the
   new table as the evidence of the bump. What a page reads that no
   preference explains is found by comparing a stock browser and a
   persona on the same page, as above (the coherence probe and
   `kage selftest browser` are the start).
4. `just capture-browser-fingerprint firefox`, then "A new browser major"
   in `docs/runbooks/fingerprint-echo.md` (the capture replaces the
   previous major's in `tests/fixtures/fingerprints/browser/`, and
   `BROWSER_ONLY_MAJORS` or `VERIFIED_MAJORS` names the new one).
5. Set `engines.firefox.browser_major` in the deployment, with the image.
6. Run the self-test against your own pages, and the live suite once with
   `KAGE_LIVE_RECORD=1` after reviewing its findings: it judges the run
   against the previous major's baseline and records the new one
   (`tests/fixtures/selftest/firefox-<major>-live.json`), which replaces
   the old.
7. Update `docs/versions.md` and the changelog.

**The personas move by themselves.** A persona's document names the
build's version; at its first open under a newer major the sidecar moves
it (`POST /v1/personas/upgrade`) and kage stores it, and the engine copies
the profile to `.upgrade/<persona id>` before Firefox moves it forward.
What an operator does around a deploy, and the way back, is in
[upgrading-an-engine.md](upgrading-an-engine.md).

## Fallback: a Python engine behind the same contract (risk R1)

camoufox-js is labeled experimental. If it becomes unusable, the Firefox
engine is replaced by a Python service that uses the upstream `camoufox`
launcher and Playwright for Python. Kage and the Chromium sidecar do not
change: the replacement is a container that speaks the same HTTP contract,
`sidecar/packages/contract/openapi.json` (contract 1.4.0), on the same
port, with the same shared secret.

It must implement exactly these endpoints, with the schemas of that
document validated in both directions and every error as `{code, message}`
with the document's codes:

| Endpoint | What it must do |
|---|---|
| `GET /v1/health` | `engine: "firefox"`, the browser version, contexts in use, `contract_version`. |
| `GET /v1/metrics` | The metrics of spec §20 that start with `sidecar_`. |
| `POST /v1/personas/generate` | A fingerprint document from a 64-hex seed: the same seed and constraints give the same bytes. The document is opaque to Kage; it must hold everything needed to start the same browser again. |
| `POST /v1/personas/upgrade` | The same persona for the browser the engine runs: a document of an older major moved (its browser version and `User-Agent`, nothing of the device), the same bytes for the same request; one of a newer major refused with `browser_major_mismatch`. |
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
- No addon, no download at run time, Firefox's own default for autoplay,
  the persona's UI language at startup, no privacy signal the user did
  not ask for, and the preferences of `basePreferences` (the Python
  launcher starts the same build with the same `camoufox.cfg`).
- Reads that grant no user activation. Playwright for Python drives the
  same Juggler, so the same rule about the build holds: a build that has
  the fix (156 and later). The probe fails hard otherwise
  (`automation.user_activation`).
- The parity suite's expectations. Its tests are TypeScript, but they only
  need an `Engine`; the simplest proof for a Python engine is to run them
  against it through a thin `Engine` that calls its HTTP endpoints.
- The license review of its own dependency tree (the Python launcher's is
  not the one reviewed here).
