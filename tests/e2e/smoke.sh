#!/usr/bin/env bash
# Stack smoke test: brings the compose project up and checks that kage serves
# /healthz, both sidecars serve /v1/health, Postgres accepts connections, the
# migrations are applied, and the configuration loads with secrets redacted.
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

echo "==> sidecars are reachable by service name on the compose network"
"${compose[@]}" exec -T browser-firefox node -e \
    "fetch('http://browser-chromium:8090/v1/health').then((r) => process.exit(r.ok ? 0 : 1), () => process.exit(1))" \
    || fail "browser-chromium is not reachable from browser-firefox"

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
[ "${tables}" -eq 18 ] || fail "expected the 18 tables of spec §18, found ${tables}"
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
