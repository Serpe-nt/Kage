#!/usr/bin/env bash
# Live check of the browser path (docs/02-plan.md, M4 and M5): through two
# real proxies, one context of each engine loads the detection pages listed
# in `selftest.browser.third_party_pages` (bot.sannysoft.com and CreepJS),
# and the sidecar judges what they show: no automation flag, a coherent
# identity, a score above the threshold.
#
# Each sidecar runs as its image is deployed: non-root, read-only root, no
# capabilities; Chrome inside its own sandbox (deploy/seccomp/chromium.json),
# Camoufox under Docker's default seccomp profile.
#
# Two proxies per run and not the pool: the pages are run by others and see
# the exit address. The pair moves through the pool by date and is never
# one of the two the http self-test uses on the same day. Every engine
# leaves through the same pair, so the pages see two addresses per run
# however many engines are checked. The engines run one after the other,
# each in a sidecar of its own, and so do the two contexts of an engine
# (SIDECAR_MAX_CONTEXTS=1).
#
# It contacts the Webshare API (one read of the list and of the account) and
# the two detection pages, and nothing else: never a target site. It
# requests no replacement and changes no account setting. Run it through
# tests/live/run.sh, which checks the gate and the secrets.
#
#   KAGE_LIVE_ENGINES        the engines to check, in this order;
#                            "chromium firefox" when not set
#   KAGE_LIVE_SELFTEST_DAY   overrides the day number that picks the pair
#   KAGE_LIVE_FIREFOX_CLAIMS the systems the two Firefox stand-ins claim, in
#                            the pair's order; "windows windows" when not
#                            set ("windows macos" checks both claims)
#   KAGE_LIVE_RECORD=1       records the findings as the regression baseline
#                            of each engine checked
#                            (tests/fixtures/selftest/<engine>-<major>-live.json);
#                            review, then commit. An engine without a
#                            baseline fails the check until one is recorded;
#                            a recording run of a new major is judged
#                            against the previous major's baseline first.
#   KAGE_LIVE_EVIDENCE_DIR   keeps the pages' text and screenshots there, in
#                            one directory per engine; they are evidence for
#                            a person, never committed
#   KAGE_LIVE_IDLE_SECONDS   measures what an idle browser sends and receives
#                            by itself (tests/live/background-traffic.mjs).
#                            Firefox: its first context stays open and
#                            untouched for this long before its pages load.
#                            Chromium (ADR 0014): after the judged runs, two
#                            windows of this length (600 s at most), each a
#                            fresh stand-in of one proxy of the pair in a
#                            measurement image (measure-chromium.Dockerfile)
#                            with Chrome's NetLog on: the first with Kage's
#                            switches, the second with
#                            --disable-background-networking left to Chrome;
#                            the bytes per persona-hour and the hosts asked
#                            for (tests/live/netlog-hosts.mjs). Each window
#                            ends when its self-test loads the detection
#                            pages, which are not judged. Google is contacted
#                            from the two proxies, as Chrome does.
#
# The images are the ones deploy/compose.yaml names
# (KAGE_BROWSER_CHROMIUM_IMAGE, KAGE_BROWSER_FIREFOX_IMAGE); one that is
# missing is built here.
#
# Needs Docker, the Rust toolchain and Node.
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "${KAGE_LIVE:-}" != "1" ]; then
    echo "error: this is a live check; run it through tests/live/run.sh" >&2
    exit 1
fi

# The same image, by digest, as deploy/compose.yaml (docs/versions.md).
postgres_image="postgres:17.11-trixie@sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3"
postgres="kage-live-browser-db-$$"
# The sidecar of the engine that is being checked; empty between engines.
sidecar=""
workdir="$(mktemp -d)"
idle="${KAGE_LIVE_IDLE_SECONDS:-0}"
sampler=""

cleanup() {
    if [ -n "${sampler}" ]; then
        kill "${sampler}" 2> /dev/null || true
    fi
    docker rm --force --volumes "${postgres}" ${sidecar:+"${sidecar}"} > /dev/null 2>&1 || true
    # The results name real proxy addresses; they do not outlive the run.
    rm -rf "${workdir}"
}
trap cleanup EXIT

fail() {
    echo "error: $*" >&2
    exit 1
}

read -ra engines <<< "${KAGE_LIVE_ENGINES:-chromium firefox}"
read -ra firefox_claims <<< "${KAGE_LIVE_FIREFOX_CLAIMS:-windows windows}"
for claim in "${firefox_claims[@]}"; do
    case "${claim}" in
        windows | macos) ;;
        *) fail "KAGE_LIVE_FIREFOX_CLAIMS: a Firefox stand-in claims windows or macos, not ${claim}" ;;
    esac
done
[ "${#engines[@]}" -gt 0 ] || fail "KAGE_LIVE_ENGINES names no engine"
for engine in "${engines[@]}"; do
    case "${engine}" in
        chromium | firefox) ;;
        *) fail "KAGE_LIVE_ENGINES names '${engine}'; the engines are chromium and firefox" ;;
    esac
done
[[ "${idle}" =~ ^[0-9]+$ ]] || fail "KAGE_LIVE_IDLE_SECONDS is a number of seconds"
if [ "${idle}" -gt 600 ] && [[ " ${engines[*]} " == *" chromium "* ]]; then
    fail "KAGE_LIVE_IDLE_SECONDS: Chromium's two idle windows are 600 s each at most"
fi

# The image of an engine's sidecar, as deploy/compose.yaml names it.
image_of() {
    case "$1" in
        chromium) echo "${KAGE_BROWSER_CHROMIUM_IMAGE:-kage-browser-chromium:dev}" ;;
        firefox) echo "${KAGE_BROWSER_FIREFOX_IMAGE:-kage-browser-firefox:dev}" ;;
    esac
}

for engine in "${engines[@]}"; do
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

# Waits up to a minute for a command to succeed.
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
# This check must never spend a replacement. Nothing it runs requests one;
# a budget of zero makes that impossible as well.
export KAGE__PROXIES__REPLACEMENTS_PER_MONTH=0
# The configuration is the defaults plus the environment, whatever file the
# caller's shell points at.
unset KAGE_CONFIG

echo "    building kage"
cargo build --quiet --locked --package kage
kage() {
    cargo run --quiet --locked --package kage -- "$@"
}

echo "    applying the migrations"
kage migrate > /dev/null

echo "    reading the proxy list"
kage proxy list --sync --json > "${workdir}/proxies.json" \
    || fail "kage proxy list --sync failed; its log is above"

day="${KAGE_LIVE_SELFTEST_DAY:-$(($(date -u +%s) / 86400))}"
node tests/live/pick-browser-proxy.mjs "${workdir}/proxies.json" "${day}" > "${workdir}/pair.txt" \
    || fail "no pair of proxies could be picked"

# Starts the sidecar of an engine with the hardening of its service in
# deploy/compose.yaml and waits until it answers. A second argument names
# another image of the engine (the measurement image).
start_sidecar() {
    local engine="$1" image="${2:-$(image_of "$1")}" port
    local hardening=(--init --read-only --cap-drop ALL --security-opt no-new-privileges)
    if [ "${engine}" = chromium ]; then
        # Chrome's sandbox needs the derived profile. The firefox service
        # has no seccomp entry: it keeps Docker's default.
        hardening+=(--security-opt "seccomp=deploy/seccomp/chromium.json")
    fi
    sidecar="kage-live-browser-${engine}-$$"
    docker run --detach --name "${sidecar}" "${hardening[@]}" \
        --shm-size 1g --tmpfs /tmp --pids-limit 4096 --memory 4g \
        --volume /var/lib/kage/personas --volume /var/lib/kage/artifacts \
        --env "ENGINE=${engine}" --env "SIDECAR_TOKEN=${token}" --env SIDECAR_MAX_CONTEXTS=1 \
        --env "SIDECAR_CONTEXT_IDLE_MS=$(((idle + 300) * 1000))" \
        --publish 127.0.0.1::8090 "${image}" > /dev/null
    wait_for docker exec "${sidecar}" node dist/healthcheck.js \
        || fail "the ${engine} sidecar did not become ready"
    port="$(docker port "${sidecar}" 8090/tcp | head -n 1 | sed 's/.*://')"
    case "${engine}" in
        chromium) export KAGE__SIDECAR__CHROMIUM_URL="http://127.0.0.1:${port}" ;;
        firefox) export KAGE__SIDECAR__FIREFOX_URL="http://127.0.0.1:${port}" ;;
    esac
}

# The counters of the sidecar's network interface, every five seconds, into
# <file>, in the background. Reading them goes through the container
# runtime, not the network.
start_sampler() {
    local file="$1"
    (
        while true; do
            counters="$(docker exec "${sidecar}" cat \
                /sys/class/net/eth0/statistics/rx_bytes /sys/class/net/eth0/statistics/tx_bytes \
                2> /dev/null | tr '\n' ' ')" || break
            echo "$(date +%s%3N) ${counters}" >> "${file}"
            sleep 5
        done
    ) &
    sampler=$!
}

# Chromium's idle traffic with and without --disable-background-networking
# (ADR 0014): one window per proxy of the pair, each in a measurement image
# and on a fresh profile (a new container), the first with Kage's switches,
# the second without that one. What is kept is what the two reporters
# print: the bytes and the host names.
measure_chromium_idle() {
    local mode image window=0
    while read -r proxy country; do
        window=$((window + 1))
        if [ "${window}" -eq 1 ]; then mode=with; else mode=without; fi
        image="kage-live-measure-${mode}-$$"
        echo "    idle window ${window}: Chromium ${mode} --disable-background-networking, through proxy ${window} of the pair (${country}), ${idle} s"
        docker build --quiet --file tests/live/measure-chromium.Dockerfile \
            --build-arg "BASE=$(image_of chromium)" --build-arg "MODE=${mode}" \
            --tag "${image}" tests/live > /dev/null || fail "the measurement image could not be built"
        start_sidecar chromium "${image}"
        start_sampler "${workdir}/idle-${mode}-net.txt"
        kage selftest browser --proxy "${proxy}" --engine chromium --idle-before "${idle}s" \
            --third-party --json < /dev/null > "${workdir}/idle-${mode}.json" || true
        kill "${sampler}" 2> /dev/null || true
        sampler=""
        docker logs "${sidecar}" > "${workdir}/idle-${mode}-sidecar.log" 2>&1 || true
        # The browser has exited with its context: its NetLog is complete.
        docker exec "${sidecar}" cat /tmp/kage-netlog.json \
            > "${workdir}/idle-${mode}-netlog.json" 2> /dev/null || true
        node tests/live/background-traffic.mjs "${workdir}/idle-${mode}-net.txt" "${workdir}/idle-${mode}-sidecar.log"
        node tests/live/netlog-hosts.mjs "${workdir}/idle-${mode}-netlog.json" "${workdir}/idle-${mode}-sidecar.log"
        docker rm --force --volumes "${sidecar}" > /dev/null 2>&1 || true
        sidecar=""
        docker rmi "${image}" > /dev/null 2>&1 || true
    done < "${workdir}/pair.txt"
}

baseline_dir="tests/fixtures/selftest"
failed=()
for engine in "${engines[@]}"; do
    echo "    starting the ${engine} sidecar"
    start_sidecar "${engine}"

    # Chromium measures its idle traffic in windows of its own, after the
    # judged runs (measure_chromium_idle).
    engine_idle="${idle}"
    [ "${engine}" = chromium ] && engine_idle=0
    if [ "${engine_idle}" -gt 0 ]; then
        start_sampler "${workdir}/${engine}-net.txt"
    fi

    status=0
    run=0
    while read -r proxy country; do
        run=$((run + 1))
        idle_flags=()
        # Only the first context idles: one measurement per engine is enough.
        if [ "${engine_idle}" -gt 0 ] && [ "${run}" -eq 1 ]; then
            idle_flags+=(--idle-before "${idle}s")
            echo "    the first context stays idle for ${idle} s before its pages load"
        fi
        claim_flags=()
        if [ "${engine}" = firefox ]; then
            claim="${firefox_claims[$((run - 1))]:-windows}"
            claim_flags+=(--claimed-os "${claim}")
            echo "    ${engine} through proxy ${run} of the pair (${country}), claiming ${claim}"
        else
            echo "    ${engine} through proxy ${run} of the pair (${country})"
        fi
        # The time zone and the languages are the stand-in's own: what a
        # persona of this engine behind this proxy would get.
        # A failing self-test still prints its report; the judge reads it.
        # stdin is the list of the loop: the command must not read from it.
        kage selftest browser --proxy "${proxy}" --engine "${engine}" \
            "${claim_flags[@]}" "${idle_flags[@]}" --third-party --json \
            < /dev/null > "${workdir}/${engine}-report-${run}.json" || status=1
    done < "${workdir}/pair.txt"

    if [ "${engine_idle}" -gt 0 ]; then
        kill "${sampler}" 2> /dev/null || true
        sampler=""
        docker logs "${sidecar}" > "${workdir}/${engine}-sidecar.log" 2>&1 || true
        node tests/live/background-traffic.mjs "${workdir}/${engine}-net.txt" "${workdir}/${engine}-sidecar.log"
    fi

    if [ -n "${KAGE_LIVE_EVIDENCE_DIR:-}" ]; then
        mkdir -p "${KAGE_LIVE_EVIDENCE_DIR}/${engine}"
        docker cp "${sidecar}:/var/lib/kage/artifacts/." "${KAGE_LIVE_EVIDENCE_DIR}/${engine}/" > /dev/null \
            || echo "    the evidence files could not be copied" >&2
    fi

    docker rm --force --volumes "${sidecar}" > /dev/null 2>&1 || true
    sidecar=""

    # Both reports are judged and compared with the engine's baseline; the
    # first is the one a recording run stores. Without a baseline the judge
    # fails and says how to record one.
    judged=0
    node tests/live/check-browser-selftest.mjs "${workdir}/${engine}-report-1.json" "${baseline_dir}" \
        ${KAGE_LIVE_RECORD:+--record} || judged=1
    node tests/live/check-browser-selftest.mjs "${workdir}/${engine}-report-2.json" "${baseline_dir}" \
        || judged=1
    if [ "${judged}" -ne 0 ]; then
        echo "error: a ${engine} self-test did not pass" >&2
        failed+=("${engine}")
    elif [ "${status}" -ne 0 ]; then
        echo "error: a ${engine} self-test command failed; its log is above" >&2
        failed+=("${engine}")
    fi

    if [ "${engine}" = chromium ] && [ "${idle}" -gt 0 ]; then
        measure_chromium_idle
    fi
done

[ "${#failed[@]}" -eq 0 ] || fail "the browser self-test did not pass for: ${failed[*]}"
