#!/usr/bin/env bash
# End-to-end test of the scheduler's rules over a pool of five proxies
# (spec §7.3, §8.2, §10.5, §11): the compose stack with real browsers in the
# world of the fetch test (deploy/compose.e2e.yaml), with five proxies whose
# exits the world's echo places in three /24s and four autonomous systems
# (deploy/compose.e2e-pool.yaml). Nothing here needs or reaches the
# internet.
#
# It shows, through the published port, the fixture site's log of who asked
# and the stack's own metrics:
#   - after a block, no retry leaves from the /24 of a persona the request
#     was blocked on, nor from its autonomous system while a persona of
#     another /24 and another system is untried (F168, QA round two's
#     pool-01): four sites that refuse every page, three attempts each;
#   - a flow's later site with one seat (`domain_concurrency: 1`) waits for
#     another persona's visit of it to end instead of a second address
#     reading it at once (F188, left-01);
#   - with every proxy of the pool stopped, the outage guard holds, the
#     gauge counts every proxy as `backoff` (F151), a batch is `paused`
#     with `no_persona` instead of failing on dead proxies, a synchronous
#     fetch is answered `no_persona` with `retry_at`, and when the proxies
#     are back the batch ends with its pages (F169, pool-02).
#
# It uses its own compose project name and host port. With
# KAGE_E2E_EVIDENCE set to a directory, what was measured is kept there.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_POOL_PORT:-18090}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false

# The shipped policies, a profile of one seat, and the domain rules of the
# test: the fixture site is the operator's own, so it may be read at the
# pace of a test.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
cat >> "${policies}/pacing-profiles.yaml" << 'EOF'

# Added by tests/e2e/pool.sh: `fast` with one seat on the site.
single:
  gap_ms: { dist: uniform, min: 300, max: 1200 }
  burst_pages: { min: 50, max: 50 }
  burst_break_ms: { dist: uniform, min: 0, max: 0 }
  daily_cap_per_persona_per_domain: 5000
  domain_concurrency: 1
  domain_rpm: 300
EOF
sed --in-place 's/^domains: {}$/domains:/' "${policies}/domains.yaml"
[ "$(grep '^[a-z_]*:' "${policies}/domains.yaml" | tail -n 1)" = 'domains:' ] \
    || { echo "FAIL: policies/domains.yaml does not end with its domains mapping" >&2; exit 1; }
{
    echo '  # Added by tests/e2e/pool.sh.'
    for name in pool1 pool2 pool3 pool4 dc2 outage; do
        printf '  %s.kage.test:\n    pacing: fast\n    min_dwell: 500ms\n    active_hours: ignore\n' "${name}"
    done
    printf '  dc1.kage.test:\n    pacing: single\n    min_dwell: 500ms\n    active_hours: ignore\n'
} >> "${policies}/domains.yaml"
chmod --recursive a+rX "${policies}"
scratch="$(mktemp -d)"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --file deploy/compose.e2e-pool.yaml
    --project-name "${KAGE_E2E_POOL_PROJECT:-kage-e2e-pool}")
base="http://127.0.0.1:${KAGE_PUBLISH_PORT}"
proxies=(proxy-a proxy-b proxy-c proxy-d proxy-e)

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- pool test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 160 kage world >&2 || true
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

# value <json> <expression over d>: prints the expression's value, read by a
# helper's node, so the host needs no JSON tool.
value() {
    printf '%s' "$1" | "${compose[@]}" exec -T world node -e \
        "let t = ''; process.stdin.on('data', (c) => (t += c)).on('end', () => { const d = JSON.parse(t); const v = ($2); console.log(typeof v === 'string' ? v : JSON.stringify(v)); });"
}
# check <what> <json> <expression over d>: fails unless the expression holds.
check() {
    [ "$(value "$2" "Boolean($3)")" = true ] || fail "$1: $(printf '%s' "$2" | head -c 3000)"
}
# got <url>: what a helper serves, asked from inside the network.
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
# call <method> <path> [token] [json body]: one request to the API; the
# status is left in ${status}, the body in ${body}.
call() {
    local method="$1" path="$2" token="${3:-}" data="${4:-}"
    local arguments=(--silent --show-error --max-time 280 --request "${method}"
        --output "${scratch}/body" --write-out '%{http_code}')
    if [ -n "${token}" ]; then
        arguments+=(--header "Authorization: Bearer ${token}")
    fi
    if [ -n "${data}" ]; then
        arguments+=(--header 'Content-Type: application/json' --data "${data}")
    fi
    status="$(curl "${arguments[@]}" "${base}${path}")"
    body="$(cat "${scratch}/body")"
}
expect() {
    [ "${status}" = "$1" ] || fail "$2: status ${status}, expected $1: $(printf '%s' "${body}" | head -c 2000)"
}
# keep <file> <text>: what was measured, when KAGE_E2E_EVIDENCE names a
# directory.
keep() {
    if [ -n "${KAGE_E2E_EVIDENCE:-}" ]; then
        mkdir -p "${KAGE_E2E_EVIDENCE}"
        printf '%s\n' "$2" > "${KAGE_E2E_EVIDENCE}/$1"
    fi
}
# The address each proxy has on the compose network, as the site sees it.
addresses() {
    "${compose[@]}" exec -T world node -e \
        "const dns = require('node:dns'); Promise.all(['proxy-a', 'proxy-b', 'proxy-c', 'proxy-d', 'proxy-e'].map((n) => dns.promises.lookup(n, { family: 4 }).then((r) => [r.address, n]))).then((p) => console.log(JSON.stringify(Object.fromEntries(p))))"
}
# The /24 and the system of each proxy, as the world's echo declares them.
nets="{ 'proxy-a': ['203.0.113', 'AS64501'], 'proxy-b': ['203.0.113', 'AS64501'], 'proxy-c': ['198.51.100', 'AS64502'], 'proxy-d': ['192.0.2', 'AS64501'], 'proxy-e': ['198.51.100', 'AS64503'] }"

echo "==> starting the stack with five proxies in three /24s and four systems"
"${compose[@]}" up --detach --wait --wait-timeout 300
personas=''
for _ in $(seq 1 80); do
    personas="$(kage persona list --json 2> /dev/null || true)"
    if [ -n "${personas}" ] && [ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 5 ]; then
        break
    fi
    sleep 3
done
keep personas.json "${personas}"
check "five ready personas" "${personas}" "d.personas.filter((p) => p.ready && p.state === 'active').length === 5"
check "both engines have a persona" "${personas}" "['chromium', 'firefox'].every((e) => d.personas.some((p) => p.engine === e))"
# Every proxy's exit and system as its probe read them.
seen=''
for _ in $(seq 1 40); do
    seen="$(psql "SELECT provider_ref || ' ' || coalesce(last_health->>'exit_ip', '-') || ' ' || coalesce(last_health->'geo'->>'asn', asn, '-') FROM proxies ORDER BY 1")"
    if ! grep -q ' -' <<< "${seen}" && [ "$(grep -c . <<< "${seen}")" -eq 5 ]; then
        break
    fi
    sleep 3
done
keep exits.txt "${seen}"
echo "${seen}" | sed 's/^/    /'
for expected in 'proxy-a:3128 203.0.113.10 AS64501' 'proxy-c:3128 198.51.100.30 AS64502' 'proxy-e:3128 198.51.100.50 AS64503'; do
    grep -qx "${expected}" <<< "${seen}" || fail "the probes did not read the declared exits: ${seen}"
done

echo "==> a client's key"
created="$(kage key create --name e2e-operator --scope admin --json)"
admin="$(value "${created}" "d.token")"
call POST /v1/admin/keys "${admin}" '{"name":"e2e-pool","scopes":["fetch","jobs"],"requests_per_minute":600,"max_concurrent":16}'
expect 201 "POST /v1/admin/keys"
client="$(value "${body}" "d.token")"

echo "==> after a block, no retry from the blocked /24, nor from its system while another is untried (F168)"
for site in pool1 pool2 pool3 pool4; do
    call POST /v1/fetch "${client}" "{\"url\":\"http://${site}.kage.test:8088/denied\",\"level\":2,\"max_retries\":2}"
    expect 200 "the fetch of ${site}"
    check "${site}: three attempts, each refused" "${body}" \
        "d.attempts.length === 3 && d.attempts.every((a) => a.outcome === 'forbidden')"
done
requests="$(got 'http://fixture.kage.test:8088/__kage/requests')"
keep block-requests.json "${requests}"
mapped="$(printf '{"requests": %s, "addresses": %s}' "${requests}" "$(addresses)")"
for site in pool1 pool2 pool3 pool4; do
    verdict="$(value "${mapped}" "((nets) => {
        const tried = d.requests.filter((r) => r.host === '${site}.kage.test:8088' && r.path === '/denied')
            .sort((a, b) => a.at - b.at).map((r) => d.addresses[String(r.from).replace(/^::ffff:/, '')] ?? String(r.from));
        const broken = [];
        tried.forEach((p, n) => {
            const before = tried.slice(0, n);
            if (n === 0 || !nets[p]) return;
            if (before.some((b) => nets[b][0] === nets[p][0])) {
                broken.push('attempt ' + (n + 1) + ' from ' + p + ', in the /24 of a block');
            } else if (before.some((b) => nets[b][1] === nets[p][1])) {
                const better = Object.keys(nets).filter((q) => !tried.slice(0, n + 1).includes(q)
                    && before.every((b) => nets[b][0] !== nets[q][0] && nets[b][1] !== nets[q][1]));
                if (better.length > 0) broken.push('attempt ' + (n + 1) + ' from ' + p + ', in the system of a block while ' + better.join(' ') + ' was untried');
            }
        });
        return { tried, broken, known: tried.every((p) => nets[p] !== undefined) };
    })(${nets})")"
    echo "    ${site}: $(value "${verdict}" "d.tried.join(' -> ')")"
    check "${site}: three attempts from proxies of the pool" "${verdict}" "d.tried.length === 3 && d.known"
    check "${site}: the rule of spec §8.2" "${verdict}" "d.broken.length === 0"
done

echo "==> a flow's later site with one seat waits for another persona's visit (F188)"
for page in v1 v2; do
    call POST /v1/fetch "${client}" "{\"url\":\"http://dc1.kage.test:8088/listing?page=${page}\",\"level\":2,\"persona\":{\"engines\":[\"chromium\"]}}"
    expect 200 "the visitor's page ${page}"
    check "the visitor's page ${page}" "${body}" "d.outcome === 'ok'"
done
call POST /v1/jobs "${client}" '{"type":"flow","level":2,"persona":{"engines":["firefox"]},"actions":[{"action":"goto","url":"http://dc2.kage.test:8088/listing?page=fa"},{"action":"wait_for","ms":500},{"action":"goto","url":"http://dc1.kage.test:8088/listing?page=fb"}]}'
expect 201 "the flow"
flow="$(value "${body}" "d.id")"
for _ in $(seq 1 90); do
    call GET "/v1/jobs/${flow}" "${client}"
    case "$(value "${body}" "d.state")" in succeeded | failed | partial | cancelled) break ;; esac
    sleep 2
done
keep flow.json "${body}"
check "the flow ended with its page" "${body}" "d.state === 'succeeded'"
requests="$(got 'http://fixture.kage.test:8088/__kage/requests')"
keep flow-requests.json "${requests}"
seats="$(value "${requests}" "((pages) => {
    const at = (page) => pages.find((r) => r.path === '/listing?page=' + page);
    const visitor = [at('v1'), at('v2')];
    const later = at('fb');
    return { visitor: visitor.map((r) => r && r.from), later: later && later.from,
        gap_ms: later && visitor[1] ? later.at - visitor[1].at : null };
})(d.filter((r) => r.host === 'dc1.kage.test:8088'))")"
keep flow-seats.json "${seats}"
echo "    the visitor at dc1 from $(value "${seats}" "d.visitor.join(', ')"); the flow's goto from $(value "${seats}" "d.later"), $(value "${seats}" "d.gap_ms") ms after its last page"
check "the flow's goto came from another persona" "${seats}" "d.later && d.later !== d.visitor[1]"
# The visit lasts 20 s after its last page (KAGE__SCHEDULER__VISIT_IDLE);
# a page or two of the flow's own come between, so the site sees the gap
# shorter by its pages' loads.
check "not before the visit was over" "${seats}" "d.gap_ms >= 15000"

echo "==> every proxy stopped: the guard holds, work waits, nothing fails (F169, F151)"
"${compose[@]}" stop "${proxies[@]}" > /dev/null 2>&1
exposition=''
for _ in $(seq 1 60); do
    exposition="$(got 'http://kage:9464/metrics')"
    if grep -qx 'kage_proxy_state{state="backoff"} 5' <<< "${exposition}"; then
        break
    fi
    sleep 3
done
keep outage-metrics.txt "$(grep '^kage_proxy_state' <<< "${exposition}")"
grep -qx 'kage_proxy_state{state="backoff"} 5' <<< "${exposition}" \
    || fail "the gauge does not count the stopped pool as backoff: $(grep '^kage_proxy_state' <<< "${exposition}" | tr '\n' ' ')"
states="$(psql "SELECT provider_ref || ' ' || state FROM proxies ORDER BY 1")"
keep outage-states.txt "${states}"
[ "$(psql "SELECT count(*) FROM proxies WHERE state IN ('healthy', 'degraded')")" -eq 5 ] \
    || fail "the counters were not frozen: ${states}"
[ "$(psql "SELECT count(*) FROM health_events WHERE kind = 'pool_outage_suspected'")" -gt 0 ] \
    || fail "no pool_outage_suspected"
echo "    $(tr '\n' ' ' <<< "${states}")"
call POST /v1/jobs "${client}" '{"type":"batch","defaults":{"level":2},"items":[{"url":"http://outage.kage.test:8088/listing?page=1"},{"url":"http://outage.kage.test:8088/listing?page=2"}]}'
expect 201 "the batch"
batch="$(value "${body}" "d.id")"
for _ in $(seq 1 15); do
    call GET "/v1/jobs/${batch}" "${client}"
    if [ "$(value "${body}" "Boolean(d.paused)")" = true ]; then
        break
    fi
    sleep 2
done
sleep 20
call GET "/v1/jobs/${batch}" "${client}"
keep outage-batch.json "${body}"
check "the batch waits the outage out" "${body}" \
    "d.paused && d.paused.reason === 'no_persona' && d.paused.until && !['failed', 'partial'].includes(d.state) && d.items.failed === 0"
call POST /v1/fetch "${client}" '{"url":"http://outage.kage.test:8088/listing?page=sync","level":2}'
expect 200 "the fetch during the outage"
keep outage-fetch.json "${body}"
check "a synchronous fetch is told when to ask again" "${body}" \
    "d.outcome === 'provider_error' && d.reason === 'no_persona' && d.retry_at && d.attempts.length === 0"
retry_at="$(value "${body}" "d.retry_at")"
call GET "/v1/jobs/${batch}" "${client}"
echo "    batch: $(value "${body}" "d.state + ', paused ' + d.paused.reason + ' until ' + d.paused.until"); the fetch: no_persona, retry_at ${retry_at}"

echo "==> the proxies are back: the batch ends with its pages"
"${compose[@]}" start "${proxies[@]}" > /dev/null 2>&1
for _ in $(seq 1 90); do
    call GET "/v1/jobs/${batch}" "${client}"
    case "$(value "${body}" "d.state")" in succeeded | failed | partial | cancelled) break ;; esac
    sleep 3
done
keep recovered-batch.json "${body}"
check "the batch succeeded" "${body}" "d.state === 'succeeded' && d.items.succeeded === 2"
exposition="$(got 'http://kage:9464/metrics')"
keep recovered-metrics.txt "$(grep '^kage_proxy_state' <<< "${exposition}")"
grep -qx 'kage_proxy_state{state="backoff"} 0' <<< "${exposition}" \
    || fail "the gauge still counts a proxy as backoff: $(grep '^kage_proxy_state' <<< "${exposition}" | tr '\n' ' ')"

echo "pool test passed"
