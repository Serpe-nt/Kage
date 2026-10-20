#!/usr/bin/env bash
# End-to-end test of the public API (spec §16, §19, §20): the compose stack
# with real browsers and real personas on a static proxy list, in the world
# of the fetch test (deploy/compose.e2e.yaml). Nothing here needs or
# reaches the internet.
#
# It shows, through the published port and nothing else:
#   - the first admin key made at the command line, and a client's key
#     made with it through the admin API, its token shown once;
#   - a request without a key, with a wrong key and with a key that lacks
#     the scope refused with a problem document;
#   - a page of the fixture site fetched through `POST /v1/fetch` at level
#     3 in the browser of a real persona of each engine, by a TypeScript
#     client that was generated from the API's OpenAPI document
#     (tests/api-client), with the stored result and the kept document and
#     screenshot read back;
#   - the same page at level 0 (spec §25): a plain client, nobody's, from
#     the site's scout's address;
#   - a session that pins its requests to one persona and never moves;
#   - what a request may shorten of a persona's behaviour, and that the
#     answer says what applied;
#   - what cannot be asked for: no level, a target the SSRF guard refuses,
#     a header a caller may not set, a session at level 0, a script's
#     request in a browser, `render`, which 2.0 has no more;
#   - the admin endpoints: personas, the pool, a policy reload, a self-test
#     that runs in the server in the persona's own browser, a key revoked;
#   - a request no persona can serve under the reloaded policy, answered
#     as a result without an attempt;
#   - an audit row for every admin call, and the series of `/metrics`.
#
# Level 1 cannot run here: no persona of this release has an http profile
# for its browser's major (follow-up F30), and it is refused with
# `level_unavailable`. It is proven one layer down
# (crates/kage-api/tests/fetch.rs, on personas with a profile).
#
# Needs what `just setup` installs: docker, curl, and pnpm for the client.
# It uses its own compose project name and host port.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_API_PORT:-18082}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false
# The self-test of the browser path loads the coherence probe of the
# fixture site, through the persona's proxy.
export KAGE_E2E_SELFTEST_PAGES='[{id="probe",kind="probe",url="http://fixture.kage.test:8088/probe/"}]'

# The shipped policies; the test changes them later, to reload them.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
chmod --recursive a+rX "${policies}"
# What a call to the API answered.
scratch="$(mktemp -d)"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --project-name "${KAGE_E2E_API_PROJECT:-kage-e2e-api}")
base="http://127.0.0.1:${KAGE_PUBLISH_PORT}"

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- API test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 120 kage browser-chromium browser-firefox >&2 || true
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
# add_domains: appends the domain rules on stdin to the `domains` mapping
# of the copy of domains.yaml, which is the file's last key (`domains: {}`
# while the shipped file names no domain). A second `domains:` would be a
# duplicate key, which the policy loader refuses. The file is changed in
# place, never replaced, while the server reads it.
add_domains() {
    local file="${policies}/domains.yaml"
    if grep --quiet '^domains: {}$' "${file}"; then
        sed 's/^domains: {}$/domains:/' "${file}" > "${scratch}/domains.tmp"
        cat "${scratch}/domains.tmp" > "${file}"
    fi
    [ "$(grep '^[a-z_]*:' "${file}" | tail -n 1)" = 'domains:' ] \
        || fail "${file} does not end with its domains mapping"
    cat >> "${file}"
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
# document of that code, which names the request.
problem() {
    expect "$1" "$3"
    case "${headers}" in
        *'content-type: application/problem+json'*) ;;
        *) fail "$3: not a problem document: ${headers}" ;;
    esac
    check "$3" "${body}" "d.code === '$2' && d.status === $1 && d.type === 'urn:kage:problem:$2' && typeof d.request_id === 'string'"
}

echo "==> starting the stack with a world of its own"
"${compose[@]}" up --detach --wait --wait-timeout 300

echo "==> the port is published on the host's loopback only"
container="$("${compose[@]}" ps --quiet kage)"
bindings="$(docker inspect --format '{{json .HostConfig.PortBindings}}' "${container}")"
case "${bindings}" in
    *'"HostIp":"127.0.0.1"'*) ;;
    *) fail "the API is published beyond loopback: ${bindings}" ;;
esac
case "${bindings}" in
    *8899*) fail "the forwarder is published on the host: ${bindings}" ;;
esac
echo "    ${bindings}"

echo "==> liveness and the API's document need no key"
call GET /healthz
expect 200 "/healthz"
[ "${body}" = '{"status":"ok"}' ] || fail "unexpected /healthz body: ${body}"
call GET /openapi.json
expect 200 "/openapi.json"
check "the document describes the API" "${body}" "d.openapi === '3.1.0' && d.paths['/v1/fetch'].post.operationId === 'fetch'"
call GET /docs/
problem 401 missing_key "Swagger UI without a key"

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
call GET /readyz
expect 200 "/readyz"
check "ready: the database, a persona, a sidecar" "${body}" "d.status === 'ready' && d.checks.database && d.checks.persona && d.checks.sidecar"
echo "    ${body}"

echo "==> the first admin key is made at the command line; its token is shown once"
created="$(kage key create --name e2e-operator --scope admin --json)"
admin="$(value "${created}" "d.token")"
admin_id="$(value "${created}" "d.id")"
case "${admin}" in
    kage_*_*) ;;
    *) fail "not a token: $(printf '%s' "${created}" | head -c 300)" ;;
esac
listed="$(kage key list --json)"
check "the key is listed without a token" "${listed}" "d.keys.length === 1 && d.keys[0].id === '${admin_id}' && d.keys[0].token === undefined && d.keys[0].scopes.join() === 'admin'"
stored="$(psql "SELECT encode(secret_hash, 'hex') FROM api_keys")"
case "${admin}" in
    *"${stored}"*) fail "the key's secret is stored in the clear" ;;
esac
[ "${#stored}" -eq 64 ] || fail "the stored hash is not SHA-256: ${stored}"
echo "    key ${admin_id}, stored as a hash of ${#stored} hex digits"

echo "==> a client's key is made with it through the admin API"
call POST /v1/admin/keys "${admin}" '{"name":"e2e-client","scopes":["fetch","sessions"],"requests_per_minute":120,"max_concurrent":4,"daily_page_budget":50}'
expect 201 "POST /v1/admin/keys"
client="$(value "${body}" "d.token")"
client_id="$(value "${body}" "d.key.id")"
case "${headers}" in
    *'cache-control: no-store'*) ;;
    *) fail "a token may be cached: ${headers}" ;;
esac
call POST /v1/admin/keys "${admin}" '{"name":"e2e-reader","scopes":["fetch"]}'
expect 201 "POST /v1/admin/keys"
reader="$(value "${body}" "d.token")"
reader_id="$(value "${body}" "d.key.id")"
echo "    client ${client_id} (fetch, sessions), reader ${reader_id} (fetch)"

echo "==> no key, a wrong key and a key without the scope are refused"
call POST /v1/fetch '' '{"url":"http://fixture.kage.test:8088/","level":3}'
problem 401 missing_key "no key"
call POST /v1/fetch "${client}x" '{"url":"http://fixture.kage.test:8088/","level":3}'
problem 401 invalid_key "a wrong secret"
call GET /v1/admin/personas "${client}"
problem 403 scope_missing "a client at an admin endpoint"
call POST /v1/fetch "${admin}" '{"url":"http://fixture.kage.test:8088/","level":3}'
problem 403 scope_missing "admin implies no other scope"
call POST /v1/sessions "${reader}" '{}'
problem 403 scope_missing "a key without the sessions scope"
echo "    401 missing_key, 401 invalid_key, 403 scope_missing"

echo "==> a page of the fixture site through POST /v1/fetch at level 3, in the browser of a persona of each engine, by the generated TypeScript client"
summary="$(cd tests/api-client && pnpm --silent run build && KAGE_API_URL="${base}" KAGE_API_KEY="${client}" \
    KAGE_E2E_URL='http://fixture.kage.test:8088/' KAGE_E2E_EXPECT='Fixture home' \
    pnpm --silent run e2e)" || fail "the generated client failed"
check "both engines fetched the page, kept its document and a screenshot, and refused a metadata address" "${summary}" \
    "d.pages.length === 2 && d.pages[0].engine === 'chromium' && d.pages[1].engine === 'firefox' && d.pages.every((p) => p.status === 200 && p.document_bytes > 100 && p.screenshot_bytes > 1000 && /^p_[0-9a-f]{16}\$/.test(p.persona)) && d.pages[0].persona !== d.pages[1].persona && d.refused === 'ssrf_denied'"
echo "    $(value "${summary}" "d.pages.map((p) => p.engine + ': persona ' + p.persona + ', document ' + p.document_bytes + ' B, screenshot ' + p.screenshot_bytes + ' B, dwelt ' + p.dwell_ms + ' ms, ' + p.total_ms + ' ms in all').join('; ')")"
first="$(value "${summary}" "d.pages[0].id")"

echo "==> what one key fetched, another key cannot read; an admin can, and sees the persona as it is"
call GET "/v1/results/${first}" "${reader}"
problem 404 result_not_found "another key's result"
call GET "/v1/results/${first}" "${client}"
expect 200 "the client's own result"
check "no proxy and no persona id in a client's result" "${body}" \
    "d.level === 3 && /^p_/.test(d.persona.id) && d.attempts.every((a) => /^p_/.test(a.persona) && a.exit === null && a.seed === undefined) && !JSON.stringify(d).includes('proxy')"
artifact="$(value "${body}" "d.artifacts.dom_html")"
call GET "/v1/artifacts/${artifact}" "${reader}"
problem 404 artifact_not_found "another key's artifact"
call GET "/v1/artifacts/${artifact}" "${client}"
expect 200 "the client's own artifact"
case "${headers}" in
    *'x-content-type-options: nosniff'*'content-security-policy: sandbox'* | *'content-security-policy: sandbox'*'x-content-type-options: nosniff'*) ;;
    *) fail "an artifact is served without its guards: ${headers}" ;;
esac
call POST /v1/admin/keys "${admin}" '{"name":"e2e-auditor","scopes":["fetch","admin"]}'
expect 201 "POST /v1/admin/keys"
auditor="$(value "${body}" "d.token")"
call GET "/v1/results/${first}" "${auditor}"
expect 200 "an admin reads any result"
real="$(value "${body}" "d.persona.id")"
check "the admin sees the persona's id and the attempt's seed" "${personas}" "d.personas.some((p) => p.id === '${real}')"
check "the attempt's seed is an admin's to see" "${body}" "typeof d.attempts[0].seed === 'string'"
echo "    result ${first}: a client sees $(value "${summary}" "d.pages[0].persona"), an admin ${real}"

# Asked now, while two of the three personas have read the site: the third
# is the one left to be its scout, whose address is the site's plain exit.
echo "==> the same page at level 0: a plain client, nobody's, from the site's scout's address"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/","level":0}'
expect 200 "a fetch at level 0"
check "level 0: no persona, one attempt from the pool's plain exit, the page as it was sent" "${body}" \
    "d.outcome === 'ok' && d.status === 200 && d.level === 0 && d.persona === null && d.attempts.length === 1 && d.attempts[0].render === 'http' && d.attempts[0].exit === 'pool' && d.attempts[0].persona === null && d.attempts[0].engine === null && typeof d.raw_body === 'string' && d.raw_body.includes('Fixture home') && !JSON.stringify(d).includes('proxy')"
echo "    $(value "${body}" "'level ' + d.level + ': ' + d.outcome + ' ' + d.status + ', from the ' + d.attempts[0].exit + ' exit, ' + d.timing.total_ms + ' ms'")"

echo "==> a session pins its requests to one persona and never moves"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/landing","level":3,"session_id":"visit-1","timeout_ms":30000}'
problem 404 session_not_found "a fetch does not open a session"
call POST /v1/sessions "${client}" '{"id":"visit-1","persona":{"engines":["firefox"]},"domain":"fixture.kage.test"}'
expect 201 "a new session"
pinned="$(value "${body}" "d.persona.id")"
check "pinned to a firefox persona" "${body}" "d.id === 'visit-1' && d.persona.engine === 'firefox'"
call POST /v1/sessions "${client}" '{"id":"visit-1"}'
expect 200 "the session is open already"
for page in /landing /; do
    call POST /v1/fetch "${client}" "{\"url\":\"http://fixture.kage.test:8088${page}\",\"level\":3,\"session_id\":\"visit-1\",\"timeout_ms\":30000}"
    expect 200 "a fetch in the session"
    check "the session's persona fetched ${page}" "${body}" "d.outcome === 'ok' && d.session_id === 'visit-1' && d.persona.id === '${pinned}' && d.attempts.length === 1"
done
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/landing","level":3,"session_id":"visit-1","persona":{"engines":["chromium"]},"timeout_ms":30000}'
problem 409 session_persona_conflict "a constraint the session's persona fails"
check "the conflict names the constraint and what the persona is" "${body}" "d.constraint === 'request.persona.engines' && d.engine === 'firefox' && d.persona === '${pinned}'"
call GET /v1/sessions/visit-1 "${client}"
expect 200 "the session is still there"
check "and has not moved" "${body}" "d.persona.id === '${pinned}'"
call DELETE /v1/sessions/visit-1 "${client}"
expect 204 "the session is ended"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/landing","level":3,"session_id":"visit-1","timeout_ms":30000}'
problem 404 session_not_found "a session that ended is not reopened by a fetch"
echo "    two pages by ${pinned}; a chromium constraint is a 409; ended; 404 afterwards"

echo "==> a request may shorten what the persona does on a page, and the answer says what applied"
call POST /v1/fetch "${client}" '{"url":"http://lenient.kage.test:8088/landing","level":3,"include":["dom"],"behavior":{"scroll_depth":0,"dwell_ms":1000,"referer_strategy":"none"},"timeout_ms":30000}'
expect 200 "a fetch with its own behaviour"
check "the request's behaviour applied, no lower than the policy's floor" "${body}" \
    "d.outcome === 'ok' && d.level === 3 && d.behavior.applied === 'request' && d.behavior.request_ignored === false && d.behavior.scroll_depth === 0 && d.behavior.notches === 0 && d.behavior.referer_strategy === 'none' && d.behavior.dwell_ms >= 1500 && d.pacing.applied === 'human'"
quick="$(value "${body}" "'dwelt ' + d.behavior.dwell_ms + ' ms, ' + d.timing.total_ms + ' ms in all'")"
call POST /v1/fetch "${client}" '{"url":"http://lenient.kage.test:8088/landing","level":3,"timeout_ms":5000}'
problem 422 timeout_too_short "a budget too short for a visit is refused, not clamped"
call POST /v1/fetch "${client}" '{"url":"http://lenient.kage.test:8088/landing","level":2,"behavior":{"scroll_depth":0}}'
problem 422 validation_failed "behaviour at level 2, a visit that does nothing on the page"
check "the refusal names the field" "${body}" "d.field === 'behavior'"
echo "    no scrolling, ${quick}; a budget of 5 s is refused with timeout_too_short, behaviour at level 2 with validation_failed"

echo "==> what cannot be asked for is refused before anything is fetched"
before="$(psql "SELECT count(*) FROM requests")"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/"}'
problem 422 level_required "a request that names no level"
check "the refusal names the field" "${body}" "d.field === 'level'"
call POST /v1/fetch "${client}" '{"url":"http://127.0.0.1:8080/v1/admin/keys","level":3}'
problem 422 ssrf_denied "the API's own address as a target"
call POST /v1/fetch "${client}" '{"url":"http://postgres:5432/","level":0}'
problem 422 ssrf_denied "a name of the compose network, at level 0 too"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/","level":0,"headers":{"X-Forwarded-For":"203.0.113.7"}}'
problem 422 header_denied "a forwarding header, at level 0 too"
check "the refusal names the header" "${body}" "d.header === 'X-Forwarded-For'"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/","level":0,"session_id":"visit-1"}'
problem 422 session_needs_persona "a session at level 0, which has no persona"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/","level":3,"request_kind":"fetch"}'
problem 422 level_kind_conflict "a script's request in a browser"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/","level":3,"render":"browser"}'
problem 422 validation_failed "render, which 2.0 has no more"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/","level":3,"locale":"de-DE"}'
problem 422 validation_failed "a field the API does not take"
echo "    level_required, ssrf_denied twice, header_denied, session_needs_persona, level_kind_conflict, validation_failed twice"

echo "==> level 1 is refused before anything is fetched: no persona of this release has an http profile"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/landing","level":1}'
problem 422 level_unavailable "level 1 while no browser major has a verified http profile"
after="$(psql "SELECT count(*) FROM requests")"
[ "${before}" = "${after}" ] || fail "a refused request was stored: ${before} before, ${after} after"
echo "    level_unavailable; nothing that was refused was fetched or stored"

echo "==> the admin endpoints: personas, proxies, the pool, the policies"
call GET /v1/admin/personas "${admin}"
expect 200 "GET /v1/admin/personas"
check "three personas, as they are" "${body}" "d.personas.length === 3 && d.personas.every((p) => p.ready && typeof p.user_agent === 'string' && p.fingerprint === undefined)"
target="$(value "${body}" "d.personas.find((p) => p.engine === 'chromium').id")"
call GET "/v1/admin/personas/${target}?fingerprint=true" "${admin}"
expect 200 "GET /v1/admin/personas/{id}"
check "one persona with its fingerprint" "${body}" "d.id === '${target}' && typeof d.fingerprint === 'object'"
call GET /v1/admin/proxies "${admin}"
expect 200 "GET /v1/admin/proxies"
check "three proxies, without what they are logged in with" "${body}" "d.proxies.length === 3 && !JSON.stringify(d).includes('e2e-only') && !JSON.stringify(d).includes('provider-a')"
proxy="$(value "${body}" "d.proxies[0].id")"
call POST "/v1/admin/proxies/${proxy}/health" "${admin}"
expect 200 "POST /v1/admin/proxies/{id}/health"
check "probed now, through the proxy" "${body}" "d.id === '${proxy}' && d.state === 'healthy' && d.consecutive_failures === 0"
call GET /v1/admin/pool "${admin}"
expect 200 "GET /v1/admin/pool"
check "the pool at a glance" "${body}" "d.personas.live === 3 && d.personas.ready === 3 && d.bandwidth !== null"
echo "    $(value "${body}" "'personas: ' + JSON.stringify(d.personas)")"
add_domains << 'EOF'
  # Added by tests/e2e/api.sh.
  reloaded.kage.test:
    min_level: 2
    min_dwell: 3s
  rules.kage.test:
    countries: [JP]
EOF
call POST /v1/admin/policies/reload "${admin}"
expect 200 "POST /v1/admin/policies/reload"
check "the policies were read again" "${body}" "d.domains >= 1 && d.pacing_profiles >= 2 && d.signature_rules > 0"
call GET /v1/policies/domains/www.reloaded.kage.test "${client}"
expect 200 "GET /v1/policies/domains/{domain}"
check "the new rule is in force" "${body}" "d.min_level === 2 && d.policy.min_level === 2 && d.policy.min_dwell === '3s'"
echo "    reloaded: $(value "${body}" "JSON.stringify(d.policy.matched) + ' takes level ' + d.min_level + ' and above, min_dwell ' + d.policy.min_dwell")"

echo "==> a request no persona can serve is answered as a result: the site takes personas from JP, and every one of this pool is in DE"
call POST /v1/fetch "${client}" '{"url":"http://rules.kage.test:8088/landing","level":3}'
expect 200 "a fetch that found no persona"
check "provider_error with its reason, without an attempt" "${body}" "d.outcome === 'provider_error' && d.reason === 'no_persona' && d.attempts.length === 0"
echo "    $(value "${body}" "d.outcome + ': ' + d.reason")"

echo "==> a self-test runs in the server, in the persona's own browser, while real work has it open"
call POST /v1/fetch "${client}" '{"url":"http://fixture.kage.test:8088/landing","level":3,"persona":{"engines":["chromium"]},"timeout_ms":30000}'
expect 200 "a fetch before the self-test"
call POST "/v1/admin/personas/${target}/selftest" "${admin}"
expect 200 "POST /v1/admin/personas/{id}/selftest"
check "the browser path was tested and a run is stored; the http path has nothing to test" "${body}" \
    "['passed', 'failed', 'inconclusive'].includes(d.browser.verdict) && typeof d.browser.run === 'string' && typeof d.browser.score === 'number' && d.http.verdict === 'skipped' && d.persona.id === '${target}'"
runs="$(psql "SELECT count(*) FROM selftest_runs WHERE persona_id = '${target}' AND render = 'browser'")"
[ "${runs}" -eq 1 ] || fail "${runs} self-test runs are stored for the persona, expected 1"
conflicts="$("${compose[@]}" logs --no-color browser-chromium 2> /dev/null | grep --count 'context_conflict' || true)"
[ "${conflicts}" -eq 0 ] || fail "the self-test met context_conflict ${conflicts} times"
echo "    $(value "${body}" "'browser: ' + d.browser.verdict + ' with a score of ' + d.browser.score + '; http: ' + d.http.verdict + '; the persona is ' + d.persona.state")"

echo "==> a key is rotated and revoked: the old token is dead at once"
call POST "/v1/admin/keys/${reader_id}/rotate" "${admin}"
expect 200 "POST /v1/admin/keys/{id}/rotate"
rotated="$(value "${body}" "d.token")"
call GET /v1/policies/domains/example.org "${reader}"
problem 401 invalid_key "the token before the rotation"
call GET /v1/policies/domains/example.org "${rotated}"
expect 200 "the token after the rotation"
call DELETE "/v1/admin/keys/${reader_id}" "${admin}"
expect 200 "DELETE /v1/admin/keys/{id}"
call GET /v1/policies/domains/example.org "${rotated}"
problem 401 key_disabled "a revoked key"
revoked="$(kage key revoke "${client_id}" --json)"
check "revoked at the command line too" "${revoked}" "d.id === '${client_id}' && d.active === false"
call GET /v1/policies/domains/example.org "${client}"
problem 401 key_disabled "a key revoked at the command line"
echo "    rotated, revoked through the API; the client's key revoked with kage key revoke"

echo "==> every admin call left an audit row, with who, what and how it ended"
call GET '/v1/admin/audit?limit=200' "${admin}"
expect 200 "GET /v1/admin/audit"
trail="${body}"
for action in admin.key.create admin.persona.list admin.persona.show admin.proxy.list admin.proxy.health \
    admin.pool.show admin.policy.reload admin.persona.selftest admin.key.rotate admin.key.revoke; do
    check "an audit row for ${action}" "${trail}" \
        "d.events.some((e) => e.action === '${action}' && e.outcome === 'success' && e.actor === 'api_key' && e.api_key_id === '${admin_id}' && typeof e.details.request_id === 'string')"
done
check "the refused admin call is there as denied" "${trail}" \
    "d.events.some((e) => e.action === 'admin.persona.list' && e.outcome === 'denied' && e.api_key_id === '${client_id}')"
check "what the command line did is there too" "${trail}" \
    "d.events.some((e) => e.action === 'key.create' && e.actor === 'cli') && d.events.some((e) => e.action === 'key.revoke' && e.actor === 'cli' && e.target === '${client_id}')"
check "no token and no hash is in the trail" "${trail}" \
    "!JSON.stringify(d).includes('${admin#kage_*_}') && !JSON.stringify(d).includes('${stored}')"
rows="$(psql "SELECT count(*) FROM audit_events WHERE api_key_id = '${admin_id}' AND action LIKE 'admin.%'")"
[ "${rows}" -ge 12 ] || fail "${rows} audit rows of the admin key are stored, expected at least 12"
by_action="$(psql "SELECT action || '=' || count(*) FROM audit_events GROUP BY action ORDER BY action" | tr '\n' ' ')"
echo "    ${rows} rows of the operator's key; by action: ${by_action}"

echo "==> /metrics: the series of the specification, for a key with the admin scope"
call GET /metrics "${auditor}"
expect 200 "GET /metrics"
# Kept in a file: `grep --quiet` on a pipe ends the writer with SIGPIPE at
# its first match, which `pipefail` would report as a failed check.
metrics="${scratch}/metrics"
printf '%s\n' "${body}" > "${metrics}"
for series in kage_requests_total kage_attempts_total kage_attempt_duration_seconds_bucket kage_persona_state \
    kage_proxy_state kage_scheduler_wait_seconds_bucket kage_queue_depth kage_scheduler_in_flight \
    kage_scheduler_refusals_total kage_bandwidth_bytes_total kage_bandwidth_budget_ratio kage_selftest_score \
    kage_api_requests_total kage_api_request_duration_seconds_bucket kage_api_refusals_total \
    sidecar_contexts_open sidecar_navigation_duration_seconds_bucket; do
    grep --quiet "^${series}[{ ]" "${metrics}" || fail "no sample of ${series} in /metrics"
done
# One line with all three labels, in whatever order the exporter writes them.
for level in 3 0; do
    awk -v level="level=\"${level}\"" '/^kage_requests_total\{/ && /domain="fixture.kage.test"/ && /outcome="ok"/ && index($0, level) { found = 1 } END { exit !found }' "${metrics}" \
        || fail "the fetches of the fixture site at level ${level} are not counted: $(grep '^kage_requests_total' "${metrics}" | head -8)"
done
if grep --quiet --extended-regexp '^kage_(requests|attempts)_total\{.*render=' "${metrics}"; then
    fail "a fetch is counted by its render, not its level: $(grep '^kage_requests_total' "${metrics}" | head -5)"
fi
awk '/^kage_bandwidth_bytes_total\{/ && /direction="down"/ && /egress="pool"/ { found = 1 } END { exit !found }' "${metrics}" \
    || fail "the bytes through the pool are not counted by their exit: $(grep '^kage_bandwidth_bytes_total' "${metrics}" | head -5)"
grep --quiet '^sidecar_contexts_open{.*engine="firefox"' "${metrics}" || fail "the firefox sidecar's series are missing"
families="$(grep --count '^# TYPE ' "${metrics}")"
call GET /metrics
problem 401 missing_key "/metrics without a key"
echo "    ${families} families, among them every one of spec §20 that this run could move"

echo "API test passed"
