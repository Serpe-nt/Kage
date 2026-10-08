# ADR 0002: Two browser engines behind one contract, Chrome-weighted persona mix

Status: accepted, 2026-10-08

## Context

Camoufox gives the strongest spoofing (C++ level) but is Firefox, which has a
small real-world share; twenty Firefox households behind twenty IPs is a
correlation risk. Patched Chromium matches market share but spoofs at JS level
and must hide CDP. patchright pins playwright 1.63 while camoufox-js requires
playwright-core below 1.63, so they cannot share a package.

## Decision

Both engines are supported. Each lives in its own pnpm package with its own
pins, selected per container by `ENGINE`. Persona engine and OS follow a
configurable mix defaulting to Chrome-majority. Camoufox is labeled not
production-stable by its maintainer, so the Firefox engine is replaceable by
a Python implementation of the same contract, and the mix can be set to 100 %
Chromium by configuration.

## Consequences

- Parity test suite runs every contract test against both engines.
- Engine upgrades are fingerprint-critical changes requiring the live
  self-test suite.
