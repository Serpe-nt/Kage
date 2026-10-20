# Levels and egress

Since 2.0 every request says how much of a person Kage should be for it,
by a **camouflage level** from 0 to 3, and the operator says two things:
the lowest level each site may be read at (`min_level`), and which
connection carries the traffic (`egress.mode`). Spec §25 is the contract,
ADR 0012 the decision; this page is the practice.

**Below 3 Kage is not pretending to be a person.** Level 0 is a
well-behaved HTTP client that says it is Kage; levels 1 and 2 are a
persona with less of a person's behaviour. A request's level is the
caller's responsibility, and a site's floor is the operator's.

## The levels

| Level | Name | What runs | Persona | Pacing | What a site sees |
|---|---|---|---|---|---|
| 0 | plain | wreq with its own TLS and HTTP/2 settings and a minimal header set (`User-Agent`, `Accept`, `Accept-Encoding`); `User-Agent: kage/<version>` unless the caller or `egress.plain_user_agent` names another; the caller's headers and body as given; no cookie jar; redirects followed as a plain client follows them, each hop through the SSRF guard | none (`persona: null`) | none of a persona's: only the site's own limits (concurrency, requests per minute, the breaker's pause, the pause after server errors), the key's limits and its daily budget | a well-behaved HTTP client that says who it is |
| 1 | http-emulated | the persona's http client (spec §9): its verified browser profile, header tables, cookie jar and request kinds | yes | the persona's | a browser's wire fingerprint with no page behaviour |
| 2 | browser-fast | the persona's real browser, a still visit: the page is typed and loaded, nothing is scrolled, the pointer rests, no patience is drawn, and the persona dwells for the site policy's `min_dwell` drawn upward; captures, screenshots and extraction as at 3 | yes | the persona's pacing, visits and nights: they are the address's rhythm, not the page's | a real browser that loads a page and leaves |
| 3 | human | everything of 1.x: the behaviour engine, the patience, the reader's order, the floors of D40 and D44 | yes | the persona's | a person |

What a page costs:

| Level | Time a page | Bandwidth a page |
|---|---|---|
| 0 | one request: the response and its redirects, plus whatever the site's own limits make it wait | one request's bytes |
| 1 | one request, at the persona's pace | one request's bytes |
| 2 | a page load and the dwell: `min_dwell` drawn upward, one and a half times it at the median and under three times it 95 times in 100 (3 s and 6 s with the default `min_dwell` of 2 s), plus the persona's pacing between pages | a whole page load in a browser: its scripts, styles and images |
| 3 | the behaviour engine: about 35 s a page at the median, about 100 pages an hour per persona ([api.md](api.md#what-a-fetch-costs-and-what-a-request-may-shorten) has the table), plus the persona's pacing | a page load and what scrolling loads after it |

A browser page costs a few hundred kilobytes to a few megabytes of the
proxy plan; a session of a few hundred browser pages is a few hundred
megabytes to a gigabyte ([field-campaign.md](field-campaign.md)).

What a level takes of a request (spec §16.4). "refused" is
`validation_failed` naming the field; the other cells are the code the
API answers with:

| Field | 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| `method`, `body`, `headers` | yes (level 0's list, below) | yes | `level_needs_http` | `level_needs_http` |
| `request_kind` `fetch`, `xhr` | `level_kind_conflict` | yes | `level_kind_conflict` | `level_kind_conflict` |
| `session_id` | `session_needs_persona` | yes | yes | yes |
| `persona` | refused | yes | yes | yes |
| `behavior` | refused | refused | refused | yes |
| `wait` | refused | refused | yes | yes |
| `pacing` | refused | yes | yes | yes |
| `include` `dom`, `xhr`, `screenshot`, `pdf`, `console` | `level_include_conflict` | `level_include_conflict` | yes | yes |
| `robots`, `cache_ttl_s`, `extract` | yes | yes | yes | yes |
| `timeout_ms` at least | 1 000 | 1 000 | 15 000 | 15 000 |

Without `include`, levels 0 and 1 return `raw` and `headers`, levels 2
and 3 `dom` and `headers`. A batch's or a crawl's `visitors` is refused
at level 0, which has no persona to count; a flow is level 2 or 3 (a flow
is a browser), and a flow at level 2 takes no `behavior`. At level 0 a
crawl types every page and reads its links from the markup.

**Level 1 is not available in a default deployment.** It is refused with
`level_unavailable` while no browser major the installation runs has a
verified http profile, and the shipped majors (Chrome 155, Camoufox 156)
have none (follow-up F30). In a default deployment only 0, 2 and 3 work.
`kage config check` says which: `egress.level_one` is `false` until a
configured major resolves to a profile.

Which level to ask for:

- **0** for a page that can be read with `curl`: a feed, a public API, a
  document the site serves to everybody.
- **2** for a site that needs a browser (its pages are built by scripts)
  but does not watch how they are read.
- **3** for a site that watches. A site that turned level 2 away is read
  at 3 or not at all.
- **1** waits for a verified major.

What a level never changes: persona handles, sessions, jobs, crawls,
flows, webhooks, extraction, `robots.txt`, the cache (its key includes the
level) and the shape of a result. The SSRF guard, the key's scopes and
rate limits and the daily page budget apply at every level. The result
says the level it was made at (`level`; null for a result of 1.x).

**Nothing escalates.** The level is the request's ceiling as much as
`min_level` is its floor: a block at level 0 or 1 is reported, never
retried in a browser. At level 0 a block (403, 429, a captcha, a
challenge, a soft block) is the answer at once and is never tried from
another address; a network failure, a timeout or a 5xx is tried again
from the same exit after a pause. A level-0 block cools no persona and
does not count on the site's breaker.

## A site's floor: `min_level`

```yaml
# policies/domains.yaml
defaults:
  min_level: 0
domains:
  "shop.example.org":
    min_level: 3
```

`min_level` (0 to 3, default 0) is the lowest level a request for the
domain may name. A request below it is refused with
`422 level_below_policy` (`kage fetch` says the same), and **a request is
never raised**: a caller who asked for level 0 did not ask for a browser,
and is told so instead of being given one. It cascades like every policy
field: an entry for `example.org` covers `shop.example.org` unless that
entry sets its own. Kage's own traffic (scouts, warm-up, self-tests) has no
request level and is not judged by it. `GET /v1/policies/domains/{domain}`
shows the floor in force for a host.

The shipped policy file sets `min_level: 3` for the detection pages
(`bot.sannysoft.com`, `abrahamjuliot.github.io`, `browserscan.net`,
`pixelscan.net`, `browserleaks.com`, `fingerprint.com`,
`demo.fingerprint.com`): only a person is shown to them, never a client
that is not pretending to be one. Raise a site's floor:

- for a site that watches, so that no caller reads it as less than a
  person by mistake;
- for every site the host's persona reads under `egress.mode: direct`
  (below).

## Egress: where requests leave from

`egress.mode` is the operator's choice, for the whole installation:

| | `proxies` (the default) | `direct` |
|---|---|---|
| Personas | one per proxy of the pool, as in 1.x | 1 to 4 host personas (`egress.direct.personas`) |
| Level 0 (`egress.plain`) | `pool`: through the site's scout's proxy | `host`: the host's own connection |
| Leaves from the host's own address | nothing but the webhooks and the host's own lookups ([proxies.md](proxies.md#what-the-health-checks-contact)) | everything |
| Leaves through the pool | everything | nothing: the pool is idle |
| Leak check, provider budget | yes | no |

`egress.plain` names level 0's exit and is fixed by the mode: unset, it is
the one the mode implies. **There is no mixed mode.** `proxies` with
`egress.plain: host` and `direct` with `egress.plain: pool` are refused at
load, at start and by `kage config check`: a site that sees a visitor move
from a residential address to the host's has seen a bot.

### Under the pool (`egress.mode: proxies`)

Everything is 1.x's ([proxies.md](proxies.md), [personas.md](personas.md)),
and level 0 leaves through one address per site, its **plain exit**. The
plain client reaches it through the forwarder under a credential of its
own (`plain-<proxy id>`), never a persona's. The reason is one sentence:
**an address that sends `kage/<version>` must never be one that reads it as
a person.** A persona that read a site from an address that also said
"kage" to it is a bot that said so. A site's plain exit is:

1. **one of the pool's own plain exits**, when it has any: an entry of a
   declared list without a country, which carries no persona and so reads
   no site as a person (ADR 0013). A site that has none yet gets one of
   the usable ones, by a draw seeded by the site, and keeps it while it
   lives (`site_exits`). An operator who wants level 0 at scale dedicates a
   few such entries to it ([proxies.md](proxies.md#plain-exits));
2. else the proxy of the site's **scout**, the persona that asks for its
   `robots.txt` and never reads the site (chosen by the scout rule when the
   site has none, and its scout from then on). The scout's address is
   already the one marked for the site by its `robots.txt`, so one address
   per site is marked, not two.

`robots.txt` and sitemaps of a level-0 request are asked for by the plain
client from the same exit. A proxy that leaves by an older proxy's address
(`proxy_exit_shared`) carries no level 0.

When that exit cannot carry traffic, level 0 waits or is refused and is
never sent from another address: a fetch is answered `provider_error` with
the reason `plain_exit_unavailable` and a `retry_at`, and a job's page
waits (`paused`). It happens when the site's plain exit, or its scout's
proxy, is in backoff or being replaced, and, without plain exits of the
pool's own, when nobody may scout the site: a pool of one persona, or
every persona has read the site within `robots.scout_quarantine` (30
days). What to do:

- Wait for the proxy ([proxies.md](proxies.md#states)).
- Give the site another scout, `POST /v1/admin/sites/{site}/scout`
  ([api.md](api.md#robotstxt)). The new scout's proxy is the site's plain
  exit from then on: one more address has then said "kage" to the site.
  Do it for a proxy that will not be back soon.
- Ask for the page at level 2 or 3, which a persona reads from its own
  address.

The plain client's bytes are counted with `egress: pool` and against the
proxy plan's budget, and the pool's stops (a leak on record, the budget
used up) hold it as they hold every persona.

### The host's own connection (`egress.mode: direct`)

For an operator who runs Kage on a machine whose own connection may be
seen by the sites it reads. Under `direct`:

- **The host's rows.** One row per host persona stands in the pool:
  provider `host`, `host-1` to `host-4` in `kage proxy list`, address
  `host.invalid`, `healthy` from the start. Nothing is relayed to that
  address: the forwarder relays the persona's traffic with no upstream.
  A host row
  is never synced, probed, leak-checked, counted in the outage share or
  the capacity, or replaced (`kage proxy replace` answers that the
  provider cannot replace it).
- **The host personas** (`egress.direct.personas`, 1 by default, at most
  4: a few households behind one address, never a pool) are generated,
  warmed up, paced and self-tested like any persona
  ([personas.md](personas.md)).
- **Their place.** At start and once per `proxies.health.geo_interval`
  (1 d) Kage reads the host's own address (`proxies.health.host_ip_url`)
  and looks it up (`proxies.health.geo_url`), both directly from the
  host. Declared values win: `egress.direct.country`, and
  `egress.direct.locale` (a BCP 47 tag with that country as its region,
  `de-AT`, which needs `country`); a lookup then only verifies them, and
  two that disagree in a row are `geo_mismatch`. Behind a carrier NAT, a
  VPN or a lookup that places the host wrongly, declare them. With
  neither a declared country nor a lookup that answers there is no host
  persona, and only level 0 works (`kage config check` warns).
- **Their engines**: `egress.direct.engines`, a subset of
  `personas.mix`; unset, they are drawn from the mix.
- **The pool is idle**: not synced, not probed, not used. Its personas
  wait unused and are scheduled again when the mode goes back to
  `proxies`. A provider that is still configured raises a warning in
  `kage config check` and in the log at start.
- **The forwarder still fronts every browser and client**, with no
  upstream: it resolves every target itself, refuses one that has any
  address in a private, loopback, CGNAT or ULA range (unless its name is
  in `private_names`, below), connects only to an address it checked, and
  counts every byte. Link-local, metadata and multicast addresses are
  refused for every name.
- **`egress.direct.private_names`** lists exact host names (no scheme, no
  port, no path; at most 64) that may resolve to a private or loopback
  address: the operator's own network. `kage config check` warns while
  the list is not empty. List only what you run yourself.
- **No leak check and no provider budget.** There is no proxy to leak
  through, and the pool's stops do not hold the host's connection. Bytes
  are still counted, with `egress: host` (`bandwidth_usage.egress`,
  `kage_bandwidth_bytes_total{egress="host"}`), and do not count against
  `bandwidth.monthly_cap_gb`.
- **Level 0 leaves from the host** (`egress.plain: host`), and asks for a
  site's `robots.txt` and sitemaps from there.

**One address for everything.** Under `direct` a site read at level 0
and at level 2 or 3 sees one address send `kage/<version>` and a
browser. The guard is the site's floor: set `min_level: 2` (or 3) for
every site the host persona reads, so that nobody reads it at level 0
from the same address.

**A lone host persona is its own scout.** With `egress.direct.personas:
1` nobody else can ask a site for its `robots.txt`, so the persona that
reads the site asks for the file itself: one address both reads a site
and asks for its `robots.txt`, which is the mark the scout rule exists to
avoid. With more host personas another one asks, but every one of them
has the host's address: the scout keeps a persona apart, not an address.
Under `direct`, keep `robots: ignore` (the default) for the sites the host
reads unless their verdict is worth that mark.

**Changing the mode.** From `direct` to `proxies`, the host's rows are
retired at the next start (`egress_mode`) and their personas with them:
their profiles are archived and deleted after `personas.retention_days`.
Going back to `direct` makes new host personas that know nothing of the
old ones. The pool's personas outlive a stretch under `direct` unused.

To switch, set the keys in `kage.yaml`, or in a compose deployment as
environment of the `kage` service (`KAGE__EGRESS__MODE: direct`,
`KAGE__EGRESS__DIRECT__COUNTRY: AT`) in an override file of your own,
and recreate `kage`:

```yaml
egress:
  mode: direct
  direct:
    personas: 1
    country: AT
    locale: de-AT
    engines: [firefox]
    private_names: []
```

## The plain client's name: `egress.plain_user_agent`

A level-0 request that names no `User-Agent` sends `kage/<version>`
(`kage/2.0.0`). An operator may give it another name, 1 to 256 printable
ASCII characters; the useful form names the client and a page that says
who runs it and how to reach them:

```yaml
egress:
  plain_user_agent: "acme-bot/1 (+https://acme.example/bot)"
```

A request may set its own `User-Agent` at level 0: the caller's headers
are its request as given. A browser's `User-Agent` there does not make the
client a browser (its TLS, HTTP/2 settings and header set stay the plain
client's); it is the caller's request, not camouflage.

## A caller's `Cookie` and `Authorization` at level 0

At level 0 the caller's headers are sent as given, `User-Agent`, `Cookie`
and `Authorization` included; only the transport's own headers,
`Proxy-*`, the forwarding family (`X-Forwarded-*`, `Forwarded`,
`X-Real-IP`, …) and `Accept-Encoding` are refused (`header_denied`).

- A caller's `Cookie` and `Authorization` go to the request's origin only
  and are dropped on a redirect to another origin.
- They are never logged, and the stored request keeps their names with
  `[redacted]` values.
- A job refuses them (`header_denied`): a job keeps its items until they
  run. A fetch that is carried on as a job after a restart is not made
  again without them; it ends `interrupted`.
- The plain client keeps no cookie jar: cookies a redirect chain sets
  live for that chain only.

Kage holds no credential for any site and never logs in. A key with the
`fetch` scope can make Kage send the caller's own token to any site the
SSRF guard allows, from the scout's address or the host's, at the key's
rate: give that scope to consumers you would let use that address.

## Checking a deployment

`kage config check` validates the configuration and the policies (a
policy file that still has `render`, or a crossing of the modes, is
refused there) and reports where requests leave from:

```json
"egress": {
  "mode": "direct",
  "plain": "host",
  "plain_user_agent": "kage/2.0.0",
  "host_personas": 1,
  "level_one": false,
  "warnings": [
    "egress.mode is direct and a proxy provider is configured: the pool is idle, neither synced, probed nor used"
  ]
}
```

| Warning | Meaning |
|---|---|
| `egress.mode is direct and a proxy provider is configured` | The pool is idle. Remove the provider's key, or go back to `proxies`. |
| `egress.mode is direct with neither egress.direct.country nor proxies.health.geo_url` | No host persona can be made: only level 0 works. Declare the country. |
| `egress.direct.private_names lets … name(s) resolve to a private or loopback address` | The names that may reach your own network from the host's connection. |

Then one request at each level you mean to use, against a site you may
fetch:

```bash
docker compose --file deploy/compose.yaml exec kage \
    kage fetch https://example.org/ --level 0
```

```text
ok  200  https://example.org/
request 0192…  result 0192…
attempts:
  1  http     plain     exit pool  ok  200  412 ms
…
```

A level-0 attempt names its exit (`exit pool` or `exit host`, never an
address) where another names its persona. `--persona`, `--engine` and
`--pacing` are refused at level 0; a level below the site's floor is
refused with `level_below_policy`, level 1 with `level_unavailable` while
it is not available. Under `direct`, `kage proxy list` shows the host's
rows and `kage persona list` their personas.

## Upgrading from 1.x

2.0.0 breaks every 1.x request on purpose: nothing is decided for the
caller.

1. **Drain the job queues first.** An item of 1.x still queued names no
   level and fails at take-up with `level_required`. Stop sending jobs and
   wait until none is `queued`, `running` or `paused`
   (`kage_jobs{state=~"queued|running|paused"}`, or `SELECT state,
   count(*) FROM jobs GROUP BY state;`), or cancel what may go
   (`POST /v1/jobs/{id}/cancel`).
2. **Remove `render` from `policies/domains.yaml`.** The shipped 1.x file
   had `render: auto` under `defaults`; a file that still has `render`,
   anywhere, is refused with a message that names `min_level`. Delete the
   key; where a site had `render: browser` because it watches, give it
   `min_level: 3`. The 2.0 file sets `min_level: 3` for the detection
   pages.
3. **Remove `escalate` from your own block signatures.** A rule with it
   is refused: nothing escalates.
4. **Make every request name `level`**: a fetch, every item of a batch
   (its own or `defaults.level`), a crawl's `defaults.level`, a flow's
   `level` (2 or 3), and `kage fetch --level` (`--render` is refused).
   1.x's `render: browser` was level 3; `render: http` was level 1, which
   is not available, so such a request becomes 0 or 2; `render: auto` has
   no equivalent: choose.
5. **Rename the error codes your clients match on:**

   | 1.x | 2.0 |
   |---|---|
   | `render_needs_http` | `level_needs_http` |
   | `render_kind_conflict` | `level_kind_conflict` |
   | `render_include_conflict` | `level_include_conflict` |
   | | `level_required`, `level_below_policy`, `level_unavailable`, `session_needs_persona` (new) |

   A result has `level` and no `escalated`; an attempt at level 0 has
   `persona` and `engine` null and says its `exit`. The policy preview says
   `min_level` in place of `first_render`.
6. **Dashboards and alerts.** `kage_requests_total`, `kage_attempts_total`
   and `kage_attempt_duration_seconds` carry `level` (`"0"` to `"3"`) in
   place of `render`, and `kage_jobs_accepted_total` and
   `kage_job_items_total` gain it. The shipped dashboards and alert rules
   follow (KageBlockRateAboveThreshold leaves out the blocks of level 0,
   which the breaker never counts); a query or rule of your own that says
   `render="browser"` reads `level=~"2|3"`, and `render="http"`
   `level=~"0|1"`. `kage_scheduler_in_flight` keeps its `render`.
7. **The response cache starts empty in effect.** Its key is at version 2
   (the level replaces the render): no entry of 1.x is served.
8. Apply the migrations (the compose stack's `migrate` service does it;
   0017 adds `requests.level`, `attempts.exit` and
   `bandwidth_usage.egress`), start, and run `kage config check`. Without
   an `egress` section a 1.x configuration is `egress.mode: proxies`, as
   before.
