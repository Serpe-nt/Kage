# ADR 0008: AGPL-licensed ua-parser-js in the Firefox engine dependency tree

Status: accepted, option B applied (M5, 2026-10-09), and the effect of
option C on top of it for a second reason; see "Outcome". The original
decision (2026-10-08): option B, with C as the fallback. M5 overrides
`ua-parser-js` to the MIT 1.0.x line and proves the launcher unchanged
under the engine parity suite; if any behavioral difference appears, M5
keeps the upstream dependency and the release workflow stops publishing
the Firefox image (option C). The exception in
`sidecar/license-policy.json` is removed in the same PR that lands B or C.

## Context

M0 added a license gate for the sidecar dependency tree (the plan's allowlist
names "MPL-2.0 for Camoufox pieces", and those pieces are npm packages that
cargo-deny cannot see). The gate found one package outside the allowlist:

- `camoufox-js` 0.12.1, the pinned Firefox launcher (D2, `versions.md`),
  depends on `ua-parser-js ^2.0.10`.
- `ua-parser-js` 2.x is licensed `AGPL-3.0-or-later`. Its 1.0.x line (npm
  dist-tag `legacy`, currently 1.0.41) is MIT.
- `camoufox-js` imports it in `dist/utils.js` (`UAParser`).

Scope of the exposure:

- Only the Firefox sidecar image contains the package. The Chromium sidecar
  image and the `kage` binary do not; each sidecar image installs the
  production dependencies of its own engine only.
- At M0 the Firefox engine is a stub and nothing executes the package. From
  M5 on the engine calls `camoufox-js`, which calls it.

Kage is published under MIT OR Apache-2.0 (D26). A copyleft component inside
one of the published images changes what a recipient of that image may assume
about it. This is a licensing question for the owner; nothing here is legal
advice.

Until the decision is made, `sidecar/license-policy.json` carries one explicit
exception for exactly this package and license, and `just deny` prints a
warning naming this ADR on every run. Any other package outside the allowlist
still fails the gate.

## Options

A. Accept the dependency. Keep the exception, and state in the README that
   the Firefox sidecar image contains an AGPL-3.0-or-later component and is
   distributed under those terms.

B. Override `ua-parser-js` to the MIT-licensed 1.0.x line with a pnpm
   override. `camoufox-js` was written against 2.x, so M5 must prove with its
   engine parity suite that the launcher still behaves identically. An
   override on a fingerprint-critical launcher is a deliberate deviation from
   upstream and has to be re-verified on every `camoufox-js` bump.

C. Do not publish the Firefox sidecar image. The release workflow pushes the
   `kage` and Chromium images only; operators build the Firefox image
   locally. The repository still lists the package in its lockfile.

D. Replace the launcher. M5 already documents a Python fallback for the
   Firefox engine; its dependency tree would need the same license review.

E. Ask upstream to drop or replace the dependency, and stay on option A or C
   until that lands.

## Recommendation

Try B first, in M5, because it removes the problem instead of documenting it.
If the parity suite shows any behavioral difference, fall back to C, which
keeps every published artifact under the project's own license.

## Outcome (M5)

The override is in place: `ua-parser-js` resolves to 1.0.41 (MIT) through
`overrides` in `sidecar/pnpm-workspace.yaml`, the exception is gone from
`sidecar/license-policy.json`, and `just deny` passes with no exception.

The launcher is **not** unchanged under the override. One of its own
functions differs: `launchOptions` derives the claimed system from the
`User-Agent` and tests whether the parsed name starts with `macOS`. The
2.x line answers `macOS`, the 1.x line answers `Mac OS`, so under the
override the launcher would take a macOS persona for a Linux one (Linux
font list, Linux fontconfig, Linux WebGL sample). Windows and Linux are
read the same by both lines.

By the wording of the decision that difference would mean option C. It was
accepted as option B in substance (consult 4 of `m5-consults.md`, answered
by the planning session on the owner's delegation), because nothing Kage
executes reaches the package:

- The Firefox engine does not call `launchOptions` at all. It generates a
  persona from the launcher's parts and starts the browser from the
  persisted fingerprint by its own launch plan, which names the claimed
  system itself (`docs/runbooks/engine-firefox.md` gives the other three
  reasons for that: determinism, a fontconfig path that does not exist in
  the pinned build, and a download at run time).
- The engine never imports the launcher's entry point, so `ua-parser-js`
  is never loaded in a sidecar process. The test "ua-parser-js (ADR 0008)"
  in `sidecar/packages/engine-firefox/src/index.test.ts` generates and
  plans a Windows, a macOS and a Linux persona and then asserts that no
  module of the package was loaded; a second test loads the launcher on
  purpose and pins the differing answer, so that the difference stays
  written down next to the code.
- The engine parity suite runs on the overridden tree.

What this costs: whoever calls `launchOptions` in this tree gets macOS
wrong, and the override has to be looked at again on every camoufox-js
bump (the two tests fail when the launcher starts to load the package on
the paths the engine uses, or when the 1.x line changes its answer).

**The Firefox image is not published all the same**, for a reason this ADR
did not know: the Camoufox build bundles Microsoft's and Apple's font
files (144 and 61 files). They are what makes a Windows or macOS persona's
fonts coherent, and a public image of ours would redistribute them.
`release.yaml` publishes the `kage` and the Chromium image; operators build
the Firefox image where it runs (`just build-images`). This is the
planning session's default on the owner's behalf, flagged to the owner; it
is not legal advice, and it is one matrix entry to reverse.

## Consequences

- The exception in `sidecar/license-policy.json` is removed when this ADR is
  accepted with option B, C or D, and stays, with the README statement, under
  option A.
- `release.yaml` must not publish the Firefox image before this ADR is
  decided. No release is cut before M12, so nothing is published meanwhile.
