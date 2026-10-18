# ADR 0011: The Firefox engine runs Camoufox 156, and personas move with it

Status: accepted, 2026-10-18, by the planning session on the owner's
standing delegation (the consults of the Camoufox 156 milestone,
`camoufox156-consults.md` and `camoufox156-consult-answers.md`).
Supersedes [ADR 0010](0010-juggler-backport-in-the-pinned-camoufox.md).

## Context

The 1.0.0 Firefox engine runs Camoufox `v152.0.4-beta.30` with upstream's
user-activation fix backported into its `omni.ja` (ADR 0010). Measured
against stock Firefox 152 and 156 that nothing drives, the personas it
runs show a page four things stock Firefox does not: a page's
`new Worker("chrome://juggler/content/content/WorkerMain.js")` returns a
worker (hCaptcha's probe); a cross-site frame reads the persona's
first-party cookie and storage (`network.cookie.cookieBehavior` 4, stock
5); `window.open()` without activation opens a window; and every page has
15 extra global functions (`setNavigatorUserAgent`, `setTimezone`, …).
The `v156.0.1` line removes all four and has the activation fix natively.
No camoufox-js release accepts it; Kage never runs that check. Firefox
itself is at 158.

156 also changed three things for the worse, measured: `Intl` reported
`en-US` while the persona's languages were German, `globalPrivacyControl`
became `true` for most personas without a `Sec-GPC` header (the generator
writes it), and canvas pixels became the host's for every persona (the
per-persona canvas seeds are gone from the build).

A persona's profile moves forward when Firefox 156 opens it and is
refused by Firefox 152 afterwards (downgrade protection). The documents
of the existing personas name 152; the engine refuses a document of
another major.

## Decision

1. **The pin is `v156.0.1-beta.39`**, by URL and checksum, and its
   `omni.ja` by a second checksum. The backport, its tool and its two
   checksums are removed. camoufox-js 0.12.1 and playwright-core 1.62.1
   stay. Upstream's pre-release flag is not a verdict (it pre-releases
   every merge since its pull request 810).
2. **The engine sends only what the build reads**: the keys the build's
   `properties.json` declares `removed` are left out of the configuration,
   and `navigator.doNotTrack` and `navigator.globalPrivacyControl` are
   never applied, for old and new documents alike.
3. **The launch plan speaks the persona's language at startup**: the
   persona's Firefox UI locale (`intl.locale.requested`, from a
   per-country table checked against the build's locale list) and the
   plan's other preferences reach the build through `CAMOU_PREFS` before
   the first page, and through the driver as before. WebGPU is on where a
   real Firefox 156 has it (Windows, and macOS with an Apple GPU); the
   memory cache and compression dictionaries are as in stock; the
   autoplay preference stays as a guard. The rest of `camoufox.cfg` is
   kept and its page-visible rows are documented as known tells
   (`docs/runbooks/engine-firefox.md`).
4. **Personas move with the engine.** At a persona's first open under a
   newer major the manager has the sidecar move its document
   (`POST /v1/personas/upgrade`, contract 1.4.0, deterministic: the
   browser version and the `User-Agent`, nothing of the device), stores
   it with the new major and the wreq profile that major resolves to, and
   records `persona_browser_upgraded`. Sessions, cookies, the http jar
   and the per-domain state continue.
5. **A Firefox major is one-way.** Before a profile's first start under a
   newer major the engine copies it to `.upgrade/<persona id>/` on the
   personas volume; Kage deletes a copy after
   `personas.upgrade_backup_days` (7). Going back within that window is
   the runbook's procedure; Firefox's `--allow-downgrade` is never used.
6. **The host's canvas is accepted** for this pull request and recorded as
   a pool-level risk (personas of one host share a canvas hash while
   naming different graphics cards). The answer, if the field campaign
   finds the hash rare enough to link personas, is a real GPU or
   per-persona rendering, never noise.

## Consequences

- The four tells of 1.0.0 end at the deploy, for every Firefox persona at
  its next open; until then the 1.0.0 personas carry them.
- The Firefox image grows by about 1.2 GB (the build bundles 2.1 GB of
  fonts).
- Two hard probe checks keep the regressions from coming back unnoticed:
  `Intl`'s language against `navigator.language`, and
  `globalPrivacyControl` against the navigation's `Sec-GPC`. The parity
  suite states what a page sees of a persona's document, of a moved
  persona, of third-party storage and of an unrequested popup.
- The parity suite's storage statement found a Chrome tell (patchright's
  default switches turn third-party storage partitioning off). Chrome is
  unchanged here; its switch audit is the next milestone's.
- A later Camoufox build is a routine fingerprint-critical pull request:
  the preference audit, the parity suite, the capture against the echo
  and the live self-test (`docs/runbooks/engine-firefox.md`).
