# First start

From a machine with Docker to a first page fetched through a persona. The
whole stack is built from this repository; nothing but Docker, git and
curl is needed on the host.

## The machine

- Linux (or WSL2 on Windows), Docker Engine 27 or newer with Compose v2,
  git, curl.
- 4 cores and 8 GB of memory for the stack as shipped (two sidecars of
  4 GB each at most, kage 1 GB, Postgres 1 GB); 20 GB of disk for the
  images, the browser profiles and the stored pages.
- The first build compiles Kage and downloads Chrome and Camoufox (about
  700 MB): count 20 to 40 minutes.

## 1. The code and the secrets

```bash
git clone https://github.com/Serpe-nt/Kage.git kage && cd kage
git checkout v1.0.0
scripts/dev-secrets.sh
```

`scripts/dev-secrets.sh` writes `deploy/.env` (mode 600, ignored by git)
with random values for the database password, the shared secret of Kage
and its sidecars, the master key that encrypts proxy credentials at rest,
and the Grafana admin of the observability profile. Running it again
keeps what is there. Keep the file: a new `KAGE_MASTER_KEY` leaves the
stored proxy credentials sealed until the next sync with the provider
([rotating-keys.md](rotating-keys.md)). Never commit it.

## 2. The proxies

Kage fetches only through proxies, one persona per proxy
([proxies.md](proxies.md)). Give it one of the two:

- **Webshare**: add `KAGE_WEBSHARE_API_KEY=<key>` to `deploy/.env`. The
  pool is the account's static residential proxies.
- **A static list**: the list in `KAGE__PROXIES__STATIC_LIST` (one
  `host:port:username:password:country` per line), or in a file named by
  `KAGE__PROXIES__STATIC_LIST_FILE`, given to the `kage` service.

Kage takes Webshare when its key is set, else the static list
(`proxies.provider: auto`).

Without either the stack starts, the pool stays empty, and every fetch is
answered without an attempt.

## 3. The stack

```bash
docker compose --file deploy/compose.yaml up --build --detach --wait
curl http://127.0.0.1:8080/healthz
curl http://127.0.0.1:8080/readyz
```

`migrate` applies the schema and exits; `kage` starts after it. The API
is published on `127.0.0.1:8080` only; to serve other machines, put a
reverse proxy that terminates TLS in front of it
([api.md](api.md#where-it-listens)). `/readyz` says `ready` once the
database answers, a sidecar is up and a persona is ready.

The pool and the personas build themselves: within a few minutes every
proxy is health-checked and gets a persona, which then warms up for a few
hours by visiting popular neutral sites, as a new household browser
would ([personas.md](personas.md)).

```bash
docker compose --file deploy/compose.yaml exec kage kage proxy list
docker compose --file deploy/compose.yaml exec kage kage persona list
```

## 4. The first key and the first page

```bash
docker compose --file deploy/compose.yaml exec kage \
    kage key create --name operator --scope admin --scope fetch
```

The token (`kage_<id>_<secret>`) is shown once and stored only as a hash.

```bash
curl http://127.0.0.1:8080/v1/fetch \
    --header "Authorization: Bearer <token>" \
    --header 'Content-Type: application/json' \
    --data '{"url": "https://example.org/"}'
```

Pick a site you may fetch. The answer is the result: its `outcome`, the
page's status and headers, what was captured, and how the visit went.
[api.md](api.md) is the guide to what a request may ask for, jobs,
crawls, sessions and the admin endpoints; [fetching.md](fetching.md) says
what happens on the way.

## 5. Watching it

```bash
docker compose --file deploy/compose.yaml --file deploy/compose.observability.yaml \
    --profile observability up --detach --wait
```

Grafana on `http://127.0.0.1:3000`, user `admin`, the password
`KAGE_GRAFANA_ADMIN_PASSWORD` of `deploy/.env`: dashboards, alerts and the
traces of every request ([observability.md](observability.md)).

## Every day after

| Task | Where |
|---|---|
| More proxies, a proxy that is down, an address to replace | [proxies.md](proxies.md) |
| A persona that fails its self-test, retiring one | [personas.md](personas.md) |
| A site that blocks | [block-report.md](block-report.md) |
| A key that leaked, rotating the secrets | [rotating-keys.md](rotating-keys.md) |
| A new Chrome or Camoufox | [upgrading-an-engine.md](upgrading-an-engine.md) |
| Stopping and upgrading Kage | [fetching.md](fetching.md#stopping-the-server), the release notes in `CHANGELOG.md` |

The database is the volume `pgdata`: back it up with `docker compose exec
postgres pg_dump --username kage kage`. The stored pages are in
`artifacts`, the browser profiles in `personas`.
