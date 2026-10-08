# ADR 0003: Static residential proxies with one long-lived persona per IP

Status: accepted, 2026-10-08

## Context

The owner buys Webshare Static Residential (ISP) proxies: a fixed list of
IPs with unlimited bandwidth, starting at twenty EU IPs. Rotating residential
gateways were rejected on cost. A real household has one IP, one or two
devices, stable cookies and history, and a stable locale and timezone.

## Decision

Each proxy IP is bound 1:1 to a persona: engine, OS, generated fingerprint,
locale, timezone, geolocation, persistent browser profile and a matching wreq
emulation profile. No identity attribute rotates during a persona's life. A
burned IP is replaced through the provider API within a monthly budget and
its persona is retired and regenerated.

## Consequences

- Pool capacity is small; pacing, cooldowns and a per-domain circuit breaker
  protect it.
- Health and self-tests operate per persona, not per request.
- The pool/rotator model from the Go reference is conceptually reused; its
  direct-connection fallback is not (see ADR 0006).
