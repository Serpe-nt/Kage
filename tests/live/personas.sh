#!/usr/bin/env bash
# Live check of the persona manager (spec §6, §7.4): generates the personas
# of the real pool into a throwaway database, then runs `kage persona
# selftest --third-party` for one Chrome and one Firefox persona through
# their own proxies and judges the reports with the recorded baselines of
# the engines' own live checks.
#
# What it talks to: the Webshare API, the provider's echo and the geo
# lookup through every proxy (as `kage serve` does), and the third-party
# detection pages through two proxies. Never a target site, and warm-up is
# off, so no persona visits anything by itself.
#
# It is not part of the nightly run: it shows two more exit addresses to
# the third-party pages. Run it through tests/live/run.sh with
# KAGE_LIVE_PERSONAS=1.
#
# Environment (besides the secrets run.sh checks):
#   KAGE_LIVE_EVIDENCE_DIR  keep the reports and the persona list here
#   KAGE_LIVE_PERSONA_WAIT  seconds to wait for the personas (default 480)
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "${KAGE_LIVE:-}" != "1" ]; then
    echo "error: this is a live check; run it through tests/live/run.sh" >&2
    exit 1
fi

postgres_image="postgres:17.11-trixie@sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3"
postgres="kage-live-personas-db-$$"
sidecars=()
server=""
workdir="$(mktemp -d)"
wait_seconds="${KAGE_LIVE_PERSONA_WAIT:-480}"

cleanup() {
    if [ -n "${server}" ]; then
        kill "${server}" 2> /dev/null || true
        wait "${server}" 2> /dev/null || true
    fi
    docker rm --force --volumes "${postgres}" "${sidecars[@]}" > /dev/null 2>&1 || true
    rm -rf "${workdir}"
}
trap cleanup EXIT

fail() {
    echo "error: $*" >&2
    exit 1
}

image_of() {
    case "$1" in
        chromium) echo "${KAGE_BROWSER_CHROMIUM_IMAGE:-kage-browser-chromium:dev}" ;;
        firefox) echo "${KAGE_BROWSER_FIREFOX_IMAGE:-kage-browser-firefox:dev}" ;;
    esac
}

for engine in chromium firefox; do
    image="$(image_of "${engine}")"
    if ! docker image inspect "${image}" > /dev/null 2>&1; then
        echo "    building ${image}"
        docker build --quiet --file deploy/docker/sidecar.Dockerfile --build-arg "ENGINE=${engine}" \
            --tag "${image}" . > /dev/null
    fi
done

echo "    starting a throwaway Postgres"
password="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
docker run --detach --name "${postgres}" \
    --env "POSTGRES_PASSWORD=${password}" --publish 127.0.0.1::5432 "${postgres_image}" > /dev/null
postgres_port="$(docker port "${postgres}" 5432/tcp | head -n 1 | sed 's/.*://')"

wait_for() {
    for _ in $(seq 1 60); do
        if "$@" > /dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    return 1
}
wait_for docker exec "${postgres}" pg_isready --host 127.0.0.1 --username postgres \
    || fail "Postgres did not become ready"

export KAGE__DATABASE__URL="postgres://postgres:${password}@127.0.0.1:${postgres_port}/postgres"
export KAGE__SIDECAR__TOKEN="${token}"
export KAGE__PROXIES__PROVIDER=webshare
# This check never replaces a proxy, and no persona visits a site.
export KAGE__PROXIES__REPLACEMENTS_PER_MONTH=0
export KAGE__PERSONAS__WARMUP__ENABLED=false
# kage does not see the sidecars' volumes here; nothing is retired anyway.
export KAGE__PERSONAS__DIR="${workdir}/personas"
unset KAGE_CONFIG

start_sidecar() {
    local engine="$1" name port
    local hardening=(--init --read-only --cap-drop ALL --security-opt no-new-privileges)
    if [ "${engine}" = chromium ]; then
        hardening+=(--security-opt "seccomp=deploy/seccomp/chromium.json")
    fi
    name="kage-live-personas-${engine}-$$"
    sidecars+=("${name}")
    docker run --detach --name "${name}" "${hardening[@]}" \
        --shm-size 1g --tmpfs /tmp --pids-limit 4096 --memory 4g \
        --volume /var/lib/kage/personas --volume /var/lib/kage/artifacts \
        --env "ENGINE=${engine}" --env "SIDECAR_TOKEN=${token}" --env SIDECAR_MAX_CONTEXTS=1 \
        --publish 127.0.0.1::8090 "$(image_of "${engine}")" > /dev/null
    wait_for docker exec "${name}" node dist/healthcheck.js \
        || fail "the ${engine} sidecar did not become ready"
    port="$(docker port "${name}" 8090/tcp | head -n 1 | sed 's/.*://')"
    case "${engine}" in
        chromium) export KAGE__SIDECAR__CHROMIUM_URL="http://127.0.0.1:${port}" ;;
        firefox) export KAGE__SIDECAR__FIREFOX_URL="http://127.0.0.1:${port}" ;;
    esac
}
echo "    starting both sidecars"
start_sidecar chromium
start_sidecar firefox

echo "    building kage"
cargo build --quiet --locked --package kage
kage_bin="$(cargo metadata --format-version 1 --no-deps --locked \
    | node -e "let text = ''; process.stdin.on('data', (chunk) => (text += chunk)).on('end', () => console.log(JSON.parse(text).target_directory));")/debug/kage"
[ -x "${kage_bin}" ] || fail "the kage binary is not at ${kage_bin}"
kage() {
    "${kage_bin}" "$@"
}

echo "    applying the migrations"
kage migrate > /dev/null

# The server syncs the pool, probes every proxy and generates the personas,
# exactly as in production. Nothing calls its API.
echo "    starting kage serve; waiting up to ${wait_seconds} s for the personas"
kage serve --bind 127.0.0.1:0 > "${workdir}/serve.log" 2>&1 &
server=$!

count() {
    node -e "
        const { readFileSync } = require('node:fs');
        const document = JSON.parse(readFileSync(process.argv[1], 'utf8'));
        console.log(document[process.argv[2]].filter((item) => process.argv.slice(3).includes(item.state)).length);
    " "$@"
}
personas=0
usable=-1
deadline=$(($(date +%s) + wait_seconds))
while [ "$(date +%s)" -lt "${deadline}" ]; do
    kill -0 "${server}" 2> /dev/null || {
        tail -n 40 "${workdir}/serve.log" >&2
        fail "kage serve stopped"
    }
    sleep 10
    kage proxy list --json > "${workdir}/proxies.json" 2> /dev/null || continue
    kage persona list --json > "${workdir}/personas.json" 2> /dev/null || continue
    usable="$(count "${workdir}/proxies.json" proxies healthy degraded)"
    waiting="$(count "${workdir}/proxies.json" proxies new)"
    personas="$(count "${workdir}/personas.json" personas active cooling)"
    echo "    ${personas} personas for ${usable} usable proxies (${waiting} not probed yet)"
    if [ "${usable}" -gt 0 ] && [ "${waiting}" -eq 0 ] && [ "${personas}" -ge "${usable}" ]; then
        break
    fi
done
kill "${server}" 2> /dev/null || true
wait "${server}" 2> /dev/null || true
server=""
[ "${usable}" -gt 0 ] || fail "no proxy became usable; see docs/runbooks/proxies.md"
[ "${personas}" -eq "${usable}" ] || {
    grep -E '"level":"(WARN|ERROR)"' "${workdir}/serve.log" | tail -n 20 \
        | sed -E 's/([0-9]{1,3}\.){3}[0-9]{1,3}/[address]/g' >&2 || true
    fail "${personas} personas were generated for ${usable} usable proxies"
}

echo "==> the personas of the pool"
kage persona list | tee "${workdir}/personas.txt"
# Every persona is where its proxy was observed to be, the mix is within
# two personas of the configured shares, and no two share a seed.
node tests/live/check-personas.mjs "${workdir}/personas.json" "${workdir}/proxies.json" \
    > "${workdir}/pair.txt" || fail "the generated personas are not what the configuration asks for"

if [ -n "${KAGE_LIVE_EVIDENCE_DIR:-}" ]; then
    mkdir -p "${KAGE_LIVE_EVIDENCE_DIR}"
    # Ids, engines, places and screens; no credentials and no addresses.
    node -e "
        const { readFileSync, writeFileSync } = require('node:fs');
        const document = JSON.parse(readFileSync(process.argv[1], 'utf8'));
        for (const persona of document.personas) { delete persona.user_agent; delete persona.geolocation; }
        writeFileSync(process.argv[2], JSON.stringify(document, null, 2) + '\n');
    " "${workdir}/personas.json" "${KAGE_LIVE_EVIDENCE_DIR}/personas.json"
    cp "${workdir}/personas.txt" "${KAGE_LIVE_EVIDENCE_DIR}/personas.txt"
fi

baseline_dir="tests/fixtures/selftest"
failed=()
while read -r engine persona; do
    echo "==> kage persona selftest --third-party for the ${engine} persona ${persona}"
    status=0
    # A failing self-test still prints its report; the judge reads it.
    # stdin is the list of the loop: the command must not read from it.
    kage persona selftest "${persona}" --third-party --json \
        < /dev/null > "${workdir}/${engine}-selftest.json" || status=1
    # The judge of the engines' live checks reads a self-test report with
    # the engine and the browser beside it.
    node -e "
        const { readFileSync, writeFileSync } = require('node:fs');
        const document = JSON.parse(readFileSync(process.argv[1], 'utf8'));
        const run = document.browser;
        if (!run || !run.report) { console.error('error: the browser self-test did not run: ' + JSON.stringify(run)); process.exit(1); }
        const persona = document.persona;
        writeFileSync(process.argv[2], JSON.stringify({
            ...run.report,
            engine: persona.engine,
            persona: {
                browser_major: persona.browser_major,
                browser_version: String(persona.browser_major),
                claimed_os: persona.claimed_os,
            },
        }));
    " "${workdir}/${engine}-selftest.json" "${workdir}/${engine}-report.json" || {
        failed+=("${engine}")
        continue
    }
    # What the judge prints holds no address: it replaces anything that
    # looks like one in what a page showed. The report itself is not kept.
    judged=0
    node tests/live/check-browser-selftest.mjs "${workdir}/${engine}-report.json" "${baseline_dir}" \
        > "${workdir}/${engine}-judged.txt" 2>&1 || judged=1
    cat "${workdir}/${engine}-judged.txt"
    if [ -n "${KAGE_LIVE_EVIDENCE_DIR:-}" ]; then
        cp "${workdir}/${engine}-judged.txt" "${KAGE_LIVE_EVIDENCE_DIR}/${engine}-judged.txt"
    fi
    if [ "${judged}" -ne 0 ]; then
        echo "error: the ${engine} persona did not pass" >&2
        failed+=("${engine}")
    elif [ "${status}" -ne 0 ]; then
        echo "error: the self-test command of the ${engine} persona failed; its log is above" >&2
        failed+=("${engine}")
    fi
done < "${workdir}/pair.txt"

[ "${#failed[@]}" -eq 0 ] || fail "the persona self-test did not pass for: ${failed[*]}"
echo "    the personas of the pool were generated and one of each engine passed its self-test"
