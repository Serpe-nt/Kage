# ADR 0014: Kage owns Chrome's launch switches

Status: accepted on 2026-10-19 by the planning session on the owner's
standing delegation (the consults of the Chrome switch audit,
`chrome-switches-consults.md` and `chrome-switches-consult-answers.md`).
The decision row is the planner's. It is the frame of D33 ("identity via
launch parameters") applied to the switches, which are launch parameters
too.

## Context

Up to 1.0.1 the Chromium engine started Chrome with the driver's default
switches: patchright 1.63.0 keeps Playwright's `chromiumSwitches`, 25
switches and 15 disabled features meant for test runs, and the engine
passed no `ignoreDefaultArgs`. The Camoufox 156 milestone found the first
consequence by its storage check (security review T2). The Chrome switch
audit then measured every one of them against a stock Chrome 155 that
nothing drives (started with the address on its command line, a display,
time zone, languages and preferences set up as a persona's), switch by
switch, with a reference page of about 110 values on plain http and in a
secure context, a local CreepJS, `chrome://version`, screenshots of the
display, Kage's parity suite and integration tests, and the echo.

What a page or a server could see of a 1.0.0 Chromium persona because of
those switches, and a stock Chrome does not show:

- **a bar over every page**: "You are using an unsupported command-line
  flag: `--disable-blink-features=AutomationControlled`". The viewport is
  56 px shorter than the window's (`outerHeight − innerHeight` 143
  instead of 87). The flag cannot simply go: the driver's
  `--remote-debugging-pipe` turns Chrome's automation flag on, and
  without the switch `navigator.webdriver` is `true` (CreepJS: 33 %
  headless);
- **unpartitioned third-party storage** (`ThirdPartyStoragePartitioning`
  off): a cross-site frame read its site's first-party `localStorage` and
  IndexedDB;
- **no HTTPS upgrade** (`HttpsUpgrades` off): a stock Chrome 155 asks a
  public host on 443 before it sends an `http` address there, and falls
  back by itself (1 ms after a refused tunnel, about 90 ms after an
  untrusted certificate, 2.95 s when 443 never answers; no interstitial);
- **the built-in AI APIs unavailable** (`OptimizationHints` off):
  `LanguageDetector`, `Summarizer` and `LanguageModel` answer
  `unavailable`;
- and, seen only by Google, no variations seed, no component update and
  no `X-Client-Data` header (`--disable-background-networking`).

The other switches changed nothing a page reads (no-ops on Google Chrome
or on Linux, UI only, or effects only on a hidden tab, which a persona's
page never is), and the driver needs none of them for what Kage does but
`--no-first-run` (a new profile's first-run window holds the start page
and the launch times out) and the AutomationControlled switch.

## Decision

1. **The engine names every switch Chrome is started with.** It leaves
   out the driver's defaults it does not keep (`DRIVER_SWITCHES_LEFT_OUT`
   in `engine-chromium/src/launch-plan.ts`, spelled as patchright 1.63.0
   spells them, passed as `ignoreDefaultArgs`) and adds its own
   (`KAGE_SWITCHES`). `ignoreDefaultArgs: true` is never used: the driver's
   launcher keeps its tested path for the pipe, the profile, the start
   page and the proxy.
2. **A switch is Chrome's default unless** the driver, the hand or the
   container needs it, or no page can see it and Kage gains something by
   it. Every switch that stays carries its reason in the code and in
   `docs/runbooks/engine-chromium.md`, and every one a page or a server
   can see is a known tell there.
3. **`--test-type`** is added: it suppresses Chrome's startup bars, the
   one for the AutomationControlled switch among them, and measurably
   nothing a page reads (the reference page, CreepJS and the active field
   trials are those of a Chrome without it).
4. **Kept off, as known tells:** `OptimizationHints` (no model is ever
   downloaded through a persona's proxy; a model of the AI APIs can be
   gigabytes), `MediaRouter` and `DialMediaRouteProvider` (no page can
   start Cast discovery, multicast on the sidecar's network; no page-
   visible difference was measured), and `--disable-background-networking`
   (Google's background services stay quiet; measured live in the audit:
   about 89 kB per persona-hour with it, 139 kB without, which adds the
   variations seed and Safe Browsing to the hosts asked for).
5. **Guarded twice:** a unit test reads patchright-core's own default
   list from the installed bundle and checks that each switch is either
   kept or left out; an integration test reads the browser process's
   `/proc/<pid>/cmdline` and fails on any switch that is neither Kage's
   nor the launcher's. A driver bump that adds or respells a default is
   seen at once.

## Consequences

- Every Chromium persona moves at its next context open: no bar, storage
  partitioned, the HTTPS upgrade as in Chrome. Third-party storage it
  wrote unpartitioned is not read by partitioned frames any more, as for
  every Chrome user in 2023.
- A result of an upgraded navigation shows the https address; one that
  fell back shows Chrome's own internal 307 from the https address as a
  hop of the chain, which no server sent.
- The wire does not move: the capture against the echo with the new
  switches equals `tests/fixtures/fingerprints/browser/chromium-155.json`.
- The names of the test world (`*.kage.test`, `*.localhost`, single-label
  compose names) are never upgraded by Chrome; the parity suite, the
  integration tests and the e2e need no TLS for it.
- A patchright bump re-reads its default switches against this list (the
  unit test fails first) and the switch audit's reference runs
  (`docs/runbooks/engine-chromium.md`).

## Alternatives considered

- **Keep the driver's defaults** and document them: rejected, three of
  them are tells on every page a persona reads, one a bar the page sees.
- **Close the bar with the hand** at each launch instead of `--test-type`:
  more moving parts (the bar's position moves with the window, and the
  click must come before the first page), and no gain a page could see.
- **Restore `OptimizationHints`**, or restore it and refuse the large
  model by enterprise policy: the first lets a page make each persona
  download models through the pool; the second makes a managed browser
  (`chrome://management`), a tell of its own. Left for the field campaign
  to show whether a site probes the AI APIs at all.
