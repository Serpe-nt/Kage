# Fetching: the scheduler, the forwarder and the bandwidth budget

What happens between a fetch order and the wire since M7 (spec §3.2, §7.5,
§8, §10.6, §11, §12), and what an operator can see and change.

## One request by hand

```bash
docker compose --file deploy/compose.yaml exec kage \
    kage fetch https://example.org/page
```

`kage fetch <url>` runs one request through the whole pipeline in its own
process: the domain policy, the scheduler, a persona, its path, the
classifier, and the store. It prints the outcome, the attempts (persona,
path, outcome, status, time), the pacing that applied, cookies handed over
and the bytes relayed; `--json` prints one document, with the behavior the
browser reports having performed. It exits non-zero unless the page was
fetched. The request, its attempts and its result are stored like any
other, and the command is in the audit log.

| Option | What |
|---|---|
| `--render auto\|http\|browser` | The path. `auto`: the policy decides, and a block on the http path moves the request to the browser once. |
| `--persona <id>` | Only this persona. A pinned persona that is blocked is not replaced. |
| `--engine chromium\|firefox` | Only personas on this engine. |
| `--timeout 45s` | Longest one attempt may take (default 60 s in the browser, 30 s over http, at most 300 s). A browser attempt includes the time the persona spends on the page. |
| `--retries N` | Retries after the first attempt, instead of the policy's. |
| `--pacing <profile>` | A pacing profile instead of the policy's, unless the policy locks pacing. |
| `--output FILE` | Write the document there. |
| `--extract` | Also read what the page says from the document that was fetched (`docs/runbooks/api.md`, "What a page says"): the title, the words of the main content and the links in a sentence, everything under `extract` with `--json`. Nothing is extracted from a page that was not fetched or from a response that is no HTML document. |
| `--cache-ttl 10m` | For how long the result may be answered from the response cache; `0s` is never; left out, the site's policy decides (`cache.ttl`). Longer than results of the site are served for is refused, not cut. |
| `--robots policy\|report\|enforce\|ignore` | What to do about the site's `robots.txt`. More care than the policy takes may be asked for, never less: `enforce` where the policy never asks for the file, and `report` or `ignore` where it enforces it, are refused before anything starts. |

What the three return is shown like everything else. `robots.txt:` says
what the site's file says of the address (`allowed` or `disallowed`, the
rule that decided, a `Crawl-delay`), or `not asked for` when the command
named `--robots` and the policy leaves the file alone. `cache:` says
whether the answer came from the cache and when its page was fetched, or
that the page was fetched now and is kept. `extract:` is the sentence
above. With `--json` they are `robots`, `cached`, `cached_at`,
`cache_ttl_s` and `extract`.

The cache of the command line is its own: no API key is answered from an
entry the command wrote, and the command is answered from no key's. A
fetch that may fill it keeps its document in the artifact store (the DOM
of a browser attempt, the body of an http one), because an answer from
the cache can only return what was stored: `--output` then writes the
stored document, and the answer has no attempt and relayed nothing.
`--extract`, `--render`, `--persona` and `--engine` are part of an entry's
key; the timeout, the retries, the pacing and `--robots` are not.

The command shares the store with the server, so cooldowns, daily caps,
breakers, the leak flag and the bandwidth budget hold for it. It does not
share the server's memory: its pace is its own (one request has none), and
a persona whose browser the server holds open at that moment cannot be
opened by the command (`context_conflict`); another persona then takes the
request. Never point it at a site you would not send the server to: it is
a real visit by a real persona.

In this release no persona has an http path (Chrome 155 and Firefox 152
have no verified profile), so `--render http` is answered with
`provider_error` and the reason `no_persona`, and `auto` goes to the
browser.

## What a request is answered with when nothing was attempted

The outcome is `provider_error` and the reason says why:

| Reason | Meaning | What to do |
|---|---|---|
| `egress_unsafe` | A proxy showed the host's own address. Nothing is fetched and the forwarder relays nothing. | `docs/runbooks/proxies.md`, "A leak". |
| `bandwidth_exhausted` | The bandwidth budget of the term is used up and `bandwidth.hard_stop` is on. | Below. |
| `domain_paused` | The domain's circuit breaker is open; the answer names when to ask again. | Below. |
| `no_persona` | No persona can serve the request: none is ready, none matches the engines and countries of the policy and the request, or none has an http path for `render: http`. | `kage persona list`. |
| `shutting_down` | The server is stopping: the request waited for a persona when the signal came, and nothing was asked of the site. | Ask again when it is back. |

An attempt can end with a reason too, when it was not the site that
ended it: `proxy_failed`, `timeout`, and `interrupted`.

## Stopping the server

`docker compose stop kage` (or a `SIGTERM`) does three things at once:
`/readyz` answers `503`, the scheduler grants nothing more (a request that
waited for a persona is answered `provider_error` with `shutting_down`,
an item of a job is given back), and the server waits for the pages that
are being read, for at most `jobs.wind_down` (150 s). A page under the
shipped behavior takes one to two minutes, so a stop takes that long
while one is open and no time when none is.

- A page that ends inside the wait is stored like any other: one attempt,
  the site asked once. Its job goes on after the restart with the next
  page.
- A page that is still being read when the wait is over is cut. Its
  attempt is on the record with the outcome `provider_error` and the
  reason `interrupted`; it cools no persona and counts on no breaker. The
  request is taken up again after the restart (a job's, a carried
  fetch's) and counts that attempt against its retries, so the site sees
  that page twice. A fetch whose caller still waited ends with it.
- `jobs.wind_down: 0s` is a stop that waits for nothing: every page under
  way is cut and written down.
- Docker kills a container that has not stopped after
  `stop_grace_period`; `deploy/compose.yaml` gives `kage` 200 s, which is
  `jobs.wind_down` and 50 s. Raise both together. A killed process writes
  nothing: its pages are fetched again when `jobs.lease` (1 min) has run
  out, and nothing says they had been asked for.

The log says what happened: `the server stops: the pages that are being
read are finished first` with their number, and a warning with the number
that was cut.

A fetch can also end without an attempt as `robots_denied`: the site's
policy enforces its `robots.txt` (`docs/runbooks/api.md`, "robots.txt").

| Reason | Meaning | What to do |
|---|---|---|
| none | A rule of the file disallows the address; `robots.matched_rule` names it. | Nothing: the site said no. |
| `robots_unavailable` | The file could not be had (a server error, a timeout, a block of the scout), so nothing of the origin is fetched; `retry_at` says when it is asked for again (`robots.error_ttl`, 1 h). A job's pages of the site wait for it. | Wait, or look at the scout's attempts in the log (`a robots.txt was asked for`). |
| `robots_no_scout` | Nobody can ask for the file: every persona that could has read the site, or there is only one. | Add a persona, or set `robots: report` for the site. |
| `robots_enforced`, `robots_ignored` | The request's `robots` and the policy cannot both be had; only seen in a job whose site's policy changed after it was accepted (the API refuses such a request at once). | Send the job again. |

## Pace

A persona that fetched a page from a domain keeps that domain's work for
its visit: bursts of pages with the gaps of the pacing profile at the
persona's own factor, breaks between bursts, and a long break after a few
bursts (`visit_bursts`, `visit_break_ms` in `policies/pacing-profiles.yaml`).
A new visitor is chosen only while fewer than `domain_concurrency` personas
are visiting. The gap counts from the start of the previous page and is a
floor: a browser page that the persona dwelt on for longer than the gap is
followed at once. A synchronous request does not wait for a visitor's
break. Two personas never start within the same moment: a start that a
persona's own pace did not time is kept apart from the previous such start
(`scheduler.visit_start_spacing`).

Each persona has a day of its own from its seed (up between 06:30 and
08:30, to bed between 22:30 and 00:30 in its time zone, shifted a little
every day: `scheduler.active_hours`). What that means is per domain
(`active_hours` in `policies/domains.yaml`): `prefer` (default: a job never
wakes a sleeper, a synchronous request may), `enforce`, or `ignore`.

## After a block

- The persona stays away from the domain for the policy's cooldown for the
  outcome, scaled by the confidence of the signature that matched, never
  under `scheduler.cooldown_floor` (5 min), moved by up to
  `scheduler.cooldown_jitter` (a tenth) so that personas blocked together
  do not come back together. `kage persona show <id>` lists it.
- Its browser is closed: nothing of the wall stays open.
- Another persona tries after a pause (`scheduler.retry_pause`), from
  another /24 and preferably another network, up to the policy's retries.
- A block on the http path under `render: auto` moves the request to the
  browser once: the same persona's when a browser cures it (a challenge, a
  page that needs scripts), another persona's after a refusal (403, a
  captcha, a rate limit).
- The domain's breaker counts it. After `breaker_threshold` blocks within
  an hour the domain is paused for `breaker_pause`, and `domain_paused` is
  raised. After the pause one request is the trial: served, the domain
  flows again; blocked, the pause doubles (up to
  `scheduler.breaker_max_pause`, 6 h). The state is in `domain_breakers`:

  ```sql
  SELECT domain, state, block_count_1h, trips, paused_until FROM domain_breakers;
  ```

  To end a pause by hand, after reading why the site blocked, close the
  breaker: `UPDATE domain_breakers SET state = 'closed', paused_until =
  NULL, trips = 0, block_count_1h = 0, trial_started_at = NULL WHERE domain
  = '...';`. The personas that were blocked keep their cooldowns.

Server errors and timeouts cool no persona and do not count on the breaker.
Three in a row from one domain, across personas, leave it alone for
30 s, twice as long with every further one, up to 10 min
(`scheduler.server_errors`); any other answer ends the row.

## The forwarder

Every persona's browser and http client is given kage's own forwarder as
its proxy (`forwarder.bind`, `forwarder.advertise`), under a credential
that names only the persona. The provider's credentials stay in the kage
process. In compose the sidecars reach it as `http://kage:8899`; it is
never published on the host, and must not be.

With `just dev` kage runs on the host and the sidecars in containers:
`scripts/dev-env.sh` advertises the forwarder as
`http://host.docker.internal:8899`, a name `deploy/compose.dev.yaml` gives
the sidecars for the host. The forwarder asks every client for a persona's
credential, but it is not meant to be reachable from outside the machine:
on a host with a public interface, firewall the port or set
`KAGE__FORWARDER__BIND` to the address of the Docker bridge.

It refuses, without an answer a page could read: a target the SSRF guard
refuses (loopback, private ranges, the names of the compose network), a
persona that is unknown, cooling or retired, a proxy that is not usable,
and every new connection while a leak is on record or the budget is used
up. In the log (`RUST_LOG=kage_scheduler=debug`) each refusal is one line,
`the forwarder refused a connection`, with its reason.

`kage persona selftest`, `kage selftest browser` and `kage selftest http`
run in their own process and hand the sidecar the provider's proxy
directly, as before M7. A persona whose browser the server holds open
(for up to `scheduler.context_idle`, 5 min, after its last page) cannot be
self-tested from the command line at that moment: the sidecar answers
`context_conflict`. Try again a few minutes later.

## The bandwidth budget

The forwarder counts every byte per persona and per domain being fetched;
the counts are in `bandwidth_usage`, by the hour:

```sql
-- This month, by domain, in MB.
SELECT domain, round(sum(bytes_up + bytes_down) / 1e6, 1) AS mb
FROM bandwidth_usage WHERE hour >= date_trunc('month', now())
GROUP BY domain ORDER BY mb DESC LIMIT 20;
```

`(idle)` is what a browser sent while its persona was fetching nothing:
its own background traffic. The budget (`bandwidth.monthly_cap_gb`, 250)
runs over the provider's subscription term, or the calendar month before
the account was read, and is judged on the larger of Kage's count and the
provider's own: the provider also bills health probes and command-line
self-tests, which do not pass the forwarder.

- At 80 % (`bandwidth.warn_at`): `bandwidth_budget_warning`, once per term,
  with the day the cap would be reached at the rate so far.
- At the cap: `bandwidth_budget_exhausted`, once per term. With
  `bandwidth.hard_stop: true` (default) nothing is fetched until the term
  ends: requests are answered `bandwidth_exhausted`, the forwarder relays
  nothing new, open browsers are closed, warm-up visits and scheduled
  self-tests wait. Health probes go on. To go on fetching, raise
  `bandwidth.monthly_cap_gb` (after buying the traffic) and restart, or set
  `bandwidth.hard_stop: false` to be alerted only.

## Browsers stay open

A persona's browser context stays open between the pages of its visit and
for `scheduler.context_idle` (5 min) after the last one. It is closed
earlier after a block, when another persona needs the engine's slot
(`scheduler.max_contexts_per_engine`, which must equal the sidecar's
`SIDECAR_MAX_CONTEXTS`; the least recently used context that is not in the
middle of a visit goes first), at a stop, and at shutdown. The sidecar's
own `SIDECAR_CONTEXT_IDLE_MS` (6 min in compose) is only the backstop.

## Processor counts of chromium personas (follow-up F25)

A chromium persona reports the processor count the generator drew for it,
but never more than the logical CPUs its sidecar's host shows
(ADR 0009). The compose `cpus:` limit is a quota and does not change what
the browser reports. So on a host with fewer CPUs than the draws, every
chromium persona whose draw is larger reports the host's count. Compare
`hardware_concurrency` in `kage persona show <id> --json` with `nproc` in
the `browser-chromium` container. The scheduler does not change any of
this; it only bounds how many chromium personas are open at once (three).
Sizing the production host so that the draws survive is an item of the
deployment milestone (M12).
