#!/usr/bin/env bash
# End-to-end test of crawls (spec §13): the compose stack with real browsers
# and real personas on a static proxy list, in the world of the fetch test
# (deploy/compose.e2e.yaml). Nothing here needs or reaches the internet.
#
# It shows, through the published port and nothing else:
#   - the reading room of the fixture site (/site/) crawled by a persona of
#     each engine: the pages inside the depth and no others, each at its
#     depth; the seed typed and the pages after it reached by a click on a
#     link of the open page or of a page the persona went back to (the
#     articles of the listing each from the listing), at most 5 of the 21
#     addresses typed; five
#     links nobody can see left alone; copies and near-copies flagged; the
#     long page left when the persona's patience with it was used up, at
#     the depth the page itself reports;
#   - what a page that is left that way sees, in the page's own words: the
#     pointer arriving, a trusted click, user activation;
#   - a crawl with sitemaps: a scout reads them, from another address than
#     the site's reader, which never asks for one; the pages only a
#     sitemap names are read;
#   - the rest between two pages under a pacing profile that has one, by
#     the site's own clock, against the same pages read under the profile
#     without one (a click takes its own second or so either way);
#   - a crawl that is cancelled while it reads: the page under way is
#     finished and not one more is asked for;
#   - no page of any crawl asked for twice, and the series of `/metrics`.
#
# The crawls are run by a TypeScript client that was generated from the
# API's OpenAPI document (tests/api-client).
#
# Needs what `just setup` installs: docker, curl, and pnpm for the client.
# It uses its own compose project name and host port. With
# KAGE_E2E_EVIDENCE set to a directory, what was measured is kept there.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_CRAWL_PORT:-18084}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false
# A persona's patience with a page, short enough for a test: the long page
# of the fixture takes minutes to scroll and is left after seconds.
export KAGE_E2E_PATIENCE_MIN=2s
export KAGE_E2E_PATIENCE_MEDIAN=4s
export KAGE_E2E_PATIENCE_P95=8s
export KAGE_E2E_PATIENCE_MAX=12s

# The shipped policies with the domain rule of this test (the fixture site
# is the operator's own, so it may be read at the pace of a test) and a
# pacing profile that is `fast` with a rest between two pages.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
# The rule is an entry of the shipped file's `domains` mapping, its last
# key: a second `domains:` would be a duplicate key, which the policy
# loader refuses, and `domains: {}` takes no entry.
sed --in-place 's/^domains: {}$/domains:/' "${policies}/domains.yaml"
[ "$(grep '^[a-z_]*:' "${policies}/domains.yaml" | tail -n 1)" = 'domains:' ] \
    || { echo "FAIL: policies/domains.yaml does not end with its domains mapping" >&2; exit 1; }
cat >> "${policies}/domains.yaml" << 'EOF'
  # Added by tests/e2e/crawl.sh.
  kage.test:
    pacing: fast
    min_dwell: 500ms
    active_hours: ignore
EOF
cat >> "${policies}/pacing-profiles.yaml" << 'EOF'

# Added by tests/e2e/crawl.sh: `fast`, and a rest of about a second between
# leaving a page and asking for the next one.
e2e_rested:
  gap_ms: { dist: uniform, min: 300, max: 1200 }
  rest_ms: { dist: uniform, min: 1200, max: 1800 }
  burst_pages: { min: 50, max: 50 }
  burst_break_ms: { dist: uniform, min: 0, max: 0 }
  daily_cap_per_persona_per_domain: 5000
  domain_concurrency: 10
  domain_rpm: 300
EOF
chmod --recursive a+rX "${policies}"
# What a call to the API answered.
scratch="$(mktemp -d)"

compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml
    --project-name "${KAGE_E2E_CRAWL_PROJECT:-kage-e2e-crawl}")
base="http://127.0.0.1:${KAGE_PUBLISH_PORT}"
site='http://fixture.kage.test:8088'

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- crawl test failed: service state and logs ---" >&2
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
# problem <status> <code> <what>: the last call was refused with a problem
# document of that code.
problem() {
    expect "$1" "$3"
    check "$3" "${body}" "d.code === '$2' && d.status === $1"
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

echo "==> an operator's key, and a client's key that may crawl"
created="$(kage key create --name e2e-operator --scope admin --json)"
admin="$(value "${created}" "d.token")"
call POST /v1/admin/keys "${admin}" '{"name":"e2e-crawl","scopes":["jobs","crawl"],"requests_per_minute":600,"max_concurrent":16}'
expect 201 "POST /v1/admin/keys"
client="$(value "${body}" "d.token")"
call POST /v1/admin/keys "${admin}" '{"name":"e2e-jobs-only","scopes":["jobs","fetch"]}'
expect 201 "POST /v1/admin/keys"
jobs_only="$(value "${body}" "d.token")"
call POST /v1/jobs "${jobs_only}" "{\"type\":\"crawl\",\"seeds\":[\"${site}/site/\"],\"defaults\":{\"level\":3}}"
problem 403 scope_missing "a crawl without the crawl scope"
[ "$(psql "SELECT count(*) FROM jobs")" -eq 0 ] || fail "a refused crawl was stored"
echo "    a crawl without its scope is 403"

echo "==> the reading room in each engine, a page that is left, sitemaps, a rest and a cancel, by the generated TypeScript client"
summary="$(cd tests/api-client && pnpm --silent run build && KAGE_API_URL="${base}" KAGE_API_KEY="${client}" \
    KAGE_E2E_SITE="${site}" pnpm --silent run e2e:crawl)" || fail "the generated client failed"
keep crawl-summary.json "${summary}"
check "a crawl that cannot be accepted is refused whole" "${summary}" \
    "JSON.stringify(d.refusals) === JSON.stringify(['validation_failed', 'ssrf_denied', 'validation_failed', 'validation_failed', 'level_required'])"
check "the reading room was read in both engines, by a persona of each" "${summary}" \
    "d.rooms.length === 2 && d.rooms[0].engine === 'chromium' && d.rooms[1].engine === 'firefox' && d.rooms.every((r) => r.pages === 21) && d.rooms[0].persona !== d.rooms[1].persona"
check "typed, click and back_click all occur in both engines, and few addresses are typed" "${summary}" \
    "d.rooms.every((r) => r.reached.typed >= 1 && r.reached.typed <= 5 && r.reached.click >= 4 && r.reached.back_click >= 8)"
check "the long page was left by the persona's patience" "${summary}" \
    "d.rooms.every((r) => r.tall.ended_by === 'patience' && r.tall.reached < 0.6)"
for room in 0 1; do
    echo "    $(value "${summary}" "d.rooms[${room}].engine + ': ' + d.rooms[${room}].pages + ' pages in ' + d.rooms[${room}].seconds + ' s, ' + JSON.stringify(d.rooms[${room}].reached) + ', skipped ' + JSON.stringify(d.rooms[${room}].skipped) + ', the near-copy ' + d.rooms[${room}].near_duplicate_bits + ' bits apart, scrolling ended by ' + JSON.stringify(d.rooms[${room}].scroll_ends)")"
    echo "      $(value "${summary}" "d.rooms[${room}].order.join(', ')")"
done

echo "==> the site's own account: every page once, the long page as far as the crawl says, a trusted click on the page that was left"
seen="$(got "${site}/__kage/requests")"
reports="$(got "${site}/__kage/reports")"
keep site-requests.json "${seen}"
keep site-reports.json "${reports}"
both="{\"crawl\":${summary},\"seen\":${seen},\"reports\":${reports}}"
# (The address that moved is answered with a redirect to the first article,
# which the browser follows by itself: that one page is asked for a second
# time, by the site's own doing.)
check "no page of the reading room was asked for twice by one crawl" "${both}" \
    "d.crawl.rooms.every((room) => { const t0 = Date.parse(room.times[0].started_at), t1 = Math.max(...room.times.map((t) => Date.parse(t.finished_at))); const paths = d.seen.filter((r) => r.at >= t0 - 50 && r.at <= t1 && r.path.startsWith('/site/')).map((r) => r.host + r.path); const twice = paths.filter((path, index) => paths.indexOf(path) !== index); return paths.length >= 21 && twice.length <= 1 && twice.every((path) => path.endsWith('/site/a')); })"
check "nobody asked for a page no reader can reach" "${seen}" \
    "!d.some((r) => r.path.startsWith('/site/trap'))"
check "the reader never asked for robots.txt, and nobody for a sitemap outside the crawls that read them" "${both}" \
    "!d.seen.some((r) => r.path === '/robots.txt') && d.seen.filter((r) => r.path.includes('sitemap')).every((r) => d.crawl.sitemaps.some((s) => r.at >= Date.parse(s.created_at) && r.at <= Date.parse(s.finished_at)))"
check "the long page says it was left where the crawl says" "${both}" \
    "(() => { const tall = d.reports.filter((r) => r.page === 'tall'); return tall.length === 2 && tall.every((r, i) => Math.abs(r.scrollY / (r.scrollHeight - r.innerHeight) - d.crawl.rooms[i].tall.reached) < 0.03); })()"
check "the page that was left saw the pointer come, a trusted click and user activation, twice in each engine" "${both}" \
    "d.crawl.left.every((l) => { const mine = d.reports.filter((r) => r.page === 'leave' && r.run === l.marker); return mine.length === 2 && mine.every((r) => r.activation === true && r.moves > 5 && r.events.some((e) => e.type === 'click' && e.trusted === true)) && mine[0].events.some((e) => e.type === 'click' && e.target === 'next') && mine[1].events.some((e) => e.type === 'click' && e.target === 'below') && mine[1].wheels > 0; })"
check "no request names a forwarding header" "${seen}" \
    "d.every((r) => !r.headers.some(([name]) => /^(x-forwarded-|forwarded\$|x-real-ip\$|via\$)/i.test(name)))"
left="$(value "${both}" "JSON.stringify(d.crawl.left.map((l) => ({ engine: l.engine, leaves: d.reports.filter((r) => r.page === 'leave' && r.run === l.marker).map((r) => ({ events: r.events.map((e) => e.type + (e.target ? '#' + e.target : '') + (e.trusted === false ? '!untrusted' : '')).join(' '), moves: r.moves, wheels: r.wheels, activation: r.activation, click_to_pagehide_ms: (r.events.findLast((e) => e.type === 'pagehide') || {}).t - (r.events.findLast((e) => e.type === 'click') || {}).t })) })))")"
keep left-pages.json "${left}"
echo "    what the pages that were left saw: ${left}"
echo "    the long page: $(value "${both}" "d.reports.filter((r) => r.page === 'tall').map((r, i) => d.crawl.rooms[i].engine + ' ' + (r.scrollY / (r.scrollHeight - r.innerHeight)).toFixed(4) + ' by the page, ' + d.crawl.rooms[i].tall.reached + ' by the crawl, patience ' + d.crawl.rooms[i].tall.patience_ms + ' ms').join('; ')")"

echo "==> sitemaps: a scout asked for them, from another address than the reader's"
check "each sitemap crawl read the index and the plain sitemap" "${summary}" \
    "d.sitemaps.length === 2 && d.sitemaps.every((s) => s.sitemaps[0].state === 'index' && s.sitemaps.some((m) => m.state === 'read' && m.urls === 3))"
# The site has one scout for both crawls (the persona neither reading room
# was read by), so both saw the gzipped sitemap the same way: a Firefox
# reads it, Chrome saves it and shows nothing of it.
check "a Firefox scout read the gzipped sitemap and its pages were read; a Chrome scout was shown nothing of it" "${summary}" \
    "d.sitemaps.every((s) => { const packed = s.sitemaps.find((m) => m.url.endsWith('.xml.gz')); return packed.state === 'read' ? s.pages.includes('/site/only-1') && s.pages.includes('/site/only-2') : packed.state === 'sitemap_unreadable' && s.pages.length === 3; }) && new Set(d.sitemaps.map((s) => s.sitemaps.find((m) => m.url.endsWith('.xml.gz')).state)).size === 1"
check "one address asked for every sitemap of both crawls, and read no page of the site" "${both}" \
    "(() => { const scouts = new Set(d.seen.filter((r) => r.path.includes('sitemap')).map((r) => r.from)); return scouts.size === 1 && !d.seen.some((r) => scouts.has(r.from) && r.path.startsWith('/site/')); })()"
check "the sitemaps came from one address and the pages from another, in each crawl" "${both}" \
    "d.crawl.sitemaps.every((s) => { const created = Date.parse(s.created_at), first = Date.parse(s.first_page_at), end = Date.parse(s.finished_at); const maps = d.seen.filter((r) => r.path.includes('sitemap') && r.at >= created && r.at <= end); const pages = d.seen.filter((r) => r.path.startsWith('/site/') && r.at >= first - 50 && r.at <= end); const scouts = new Set(maps.map((r) => r.from)), readers = new Set(pages.map((r) => r.from)); return maps.length >= 3 && pages.length >= 3 && scouts.size === 1 && readers.size === 1 && ![...scouts].some((from) => readers.has(from)) && maps.every((r) => r.at <= first); })"
check "a sitemap is asked for like a typed address" "${both}" \
    "d.seen.filter((r) => r.path.includes('sitemap')).every((r) => !r.headers.some(([name]) => name.toLowerCase() === 'referer'))"
echo "    $(value "${summary}" "d.sitemaps.map((s) => 'reader ' + s.engine + ': ' + s.sitemaps.map((m) => m.url.replace(/^.*\\//, '') + ' ' + m.state + ' ' + m.urls).join(', ') + '; pages ' + s.pages.join(' ')).join(' | ')")"
echo "    $(value "${both}" "d.crawl.sitemaps.map((s) => { const created = Date.parse(s.created_at), end = Date.parse(s.finished_at); const maps = d.seen.filter((r) => r.path.includes('sitemap') && r.at >= created && r.at <= end); const ua = (r) => (r.headers.find(([name]) => name.toLowerCase() === 'user-agent') || [])[1] || ''; return 'reader ' + s.engine + ': the scout was ' + (/Firefox/.test(ua(maps[0])) ? 'Firefox' : 'Chrome') + ' at ' + maps[0].from; }).join('; ')")"

echo "==> the rest between two pages, by the site's own clock"
rests="$(value "${both}" "(() => { const gaps = (times) => { const t0 = Date.parse(times[0].started_at); const t1 = Math.max(...times.map((t) => Date.parse(t.finished_at))); const at = (url) => { const u = new URL(url); const hit = d.seen.find((r) => r.at >= t0 - 50 && r.at <= t1 && r.host === u.host && r.path === u.pathname + u.search); return hit ? hit.at : null; }; return times.slice(1).map((next, i) => at(next.url) === null ? null : at(next.url) - Date.parse(times[i].finished_at)).filter((gap) => gap !== null); }; const rested = gaps(d.crawl.rested.times); const same = gaps(d.crawl.unrested.times); const fast = d.crawl.rooms.flatMap((room) => gaps(room.times)); const stats = (all) => ({ pages: all.length, min_ms: Math.min(...all), median_ms: all.slice().sort((a, b) => a - b)[Math.floor(all.length / 2)], max_ms: Math.max(...all) }); return JSON.stringify({ e2e_rested: stats(rested), fast_same_pages: stats(same), fast_reading_rooms: stats(fast) }); })()")"
keep rest-between-pages.json "${rests}"
echo "    from the end of a page to the request for the next: ${rests}"
# The rest of `e2e_rested` is 1200 to 1800 ms and comes before the pointer
# moves at all, so no next page may be asked for sooner than 1200 ms. A
# click takes its own time under either profile (the pointer travels, the
# link is pressed), which is why the same five clicks without the rest are
# the measure of what the rest adds.
check "under a profile with a rest no next page is asked for before the rest is over" "${rests}" \
    "d.e2e_rested.pages === 5 && d.e2e_rested.min_ms >= 1200"
check "the same pages under the profile without the rest are asked for most of a second sooner" "${rests}" \
    "d.fast_same_pages.pages === 5 && d.e2e_rested.median_ms - d.fast_same_pages.median_ms >= 800"

echo "==> the cancel stopped the crawl within one page"
check "the cancel stopped the crawl within one page" "${summary}" \
    "d.cancelled.succeeded <= d.cancelled.succeeded_at_cancel + 1 && d.cancelled.asked === d.cancelled.succeeded && d.cancelled.cancelled <= 1"
check "the site saw the slow pages that ended and none after them" "${both}" \
    "d.seen.filter((r) => r.path.startsWith('/site/slow/')).length === d.crawl.cancelled.succeeded"
echo "    $(value "${summary}" "d.cancelled.succeeded_at_cancel + ' pages had ended when the cancel came, ' + d.cancelled.succeeded + ' in the end'")"

echo "==> /metrics: the crawls, how their pages were reached, what they skipped and what repeated"
call GET /metrics "${admin}"
expect 200 "GET /metrics"
metrics="${scratch}/metrics"
printf '%s\n' "${body}" > "${metrics}"
for series in kage_jobs kage_job_items_total kage_jobs_finished_total kage_crawl_pages_total \
    kage_crawl_skipped_total kage_crawl_duplicates_total; do
    grep --quiet "^${series}[{ ]" "${metrics}" || fail "no sample of ${series} in /metrics"
done
awk '/^kage_jobs\{/ && /job_type="crawl"/ && /state="succeeded"/ { found = 1 } END { exit !found }' "${metrics}" \
    || fail "the crawls are not counted: $(grep '^kage_jobs' "${metrics}" | head -8)"
awk '/^kage_jobs\{/ && /job_type="crawl"/ && /state="cancelled"/ { found = 1 } END { exit !found }' "${metrics}" \
    || fail "the cancelled crawl is not counted: $(grep '^kage_jobs' "${metrics}" | head -8)"
for reached in typed click back_click; do
    grep --quiet "^kage_crawl_pages_total{.*reached=\"${reached}\"" "${metrics}" \
        || fail "no page counted as ${reached}: $(grep '^kage_crawl_pages_total' "${metrics}")"
done
for reason in hidden off_site not_a_page too_deep; do
    grep --quiet "^kage_crawl_skipped_total{.*reason=\"${reason}\"" "${metrics}" \
        || fail "nothing counted as skipped for ${reason}: $(grep '^kage_crawl_skipped_total' "${metrics}")"
done
for kind in exact near; do
    grep --quiet "^kage_crawl_duplicates_total{.*kind=\"${kind}\"" "${metrics}" \
        || fail "no duplicate of kind ${kind}: $(grep '^kage_crawl_duplicates_total' "${metrics}")"
done
keep metrics-crawl.txt "$(grep --extended-regexp '^kage_(crawl_|jobs\{|jobs_finished_total|job_items_total)' "${metrics}")"
echo "    $(grep '^kage_crawl_pages_total' "${metrics}" | tr '\n' ' ')"
echo "    $(grep '^kage_crawl_skipped_total' "${metrics}" | tr '\n' ' ')"
echo "    $(grep '^kage_crawl_duplicates_total' "${metrics}" | tr '\n' ' ')"

echo "==> nothing of who read is shown to the key, and the frontier is stored"
call GET "/v1/jobs/$(value "${summary}" "d.rooms[0].id")" "${client}"
expect 200 "GET /v1/jobs/{id}"
check "a crawl shows its progress and nothing of its visitors" "${body}" \
    "d.type === 'crawl' && d.crawl.ended === 'completed' && d.crawl.pages === 21 && !JSON.stringify(d).includes('persona') && !('slots' in d.crawl) && !('scouts' in d.crawl)"
[ "$(psql "SELECT count(*) FROM crawl_frontier WHERE state = 'fetching'")" -eq 0 ] || fail "a page is still marked as being read"
[ "$(psql "SELECT count(DISTINCT job_id) FROM crawl_frontier")" -ge 8 ] || fail "the frontiers of the crawls are not stored"

echo "crawl test passed"
