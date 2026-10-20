# Observability: logs, traces, metrics, dashboards, alerts

What Kage says about itself, where it goes, and how to read it (spec §20).
Logs and metrics are always there; traces leave the process only when a
collector is named; the dashboards and alerts come with the
`observability` profile.

## Logs

Kage and both sidecars write one JSON object per line on stdout
(`docker compose logs`). A line written inside a request carries
`trace_id` and `span_id`: the same `trace_id` in Kage's lines and in the
sidecar's for one request, also when no trace is sent anywhere. To follow
one request:

```bash
docker compose --file deploy/compose.yaml logs --no-color kage browser-chromium browser-firefox | grep '"trace_id":"<trace id>"'
```

No line holds a secret, a proxy credential, an API token, a cookie's
value or a request body; `tests/e2e/observability.sh` greps every line of
every service for the stack's secrets after real traffic (the redaction
audit). `RUST_LOG` sets Kage's level (`info` unless said); `LOG_LEVEL` a
sidecar's.

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
fingerprint or a body. Traces are sent only when Kage is given
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
| `jaeger` | Jaeger 2.22.0 | keeps the traces for seven days (Badger, volume `jaeger-data`) |
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
| Kage · bandwidth | the share of the plan's monthly cap used (`bandwidth.cap`, 250 GB unless set), bytes through the forwarder (the host's own connection under `egress.mode: direct` included, which the cap does not count) |
| Kage · cache and robots.txt | the response cache, the `robots.txt` asked for, what it kept from being fetched |

## Alerts

`deploy/prometheus/alerts.yml`, tested by `just lint-observability`
(`promtool test rules`). They show in Prometheus and in Grafana; to have
them sent somewhere, point an Alertmanager of your own at Prometheus.

| Alert | When | What to do |
|---|---|---|
| KageDown (critical) | Prometheus has not scraped Kage for 2 minutes | `docker compose ps`, `docker compose logs kage` |
| KagePoolUnavailable (critical) | no proxy healthy or degraded for 10 minutes; under `egress.mode: direct` the pool is idle and publishes nothing, so it stays quiet | [proxies.md](proxies.md) |
| KageBandwidthBudget (warning) | 80 % of the month's cap used | [fetching.md](fetching.md#the-bandwidth-budget) |
| KageBreakerOpen (warning) | a domain is paused | [block-report.md](block-report.md) |
| KageBlockRateAboveThreshold (warning) | a domain was blocked, at levels 1 to 3, as often in the last hour as its `breaker_threshold` allows | [block-report.md](block-report.md) |
| KagePersonaRetired (info) | a persona was retired in the last 30 minutes | [personas.md](personas.md) |
| KageWebhookFailing (warning) | a webhook delivery failed for good in the last 30 minutes | [api.md](api.md) |

## Metrics

The families and their labels are listed in spec §20. Kage's `/metrics`
is one exposition: the process's own series, what is read from the store
at the scrape, and each sidecar's series merged in, every one of them
under its `engine`. No label is a secret, a token or a proxy's address.

Since 2.0 `kage_requests_total`, `kage_attempts_total` and
`kage_attempt_duration_seconds` carry `level` (`"0"` to `"3"`) where 1.x
had `render`, `kage_jobs_accepted_total` and `kage_job_items_total` carry
it too, and `kage_bandwidth_bytes_total` carries `egress` (`pool`,
`host`). `kage_scheduler_in_flight` keeps `render`: it counts the paths
in use. A query of your own that read `render="browser"` reads
`level=~"2|3"` now ([levels-and-egress.md](levels-and-egress.md#upgrading-from-1x)).
