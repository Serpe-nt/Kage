#!/usr/bin/env bash
# The load test of the release (spec §21, M12): 200 queued fetches, one
# batch job of three visitors, of the fixture site, in the world of the
# fetch test (deploy/compose.e2e.yaml). Nothing here needs or reaches the
# internet; k6 runs from its pinned image on the stack's default network,
# with its usage report off.
#
#   tests/e2e/load.sh        design A, in `just test-e2e`: the shipped `fast`
#                            profile; 45 s to 240 s
#   tests/e2e/load.sh load   design B, `just test-load`: the test-only `load`
#                            profile, `human`'s shape at a fifth of its times
#                            with its tails bounded and the shipped caps
#                            (3 at once, 20 a minute); 540 s to 720 s, the
#                            cap reached and never exceeded
#
# Both: every item `ok` with 200 at its first attempt in a browser and not
# from the cache, every page asked for once, by at most three addresses,
# without a header that names a forwarder, never two pages of one address
# at once, the job never `paused`. B also: at least 100 ms from the end of
# a page to the next page of the same persona.
#
# Needs what `just setup` installs: docker and curl. It uses its own
# compose project name and host port.
set -euo pipefail
cd "$(dirname "$0")/../.."

design="${1:-fast}"
case "${design}" in
    fast) min_s=45 max_s=240 min_gap_ms=300 ;;
    load) min_s=540 max_s=720 min_gap_ms=300 ;;
    *) echo "usage: tests/e2e/load.sh [fast|load]" >&2; exit 2 ;;
esac
k6_image='grafana/k6:2.3.0@sha256:9c2dee7f8ed74d317e4027c06a10f169b625638189de8d4555d0b3486a5aeb34'

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_LOAD_PORT:-18087}"
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false

# The shipped policies with the domain rule of this test, and for design B
# the test-only profile: never shipped, never a default.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
if [ "${design}" = load ]; then
    cat >> "${policies}/pacing-profiles.yaml" << 'EOF'

# Added by tests/e2e/load.sh: `human` at a fifth of its times, its tails
# bounded, no visits, the shipped caps. Test only.
load:
  gap_ms: { dist: lognormal, median: 900, p95: 3000, min: 300, max: 12000 }
  rest_ms: { dist: lognormal, median: 440, p95: 1400, min: 140, max: 6000 }
  burst_pages: { min: 4, max: 14 }
  burst_break_ms: { dist: lognormal, median: 12000, p95: 48000, max: 120000 }
  daily_cap_per_persona_per_domain: 300
  domain_concurrency: 3
  domain_rpm: 20
EOF
fi
cat >> "${policies}/domains.yaml" << EOF

# Added by tests/e2e/load.sh.
domains:
  fixture.kage.test:
    pacing: ${design}
    min_dwell: 500ms
    active_hours: ignore
EOF
if [ "$(grep --count '^domains:' "${policies}/domains.yaml")" -ne 1 ]; then
    grep --invert-match '^domains: {}$' "${policies}/domains.yaml" > "${policies}/domains.tmp"
    mv "${policies}/domains.tmp" "${policies}/domains.yaml"
fi
chmod --recursive a+rX "${policies}"
scratch="$(mktemp -d)"
evidence="${KAGE_E2E_EVIDENCE:-${scratch}}"
project="${KAGE_E2E_LOAD_PROJECT:-kage-e2e-load}"
compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --project-name "${project}")

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- load test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 120 kage >&2 || true
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
# A helper's node reads JSON (a sidecar cannot be the helper: it reaches
# nothing but kage).
value() {
    printf '%s' "$1" | "${compose[@]}" exec -T world node -e \
        "let t = ''; process.stdin.on('data', (c) => (t += c)).on('end', () => { const d = JSON.parse(t); const v = ($2); console.log(typeof v === 'string' ? v : JSON.stringify(v)); });"
}
kage() {
    "${compose[@]}" exec -T kage /usr/local/bin/kage "$@"
}
psql() {
    "${compose[@]}" exec -T postgres psql --username kage --dbname kage \
        --no-align --tuples-only --field-separator ' ' --command "$1"
}

echo "==> starting the stack with the world of the fetch test, design ${design}"
"${compose[@]}" up --detach --wait --wait-timeout 300
personas=''
for _ in $(seq 1 60); do
    personas="$(kage persona list --json 2> /dev/null || true)"
    if [ -n "${personas}" ] && [ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 3 ]; then
        break
    fi
    sleep 3
done
[ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 3 ] \
    || fail "the personas did not get ready: $(printf '%s' "${personas}" | head -c 1000)"
created="$(kage key create --name e2e-load --scope jobs --json)"
token="$(value "${created}" "d.token")"

echo "==> k6: one batch of 200 pages, 3 visitors, ${min_s} s to ${max_s} s"
k6_status=0
docker run --rm --network "${project}_default" \
    --env K6_NO_USAGE_REPORT=true \
    --env KAGE_URL=http://kage:8080 --env "KAGE_TOKEN=${token}" \
    --env SITE=http://fixture.kage.test:8088 --env SITE_HELPER=http://fixture:8088 \
    --env PAGES=200 --env VISITORS=3 --env FIRST_PAGE=1001 \
    --env "MIN_SECONDS=${min_s}" --env "MAX_SECONDS=${max_s}" --env "MIN_GAP_MS=${min_gap_ms}" \
    --volume "${PWD}/tests/load:/load:ro" --volume "${scratch}:/out" \
    --user "$(id -u):$(id -g)" \
    "${k6_image}" run --quiet --summary-export /out/k6-summary.json /load/batch.js \
    > "${scratch}/k6.log" 2>&1 || k6_status=$?
cp "${scratch}/k6.log" "${evidence}/k6-${design}.log" 2> /dev/null || true
cp "${scratch}/k6-summary.json" "${evidence}/k6-${design}-summary.json" 2> /dev/null || true
summary="$(cat "${scratch}/k6-summary.json" 2> /dev/null || echo '{}')"
gauge() {
    value "${summary}" "(d.metrics && d.metrics['$1'] && d.metrics['$1'].value) ?? null"
}
total="$(gauge load_total_seconds)"
echo "    ${total} s; the site saw $(gauge load_pages_seen) pages from $(gauge load_addresses) addresses, at least $(gauge load_least_gap_ms) ms apart per address, at most $(gauge load_site_most_in_60s) in any 60 s"
if [ "${k6_status}" -ne 0 ]; then
    grep -E '✗|✓|checks|http_req_failed' "${scratch}/k6.log" >&2 || tail -n 40 "${scratch}/k6.log" >&2
    fail "k6 exited ${k6_status}"
fi

echo "==> Kage's own record of the attempts: one page of a persona at a time, and the cap"
job="$(psql "SELECT id FROM jobs ORDER BY created_at DESC LIMIT 1")"
read -r overlapping least_rest_ms personas_used attempts <<< "$(psql "
    WITH a AS (
        SELECT a.persona_id, a.started_at, a.finished_at,
               lag(a.finished_at) OVER (PARTITION BY a.persona_id ORDER BY a.started_at) AS before
        FROM attempts a JOIN requests r ON r.id = a.request_id
        WHERE r.job_id = '${job}')
    SELECT count(*) FILTER (WHERE started_at < before),
           coalesce(min(round(extract(epoch FROM started_at - before) * 1000)), -1)::bigint,
           count(DISTINCT persona_id), count(*)
    FROM a")"
most_in_a_minute="$(psql "
    SELECT max(n) FROM (
        SELECT count(*) OVER (ORDER BY a.started_at
                              RANGE BETWEEN CURRENT ROW AND interval '59.999999 seconds' FOLLOWING) AS n
        FROM attempts a JOIN requests r ON r.id = a.request_id
        WHERE r.job_id = '${job}') starts")"
echo "    ${attempts} attempts by ${personas_used} personas; ${overlapping} overlapping; at least ${least_rest_ms} ms from the end of a page to the next of its persona; at most ${most_in_a_minute} starts in any 60 s"
[ "${attempts}" = 200 ] || fail "${attempts} attempts, not 200"
[ "${personas_used}" -le 3 ] || fail "${personas_used} personas read the batch"
[ "${overlapping}" = 0 ] || fail "a persona began a page before its last one ended"
if [ "${design}" = load ]; then
    [ "${least_rest_ms}" -ge 100 ] || fail "a persona went on ${least_rest_ms} ms after the end of a page"
    [ "${most_in_a_minute}" = 20 ] || fail "${most_in_a_minute} starts in a minute, not the cap of 20"
else
    [ "${most_in_a_minute}" -le 300 ] || fail "${most_in_a_minute} starts in a minute"
fi
cat > "${evidence}/load-${design}.json" << EOF
{"design":"${design}","seconds":${total},"bounds":[${min_s},${max_s}],"attempts":${attempts},"personas":${personas_used},"overlapping":${overlapping},"least_rest_ms":${least_rest_ms},"most_starts_in_60s":${most_in_a_minute},"site_pages":$(gauge load_pages_seen),"site_addresses":$(gauge load_addresses),"site_least_gap_ms":$(gauge load_least_gap_ms),"site_most_in_60s":$(gauge load_site_most_in_60s)}
EOF
echo "load test (${design}) passed"
