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

## Enforcement (F174, 2026-11-06)

QA round two ran a second server on one database (pool-03): a job taken
over after its server was killed hung `running` for good, and spec §16.6
promised such a take-over "by another one". The owner's decision of
2026-11-05 keeps this ADR: Kage stays single-instance, and a second server
is refused at start. A server holds a Postgres session-level advisory lock
on its database for its whole life (`kage_store::ServerLock`), on a
connection of its own outside the pool; `kage serve` on a database whose
lock another session holds exits non-zero and says why. An advisory lock
rather than a heartbeat row with a lease, because Postgres lets the lock go
the moment the holding session ends, however it ended: a server killed and
started again starts at once, with nothing stale to wait out or judge. The
session's TCP keepalives are short, so a host that vanished lets it go
within about a minute. A server that lost its connection takes the lock
again, or stops when another server took it meanwhile. A job's lease still
moves, to the server started after the one that stopped.
