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
#   - a fetch that waits for its site's one visitor past the wall time,
#     carried on and cancelled: the site never gets it, and its result
#     says it was cancelled (F106, F109);
#   - the series of `/metrics` for jobs, and the admin purge of everything
#     the key fetched, on the audit log;
#   - a flow whose later goto meets the policy of its own site (F101): the
#     site's floor refuses it at acceptance, the site's enforced robots.txt
#     ends it before anything is asked, and a wall the site shows is booked
#     to that site, not to the flow's first one;
#   - a flow whose page opens a tab (F184), in each engine: the persona's
#     later click and keys reach the flow's page and not the tab, the step
#     names the tab, and the tab runs no more once the flow has ended;
#   - a flow cancelled during a wait: it stops at the step under way and
#     none of its later pages is asked for (F175);
#   - a second server on the same database: refused at start, and it says
#     why (F174).
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

# The shipped policies (no persona asleep: policies.sh) with the domain rule
# of this test: the fixture site is the operator's own, so it may be read at
# the pace of a test.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
tests/e2e/policies.sh "${policies}"
# The rule is an entry of the shipped file's `domains` mapping, its last
# key: a second `domains:` would be a duplicate key, which the policy
# loader refuses, and `domains: {}` takes no entry.
sed --in-place 's/^domains: {}$/domains:/' "${policies}/domains.yaml"
[ "$(grep '^[a-z_]*:' "${policies}/domains.yaml" | tail -n 1)" = 'domains:' ] \
    || { echo "FAIL: policies/domains.yaml does not end with its domains mapping" >&2; exit 1; }
cat >> "${policies}/domains.yaml" << 'EOF'
  # Added by tests/e2e/jobs.sh.
  fixture.kage.test:
    pacing: fast
    min_dwell: 500ms
    active_hours: ignore
  # A site nobody else in this test reads: the cancel of a fetch that
  # waits for its one visitor.
  lenient.kage.test:
    min_dwell: 500ms
    active_hours: ignore
  # A flow meets the policy of each site it goes to (F101): this one
  # wants level 3 at least,
  rules.kage.test:
    min_level: 3
    pacing: fast
    min_dwell: 500ms
    active_hours: ignore
  # and this one, a site of its own (field.test) that nobody has read when
  # the flows come, so that it has a scout to ask for its file, enforces
  # its robots.txt.
  wall.field.test:
    robots: enforce
    pacing: fast
    min_dwell: 500ms
    active_hours: ignore
EOF
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
# A bound on the script (`just test-e2e`) stops it with TERM: leave through
# the cleanup then too, whatever the script was waiting for.
trap 'exit 143' TERM INT

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
call POST /v1/jobs "${fetch_only}" "{\"type\":\"batch\",\"urls\":[\"${site}/\"],\"defaults\":{\"level\":3}}"
problem 403 scope_missing "a job without the jobs scope"
call POST /v1/jobs "${client}" "{\"type\":\"batch\",\"urls\":[\"${site}/\"],\"defaults\":{\"level\":3},\"webhook\":{\"url\":\"http://fixture.kage.test:8088/hook\",\"secret\":\"a secret of sixteen bytes or more\"}}"
problem 422 webhook_origin_not_allowed "a receiver the operator did not list"
# A job keeps its items until they run, and Kage holds no caller's
# credential: a fetch may send one at level 0, a job may not.
call POST /v1/jobs "${client}" "{\"type\":\"batch\",\"items\":[{\"url\":\"${site}/\",\"level\":0,\"headers\":{\"Cookie\":\"session=e2e\"}}]}"
problem 422 header_denied "a caller's cookie kept by a job"
check "the refusal names the header" "${body}" "d.header === 'Cookie' && d.item === 0"
[ "$(psql "SELECT count(*) FROM jobs")" -eq 0 ] || fail "a refused job was stored"
echo "    client ${client_id}; a job without the scope is 403, a receiver that is not listed 422, a cookie in a job 422"

echo "==> a batch of 50 pages, a flow in each engine, and a cancel, by the generated TypeScript client"
summary="$(cd tests/api-client && pnpm --silent run build && KAGE_API_URL="${base}" KAGE_API_KEY="${client}" \
    KAGE_E2E_SITE="${site}" KAGE_E2E_HOOK="${hook}" KAGE_E2E_HOOK_SECRET="${hook_secret}" \
    pnpm --silent run e2e:jobs)" || fail "the generated client failed"
check "the batch ended with every item succeeded, read by both engines" "${summary}" \
    "d.batch.state === 'succeeded' && d.batch.items.succeeded === 50 && d.batch.engines.chromium > 0 && d.batch.engines.firefox > 0 && d.batch.visitors <= 3"
check "a flow ran in each engine, paced, with three captures" "${summary}" \
    "d.flows.length === 2 && d.flows[0].engine === 'chromium' && d.flows[1].engine === 'firefox' && d.flows.every((f) => f.captures === 3 && f.screenshot_bytes > 1000 && f.steps.length === 10) && d.flows[0].persona !== d.flows[1].persona"
check "the cancel stopped the batch within one item" "${summary}" \
    "d.cancelled.succeeded <= d.cancelled.succeeded_at_cancel + 1 && d.cancelled.succeeded + d.cancelled.cancelled === 30"
check "a job that cannot be accepted whole is refused whole: a flow to a metadata address, a batch without a level" "${summary}" \
    "d.refused === 'ssrf_denied' && d.unleveled === 'level_required'"
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

echo "==> the batch's events arrived signed, from Kage's own address, the first one refused and then retried or overtaken"
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
# The receiver refuses the first event. It comes again under its own id
# after the first retry's wait (2 s here), unless it is a `job.progress` and
# the batch queued a newer one first: then it is dropped (`superseded`,
# spec §16.6) and the receiver is told the newer figure. A batch that reads
# ten pages in less than the wait and the sender's poll does the second.
first="$(value "${deliveries}" "d[0].delivery")"
first_state="$(psql "SELECT state FROM webhook_deliveries WHERE id = '${first}'")"
check "the first delivery was refused, then came again under its own id or was overtaken by newer progress" "${deliveries}" \
    "d[0].answered === 503 && ((d.filter((e) => e.delivery === d[0].delivery).length === 2 && d.filter((e) => e.delivery === d[0].delivery)[1].answered === 204 && '${first_state}' === 'delivered') || (d[0].event === 'job.progress' && d.filter((e) => e.delivery === d[0].delivery).length === 1 && '${first_state}' === 'superseded'))"
echo "    the first event was refused, then ${first_state}"
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
check "the delivery log shows the refused event as it ended, and of the receiver only its host" "${body}" \
    "d.webhook.host === 'hooks.kage.test' && d.webhook.deliveries.some((e) => '${first_state}' === 'delivered' ? (e.attempts === 2 && e.state === 'delivered' && e.last_status === 204) : (e.attempts === 1 && e.state === 'superseded' && e.last_status === 503)) && d.webhook.deliveries.every((e) => e.state === 'delivered' || e.state === 'superseded') && !JSON.stringify(d).includes('${hook_secret}')"
sealed="$(psql "SELECT count(*) FROM jobs WHERE webhook_secret IS NOT NULL AND position('${hook_secret}' in encode(webhook_secret, 'escape')) > 0")"
[ "${sealed}" -eq 0 ] || fail "a webhook secret is stored in the clear"
echo "    $(value "${deliveries}" "d.length + ' deliveries: ' + d.map((e) => e.event.replace('job.', '') + ' ' + e.answered).join(', ')")"

echo "==> /metrics: the jobs, their items and the deliveries, before the restart (the counters are the process's own)"
call GET /metrics "${admin}"
expect 200 "GET /metrics"
metrics="${scratch}/metrics"
printf '%s\n' "${body}" > "${metrics}"
for series in kage_jobs kage_job_items kage_webhook_deliveries_pending kage_job_items_total \
    kage_jobs_accepted_total kage_jobs_finished_total kage_webhook_deliveries_total; do
    grep --quiet "^${series}[{ ]" "${metrics}" || fail "no sample of ${series} in /metrics"
done
# Jobs and their items are counted by their level (spec §25).
for job_type in batch flow; do
    awk -v job_type="job_type=\"${job_type}\"" '/^kage_jobs_accepted_total\{/ && index($0, job_type) && /level="3"/ { found = 1 } END { exit !found }' "${metrics}" \
        || fail "the ${job_type} jobs are not counted at level 3: $(grep '^kage_jobs_accepted_total' "${metrics}" | head -8)"
    awk -v job_type="job_type=\"${job_type}\"" '/^kage_job_items_total\{/ && index($0, job_type) && /state="succeeded"/ && /level="3"/ { found = 1 } END { exit !found }' "${metrics}" \
        || fail "the items of the ${job_type} jobs are not counted at level 3: $(grep '^kage_job_items_total' "${metrics}" | head -8)"
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
call POST /v1/fetch "${client}" "{\"url\":\"${site}/slow?ms=20000&n=overflow\",\"level\":3,\"timeout_ms\":90000}"
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
# Readiness is asked all the while: from the signal until the page has been
# read, the listener stays and says `503 stopping`, never a reset (F121).
readiness="${scratch}/readyz-during-stop"
: > "${readiness}"
(
    for _ in $(seq 1 300); do
        printf '%s\n' "$(curl --silent --max-time 2 --write-out ' %{http_code}' "${base}/readyz" 2> /dev/null || true)" >> "${readiness}"
        sleep 0.2
    done
) &
poller=$!
restarting="$(date +%s)"
"${compose[@]}" restart kage > /dev/null 2>&1 || fail "the server did not restart"
restarted_after="$(($(date +%s) - restarting))"
kill "${poller}" 2> /dev/null || true
wait "${poller}" 2> /dev/null || true
stopping_answers="$(grep --count '"status":"stopping".* 503$' "${readiness}" || true)"
[ "${stopping_answers}" -ge 3 ] \
    || fail "/readyz did not answer 503 stopping while the server stopped: $(sort "${readiness}" | uniq --count | head -c 2000)"
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
echo "    request ${request}: 202 with job ${carried}, the server restarted (its stop took ${restarted_after} s: it waited for the page, and /readyz said 503 stopping ${stopping_answers} times meanwhile), finished after ${attempts} attempt(s), the site asked once"

echo "==> a fetch that waits for its site's one visitor is carried on, and cancelled: the site never gets it"
# One seat on the site and a gap of a minute: the second page waits for the
# visitor of the first (F106, never another persona), past the wall time,
# and is carried on as a job. The job's cancel takes it out of the queue at
# once (F109), and its result says so.
seat='{"custom":{"gap_ms":{"dist":"uniform","min":60000,"max":60000},"burst_pages":{"min":50,"max":50},"burst_break_ms":{"dist":"uniform","min":0,"max":0},"daily_cap_per_persona_per_domain":100000,"domain_concurrency":1,"domain_rpm":1000}}'
lenient='http://lenient.kage.test:8088'
call POST /v1/fetch "${client}" "{\"url\":\"${lenient}/slow?ms=10&n=seat-1\",\"level\":2,\"pacing\":${seat}}"
# A browser that opens for the first time may take longer than the wall
# time: then the page is read as a job, and its result is waited for. The
# step leaves one job of the key, the second page's, or two.
seat_jobs=1
if [ "${status}" = 202 ]; then
    seat_jobs=2
    first_request="$(value "${body}" "d.id")"
    for _ in $(seq 1 60); do
        call GET "/v1/results/${first_request}" "${client}"
        [ "${status}" = 200 ] && break
        sleep 2
    done
fi
expect 200 "the visitor's first page"
check "the visitor read the first page" "${body}" "d.outcome === 'ok'"
call POST /v1/fetch "${client}" "{\"url\":\"${lenient}/slow?ms=10&n=seat-2\",\"level\":2,\"pacing\":${seat}}"
expect 202 "the second page, waiting for the visitor's gap of a minute"
waiting_request="$(value "${body}" "d.id")"
waiting_job="$(value "${body}" "d.job_id")"
cancelling="$(date +%s)"
call POST "/v1/jobs/${waiting_job}/cancel" "${client}"
expect 200 "POST /v1/jobs/{id}/cancel of the carried fetch"
for _ in $(seq 1 20); do
    call GET "/v1/jobs/${waiting_job}" "${client}"
    [ "$(value "${body}" "d.state")" = cancelled ] && break
    sleep 1
done
check "the carried fetch's job is cancelled at once" "${body}" "d.state === 'cancelled' && d.items.cancelled === 1 && d.items.pending === 0"
cancelled_after="$(($(date +%s) - cancelling))"
call GET "/v1/results/${waiting_request}" "${client}"
expect 200 "the result of the cancelled fetch"
check "the request ended with its job: provider_error, cancelled, nothing attempted" "${body}" \
    "d.id === '${waiting_request}' && d.state === 'finished' && d.outcome === 'provider_error' && d.reason === 'cancelled' && d.attempts.length === 0"
seen="$(got "${site}/__kage/requests")"
check "the site got the first page once and never the cancelled one" "${seen}" \
    "d.filter((r) => r.path === '/slow?ms=10&n=seat-1').length === 1 && d.filter((r) => r.path === '/slow?ms=10&n=seat-2').length === 0"
call GET "/v1/jobs/${waiting_job}/items" "${client}"
expect 200 "the items of the cancelled job"
check "the item shows what ended it, as its result does (F155)" "${body}" \
    "d.items.length === 1 && d.items[0].result !== null && d.items[0].result.outcome === 'provider_error' && d.items[0].result.reason === 'cancelled' && d.items[0].result.id === '${waiting_request}'"
echo "    request ${waiting_request}: 202 with job ${waiting_job}, cancelled in ${cancelled_after} s, 200 provider_error/cancelled, never asked of the site"

echo "==> an operator's key reads any key's job, and the trail says so; every cancel is on it (F117)"
call POST /v1/admin/keys "${admin}" '{"name":"e2e-operator-jobs","scopes":["admin","jobs"]}'
expect 201 "POST /v1/admin/keys"
operator="$(value "${body}" "d.token")"
operator_id="$(value "${body}" "d.key.id")"
call POST /v1/admin/keys "${admin}" '{"name":"e2e-other-jobs","scopes":["jobs"]}'
expect 201 "POST /v1/admin/keys"
other="$(value "${body}" "d.token")"
call GET "/v1/jobs/${waiting_job}" "${other}"
problem 404 job_not_found "another key's job, for a key without admin"
call GET "/v1/jobs/${waiting_job}" "${operator}"
expect 200 "an operator reads another key's job"
call GET '/v1/admin/audit?limit=200' "${admin}"
expect 200 "GET /v1/admin/audit"
check "the operator's read is on the trail with both keys" "${body}" \
    "d.events.some((e) => e.action === 'job.show' && e.api_key_id === '${operator_id}' && e.target === '${waiting_job}' && e.details.owner_key === '${client_id}' && e.details.another_keys === true)"
check "the owner's cancel is on the trail" "${body}" \
    "d.events.some((e) => e.action === 'job.cancel' && e.api_key_id === '${client_id}' && e.target === '${waiting_job}' && e.details.another_keys === false)"
echo "    another key: 404; the operator: 200, on the trail with owner_key ${client_id}; the owner's cancel on the trail"

echo "==> an admin purges everything the key fetched; it is on the audit log"
before="$(psql "SELECT count(*) FROM requests WHERE api_key_id = '${client_id}'")"
# Five jobs of the steps above, and those of the step of one seat.
jobs_of_key=$((5 + seat_jobs))
call POST /v1/admin/purge "${client}" "{\"key_id\":\"${client_id}\"}"
problem 403 scope_missing "a purge by a key that is no admin's"
call POST /v1/admin/purge "${admin}" "{\"key_id\":\"${client_id}\"}"
expect 200 "POST /v1/admin/purge"
check "the key's jobs and results went" "${body}" "d.jobs === ${jobs_of_key} && d.requests === ${before} && d.artifacts >= 50"
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
    "d.events.some((e) => e.action === 'admin.data.purge' && e.outcome === 'success' && e.target === '${client_id}' && e.details.result.jobs === ${jobs_of_key}) && d.events.some((e) => e.action === 'admin.data.purge' && e.outcome === 'denied')"
echo "    ${purged}"

echo "==> every goto of a flow meets its own site's policy, and a wall is booked to the site that showed it (F101)"
rules='http://rules.kage.test:8088'
wall='http://wall.field.test:8088'
# wait_flow <job id>: waits until the job ends; its state is left in ${state}.
wait_flow() {
    state=''
    for _ in $(seq 1 90); do
        call GET "/v1/jobs/$1" "${client}"
        state="$(value "${body}" "d.state")"
        case "${state}" in succeeded | partial | failed | cancelled) return 0 ;; esac
        sleep 2
    done
    fail "the flow $1 did not end: ${state}"
}
# A later goto to a site whose floor is above the flow's level is refused at
# acceptance, naming the step, as a fetch of that address would be.
call POST /v1/jobs "${client}" "{\"type\":\"flow\",\"level\":2,\"actions\":[{\"action\":\"goto\",\"url\":\"${site}/landing\"},{\"action\":\"goto\",\"url\":\"${rules}/landing?f101\"}]}"
problem 422 level_below_policy "a flow's second goto under its site's floor"
check "the refusal names the step and the floor" "${body}" "d.step === 1 && d.min_level === 3"
# A later goto to what the site's enforced robots.txt disallows: the flow
# ends robots_denied before anything is asked, with that address's rule.
got "http://fixture:8088/__kage/reset" > /dev/null
call POST /v1/jobs "${client}" "{\"type\":\"flow\",\"level\":3,\"behavior\":{\"dwell_ms\":500},\"timeout_ms\":120000,\"actions\":[{\"action\":\"goto\",\"url\":\"${site}/landing?f101\"},{\"action\":\"goto\",\"url\":\"${wall}/site/private/later\"}]}"
expect 201 "a flow whose second goto robots.txt disallows"
denied="$(value "${body}" "d.id")"
wait_flow "${denied}"
[ "${state}" = failed ] || fail "the flow to a disallowed address ended ${state}"
call GET "/v1/jobs/${denied}/items" "${client}"
check "it ended robots_denied with the rule of its second address" "${body}" \
    "d.items.at(-1).outcome === 'robots_denied' && d.items.at(-1).result.robots.allowed === false && d.items.at(-1).result.robots.matched_rule === 'Disallow: /site/private'"
seen="$(got "http://fixture:8088/__kage/requests")"
check "the site saw its robots.txt and none of the flow's pages" "${seen}" \
    "d.every((r) => !r.path.includes('f101') && !r.path.includes('/site/private'))"
# A wall at the flow's second site cools the persona there and counts on
# that site's breaker; the first site, which showed no wall, is left as it
# was.
fixture_cooling() {
    psql "SELECT count(*) FROM persona_domain_state WHERE domain = 'fixture.kage.test' AND cooldown_until > now()"
}
fixture_blocks() {
    psql "SELECT coalesce(max(block_count_1h), 0) FROM domain_breakers WHERE domain = 'fixture.kage.test'"
}
before="$(fixture_cooling)"
blocks_before="$(fixture_blocks)"
call POST /v1/jobs "${client}" "{\"type\":\"flow\",\"level\":3,\"behavior\":{\"dwell_ms\":500},\"timeout_ms\":120000,\"max_retries\":0,\"actions\":[{\"action\":\"goto\",\"url\":\"${site}/landing?f101-wall\"},{\"action\":\"goto\",\"url\":\"${wall}/challenge/stuck?f101\"}]}"
expect 201 "a flow whose second site shows a wall"
walled="$(value "${body}" "d.id")"
wait_flow "${walled}"
call GET "/v1/jobs/${walled}/items" "${client}"
check "the flow met the wall at its second site" "${body}" \
    "['challenge_failed', 'forbidden', 'captcha'].includes(d.items.at(-1).outcome) && d.items.at(-1).result.final_url.startsWith('${wall}/')"
walled_state="$(psql "SELECT count(*) || ' ' || coalesce(max(block_count_24h), 0) FROM persona_domain_state WHERE domain = 'wall.field.test' AND cooldown_until > now()")"
[ "${walled_state}" = '1 1' ] || fail "the wall's site does not hold the block: ${walled_state}"
[ "$(fixture_cooling)" = "${before}" ] || fail "the first site of the flow was booked the wall"
breakers="$(psql "SELECT coalesce(max(block_count_1h), 0) FROM domain_breakers WHERE domain = 'wall.field.test'")"
[ "${breakers}" = 1 ] || fail "the wall's breaker did not count it: ${breakers}"
[ "$(fixture_blocks)" = "${blocks_before}" ] || fail "the first site's breaker counted a wall it did not show"
echo "    floor: 422 at step 1; robots: denied at the second address, nothing asked; wall: booked to wall.field.test (its breaker at ${breakers}), the fixture's left at ${blocks_before}"

echo "==> a tab a flow's page opens is the flow's: the keys stay on the flow's page, the step names the tab, and it ends with the flow (F184)"
# The round's flow (flows-02): a link with target=_blank, a wait, then a
# click and typed text meant for the flow's own page. Both pages tell the
# fixture every key and press they get; the opened one also beats every
# 250 ms while it is open.
alive() {
    got "http://fixture:8088/__kage/reports" | node -e "let t = ''; process.stdin.on('data', (c) => (t += c)).on('end', () => console.log(JSON.parse(t).filter((r) => r.page === 'opener' && r.type === 'alive').length));"
}
opened_note=''
for engine in chromium firefox; do
    got "http://fixture:8088/__kage/reset" > /dev/null
    call POST /v1/jobs "${client}" "{\"type\":\"flow\",\"level\":3,\"max_retries\":0,\"persona\":{\"engines\":[\"${engine}\"]},\"behavior\":{\"dwell_ms\":500},\"timeout_ms\":120000,\"actions\":[{\"action\":\"goto\",\"url\":\"${site}/opener/?${engine}\"},{\"action\":\"wait_for\",\"selector\":\"body[data-ready]\"},{\"action\":\"click\",\"selector\":\"#blank\"},{\"action\":\"wait_for\",\"ms\":2500},{\"action\":\"click\",\"selector\":\"#q\"},{\"action\":\"type\",\"selector\":\"#q\",\"text\":\"hello\"}]}"
    expect 201 "a flow whose page opens a tab (${engine})"
    opener="$(value "${body}" "d.id")"
    wait_flow "${opener}"
    [ "${state}" = succeeded ] || fail "the flow that opened a tab ended ${state} on ${engine}"
    call GET "/v1/jobs/${opener}/items" "${client}"
    check "every step ok, the tab named once, the page it ended on the flow's own (${engine})" "${body}" \
        "d.items.filter((i) => i.step.action !== 'final').every((i) => i.step.ok === true) && JSON.stringify(d.items.flatMap((i) => i.step.opened_tabs)) === JSON.stringify(['${site}/opener/new']) && d.items.at(-1).result.final_url.startsWith('${site}/opener/')"
    steps="$(value "${body}" "d.items.filter((i) => i.step.opened_tabs.length > 0).map((i) => i.step.action).join(',')")"
    sleep 1
    before="$(alive)"
    reports="$(got "http://fixture:8088/__kage/reports")"
    check "the flow's page heard the five keys and the presses, the tab none (${engine})" "${reports}" \
        "d.filter((r) => r.page === 'opener' && r.who === 'opener' && r.type === 'keydown').map((r) => r.key).join('') === 'hello' && d.filter((r) => r.page === 'opener' && r.who === 'opener' && r.type === 'mousedown').length >= 3 && !d.some((r) => r.page === 'opener' && r.who === 'opened' && r.type !== 'alive')"
    [ "${before}" -gt 0 ] || fail "the tab never ran on ${engine}: the link opened none"
    sleep 3
    [ "$(alive)" = "${before}" ] || fail "the tab ran on after its flow ended on ${engine}"
    opened_note="${opened_note} ${engine}: named by ${steps}, ${before} beats while open, none after;"
done
echo "   ${opened_note}"

echo "==> a flow's cancel stops it at the step under way: no later page is asked for (F175)"
# The round's flow (pool-04): a page, a wait of 15 s, a page, a wait, a
# page. The cancel comes during the first wait; the sidecar finishes that
# step and begins no other.
got "http://fixture:8088/__kage/reset" > /dev/null
call POST /v1/jobs "${client}" "{\"type\":\"flow\",\"level\":2,\"max_retries\":0,\"timeout_ms\":120000,\"actions\":[{\"action\":\"goto\",\"url\":\"${site}/landing?f175-a\"},{\"action\":\"wait_for\",\"ms\":15000},{\"action\":\"goto\",\"url\":\"${site}/landing?f175-b\"},{\"action\":\"wait_for\",\"ms\":5000},{\"action\":\"goto\",\"url\":\"${site}/landing?f175-c\"}]}"
expect 201 "a flow of three pages with waits between them"
stopping="$(value "${body}" "d.id")"
asked_first=false
for _ in $(seq 1 90); do
    seen="$(got "http://fixture:8088/__kage/requests")"
    asked_first="$(value "${seen}" "d.some((r) => r.path.includes('f175-a'))")"
    [ "${asked_first}" = true ] && break
    sleep 1
done
[ "${asked_first}" = true ] || fail "the flow's first page was never asked for"
sleep 2
call POST "/v1/jobs/${stopping}/cancel" "${client}"
expect 200 "the cancel of the flow"
wait_flow "${stopping}"
[ "${state}" = cancelled ] || fail "the cancelled flow ended ${state}"
call GET "/v1/jobs/${stopping}/items" "${client}"
check "every item of the cancelled flow is cancelled" "${body}" \
    "d.items.length > 0 && d.items.every((i) => i.state === 'cancelled')"
# Past the end of the wait that was under way, and of the next page's.
sleep 20
seen="$(got "http://fixture:8088/__kage/requests")"
check "no later page of the cancelled flow was asked for" "${seen}" \
    "d.some((r) => r.path.includes('f175-a')) && !d.some((r) => r.path.includes('f175-b') || r.path.includes('f175-c'))"
echo "    flow ${stopping}: cancelled during its first wait, its later pages never asked for"

echo "==> a second server on the same database refuses to start and says why (F174)"
if second="$("${compose[@]}" run --rm --no-deps -T kage serve 2>&1)"; then
    fail "a second server started on the same database: ${second}"
fi
case "${second}" in
    *"another kage server is running on this database"*) ;;
    *) fail "the second server did not say why it stopped: ${second}" ;;
esac
expect_ready() {
    call GET /readyz
    expect 200 "/readyz of the first server, after the second was refused"
}
expect_ready
echo "    refused: another kage server is running on this database; the first still ready"

echo "jobs test passed"
