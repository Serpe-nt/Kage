#!/usr/bin/env bash
# End-to-end test of a declared proxy list (ADR 0013, spec §7.1, §7.4): the
# compose stack with real browsers and a world of its own
# (deploy/compose.e2e.yaml), with Kage on the operator's own list of the
# world's proxies (deploy/compose.e2e-declared.yaml) and no provider API.
# Nothing here needs or reaches the internet.
#
# It shows, with `kage` as the one way in:
#   - `kage config check` naming the provider as a list that offers no
#     replacement, no replaced log, no account and no stats;
#   - three personas, each made of its entry's declaration: Vienna in
#     German, Geneva in French with Zurich's zone, and an entry declared in
#     Germany; their languages are the browser's own list for the declared
#     language, their zones the declared or the country's;
#   - `kage proxy health` saying which proxies are live and which dead,
#     from the echo: three live, the entry where no proxy listens dead;
#   - the entry the world's echo places in Milan although it is declared in
#     Germany: `proxy_geo_mismatch` after two lookups, the proxy degraded
#     and usable, its persona German as declared.
#
# It uses its own compose project name and host port. With
# KAGE_E2E_EVIDENCE set to a directory, what was measured is kept there.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_DECLARED_PORT:-18089}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false

# The shipped policies, unchanged.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
chmod --recursive a+rX "${policies}"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --file deploy/compose.e2e-declared.yaml
    --project-name "${KAGE_E2E_DECLARED_PROJECT:-kage-e2e-declared}")

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- declared test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 120 kage world >&2 || true
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
kage() {
    "${compose[@]}" exec -T kage /usr/local/bin/kage "$@"
}
# keep <file> <text>: what was measured, when KAGE_E2E_EVIDENCE names a
# directory.
keep() {
    if [ -n "${KAGE_E2E_EVIDENCE:-}" ]; then
        mkdir -p "${KAGE_E2E_EVIDENCE}"
        printf '%s\n' "$2" > "${KAGE_E2E_EVIDENCE}/$1"
    fi
}

echo "==> starting the stack with a world of its own and the operator's declared list"
"${compose[@]}" up --detach --wait --wait-timeout 300

echo "==> kage config check names the provider and what it does not offer"
checked="$(kage config check)"
keep config-check.json "${checked}"
check "a declared list, listing only" "${checked}" \
    "d.proxies.provider === 'static' && d.proxies.static_proxies === 4 && JSON.stringify(d.proxies.unsupported) === JSON.stringify(['replacement', 'replaced_log', 'account', 'stats']) && d.proxies.plain_exits.length === 0"
check "the probes read the world's echo, as configured" "${checked}" \
    "d.proxies.health_endpoints.echo === 'http://world.kage.test:8089/ip'"

echo "==> three personas, each made of its entry's declaration"
personas=''
for _ in $(seq 1 60); do
    personas="$(kage persona list --json 2> /dev/null || true)"
    if [ -n "${personas}" ] && [ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 3 ]; then
        break
    fi
    sleep 3
done
keep personas.json "${personas}"
proxies="$(kage proxy list --json)"
keep proxies.json "${proxies}"
check "three ready personas" "${personas}" "d.personas.filter((p) => p.ready && p.state === 'active').length === 3"
# The browsers' own lists for the declared languages (spec §6.1).
lists="{ de: { chromium: 'de-DE,de,en-US,en', firefox: 'de,en-US,en' }, fr: { chromium: 'fr-FR,fr,en-US,en', firefox: 'fr,fr-FR,en-US,en' } }"
both="$(printf '{"personas": %s, "proxies": %s}' "${personas}" "${proxies}")"
for expected in \
    "vienna AT de-AT de Europe/Vienna" \
    "geneva CH fr-CH fr Europe/Zurich" \
    "berlin DE de-DE de Europe/Berlin"; do
    read -r entry country locale language zone <<< "${expected}"
    check "${entry}: ${country}, ${locale}, its browser's ${language} list, ${zone}" "${both}" \
        "((proxy) => { const p = d.personas.personas.find((p) => p.proxy_id === proxy.id); const lists = ${lists}; return proxy.declared && proxy.country === '${country}' && p && p.country === '${country}' && p.locale === '${locale}' && p.languages.join(',') === lists['${language}'][p.engine] && p.timezone === '${zone}'; })(d.proxies.proxies.find((x) => x.provider_ref === '${entry}'))"
    echo "    ${entry}: $(value "${both}" "((proxy) => { const p = d.personas.personas.find((p) => p.proxy_id === proxy.id); return p.engine + ' ' + p.country + ' ' + p.locale + ' [' + p.languages.join(', ') + '] ' + p.timezone; })(d.proxies.proxies.find((x) => x.provider_ref === '${entry}'))")"
done
# Once its backoff ends the entry where no proxy listens may get a persona
# of its declaration, as any proxy would (1.x); that persona is never ready.
check "the entry where no proxy listens has no ready persona" "${both}" \
    "((proxy) => !d.personas.personas.some((p) => p.proxy_id === proxy.id && p.ready) && proxy.state !== 'healthy')(d.proxies.proxies.find((x) => x.provider_ref === 'gone'))"

echo "==> kage proxy health says which proxies are live and which dead, from the echo"
health="$(kage proxy health --json 2> /dev/null)"
keep health.json "${health}"
check "three live, one dead" "${health}" "d.live === 3 && d.dead === 1 && d.results.length === 4"
check "the dead one is the entry where no proxy listens" "${health}" \
    "d.results.filter((r) => r.verdict !== 'ok').map((r) => r.provider_ref + ':' + r.verdict).join() === 'gone:unreachable'"
table="$(kage proxy health 2> /dev/null)"
keep health.txt "${table}"
case "${table}" in
    *"4 checked: 3 live, 1 dead"$'\n'"pool: "*) ;;
    *) fail "the table does not count live and dead proxies: ${table}" ;;
esac
case "${table}" in
    *"provider: static (lists only; replacement, replaced_log, account and stats unsupported)"*) ;;
    *) fail "the table does not say what the provider does not offer: ${table}" ;;
esac
echo "${table}" | sed 's/^/    /'

echo "==> declared in Germany, placed in Milan twice: geo_mismatch, degraded, German as declared"
berlin=''
for _ in $(seq 1 100); do
    proxies="$(kage proxy list --json 2> /dev/null)"
    berlin="$(value "${proxies}" "d.proxies.find((x) => x.provider_ref === 'berlin')")"
    if [ "$(value "${berlin}" "d.state === 'degraded' && d.last_health.geo_mismatches >= 2")" = true ]; then
        break
    fi
    sleep 3
done
keep mismatch-proxies.json "${proxies}"
check "degraded and usable, the lookup's country beside the declared one" "${berlin}" \
    "d.state === 'degraded' && d.country === 'DE' && d.observed_country === 'IT' && d.last_health.geo_mismatches >= 2 && d.carries_persona"
alert="$("${compose[@]}" logs --no-color kage | grep 'proxy_geo_mismatch' | grep 'declared in DE in IT' | head -n 1 || true)"
keep mismatch-alert.log "${alert}"
[ -n "${alert}" ] || fail "no proxy_geo_mismatch alert names the declared and the observed country"
after="$(kage persona list --json)"
keep mismatch-personas.json "${after}"
check "its persona is the same, German and in Berlin's zone" "$(printf '{"personas": %s, "proxy": %s}' "${after}" "${berlin}")" \
    "((p) => p && p.state === 'active' && p.country === 'DE' && p.locale === 'de-DE' && p.timezone === 'Europe/Berlin' && p.geolocation === null)(d.personas.personas.find((p) => p.proxy_id === d.proxy.id))"
check "the two placed where they are declared stay healthy" "${proxies}" \
    "['vienna', 'geneva'].every((entry) => d.proxies.find((x) => x.provider_ref === entry).state === 'healthy')"
echo "    berlin: $(value "${berlin}" "d.state + ', declared ' + d.country + ', observed ' + d.observed_country + ', ' + d.last_health.geo_mismatches + ' mismatches'")"

echo "declared test passed"
