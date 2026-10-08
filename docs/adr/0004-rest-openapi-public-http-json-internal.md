# ADR 0004: REST/JSON with OpenAPI 3.1 publicly, HTTP/JSON contract internally

Status: accepted, 2026-10-08

## Context

Consumers are the owner's own projects and coding agents. A curl-able JSON
API with a machine-readable schema is the lowest-friction interface. The Go
reference attempted gRPC and never finished the protos.

## Decision

Public API: axum, OpenAPI 3.1 generated from Rust types with utoipa, RFC
9457 problem details, per-consumer API keys with scopes. Internal
Rust ↔ sidecar link: HTTP/JSON on the compose network, schemas owned by the
sidecar in zod, OpenAPI emitted and committed, Rust types generated from it,
contract test on drift, NDJSON streaming for flows.

## Consequences

- One schema language per side; drift is caught in CI, not at runtime.
- gRPC can be introduced later for streaming-heavy internal paths without
  changing the public API.
