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
| `jobs` | `POST /v1/jobs` for a batch or a flow, `GET /v1/jobs/{id}`, its items, `POST /v1/jobs/{id}/cancel` |
| `crawl` | `POST /v1/jobs` for a crawl (beside `jobs`) |
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
bounds of D9 and no key can touch where a policy locks it. A request at
level 0 has no persona to pace: the site's own limits (its concurrency
and requests per minute, the breaker's pause) and these hold it, and a
site's `min_level` keeps it from the sites that should not see it.

## Fetching a page

```bash
curl --silent http://127.0.0.1:8080/v1/fetch \
    --header "Authorization: Bearer ${KAGE_KEY}" \
    --header 'Content-Type: application/json' \
    --data '{"url": "https://example.org/listing?page=2", "level": 3}'
```

**Every request names `level`**, 0 to 3, and one without it is refused
(`422 level_required`): 0 a plain HTTP client that says it is Kage, with
no persona; 1 a persona's http client (not available in this release,
`level_unavailable`); 2 a persona's browser that loads the page and
leaves; 3 a persona's browser as a person. Below 3 Kage is not pretending
to be a person. A level below the site's `min_level` is refused
(`level_below_policy`), never raised, and nothing escalates: a block is
never retried at another level. What each level costs, what it refuses
and where its traffic leaves from is in
[levels-and-egress.md](levels-and-egress.md). The rest of this section is
level 3 unless it says otherwise.

The answer is `200` with the result whenever a fetch was made. A block is
not an HTTP error: `outcome` says `challenge_failed`, `forbidden`,
`rate_limited`, …, and `block` says who and how. `outcome:
"provider_error"` with a `reason` means nothing was attempted
(`docs/runbooks/fetching.md` lists the reasons). A `4xx` problem document
means the request cannot be made as asked, and its `code` says why; nothing
was fetched then and nothing counted against the day's pages. The result
says the `level` it was made at; at level 0 its `persona` is null, and an
attempt's `persona` and `engine` are null and its `exit` says where it
left from (`pool` or `host`, never an address). There is no `escalated`
any more.

A fetch waits for the persona's pace and for the persona to behave on the
page, so it takes seconds to minutes. One that outlasts
`scheduler.sync_wall_time` (180 s) is answered `202` with the request's id:
it runs on, and `GET /v1/results/{id}` returns `202` until it ended and the
result afterwards. The id of every fetch is also in `X-Request-Id`.

`include` chooses what comes back and what is kept: `dom` (the document as
the browser had it), `raw` (the main response's body), `headers`,
`extract` (what the page says, below), `xhr` (the responses the page's
scripts fetched), `screenshot`, `pdf`, `console`.
Without it a fetch returns `dom` and `headers` at level 2 and 3, `raw`
and `headers` at 0 and 1, where `dom`, `xhr`, `screenshot`, `pdf` and
`console` are refused (`level_include_conflict`): there is no browser. What was asked for is also
stored as artifacts for `artifacts.retention_days` (14) and named in
`artifacts`; `GET /v1/artifacts/{id}` streams one. A screenshot is WebP
unless `screenshot.format` says otherwise, except from a firefox persona,
whose browser encodes PNG and JPEG only: read the artifact's
`Content-Type`.

### What a fetch costs, and what a request may shorten

At level 3 a persona in a browser does what a person does: it arrives,
reads, moves the pointer, scrolls, and leaves. With the shipped policy a
page of three screens takes about 33 s (median), which is about 100 pages
an hour per persona. At level 2 it loads the page, sends no input and
leaves after the policy's `min_dwell` drawn upward (one and a half times
it at the median); `behavior` is refused there. The rest of this section
is level 3's.

How long it stays is its own. It scrolls to the depth the policy or the
request names, or until its **patience** with the page is used up,
whichever comes first, and then dwells. Kage draws the patience anew for
every page (`browser.patience`: at least 6 s, 38 s in the middle, 110 s
for one page in twenty, never more than 4 min) and no request sets it. A
page of a screen and a half takes about 16 s, one of three screens 33 s,
one of six 45 s; a page of ten screens or of a hundred takes about 45 s
too (one in twenty over 100 s) and is left partway, at another moment
every time. The result's `behavior` says what the patience was
(`patience_ms`), how far down the page was when the scrolling ended
(`scroll_reached`) and what ended it (`scroll_ended_by`: `depth`,
`patience`, or `page` for a page that does not go further).

`timeout_ms` is the time to **reach** the page: the navigation, its waits
and a challenge's time to resolve. It bounds nothing after that. Until M10
the scrolling shared it, and every page longer than a minute's reading was
left at the same instant, 56 s after it was asked for: a signature. If you
need the bottom of a tall page, do not look for a longer timeout: use a
flow with a `scroll` step (see Flows), which goes where it says.

A request may ask for less, within the policy's bounds:

```json
{ "url": "https://example.org/listing?page=2", "level": 3,
  "behavior": { "scroll_depth": 0.3, "dwell_ms": 3000 } }
```

| `behavior` | A page takes (median, p95) | Pages an hour per persona |
|---|---|---|
| none: the policy's (scroll 0.6 to 0.9, a person's dwell) | 33 s, 59 s | about 100 |
| `scroll_depth: 0.3` | 15 s, 42 s | about 200 |
| `scroll_depth: 0.3, dwell_ms: 3000` | 11 s, 35 s | about 260 |
| `scroll_depth: 0, dwell_ms: 3000` | 5 s, 8 s | about 700 |

(Simulated with the behavior model of the sidecar on a page of three
screens, 800 visits each; a site's own load time comes on top. The pacing
profile's gaps between a persona's pages, and since M10 its rest between
leaving a page and asking for the next (0.7 s at least, 2.2 s in the
middle under `human`), are the larger part of an hour's count on most
sites.)

- `scroll_depth` is taken as given; `0` is no scrolling.
- `dwell_ms` is the middle of what the persona draws from, not a fixed
  time, and never below the policy's `min_dwell` (2 s unless a domain says
  otherwise). The result's `behavior.dwell_ms` is what it dwelt.
- At level 3 the pointer always moves. A page that is loaded and left with
  no input is level 2, which the caller asks for by name: a site that
  watches learns from it that the visitor is a script, so give such a site
  `min_level: 3`.
- `timeout_ms` below 15 000 is refused (`timeout_too_short`) at level 2
  and 3: a page that is slow to arrive needs that much. At level 0 and 1
  it may be as low as 1 000.
- A fetch may therefore take longer than its `timeout_ms`: reaching the
  page, then up to the patience, then the dwell. One that outlasts the
  server's wall time is carried on as a job (below), as before.
- A policy with `behavior_locked: true` ignores `behavior`, and one with
  `pacing_locked: true` ignores `pacing`; the result's `behavior` and
  `pacing` say what applied and whether the request's wish was ignored.

For a listing that is paged through, `scroll_depth: 0.3, dwell_ms: 3000`
is the setting that still looks like someone skimming. Keep the default
for a site that watches.

### What a page says: `extract`

`include: ["extract"]` returns what the page says beside (or instead of)
its markup:

```bash
curl --silent http://127.0.0.1:8080/v1/fetch \
    --header "Authorization: Bearer ${KAGE_KEY}" \
    --header 'Content-Type: application/json' \
    --data '{"url": "https://example.org/article", "level": 3, "include": ["extract"]}'
```

`extract` holds the `title`, `description`, `canonical`, `lang`, when the
page says it was published and changed, its `meta`, `opengraph`,
`twitter` and `jsonld`, its `links` (absolute, with `same_site`), and its
main content: `headings`, `text`, `markdown`, `word_count`. It is read
from the document the visit captured, so it costs no second request and
nothing a site could see. `extract: { "markdown": false, "links": false }`
leaves either out.

- The main content is what a reader would read: Firefox's reader-view
  rules, without navigation, side bars, footers and bylines. On a page
  that is a list of the same thing (a thread, search results) every one
  of them is content.
- `thin_content: true` says the main content has fewer than
  `extract.thin_words` (80) words. At level 0 or 1 that usually means
  the page is built by scripts or a wall stands in front of it: fetch it
  at level 2 or 3. The result does not guess which.
- `extract` is null when the fetch did not get its page (a block, a
  failure) and for a response that is no HTML document (a PDF, JSON).
- `title` is the document's `<title>`, as written; the headline is in
  `headings`, what the page says to social networks in `opengraph`.
- A time with a zone is given in UTC; one without is given as the page
  wrote it. Kage invents no zone.

### The same page again: `cache_ttl_s`

A request with `cache_ttl_s` may be answered from the response cache:

```bash
curl --silent http://127.0.0.1:8080/v1/fetch \
    --header "Authorization: Bearer ${KAGE_KEY}" \
    --header 'Content-Type: application/json' \
    --data '{"url": "https://example.org/prices", "level": 3, "include": ["extract"], "cache_ttl_s": 600}'
```

The first one is fetched and kept for ten minutes. A later request that
asks for the same and accepts a result up to its own `cache_ttl_s` old is
answered with it: `cached: true`, `cached_at` (when the page was
fetched), `attempts: []`. Nothing is asked of the site, no persona
visits, the page does not count against the key's daily budget, and the
answer comes while the site is paused or every persona sleeps. It still
counts against the key's requests per minute. `behavior`, `console` and
`side_effects` of such an answer are those of the visit that got the
page (how far it was scrolled matters for what the document holds);
`pacing` is the request's own, with `waited_ms: 0`.

- **"The same" is strict.** Another `include` set, another `level`,
  other `wait`, `behavior`, `extract`, `screenshot`, `pdf`, `xhr` or
  persona hints, another method, body or header: each is another entry.
  No entry of 1.x is served (the key is at version 2 since 2.0).
  A request for fewer parts is not served from a result with more.
  `timeout_ms`, `max_retries`, `pacing` and `robots` do not matter.
- **A key's entries are its own.** Another key's identical request is a
  visit, unless the operator set `cache: { shared: true }` for the site
  (a site that shows everybody the same). Across keys the result does not
  say who fetched it (`persona: null`).
- **Only good pages are kept**: `ok` or `challenge_passed` with a `2xx`,
  and not a page extraction called thin. A block is never served twice.
- `cache_ttl_s: 0` is "never from the cache", whatever the site's policy
  says. More than the site allows (`cache.max_ttl`, a day unless
  configured, never longer than results are kept) is refused with
  `cache_ttl_too_long`; `maximum_s` says the most.
- A request in a session is never answered from the cache and never
  fills it, and says so when it asks (`validation_failed`).
- A crawl's pages fill the cache (when its `defaults` name
  `cache_ttl_s`) and are never answered from it: a page that is not
  visited is not open in the tab, and its links cannot be clicked. A
  second crawl of a site visits every page again.
- A flow has no part in the cache.
- Two identical requests at the same moment are one visit: the second
  waits for the first and is answered from what it stored (`cached:
  true`, `cached_at` the first one's). When the first one's page is none
  the cache keeps, the second makes its own visit.

An operator sets the bounds: `cache.max_ttl` in `kage.yaml` for every
site, and per site in `policies/domains.yaml`
(`cache: { shared, max_ttl, ttl }`, where `ttl` is what a request gets
that does not name `cache_ttl_s`). `kage_cache_requests_total{result}`
counts `hit`, `miss`, `stored`, `bypass` (a result the cache does not
keep) and `joined` (a request that waited for an identical one under
way; it is then a `hit` or, when that one kept nothing, a `miss`).

### `robots.txt`

**Nobody asks for a site's `robots.txt` unless its policy says so.** No
reader opens that file, so the request marks the address that makes it.
The shipped default is therefore `robots: ignore`: nothing is asked, and
every result says `"robots": { "fetched": false, "allowed": true }`. The
operator chooses per site whether a verdict is worth one marked address
(`policies/domains.yaml`):

```yaml
domains:
  "example.org":
    robots: enforce   # or report
```

- `report`: the file is asked for and every result says what it says of
  the address (`allowed`, `matched_rule`, `crawl_delay`); the page is
  fetched either way. Nothing waits: the first results of an origin say
  `fetched: false`, and the verdict is there from the next one on.
- `enforce`: what the file disallows is not fetched. The answer is a
  result with `outcome: "robots_denied"`, the rule in
  `robots.matched_rule`, no attempts, and nothing asked of the site. The
  first fetch of an origin waits for the file (about 8 s at the median, a
  visit like any other). The file's `Crawl-delay` is kept to: each
  persona leaves at least that long between two pages of the host, and a
  site that asks for an hour gets an hour. If that is too slow, `report`
  is the way out; Kage does not half-obey.
- **Who asks** is the site's scout: one persona per site that asks for
  the file (and for sitemaps, in a crawl) from its own address, in its
  own browser, as a person who typed the address, and never reads a page
  of that site. One address per site is marked, not two. A site
  therefore needs a persona to spare: with a single persona, or when
  every persona has read the site within the last 30 days
  (`robots.scout_quarantine`), nobody asks (`reason: "robots_no_scout"`
  under `enforce`). After the 30 days a persona that read the site may
  become its scout. A site's only Firefox is not taken as its scout while
  another persona can ask. For a request at level 0 the file is asked for
  by the plain client from the site's plain exit, which under the pool is
  the scout's proxy; under `egress.mode: direct` from the host, where a
  lone host persona is its own scout
  ([levels-and-egress.md](levels-and-egress.md)).
- The file is kept for about a day (`robots.ttl`) and asked for again
  only when a request needs it. A `404` is "no file": everything is
  allowed. A server error, a timeout or a block is "unreachable" for an
  hour (`robots.error_ttl`): under `enforce` nothing of the origin is
  fetched meanwhile (`reason: "robots_unavailable"`, with `retry_at`), a
  job's pages of that site wait (`paused`, `robots_unavailable`).
- Kage answers to the group of `*`, and to a group that names `kage`
  (`robots.agents`) in its place: a site owner who wants to address Kage
  can. A scout's request carries its browser's `User-Agent`; a plain
  client's (level 0) carries `kage/<version>` or the operator's
  `egress.plain_user_agent`.
- A request may say `robots` itself: `enforce` where the policy reports
  is honoured. Less than the policy enforces is refused
  (`robots_enforced`), and so is `enforce` where the policy never asks
  for the file (`robots_ignored`): a request does not make Kage ask a
  site the operator left alone.
- To make Kage ask again before the day is over, delete the origin's row
  from `robots_cache`; it is noticed within half a minute.
- A scout that is not ready (its proxy is down, it is cooling) closes a
  site that enforces its file until it is back: its fetches answer
  `robots_unavailable` and a job's pages of the site wait. To give the
  site another scout, `POST /v1/admin/sites/{site}/scout` (any host of
  the site will do). The answer names the new scout and the one before,
  which from then on reads the site like any persona that asked it. One
  address more has then asked the site for its file: do it for a scout
  that will not be back soon. `409 no_matching_persona` says nobody else
  may ask, and the site keeps its scout; retiring the scout also frees
  the site, and the next fetch chooses a new one.

`kage_robots_requests_total{result}` counts what came of asking (`file`,
`missing`, `unreachable`, `not_asked` for a proxy or sidecar failure,
`no_scout`), `kage_robots_denied_total{reason}` what was not fetched.

### When a fetch takes longer than the server waits

A fetch that outlasts `scheduler.sync_wall_time` (180 s) is answered `202`
with the request's id and a `job_id`: from then on a job of one item
carries it on (`origin: sync_overflow`). Poll `GET /v1/results/{id}`; that
needs no more than the `fetch` scope. With the `jobs` scope the job itself
can be read (`GET /v1/jobs/{job_id}` says when it is `paused` and for
what) and cancelled. Such a fetch no longer ends with an error because its
site was paused or its persona asleep: it waits, as a job waits, for up to
`jobs.max_age`. If the server is stopped while it runs, the job worker of
the next start takes it up again under the same request id.

### JSON endpoints

When the data is what a page's own scripts fetch, fetch the page and ask
for `include: ["xhr"]`: the browser makes the requests as the page does,
with everything a page's request carries, and the responses come back in
`xhr` (`xhr.content_types` and `xhr.max_bytes` filter them). This works
with every persona, at level 2 and 3.

`level: 1` with `request_kind: "fetch"` or `"xhr"` sends one request
as a script's, without a browser: with the header order, `Sec-Fetch-*`,
`Accept` and `Priority` of a real `fetch()` or `XMLHttpRequest`, and the
page that makes it as its `Referer`, which the request must name. Any
other level refuses it (`level_kind_conflict`). It needs a persona with an
http path (none in this release, follow-up F30: level 1 is
`level_unavailable`), takes no other caller header than `Referer`,
`Accept` and `Content-Type`, and refuses what a browser would preflight.
Without `request_kind` a request is a navigation, whatever its URL looks
like: Kage never guesses. A JSON endpoint that anybody may read can be
asked for at level 0, as a plain client.

### What only the http path sends

A `method` other than `GET`, a `body` and `headers` need level 0 or 1.
At level 2 and 3 they are refused (`level_needs_http`) rather than
dropped: a browser visit is a plain navigation. Arriving from a page is
`behavior.referer_strategy` in a browser, not a `Referer` header.

At level 1, headers that are the persona's identity or the transport's
cannot be set (`header_denied` names the header): `User-Agent`,
`Accept-Language`, `Accept-Encoding`, `Cookie`, `Origin`, `Host`, every
`Sec-*` and `Proxy-*`, and the rest of spec §9's list. To speak another
language, ask for a persona of another country (`persona.countries`).

At level 0 there is no persona to protect: the headers are the caller's
request as given, `User-Agent`, `Cookie` and `Authorization` included.
Only the transport's own, `Proxy-*`, the forwarding family
(`X-Forwarded-*`, `Forwarded`, `X-Real-IP`, …) and `Accept-Encoding` are
refused. A caller's `Cookie` and `Authorization` go to the request's
origin only, never across a redirect to another, are never logged, and
the stored request keeps `[redacted]` in their place; a job refuses them
(`header_denied`). See
[levels-and-egress.md](levels-and-egress.md#a-callers-cookie-and-authorization-at-level-0).

## Jobs

`POST /v1/jobs` accepts work and answers at once; the work is done in the
background and read with `GET /v1/jobs/{id}` and
`GET /v1/jobs/{id}/items`. It needs the `jobs` scope. A job is a `batch`
(pages you name), a `flow` (steps in one browser) or a `crawl` (a site
read along its links).

```json
{ "type": "batch",
  "urls": ["https://example.org/listing?page=1", "https://example.org/listing?page=2"],
  "defaults": { "level": 3, "include": ["dom"], "behavior": { "scroll_depth": 0.3, "dwell_ms": 3000 } } }
```

Every item ends with a level: its own or the batch's `defaults.level`;
one that has neither refuses the job (`level_required`). Items of one
batch may differ in level. A job keeps its items until they run, so it
refuses a caller's `Cookie` or `Authorization` header (`header_denied`).

A job is accepted whole or not at all: one item the API would refuse as a
fetch refuses the job (`item` in the problem says which), and nothing is
stored. Send an `Idempotency-Key` header with every submission you might
repeat: the same request under the same key within a day is answered
`200` with the job it made the first time (`Idempotency-Replayed: true`),
so a client that lost the answer does not start the batch twice.

### What a batch costs

Per site a batch has **one visitor**: one persona reads the site's pages
in the order given, in sittings, with a person's pauses, and comes back
after its breaks and its nights. Another persona takes over only when that
one is blocked there, reaches its daily cap or is gone. The site sees one
household reading. That is slow on purpose:

| Batch, one site, `human` pacing | Takes | Addresses the site sees |
|---|---|---|
| 50 pages, one visitor (the default) | about an hour (median 61 min) | 1 |
| 300 pages, one visitor | a working day (about 8 h): the daily cap of one persona | 1 |
| 50 pages, `"visitors": 3` | about 20 minutes | 3 to 4 |
| 200 pages, `"visitors": 3` | about an hour (median 61 min) | 3 |

(Simulated with the scheduler's own planner over 30 seeds, the last row
over 200; a batch that meets the night ends in the morning.)

Whatever the pace, `domain_rpm` caps the pages begun on a site within a
minute as Kage begins them. The site may see a request or two more inside
a sliding minute: between the start and the request a persona types the
address and loads the page, in a time of its own. The load test measured
it at the cap of 20 a minute: 21 starts in a row never within 60 s at
Kage, within 57.3 s at the site.

`visitors` is the way to trade stealth for time: up to the
`domain_concurrency` of the pacing in force for the site (3 under
`human`), refused beyond it and never reduced, and refused at level 0,
which has no persona. Sites of one batch are
independent and are read side by side, each by its own visitor. A faster
`pacing` does the rest where the policy does not lock it. Every page
counts against the key's daily page budget when it is begun.

A job-level `session_id` pins every item to an open session's persona
(the `sessions` scope and a fetch's session rules apply); the job counts
as use of the session while it has items left.

### A job that waits

An item that cannot be attempted now is not failed. When the site is
paused by its breaker, the visitor sleeps, the bandwidth term is used up,
the key's daily page budget is spent, or at level 0 the site's plain exit
cannot carry traffic (`plain_exit_unavailable`), the item goes back into
the queue, and a job whose every remaining item waits is `paused`:

```json
{ "state": "paused", "paused": { "reason": "domain_paused", "until": "2026-10-12T06:40:00Z" },
  "items": { "total": 50, "succeeded": 31, "failed": 0, "cancelled": 0, "pending": 19 } }
```

It is taken up again by itself at that time. A job waits at most
`jobs.max_age` (7 days) from its acceptance (`deadline`); past it the
items left fail with `expired: <reason>` and the job ends `partial`.
`POST /v1/jobs/{id}/cancel` ends it earlier: what waits is cancelled at
once, the page that is being fetched is finished, and nothing more is
begun.

### Flows

A flow is a script of steps in one browser, by one persona:

```json
{ "type": "flow",
  "level": 3,
  "actions": [
    { "action": "goto", "url": "https://example.org/" },
    { "action": "type", "selector": "input[name=q]", "text": "lamp" },
    { "action": "press", "key": "Enter" },
    { "action": "wait_for", "selector": "#results" },
    { "action": "extract", "include": ["dom", "extract"] } ] }
```

- It names `level` 2 or 3: a flow is a browser. At level 2 it takes no
  `behavior` and rests the hand while it dwells.
- It starts with a `goto`. Every `goto` is checked by the SSRF guard when
  the job is accepted, and every page the flow visited is checked again
  afterwards; one refused address and nothing of the flow is kept.
- It is always paced: the persona stays on every page it arrives on
  (around `behavior.dwell_ms` when given, its own dwell of about 6 s
  otherwise, never below the policy's `min_dwell`) and pauses before every
  action. Nothing turns that off. Budget for it: `timeout_ms` is the
  whole flow's (3 minutes unless said, at most 5), and a flow with ten
  page loads at the persona's own dwell needs more than a minute for the
  dwells alone.
- Steps are done by the persona's own mouse and keyboard. `human: false`
  (the browser's driver, which a page can tell apart) is refused with
  `driver_input_denied` unless the site's policy says
  `driver_input: true`. Say so only for a site that does not look.
- A wall met on the way is retried by another persona from the first
  step; a step that fails otherwise (a selector that is not there, an
  assertion) ends the flow and is not tried again.
- The items of the job are the steps, each with `step.ok`,
  `step.duration_ms` and `step.paced_ms`; an `extract`, `screenshot` or
  `pdf` step names a result of its own; the last item is the page the
  flow ended on.

### Crawls

A crawl reads a site from its seeds along its links. It needs the `crawl`
scope besides `jobs`.

```json
{ "type": "crawl",
  "seeds": ["https://example.org/"],
  "deny": ["/login", "\\?sort="],
  "max_depth": 3,
  "max_pages": 50,
  "defaults": { "level": 3, "include": ["dom", "headers"] } }
```

| Field | Default | |
|---|---|---|
| `seeds` | | 1 to 100 addresses. A seed is read whatever `allow` says. |
| `allow`, `deny` | none | Regular expressions on the normalized address, at most 32 each. `deny` wins; a seed it matches refuses the job. |
| `max_depth` | 3 | Links from a seed, by the shortest way the crawl finds; at most 32; 0 reads the seeds only. |
| `max_pages` | 100 | Pages asked of the sites, a failed one included; at most `jobs.max_items`. |
| `same_site_only` | true | A site is a registrable domain: `shop.example.org` and `www.example.org` are one. |
| `use_sitemaps` | false | See below: a sitemap request is a tell. |
| `defaults` | | What every page is fetched with: `level` (required), `include`, `pacing`, `behavior`, `persona`, … Not `url`, `session_id`, `method`, `body`, `request_kind`. At level 0 every page is typed, its links are read from the markup, and `robots.txt` and sitemaps are asked for by the plain client from the site's plain exit. |
| `visitors` | 1 | Personas that read one site at a time; at most the `domain_concurrency` of the pacing in force, refused beyond, and refused at level 0. |
| `session_id` | | An open session: its persona reads every page. |

**It reads like a reader, and takes a reader's time.** The first page is
typed. After that the persona follows a link in the text of the page it
has open, goes back up to three pages for one when the open page has none
left (as a reader goes back to a listing for the next article), takes a
link of the menu when the text within reach is exhausted, and types an
address only when its tab leads nowhere new. In a browser every page is scrolled and dwelt
on like any other and the persona rests before the next one. A crawl
therefore costs what a batch of as many pages of one site costs (see
"What a batch costs"), and a crawl that meets its visitor's night goes on
in the morning: it is `paused` meanwhile, with the reason and the time. A
faster `pacing` in `defaults` and more `visitors` buy time with stealth,
as in a batch. A crawl has no breadth-first mode: on a shop or a blog that
order types four addresses of five, each arriving from nowhere, which no
reader does.

**What a job shows.** A crawl has no items when it is accepted; one is
added for every page it asks for, in that order, so `items.total` grows
to at most `max_pages`. The job's `crawl` counts what it discovered,
fetched, failed and skipped by reason (`off_site`, `too_deep`, `denied`,
`not_allowed`, `not_a_page`, `hidden`, `already_read`, `ssrf_denied`),
its duplicates, how its pages were reached, and `ended`: `completed`
(nothing inside its scope is left), `max_pages`, `cancelled` or `expired`.
An item's `crawl` says the page's depth, where its link was first seen,
how it was reached (`typed`, `click`, `back_click`, or `referer` on the
http path), its content hash, and what it repeats:

```json
{ "seq": 7, "state": "succeeded", "url": "https://example.org/articles/b?utm_source=feed",
  "crawl": { "depth": 2, "found_by": "link", "found_on": "https://example.org/articles",
             "reached": "click", "backs": 0,
             "content_hash": "9f2c…", "duplicate_of": null,
             "near_duplicate_of": { "url": "https://example.org/articles/b-print", "bits": 4 },
             "links": { "found": 12, "new": 3 },
             "scroll": { "patience_ms": 41200, "reached": 0.62, "ended_by": "depth" } } }
```

- A page that says word for word what an earlier one says is a duplicate
  (`duplicate_of`); it is fetched and stored, and its own links are not
  followed, they are the original's. A page that says nearly the same
  (`near_duplicate_of`, at most 6 of 64 bits apart) is flagged and read on
  from. What is compared is the text without what the site repeats on
  every page.
- A page is one address however it is spelled: tracking parameters, the
  order of the query and the fragment do not make a second one, and the
  end of a redirect is not asked for again under its own address. `www.`
  and `http`/`https` are not folded.
- A page the site answers with an error status (a `404`) is an item that
  succeeded as a fetch, with the status in its result, and is counted as
  `failed` by the crawl: nothing is taken from what an error page links
  to.
- A page reached by a click is asked for under the address the page it
  was clicked on gives, so an item's `url` may carry another campaign tag
  than the address the crawl first met.
- A cancel finishes the page that is being read and asks for no other.

**Links nobody can see are not followed.** In a browser the crawl takes
only links a reader can see: rendered, with a box, on the page, not
transparent. The others are counted as `hidden` and never asked for; a
link that only a script would find is the oldest trap there is. At level
0 and 1 nothing is rendered, so only what the markup itself hides is
known (`hidden`, an inline `display: none` or `visibility: hidden`); a
link hidden by a stylesheet or moved off the page is followed there. Crawl
a site that sets traps at level 2 or 3.

**A sitemap request is a tell.** No reader opens `/sitemap.xml`. With
`use_sitemaps` Kage asks for it anyway, so it is not the site's reader
that does: another persona (a scout) reads the sitemaps first, in its own
browser, from its own address, and never reads a page for the crawl. The
site still sees that somebody asked. Use it when the pages you need are
linked from nowhere, not as a habit.

- A scout is a Firefox persona when one is free: Chrome saves a gzipped
  sitemap (`.xml.gz`) as a file and shows nothing of it, and such a file
  is then `sitemap_unreadable`. Keep a Firefox persona in the mix if you
  rely on gzipped sitemaps.
- With a single usable persona nothing is asked (`no_scout`): the reader
  never fetches a sitemap.
- What a sitemap names are places to start from, inside `allow`, `deny`,
  `same_site_only` and `max_pages`; `lastmod` is shown and orders nothing.
  At most `crawl.max_sitemaps` (10) files per site.
- Where the site's policy has its `robots.txt` asked for (`report` or
  `enforce`, above), the crawl reads that file first, by the same scout,
  and takes the sitemaps it names in place of the guess `/sitemap.xml`.
  The file is listed with the sitemaps (`state: "read"`, or `missing`).
- A scout the site turned away is not waited for: the files it could not
  ask for are `no_scout`, and the crawl reads on.

**A crawl keeps to an enforced `robots.txt`.** Where a site's policy says
`robots: enforce`, an address the file disallows never becomes a page of
the crawl: it has no item, costs no page, and is counted under
`crawl.skipped.robots_denied`. Every page's result carries the verdict.
The first page of an origin waits for the file like any fetch; a file
that cannot be had pauses the job (`robots_unavailable`) until it is
asked for again. A crawl's `defaults` take `include: ["extract"]`,
`cache_ttl_s` (its pages then fill the cache for later fetches) and
`robots` like a fetch.

**Limits an operator sets** (`crawl` in `kage.yaml`): `default_max_depth`,
`default_max_pages`, `max_seeds`, `max_frontier` (addresses a crawl keeps,
50 000; what it finds beyond is counted as `dropped`), `back_limit`,
`near_duplicate_bits`, `max_sitemaps`, `max_sitemap_urls` and
`skipped_extensions` (endings that are files, not pages).

### Webhooks

A job may name a receiver for its events:

```json
"webhook": { "url": "https://hooks.example.org/kage", "secret": "<16 to 256 bytes>", "progress_every": 10 }
```

**No job may have a webhook until you list its receiver.** Under
`egress.mode: proxies` a webhook is the one request Kage makes from this
host's own address (under `direct` every request is), and it goes only
where the operator said:

```yaml
webhooks:
  allowed_origins: ["https://hooks.example.org", "http://app:3000"]
  # Those of them on your own network: the compose network, the LAN, this host.
  private_origins: ["http://app:3000"]
```

An origin is `scheme://host[:port]` and nothing else. A receiver that is
not listed is `422 webhook_origin_not_allowed`. A listed one must be a
public address unless it is also in `private_origins`; a private origin
is your own network by definition, so list there only what you run
yourself. Link-local and metadata addresses are refused whatever is
listed, and `kage config check` refuses a private origin that is not also
an allowed one.

What arrives: `POST` with a JSON body `{ "id", "type", "created_at",
"job" }`, where `job` is what `GET /v1/jobs/{id}` shows. The body names no
page and no persona: read the items with your key. Verify every delivery:

```js
const expected = 'sha256=' + crypto.createHmac('sha256', secret)
  .update(req.headers['x-kage-timestamp'] + '.' + rawBody).digest('hex');
const fresh = Math.abs(Date.now() / 1000 - Number(req.headers['x-kage-timestamp'])) <= 300;
const ok = fresh && crypto.timingSafeEqual(Buffer.from(expected), Buffer.from(req.headers['x-kage-signature']));
```

(Compare lengths first; `timingSafeEqual` throws on unequal ones.) Use the
body exactly as it arrived, before any JSON parser touched it.
`X-Kage-Delivery` is the event's id and is the same on every attempt:
drop a repeat. Answer `2xx` within 10 s; anything else is tried again
after 30 s, 2 min, 8 min, 32 min and 2 h 8 min (each moved by up to a
fifth), then given up. `Retry-After` is honoured up to an hour; `410`
ends the retries at once. A redirect is a failure and is never followed.
While a receiver fails, its other events wait behind the next probe: a
receiver that is down gets one request per step, not one per job. The log
of every attempt is on the job (`webhook.deliveries`), and
`kage_webhook_deliveries_total{event,result}` counts them.

### Retention and the purge

A finished job goes `jobs.retention_days` (14) after it ended, with its
items, their results and their artifacts; a result that is no job's goes
with `artifacts.retention_days`. To delete what a key fetched before
that, for example on a deletion request:

```sh
curl -X POST -H "Authorization: Bearer $ADMIN" -H 'Content-Type: application/json' \
  -d '{"key_id":"<the key's id>"}' http://127.0.0.1:8080/v1/admin/purge
```

`before` (a time) limits it to what is older; without it everything to
this moment goes. Work that is still under way is left: a request that
has not ended, and the request an open item of a job waits on (cancel the
job first to purge all of it). The answer counts the requests, jobs and
artifacts that went, and the call is on the audit log as
`admin.data.purge`. The files of deleted artifacts go with the next sweep.

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
"proj-a-42"` (the key needs `sessions` and `fetch`). A session is a
persona's, so a fetch at level 0 cannot name one
(`422 session_needs_persona`).

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
| `POST /v1/admin/sites/{site}/scout` | Give a site another scout (above, under `robots.txt`); `409 no_matching_persona` when nobody else may ask. |

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
stop, while requests in flight finish (`server.shutdown_grace`, 30 s) and
the pages that are being read are waited for (`jobs.wind_down`, 150 s;
`docs/runbooks/fetching.md`, "Stopping the server").

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

For jobs: `kage_jobs{job_type,state}` (a growing `paused` is sites that
are closed or personas that sleep; a growing `queued` is more work than
`jobs.max_running` takes), `kage_job_items{state}` for what waits and what
is being worked, `kage_jobs_accepted_total{job_type,level}` for what was
accepted, `kage_job_items_total{job_type,state,level}` and
`kage_jobs_finished_total{job_type,state}` for what ended and how, and
`kage_webhook_deliveries_pending` with
`kage_webhook_deliveries_total{event,result}`: deliveries that stay
pending, or a rise of `failed`, is a receiver to look at.

For crawls (a crawl is a job with `job_type="crawl"`):
`kage_crawl_pages_total{reached}` says how pages were reached (a share of
`typed` that grows is sites whose pages do not link to each other, or
browsers that are closed between pages), `kage_crawl_skipped_total{reason}`
what was known and not read (a rise of `hidden` is a site that sets
traps), and `kage_crawl_duplicates_total{kind}` what repeated.

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
| `level_required` | The request, an item of a batch with no `defaults.level`, a crawl's `defaults` or a flow names no `level`. Nothing is decided for the caller. |
| `level_below_policy` | The level is below the site's `min_level`; a request is never raised. |
| `level_unavailable` | Level 1 while no browser major of the installation has a verified http profile (every default deployment). |
| `level_needs_http`, `level_kind_conflict`, `level_include_conflict` | What was asked does not fit the level: a method, a body or headers at level 2 or 3; a `fetch` or `xhr` at another level than 1; an `include` part that needs a browser at level 0 or 1. They were `render_needs_http`, `render_kind_conflict` and `render_include_conflict` in 1.x. |
| `session_needs_persona` | A session named at level 0, which has no persona. |
| `request_kind_needs_referer`, `preflight_required` | A script's request without its page, or one a browser would preflight. |
| `timeout_too_short`, `invalid_pacing` | `minimum_ms` says the least; a pacing profile that does not exist or leaves D9's bounds. |
| `robots_enforced`, `robots_ignored` | The request asks for less care about `robots.txt` than the site's policy enforces, or to have a file enforced that the policy never asks for. |
| `cache_ttl_too_long` | `cache_ttl_s` is more than the site's results may be served for; `maximum_s` says the most. |
| `job_not_found` | `404`: no such job among those this key made. |
| `idempotency_key_reused` | `409`: the `Idempotency-Key` made another job within the last day; `job_id` names it. |
| `webhook_origin_not_allowed` | `422`: the webhook's receiver is not in `webhooks.allowed_origins`. |
| `driver_input_denied` | `422`: a flow step with `human: false` on a site whose policy does not have `driver_input: true`. |
| `session_not_found`, `session_persona_conflict`, `no_matching_persona` | Sessions (above). |
| `unavailable` | `503`: no proxy pool is configured, or the server is stopping. |
| `internal` | `500`. The cause is in the log under `request_id`, never in the answer. |
