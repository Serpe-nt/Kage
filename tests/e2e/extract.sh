#!/usr/bin/env bash
# End-to-end test of extraction, robots.txt and the response cache (spec
# §14, §15): the compose stack with real browsers and real personas on a
# static proxy list, in the world of the fetch test
# (deploy/compose.e2e.yaml). Nothing here needs or reaches the internet.
#
# The fixture site answers under two names here. The policy of
# fixture.kage.test says nothing about robots.txt (the shipped default:
# nobody asks); the policy of rules.kage.test enforces it. Both are one
# site (kage.test), so the site has one scout.
#
# It shows, through the published port and the site's own request log:
#   - an essay fetched with include: ["extract"] by a persona of each
#     engine: its metadata, headings, text and markdown;
#   - the same request again answered from the cache: `cached: true`, and
#     not one request at the site while it was answered;
#   - another include set is a visit;
#   - nobody ever asked fixture.kage.test for its robots.txt;
#   - rules.kage.test was asked for its robots.txt once, by one address
#     that asked for no page of the site, before the first page there;
#   - a page the file disallows is `robots_denied` with the rule, and was
#     never asked for, by a fetch or by a crawl;
#   - a crawl there reads the sitemap the file names, not the guess
#     /sitemap.xml, by the same scout, and counts what it left out;
#   - the series of /metrics.
#
# The requests are made by a TypeScript client that was generated from the
# API's OpenAPI document (tests/api-client).
#
# Needs what `just setup` installs: docker, curl, and pnpm for the client.
# It uses its own compose project name and host port. With
# KAGE_E2E_EVIDENCE set to a directory, what was measured is kept there.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_EXTRACT_PORT:-18085}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false

# The shipped policies with the domain rules of this test: the fixture
# site is the operator's own, so it may be read at the pace of a test, and
# under one of its names its robots.txt is enforced.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
# The rules are entries of the shipped file's `domains` mapping, its last
# key: a second `domains:` would be a duplicate key, which the policy
# loader refuses, and `domains: {}` takes no entry.
sed --in-place 's/^domains: {}$/domains:/' "${policies}/domains.yaml"
[ "$(grep '^[a-z_]*:' "${policies}/domains.yaml" | tail -n 1)" = 'domains:' ] \
    || { echo "FAIL: policies/domains.yaml does not end with its domains mapping" >&2; exit 1; }
cat >> "${policies}/domains.yaml" << 'EOF'
  # Added by tests/e2e/extract.sh.
  kage.test:
    pacing: fast
    min_dwell: 500ms
    active_hours: ignore
  rules.kage.test:
    robots: enforce
EOF
chmod --recursive a+rX "${policies}"
# What a call to the API answered.
scratch="$(mktemp -d)"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --project-name "${KAGE_E2E_EXTRACT_PROJECT:-kage-e2e-extract}")
base="http://127.0.0.1:${KAGE_PUBLISH_PORT}"
site='http://fixture.kage.test:8088'
rules='http://rules.kage.test:8088'

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- extract test failed: service state and logs ---" >&2
        "${compose[@]}" ps --all >&2 || true
        "${compose[@]}" logs --no-color --tail 160 kage browser-chromium browser-firefox >&2 || true
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
# status is left in ${status}, the body in ${body}.
call() {
    local method="$1" path="$2" token="${3:-}" data="${4:-}"
    local arguments=(--silent --show-error --max-time 200 --request "${method}"
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
# expect <status> <what>: the last call ended with that status.
expect() {
    [ "${status}" = "$1" ] || fail "$2: status ${status}, expected $1: $(printf '%s' "${body}" | head -c 2000)"
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
# keep <name> <text>: keeps what was measured, when asked to.
keep() {
    if [ -n "${KAGE_E2E_EVIDENCE:-}" ]; then
        mkdir -p "${KAGE_E2E_EVIDENCE}"
        printf '%s\n' "$2" > "${KAGE_E2E_EVIDENCE}/$1"
    fi
}

echo "==> starting the stack with a world of its own"
"${compose[@]}" up --detach --wait --wait-timeout 300
ready
personas="$(kage persona list --json)"
check "both engines have a persona" "${personas}" "['chromium', 'firefox'].every((e) => d.personas.some((p) => p.engine === e))"

echo "==> an operator's key, and a client's key that may fetch and crawl"
created="$(kage key create --name e2e-operator --scope admin --json)"
admin="$(value "${created}" "d.token")"
call POST /v1/admin/keys "${admin}" '{"name":"e2e-extract","scopes":["fetch","jobs","crawl"],"requests_per_minute":600,"max_concurrent":16}'
expect 201 "POST /v1/admin/keys"
client="$(value "${body}" "d.token")"

echo "==> what a page says, the cache, robots.txt and a crawl that keeps to it, by the generated TypeScript client"
summary="$(cd tests/api-client && pnpm --silent run build && KAGE_API_URL="${base}" KAGE_API_KEY="${client}" \
    KAGE_E2E_SITE="${site}" KAGE_E2E_RULES="${rules}" pnpm --silent run e2e:extract)" || fail "the generated client failed"
keep extract-summary.json "${summary}"
check "the essay was extracted in both engines, by a persona of each" "${summary}" \
    "d.essays.length === 2 && d.essays[0].engine === 'chromium' && d.essays[1].engine === 'firefox' && d.essays[0].reader !== d.essays[1].reader && d.essays.every((e) => e.words > 150)"
check "what a site's policy does not let a request ask for is refused" "${summary}" \
    "JSON.stringify(d.refusals) === JSON.stringify(['robots_ignored', 'robots_enforced', 'robots_enforced', 'cache_ttl_too_long', 'validation_failed'])"
for index in 0 1; do
    echo "    $(value "${summary}" "d.essays[${index}].engine + ': ' + d.essays[${index}].words + ' words and ' + d.essays[${index}].links + ' links extracted; the visit took ' + d.essays[${index}].visit_ms + ' ms, the answer from the cache ' + d.essays[${index}].hit_ms + ' ms'")"
done

echo "==> the site's own account"
seen="$(got "${site}/__kage/requests")"
keep site-requests.json "${seen}"
both="{\"run\":${summary},\"seen\":${seen}}"
fixture_host='fixture.kage.test:8088'
rules_host='rules.kage.test:8088'

# The essay: one request for the first fetch and one for the other include
# set, and none while the cache answered.
check "each essay was asked for twice in all: the first fetch, and the other include set" "${both}" \
    "d.run.essays.every((e) => d.seen.filter((r) => r.host === '${fixture_host}' && r.path === e.path).length === 2)"
# (A browser asks for a site's icon by itself, some time after a page.)
check "the site saw no request while the cache answered" "${both}" \
    "d.run.essays.every((e) => !d.seen.some((r) => r.at >= e.hit_from && r.at <= e.hit_until && r.path !== '/favicon.ico'))"
echo "    the cache answered and the site saw nothing: $(value "${both}" "d.run.essays.map((e) => e.engine + ' ' + d.seen.filter((r) => r.path === e.path).length + ' requests for two fetches and a hit').join('; ')")"

# robots.txt: never under the name whose policy leaves it alone; once under
# the name whose policy enforces it.
check "nobody ever asked fixture.kage.test for its robots.txt" "${both}" \
    "!d.seen.some((r) => r.host === '${fixture_host}' && r.path === '/robots.txt')"
check "rules.kage.test was asked for its robots.txt exactly once" "${both}" \
    "d.seen.filter((r) => r.host === '${rules_host}' && r.path === '/robots.txt').length === 1"
scout="$(value "${both}" "d.seen.find((r) => r.host === '${rules_host}' && r.path === '/robots.txt').from")"
keep scout.txt "${scout}"
check "the file was asked for like a typed address, before any page under that name" "${both}" \
    "(() => { const file = d.seen.find((r) => r.host === '${rules_host}' && r.path === '/robots.txt'); const pages = d.seen.filter((r) => r.host === '${rules_host}' && r.path.startsWith('/site/')); return !file.headers.some(([name]) => name.toLowerCase() === 'referer') && pages.length > 0 && pages.every((r) => r.at > file.at); })()"
check "the address that asked for robots.txt asked for no page of the site, under either name" "${both}" \
    "!d.seen.some((r) => r.from === '${scout}' && (r.path.startsWith('/site/') || r.path.startsWith('/read/')))"
check "the scout asked for nothing but the file, the sitemaps and what a browser asks for by itself" "${both}" \
    "d.seen.filter((r) => r.from === '${scout}').every((r) => r.host === '${rules_host}' && (r.path === '/robots.txt' || r.path === '/favicon.ico' || r.path.startsWith('/sitemaps/')))"
check "no reader asked for robots.txt or a sitemap" "${both}" \
    "d.seen.filter((r) => r.path === '/robots.txt' || r.path.includes('sitemap')).every((r) => r.from === '${scout}')"
check "the readers are two other addresses" "${both}" \
    "(() => { const readers = new Set(d.seen.filter((r) => r.path.startsWith('/site/') || r.path.startsWith('/read/')).map((r) => r.from)); return readers.size === 2 && !readers.has('${scout}'); })()"
echo "    robots.txt: asked once, by ${scout}, which made $(value "${both}" "d.seen.filter((r) => r.from === '${scout}').length") requests in all: $(value "${both}" "d.seen.filter((r) => r.from === '${scout}').map((r) => r.path).join(' ')")"

# What the file disallows was never asked for.
check "the page the file disallows was never asked for, by a fetch or by a crawl" "${both}" \
    "!d.seen.some((r) => r.host === '${rules_host}' && r.path.startsWith('/site/private'))"
check "a page the file allows was read by a persona of each engine" "${both}" \
    "d.run.enforced.length === 2 && d.run.enforced.every((p) => d.seen.filter((r) => r.host === '${rules_host}' && r.path === '/site/a?reader=' + p.engine).length === 1)"
echo "    the first fetch under the enforced name waited for the file: $(value "${summary}" "d.enforced.map((p) => p.engine + ' ' + p.allowed_ms + ' ms').join(', ')")"

echo "==> the crawls: the sitemap the file names, and no page the file disallows"
check "each crawl read the file first and the sitemap it names, not the guess" "${summary}" \
    "d.crawls.length === 2 && d.crawls.every((c) => c.sitemaps[0].url.endsWith('/robots.txt') && c.sitemaps[0].state === 'read' && c.sitemaps.some((m) => m.url.endsWith('/sitemaps/index.xml') && m.state === 'index') && !c.sitemaps.some((m) => m.url.endsWith('/sitemap.xml')))"
check "nobody asked for the guess /sitemap.xml" "${both}" \
    "!d.seen.some((r) => r.path === '/sitemap.xml')"
check "each crawl counted the page it left out and read the others" "${summary}" \
    "d.crawls.every((c) => c.skipped.robots_denied === 1 && c.pages.includes('/site/about') && c.pages.includes('/site/annex') && c.pages.includes('/site/a') && !c.pages.includes('/site/private'))"
check "the sitemaps of each crawl were asked for by the scout, inside that crawl" "${both}" \
    "d.run.crawls.every((c) => { const from = Date.parse(c.created_at), until = Date.parse(c.finished_at); const maps = d.seen.filter((r) => r.path.startsWith('/sitemaps/') && r.at >= from && r.at <= until); return maps.length >= 3 && maps.every((r) => r.from === '${scout}' && r.host === '${rules_host}'); })"
for index in 0 1; do
    echo "    $(value "${summary}" "'reader ' + d.crawls[${index}].engine + ': ' + d.crawls[${index}].sitemaps.map((m) => m.url.replace(/^.*8088/, '') + ' ' + m.state + ' ' + m.urls).join(', ') + '; pages ' + d.crawls[${index}].pages.join(' ') + '; skipped ' + JSON.stringify(d.crawls[${index}].skipped)")"
done

echo "==> the store: one scout for the site, one answer for the origin, the entries of the cache"
[ "$(psql "SELECT count(*) FROM site_scouts WHERE site = 'kage.test'")" -eq 1 ] || fail "the site has not exactly one scout: $(psql 'SELECT * FROM site_scouts')"
[ "$(psql "SELECT count(*) FROM site_scouts")" -eq 1 ] || fail "more than one site has a scout"
[ "$(psql "SELECT origin || ' ' || status || ' ' || unreachable::text FROM robots_cache")" = "${rules} 200 false" ] \
    || fail "robots_cache is not the one answer: $(psql 'SELECT origin, status, unreachable FROM robots_cache')"
[ "$(psql "SELECT count(*) FROM response_cache")" -eq 4 ] || fail "the cache does not hold the four results that named a lifetime: $(psql 'SELECT url_key FROM response_cache')"
[ "$(psql "SELECT count(*) FROM results WHERE cached")" -eq 2 ] || fail "not exactly two results were served from the cache"
[ "$(psql "SELECT count(*) FROM results WHERE cached AND cached_at IS NULL")" -eq 0 ] || fail "a cached result does not say since when"
[ "$(psql "SELECT count(*) FROM requests WHERE api_key_id IS NULL AND spec ->> 'robots' = 'true'")" -eq 1 ] \
    || fail "the request for robots.txt is not Kage's own, or there is more than one"

echo "==> /metrics: the cache, robots.txt and extraction"
call GET /metrics "${admin}"
expect 200 "GET /metrics"
metrics="${scratch}/metrics"
printf '%s\n' "${body}" > "${metrics}"
sample() {
    awk -v series="$1" -v label="$2" 'index($0, series "{") == 1 && index($0, label) { print $NF; found = 1 } END { if (!found) print 0 }' "${metrics}" | head -n 1
}
[ "$(sample kage_cache_requests_total 'result="hit"')" = 2 ] || fail "two requests were answered from the cache: $(grep '^kage_cache_requests_total' "${metrics}")"
[ "$(sample kage_cache_requests_total 'result="stored"')" = 4 ] || fail "four results were put into the cache: $(grep '^kage_cache_requests_total' "${metrics}")"
[ "$(sample kage_robots_requests_total 'result="file"')" = 1 ] || fail "one robots.txt was brought: $(grep '^kage_robots_requests_total' "${metrics}")"
[ "$(sample kage_robots_denied_total 'reason="rule"')" = 2 ] || fail "two fetches were denied by a rule: $(grep '^kage_robots_denied_total' "${metrics}")"
grep --quiet '^kage_extract_duration_seconds_count ' "${metrics}" || fail "no sample of kage_extract_duration_seconds in /metrics"
grep --quiet '^kage_crawl_skipped_total{.*reason="robots_denied"' "${metrics}" \
    || fail "nothing counted as skipped for robots_denied: $(grep '^kage_crawl_skipped_total' "${metrics}")"
keep metrics-extract.txt "$(grep --extended-regexp '^kage_(cache_|robots_|extract_|crawl_skipped)' "${metrics}")"
echo "    $(grep --extended-regexp '^kage_(cache_requests|robots_requests|robots_denied)_total' "${metrics}" | tr '\n' ' ')"

echo "extract test passed"
