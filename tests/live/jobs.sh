#!/usr/bin/env bash
# Live check of jobs (spec §16.6, §10.5, §8.4): `kage serve` against the
# real pool, with real personas of both engines, at the shipped `human`
# pace, through the public API and nothing else.
#
#   a. a synchronous fetch of one detection page in each engine, with a
#      screenshot;
#   b. a batch of the two detection pages, each twice, with the default one
#      visitor per site;
#   c. a flow in each engine: a page, a wait, a scroll, a screenshot, the
#      other page, a wait, the keyboard's way back, a wait for the address,
#      a screenshot;
#   d. the events of the batch and of one flow at a receiver on this
#      machine's loopback that verifies their signatures, answers the first
#      delivery 503 and the others 204;
#   e. a synchronous fetch that outlasts a short wall time and is carried
#      on as a job, with `kage serve` stopped and started while it runs,
#      and its result read under the same request id afterwards.
#
# What it talks to: the Webshare API, the provider's echo and the geo
# lookup through every proxy (as `kage serve` does), and the two third-party
# detection pages (bot.sannysoft.com, CreepJS) through the proxies of a few
# personas: about a dozen page loads in all. Never a target site; warm-up
# is off, so no persona visits anything by itself; no proxy is replaced.
#
# It is not part of the nightly run: it shows more exit addresses to the
# third-party pages than the nightly checks do. Run it through
# tests/live/run.sh with KAGE_LIVE_JOBS=1.
#
# Environment (besides the secrets run.sh checks):
#   KAGE_LIVE_EVIDENCE_DIR        keep what the judge writes here
#   KAGE_LIVE_PERSONA_WAIT        seconds to wait for the personas (default 480)
#   KAGE_LIVE_JOBS_ACTIVE_HOURS   `ignore`: the hour of the personas' day is
#                                 not looked at for the two pages. A job
#                                 never wakes a persona, so at night in the
#                                 pool's countries the batch and the flows
#                                 wait for the morning unless this is set.
#                                 The pace itself stays the shipped one.
#   KAGE_LIVE_JOBS_ONLY           `flows`: only the two flows and the event
#                                 of one (four page loads in all), to look
#                                 at a flow again without the rest.
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "${KAGE_LIVE:-}" != "1" ]; then
    echo "error: this is a live check; run it through tests/live/run.sh" >&2
    exit 1
fi

postgres_image="postgres:17.11-trixie@sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3"
postgres="kage-live-jobs-db-$$"
sidecars=()
server=""
receiver=""
workdir="$(mktemp -d)"
wait_seconds="${KAGE_LIVE_PERSONA_WAIT:-480}"
rehearsal="${KAGE_LIVE_JOBS_REHEARSAL:-}"

cleanup() {
    local status=$?
    for process in "${server}" "${receiver}"; do
        if [ -n "${process}" ]; then
            kill "${process}" 2> /dev/null || true
            wait "${process}" 2> /dev/null || true
        fi
    done
    if [ "${status}" -ne 0 ]; then
        # What the server said, with anything that looks like an address
        # replaced.
        for log in "${workdir}"/serve-*.log; do
            [ -f "${log}" ] || continue
            echo "--- ${log##*/}: warnings and errors ---" >&2
            grep -E '"level":"(WARN|ERROR)"' "${log}" | tail -n 25 \
                | sed -E 's/([0-9]{1,3}\.){3}[0-9]{1,3}/[address]/g' >&2 || true
        done
        for sidecar in "${sidecars[@]}"; do
            echo "--- ${sidecar%-*}: warnings and errors ---" >&2
            docker logs "${sidecar}" 2>&1 | grep -E '"level":(40|50|60)' | tail -n 15 \
                | sed -E 's/([0-9]{1,3}\.){3}[0-9]{1,3}/[address]/g' >&2 || true
        done
    fi
    # What a sidecar wrote into the shared directory is its user's: it
    # removes it itself.
    for sidecar in "${sidecars[@]}"; do
        docker exec "${sidecar}" node -e \
            "require('node:fs').rmSync('/var/lib/kage/artifacts/sidecar', { recursive: true, force: true })" \
            > /dev/null 2>&1 || true
    done
    docker rm --force --volumes "${postgres}" "${sidecars[@]}" > /dev/null 2>&1 || true
    rm -rf "${workdir}" 2> /dev/null || true
    exit "${status}"
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

free_port() {
    node -e "const s = require('node:net').createServer().listen(0, '127.0.0.1', () => { console.log(s.address().port); s.close(); });"
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

echo "    starting a throwaway Postgres"
password="$(random_hex 16)"
token="$(random_hex 32)"
docker run --detach --name "${postgres}" \
    --env "POSTGRES_PASSWORD=${password}" --publish 127.0.0.1::5432 "${postgres_image}" > /dev/null
postgres_port="$(docker port "${postgres}" 5432/tcp | head -n 1 | sed 's/.*://')"
wait_for docker exec "${postgres}" pg_isready --host 127.0.0.1 --username postgres \
    || fail "Postgres did not become ready"

# The sidecars reach the forwarder on the Docker bridge of this machine, by
# the name the development compose file gives the host; a browser's files
# reach kage through a directory both see.
bridge="$(docker network inspect bridge --format '{{(index .IPAM.Config 0).Gateway}}')"
[ -n "${bridge}" ] || fail "the Docker bridge has no gateway address"
artifacts="${workdir}/artifacts"
mkdir -p "${artifacts}"
# A sidecar writes its files for its own user and group only. With the
# set-group-ID bit what it creates here belongs to this user's group, so
# that kage, which runs as this user and not in a container, can read it.
chmod 2777 "${artifacts}"
api_port="$(free_port)"
hook_port="$(free_port)"
hook_origin="http://127.0.0.1:${hook_port}"

export KAGE__DATABASE__URL="postgres://postgres:${password}@127.0.0.1:${postgres_port}/postgres"
export KAGE__SIDECAR__TOKEN="${token}"
if [ "${rehearsal}" != "1" ]; then
    export KAGE__PROXIES__PROVIDER=webshare
fi
# This check never replaces a proxy, and no persona visits a site by itself.
export KAGE__PROXIES__REPLACEMENTS_PER_MONTH=0
export KAGE__PERSONAS__WARMUP__ENABLED=false
# kage does not see the sidecars' profile volumes here; nothing is retired.
export KAGE__PERSONAS__DIR="${workdir}/personas"
export KAGE__ARTIFACTS__DIR="${artifacts}"
export KAGE__FORWARDER__BIND="${bridge}:8899"
export KAGE__FORWARDER__ADVERTISE="http://host.docker.internal:8899"
# The one receiver a job may name: this machine's own, on its loopback.
export KAGE__WEBHOOKS__ALLOWED_ORIGINS="[\"${hook_origin}\"]"
export KAGE__WEBHOOKS__PRIVATE_ORIGINS="[\"${hook_origin}\"]"
unset KAGE_CONFIG

# The shipped policies: the pace of the check is the `human` profile.
if [ "${KAGE_LIVE_JOBS_ACTIVE_HOURS:-}" = "ignore" ]; then
    policies="${workdir}/policies"
    mkdir -p "${policies}"
    cp policies/*.yaml "${policies}/"
    cat >> "${policies}/domains.yaml" << 'EOF'

# Added by tests/live/jobs.sh (KAGE_LIVE_JOBS_ACTIVE_HOURS=ignore): the hour
# of a persona's day is not looked at for the two detection pages.
domains:
  bot.sannysoft.com:
    active_hours: ignore
  abrahamjuliot.github.io:
    active_hours: ignore
EOF
    # A `domains: {}` of the shipped file would be a second key of that name.
    if [ "$(grep --count '^domains:' "${policies}/domains.yaml")" -ne 1 ]; then
        grep --invert-match '^domains: {}$' "${policies}/domains.yaml" > "${policies}/domains.tmp"
        mv "${policies}/domains.tmp" "${policies}/domains.yaml"
    fi
    export KAGE__POLICIES__DIR="${policies}"
    echo "    the hour of the personas' day is not looked at for the two pages"
elif [ -n "${KAGE_LIVE_JOBS_POLICIES:-}" ] && [ "${rehearsal}" = "1" ]; then
    export KAGE__POLICIES__DIR="${KAGE_LIVE_JOBS_POLICIES}"
fi

start_sidecar() {
    local engine="$1" name port
    local hardening=(--init --read-only --cap-drop ALL --security-opt no-new-privileges)
    if [ "${engine}" = chromium ]; then
        hardening+=(--security-opt "seccomp=deploy/seccomp/chromium.json")
    fi
    name="kage-live-jobs-${engine}-$$"
    sidecars+=("${name}")
    docker run --detach --name "${name}" "${hardening[@]}" \
        --shm-size 1g --tmpfs /tmp --pids-limit 4096 --memory 4g \
        --add-host host.docker.internal:host-gateway \
        --volume /var/lib/kage/personas --volume "${artifacts}:/var/lib/kage/artifacts" \
        --env "ENGINE=${engine}" --env "SIDECAR_TOKEN=${token}" --env SIDECAR_MAX_CONTEXTS=3 \
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

echo "    starting the receiver of webhooks on the loopback"
KAGE_LIVE_HOOK_SECRET="$(random_hex 24)"
export KAGE_LIVE_HOOK_SECRET
node tests/live/hooks-receiver.mjs --port "${hook_port}" --fail-first 1 &
receiver=$!
wait_for curl --silent --fail --max-time 2 "${hook_origin}/__deliveries" \
    || fail "the receiver did not start"

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

base="http://127.0.0.1:${api_port}"
servers=0
# serve: starts `kage serve` and waits until it is ready.
serve() {
    servers=$((servers + 1))
    # The binary itself, not the shell function: the process that is stopped
    # later must be the server and not a shell around it.
    "${kage_bin}" serve --bind "127.0.0.1:${api_port}" > "${workdir}/serve-${servers}.log" 2>&1 &
    server=$!
    for _ in $(seq 1 120); do
        kill -0 "${server}" 2> /dev/null || {
            tail -n 40 "${workdir}/serve-${servers}.log" | sed -E 's/([0-9]{1,3}\.){3}[0-9]{1,3}/[address]/g' >&2
            fail "kage serve stopped"
        }
        if [ "${1:-}" = "ready" ]; then
            [ "$(curl --silent --output /dev/null --write-out '%{http_code}' --max-time 3 "${base}/readyz" || true)" = 200 ] && return 0
        else
            curl --silent --fail --output /dev/null --max-time 3 "${base}/healthz" && return 0
        fi
        sleep 2
    done
    fail "kage serve did not answer"
}
# stop_server: the way an operator stops it, and waits until it has.
stop_server() {
    local began
    began="$(date +%s)"
    kill -TERM "${server}" 2> /dev/null || true
    for _ in $(seq 1 120); do
        kill -0 "${server}" 2> /dev/null || break
        sleep 1
    done
    if kill -0 "${server}" 2> /dev/null; then
        echo "    kage serve did not stop within two minutes of SIGTERM; killing it" >&2
        kill -KILL "${server}" 2> /dev/null || true
    fi
    wait "${server}" 2> /dev/null || true
    server=""
    echo "    kage serve stopped after $(($(date +%s) - began)) s"
}

count() {
    node -e "
        const { readFileSync } = require('node:fs');
        const document = JSON.parse(readFileSync(process.argv[1], 'utf8'));
        console.log(document[process.argv[2]].filter((item) => process.argv.slice(3).includes(item.state)).length);
    " "$@"
}

# The server syncs the pool, probes every proxy and generates the personas,
# exactly as in production.
echo "    starting kage serve; waiting up to ${wait_seconds} s for the personas"
serve
personas=0
usable=-1
deadline=$(($(date +%s) + wait_seconds))
while [ "$(date +%s)" -lt "${deadline}" ]; do
    kill -0 "${server}" 2> /dev/null || fail "kage serve stopped"
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
[ "${usable}" -gt 0 ] || fail "no proxy became usable; see docs/runbooks/proxies.md"
[ "${personas}" -eq "${usable}" ] || fail "${personas} personas were generated for ${usable} usable proxies"
node -e "
    const { readFileSync } = require('node:fs');
    const personas = JSON.parse(readFileSync(process.argv[1], 'utf8')).personas.filter((p) => p.state !== 'retired');
    const by = (engine) => personas.filter((p) => p.engine === engine).length;
    if (by('chromium') === 0 || by('firefox') === 0) { console.error('error: the pool has no persona of one engine'); process.exit(1); }
    console.log('    ' + personas.length + ' personas: ' + by('chromium') + ' chromium, ' + by('firefox') + ' firefox');
" "${workdir}/personas.json"

# Keys are made at the command line; their tokens stay in this shell.
token_of() {
    node -e "let text = ''; process.stdin.on('data', (chunk) => (text += chunk)).on('end', () => console.log(JSON.parse(text).token));"
}
KAGE_API_KEY="$(kage key create --name live-jobs --scope fetch --scope jobs --scope sessions --json | token_of)"
KAGE_ADMIN_KEY="$(kage key create --name live-jobs-operator --scope admin --json | token_of)"
export KAGE_API_KEY KAGE_ADMIN_KEY
export KAGE_API_URL="${base}"
export KAGE_LIVE_HOOK_URL="${hook_origin}/kage"

node tests/live/check-jobs.mjs snapshot "${workdir}" before
node tests/live/check-jobs.mjs run "${workdir}"
node tests/live/check-jobs.mjs snapshot "${workdir}" after

if [ "${KAGE_LIVE_JOBS_ONLY:-}" = "flows" ]; then
    stop_server
    node tests/live/check-jobs.mjs judge "${workdir}" ${KAGE_LIVE_EVIDENCE_DIR:+"${KAGE_LIVE_EVIDENCE_DIR}"} \
        || fail "the live check of the flows did not pass"
    echo "    a flow ran in each engine against the real pool at the shipped pace"
    exit 0
fi

# (e) The same server with a wall time no page can make (a persona stays
# at least the policy's `min_dwell` of two seconds on a page): the fetch is
# answered 202 and carried on as a job. The server is stopped while the
# page is being fetched and started again; the job worker of the new
# process takes the request up under its own id.
echo "==> the server again, with a wall time of two seconds"
stop_server
export KAGE__SCHEDULER__SYNC_WALL_TIME=2s
serve ready
node tests/live/check-jobs.mjs overflow-begin "${workdir}"
echo "    stopping kage serve while the page is being fetched, and starting it again"
stop_server
serve ready
node tests/live/check-jobs.mjs overflow-end "${workdir}"
node tests/live/check-jobs.mjs snapshot "${workdir}" end
stop_server

node tests/live/check-jobs.mjs judge "${workdir}" ${KAGE_LIVE_EVIDENCE_DIR:+"${KAGE_LIVE_EVIDENCE_DIR}"} \
    || fail "the live check of jobs did not pass"
echo "    jobs ran against the real pool at the shipped pace: the batch, a flow in each engine, their webhooks, and a fetch carried across a restart"
