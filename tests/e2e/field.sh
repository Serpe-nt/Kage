#!/usr/bin/env bash
# End-to-end test of the field campaign's runner (tests/field,
# docs/runbooks/field-campaign.md, "A session runs it"): the compose stack in
# the world of the fetch test (deploy/compose.e2e.yaml), and the runner run
# for real against the fixture site in place of a round's real sites.
# Nothing here needs or reaches the internet.
#
# A round of two sites and one control, all of the fixture site:
#   site 1  fixture.kage.test, from /site/about: a link to a host the deny
#           list keeps the reader off, a link its robots.txt disallows, the
#           site's menu
#   site 2  wall.field.test, from /wall/: its first link leads into a
#           challenge that never resolves, its second around it
#   C1      rules.kage.test, the control
#
# It shows, through the published port and the store and nothing else:
#   - the runner refuses to run without KAGE_FIELD=1;
#   - `assign` deals the world's personas to the two sites, one each, by a
#     draw that is written down, and `policies` puts the round's entries in
#     force;
#   - site 1's batch: a session pinned to the site's persona, every page read
#     by that persona alone, the robots.txt asked for from another address
#     and kept to, the deny list kept, the record and the section written;
#   - the same site again the same day is refused before anything is asked;
#   - site 2's batch: the second page is a block, the persona is not
#     replaced, the runner ends the batch at once, and the page that was
#     left to read is never asked for;
#   - the report names no host, no address and no id, while the private
#     record names the host it read.
#
# Needs what `just setup` installs: docker, curl, node. It uses its own
# compose project name and host port. With KAGE_E2E_EVIDENCE set to a
# directory, what was measured is kept there.
set -euo pipefail
cd "$(dirname "$0")/../.."

scripts/dev-secrets.sh

export KAGE_PUBLISH_PORT="${KAGE_E2E_FIELD_PORT:-18087}"
# No provider API, no reputation lookups, no warm-up visits: the shell wins
# over the env file.
export KAGE_WEBSHARE_API_KEY=
export KAGE_ABUSEIPDB_KEY=
export KAGE_PERSONAS_WARMUP=false
# A persona's patience with a page, short enough for a test.
export KAGE_E2E_PATIENCE_MIN=1s
export KAGE_E2E_PATIENCE_MEDIAN=2s
export KAGE_E2E_PATIENCE_P95=4s
export KAGE_E2E_PATIENCE_MAX=6s

# The shipped policies; the runner writes the round's entries into the
# same directory, as it does into the private one of a round.
policies="$(mktemp -d)"
export KAGE_E2E_POLICIES="${policies}"
cp policies/*.yaml "${policies}/"
chmod --recursive a+rwX "${policies}"
scratch="$(mktemp -d)"
private="${scratch}/private"

project="${KAGE_E2E_FIELD_PROJECT:-kage-e2e-field}"
compose=(docker compose --file deploy/compose.yaml --file deploy/compose.e2e.yaml --project-name "${project}")
base="http://127.0.0.1:${KAGE_PUBLISH_PORT}"

cleanup() {
    local status=$?
    if [ "${status}" -ne 0 ]; then
        echo "--- field test failed: service state and logs ---" >&2
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
# value <json> <expression over d>: prints the expression's value.
value() {
    printf '%s' "$1" | node -e \
        "let t = ''; process.stdin.on('data', (c) => (t += c)).on('end', () => { const d = JSON.parse(t); const v = ($2); console.log(typeof v === 'string' ? v : JSON.stringify(v)); });"
}
# check <what> <json> <expression over d>: fails unless the expression holds.
check() {
    [ "$(value "$2" "Boolean($3)")" = true ] || fail "$1: $(printf '%s' "$2" | head -c 3000)"
}
# got <url>: the JSON a helper of the world serves, asked from inside the network.
got() {
    "${compose[@]}" exec -T world node -e \
        "fetch('$1').then((r) => r.text()).then((t) => console.log(t), (e) => { console.error(String(e)); process.exit(1); })"
}
kage() {
    "${compose[@]}" exec -T kage /usr/local/bin/kage "$@"
}
keep() {
    if [ -n "${KAGE_E2E_EVIDENCE:-}" ]; then
        mkdir -p "${KAGE_E2E_EVIDENCE}"
        printf '%s\n' "$2" > "${KAGE_E2E_EVIDENCE}/$1"
    fi
}
# field <command> [args]: the runner, as a round runs it.
field() {
    KAGE_FIELD=1 KAGE_FIELD_SITES="${scratch}/sites.json" KAGE_FIELD_DIR="${private}" \
        KAGE_API_URL="${base}" KAGE_FIELD_KEY="${token}" KAGE_FIELD_POSTGRES="${project}-postgres-1" \
        KAGE_FIELD_POLICIES="${policies}" KAGE_FIELD_SHIPPED=policies \
        KAGE_FIELD_POLL_S=2 KAGE_FIELD_KEEPALIVE_S=20 node tests/field/field.mjs "$@"
}

echo "==> the runner refuses to run without its guard"
if KAGE_FIELD_SITES=tests/field/sites.example.json node tests/field/field.mjs check 1 > "${scratch}/guard" 2>&1; then
    fail "the runner ran without KAGE_FIELD=1"
fi
grep --quiet "KAGE_FIELD=1" "${scratch}/guard" || fail "the refusal does not say why: $(cat "${scratch}/guard")"

echo "==> starting the stack with a world of its own"
"${compose[@]}" up --detach --wait --wait-timeout 300
for _ in $(seq 1 60); do
    personas="$(kage persona list --json 2> /dev/null || true)"
    if [ -n "${personas}" ] && [ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 3 ]; then
        break
    fi
    sleep 3
done
[ "$(value "${personas}" "d.personas.filter((p) => p.ready).length")" = 3 ] || fail "the personas did not get ready"

echo "==> the round's key, and its sites file"
admin="$(value "$(kage key create --name e2e-field-operator --scope admin --json)" "d.token")"
created="$(curl --silent --show-error --fail --request POST --header "Authorization: Bearer ${admin}" \
    --header 'Content-Type: application/json' \
    --data '{"name":"e2e-field","scopes":["fetch","jobs","crawl","sessions","admin"],"requests_per_minute":600,"max_concurrent":16}' \
    "${base}/v1/admin/keys")"
token="$(value "${created}" "d.token")"
entry='{ "pacing": "fast", "active_hours": "ignore", "min_dwell": "500ms" }'
cat > "${scratch}/sites.json" << EOF
{
  "round": "e2e-field",
  "draw_seed": "the e2e round's own seed",
  "since": "$(date -u -d '-1 minute' +%Y-%m-%dT%H:%M:%SZ)",
  "budget": { "pages_per_day": 6, "pages_per_site": 12, "days_per_site": 2, "pages_per_round": 20 },
  "stops": { "min_usable_proxies": 3, "max_term_share": 0.9, "max_round_bytes": 500000000 },
  "sites": [
    { "index": "1", "domain": "fixture.kage.test", "seed": "http://fixture.kage.test:8088/site/about",
      "category": "reading room", "country": "DE", "engine": null, "persona": null, "timezone": "UTC",
      "window": { "from": "00:00", "until": "23:59" }, "pages": 4, "max_depth": 2,
      "names": ["harbour"], "deny": ["(?i)^https?://lenient\\\\."], "entry": ${entry} },
    { "index": "2", "domain": "wall.field.test", "seed": "http://wall.field.test:8088/wall/",
      "category": "test barrier", "country": "DE", "engine": null, "persona": null, "timezone": "UTC",
      "window": { "from": "00:00", "until": "23:59" }, "pages": 3, "max_depth": 1, "entry": ${entry} },
    { "index": "C1", "domain": "rules.kage.test", "seed": "http://rules.kage.test:8088/site/",
      "category": "control, no defence", "country": "DE", "timezone": "UTC", "control": true,
      "window": { "from": "00:00", "until": "23:59" }, "pages": 1, "max_depth": 0, "entry": ${entry} }
  ]
}
EOF

echo "==> the draw and the round's policies"
field check 1 > "${scratch}/before-draw" 2>&1 || true
grep --quiet "has no persona: run \`assign\` first" "${scratch}/before-draw" \
    || fail "a site without a persona was not refused: $(cat "${scratch}/before-draw")"
field assign
sites="$(cat "${scratch}/sites.json")"
check "each site has a persona of its own, of the world's, with that persona's engine" "{\"sites\":${sites},\"personas\":${personas}}" \
    "(() => { const s = d.sites.sites.filter((x) => !x.control); const ids = s.map((x) => x.persona); return new Set(ids).size === 2 && s.every((x) => { const p = d.personas.personas.find((q) => q.id === x.persona); return p && p.engine === x.engine && p.country === x.country; }) && d.sites.draw.dealt.length === 2; })()"
field policies
call_policy="$(curl --silent --show-error --fail --header "Authorization: Bearer ${token}" "${base}/v1/policies/domains/fixture.kage.test")"
check "the round's entry is in force for site 1" "{\"view\":${call_policy},\"sites\":${sites}}" \
    "d.view.policy.robots === 'enforce' && d.view.min_level === 3 && JSON.stringify(d.view.policy.engines) === JSON.stringify([d.sites.sites[0].engine]) && JSON.stringify(d.view.policy.countries) === '[\"DE\"]' && d.view.policy.pacing_locked === true"
field check 1 | tee "${scratch}/check-1"
grep --quiet "the batch may begin: 4 pages" "${scratch}/check-1" || fail "the preflight of site 1 did not pass"

echo "==> site 1's batch: one persona, its robots.txt kept to, its deny list kept"
field run 1 | tee "${scratch}/run-1"
record1="$(cat "${private}/records/site-1-pass1.json")"
keep record-site-1.json "${record1}"
check "every page of site 1 was read by its persona alone, and a page was skipped for robots.txt" "${record1}" \
    "d.readers.length === 1 && d.readers[0] === d.persona.id && d.pages.length >= 2 && d.pages.every((p) => p.persona === d.persona.id && p.attempts === 1) && d.skipped.robots_denied >= 1"
check "the pages were reached as a reader reaches them, and the scout was not the reader" "${record1}" \
    "d.pages[0].reached === 'typed' && d.pages.slice(1).some((p) => p.reached === 'click' || p.reached === 'back_click') && d.scout && d.scout.is_reader === false"
check "the record has the runbook's rates and the bytes of the batch" "${record1}" \
    "d.rates.length >= 1 && d.rates.every((r) => r.level === 3 && r.engine === d.persona.engine) && d.bytes > 0 && d.decision.startsWith('keep')"
seen="$(got "http://fixture.kage.test:8088/__kage/requests")"
keep site-requests.json "${seen}"
check "the site saw its pages from one address, robots.txt from another, and never the disallowed page or the denied host" "${seen}" \
    "(() => { const pages = d.filter((r) => r.host.startsWith('fixture.kage.test') && r.path.startsWith('/site/')); const robots = d.filter((r) => r.host.startsWith('fixture.kage.test') && r.path === '/robots.txt'); const readers = new Set(pages.map((r) => r.from)); return pages.length >= 2 && readers.size === 1 && robots.length >= 1 && robots.every((r) => !readers.has(r.from)) && !d.some((r) => r.path.startsWith('/site/private')) && !d.some((r) => r.host.startsWith('lenient.')); })()"

echo "==> the same site again the same day is refused before anything is asked"
asked="$(value "${seen}" "d.length")"
if field run 1 > "${scratch}/run-1-again" 2>&1; then
    fail "a second batch of site 1 ran the same day"
fi
grep --quiet "budget: the site had its batch today" "${scratch}/run-1-again" || fail "the refusal does not say why: $(cat "${scratch}/run-1-again")"
[ "$(value "$(got "http://fixture.kage.test:8088/__kage/requests")" "d.filter((r) => r.host.startsWith('fixture.kage.test')).length")" \
    = "$(value "${seen}" "d.filter((r) => r.host.startsWith('fixture.kage.test')).length")" ] || fail "the refused batch asked the site for something"
echo "    refused, ${asked} requests at the site before and after"

echo "==> site 2's batch: a wall on the second page ends the day"
field run 2 | tee "${scratch}/run-2"
record2="$(cat "${private}/records/site-2-pass1.json")"
keep record-site-2.json "${record2}"
check "the wall was recorded, the persona not replaced, the batch ended by the runner at the wall" "${record2}" \
    "d.pages.length === 2 && d.pages[0].outcome === 'ok' && d.pages[1].outcome === 'challenge_failed' && d.pages.every((p) => p.attempts === 1 && p.persona === d.persona.id) && d.unread.every((p) => p.reason === 'cancelled') && d.blocks.length === 1 && d.blocks[0].vendor === 'cloudflare' && d.ended.startsWith('cancelled by the runner: a block') && d.decision.startsWith('pass 2')"
seen2="$(got "http://fixture.kage.test:8088/__kage/requests")"
keep site-requests-2.json "${seen2}"
check "the site saw the wall once and never the page that was left to read" "${seen2}" \
    "d.filter((r) => r.host.startsWith('wall.field.test') && r.path === '/challenge/stuck').length === 1 && !d.some((r) => r.path === '/wall/after')"

echo "==> the control, read by site 1's persona of the day"
reader="$(value "${record1}" "d.persona.id")"
field run C1 --persona "${reader}" | tee "${scratch}/run-c1"
control="$(cat "${private}/records/site-C1-pass1.json")"
check "the control was read by the persona named for it, and nothing blocked it" "${control}" \
    "d.readers.length === 1 && d.readers[0] === d.persona.id && d.pages.length === 1 && d.pages[0].outcome === 'ok' && d.decision === 'control read as expected'"
[ "$(value "${control}" "d.persona.id")" = "${reader}" ] || fail "the control was not read by the persona named for it"

echo "==> the report: no host, no address, no id; the private record has them"
field warmup
field report "${scratch}/report.md" --title "Field report (e2e)"
grep --quiet '^## Warm-up (F85)' "${scratch}/report.md" || fail "the report has no warm-up section"
grep --quiet 'In all 0 warm-up visits made and 0 skipped by 3 personas' "${scratch}/report.md" \
    || fail "the warm-up section does not count a stack without warm-up: $(grep -A 12 'Warm-up' "${scratch}/report.md")"
report="$(cat "${scratch}/report.md")"
keep report.md "${report}"
for word in kage.test fixture wall.field field.test lenient harbour; do
    if grep --quiet --ignore-case "${word}" "${scratch}/report.md"; then
        fail "the report names ${word}"
    fi
done
grep --quiet --extended-regexp '([0-9]{1,3}\.){3}[0-9]{1,3}' "${scratch}/report.md" && fail "the report holds an address"
grep --quiet '^## Site 1: DE reading room, ' "${scratch}/report.md" || fail "no section for site 1: ${report}"
grep --quiet '^## Site 2: DE test barrier, cloudflare' "${scratch}/report.md" || fail "no section for site 2: ${report}"
grep --quiet 'fixture.kage.test' "${private}/records/site-1-pass1.json" || fail "the private record does not name what it read"
if grep --recursive --quiet --extended-regexp '"from": "([0-9]{1,3}\.){3}[0-9]{1,3}"|kage_[0-9a-f]{32}_' "${private}"; then
    fail "the private evidence holds an address or a token"
fi
echo "    $(grep -c '^## Site' "${scratch}/report.md") sections, nothing of the sites in them"

echo "field test passed"
