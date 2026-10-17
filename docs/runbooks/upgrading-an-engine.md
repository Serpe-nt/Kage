# Upgrading an engine

A new Chrome or a new Camoufox build changes what every persona of that
engine looks like to a site: it is fingerprint-critical
(`docs/versions.md`) and goes in a pull request of its own, never with
anything else and never merged by a bot. The steps of each engine are in
its runbook; this page is what they share and the order to keep.

| Engine | Procedure |
|---|---|
| Chromium (patchright with the pinned Chrome) | [engine-chromium.md, "Moving to a new Chrome"](engine-chromium.md#moving-to-a-new-chrome) |
| Firefox (Camoufox) | [engine-firefox.md, "Moving to a new Camoufox build"](engine-firefox.md#moving-to-a-new-camoufox-build) |

## What every upgrade does

1. **Pin the new build** by version and checksum in
   `deploy/docker/sidecar.Dockerfile` (and, for Camoufox, the constants of
   `engine-firefox/src/host.ts`). The image build and `just setup` read the
   same lines; an engine refuses any build but the pinned one.
2. **Run the sidecar's tests on it**: `just test-node`. The parity suite
   runs the same contract, capture and behaviour tests on both engines,
   in the real browsers.
3. **Record the browser's fingerprint** (`just
   capture-browser-fingerprint [firefox]`) and decide what the new major
   may do, as "A new browser major" in
   [fingerprint-echo.md](fingerprint-echo.md#a-new-browser-major) says:
   whether the http path may stand in for it, or the major is
   browser-only.
4. **Tell the deployment** the new major: `engines.chromium.browser_major`
   or `engines.firefox.browser_major` (`KAGE_CHROMIUM_BROWSER_MAJOR`,
   `KAGE_FIREFOX_BROWSER_MAJOR` in compose). The stack's smoke test fails
   when the image and the setting disagree.
5. **Test the personas**: rebuild the images (`just build-images`), start
   the stack, run `kage persona selftest <id>` on a persona of the engine
   against your own pages, then the live suite once
   ([repo.md](repo.md#nightly-live-suite)).
6. **Write it down**: `docs/versions.md` (the row and the verification
   log) and the changelog.

## On a running deployment

- Personas keep their seeds and their profiles. A Chromium persona's
  document holds no browser version, so it takes the new Chrome as it is.
  A Firefox persona's document belongs to the build it was made with:
  after a new Firefox major the sidecar refuses it
  (`browser_major_mismatch`), and the persona is replaced by a new one
  behind the same proxy (`kage persona regenerate <id>`, which inherits
  nothing; [personas.md](personas.md)).
- Roll out by recreating the sidecar of the engine alone (`docker compose
  up --detach --wait browser-chromium`); kage closes the contexts of that
  engine and opens them again at the next page.
- Keep the previous image tagged until the new one has run a day: going
  back is recreating the sidecar from it.
