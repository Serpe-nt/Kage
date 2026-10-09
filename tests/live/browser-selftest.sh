#!/usr/bin/env bash
# Live check of the browser path (docs/02-plan.md, M4): through two real
# proxies, one Chromium context each loads the detection pages listed in
# `selftest.browser.third_party_pages` (bot.sannysoft.com and CreepJS), and
# the sidecar judges what they show: no automation flag, a coherent
# identity, a score above the threshold.
#
# The sidecar runs as the image deploys it: non-root, read-only root, no
# capabilities, Chrome inside its own sandbox (deploy/seccomp/chromium.json).
#
# Two proxies per run and not the pool: the pages are run by others and see
# the exit address. The pair moves through the pool by date and is never
# one of the two the http self-test uses on the same day. The two contexts
# run one after the other in one sidecar (SIDECAR_MAX_CONTEXTS=1).
#
# It contacts the Webshare API (one read of the list and of the account) and
# the two detection pages, and nothing else: never a target site. It
# requests no replacement and changes no account setting. Run it through
# tests/live/run.sh, which checks the gate and the secrets.
#
#   KAGE_LIVE_SELFTEST_DAY   overrides the day number that picks the pair
#   KAGE_LIVE_RECORD=1       records the findings as the regression baseline
#                            (tests/fixtures/selftest/); review, then commit
#   KAGE_LIVE_EVIDENCE_DIR   keeps the pages' text and screenshots there;
#                            they are evidence for a person, never committed
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
sidecar_image="${KAGE_BROWSER_CHROMIUM_IMAGE:-kage-browser-chromium:dev}"
postgres="kage-live-browser-db-$$"
sidecar="kage-live-browser-$$"
workdir="$(mktemp -d)"

cleanup() {
    docker rm --force --volumes "${postgres}" "${sidecar}" > /dev/null 2>&1 || true
    # The results name real proxy addresses; they do not outlive the run.
    rm -rf "${workdir}"
}
trap cleanup EXIT

fail() {
    echo "error: $*" >&2
    exit 1
}

if ! docker image inspect "${sidecar_image}" > /dev/null 2>&1; then
    echo "    building ${sidecar_image}"
    docker build --quiet --file deploy/docker/sidecar.Dockerfile --build-arg ENGINE=chromium \
        --tag "${sidecar_image}" . > /dev/null
fi

echo "    starting a throwaway Postgres and the Chromium sidecar"
password="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
docker run --detach --name "${postgres}" \
    --env "POSTGRES_PASSWORD=${password}" --publish 127.0.0.1::5432 "${postgres_image}" > /dev/null
# The hardening of deploy/compose.yaml, service browser-chromium.
docker run --detach --name "${sidecar}" --init --read-only --cap-drop ALL \
    --security-opt no-new-privileges --security-opt "seccomp=deploy/seccomp/chromium.json" \
    --shm-size 1g --tmpfs /tmp --pids-limit 4096 --memory 4g \
    --volume /var/lib/kage/personas --volume /var/lib/kage/artifacts \
    --env ENGINE=chromium --env "SIDECAR_TOKEN=${token}" --env SIDECAR_MAX_CONTEXTS=1 \
    --publish 127.0.0.1::8090 "${sidecar_image}" > /dev/null
postgres_port="$(docker port "${postgres}" 5432/tcp | head -n 1 | sed 's/.*://')"
sidecar_port="$(docker port "${sidecar}" 8090/tcp | head -n 1 | sed 's/.*://')"

ready=""
for _ in $(seq 1 60); do
    if docker exec "${postgres}" pg_isready --host 127.0.0.1 --username postgres > /dev/null 2>&1 \
        && docker exec "${sidecar}" node dist/healthcheck.js > /dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 1
done
[ -n "${ready}" ] || fail "Postgres or the sidecar did not become ready"

export KAGE__DATABASE__URL="postgres://postgres:${password}@127.0.0.1:${postgres_port}/postgres"
export KAGE__SIDECAR__TOKEN="${token}"
export KAGE__SIDECAR__CHROMIUM_URL="http://127.0.0.1:${sidecar_port}"
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

status=0
run=0
while read -r proxy timezone languages; do
    run=$((run + 1))
    language_flags=()
    IFS=',' read -ra tags <<< "${languages}"
    for tag in "${tags[@]}"; do
        language_flags+=(--language "${tag}")
    done
    echo "    chromium through proxy ${run} of the pair (${timezone}, ${languages})"
    # A failing self-test still prints its report; the judge reads it.
    # stdin is the list of the loop: the command must not read from it.
    kage selftest browser --proxy "${proxy}" --engine chromium --timezone "${timezone}" \
        "${language_flags[@]}" --third-party --json \
        < /dev/null > "${workdir}/report-${run}.json" || status=1
done < "${workdir}/pair.txt"

if [ -n "${KAGE_LIVE_EVIDENCE_DIR:-}" ]; then
    mkdir -p "${KAGE_LIVE_EVIDENCE_DIR}"
    docker cp "${sidecar}:/var/lib/kage/artifacts/." "${KAGE_LIVE_EVIDENCE_DIR}/" > /dev/null \
        || echo "    the evidence files could not be copied" >&2
fi

# Both reports are judged and compared with the baseline; the first is the
# one a recording run stores.
baseline_dir="tests/fixtures/selftest"
judged=0
node tests/live/check-browser-selftest.mjs "${workdir}/report-1.json" "${baseline_dir}" \
    ${KAGE_LIVE_RECORD:+--record} || judged=1
node tests/live/check-browser-selftest.mjs "${workdir}/report-2.json" "${baseline_dir}" || judged=1
[ "${judged}" -eq 0 ] || fail "a browser self-test did not pass"
[ "${status}" -eq 0 ] || fail "a self-test command failed; its log is above"
