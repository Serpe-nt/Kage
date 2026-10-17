#!/usr/bin/env bash
# End-to-end test of the `observability` profile (spec §20): the world of
# the fetch test (deploy/compose.e2e.yaml) with the profile's collector,
# Jaeger, Prometheus and Grafana (deploy/compose.observability.yaml).
# Nothing here needs or reaches the internet.
#
# It shows:
#   - one trace in Jaeger for a page fetched through `POST /v1/fetch`, from
#     Kage's API through its scheduler and its attempt to the sidecar that
#     drove the browser, joined by the traceparent of Kage's own request;
#   - the lines of Kage and of that sidecar for the request carrying the
#     trace's id;
#   - every scrape target up, the alert rules loaded, and Grafana answering
#     with both data sources and the seven dashboards;
#   - the log redaction audit: no secret of the stack, no proxy credential
#     header and no API token in any line any service wrote, nor in any
#     span exported, after a page, a batch job with a webhook and a refused
#     request.
#
# Needs what `just setup` installs: docker and curl. It uses its own
# compose project name and host ports.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh
# The secrets of deploy/.env, which the audit looks for.
set -a
# shellcheck disable=SC1091
. deploy/.env
set +a

export KAGE_PUBLISH_PORT="${KAGE_E2E_OBSERVABILITY_PORT:-18086}"
export KAGE_GRAFANA_PORT="${KAGE_E2E_GRAFANA_PORT:-13086}"
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
chmod --recursive a+rX "${policies}"
scratch="$(mktemp -d)"
evidence="${KAGE_E2E_EVIDENCE:-${scratch}}"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --file deploy/compose.observability.yaml --profile observability
    --project-name "${KAGE_E2E_OBSERVABILITY_PROJECT:-kage-e2e-observability}")
base="http://127.0.0.1:${KAGE_PUBLISH_PORT}"
grafana="http://127.0.0.1:${KAGE_GRAFANA_PORT}"

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- observability test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 80 kage browser-chromium otel-collector jaeger prometheus grafana >&2 || true
    fi
    "${compose[@]}" down --volumes --remove-orphans > /dev/null 2>&1 || true
    rm -rf "${policies}" "${scratch}"
    exit "${status}"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}
value() {
    printf '%s' "$1" | "${compose[@]}" exec -T browser-chromium node -e \
        "let t = ''; process.stdin.on('data', (c) => (t += c)).on('end', () => { const d = JSON.parse(t); const v = ($2); console.log(typeof v === 'string' ? v : JSON.stringify(v)); });"
}
check() {
    [ "$(value "$2" "Boolean($3)")" = true ] || fail "$1: $(printf '%s' "$2" | head -c 3000)"
}
# got <url>: a GET from inside the compose network.
got() {
    "${compose[@]}" exec -T browser-chromium node -e \
        "fetch('$1').then((r) => r.text()).then((t) => console.log(t), (e) => { console.error(String(e)); process.exit(1); })"
}
kage() {
    "${compose[@]}" exec -T kage /usr/local/bin/kage "$@"
}
# search <service> <traces>: Jaeger's v3 search for a service's traces of
# the last hour (Jaeger 2.22 has no v1 search).
search() {
    local from to
    from="$(date -u -d '-1 hour' +%Y-%m-%dT%H:%M:%SZ)"
    to="$(date -u -d '+5 minutes' +%Y-%m-%dT%H:%M:%SZ)"
    printf 'http://jaeger:16686/api/v3/traces?query.service_name=%s&query.start_time_min=%s&query.start_time_max=%s&query.num_traces=%s'         "$1" "${from}" "${to}" "$2"
}
call() {
    local method="$1" path="$2" token="${3:-}" data="${4:-}"
    local arguments=(--silent --show-error --max-time 200 --request "${method}"
        --dump-header "${scratch}/headers" --output "${scratch}/body" --write-out '%{http_code}')
    if [ -n "${token}" ]; then
        arguments+=(--header "Authorization: Bearer ${token}")
    fi
    if [ -n "${data}" ]; then
        arguments+=(--header 'Content-Type: application/json' --data "${data}")
    fi
    status="$(curl "${arguments[@]}" "${base}${path}")"
    body="$(cat "${scratch}/body")"
    headers="$(tr -d '\r' < "${scratch}/headers")"
}
expect() {
    [ "${status}" = "$1" ] || fail "$2: status ${status}, expected $1: $(printf '%s' "${body}" | head -c 2000)"
}

echo "==> starting the stack with a world of its own and the observability profile"
"${compose[@]}" up --detach --wait --wait-timeout 300

echo "==> Grafana is published on loopback only, and nothing else of the profile"
for service in otel-collector jaeger prometheus; do
    published="$(docker inspect --format '{{json .HostConfig.PortBindings}}' "$("${compose[@]}" ps --quiet "${service}")")"
    [ "${published}" = '{}' ] || fail "${service} publishes a port: ${published}"
done
published="$(docker inspect --format '{{json .HostConfig.PortBindings}}' "$("${compose[@]}" ps --quiet grafana)")"
case "${published}" in
    *'"HostIp":"127.0.0.1"'*) ;;
    *) fail "Grafana is published beyond loopback: ${published}" ;;
esac

echo "==> the pool adopts the static list and a persona is ready on every proxy"
personas=''
for _ in $(seq 1 60); do
    personas="$(kage persona list --json 2> /dev/null || true)"
    if [ -n "${personas}" ] && [ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 3 ]; then
        break
    fi
    sleep 3
done
check "three ready personas" "${personas}" "d.personas.length === 3 && d.personas.every((p) => p.ready)"

echo "==> a page, a batch job with a webhook, a refused request"
created="$(kage key create --name e2e-observability --scope fetch --scope jobs --json)"
token="$(value "${created}" "d.token")"
call POST /v1/fetch "${token}" '{"url":"http://fixture.kage.test:8088/landing","render":"browser","timeout_ms":60000}'
expect 200 "POST /v1/fetch"
request="$(printf '%s\n' "${headers}" | sed -n 's/^x-request-id: //p')"
[ -n "${request}" ] || fail "the answer names no request: ${headers}"
check "the page was fetched" "${body}" "d.outcome === 'ok'"
call POST /v1/jobs "${token}" '{"type":"batch","urls":["http://fixture.kage.test:8088/listing?page=901","http://fixture.kage.test:8088/listing?page=902"],"webhook":{"url":"http://hooks.kage.test:8090/kage","secret":"e2e-only-webhook-secret"}}'
expect 201 "POST /v1/jobs"
job="$(value "${body}" "d.id")"
for _ in $(seq 1 90); do
    call GET "/v1/jobs/${job}" "${token}"
    if [ "$(value "${body}" "['succeeded','partial','failed','cancelled'].includes(d.state)")" = true ]; then
        break
    fi
    sleep 2
done
check "the job succeeded" "${body}" "d.state === 'succeeded'"
call POST /v1/fetch "x${token}" '{"url":"http://fixture.kage.test:8088/"}'
expect 401 "a wrong key"
echo "    request ${request}, job ${job}"

echo "==> the queries of the runbooks block-report.md and field-campaign.md run against the schema"
sed -n '/^```sql$/,/^```$/p' docs/runbooks/block-report.md docs/runbooks/field-campaign.md \
    | grep -v '^```' > "${scratch}/block-report.sql"
[ "$(grep -c ';$' "${scratch}/block-report.sql")" -eq 4 ] || fail "the runbooks' queries were not found"
"${compose[@]}" exec -T postgres psql --username kage --dbname kage --set ON_ERROR_STOP=1 --quiet \
    < "${scratch}/block-report.sql" > /dev/null || fail "a query of the runbooks does not run"

echo "==> one trace from the API to the browser, in Jaeger"
trace=''
for _ in $(seq 1 30); do
    traces="$(got "$(search kage 200)")"
    if trace="$(printf '%s' "${traces}" | "${compose[@]}" exec -T world node /e2e/observability.mjs trace "${request}" 2> "${scratch}/trace-error")"; then
        break
    fi
    trace=''
    sleep 2
done
[ -n "${trace}" ] || fail "$(cat "${scratch}/trace-error")"
trace_id="$(value "${trace}" "d.trace")"
got "http://jaeger:16686/api/v3/traces/${trace_id}" > "${evidence}/trace.json"
echo "    ${trace}"

echo "==> the lines of the request carry the trace's id, in Kage and in the sidecar"
"${compose[@]}" logs --no-color kage browser-chromium browser-firefox > "${scratch}/stack.log"
grep -q "\"trace_id\":\"${trace_id}\"" <(grep '^kage-' "${scratch}/stack.log") \
    || fail "no line of kage carries the trace ${trace_id}"
grep -Eq "^browser-(chromium|firefox).*\"trace_id\":\"${trace_id}\"" "${scratch}/stack.log" \
    || fail "no line of a sidecar carries the trace ${trace_id}"
echo "    kage: $(grep -c "\"trace_id\":\"${trace_id}\"" <(grep '^kage-' "${scratch}/stack.log")) lines, sidecars: $(grep -Ec "^browser-.*\"trace_id\":\"${trace_id}\"" "${scratch}/stack.log") lines"

echo "==> every scrape target is up, and the alert rules are loaded"
for _ in $(seq 1 30); do
    up="$(got 'http://prometheus:9090/api/v1/query?query=up')"
    if [ "$(value "${up}" "d.data.result.length === 4 && d.data.result.every((r) => r.value[1] === '1')")" = true ]; then
        break
    fi
    sleep 2
done
check "kage, the collector, Jaeger and Prometheus are up" "${up}" \
    "['kage','otel-collector','jaeger','prometheus'].every((job) => d.data.result.some((r) => r.metric.job === job && r.value[1] === '1'))"
rules="$(got 'http://prometheus:9090/api/v1/rules')"
check "the seven rules of Kage are loaded and healthy" "${rules}" \
    "d.data.groups.length === 1 && d.data.groups[0].rules.length === 7 && d.data.groups[0].rules.every((r) => r.health === 'ok')"
names=(kage_requests_total kage_attempts_total kage_policy_breaker_threshold kage_persona_state
    kage_proxy_state kage_bandwidth_budget_ratio kage_api_requests_total kage_jobs
    kage_webhook_deliveries_total sidecar_contexts_open process_resident_memory_bytes)
wanted="$(printf "'%s'," "${names[@]}")"
# Scraped every 15 s: what the last requests made is there after the next scrape.
for _ in $(seq 1 30); do
    series="$(got 'http://prometheus:9090/api/v1/label/__name__/values')"
    if [ "$(value "${series}" "[${wanted}].every((name) => d.data.includes(name))")" = true ]; then
        break
    fi
    sleep 2
done
for name in "${names[@]}"; do
    check "Prometheus holds ${name}" "${series}" "d.data.includes('${name}')"
done
memory="$(got 'http://prometheus:9090/api/v1/query?query=process_resident_memory_bytes')"
check "each sidecar's memory is a series of its own" "${memory}" \
    "['chromium','firefox'].every((engine) => d.data.result.filter((r) => r.metric.engine === engine).length === 1)"

echo "==> Grafana answers with both data sources and the seven dashboards"
auth="admin:${KAGE_GRAFANA_ADMIN_PASSWORD}"
for uid in prometheus jaeger; do
    health="$(curl --silent --show-error --user "${auth}" "${grafana}/api/datasources/uid/${uid}/health")"
    check "the ${uid} data source is healthy" "${health}" "d.status === 'OK'"
done
dashboards="$(curl --silent --show-error --user "${auth}" "${grafana}/api/search?tag=kage")"
check "seven dashboards in the folder Kage" "${dashboards}" \
    "d.length === 7 && d.every((x) => x.folderTitle === 'Kage')"
[ "$(curl --silent --output /dev/null --write-out '%{http_code}' "${grafana}/api/search")" = 401 ] \
    || fail "Grafana answers without a login"
echo "    $(value "${dashboards}" "d.map((x) => x.title).join(', ')")"

echo "==> the redaction audit: no secret in any line or span"
"${compose[@]}" logs --no-color > "${scratch}/all.log"
for service in kage kage-sidecar-chromium kage-sidecar-firefox; do
    got "$(search "${service}" 1000)" >> "${scratch}/all.log"
done
secrets="$(printf '%s\n' "${KAGE_POSTGRES_PASSWORD}" "${KAGE_SIDECAR_TOKEN}" "${KAGE_MASTER_KEY}" \
    "${KAGE_GRAFANA_ADMIN_PASSWORD}" "${token}" "${token#*_*_}" e2e-only-a e2e-only-b e2e-only-c \
    e2e-only-webhook-secret)"
audit="$("${compose[@]}" exec -T -e AUDIT_SECRETS="${secrets}" world node /e2e/observability.mjs audit < "${scratch}/all.log")" \
    || fail "the redaction audit found something (see above)"
echo "    ${audit}"
cp "${scratch}/all.log" "${evidence}/all-lines-and-spans.log" 2> /dev/null || true
echo "observability: all checks passed"
