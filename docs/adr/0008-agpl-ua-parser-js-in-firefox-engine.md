# ADR 0008: AGPL-licensed ua-parser-js in the Firefox engine dependency tree

Status: proposed, 2026-10-08. Needs an owner decision before M5.

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

## Consequences

- The exception in `sidecar/license-policy.json` is removed when this ADR is
  accepted with option B, C or D, and stays, with the README statement, under
  option A.
- `release.yaml` must not publish the Firefox image before this ADR is
  decided. No release is cut before M12, so nothing is published meanwhile.
