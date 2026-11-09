# Observability: logs, traces, metrics, dashboards, alerts

What Kage says about itself, where it goes, and how to read it (spec §20).
Logs and metrics are always there; traces leave the process only when a
collector is named; the dashboards and alerts come with the
`observability` profile.

## Logs

Kage and both sidecars write one JSON object per line on stdout
(`docker compose --file deploy/compose.yaml logs`), in one shape (F144): `timestamp` (ISO 8601,
UTC), `level` (`INFO`, `WARN`, ...), the text and everything the line
was written with under `fields` (`fields.message`, `fields.persona`),
and `trace_id` and `span_id` at the top. Kage's lines add `target` (the
module); a sidecar's request lines carry its own id of the request as
`request` (Kage's request id is in Kage's lines; the trace joins the
two). A line written inside a request carries `trace_id` and `span_id`:
the same `trace_id` in Kage's lines and in the sidecar's for one request,
also when no trace is sent anywhere; a sidecar's line names the sidecar's
span, or outside it the span of Kage's request to it. To follow one
request:

```bash
docker compose --file deploy/compose.yaml logs --no-color kage browser-chromium browser-firefox | grep '"trace_id":"<trace id>"'
```

No line holds a secret, a proxy credential, an API token, a cookie's
value or a request body; `tests/e2e/observability.sh` greps every line of
every service for the stack's secrets after real traffic (the redaction
audit). `RUST_LOG` sets Kage's level (`info` unless said, or a filter
such as `kage_scheduler=debug`); `LOG_LEVEL` a sidecar's (`trace`,
`debug`, `info`, `warn`, `error`). The shipped compose file passes both
through from the shell or `deploy/.env` (F144):

```bash
RUST_LOG=kage_scheduler=debug LOG_LEVEL=debug docker compose --file deploy/compose.yaml up --detach
```

A proxy is named by its id in a line at `info` and above, never by its
host or port, and an alert's details in the log have its addresses
replaced by `[redacted]` (F153): a log may be public (the nightly's is on
GitHub) and the addresses are the operator's. The full details stay with
the alert in the store (`health_events`) and are in the log at `debug`. At
`debug`, a redirect target a site sent is named by its origin only, never
its path or query (F144).

## Traces

Every request is one trace:

| Span | Where | What it says |
|---|---|---|
| `api.request` | Kage | the route as its pattern (never the path a client sent), the method, the status, the key's id |
| `pipeline.fetch` | Kage | the request's id, its domain, its `level` and the path it implies (`render`: `http` at 0 and 1, `browser` at 2 and 3), how it ended |
| `scheduler.wait` | Kage | waiting for a persona and its turn, or at level 0 for the site's own limits |
| `attempt` | Kage | its number, its `level` and `render`, the persona's id (at level 0 the `exit` instead: `pool` or `host`, never an address), its outcome and vendor |
| `sidecar.navigate`, `sidecar.flow` | Kage, then the sidecar | the client side in Kage and the server side in the sidecar, joined by the W3C `traceparent` of Kage's request on the compose network |
| `job.item` | Kage | a job's page: a trace of its own, with the job's id |
| `webhook.deliver` | Kage | one delivery attempt: the delivery, its job, its event, the status, the result |

No span holds a URL's path or query, an address, a seed, a token, a
fingerprint or a body. That includes the span events: a log line written
inside a span leaves with the trace as an event of it, so the attempt's
line carries no seed (F182; the seed is `attempts.seed` in the store).
Since F196 that does not rest on each line: before a span leaves, every
event of it is stripped to its level, its target and where it was
written, under the name `log` (a line's own field named `level`,
`target` or `code.*` is dropped too), and a span's error status leaves
without a description: an error's text is never the status description
or an `exception.message`. In Jaeger a span's logs therefore say that
a line was written and how loud it was; to read the line, search Kage's
log for the span's `trace_id` and `span_id`. Traces are sent only when Kage is given
`telemetry.otlp_endpoint` (`KAGE__TELEMETRY__OTLP_ENDPOINT`) and a sidecar
`SIDECAR_OTLP_ENDPOINT`: OTLP over HTTP with protobuf, the address with
its path (`http://otel-collector:4318/v1/traces`), asked directly and
never through a proxy. A failed export is a warning in Kage's log; it
never fails a request.

## The `observability` profile

```bash
docker compose --file deploy/compose.yaml --file deploy/compose.observability.yaml \
  --profile observability up --detach --wait
```

| Service | Image | What it does |
|---|---|---|
| `otel-collector` | OpenTelemetry Collector (core) 0.162.0 | takes OTLP over HTTP from Kage and the sidecars, hands it to Jaeger |
| `jaeger` | Jaeger 2.20.0 | keeps the traces for seven days (Badger, volume `jaeger-data`); the last release with the v1 search that Grafana's Jaeger data source sends (F176, [versions.md](../versions.md)) |
| `prometheus` | Prometheus 3.13.4 (LTS) | scrapes Kage on its metrics listener every 15 s, keeps 15 days, evaluates the alert rules |
| `grafana` | Grafana OSS 13.2.4 | the dashboards, with Prometheus and Jaeger as its data sources |

Every image is pinned by digest, read-only, without capabilities, with
memory, CPU and process limits.
Only Grafana is published, on loopback, port 3000 (`KAGE_GRAFANA_PORT`);
the user is `admin` with the password `KAGE_GRAFANA_ADMIN_PASSWORD` of
`deploy/.env`, which `just setup` creates. Grafana downloads nothing:
its preinstalled plugins, analytics and update checks are off. To read a
trace, open **Explore**, choose **Jaeger** and search the service `kage`
(or `kage-sidecar-chromium`, `kage-sidecar-firefox`), or paste a
`trace_id` from a log line.

The profile sets, for Kage, `KAGE__TELEMETRY__OTLP_ENDPOINT` and
`KAGE__SERVER__METRICS_BIND=0.0.0.0:9464`: a second listener that serves
`/metrics` and `/healthz` without a key, on the compose network only.
Never publish it. Without the profile, `/metrics` is served by the API to
keys with the `admin` scope.

## Dashboards

In the folder **Kage**, written by `scripts/grafana-dashboards.mjs` (edit
the definitions there and run it; `just lint` checks that the files are
current and that every metric they name exists):

| Dashboard | What |
|---|---|
| Kage · pool and personas | proxies and personas by state, self-test scores, replacements, browser contexts, the sidecars' memory and CPU, navigation time |
| Kage · pacing and visits | fetches by outcome, attempts by level (`0` to `3`) and their time, the wait for a persona, the queue, attempts in flight by path, refusals, the API |
| Kage · blocks and breakers | per domain the blocks of the last hour against its breaker threshold, the block rate, breakers open, blocks by vendor (every level; the breaker counts no block of level 0) |
| Kage · jobs and crawls | jobs and items by state, how jobs end, how a crawl reached its pages, what it skipped |
| Kage · webhooks | deliveries by result and event, what waits, failures |
| Kage · bandwidth | the share of the plan's monthly cap used (`bandwidth.monthly_cap_gb`, 250 GB unless set), bytes through the forwarder (the host's own connection under `egress.mode: direct` included, which the cap does not count) |
| Kage · cache and robots.txt | the response cache, the `robots.txt` asked for, what it kept from being fetched |

## Alerts

`deploy/prometheus/alerts.yml`, tested by `just lint-observability`
(`promtool test rules`). They show in Prometheus and in Grafana; to have
them sent somewhere, point an Alertmanager of your own at Prometheus.

| Alert | When | What to do |
|---|---|---|
| KageDown (critical) | Prometheus has not scraped Kage for 2 minutes | `docker compose --file deploy/compose.yaml ps`, `docker compose --file deploy/compose.yaml logs kage` |
| KageServerLockUnconfirmed (critical) | at a scrape, the server had not confirmed its database's lock for over 30 s (healthy: at most about 20 s); it needs a scrape interval of 15 s or less to land in the window before the stop: it is cut off from Postgres, stops writing 45 to 50 s after its last confirmation and exits with code 75 (see Metrics) | check the network between Kage and Postgres; Compose starts the server again, and it serves once it holds the lock ([first-start.md](first-start.md)) |
| KagePoolUnavailable (critical) | no proxy healthy or degraded for 10 minutes; a proxy whose backoff ran out counts as `backoff` until a probe passes, so an outage holds the alert through every expiry, and so does one whose latest probe failed while the outage guard of a pool of four or more holds (F151); under `egress.mode: direct` the pool is idle and publishes nothing, so it stays quiet | [proxies.md](proxies.md) |
| KageBandwidthBudget (warning) | 80 % of the month's cap used | [fetching.md](fetching.md#the-bandwidth-budget) |
| KageBreakerOpen (warning) | a domain is paused: `kage_breaker_open` is read from the stored breakers at every scrape, so it holds across a restart and ends with the pause, not with the trial that closes the breaker (F126) | [block-report.md](block-report.md) |
| KageBlockRateAboveThreshold (warning) | a domain was blocked, at levels 1 to 3, as often in the last hour as its `breaker_threshold` allows; the first block counts (see Metrics) | [block-report.md](block-report.md) |
| KagePersonaRetired (info) | a persona was retired in the last 30 minutes | [personas.md](personas.md) |
| KageWebhookFailing (warning) | a webhook delivery failed for good in the last 30 minutes; the first failure counts | [api.md](api.md) |

A Prometheus counter is born with its first sample, and `increase()` never
counts that one: before F110 the first failed delivery and the first block
of a domain were lost to the two alerts and to every panel that counts
events, so each fired one event late. The rule tests
(`deploy/prometheus/alerts.test.yml`) start their input series where the
exposition starts: at zero for a counter that exists from the start, at the
first event for one born with it.

## Metrics

The families and their labels are listed in spec §20. Kage's `/metrics`
is one exposition: the process's own series, what is read from the store
at the scrape, and each sidecar's series merged in, every one of them
under its `engine`. No label is a secret, a token or a proxy's address.

Every counter whose label values are known before any event exists at
zero from the start of the process: the webhook deliveries by event and
result, the jobs accepted and finished and the items finished by kind,
state and level, the crawl's pages, skips and duplicates, the API's
refusals by code (`scope_missing` included, counted where the handler asks
for the scope, F182), the scheduler's refusals by reason, the bytes by
direction and exit, the cache, the `robots.txt` asked for and the robots
denials by result, the replacements by reason, and the sidecars' own
counters by result and reason (F150). So the first event of each
is a change from zero, which `increase()` sees (F110). A counter with a
label that arrives with the event is still born with it: `kage_requests_total`
and `kage_attempts_total` carry a domain, `kage_api_requests_total` a
status. The block-rate alert and the block panels therefore read the
counter against its value an hour earlier,
`clamp_min(counter - (counter offset 1h or counter * 0), 0)`, which counts
the first block too and counts a restart's reset as zero; a query of your
own over those three families should do the same where one event matters.
A counter that exists from zero is reset by a restart like any other.

The `domain` label has at most `server.metrics_max_domains` (500) values
of its own: the domains the policy files name, labelled first at the
start, then the first ones asked for since the start. Every other domain
is counted under `domain="other"`, which sets no
`kage_policy_breaker_threshold`, so the block-rate alert does not fire for
it; `kage_breaker_open{domain="other"}` is 1 while any of them is paused,
and the block report's query lists those breakers by domain. A domain a
policy reload adds takes the room that is left; once the bound is reached
it stays `other` until the next start, and the reload logs "domains the
reloaded policies name have no metrics label of their own" with how many
and the first: restart the server, or raise the bound, to give them
theirs.

`kage_server_lock_held` and `kage_server_lock_confirmed_timestamp_seconds`
say whether the server still holds its database's lock (ADR 0005) and
when it last saw it held. The look that confirms it runs every 15 s and
gives up after 5 s. Cut the server off from its database (a partition)
and `kage_server_lock_held` drops to 0 at the first look, while the age,
`time() - kage_server_lock_confirmed_timestamp_seconds` (under about 30 s
on a healthy server), grows: past 30 s at a scrape `KageServerLockUnconfirmed` fires.
At 45 to 50 s the server stops writing to the database and stops by
itself with exit code 75, before Postgres could give the lock to another
server; then `KageDown` takes over. The metrics listener this profile
scrapes serves the lock's series all the while; the API's own `/metrics`
cannot look up its key during a cut and answers `500` (QA round four
lock-03). A cut that heals before that keeps the server: its next look
takes the lock again on a new connection.

`kage_proxy_state{state}` counts a proxy under the state its last probe
left it in. A proxy whose backoff ran out is `degraded` in the store and in
`kage proxy list` while it waits for the probe that decides it, a few
seconds at most; the gauge counts it as `backoff` until a probe passes, so
that an outage of the whole pool reads as a flat zero of `healthy` and
`degraded` through every expiry and `KagePoolUnavailable` holds its ten
minutes (F111). In a pool of four or more, while more than half fails its
latest probe, the outage guard freezes the counters and no proxy changes
state; the gauge then counts every proxy whose latest probe failed as
`backoff`, as the scheduler holds it (F169), so that a pool that died whole
reads as no usable proxy and the alert fires (F151). A proxy whose probe
saw an older proxy's exit address (`proxy_exit_shared`) is `degraded` in
the store but carries nothing, and counts as `backoff` too while the mark
lasts (F195). The gauge is set at
every change of a proxy's state, after every probe while the guard holds or
is lifted, and at every maintenance pass of the pool.

Since 2.0 `kage_requests_total`, `kage_attempts_total` and
`kage_attempt_duration_seconds` carry `level` (`"0"` to `"3"`) where 1.x
had `render`, `kage_jobs_accepted_total` and `kage_job_items_total` carry
it too, and `kage_bandwidth_bytes_total` carries `egress` (`pool`,
`host`). `kage_scheduler_in_flight` keeps `render`: it counts the paths
in use. A query of your own that read `render="browser"` reads
`level=~"2|3"` now ([levels-and-egress.md](levels-and-egress.md#upgrading-from-1x)).
