# ADR 0009: A chromium persona claims the system its Chrome runs on

Status: accepted, 2026-10-09, by the planning session on the owner's
standing delegation to pick the option best for stealth (consult 1 of 5,
`m4-consults.md`); reviewed with the M4 merge. It changes the default of
`engines.chromium.allow_cross_os_claims` from true to false and supersedes
the chromium rule of spec §6.2 and the chromium rows of the persona mix in
§6.1.

## Context

Spec §6.2 expected the Chromium engine, which runs real Chrome on Linux in a
container, to present Windows and macOS personas by fingerprint injection,
and named a fallback: if the Client Hints coherence check fails on the
pinned Chrome, restrict the engine to `claimed_os = linux`
(`engines.chromium.allow_cross_os_claims`).

M4 measured that check on the pinned Chrome 155.0.8059.39 driven by
patchright 1.63.0, against a page that reads the identity in the window, in
a same-origin, a `srcdoc` and an `about:blank` iframe, in a dedicated worker
and in a blob worker, and that records the request headers of the
navigation, an iframe navigation, a `fetch()`, the worker script and a
`fetch()` from the worker. The raw results are in
`m4-evidence/experiments/` of the M4 handover. Four ways to claim Windows
were tried:

| Method | What it gets right | What gives it away |
|---|---|---|
| fingerprint-injector's own attach (header rewrite and init script) | window `navigator` values | request headers are rewritten by a route: `Sec-CH-UA-Platform` and the header order differ from real Chrome; workers and blob workers still say Linux; dark mode is forced; `history.length` is random |
| an init script through the driver | window `navigator` values | patchright implements init scripts by intercepting every document and rewriting its HTML: the HTTP cache is off, so every request carries `Pragma: no-cache` and `Cache-Control: no-cache` and nothing is ever served from cache; workers say Linux; request headers say Linux |
| CDP `Emulation.setUserAgentOverride` with metadata | window and iframes, and the request headers of the page | dedicated and blob workers report the real platform in `navigator.platform` and `userAgentData`, and the worker script request and worker `fetch()` send `Sec-CH-UA-Platform: "Linux"`: one page load shows both systems |
| CDP override plus a raw `Page.addScriptToEvaluateOnNewDocument` | as above, with getters patched in the window | as above for workers; patched getters are not native functions |

No method was coherent in workers, and worker checks are standard in
commercial detection scripts. A persona that says Windows in the window and
Linux in a worker of the same page is a stronger signal than a persona that
says Linux everywhere: the first is a contradiction only automation
produces, the second is an uncommon but real browser.

The same measurements showed what the launch of a real Chrome can set
without any script in the page, and identically in window, iframes and
workers: the screen (the X display's size), the time zone (`TZ`), the
languages (`--lang`, `LANG`, `intl.accept_languages`), the CPU count the
browser sees (a CPU affinity mask), and WebGL from Mesa's software renderer
through ANGLE.

## Decision

1. **A chromium persona claims the operating system its Chrome runs on.**
   In the shipped image that is Linux. Its identity is made by how the
   browser is launched, not by scripts: `inject: none` is the default, and
   then no script of Kage's runs in any page.
2. `engines.chromium.allow_cross_os_claims` defaults to **false** (kage) and
   `SIDECAR_ALLOW_CROSS_OS_CLAIMS` to false (sidecar). With the default the
   sidecar refuses to generate or open a persona that claims another system
   with the typed error `cross_os_claim_disabled`.
3. The injection path stays in the code behind that flag (`inject: window`:
   the CDP override plus one script, applied deterministically from the
   persisted fingerprint) for an operator who accepts the worker
   contradiction, for example against sites known not to look there. Its
   integration test pins what it achieves and what it does not.
4. The coherence bar a chromium persona must pass, in the fixture-site
   test and in the self-test probe, is the full one: `User-Agent` and
   `Sec-CH-UA-Platform` on the navigation, an iframe navigation, a window
   `fetch()`, the worker script request and a worker `fetch()`;
   `navigator.userAgent`, `navigator.platform`, `userAgentData.platform`
   and the high-entropy values in the window, the three kinds of iframe, a
   dedicated worker and a blob worker; native getters; navigation header
   names and order equal to the recorded capture of real Chrome; no
   `Pragma` or `Cache-Control` on a plain navigation; a second visit served
   from cache.
5. What is the host's and not the persona's is said, not hidden: the
   persisted fingerprint lists the values that come from the machine
   (`host_bound`: device memory, WebGL renderer, fonts, audio and media
   devices), so the persona manager knows that two personas on one host
   share them.

## Consequences

- Chrome on Linux is a small share of desktop traffic, so the honest
  persona is less common than the Windows persona the spec planned. It is
  coherent in every context a page can open, which the planned one could
  not be. The persona mix moves accordingly (planning session, to be written
  into §6.1 for M6): chromium on linux 40 %, firefox on windows 40 %,
  firefox on macos 15 %, firefox on linux 5 %, pending M5's measurement of
  Camoufox under the same bar.
- The path to a common *and* coherent Chrome persona is a real engine host
  of that system (risk R3: a Windows machine running the same sidecar), not
  emulation. That is a candidate milestone after M5.
- Software WebGL (`llvmpipe`) is what a Linux machine without GPU
  acceleration reports; it is plausible but it is also what containers
  report. Mapping a GPU of the host into the container (`SIDECAR_GL=host`,
  `docs/runbooks/engine-chromium.md`) yields a real renderer with no code
  change.
- The http path keeps its own rule: a chromium persona's `wreq_profile`
  follows the verified-majors gate (§6.1 step 4). Chrome 155 is
  browser-only today (`BROWSER_ONLY_MAJORS`), see
  `docs/runbooks/fingerprint-echo.md`.
- Known remaining tells of the honest persona, recorded as follow-ups and
  not solved here: no audio or media devices (F9), a screen without a
  panel or taskbar (the work area equals the screen), a font set of a
  Debian desktop.

## Alternatives considered

- **Keep the default true and accept the worker contradiction.** Rejected:
  it makes every chromium persona detectable by one comparison, on every
  site that makes it.
- **Patch workers as well** (intercept worker scripts and prepend the
  override). Rejected: it needs request interception on every worker script
  and blob URL rewriting, both observable (cache behaviour, `Function`
  source, timing), and it still leaves the `Sec-CH-UA-Platform` header of
  worker requests, which no script can change.
- **Drop the injection path altogether.** Not taken: it is small, it is
  off by default, and the test that documents its limits is the evidence
  for this decision whenever Chrome changes.
