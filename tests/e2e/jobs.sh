#!/usr/bin/env bash
# End-to-end test of jobs (spec §16.6, §10.5, §8.4): the compose stack with
# real browsers and real personas on a static proxy list, in the world of
# the fetch test (deploy/compose.e2e.yaml) and a receiver of webhooks that
# verifies their signatures. Nothing here needs or reaches the internet.
#
# It shows, through the published port and nothing else:
#   - a batch of 50 pages of the fixture site read to completion by three
#     visitors of both engines, every item with its own result, submitted
#     twice under one Idempotency-Key and stored once;
#   - a flow of several steps (a typed address, a click by the persona's
#     hand, a wait, captures, the way back, a screenshot) in the browser
#     of a persona of each engine, paced, with what each capturing step
#     brought back;
#   - a batch that is cancelled while it is read: the page under way is
#     finished and not one more is begun;
#   - the job's events at the receiver: signed, verified there, sent from
#     Kage's own address and never through a proxy, the first one refused
#     and tried again;
#   - a synchronous fetch that outlasts the wall time, carried on as a
#     job, with the server stopped and started while it runs: its result
#     is there afterwards, under the request's id;
#   - the series of `/metrics` for jobs, and the admin purge of everything
#     the key fetched, on the audit log.
#
# The batch, the flows and the cancel are run by a TypeScript client that
# was generated from the API's OpenAPI document (tests/api-client).
#
# Needs what `just setup` installs: docker, curl, and pnpm for the client.
# It uses its own compose project name and host port.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_JOBS_PORT:-18083}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false
# A fetch that takes longer than this is carried on as a job.
export KAGE_E2E_SYNC_WALL_TIME=4s

# The shipped policies with the domain rule of this test: the fixture site
# is the operator's own, so it may be read at the pace of a test.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
cat >> "${policies}/domains.yaml" << 'EOF'

# Added by tests/e2e/jobs.sh.
domains:
  fixture.kage.test:
    pacing: fast
    min_dwell: 500ms
    active_hours: ignore
EOF
# A `domains: {}` of the shipped file would be a second key of that name.
if [ "$(grep --count '^domains:' "${policies}/domains.yaml")" -ne 1 ]; then
    grep --invert-match '^domains: {}$' "${policies}/domains.yaml" > "${policies}/domains.tmp"
    mv "${policies}/domains.tmp" "${policies}/domains.yaml"
fi
chmod --recursive a+rX "${policies}"
# What a call to the API answered.
scratch="$(mktemp -d)"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --project-name "${KAGE_E2E_JOBS_PROJECT:-kage-e2e-jobs}")
base="http://127.0.0.1:${KAGE_PUBLISH_PORT}"
site='http://fixture.kage.test:8088'
hook='http://hooks.kage.test:8090/kage'
# The secret of the receiver in deploy/compose.e2e.yaml.
hook_secret='e2e-only-webhook-secret'

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- jobs test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 160 kage browser-chromium browser-firefox >&2 || true
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

# A helper's node reads JSON, so the host needs no JSON tool (a sidecar
# cannot be the helper: it reaches nothing but kage).
# value <json> <expression over d>: prints the expression's value.
value() {
    printf '%s' "$1" | "${compose[@]}" exec -T world node -e \
        "let t = ''; process.stdin.on('data', (c) => (t += c)).on('end', () => { const d = JSON.parse(t); const v = ($2); console.log(typeof v === 'string' ? v : JSON.stringify(v)); });"
}
# check <what> <json> <expression over d>: fails unless the expression holds.
check() {
    [ "$(value "$2" "Boolean($3)")" = true ] || fail "$1: $(printf '%s' "$2" | head -c 3000)"
}
# got <url>: the JSON a helper serves, asked from inside the network.
got() {
    "${compose[@]}" exec -T world node -e \
        "fetch('$1').then((r) => r.text()).then((t) => console.log(t), (e) => { console.error(String(e)); process.exit(1); })"
}
kage() {
    "${compose[@]}" exec -T kage /usr/local/bin/kage "$@"
}
psql() {
    "${compose[@]}" exec -T postgres psql --username kage --dbname kage \
        --no-align --tuples-only --command "$1"
}
# call <method> <path> [token] [json body]: one request to the API. The
# status is left in ${status}, the body in ${body}, the headers in
# ${headers}.
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
# expect <status> <what>: the last call ended with that status.
expect() {
    [ "${status}" = "$1" ] || fail "$2: status ${status}, expected $1: $(printf '%s' "${body}" | head -c 2000)"
}
# problem <status> <code> <what>: the last call was refused with a problem
# document of that code.
problem() {
    expect "$1" "$3"
    check "$3" "${body}" "d.code === '$2' && d.status === $1"
}
# ready: waits until every persona of the static list is ready.
ready() {
    local personas=''
    for _ in $(seq 1 60); do
        personas="$(kage persona list --json 2> /dev/null || true)"
        if [ -n "${personas}" ] && [ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 3 ]; then
            return 0
        fi
        sleep 3
    done
    fail "the personas did not get ready: $(printf '%s' "${personas}" | head -c 1000)"
}

echo "==> starting the stack with a world of its own and a receiver of webhooks"
"${compose[@]}" up --detach --wait --wait-timeout 300
ready
personas="$(kage persona list --json)"
check "both engines have a persona" "${personas}" "['chromium', 'firefox'].every((e) => d.personas.some((p) => p.engine === e))"

echo "==> an operator's key, and a client's key that may fetch and run jobs"
created="$(kage key create --name e2e-operator --scope admin --json)"
admin="$(value "${created}" "d.token")"
call POST /v1/admin/keys "${admin}" '{"name":"e2e-jobs","scopes":["fetch","jobs"],"requests_per_minute":600,"max_concurrent":16}'
expect 201 "POST /v1/admin/keys"
client="$(value "${body}" "d.token")"
client_id="$(value "${body}" "d.key.id")"
call POST /v1/admin/keys "${admin}" '{"name":"e2e-fetch-only","scopes":["fetch"]}'
expect 201 "POST /v1/admin/keys"
fetch_only="$(value "${body}" "d.token")"
call POST /v1/jobs "${fetch_only}" "{\"type\":\"batch\",\"urls\":[\"${site}/\"]}"
problem 403 scope_missing "a job without the jobs scope"
call POST /v1/jobs "${client}" "{\"type\":\"batch\",\"urls\":[\"${site}/\"],\"webhook\":{\"url\":\"http://fixture.kage.test:8088/hook\",\"secret\":\"a secret of sixteen bytes or more\"}}"
problem 422 webhook_origin_not_allowed "a receiver the operator did not list"
[ "$(psql "SELECT count(*) FROM jobs")" -eq 0 ] || fail "a refused job was stored"
echo "    client ${client_id}; a job without the scope is 403, a receiver that is not listed 422"

echo "==> a batch of 50 pages, a flow in each engine, and a cancel, by the generated TypeScript client"
summary="$(cd tests/api-client && pnpm --silent run build && KAGE_API_URL="${base}" KAGE_API_KEY="${client}" \
    KAGE_E2E_SITE="${site}" KAGE_E2E_HOOK="${hook}" KAGE_E2E_HOOK_SECRET="${hook_secret}" \
    pnpm --silent run e2e:jobs)" || fail "the generated client failed"
check "the batch ended with every item succeeded, read by both engines" "${summary}" \
    "d.batch.state === 'succeeded' && d.batch.items.succeeded === 50 && d.batch.engines.chromium > 0 && d.batch.engines.firefox > 0 && d.batch.visitors <= 3"
check "a flow ran in each engine, paced, with three captures" "${summary}" \
    "d.flows.length === 2 && d.flows[0].engine === 'chromium' && d.flows[1].engine === 'firefox' && d.flows.every((f) => f.captures === 3 && f.screenshot_bytes > 1000 && f.steps.length === 10) && d.flows[0].persona !== d.flows[1].persona"
check "the cancel stopped the batch within one item" "${summary}" \
    "d.cancelled.succeeded <= d.cancelled.succeeded_at_cancel + 1 && d.cancelled.succeeded + d.cancelled.cancelled === 30 && d.refused === 'ssrf_denied'"
batch="$(value "${summary}" "d.batch.id")"
echo "    batch ${batch}: 50 pages in $(value "${summary}" "d.batch.seconds") s by $(value "${summary}" "d.batch.visitors") visitors, $(value "${summary}" "JSON.stringify(d.batch.engines)")"
echo "    $(value "${summary}" "d.flows.map((f) => f.engine + ': ' + f.seconds + ' s, steps ' + f.steps.map((s) => s.action + ' ' + s.ms + '/' + s.paced_ms).join(', ')).join('; ')")"
echo "    cancelled: $(value "${summary}" "d.cancelled.succeeded_at_cancel + ' pages had ended when the cancel came, ' + d.cancelled.succeeded + ' in the end, ' + d.cancelled.cancelled + ' cancelled'")"

echo "==> the site saw every page of the batch once, and of the cancelled batch only the pages that ended"
seen="$(got "${site}/__kage/requests")"
check "50 listing pages, each once" "${seen}" \
    "Array.from({ length: 50 }, (_, i) => '/listing?page=' + (101 + i)).every((p) => d.filter((r) => r.path === p).length === 1)"
check "no page of the cancelled batch after the one under way" "${seen}" \
    "d.filter((r) => r.path.startsWith('/slow?ms=1500&n=')).length === $(value "${summary}" "d.cancelled.succeeded")"
check "no request names a forwarding header" "${seen}" \
    "d.every((r) => !r.headers.some(([name]) => /^(x-forwarded-|forwarded\$|x-real-ip\$|via\$)/i.test(name)))"
stored="$(psql "SELECT count(*) FROM jobs WHERE idempotency_key IS NOT NULL")"
[ "${stored}" -eq 1 ] || fail "${stored} jobs are stored under the batch's Idempotency-Key, expected 1"
echo "    $(value "${seen}" "d.filter((r) => r.path.startsWith('/listing?page=1')).length") listing pages asked for; one job under the Idempotency-Key"

echo "==> the batch's events arrived signed, from Kage's own address, the first one after a retry"
deliveries=''
for _ in $(seq 1 30); do
    deliveries="$(got 'http://hooks.kage.test:8090/__deliveries')"
    if [ "$(value "${deliveries}" "d.some((e) => e.event === 'job.completed' && e.answered === 204 && e.body.job.id === '${batch}')")" = true ]; then
        break
    fi
    sleep 2
done
check "every delivery was verified by the receiver" "${deliveries}" \
    "d.length >= 3 && d.every((e) => e.verified && e.method === 'POST' && e.path === '/kage' && e.content_type === 'application/json' && /^kage\//.test(e.user_agent) && e.forwarded.length === 0 && e.age_s < 60)"
check "the first delivery was refused and came again under the same id" "${deliveries}" \
    "d[0].answered === 503 && d.filter((e) => e.delivery === d[0].delivery).length === 2 && d.filter((e) => e.delivery === d[0].delivery)[1].answered === 204"
check "one job.completed for the batch, with the job as its key sees it and nothing of what was fetched" "${deliveries}" \
    "d.filter((e) => e.event === 'job.completed' && e.body.job.id === '${batch}').length === 1 && d.some((e) => e.event === 'job.completed' && e.body.job.state === 'succeeded' && e.body.job.items.succeeded === 50 && e.body.type === 'job.completed' && e.body.id === e.delivery) && !JSON.stringify(d.map((e) => e.body)).includes('listing')"
check "progress was reported on the way" "${deliveries}" \
    "d.some((e) => e.event === 'job.progress' && e.body.job.id === '${batch}' && e.body.job.items.pending > 0)"
for proxy in proxy-a proxy-b proxy-c; do
    check "no webhook went through ${proxy}" "$(got "http://${proxy}:3128/__stats")" \
        "!Object.keys(d.targets).some((t) => t.includes('hooks'))"
done
call GET "/v1/jobs/${batch}" "${client}"
expect 200 "GET /v1/jobs/{id}"
check "the delivery log shows the retry, and of the receiver only its host" "${body}" \
    "d.webhook.host === 'hooks.kage.test' && d.webhook.deliveries.some((e) => e.attempts === 2 && e.state === 'delivered' && e.last_status === 204) && d.webhook.deliveries.every((e) => e.state === 'delivered' || e.state === 'superseded') && !JSON.stringify(d).includes('${hook_secret}')"
sealed="$(psql "SELECT count(*) FROM jobs WHERE webhook_secret IS NOT NULL AND position('${hook_secret}' in encode(webhook_secret, 'escape')) > 0")"
[ "${sealed}" -eq 0 ] || fail "a webhook secret is stored in the clear"
echo "    $(value "${deliveries}" "d.length + ' deliveries: ' + d.map((e) => e.event.replace('job.', '') + ' ' + e.answered).join(', ')")"

echo "==> /metrics: the jobs, their items and the deliveries, before the restart (the counters are the process's own)"
call GET /metrics "${admin}"
expect 200 "GET /metrics"
metrics="${scratch}/metrics"
printf '%s\n' "${body}" > "${metrics}"
for series in kage_jobs kage_job_items kage_webhook_deliveries_pending kage_job_items_total \
    kage_jobs_finished_total kage_webhook_deliveries_total; do
    grep --quiet "^${series}[{ ]" "${metrics}" || fail "no sample of ${series} in /metrics"
done
awk '/^kage_jobs\{/ && /job_type="batch"/ && /state="succeeded"/ { found = 1 } END { exit !found }' "${metrics}" \
    || fail "the batch is not counted: $(grep '^kage_jobs' "${metrics}" | head -8)"
awk '/^kage_jobs\{/ && /job_type="flow"/ && /state="succeeded"/ { found = 1 } END { exit !found }' "${metrics}" \
    || fail "the flows are not counted: $(grep '^kage_jobs' "${metrics}" | head -8)"
awk '/^kage_webhook_deliveries_total\{/ && /event="job.completed"/ && /result="delivered"/ { found = 1 } END { exit !found }' "${metrics}" \
    || fail "the delivery of job.completed is not counted: $(grep '^kage_webhook_deliveries_total' "${metrics}" | head -8)"
awk '/^kage_webhook_deliveries_total\{/ && /result="retry"/ { found = 1 } END { exit !found }' "${metrics}" \
    || fail "the delivery that was tried again is not counted: $(grep '^kage_webhook_deliveries_total' "${metrics}" | head -8)"
echo "    $(grep '^kage_jobs{' "${metrics}" | tr '\n' ' ')"

echo "==> a fetch that outlasts the wall time is carried on as a job, across a restart of the server"
call POST /v1/fetch "${client}" "{\"url\":\"${site}/slow?ms=20000&n=overflow\",\"timeout_ms\":90000}"
expect 202 "a fetch of 20 s against a wall time of 4 s"
request="$(value "${body}" "d.id")"
carried="$(value "${body}" "d.job_id")"
check "the 202 names the job that carries it on" "${body}" "d.state === 'running' && /^[0-9a-f-]{36}\$/.test(d.job_id) && d.result === '/v1/results/' + d.id"
call GET "/v1/jobs/${carried}" "${client}"
expect 200 "the job of the fetch"
check "a batch of one item, made by the overflow" "${body}" "d.type === 'batch' && d.origin === 'sync_overflow' && d.items.total === 1 && d.items.pending === 1"
call GET "/v1/results/${request}" "${client}"
expect 202 "the result while the fetch runs"
check "the result names the job too" "${body}" "d.job_id === '${carried}'"
# The server stops while the page is being fetched, and starts again. It
# waits for the page that is being read before it stops (jobs.wind_down,
# follow-up F39), so the restart takes as long as the page has left.
restarting="$(date +%s)"
"${compose[@]}" restart kage > /dev/null 2>&1 || fail "the server did not restart"
restarted_after="$(($(date +%s) - restarting))"
for _ in $(seq 1 60); do
    call GET /readyz || true
    [ "${status}" = 200 ] && break
    sleep 2
done
expect 200 "/readyz after the restart"
for _ in $(seq 1 90); do
    call GET "/v1/results/${request}" "${client}"
    [ "${status}" = 200 ] && break
    sleep 2
done
expect 200 "the result after the restart"
check "the fetch was finished under its own request" "${body}" \
    "d.id === '${request}' && d.state === 'finished' && d.outcome === 'ok' && d.status === 200 && d.final_url.endsWith('/slow?ms=20000&n=overflow') && d.attempts.length >= 1"
attempts="$(value "${body}" "d.attempts.length")"
# The page was being read when the server was told to stop: it was
# finished, not cut and fetched again. One attempt, and the site was asked
# once.
check "the page that was being read was finished by the attempt that began it" "${body}" \
    "d.attempts.length === 1 && d.attempts[0].outcome === 'ok' && d.attempts[0].reason === null"
seen="$(got "${site}/__kage/requests")"
check "the site was asked for the page once, across the restart" "${seen}" \
    "d.filter((r) => r.path === '/slow?ms=20000&n=overflow').length === 1"
call GET "/v1/jobs/${carried}" "${client}"
expect 200 "the job after the restart"
check "the job ended with its item" "${body}" "d.state === 'succeeded' && d.items.succeeded === 1"
call GET "/v1/jobs/${carried}/items" "${client}"
check "the item is the request of the fetch" "${body}" "d.items.length === 1 && d.items[0].request_id === '${request}' && d.items[0].state === 'succeeded'"
echo "    request ${request}: 202 with job ${carried}, the server restarted (its stop took ${restarted_after} s: it waited for the page), finished after ${attempts} attempt(s), the site asked once"

echo "==> an admin purges everything the key fetched; it is on the audit log"
before="$(psql "SELECT count(*) FROM requests WHERE api_key_id = '${client_id}'")"
call POST /v1/admin/purge "${client}" "{\"key_id\":\"${client_id}\"}"
problem 403 scope_missing "a purge by a key that is no admin's"
call POST /v1/admin/purge "${admin}" "{\"key_id\":\"${client_id}\"}"
expect 200 "POST /v1/admin/purge"
check "the key's jobs and results went" "${body}" "d.jobs === 5 && d.requests === ${before} && d.artifacts >= 50"
purged="${body}"
left="$(psql "SELECT (SELECT count(*) FROM requests WHERE api_key_id = '${client_id}') + (SELECT count(*) FROM jobs WHERE api_key_id = '${client_id}') + (SELECT count(*) FROM artifacts a JOIN requests r ON r.id = a.request_id WHERE r.api_key_id = '${client_id}')")"
[ "${left}" -eq 0 ] || fail "${left} rows of the key are left after the purge"
call GET "/v1/jobs/${batch}" "${client}"
problem 404 job_not_found "a purged job"
call GET "/v1/results/${request}" "${client}"
problem 404 result_not_found "a purged result"
call GET '/v1/admin/audit?limit=50' "${admin}"
expect 200 "GET /v1/admin/audit"
check "the purge is on the audit log with what went" "${body}" \
    "d.events.some((e) => e.action === 'admin.data.purge' && e.outcome === 'success' && e.target === '${client_id}' && e.details.result.jobs === 5) && d.events.some((e) => e.action === 'admin.data.purge' && e.outcome === 'denied')"
echo "    ${purged}"

echo "jobs test passed"
