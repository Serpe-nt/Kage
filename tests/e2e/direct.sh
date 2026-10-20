#!/usr/bin/env bash
# End-to-end test of `egress.mode: direct` (spec §25, ADR 0012): the
# compose stack with real browsers and a world of its own
# (deploy/compose.e2e.yaml), with Kage leaving from its own connection.
# Nothing here needs or reaches the internet.
#
# It shows, with `kage` as the one way in:
#   - the host's own row and its persona, placed by the host's own geo
#     lookup, generated and ready; the pool idle: a provider is configured
#     and nothing of it is adopted, probed or used;
#   - a page of the fixture site at level 0, by the plain client from the
#     host's connection, and at level 3, by the host persona's browser:
#     the site sees one address for both, and no proxy carries a byte;
#   - a name the operator did not list as private, which resolves to a
#     private address of this network, never reached;
#   - every byte counted as the host's, none against the pool's budget;
#   - `kage config check` saying where requests leave from, with its
#     warnings.
#
# It uses its own compose project name and host port.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_DIRECT_PORT:-18088}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false
# The host's own connection, and the names of this world as the operator's
# own private ones. rules.kage.test, an alias of the fixture site, is left
# out on purpose.
export KAGE_E2E_EGRESS_MODE=direct
export KAGE_E2E_PRIVATE_NAMES='["fixture.kage.test","world.kage.test"]'

# The shipped policies, unchanged.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
chmod --recursive a+rX "${policies}"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --project-name "${KAGE_E2E_DIRECT_PROJECT:-kage-e2e-direct}")

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- direct test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 120 kage browser-chromium browser-firefox >&2 || true
    fi
    "${compose[@]}" down --volumes --remove-orphans > /dev/null 2>&1 || true
    rm -rf "${policies}"
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
# fetch_json <arguments>: the JSON report of `kage fetch`; the exit status is
# left in ${fetch_status}.
fetch_json() {
    fetch_status=0
    report="$(kage fetch "$@" --json 2> /dev/null)" || fetch_status=$?
}
psql() {
    "${compose[@]}" exec -T postgres psql --username kage --dbname kage \
        --no-align --tuples-only --command "$1"
}

echo "==> starting the stack with a world of its own, leaving from kage's own connection"
"${compose[@]}" up --detach --wait --wait-timeout 300

echo "==> kage config check says where requests leave from"
checked="$(kage config check)"
check "direct, level 0 from the host, one host persona" "${checked}" \
    "d.egress.mode === 'direct' && d.egress.plain === 'host' && d.egress.host_personas === 1 && d.egress.plain_user_agent.startsWith('kage/')"
check "the idle pool and the private names are warned about" "${checked}" \
    "d.egress.warnings.some((w) => w.includes('the pool is idle')) && d.egress.warnings.some((w) => w.includes('fixture.kage.test'))"
echo "    $(value "${checked}" "d.egress.warnings.length + ' warnings'")"

echo "==> the host's own row and its persona, placed by the host's own lookup"
personas=''
for _ in $(seq 1 60); do
    personas="$(kage persona list --json 2> /dev/null || true)"
    if [ -n "${personas}" ] && [ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 1 ]; then
        break
    fi
    sleep 3
done
check "one ready persona" "${personas}" \
    "d.personas.length === 1 && d.personas[0].ready && d.personas[0].state === 'active'"
check "placed where the host's lookup says (the world's echo: Berlin)" "${personas}" \
    "d.personas[0].country === 'DE' && d.personas[0].timezone === 'Europe/Berlin'"
host="$(value "${personas}" "d.personas[0].id")"
engine="$(value "${personas}" "d.personas[0].engine")"
proxies="$(kage proxy list --json)"
check "the one row is the host's; nothing of the provider's list was adopted" "${proxies}" \
    "d.proxies.length === 1 && d.proxies[0].provider === 'host' && d.proxies[0].state === 'healthy'"
echo "    persona ${host} (${engine}), $(value "${personas}" "d.personas[0].locale")"

echo "==> level 0 and level 3 of the same page, both from the host's own address"
got "http://fixture:8088/__kage/reset" > /dev/null
fetch_json "http://fixture.kage.test:8088/" --level 0
[ "${fetch_status}" -eq 0 ] || fail "kage fetch --level 0 failed: ${report}"
check "level 0: the plain client, from the host" "${report}" \
    "d.outcome === 'ok' && d.persona === null && d.attempts.length === 1 && d.attempts[0].exit === 'host' && d.attempts[0].persona_id === null"
plain="$(got "http://fixture:8088/__kage/requests")"
check "the site saw a client named kage" "${plain}" \
    "d.length === 1 && d[0].headers.some(([n, v]) => n.toLowerCase() === 'user-agent' && v.startsWith('kage/'))"
address="$(value "${plain}" "d[0].from")"
got "http://fixture:8088/__kage/reset" > /dev/null
fetch_json "http://fixture.kage.test:8088/" --level 3 --timeout 30s
[ "${fetch_status}" -eq 0 ] || fail "kage fetch --level 3 failed: ${report}"
check "level 3: the host persona's browser, as a person" "${report}" \
    "d.outcome === 'ok' && d.attempts.length === 1 && d.attempts[0].render === 'browser' && d.attempts[0].persona_id === '${host}' && d.attempts[0].exit === null && d.behavior.positions > 0"
seen="$(got "http://fixture:8088/__kage/requests")"
check "the site saw the browser, from the same address" "${seen}" \
    "['/', '/api/items?source=fetch'].every((path) => d.some((r) => r.path === path && r.from === '${address}' && r.headers.some(([n, v]) => n.toLowerCase() === 'user-agent' && v.includes('Mozilla/'))))"
echo "    both from ${address}"

echo "==> a name the operator did not list as private is never reached"
got "http://fixture:8088/__kage/reset" > /dev/null
fetch_json "http://rules.kage.test:8088/" --level 0 --retries 0
[ "${fetch_status}" -ne 0 ] || fail "an unlisted private name was fetched: ${report}"
check "no page came back" "${report}" "d.outcome !== 'ok' && d.status === null"
seen="$(got "http://fixture:8088/__kage/requests")"
check "the site saw nothing" "${seen}" "d.length === 0"
echo "    $(value "${report}" "d.outcome + (d.reason ? ': ' + d.reason : '')")"

echo "==> nothing went through the pool, and every byte is the host's"
for proxy in proxy-a proxy-b proxy-c; do
    stats="$(got "http://${proxy}:3128/__stats")"
    check "${proxy} carried nothing" "${stats}" \
        "Object.keys(d.targets).every((t) => t === '(stats)') && d.refused_credentials === 0"
done
egress="$(psql "SELECT string_agg(DISTINCT egress, ',') || ' ' || COALESCE(SUM(bytes_up + bytes_down), 0) FROM bandwidth_usage")"
case "${egress}" in
    'host '[1-9]*) ;;
    *) fail "the counted bytes are not all the host's: ${egress}" ;;
esac
echo "    the proxies carried nothing; counted: ${egress}"

echo "direct test passed"
