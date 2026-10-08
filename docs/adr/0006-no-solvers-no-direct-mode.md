# ADR 0006: No external CAPTCHA solvers, no manual solving, never a direct connection

Status: accepted, 2026-10-08

## Context

A coherent browser with human behavior passes silent challenges natively.
External solver services add cost, a third party in the loop, and legal
exposure. Manual solving does not scale. The Go reference fell back to the
host's own connection when proxies were down, which leaks the operator's IP
to targets.

## Decision

Challenges are handled only by the real browser running real behavior.
Visual CAPTCHAs are classified and reported, never solved. If no usable
proxy exists, requests fail; there is no direct mode. Logins with the
owner's accounts are out of scope for v1.

## Consequences

- Some sites will be reported as `captcha` and stay unreachable; that is an
  explicit product decision.
- The scheduler must treat "no eligible persona" as a hard error with a
  clear code, and alerting must cover pool exhaustion.
