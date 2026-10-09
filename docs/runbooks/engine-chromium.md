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
kage selftest browser --proxy <proxy id> --timezone Europe/Berlin \
    --language de-DE --language de --language en
kage selftest browser --proxy <proxy id> --timezone Europe/Rome \
    --language it-IT --language it --third-party --json
```

| Flag | Meaning |
|---|---|
| `--proxy` | The proxy to leave through. A retired proxy is refused. |
| `--engine` | `chromium` (default) or `firefox`. A Firefox stand-in persona claims Windows; see `engine-firefox.md`. |
| `--timezone`, `--language` | The identity of the stand-in persona; use the proxy's country. |
| `--third-party` | Also load `selftest.browser.third_party_pages`. |
| `--idle-before` | Keep the context open and untouched for this long (for example `10m`) before the pages load. For measuring what the browser sends by itself; the sidecar's `SIDECAR_CONTEXT_IDLE_MS` must be longer, or it closes the context meanwhile. |
| `--json` | One JSON document instead of a table. |

Until personas exist (M6) the command stands in with one pseudo-persona per
proxy: its seed is derived from the proxy's id and its profile directory is
`selftest/<proxy id>`, so a proxy shows the same browser on every run. The
command needs to reach the sidecar (`sidecar.chromium_url`); with `just dev`
that is `KAGE__SIDECAR__CHROMIUM_URL=http://127.0.0.1:8091`. Every run is an
audit event (`selftest.browser`).

The gated live suite (`just test-live`, `tests/live/browser-selftest.sh`)
runs it with `--third-party` through two proxies per run, in a container
hardened as in production, and compares the findings with the recorded
baseline in `tests/fixtures/selftest/`. `KAGE_LIVE_RECORD=1` records a
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

A plain navigation leaves the page as a visit would. A capture that a page
can notice is opt-in per request and named in the result's `side_effects`:

| Capture | How | A page can notice |
|---|---|---|
| DOM, title | read in an isolated world after settle | nothing |
| raw body, headers, redirects, XHR and fetch bodies | network events; never request interception | nothing |
| console | the browser's log domain: network and security messages | nothing; `console.*` calls and uncaught errors are not captured on chromium |
| screenshot of the viewport | the compositor | nothing |
| screenshot `full_page: true` | the page is laid out at its full height once | a `resize` |
| PDF | the print pipeline | `beforeprint`, `afterprint` |

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
