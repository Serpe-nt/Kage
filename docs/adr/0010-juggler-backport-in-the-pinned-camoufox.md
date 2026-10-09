# ADR 0010: The pinned Camoufox build runs with one upstream fix backported into its driver

Status: accepted, 2026-10-09, by the planning session on the owner's
standing delegation (consult 1 of M5.1, `m5-1-consults.md`). It does not
change the pin of D29: the build is still `v152.0.4-beta.30`, fetched by
URL and checksum. It changes what the engine runs of it, by 260 bytes.

## Context

M5 measured that a page nobody touched reads
`navigator.userActivation.isActive` and `hasBeenActive` as true about
300 ms after `load` on both engines, with no input event before it. On
Firefox the cause is inside the browser build: Juggler, the driver
Camoufox inherits from Playwright, runs every evaluation of a page as user
input. `evaluateScript` and `evaluateFunction` of
`chrome/juggler/content/content/Runtime.js` call
`windowUtils.setHandlingUserInput(true)` and
`document.notifyUserGestureActivation()`. Every navigation evaluates at
least once (the title and the text right after `load`), so every page a
Firefox persona renders is marked. A detector that compares
`userActivation` with the input events it recorded reads one boolean.
Spec §10.2 says captures must not change what the page experiences.

M5.1 measured, on the pinned build:

- Either call alone grants activation. With only
  `notifyUserGestureActivation()` removed the page is still active 300 ms
  after `load`; with both removed it is not, at `load`, 300 ms, 1.5 s and
  4 s later, with and without a selector wait and a DOM capture.
- Input still grants activation as it does for a person: a click through
  the display sets `isActive` at `mousedown`, six seconds later it is
  false again and `hasBeenActive` is still true. A mouse that only moves
  grants nothing.
- The engine parity suite is green on the changed build.
- Upstream made the same change: Camoufox pull request 779 (merged
  2026-09-25), commit "fix(juggler): evaluate() no longer grants user
  activation", in the builds from `v156.0.1-beta.32` on. The file at tag
  `v152.0.4-beta.30` is byte for byte the one in the pinned build
  (SHA-256 `6a3c7a6f3b40769fb161021edd00af526dd631149194f0abb9c5bf789e551364`)
  and has the four statements; at `v156.0.1-beta.32` they are gone. No
  upstream issue was filed: there is nothing left to report.
- The fixed builds are Firefox 156. camoufox-js 0.12.1 accepts no build
  above `beta.31` (`docs/versions.md`), so taking one means a new launcher
  pin, a new Firefox major, a re-measured coherence bar and a re-recorded
  capture: a fingerprint-critical bump for a pull request of its own.

## Options

- **A. Backport the fix into the pinned build.** Take the four statements
  out of the file inside the build's `omni.ja`.
- **B. Move the pin to the v156 line now.** The right end state, and not a
  change to make inside another milestone.
- **C. Keep the tell documented** until B, with the probe finding a
  warning on Firefox. 60 % of the persona mix (spec §6.1) would carry it.

## Decision

A now; B is scheduled as its own milestone and removes A.

`scripts/patch-camoufox-juggler.mjs` applies it. `omni.ja` is a zip
archive and `Runtime.js` is stored in it uncompressed, so nothing is
repacked: the four statements are replaced by statements of the same
length that do nothing (`null;` and `void 0;`, each with a comment that
names Kage), every offset in the archive stays, and the entry's CRC-32 is
written anew in its two places. 260 of 43 153 155 bytes differ. The tool
turns exactly one archive into exactly one result and refuses any other
input:

| | SHA-256 of `omni.ja` |
|---|---|
| as the release ships it | `269da6dadc2dd93b5b0baafecccc8de1849020d04889d4fdaaa5423bf7e8545e` |
| with the backport | `ad3799d8c96fa80ba26a58cb1aaa28f525801d9ecc727a3e2be9262318ccbf5c` |

Both are pinned in `deploy/docker/sidecar.Dockerfile` and listed in
`docs/versions.md`. The image build and `scripts/setup-camoufox.sh` run
the tool, so tests, CI and the image run the same bytes.

Two rules keep the tell from coming back unnoticed:

- **The engine refuses any other archive.** `detectHost` hashes `omni.ja`
  when the sidecar starts and answers `browser_launch_failed` unless it is
  the patched one, as it already refuses any other Camoufox build.
- **A profile forgets what it compiled under another archive.** Firefox
  keeps the compiled scripts of its own code in the profile
  (`startupCache/`) and trusts them while the build's version and
  directory are unchanged; the backport changes neither. Measured: a
  profile first opened under the unpatched build still runs the old
  script after the build was patched in place, on every open. Each
  profile directory therefore notes the archive it last started under
  (`omni.sha256`), and the engine removes the profile's `startupCache`
  when that is not the archive it runs.

What is not backported: upstream's change also adds a switch that lifts
the popup blocker for an evaluation (`docShell.driverPopupsAllowed`). It
needs the browser's C++ and exists so that `window.open()` inside an
evaluation keeps working without activation. Kage opens no window through
an evaluation.

## Consequences

- An untouched page reports no user activation on Firefox, as on Chromium
  (where the engine reads over its own debugger session, spec §10.2). The
  probe's `automation.user_activation` is a hard failure on both engines.
- Kage runs a browser build that differs from upstream's release of it by
  four statements. The difference is upstream's own, it is reproducible
  from the release archive with a tool in this repository, and the result
  is pinned.
- A flow step with `human: false` (the driver's click) no longer finds a
  page that its own earlier reads had activated: the page reads `isActive`
  false at `mousedown` and true at `click`. The driver's input was visible
  to a page before (spec §10.4); this is one more way to see it.
- An evaluation can no longer do what needs activation (opening a window,
  entering fullscreen, writing the clipboard). Nothing in Kage does.
- At the next Camoufox bump: the tool refuses the new archive, which is
  the reminder to delete it, its two checksums, `CAMOUFOX_OMNI_SHA256` in
  the engine and this rule's check, once the new build is measured to
  grant no activation by itself. `matchProfileToBuild` stays useful for
  any later change of that kind and may stay.
- The Firefox image was not published before (ADR 0008, the fonts) and is
  not now.
