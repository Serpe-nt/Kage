# The API: keys, fetching, sessions, admin

How the public API of M8 is used and operated (spec §16, §19, §20). The
contract is the OpenAPI document: `/openapi.json` on a running server,
`crates/kage-api/openapi.json` in the repository, `kage openapi` on the
command line, and Swagger UI at `/docs` for a key with the `admin` scope.

## Where it listens

`server.bind` is `127.0.0.1:8080`. The compose file runs the server on
`0.0.0.0:8080` inside its container and publishes that port on the host's
loopback (`KAGE_PUBLISH_ADDR`, `KAGE_PUBLISH_PORT`).

The API speaks plain HTTP. Every request but `/healthz`, `/readyz` and
`/openapi.json` needs a key, but a key on a plain connection is a key
anyone on the path can read: to serve other machines, keep the port on
loopback and put a reverse proxy that terminates TLS in front of it. Kage
warns in its log when it is started beyond loopback.

No CORS header is sent, so no page of another origin can read the API from
a browser. `server.cors_allowed_origins` names the origins that may
(`https://console.example.org`: a scheme, a host, an optional port); the
API never allows credentials and never answers `*`.

## The first key

A deployment starts without keys. The first one is made at the command
line, which works on the database and needs no running server:

```bash
docker compose --file deploy/compose.yaml exec kage \
    kage key create --name operator --scope admin
```

It prints the token once: `kage_<keyid>_<secret>`. Kage stores the SHA-256
of the secret and cannot show it again. Send it as `Authorization: Bearer
<token>`.

| Command | What |
|---|---|
| `kage key create --name N --scope S [--scope S …]` | Make a key. `--requests-per-minute` (60), `--max-concurrent` (4), `--daily-page-budget` (none). `--json` for one document. |
| `kage key list [--all]` | The keys, without tokens. `--all` includes disabled ones. |
| `kage key revoke <id>` | Disable a key: it stops authenticating at once. The row and its audit trail stay. |
| `kage key rotate <id>` | A new secret for the same key; the old token is dead at once. |

The same four exist as `POST /v1/admin/keys`, `GET /v1/admin/keys`,
`DELETE /v1/admin/keys/{id}` and `POST /v1/admin/keys/{id}/rotate`.

Scopes are independent. `admin` opens the admin endpoints, `/metrics` and
`/docs`, and nothing else: an operator who also fetches needs `fetch` too.

| Scope | Opens |
|---|---|
| `fetch` | `POST /v1/fetch`, `GET /v1/results/{id}`, `GET /v1/artifacts/{id}`, `GET /v1/policies/domains/{domain}` |
| `sessions` | `/v1/sessions`, and `session_id` in a fetch (beside `fetch`) |
| `jobs`, `crawl` | Nothing yet (M9, M10) |
| `admin` | `/v1/admin/…`, `/metrics`, `/docs` |

A key that leaked: `kage key rotate <id>` when its holder should keep
working, `kage key revoke <id>` otherwise, then read what it did:
`GET /v1/admin/audit` for admin calls, the `requests` table by
`api_key_id` for fetches.

## Limits

| Limit | Default | Answer when reached |
|---|---|---|
| Requests per minute, per key | 60 | `429 rate_limited`, `limit: requests_per_minute` |
| Requests in flight, per key | 4 | `429 concurrency_limited` |
| Pages a day (UTC), per key | none | `429 budget_exhausted`; `Retry-After` is the time to midnight UTC |
| Requests per minute, all keys | `server.requests_per_minute` 600 | `429 rate_limited`, `limit: server.requests_per_minute` |

Every `429` has `Retry-After`. A minute's allowance is a bucket: a key
with 60 a minute may send 60 at once and then one a second. Every fetch a
key starts is a page, whatever came of it; a request that is refused
before it is a fetch (a `4xx` problem) is not. The limits of a key are
part of the key: to change them, make another key.

These limits protect the API. What protects the sites is the scheduler's
pacing (`docs/runbooks/fetching.md`), which no key can raise beyond the
bounds of D9 and no key can touch where a policy locks it.

## Fetching a page

```bash
curl --silent http://127.0.0.1:8080/v1/fetch \
    --header "Authorization: Bearer ${KAGE_KEY}" \
    --header 'Content-Type: application/json' \
    --data '{"url": "https://example.org/listing?page=2"}'
```

The answer is `200` with the result whenever a fetch was made. A block is
not an HTTP error: `outcome` says `challenge_failed`, `forbidden`,
`rate_limited`, …, and `block` says who and how. `outcome:
"provider_error"` with a `reason` means nothing was attempted
(`docs/runbooks/fetching.md` lists the reasons). A `4xx` problem document
means the request cannot be made as asked, and its `code` says why; nothing
was fetched then and nothing counted against the day's pages.

A fetch waits for the persona's pace and for the persona to behave on the
page, so it takes seconds to minutes. One that outlasts
`scheduler.sync_wall_time` (180 s) is answered `202` with the request's id:
it runs on, and `GET /v1/results/{id}` returns `202` until it ended and the
result afterwards. The id of every fetch is also in `X-Request-Id`.

`include` chooses what comes back and what is kept: `dom` (the document as
the browser had it), `raw` (the main response's body), `headers`, `xhr`
(the responses the page's scripts fetched), `screenshot`, `pdf`, `console`.
Without it a fetch returns `dom` and `headers`. What was asked for is also
stored as artifacts for `artifacts.retention_days` (14) and named in
`artifacts`; `GET /v1/artifacts/{id}` streams one. A screenshot is WebP
unless `screenshot.format` says otherwise, except from a firefox persona,
whose browser encodes PNG and JPEG only: read the artifact's
`Content-Type`.

### What a fetch costs, and what a request may shorten

A persona in a browser does what a person does: it arrives, reads, moves
the pointer, scrolls, and leaves. With the shipped policy one page takes
about 35 s (median), which is about 100 pages an hour per persona, and one
in five visits is cut by the 60 s budget of an attempt.

A request may ask for less, within the policy's bounds:

```json
{ "url": "https://example.org/listing?page=2",
  "behavior": { "scroll_depth": 0.3, "dwell_ms": 3000 } }
```

| `behavior` | A page takes (median, p95) | Pages an hour per persona |
|---|---|---|
| none: the policy's (scroll 0.6 to 0.9, a person's dwell) | 35 s, 60 s | about 100 |
| `scroll_depth: 0.3` | 15 s, 42 s | about 200 |
| `scroll_depth: 0.3, dwell_ms: 3000` | 11 s, 35 s | about 260 |
| `scroll_depth: 0, dwell_ms: 3000` | 5 s, 8 s | about 700 |

(Simulated with the behavior model of the sidecar; a site's own load time
comes on top. The pacing profile's gaps between a persona's pages are the
larger part of an hour's count on most sites.)

- `scroll_depth` is taken as given; `0` is no scrolling.
- `dwell_ms` is the middle of what the persona draws from, not a fixed
  time, and never below the policy's `min_dwell` (2 s unless a domain says
  otherwise). The result's `behavior.dwell_ms` is what it dwelt.
- The pointer always moves. There is no bare load: a page that is loaded
  and left within a second and a half is what a script does, and a site
  that sees one learns the persona.
- `timeout_ms` below 15 000 is refused (`timeout_too_short`) when a browser
  may make the attempt: a budget that only fits a bare load would turn
  every visit into one. With `render: "http"` it may be as low as 1 000.
- A policy with `behavior_locked: true` ignores `behavior`, and one with
  `pacing_locked: true` ignores `pacing`; the result's `behavior` and
  `pacing` say what applied and whether the request's wish was ignored.

For a listing that is paged through, `scroll_depth: 0.3, dwell_ms: 3000`
is the setting that still looks like someone skimming. Keep the default
for a site that watches.

### JSON endpoints

When the data is what a page's own scripts fetch, fetch the page and ask
for `include: ["xhr"]`: the browser makes the requests as the page does,
with everything a page's request carries, and the responses come back in
`xhr` (`xhr.content_types` and `xhr.max_bytes` filter them). This works
with every persona.

`render: "http"` with `request_kind: "fetch"` or `"xhr"` sends one request
as a script's, without a browser: with the header order, `Sec-Fetch-*`,
`Accept` and `Priority` of a real `fetch()` or `XMLHttpRequest`, and the
page that makes it as its `Referer`, which the request must name. It needs
a persona with an http path (none in this release, follow-up F30), takes
no other caller header than `Referer`, `Accept` and `Content-Type`, and
refuses what a browser would preflight. Without `request_kind` a request is
a navigation, whatever its URL looks like: Kage never guesses.

### What only the http path sends

A `method` other than `GET`, a `body` and `headers` need `render: "http"`.
Under `browser` and `auto` they are refused (`render_needs_http`) rather
than dropped: a browser render is a plain navigation. Arriving from a page
is `behavior.referer_strategy` in a browser, not a `Referer` header.

Headers that are the persona's identity or the transport's cannot be set
(`header_denied` names the header): `User-Agent`, `Accept-Language`,
`Accept-Encoding`, `Cookie`, `Origin`, `Host`, every `Sec-*` and `Proxy-*`,
and the rest of spec §9's list. To speak another language, ask for a
persona of another country (`persona.countries`).

## Sessions

A session pins requests to one persona, for a visit that has to be one
visitor's: a login wall that was passed, a cart, pages that depend on
earlier ones.

```bash
curl --silent http://127.0.0.1:8080/v1/sessions \
    --header "Authorization: Bearer ${KAGE_KEY}" \
    --header 'Content-Type: application/json' \
    --data '{"id": "proj-a-42", "persona": {"countries": ["DE"]}, "domain": "shop.example.org"}'
```

`201` is a new session, `200` one that was open under that id; without an
`id` the server makes one. Then name it in a fetch: `"session_id":
"proj-a-42"` (the key needs `sessions` and `fetch`).

- Only `POST /v1/sessions` opens a session. A fetch that names one that is
  not open is `404 session_not_found`: it ended (ten idle minutes, six
  hours in all, or its persona was retired), and the persona behind a new
  one would be another visitor. Open it again and decide whether the visit
  can go on.
- A session never moves. A fetch whose `persona` hints, or whose domain's
  policy, exclude the session's persona is `409 session_persona_conflict`
  with the `constraint` that failed; nothing is fetched. Drop the
  constraint or open another session.
- A session's persona that is blocked is not replaced: the result is the
  block.
- Sessions are a key's own; another key's id is not found.

## What a consumer sees, and what it does not

A key without the `admin` scope sees the persona of a result as a handle
(`p_3f9a0c1d2b4e5f60`) with its engine, the system it claims and its
country. The handle is the same for that key every time and different for
every other key, so two consumers cannot tell that they were served by the
same persona. No response to such a key carries a persona's id, a proxy,
an address or an attempt's seed, and no problem document says what failed
inside (the log does, under the request's id).

The values of the cookies a site set are `[redacted]` in `headers` for
every key, with their names and attributes kept: the session a persona
holds with a site stays with the persona. `security.expose_set_cookie:
true` returns them, for a deployment whose only consumer is its operator.

A result and its artifacts belong to the key that fetched them. Another
key gets `404`; a key with `admin` (and `fetch`) reads all of them, with
the persona's real id and the seeds.

## Admin

Every call below is written to the audit trail with the key, the action,
what it was done to and how it ended, a refusal for a missing scope
included. `GET /v1/admin/audit` reads it, newest first (`limit` up to 200,
`cursor` from `next_cursor`); what was done at the command line is in the
same trail with the actor `cli`.

| Endpoint | What |
|---|---|
| `GET /v1/admin/personas[?state=…&all=true]` | The personas; live ones unless asked. |
| `GET /v1/admin/personas/{id}[?fingerprint=true]` | One persona, with its stored fingerprint when asked. |
| `POST /v1/admin/personas/{id}/retire` | Retire it: its sessions end, its cookies and its browser profile go the way of `docs/runbooks/personas.md`. |
| `POST /v1/admin/personas/{id}/regenerate` | Retire it and generate a new one behind the same proxy. |
| `POST /v1/admin/personas/{id}/selftest` | The self-tests, in the server (below). |
| `GET /v1/admin/proxies[?all=true]` | The proxies, never with their user names or passwords. |
| `POST /v1/admin/proxies/{id}/health` | Probe one now. |
| `POST /v1/admin/proxies/{id}/replace` | Ask the provider for a replacement, inside the budget (`docs/runbooks/proxies.md`); the answer says what became of it. |
| `GET /v1/admin/pool` | Proxies by state, personas by state and engine, open browsers, the bandwidth budget. |
| `POST /v1/admin/policies/reload` | Read the policy files again now. A file that is invalid changes nothing and is `422`. |

### A self-test of a persona

`POST /v1/admin/personas/{id}/selftest` tests the http path when the
persona has one and `selftest.http.echo_url` is set, and the browser path
against `selftest.browser.pages`. `{"third_party": true}` uses the echo
and the detection pages that others run instead; they see the address of
the persona's proxy, so that is never done on a timer.

It runs in the server: through the scheduler's gate, through the
forwarder, and in the browser context the persona has open when real work
holds it, which stays open afterwards. This is the way to test a persona
of a running server. The command-line self-tests (`kage persona selftest`)
run in their own process and meet `context_conflict` while the server
holds that persona's browser open, up to `scheduler.context_idle` after
its last page.

A test that fails cools the persona until one passes; one that passes ends
a cooling that waited for it. A path that cannot be tested is `skipped`
and says why.

## Readiness and metrics

`/healthz` answers while the process runs. `/readyz` is `200` when the
database answers, at least one persona is ready behind a usable proxy and
at least one sidecar answers; otherwise `503` with the three checks, and
`503` with `status: "stopping"` from the moment the server is told to
stop, while requests in flight finish (`server.shutdown_grace`, 30 s).

`/metrics` on the API needs a key with the `admin` scope. For a Prometheus
on the compose network set `server.metrics_bind` (for example
`0.0.0.0:9464`): a second listener that serves `/metrics` and `/healthz`
without a key and nothing else. Never publish it on the host.

One scrape carries Kage's own series (spec §20), the personas by state,
the latest browser self-test score of each live persona, and the series
of both sidecars. The API's own: `kage_api_requests_total{route,method,
status}`, `kage_api_request_duration_seconds`, `kage_api_requests_in_flight`
and `kage_api_refusals_total{code}`, which counts what was refused before a
handler ran: a rise of `invalid_key` is someone guessing, a rise of
`rate_limited` a consumer that needs a larger allowance or a slower loop.

## Input limits

| Setting | Default | Answer |
|---|---|---|
| `server.max_body_bytes` | 1 MiB | `413 payload_too_large` |
| `server.body_read_timeout` | 30 s | `408 request_timeout` |
| `server.max_header_bytes`, `server.max_headers` | 16 KiB, 64 | `431` from the HTTP layer, and the connection is closed |
| `server.header_read_timeout` | 10 s | The connection is closed: a client that trickles its headers, or sends nothing, is cut off |
| `server.max_uri_bytes` | 8 KiB | `414 uri_too_long` |
| `server.max_connections` | 1024 | Further connections wait in the accept queue |
| `server.request_timeout` | 30 s | `408 request_timeout`, for every endpoint but a fetch, a self-test and a proxy probe |

## Names and the SSRF guard

A target is refused before anything is fetched when its URL shows that it
is not a public site: a scheme other than http and https, credentials, a
literal address (unless `security.allow_ip_targets`, and never one in a
private, loopback, link-local, CGNAT or metadata range), `localhost`,
`*.local`, `*.internal` and their kin, a name of a single label. Every
redirect hop is checked the same way; a denied hop ends the fetch as
`ssrf_denied` and nothing read from it is returned.

Kage looks no name up by default: the proxy resolves, and a lookup from
this host would tell the name servers of every target who is asking.
`security.resolve_on_host: true` also resolves targets, the redirect hops
of the http path and webhook hosts here, with the system's resolver, and
refuses a name with an address in a denied range or with none. Turn it on
when the proxies are your own and could reach your own networks; leave it
off behind a residential provider.

## When a request is refused

| Code | Meaning |
|---|---|
| `missing_key`, `invalid_key`, `key_disabled` | `401`. A token that does not parse, a key that does not exist and a wrong secret are all `invalid_key`. |
| `scope_missing` | `403`; `scope` names the one the key lacks. |
| `validation_failed` | `422`; `field` names what is wrong. A field the API does not know is one. |
| `invalid_url`, `ssrf_denied` | The target (above). |
| `header_denied`, `header_invalid`, `header_not_placeable` | A caller's header; `header` names it. |
| `render_needs_http`, `render_kind_conflict`, `render_include_conflict` | What was asked does not fit the render mode. |
| `request_kind_needs_referer`, `preflight_required` | A script's request without its page, or one a browser would preflight. |
| `timeout_too_short`, `invalid_pacing` | `minimum_ms` says the least; a pacing profile that does not exist or leaves D9's bounds. |
| `not_available_yet` | `extract`, the cache and `robots: enforce` arrive with M11. |
| `session_not_found`, `session_persona_conflict`, `no_matching_persona` | Sessions (above). |
| `unavailable` | `503`: no proxy pool is configured, or the server is stopping. |
| `internal` | `500`. The cause is in the log under `request_id`, never in the answer. |
