#!/usr/bin/env bash
# End-to-end test of the SSRF guard under `security.resolve_on_host`
# (spec §19, §25; QA round two's F57, F167 and F166): the compose stack
# with real browsers and the world of deploy/compose.e2e-guard.yaml, where
# two names of two sites are public to Kage and reach the fixture site
# through the proxies, and every other name of the world is a name of the
# operator's own network. Nothing here needs or reaches the internet.
#
# It shows, with the API as the one way in:
#   - a name of the operator's network asked for directly: 422 ssrf_denied;
#   - a public page that sends the browser there by a redirect, a meta
#     refresh, a script or a self-submitting form, in both engines:
#     ssrf_denied at the first attempt, nothing of a page kept, and the
#     private page never asked for: the forwarder looked the name up and
#     refused it before any proxy was asked (F57);
#   - a flow whose click leads there: the same;
#   - a redirect to `file:`, `data:`, `ftp:` or `javascript:` in both
#     engines: ssrf_denied at the first attempt, nothing kept (F167);
#   - a level 0 redirect to another site, asked for from that site's own
#     plain exit, never from the first site's (F166).
#
# It uses its own compose project name and host port.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_GUARD_PORT:-18091}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false

# The shipped policies, with no persona asleep (policies.sh).
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
tests/e2e/policies.sh "${policies}"
chmod --recursive a+rX "${policies}"
scratch="$(mktemp -d)"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --file deploy/compose.e2e-guard.yaml
    --project-name "${KAGE_E2E_GUARD_PROJECT:-kage-e2e-guard}")
base="http://127.0.0.1:${KAGE_PUBLISH_PORT}"

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- guard test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 120 kage browser-chromium browser-firefox >&2 || true
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
# call <method> <path> <token> [json]: leaves ${status} and ${body}.
call() {
    local method="$1" path="$2" token="$3" data="${4:-}"
    local arguments=(--silent --show-error --max-time 200 --request "${method}"
        --output "${scratch}/body" --write-out '%{http_code}'
        --header "Authorization: Bearer ${token}")
    if [ -n "${data}" ]; then
        arguments+=(--header 'Content-Type: application/json' --data "${data}")
    fi
    status="$(curl "${arguments[@]}" "${base}${path}")"
    body="$(cat "${scratch}/body")"
}
# fetched <json>: POST /v1/fetch, which must be answered with a result.
fetched() {
    call POST /v1/fetch "${token}" "$1"
    [ "${status}" = 200 ] || fail "the fetch was not answered with a result: ${status} ${body}"
}

PUBLIC="http://pub.kage-guard.test:8088"
OTHER="http://pub.other-guard.test:8088"
THIRD="http://pub.third-guard.test:8088"
SECRET="http://rules.kage.test:8088/guard/secret"
# A result that is the guard's refusal at its first attempt, nothing of a
# page in it.
DENIED="d.outcome === 'ssrf_denied' && d.attempts.length === 1 && !d.dom_html && !d.title && !d.block"

echo "==> starting the stack with the guard's world, Kage looking names up itself"
"${compose[@]}" up --detach --wait --wait-timeout 300
checked="$(kage config check)"
check "the setting is on, under the pool" "${checked}" \
    "d.config.security.resolve_on_host === true && d.egress.mode === 'proxies'"

personas=''
for _ in $(seq 1 60); do
    personas="$(kage persona list --json 2> /dev/null || true)"
    if [ -n "${personas}" ] && [ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 3 ]; then
        break
    fi
    sleep 3
done
check "three ready personas of both engines" "${personas}" \
    "d.personas.filter((p) => p.ready).length === 3 && ['chromium', 'firefox'].every((e) => d.personas.some((p) => p.engine === e))"
token="$(value "$(kage key create --name e2e-guard --scope fetch --scope jobs --json)" "d.token")"

echo "==> a name of the operator's network, asked for directly, is refused before anything is sent"
call POST /v1/fetch "${token}" "{\"url\":\"${SECRET}\",\"level\":2}"
[ "${status}" = 422 ] || fail "the private name was not refused: ${status} ${body}"
check "422 ssrf_denied, by its address" "${body}" "d.code === 'ssrf_denied' && /private/i.test(d.detail)"

echo "==> a public page that sends the browser there: ssrf_denied, nothing kept, nothing asked (F57)"
got "http://fixture:8088/__kage/reset" > /dev/null
encoded="$(value "\"${SECRET}\"" "encodeURIComponent(d)")"
for engine in chromium firefox; do
    for road in redirect meta script form; do
        fetched "{\"url\":\"${PUBLIC}/guard/${road}?to=${encoded}\",\"level\":2,\"persona\":{\"engines\":[\"${engine}\"]},\"include\":[\"dom\"],\"timeout_ms\":60000}"
        check "${engine}, ${road}: the guard's refusal at the hop" "${body}" "${DENIED}"
        echo "    ${engine} ${road}: $(value "${body}" "d.outcome + ' at ' + d.final_url")"
    done
done
call POST /v1/jobs "${token}" "{\"type\":\"flow\",\"level\":3,\"behavior\":{\"dwell_ms\":500},\"timeout_ms\":120000,\"actions\":[{\"action\":\"goto\",\"url\":\"${PUBLIC}/guard/link?to=${encoded}\"},{\"action\":\"click\",\"selector\":\"a.next\"}]}"
[ "${status}" = 201 ] || fail "the flow was not accepted: ${status} ${body}"
job="$(value "${body}" "d.id")"
state=''
for _ in $(seq 1 90); do
    call GET "/v1/jobs/${job}" "${token}"
    state="$(value "${body}" "d.state")"
    case "${state}" in succeeded | partial | failed | cancelled) break ;; esac
    sleep 2
done
call GET "/v1/jobs/${job}/items" "${token}"
check "a flow whose click leads there ends ssrf_denied, nothing of it kept" "${body}" \
    "d.items.at(-1).outcome === 'ssrf_denied' && !d.items.at(-1).result.dom_html"
echo "    flow: ${state}, $(value "${body}" "d.items.at(-1).outcome")"
seen="$(got "http://fixture:8088/__kage/requests")"
check "the private page was never asked for" "${seen}" \
    "!d.some((r) => r.path.startsWith('/guard/secret'))"
check "the public pages were" "${seen}" \
    "d.some((r) => r.host === 'pub.kage-guard.test:8088' && r.path.startsWith('/guard/meta'))"
for proxy in proxy-a proxy-b proxy-c; do
    stats="$(got "http://${proxy}:3128/__stats")"
    check "${proxy} was never asked for the private name" "${stats}" \
        "!Object.keys(d.targets).some((t) => t.startsWith('rules.kage.test'))"
done

echo "==> a redirect to a scheme the guard refuses: ssrf_denied in both engines, nothing kept (F167)"
for engine in chromium firefox; do
    for target in 'file:///etc/passwd' 'data:text/html,<b>x</b>' 'ftp://pub.kage-guard.test/x' 'javascript:alert(1)'; do
        to="$(value "\"${target}\"" "encodeURIComponent(d)")"
        fetched "{\"url\":\"${PUBLIC}/guard/redirect?to=${to}\",\"level\":2,\"persona\":{\"engines\":[\"${engine}\"]},\"include\":[\"dom\"],\"timeout_ms\":60000}"
        check "${engine}, a redirect to ${target}, reported where it pointed" "${body}" \
            "${DENIED} && d.final_url.startsWith('${target%%:*}:')"
    done
    echo "    ${engine}: file, data, ftp, javascript: ssrf_denied"
done

echo "==> a level 0 redirect to another site leaves from that site's own exit (F166)"
# Two sites no persona has read as a person: each can have a scout, and its
# scout's proxy is its plain exit. A page of each, asked for at level 0,
# gives each its exit; when the draw gave both one, the second site is
# given another scout, so that the hop's exit is not the first site's.
exit_of() {
    psql "SELECT p.host FROM site_scouts s JOIN personas pe ON pe.id = s.persona_id JOIN proxies p ON p.id = pe.proxy_id WHERE s.site = '$1'"
}
fetched "{\"url\":\"${OTHER}/landing\",\"level\":0}"
fetched "{\"url\":\"${THIRD}/landing\",\"level\":0}"
first="$(exit_of other-guard.test)"
second="$(exit_of third-guard.test)"
[ -n "${first}" ] && [ -n "${second}" ] || fail "the two sites have no plain exits: '${first}' '${second}'"
if [ "${first}" = "${second}" ]; then
    admin="$(value "$(kage key create --name e2e-guard-admin --scope admin --json)" "d.token")"
    call POST /v1/admin/sites/third-guard.test/scout "${admin}"
    [ "${status}" = 200 ] || fail "third-guard.test could not be given another scout: ${status} ${body}"
    second="$(exit_of third-guard.test)"
fi
[ "${first}" != "${second}" ] || fail "the two sites still leave from one exit: ${first}"
got "http://fixture:8088/__kage/reset" > /dev/null
landing="$(value "\"${THIRD}/landing\"" "encodeURIComponent(d)")"
fetched "{\"url\":\"${OTHER}/guard/redirect?to=${landing}\",\"level\":0}"
check "level 0 follows the hop" "${body}" \
    "d.outcome === 'ok' && d.final_url === '${THIRD}/landing' && d.attempts.length === 1 && d.attempts[0].exit === 'pool'"
# Each request came from the address of the proxy that carried it.
address_of() {
    "${compose[@]}" exec -T world node -e \
        "require('node:dns').lookup('$1', (e, a) => { if (e) process.exit(1); console.log(a); })"
}
first_address="$(address_of "${first}")"
second_address="$(address_of "${second}")"
seen="$(got "http://fixture:8088/__kage/requests")"
check "other-guard.test's page left from its own exit" "${seen}" \
    "d.some((r) => r.host === 'pub.other-guard.test:8088' && r.path.startsWith('/guard/redirect') && r.from.endsWith('${first_address}'))"
check "the hop left from third-guard.test's exit only, naming kage" "${seen}" \
    "d.some((r) => r.host === 'pub.third-guard.test:8088') && d.filter((r) => r.host === 'pub.third-guard.test:8088').every((r) => r.from.endsWith('${second_address}') && r.headers.some(([n, v]) => n.toLowerCase() === 'user-agent' && v.startsWith('kage/')))"
echo "    other-guard.test from ${first}, the hop to third-guard.test from ${second}"

echo "guard test passed"
