# Rotating keys and secrets

Every secret Kage has, how to give it a new value, and what the change
costs. The secrets of a compose deployment live in `deploy/.env` (mode
600, never committed), which `scripts/dev-secrets.sh` fills with random
values. Kage's own variables for them (`KAGE__…`, spec §17) may instead
name a file, with `_FILE` appended, for a secret mounted by Docker. After
a change, `docker compose --file deploy/compose.yaml up --detach --wait`
recreates what reads it.

| Secret | Where | Rotate | What it costs |
|---|---|---|---|
| An API key | the `api_keys` table, as the SHA-256 of its secret | `kage key rotate <id>` (or `POST /v1/admin/keys/{id}/rotate`): a new secret for the same key, shown once | the old token is dead at once; its holder needs the new one |
| An API key that leaked | | `kage key revoke <id>` when its holder must stop, else rotate; then read what it did: `GET /v1/admin/audit` for admin calls, the `requests` table by `api_key_id` | |
| The sidecar token (`KAGE_SIDECAR_TOKEN`) | `deploy/.env`; kage and both sidecars | a new value (`head -c 32 /dev/urandom \| od -An -tx1 \| tr -d ' \n'`), then `up --detach --wait`: kage and both sidecars are recreated together | the open browser contexts close; each persona's context opens again at its next page, from its profile |
| The master key (`KAGE_MASTER_KEY`) | `deploy/.env`; kage | a new 32 random bytes as 64 hex digits, then `up`. Proxy credentials sealed under the old key no longer open: the next sync with the provider (at start, or `kage proxy list --sync`) seals them again; with a static list, the list is the source | until the sync no proxy can be probed or used. A webhook secret of a job that is still open no longer opens either: its deliveries fail, so rotate between jobs, or create those jobs again ([api.md](api.md#webhooks)) |
| The provider key (`KAGE_WEBSHARE_API_KEY`) | `deploy/.env`; kage | a new key in Webshare's dashboard, then `up`, then revoke the old one there | nothing while the old key still works; replacements and syncs need the new one |
| The static proxy list | `KAGE__PROXIES__STATIC_LIST` or its `_FILE` | change the proxies' passwords at the provider, then the list, then `up` | each proxy is probed again with its new credential |
| A webhook secret | per job, sealed under the master key | a new job with the new secret; a receiver accepts both while the old jobs end | deliveries of a job keep the secret it was created with |
| The database password (`KAGE_POSTGRES_PASSWORD`) | `deploy/.env`; Postgres and kage | `docker compose exec postgres psql --username kage --command "ALTER USER kage PASSWORD '<new>'"`, then the new value in `deploy/.env`, then `up` | kage reconnects once recreated; Postgres reads `POSTGRES_PASSWORD_FILE` only when its volume is empty, so the `ALTER USER` is what changes it |
| The Grafana admin (`KAGE_GRAFANA_ADMIN_PASSWORD`) | Grafana's own database on `grafana-data` | `docker compose --file deploy/compose.yaml --file deploy/compose.observability.yaml --profile observability exec grafana grafana cli admin reset-admin-password '<new>'`, then the same value in `deploy/.env` | none; Grafana reads the variable only on its first start |
| The AbuseIPDB key (`KAGE_ABUSEIPDB_KEY`) | `deploy/.env`; kage | a new key at AbuseIPDB, then `up` | none |

No secret is ever written to a log, a trace, an answer of the API or a
metric label (`tests/e2e/observability.sh` greps every line and span of a
running stack for them). `kage config check` prints the effective
configuration with every secret as `[redacted]`.
