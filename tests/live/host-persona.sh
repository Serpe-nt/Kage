#!/usr/bin/env bash
# Live check of the host persona (spec §25, ADR 0012): under
# `egress.mode: direct`, `kage serve` places the host's one persona where a
# lookup of this machine's own address finds it and generates it; then
# `kage persona selftest --third-party` runs it against the two detection
# pages, judged with the baselines of the engines' own live checks. One run
# per engine, each on a throwaway database with one host persona of that
# engine only.
#
# What it talks to, all from this machine's own address: the host's address
# echo (`proxies.health.host_ip_url`, by default https://api.ipify.org/) and
# the geo lookup (`proxies.health.geo_url`, by default
# https://ipinfo.io/json), once per run; the pages of
# `selftest.browser.third_party_pages` (bot.sannysoft.com and CreepJS) and
# what they load, once per engine; and the browser's own background traffic
# while it runs (docs/runbooks/engine-chromium.md). No proxy provider, no
# target site, no robots.txt, no webhook; warm-up is off, so the persona
# visits nothing by itself.
#
# It shows the operator's own address to those parties, so it runs only on
# the operator's word: never in the nightly run, never through run.sh
# unless asked. Run it as
#
#   KAGE_LIVE=1 KAGE_LIVE_HOST=1 tests/live/host-persona.sh
#
# Environment:
#   KAGE_LIVE_ENGINES       the engines, one run each (default `chromium firefox`)
#   KAGE_LIVE_EVIDENCE_DIR  keep the judged reports and the persona here,
#                           with no address, city or position
#   KAGE_LIVE_PERSONA_WAIT  seconds to wait for the persona (default 300)
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "${KAGE_LIVE:-}" != "1" ] || [ "${KAGE_LIVE_HOST:-}" != "1" ]; then
    echo "error: this live check shows this machine's address to third parties;" >&2
    echo "       run it with KAGE_LIVE=1 KAGE_LIVE_HOST=1 on the operator's word" >&2
    exit 1
fi

postgres_image="postgres:17.11-trixie@sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3"
postgres=""
sidecars=()
server=""
workdir="$(mktemp -d)"
wait_seconds="${KAGE_LIVE_PERSONA_WAIT:-300}"
engines="${KAGE_LIVE_ENGINES:-chromium firefox}"

cleanup() {
    if [ -n "${server}" ]; then
        kill "${server}" 2> /dev/null || true
        wait "${server}" 2> /dev/null || true
    fi
    docker rm --force --volumes ${postgres:+"${postgres}"} "${sidecars[@]}" > /dev/null 2>&1 || true
    rm -rf "${workdir}"
}
trap cleanup EXIT

fail() {
    echo "error: $*" >&2
    exit 1
}
random_hex() {
    head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'
}
wait_for() {
    for _ in $(seq 1 60); do
        if "$@" > /dev/null 2>&1; then
            return 0
        fi
        sleep 1
    done
    return 1
}
# What a log line of the server says, without the host's address.
scrubbed() {
    sed -E -e 's/([0-9]{1,3}\.){3}[0-9]{1,3}/[address]/g' \
        -e 's/\b([0-9a-f]{1,4}:){3,7}[0-9a-f]{1,4}\b/[address]/gi'
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

# The sidecars reach the forwarder on the Docker bridge of this machine, by
# the name the development compose file gives the host (as tests/live/jobs.sh).
bridge="$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}')"
[ -n "${bridge}" ] || fail "the Docker bridge has no gateway address"
token="$(random_hex 32)"

# Nothing of a provider: the pool is not used under direct, and this check
# names none.
unset KAGE__WEBSHARE__API_KEY KAGE__WEBSHARE__API_KEY_FILE \
    KAGE__PROXIES__STATIC_LIST KAGE__PROXIES__STATIC_LIST_FILE \
    KAGE__REPUTATION__ABUSEIPDB_KEY KAGE__REPUTATION__ABUSEIPDB_KEY_FILE KAGE_CONFIG
export KAGE__SIDECAR__TOKEN="${token}"
export KAGE__SECRETS__MASTER_KEY="${KAGE__SECRETS__MASTER_KEY:-$(random_hex 32)}"
export KAGE__EGRESS__MODE=direct
export KAGE__EGRESS__DIRECT__PERSONAS=1
# The persona is placed by the lookup of the host's address: no country is
# declared.
unset KAGE__EGRESS__DIRECT__COUNTRY KAGE__EGRESS__DIRECT__LOCALE KAGE__EGRESS__DIRECT__TIMEZONE
export KAGE__PERSONAS__WARMUP__ENABLED=false
export KAGE__PERSONAS__DIR="${workdir}/personas"
export KAGE__FORWARDER__BIND="${bridge}:8899"
export KAGE__FORWARDER__ADVERTISE="http://host.docker.internal:8899"

start_sidecar() {
    local engine="$1" name port
    local hardening=(--init --read-only --cap-drop ALL --security-opt no-new-privileges)
    if [ "${engine}" = chromium ]; then
        hardening+=(--security-opt "seccomp=deploy/seccomp/chromium.json")
    fi
    name="kage-live-host-${engine}-$$"
    sidecars+=("${name}")
    docker run --detach --name "${name}" "${hardening[@]}" \
        --shm-size 1g --tmpfs /tmp --pids-limit 4096 --memory 4g \
        --add-host host.docker.internal:host-gateway \
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

baseline_dir="tests/fixtures/selftest"
failed=()
for engine in ${engines}; do
    echo "==> the host persona of ${engine}"
    postgres="kage-live-host-db-${engine}-$$"
    password="$(random_hex 16)"
    docker run --detach --name "${postgres}" \
        --env "POSTGRES_PASSWORD=${password}" --publish 127.0.0.1::5432 "${postgres_image}" > /dev/null
    postgres_port="$(docker port "${postgres}" 5432/tcp | head -n 1 | sed 's/.*://')"
    wait_for docker exec "${postgres}" pg_isready --host 127.0.0.1 --username postgres \
        || fail "Postgres did not become ready"
    export KAGE__DATABASE__URL="postgres://postgres:${password}@127.0.0.1:${postgres_port}/postgres"
    export KAGE__EGRESS__DIRECT__ENGINES="[\"${engine}\"]"
    kage migrate > /dev/null

    # The server reads the host's address, looks it up, and generates the
    # persona, exactly as in production. Nothing calls its API.
    echo "    starting kage serve; waiting up to ${wait_seconds} s for the host persona"
    # The binary itself, not the function: `$!` must be the server, or a
    # stop leaves it running with the forwarder's port (as tests/live/jobs.sh).
    "${kage_bin}" serve --bind 127.0.0.1:0 > "${workdir}/serve-${engine}.log" 2>&1 &
    server=$!
    persona=""
    deadline=$(($(date +%s) + wait_seconds))
    while [ "$(date +%s)" -lt "${deadline}" ]; do
        kill -0 "${server}" 2> /dev/null || {
            # What it said last, whatever the form (an error of the start is
            # no JSON line), without an address.
            tail -n 30 "${workdir}/serve-${engine}.log" | scrubbed >&2 || true
            fail "kage serve stopped"
        }
        sleep 5
        kage persona list --json > "${workdir}/personas-${engine}.json" 2> /dev/null || continue
        persona="$(node -e "
            const d = JSON.parse(require('node:fs').readFileSync(process.argv[1], 'utf8'));
            const ready = d.personas.filter((p) => p.ready && p.state === 'active');
            console.log(ready.length === 1 ? ready[0].id : '');
        " "${workdir}/personas-${engine}.json")"
        [ -z "${persona}" ] || break
    done
    kill "${server}" 2> /dev/null || true
    wait "${server}" 2> /dev/null || true
    server=""
    [ -n "${persona}" ] || {
        grep -E '"level":"(WARN|ERROR)"' "${workdir}/serve-${engine}.log" | tail -n 20 | scrubbed >&2 || true
        fail "no host persona of ${engine} became ready in ${wait_seconds} s"
    }
    # The one persona, of this engine, on the host's own row.
    node -e "
        const d = JSON.parse(require('node:fs').readFileSync(process.argv[1], 'utf8'));
        const p = d.personas[0];
        if (d.personas.length !== 1 || p.engine !== process.argv[2]) {
            console.error('error: expected one ' + process.argv[2] + ' persona, got ' + d.personas.map((x) => x.engine).join(', '));
            process.exit(1);
        }
        console.log('    persona ' + p.id + ': ' + p.engine + ' claiming ' + p.claimed_os + ', ' + p.country + ' ' + p.locale + ' ' + p.timezone + ', languages ' + p.languages.join(' '));
    " "${workdir}/personas-${engine}.json" "${engine}" || fail "the host persona is not what this run asked for"
    kage proxy list --json > "${workdir}/proxies-${engine}.json"
    node -e "
        const d = JSON.parse(require('node:fs').readFileSync(process.argv[1], 'utf8'));
        if (d.proxies.length !== 1 || d.proxies[0].provider !== 'host') {
            console.error('error: the pool holds more than the host row');
            process.exit(1);
        }
    " "${workdir}/proxies-${engine}.json" || fail "the pool is not the host's one row"

    echo "    kage persona selftest --third-party"
    status=0
    kage persona selftest "${persona}" --third-party --json \
        < /dev/null > "${workdir}/${engine}-selftest.json" || status=1
    # The judge of the engines' live checks reads a self-test report with
    # the engine and the browser beside it (as tests/live/personas.sh).
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
        docker rm --force --volumes "${postgres}" > /dev/null 2>&1 || true
        postgres=""
        continue
    }
    judged=0
    node tests/live/check-browser-selftest.mjs "${workdir}/${engine}-report.json" "${baseline_dir}" \
        > "${workdir}/${engine}-judged.txt" 2>&1 || judged=1
    cat "${workdir}/${engine}-judged.txt"
    if [ -n "${KAGE_LIVE_EVIDENCE_DIR:-}" ]; then
        mkdir -p "${KAGE_LIVE_EVIDENCE_DIR}"
        cp "${workdir}/${engine}-judged.txt" "${KAGE_LIVE_EVIDENCE_DIR}/${engine}-judged.txt"
        # Engine, claimed system, country, locale, zone, languages and
        # screen; never the user agent's build, the city or the position.
        node -e "
            const { readFileSync, writeFileSync } = require('node:fs');
            const d = JSON.parse(readFileSync(process.argv[1], 'utf8'));
            for (const p of d.personas) { delete p.user_agent; delete p.geolocation; delete p.city; }
            writeFileSync(process.argv[2], JSON.stringify(d, null, 2) + '\n');
        " "${workdir}/personas-${engine}.json" "${KAGE_LIVE_EVIDENCE_DIR}/${engine}-persona.json"
    fi
    if [ "${judged}" -ne 0 ]; then
        echo "error: the ${engine} host persona did not pass" >&2
        failed+=("${engine}")
    elif [ "${status}" -ne 0 ]; then
        echo "error: the self-test command of the ${engine} host persona failed" >&2
        failed+=("${engine}")
    fi
    docker rm --force --volumes "${postgres}" > /dev/null 2>&1 || true
    postgres=""
done

[ "${#failed[@]}" -eq 0 ] || fail "the host persona's self-test did not pass for: ${failed[*]}"
echo "    the host persona of every engine passed its self-test from this machine's own address"
