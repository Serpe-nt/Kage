# ADR 0005: PostgreSQL via sqlx, no Redis, no ORM

Status: accepted, 2026-10-08

## Context

v1 includes a job queue and a crawl frontier with concurrent workers. SQLite
strains under that; Redis adds a second stateful service for queue semantics
Postgres provides natively (`FOR UPDATE SKIP LOCKED`).

## Decision

Postgres 17 is the only database. sqlx with compile-time checked queries and
in-repo migrations. Artifacts and bodies live on disk, content-addressed,
with retention jobs.

## Consequences

- Single-instance Kage; a second instance would need a Redis-backed
  scheduler (listed as v2).
- sqlx must use `tls-rustls`; `tls-native-tls` would pull OpenSSL and clash
  with BoringSSL (ADR 0007).
