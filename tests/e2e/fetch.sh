#!/usr/bin/env bash
# End-to-end test of the fetch pipeline (spec §3.2, §8, §11, §12): the
# compose stack with real browsers, real personas on a static proxy list,
# and a world of its own to fetch from (deploy/compose.e2e.yaml). Nothing
# here needs or reaches the internet.
#
# It shows, with `kage fetch` as the one way in:
#   - a page of the fixture site fetched in a persona's browser on both
#     engines, with the behavior of a person asked for and performed, at
#     the pace of the domain's policy, through the forwarder and the
#     persona's own proxy;
#   - `render: http` refused for personas without an http profile, and
#     `render: auto` going to the browser where the policy says http;
#   - a challenge that resolves by itself waited for; one that does not is
#     a block: the persona cools, another one tries, and the domain's
#     breaker opens and answers the next request at once;
#   - every byte counted: what Kage wrote to the store is what the proxies
#     carried for it.
# It also measures what the forwarder adds to a connection.
#
# The http path and the cookie handoff cannot run here: no persona of this
# release has an http profile for its browser's major. They are proven one
# layer down (crates/kage-scheduler/tests/paths.rs).
#
# It uses its own compose project name and host port.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_FETCH_PORT:-18081}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false

# The shipped policies with the domain rules of this test.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
cat >> "${policies}/domains.yaml" << 'EOF'

# Added by tests/e2e/fetch.sh.
domains:
  # Known lenient: the http path first.
  lenient.kage.test:
    render: http
  # Two blocks pause the site.
  fixture.kage.test:
    breaker_threshold: 2
    breaker_pause: 30m
EOF
# A `domains: {}` of the shipped file would be a second key of that name.
if [ "$(grep --count '^domains:' "${policies}/domains.yaml")" -ne 1 ]; then
    grep --invert-match '^domains: {}$' "${policies}/domains.yaml" > "${policies}/domains.tmp"
    mv "${policies}/domains.tmp" "${policies}/domains.yaml"
fi
chmod --recursive a+rX "${policies}"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --project-name "${KAGE_E2E_FETCH_PROJECT:-kage-e2e-fetch}")

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- fetch test failed: service state and logs ---" >&2
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
# fetch_json <arguments>: the JSON report of `kage fetch`, whether or not
# the page was fetched; the exit status is left in ${fetch_status}.
fetch_json() {
    fetch_status=0
    report="$(kage fetch "$@" --json 2> /dev/null)" || fetch_status=$?
}
psql() {
    "${compose[@]}" exec -T postgres psql --username kage --dbname kage \
        --no-align --tuples-only --command "$1"
}

echo "==> starting the stack with a world of its own"
"${compose[@]}" up --detach --wait --wait-timeout 300

echo "==> the pool adopts the static list, the probes pass, and a persona is generated for every proxy"
personas=''
for _ in $(seq 1 60); do
    personas="$(kage persona list --json 2> /dev/null || true)"
    if [ -n "${personas}" ] && [ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 3 ]; then
        break
    fi
    sleep 3
done
check "three ready personas" "${personas}" "d.personas.length === 3 && d.personas.every((p) => p.ready && p.state === 'active')"
check "both engines have a persona" "${personas}" "['chromium', 'firefox'].every((e) => d.personas.some((p) => p.engine === e))"
check "no persona has an http profile in this release" "${personas}" "d.personas.every((p) => p.wreq_profile === null)"
echo "    $(value "${personas}" "d.personas.map((p) => p.engine + ' ' + p.browser_major + '/' + p.claimed_os).join(', ')")"

for engine in chromium firefox; do
    echo "==> a page of the fixture site, in the browser of a ${engine} persona"
    got "http://fixture:8088/__kage/reset" > /dev/null
    fetch_json "http://fixture.kage.test:8088/" --engine "${engine}" --timeout 30s
    [ "${fetch_status}" -eq 0 ] || fail "kage fetch failed for ${engine}: ${report}"
    check "the page was fetched in the browser" "${report}" \
        "d.outcome === 'ok' && d.status === 200 && d.attempts.length === 1 && d.attempts[0].render === 'browser' && d.attempts[0].engine === '${engine}' && d.title === 'Fixture home'"
    # The behavior of a person was asked for with the attempt's own seed,
    # and the browser reports having performed it. The dwell is the
    # persona's own draw, never below the hand's 600 ms (a request's
    # `dwell_ms` is what `min_dwell` bounds); 1000 failed on GitHub for a
    # draw of 932 ms.
    check "human behavior, from the attempt's seed" "${report}" \
        "d.behavior !== null && d.behavior.seed === d.attempts[0].seed && d.behavior.dwell_ms >= 600 && d.behavior.entry.strategy === 'direct' && d.behavior.positions > 0"
    check "the policy's pacing applied" "${report}" "d.pacing.applied === 'human' && d.pacing.request_ignored === false"
    check "bytes were relayed and counted" "${report}" "d.relayed_bytes.down > d.document_bytes && d.relayed_bytes.up > 0"
    # The site saw a browser: the document, its script, and what the
    # script loaded, all with the persona's User-Agent.
    seen="$(got "http://fixture:8088/__kage/requests")"
    case "${engine}" in
        chromium) agent='Chrome/' ;;
        firefox) agent='Firefox/' ;;
    esac
    check "the site saw the ${engine} persona load the page and what it loads" "${seen}" \
        "['/', '/app.js', '/api/items?source=fetch'].every((path) => d.some((r) => r.path === path && r.host === 'fixture.kage.test:8088' && r.headers.some(([name, value]) => name.toLowerCase() === 'user-agent' && value.includes('${agent}'))))"
    echo "    $(value "${report}" "'dwelt ' + d.behavior.dwell_ms + ' ms, ' + d.behavior.moves + ' moves, document ' + d.document_bytes + ' B, relayed ' + d.relayed_bytes.up + ' B up and ' + d.relayed_bytes.down + ' B down'")"
done

echo "==> render http is refused: no persona has an http path"
fetch_json "http://fixture.kage.test:8088/landing" --render http
[ "${fetch_status}" -ne 0 ] || fail "render http was not refused: ${report}"
check "refused with its reason, without an attempt" "${report}" \
    "d.outcome === 'provider_error' && d.reason === 'no_persona' && d.attempts.length === 0"
echo "    $(value "${report}" "d.outcome + ': ' + d.reason")"

echo "==> render auto goes to the browser where the policy says http"
fetch_json "http://lenient.kage.test:8088/landing" --timeout 30s
[ "${fetch_status}" -eq 0 ] || fail "kage fetch failed: ${report}"
check "one attempt, in the browser, not an escalation" "${report}" \
    "d.outcome === 'ok' && d.attempts.length === 1 && d.attempts[0].render === 'browser' && d.escalated === false && d.title === 'Landing'"
echo "    $(value "${report}" "d.attempts[0].render + ' by ' + d.attempts[0].engine")"

echo "==> a challenge that resolves by itself is waited for"
fetch_json "http://fixture.kage.test:8088/challenge" --timeout 40s
[ "${fetch_status}" -eq 0 ] || fail "the challenge was not passed: ${report}"
check "challenge_passed, with the page behind it" "${report}" \
    "d.outcome === 'challenge_passed' && d.attempts.length === 1 && d.challenge.interstitial === true && d.challenge.resolved === true && d.title === 'Behind the challenge'"
echo "    $(value "${report}" "d.outcome + ' after ' + d.challenge.waited_ms + ' ms'")"

echo "==> a challenge that does not resolve is a block: the persona cools, another one tries, the breaker opens"
fetch_json "http://fixture.kage.test:8088/challenge/stuck" --timeout 45s --retries 1
[ "${fetch_status}" -ne 0 ] || fail "a stuck challenge was reported as fetched: ${report}"
check "two attempts by two personas, both blocked" "${report}" \
    "d.outcome === 'challenge_failed' && d.attempts.length === 2 && d.attempts.every((a) => a.outcome === 'challenge_failed' && a.block !== null) && d.attempts[0].persona_id !== d.attempts[1].persona_id && d.block.vendor === 'cloudflare'"
cooling="$(psql "SELECT count(*) FROM persona_domain_state WHERE domain = 'fixture.kage.test' AND cooldown_until > now() + interval '20 hours'")"
[ "${cooling}" -eq 2 ] || fail "${cooling} personas cool for the site, expected 2"
breaker="$(psql "SELECT state || ' ' || trips || ' ' || (paused_until > now() + interval '25 minutes') FROM domain_breakers WHERE domain = 'fixture.kage.test'")"
[ "${breaker}" = 'open 1 true' ] || fail "the breaker of the site is: ${breaker}"
fetch_json "http://fixture.kage.test:8088/"
[ "${fetch_status}" -ne 0 ] || fail "a paused domain was fetched: ${report}"
check "the paused domain is answered at once" "${report}" \
    "d.outcome === 'provider_error' && d.reason === 'domain_paused' && d.attempts.length === 0 && d.retry_at !== null"
echo "    two personas cool, the breaker is ${breaker%% *}, the next request: $(value "${report}" "d.reason")"
alerted="$(psql "SELECT count(*) FROM health_events WHERE kind = 'domain_paused'")"
[ "${alerted}" -eq 1 ] || fail "${alerted} domain_paused events are stored, expected 1"

echo "==> every byte is counted: what Kage wrote to the store is what the proxies carried for it"
sleep 2
counted="$(psql "SELECT COALESCE(SUM(bytes_up), 0) || ' ' || COALESCE(SUM(bytes_down), 0) FROM bandwidth_usage")"
carried_up=0
carried_down=0
for proxy in proxy-a proxy-b proxy-c; do
    stats="$(got "http://${proxy}:3128/__stats")"
    check "${proxy} refused no credential: the forwarder added the provider's" "${stats}" "d.refused_credentials === 0"
    # Everything but the health probes, which do not pass the forwarder,
    # and this very question.
    up="$(value "${stats}" "Object.entries(d.targets).filter(([t]) => t !== 'world.kage.test:8089' && t !== '(stats)').reduce((sum, [, e]) => sum + e.bytes_in, 0)")"
    down="$(value "${stats}" "Object.entries(d.targets).filter(([t]) => t !== 'world.kage.test:8089' && t !== '(stats)').reduce((sum, [, e]) => sum + e.bytes_out, 0)")"
    carried_up=$((carried_up + up))
    carried_down=$((carried_down + down))
done
echo "    kage counted (up down): ${counted}; the proxies carried: ${carried_up} ${carried_down}"
[ "${counted}" = "${carried_up} ${carried_down}" ] || fail "the counts differ"
by_domain="$(psql "SELECT domain || '=' || SUM(bytes_up + bytes_down) FROM bandwidth_usage GROUP BY domain ORDER BY domain" | tr '\n' ' ')"
echo "    by domain: ${by_domain}"
case "${by_domain}" in
    *'fixture.kage.test='*'lenient.kage.test='*) ;;
    *) fail "the bytes are not counted toward the sites that were fetched: ${by_domain}" ;;
esac

echo "==> a browser's container reaches neither the fixture site nor a proxy itself, only kage"
for target in http://fixture:8088/__kage/requests http://proxy-a:3128/__stats http://world:8089/ip; do
    reached="$("${compose[@]}" exec -T browser-chromium node -e \
        "fetch('${target}').then(() => console.log('reached'), () => console.log('unreachable'))")"
    [ "${reached}" = unreachable ] || fail "browser-chromium reached ${target} by itself"
done
echo "    the fixture site, proxy-a and the world echo are out of a sidecar's reach"

echo "==> what the forwarder adds to a connection across the compose bridge"
# shellcheck disable=SC1091
. deploy/.env
persona="$(value "${personas}" "d.personas[0].id")"
latency="$("${compose[@]}" exec -T \
    --env "MASTER_KEY=${KAGE_MASTER_KEY}" --env "PERSONA=${persona}" \
    --env FORWARDER=kage:8899 --env PROXY=proxy-a:3128 \
    --env PROXY_USER=provider-a --env PROXY_PASS=e2e-only-a \
    --env TARGET=http://world.kage.test:8089/ip --env SAMPLES=40 \
    world node --input-type=module - < tests/e2e/latency.mjs)" \
    || fail "the latency measurement failed"
echo "    ${latency}"
check "the forwarder adds little" "${latency}" "d.added_median_ms < 50"

echo "==> the server's forwarder is reachable on the compose network only"
container="$("${compose[@]}" ps --quiet kage)"
bindings="$(docker inspect --format '{{json .HostConfig.PortBindings}}' "${container}")"
case "${bindings}" in
    *8899*) fail "the forwarder is published on the host: ${bindings}" ;;
esac
echo "    ${bindings}"

echo "fetch test passed"
