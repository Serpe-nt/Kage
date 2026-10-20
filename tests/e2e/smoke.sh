#!/usr/bin/env bash
# Stack smoke test: brings the compose project up and checks that kage serves
# /healthz, both sidecars serve /v1/health behind the shared secret, each
# sidecar runs a real browser hardened as spec §19 and §22 ask, Postgres
# accepts connections, the migrations are applied, and the configuration
# loads with secrets redacted.
#
# It uses its own compose project name and host port, so it never touches a
# stack started with `just dev` or `docker compose up`.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_PORT:-18080}"
# The smoke test never talks to a proxy provider or to proxies, whatever
# deploy/.env holds: the shell wins over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
# And no persona of a test ever visits a real site.
export KAGE_PERSONAS_WARMUP=false
compose=(docker compose --file deploy/compose.yaml --project-name "${KAGE_E2E_PROJECT:-kage-e2e}")

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- smoke test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 200 >&2 || true
    fi
    "${compose[@]}" down --volumes --remove-orphans > /dev/null 2>&1 || true
    exit "${status}"
}
trap cleanup EXIT

fail() {
    echo "FAIL: $*" >&2
    exit 1
}

echo "==> starting the stack and waiting for every healthcheck"
"${compose[@]}" up --detach --wait --wait-timeout 240

echo "==> kage serves /healthz on the published port"
body="$(curl --fail --silent --show-error --max-time 5 "http://127.0.0.1:${KAGE_PUBLISH_PORT}/healthz")"
[ "${body}" = '{"status":"ok"}' ] || fail "unexpected /healthz body: ${body}"
echo "    ${body}"

for engine in chromium firefox; do
    echo "==> browser-${engine} serves /v1/health and reports its engine"
    health="$("${compose[@]}" exec -T "browser-${engine}" node dist/healthcheck.js)"
    case "${health}" in
        *"\"status\":\"ok\""*"\"engine\":\"${engine}\""*) echo "    ${health}" ;;
        *) fail "browser-${engine} reported: ${health}" ;;
    esac
done

echo "==> sidecars are reachable by service name on the compose network, with the shared secret only"
# Both sidecars hold the same secret, so one can stand in for kage here.
reach() {
    "${compose[@]}" exec -T browser-firefox node -e \
        "fetch('http://browser-chromium:8090/v1/health', { headers: $1 }).then((r) => console.log(r.status), () => console.log('unreachable'))"
}
with_token="$(reach "{ 'x-kage-sidecar-token': process.env.SIDECAR_TOKEN }")"
[ "${with_token}" = 200 ] || fail "browser-chromium answered ${with_token} to a call with the token"
without_token="$(reach "{}")"
[ "${without_token}" = 401 ] || fail "browser-chromium answered ${without_token} to a call without the token"
echo "    with the token: ${with_token}, without: ${without_token}"

echo "==> a sidecar reaches kage and nothing else: its network has no way out, Postgres is not on it"
internal="$(docker network inspect --format '{{.Internal}}' "${KAGE_E2E_PROJECT:-kage-e2e}_browsers")"
[ "${internal}" = true ] || fail "the browsers network is not internal: ${internal}"
lookup() {
    "${compose[@]}" exec -T browser-firefox node -e \
        "require('node:dns').promises.lookup('$1').then(() => console.log('known'), () => console.log('unknown'))"
}
[ "$(lookup kage)" = known ] || fail "a sidecar cannot find kage"
[ "$(lookup postgres)" = unknown ] || fail "a sidecar can find Postgres"
echo "    browsers is internal; kage is known there, Postgres is not"

# What spec §19 asks of a sidecar's container, on one line: the user, a
# read-only root, every capability dropped and none added, not privileged.
hardening_of() {
    docker inspect --format \
        '{{.Config.User}} {{.HostConfig.ReadonlyRootfs}} {{json .HostConfig.CapDrop}} {{json .HostConfig.CapAdd}} {{.HostConfig.Privileged}}' \
        "$1"
}
# The security options of a container, each up to its `=`. A seccomp entry
# holds the whole profile after that; its name is all that is looked at.
security_of() {
    docker inspect --format \
        '{{range $i, $option := .HostConfig.SecurityOpt}}{{if $i}} {{end}}{{index (split $option "=") 0}}{{end}}' \
        "$1"
}

echo "==> browser-chromium is hardened and runs Chrome inside Chrome's own sandbox"
container="$("${compose[@]}" ps --quiet browser-chromium)"
hardening="$(hardening_of "${container}")"
[ "${hardening}" = '65532:65532 true ["ALL"] null false' ] \
    || fail "browser-chromium container settings: ${hardening}"
security="$(security_of "${container}")"
case "${security}" in
    'no-new-privileges:true seccomp' | 'seccomp no-new-privileges:true') ;;
    *) fail "browser-chromium security options: ${security}" ;;
esac
echo "    user 65532, read-only root, no capabilities, no-new-privileges, the derived seccomp profile"
sandbox="$("${compose[@]}" exec -T browser-chromium node --input-type=module - < tests/e2e/chromium-hardening.mjs)" \
    || fail "the Chromium hardening check failed"
echo "    ${sandbox}"

echo "==> browser-firefox is hardened and runs Camoufox under Docker's default seccomp profile"
container="$("${compose[@]}" ps --quiet browser-firefox)"
hardening="$(hardening_of "${container}")"
[ "${hardening}" = '65532:65532 true ["ALL"] null false' ] \
    || fail "browser-firefox container settings: ${hardening}"
# no-new-privileges and nothing else: any seccomp entry would replace
# Docker's default profile, and the firefox sidecar keeps the default.
security="$(security_of "${container}")"
[ "${security}" = 'no-new-privileges:true' ] \
    || fail "browser-firefox security options: ${security}"
echo "    user 65532, read-only root, no capabilities, no-new-privileges, Docker's default seccomp profile"
sandbox="$("${compose[@]}" exec -T browser-firefox node --input-type=module - < tests/e2e/firefox-hardening.mjs)" \
    || fail "the Firefox hardening check failed"
echo "    ${sandbox}"

echo "==> postgres accepts connections"
"${compose[@]}" exec -T postgres pg_isready --username kage --dbname kage

psql() {
    "${compose[@]}" exec -T postgres psql --username kage --dbname kage \
        --no-align --tuples-only --command "$1"
}

echo "==> the migrate service applied every migration before kage started"
expected_migrations="$(find crates/kage-store/migrations -maxdepth 1 -name '*.sql' | wc -l)"
applied_migrations="$(psql 'SELECT count(*) FROM _sqlx_migrations WHERE success')"
[ "${applied_migrations}" -eq "${expected_migrations}" ] \
    || fail "${applied_migrations} of ${expected_migrations} migrations are applied"
tables="$(psql "SELECT count(*) FROM information_schema.tables WHERE table_schema = 'public' AND table_name <> '_sqlx_migrations'")"
[ "${tables}" -eq 24 ] || fail "expected the 24 tables of spec §18, found ${tables}"
echo "    ${applied_migrations} migrations, ${tables} tables"

echo "==> kage migrate --check agrees, and applying again changes nothing"
"${compose[@]}" exec -T kage /usr/local/bin/kage migrate --check > /dev/null
"${compose[@]}" exec -T kage /usr/local/bin/kage migrate > /dev/null
[ "$(psql 'SELECT count(*) FROM _sqlx_migrations')" -eq "${expected_migrations}" ] \
    || fail "a second 'kage migrate' changed the migration table"

echo "==> kage config check prints the effective configuration without secrets"
config="$("${compose[@]}" exec -T kage /usr/local/bin/kage config check)"
# shellcheck disable=SC1091
. deploy/.env
for secret in "${KAGE_POSTGRES_PASSWORD}" "${KAGE_SIDECAR_TOKEN}" "${KAGE_MASTER_KEY}"; do
    case "${config}" in
        *"${secret}"*) fail "kage config check printed a secret" ;;
    esac
done
case "${config}" in
    *'"url": "[redacted]"'*'"dir": "/etc/kage/policies"'*) ;;
    *) fail "unexpected kage config check output: ${config}" ;;
esac
echo "    secrets redacted, policies loaded from /etc/kage/policies"

echo "==> the persona manager has the shipped mix, sees the personas volume and tests nothing by itself"
# A sidecar's node reads the document, so the host needs no JSON tool.
personas="$(printf '%s' "${config}" | "${compose[@]}" exec -T browser-chromium node -e \
    "let text = ''; process.stdin.on('data', (chunk) => (text += chunk)).on('end', () => { const report = JSON.parse(text); const mix = report.personas.mix.map((entry) => entry.engine + '/' + entry.os + ' ' + entry.share).join(', '); console.log([mix, report.config.personas.dir, report.config.personas.warmup.enabled, report.personas.scheduled_selftests.http, report.personas.scheduled_selftests.browser].join(' | ')); });")"
expected='chromium/linux 0.4, firefox/windows 0.45, firefox/macos 0.15 | /var/lib/kage/personas | false | false | false'
[ "${personas}" = "${expected}" ] || fail "unexpected persona configuration: ${personas}"
# The same identity as the sidecars: kage can move what they wrote.
"${compose[@]}" exec -T kage /usr/local/bin/kage persona list > /dev/null \
    || fail "kage persona list failed on an empty pool"
owner="$("${compose[@]}" exec -T browser-chromium stat -c '%u:%g' /var/lib/kage/personas)"
[ "${owner}" = "65532:65532" ] || fail "the personas volume belongs to ${owner}"
echo "    ${personas}"

echo "==> kage is configured for the browsers its sidecars bundle"
# engines.<engine>.browser_major of the configuration kage printed. A
# sidecar's node reads the document, so the host needs no JSON tool.
configured_major() {
    printf '%s' "${config}" | "${compose[@]}" exec -T "browser-$1" node -e \
        "let text = ''; process.stdin.on('data', (chunk) => (text += chunk)).on('end', () => console.log(JSON.parse(text).config.engines['$1'].browser_major));"
}
for engine in chromium firefox; do
    bundled="$("${compose[@]}" exec -T "browser-${engine}" cat /etc/kage/browser-version)"
    configured="$(configured_major "${engine}")"
    [ "${configured}" = "${bundled%%.*}" ] \
        || fail "the browser-${engine} image bundles ${bundled}; engines.${engine}.browser_major is ${configured}"
    echo "    browser-${engine} bundles ${bundled}, engines.${engine}.browser_major is ${configured}"
done

echo "==> only kage publishes a host port"
for service in kage browser-chromium browser-firefox postgres; do
    container="$("${compose[@]}" ps --quiet "${service}")"
    bindings="$(docker inspect --format '{{json .HostConfig.PortBindings}}' "${container}")"
    if [ "${service}" = kage ]; then
        [ "${bindings}" != '{}' ] || fail "kage publishes no host port"
    else
        [ "${bindings}" = '{}' ] || fail "${service} publishes a host port: ${bindings}"
    fi
    echo "    ${service}: ${bindings}"
done

echo "==> every service is healthy"
unhealthy="$("${compose[@]}" ps --format '{{.Service}} {{.Health}}' | grep -v ' healthy$' || true)"
[ -z "${unhealthy}" ] || fail "not healthy: ${unhealthy}"
"${compose[@]}" ps --format '    {{.Service}}: {{.Health}}'

echo "smoke test passed"
