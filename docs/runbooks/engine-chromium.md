# Runbook: the Chromium engine

How the Chromium sidecar makes a persona, what its container needs from the
host, how to check that it still looks like a person's browser, and how to
move it to a new Chrome. Spec §10 and ADR 0009 are the contract; this page
is the practice.

## What a persona is made of

A chromium persona is a real Chrome stable, headful on its own X display,
with a persistent profile and an authenticated proxy. Nothing is spoofed by
default (`inject: none`): what a page reads is what the browser really is,
the same in the window, in every iframe and in every worker.

| What a page can read | Where it comes from |
|---|---|
| operating system, `User-Agent`, Client Hints | the Chrome in the image: Linux x86_64 (ADR 0009) |
| screen and window size | the persona's own Xvfb display, sized from its fingerprint |
| work area (`screen.avail*`) | the display announces a desktop's top bar of 32 px (the `_NET_WORKAREA` hint) and the window sits below it, as a maximized window on GNOME does |
| keyboard (`KeyboardEvent.code` against `key`) | the display's layout, set from the persona's locale with `setxkbmap` |
| time zone | `TZ` of the browser process, the persona's zone |
| languages, `Accept-Language` | `--lang`, `LANG`, and `intl.accept_languages` in the profile |
| `hardwareConcurrency` | a CPU affinity mask: the persona's count, at most the host's |
| WebGL vendor and renderer | Mesa llvmpipe through ANGLE, or the host's GPU (below) |
| cookies, storage, history | the persona's profile directory, kept between contexts |
| WebRTC addresses | none: `disable_non_proxied_udp`, no UDP leaves outside the proxy |
| media devices, `AudioContext` | a PulseAudio in the container with one output and one input that go nowhere (`SIDECAR_AUDIO=pulse`, the image's default) |
| device memory, fonts | the host and the image; the fingerprint lists them as `host_bound` |
| storage of a cross-site frame | **not partitioned**, unlike a Chrome nobody configured: patchright's default switches (Playwright's `chromiumSwitches`) disable `ThirdPartyStoragePartitioning`, so a frame of site B inside site A reads B's first-party `localStorage` (the cookie, `SameSite=Lax` by default, stays out). Found by the parity suite in the Camoufox 156 milestone; a known tell until the audit of Chrome's launch switches (security review T2) |

One browser process per persona context: a persistent profile is a browser
process of its own in Chrome. `SIDECAR_MAX_CONTEXTS` (default 3) is
therefore also the number of browsers a container runs at once.

## Settings

Set in `deploy/.env` (`deploy/.env.example` lists them) or on the container.

| Variable (compose) | Sidecar variable | Default | Meaning |
|---|---|---|---|
| `KAGE_SIDECAR_TOKEN` | `SIDECAR_TOKEN` or `SIDECAR_TOKEN_FILE` | none, required | The shared secret. Every route needs it, health and metrics included. At least 24 characters. |
| `KAGE_SIDECAR_MAX_CONTEXTS` | `SIDECAR_MAX_CONTEXTS` | 3 | Contexts at once. One more answers `context_limit`. |
| `KAGE_SIDECAR_CONTEXT_IDLE_MS` | `SIDECAR_CONTEXT_IDLE_MS` | 120000 | An unused context is closed after this; the profile stays. |
| `KAGE_SIDECAR_MAX_PAGES_PER_BROWSER` | `SIDECAR_MAX_PAGES_PER_BROWSER` | 500 | A browser is restarted after this many navigations. |
| `KAGE_SIDECAR_GL` | `SIDECAR_GL` | `llvmpipe` | `llvmpipe`, `swiftshader`, `none` or `host`. |
| `KAGE_SIDECAR_AUDIO` | `SIDECAR_AUDIO` | `pulse` in the images, `none` elsewhere | The sidecar runs a sound server for its browsers; without one a browser lists no media device. |
| `KAGE_SIDECAR_ALLOW_CROSS_OS_CLAIMS` | `SIDECAR_ALLOW_CROSS_OS_CLAIMS` | false | See "Claiming another system". |
| `KAGE_SIDECAR_SELFTEST_PROBE_URL`, `…_ECHO_URL` | `SIDECAR_SELFTEST_PROBE_URL`, `…_ECHO_URL` | unset | The self-test's own pages when a request names none. |

On the kage side: `sidecar.chromium_url`, `sidecar.connect_timeout`,
`sidecar.timeout` (longer than the longest navigation), and
`sidecar.connect_retries` (a call is repeated only when the connection could
not be made at all; a request that reached the sidecar never is).

## What the container needs

`deploy/compose.yaml` holds all of it; the stack smoke test
(`just test-e2e`) checks it on a running container with a real context open.

- **User 65532, read-only root filesystem, every capability dropped,
  `no-new-privileges`.** Writable: `/tmp` (tmpfs), the personas volume, the
  artifacts volume, `/dev/shm` (1 GB; the 64 MB default crashes tabs).
- **Chrome's sandbox on.** Kage never passes `--no-sandbox`. The sandbox
  puts every renderer into its own user, PID and network namespace and
  chroots it. Docker's default seccomp profile refuses those system calls
  to a container without `CAP_SYS_ADMIN`, so the service uses
  `deploy/seccomp/chromium.json`: Docker's default profile plus one rule
  allowing `chroot`, `clone`, `setns` and `unshare`. Nothing else differs.
  It is derived, not hand-edited:

  ```bash
  curl -fsSLO https://raw.githubusercontent.com/moby/profiles/<commit>/seccomp/default.json
  node scripts/derive-chromium-seccomp.mjs default.json <commit> > deploy/seccomp/chromium.json
  ```

  The commit in use is recorded in `docs/versions.md` and in the file's own
  comment. Re-derive it when Docker's default profile changes.
- **Unprivileged user namespaces on the host.** The sandbox creates them.
  Most distributions allow it. Ubuntu 23.10 and later restrict it through
  AppArmor for programs without a profile; inside a container that surfaces
  as Chrome failing to start with "No usable sandbox". Either load an
  AppArmor profile for the container that allows `userns`, or set
  `kernel.apparmor_restrict_unprivileged_userns=0` on the host. CI runners
  on Ubuntu 24.04 need the same setting (the workflow sets it).
- **Not root.** Chrome refuses to sandbox itself as root. The same holds
  for the sidecar's integration tests: run `just test-node` as a normal
  user.

When Chrome does not start, the sidecar answers `browser_launch_failed`
with Chrome's own message. "Failed to move to new namespace" or "No usable
sandbox" is one of the two points above.

### A real GPU instead of software rendering

`llvmpipe` is what a Linux machine without GPU acceleration reports. A
container can show a real renderer when a GPU of the host is mapped in. No
code changes:

```yaml
# deploy/compose.override.yaml
services:
  browser-chromium:
    devices: ["/dev/dri:/dev/dri"]
    group_add: ["<gid of the host's render group>"]
    environment:
      SIDECAR_GL: host
```

Every persona of that host then reports the same GPU, as every browser on
one machine does; the fingerprint names WebGL as host-bound for that reason.

## Claiming another system

Off by default, and for a reason (ADR 0009): a Chrome on Linux that claims
Windows says Windows in the window and Linux in every worker of the same
page, and no method changes that. With
`KAGE_SIDECAR_ALLOW_CROSS_OS_CLAIMS=true` (and
`engines.chromium.allow_cross_os_claims: true` in kage) the sidecar accepts
such a persona and applies it with `inject: window`: a `User-Agent` and
Client Hints override plus one script per document, the same bytes on
every open. Its integration test (`engine.integration.test.ts`, "fails for
a Windows claim: its workers still say Linux") pins what that achieves and
what it does not. Leave it off unless the target is known not to look.

## Checking a persona: the self-test

`POST /v1/contexts/{id}/selftest` loads pages in the persona's own context
and judges what they show. A finding is `pass`, `warn`, `fail` or `unknown`;
a `fail` on an automation flag, an identity mismatch or a transport
mismatch is hard and fails the test whatever the score. `unknown` (a page
that could not be reached or read) changes no score and sets
`complete: false`, so an outage of a test page never counts against a
persona.

| Page kind | What it is | Who runs it |
|---|---|---|
| `probe` | Kage's coherence probe, `tests/fixtures/site/probe/`: the full bar of ADR 0009 | the operator |
| `echo` | `/browser` of `kage echo serve`: JA4, HTTP/2 and header order, against the recorded capture | the operator |
| `sannysoft` | bot.sannysoft.com | a third party |
| `creepjs` | CreepJS | a third party |
| `generic` | any page, read with regular expressions from the request | whoever |

Kage's own pages are the default set. Third-party pages are loaded only
because someone asked: they see the exit address of the proxy.

```bash
kage selftest browser --proxy <proxy id>
kage selftest browser --proxy <proxy id> --engine firefox --third-party --json
kage selftest browser --persona <persona id> --third-party
```

| Flag | Meaning |
|---|---|
| `--proxy` | The proxy to leave through, with a stand-in persona. A retired proxy is refused. |
| `--persona` | Run the test as this persona instead, in its own context: its fingerprint, its profile, its proxy. The run is recorded for the persona (`docs/runbooks/personas.md`). Not with `--engine`, `--timezone` or `--language`. |
| `--engine` | `chromium` (default) or `firefox`. A Firefox stand-in persona claims Windows; see `engine-firefox.md`. |
| `--claimed-os` | The system the stand-in persona claims, in place of its engine's own: `macos` (or `windows`) on Firefox. A Chromium sidecar refuses anything but `linux` (ADR 0009). A stand-in of another claim keeps a profile of its own, `selftest/<engine>-<os>/<proxy id>`. Not with `--persona`. |
| `--timezone`, `--language` | Override the identity of the stand-in persona. By default its time zone, locale and languages are what a persona behind that proxy would get: the proxy's observed geo record and the list the engine's browser ships with in that country. |
| `--third-party` | Also load `selftest.browser.third_party_pages`. |
| `--idle-before` | Keep the context open and untouched for this long (for example `10m`) before the pages load. For measuring what the browser sends by itself; the sidecar's `SIDECAR_CONTEXT_IDLE_MS` must be longer, or it closes the context meanwhile. |
| `--json` | One JSON document instead of a table. |

Until personas exist (M6) the command stands in with one pseudo-persona per
proxy and engine: its seed is derived from the proxy's id and its profile
directory is `selftest/<engine>/<proxy id>` (`selftest/chromium/...`), so a
proxy shows the same browser on every run and the two engines never share a
profile. The command needs to reach the sidecar (`sidecar.chromium_url`);
with `just dev` that is `KAGE__SIDECAR__CHROMIUM_URL=http://127.0.0.1:8091`.
Every run is an audit event (`selftest.browser`).

The gated live suite (`just test-live`, `tests/live/browser-selftest.sh`)
runs it with `--third-party` through two proxies per run for each engine of
`KAGE_LIVE_ENGINES` (default `chromium firefox`), in containers hardened as
in production, and compares the findings with the recorded baseline of
that engine and major in `tests/fixtures/selftest/`. A run without a
baseline fails and says how to record one: `KAGE_LIVE_RECORD=1` records a
passing run as the new baseline.

### What an idle browser costs

Chrome talks to Google by itself: component updates, safe browsing lists,
field trial data. That traffic leaves through the persona's proxy like
everything else and counts against the proxy plan. It is left on, because
it is what a real Chrome at that address does (and no target sees it).
`KAGE_LIVE_IDLE_SECONDS=600 just test-live` measures it: the first
context of the browser check stays open and untouched for that long, the
counters of the sidecar container's network interface are read every five
seconds, and `tests/live/background-traffic.mjs` reports the bytes of the
first two minutes (a browser that has just started fetches the most), the
rate after that, and what one persona-hour costs. The interface counts
packet headers too, so a provider bills slightly less than the figure.

## Input and flows

Human input (the behavior engine of spec §10.4, the human steps of a flow,
the click of a `link` or `search_engine` entry) goes through the mouse and
keyboard of the context's own X display, not through the driver; the
measurements behind that, the motor profile of a persona and the entry
strategies are described in `engine-firefox.md` and hold for both engines.
On chromium a wheel notch scrolls 120 px, and two notches within one frame
reach a page as one wheel event of twice the size.

## Captures and what they cost

A capture that a page can notice by an event is opt-in per request and
named in the result's `side_effects`:

| Capture | How | A page can notice |
|---|---|---|
| DOM, title, text, storage | read in an isolated world over the engine's own debugger session (below) | nothing |
| raw body, headers, redirects, XHR and fetch bodies | network events; never request interception | nothing |
| console | the browser's log domain: network and security messages | nothing; `console.*` calls and uncaught errors are not captured on chromium |
| screenshot of the viewport | the compositor | nothing |
| screenshot `full_page: true` | the page is laid out at its full height once | a `resize` |
| PDF | the print pipeline | `beforeprint`, `afterprint` |

### Reading a page without touching it

The driver marks every evaluation of a page as a user gesture:
patchright-core 1.63.0 sends `Runtime.callFunctionOn` with
`userGesture: true`, as Playwright does. M5 measured what that means: a
page nobody touched reads `navigator.userActivation.isActive` and
`hasBeenActive` as true about 300 ms after `load`, with no input event
before it, and a new `AudioContext` is `running` where an untouched
Chrome's is `suspended`. Every navigation reads the page at least once
(the title and the text right after `load`), so that was true of every
page.

Since M5.1 nothing the engine reads of a page goes through the driver
(`engine-chromium/src/page-reader.ts`). The title, the text, the document,
selector waits, storage and the measurements of the behavior engine
(element boxes, the page's size and scroll position, the words in view)
are evaluated over the engine's own debugger session, the one that already
takes screenshots and reads cookies, with `Runtime.evaluate` in an
isolated world and no gesture flag. The driver keeps what does not
evaluate in the page (the launch, `goto`, `goBack`, the waits for a load
state and for a URL, network events) and the input of flow steps that ask
for it with `human: false`, which is visible to a page anyway.

- **A world of the engine's own.** A script registered for every new
  document (`Page.addScriptToEvaluateOnNewDocument` with a world name)
  puts a mark into that world, and every expression first checks the mark
  and that it runs in the top frame. The reason was measured: an execution
  context id is a small number that the next renderer process hands out
  again, and after a navigation to another origin id 1 was the new page's
  own world. With the check an evaluation that lands anywhere else reads
  nothing and is repeated in the right world. The world's name and the
  mark are random per context: no page can read either. patchright
  registers the same kind of script for its own world.
- **The `Page` domain is enabled on the engine's session** for that
  script: without it Chrome accepts the script and never runs it. The
  `Runtime` domain stays off, as before.
- **A wait for a selector looks every 100 ms** from the sidecar and leaves
  nothing running in the page. A selector is CSS.
- Measured on a page with a strict Content Security Policy and Trusted
  Types: the reads raise no violation and no error the page can see, also
  when an expression throws.

What a page shows now (`engine parity`, "reads a page nobody touched
without granting it user activation", and the M5 experiment rerun): not
active, never active and a `suspended` `AudioContext` at `load`, 300 ms
and 4 s later, with the selector wait and the DOM capture taken; still so
after a mouse that only moved; active from the `mousedown` of a click, as
for a person, and no longer six seconds after it.

A patchright bump needs no step for this: the engine does not use the
driver's evaluation at all, and the parity test fails if a read finds its
way back to it.

### The probe's check, and what it must not be tightened to

The coherence probe counts the trusted input events it receives and reads
`navigator.userActivation` and a new `AudioContext` when its script starts
and again half a second after `load` (a driver that reads a page touches
it within 300 ms of `load`).

| Finding | Fails or warns when | Hard |
|---|---|---|
| `automation.user_activation` | `isActive` or `hasBeenActive` is true, the page received no event that grants activation (`keydown`, `mousedown`, `pointerdown`, `pointerup`, `touchend`), and it was not reached from a page of its own origin | yes: it fails the self-test whatever the score |
| `automation.audio_autoplay` | an `AudioContext` runs, or the browser answers `allowed` when asked (`navigator.getAutoplayPolicy()` for `mediaelement` or `audiocontext`, Firefox only), under the same two conditions | no, a warning |

Three things a browser does for a person too were measured and are why
the rule is not stricter:

- A resting pointer gives Chrome a `mousemove` on a page nobody touched.
  It is input and grants nothing: the rule counts the events that grant
  activation, not input.
- Chrome carries a person's activation into the next page when that page
  was reached by **a click on a link of the same origin**:
  `hasBeenActive` is true and an `AudioContext` runs from the first script
  on, with no input on that page. Firefox does not. A typed navigation
  carries nothing on either engine, also when the page before it had been
  clicked on. The self-test loads the probe by a typed navigation; a probe
  that says it came from its own origin (by its referrer) is not judged.
- A person can allow autoplay in a browser's settings, and then sound
  starts on a page nobody touched. Sound that may start by itself is
  therefore a warning and never a failure. (The Camoufox build ships with
  autoplay allowed; what that looked like and what the engine does about
  it is in `engine-firefox.md`. With Firefox's default in place no way
  of reaching a page that was measured starts an `AudioContext` in
  Firefox without input: typed, by a link, or back in history.)

A probe page older than this check (a copy on a static host that nobody
updated) reports nothing about activation and gets a warning, so that
nobody trusts a check that did not run.

## Moving to a new Chrome

A Chrome bump is a dedicated change (`docs/versions.md`,
fingerprint-critical).

1. Take the version and the package checksum of the new stable:
   `CHROME_VERSION` and `CHROME_SHA256` in
   `deploy/docker/sidecar.Dockerfile`. The image build and
   `scripts/setup-chrome.sh` both read these two lines.
2. `just setup-chrome`, then `just test-node`. The engine's integration
   tests fail if the installed Chrome is not the pinned one, and re-check
   the coherence bar, the header order and the captures on the new build.
3. `just capture-browser-fingerprint` records
   `tests/fixtures/fingerprints/browser/chromium-<major>.json`. Then follow
   "A new browser major" in `docs/runbooks/fingerprint-echo.md`: the new
   major joins `VERIFIED_MAJORS` or `BROWSER_ONLY_MAJORS`, with the reason.
4. Set `engines.chromium.browser_major` in the deployment.
5. Run the self-test against your own pages, and the live suite once with
   `KAGE_LIVE_RECORD=1` after reviewing its findings.
6. Update `docs/versions.md` and the changelog.

A persona keeps its fingerprint document across the bump: the document
holds no browser version of its own to apply, the version is always read
from the installed Chrome.
