# ADR 0001: Real browser in an anti-detect sidecar written in TypeScript

Status: accepted, 2026-10-08

## Context

The goal is indistinguishability from a human user. Research
(`bot-detection-landscape.md`) shows every major vendor's decisive signals
live in the JavaScript and behavioral layers, which an HTTP-only client cannot
produce. The best anti-detect tooling (patchright, Camoufox, Apify
fingerprint-suite) is Playwright-based and has no Rust bindings; Rust-native
CDP drivers expose detectable automation artifacts and only JS-level spoofing.

## Decision

Browsers run in a separate Node/TypeScript service exposing a versioned
HTTP/JSON contract. Rust owns everything else (API, scheduling, personas,
proxies, storage). Playwright is Node-native, fingerprint-suite is Node, and
both patchright and camoufox-js publish Node packages.

## Consequences

- Two runtimes to build, test and secure; mitigated by a shared contract with
  generated types and contract tests.
- The engine implementation can be swapped (including to Python) without
  touching Rust.
- Resource planning must account for browser memory per persona context.
