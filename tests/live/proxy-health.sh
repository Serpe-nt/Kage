#!/usr/bin/env bash
# Live check of the proxy pool (docs/02-plan.md, M2): reads the real proxy
# list from Webshare into a throwaway Postgres and probes every proxy once
# with `kage proxy health`. It passes when the list has the expected number
# of proxies and every one answers with a latency and a country.
#
# It contacts the Webshare API and the endpoints of `proxies.health` (the
# provider's echo, the confirmation echo, the geo lookup) and nothing else:
# never a target site. Run it through tests/live/run.sh, which checks the
# gate and the secrets.
#
#   KAGE_LIVE_EXPECT_PROXIES   proxies the plan has (default 20)
#
# Needs Docker, the Rust toolchain and Node.
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "${KAGE_LIVE:-}" != "1" ]; then
    echo "error: this is a live check; run it through tests/live/run.sh" >&2
    exit 1
fi

expected="${KAGE_LIVE_EXPECT_PROXIES:-20}"
# The same image, by digest, as deploy/compose.yaml (docs/versions.md).
image="postgres:17.11-trixie@sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3"
container="kage-live-$$"
workdir="$(mktemp -d)"

cleanup() {
    docker rm --force "${container}" > /dev/null 2>&1 || true
    # The result names the real proxy addresses; it does not outlive the run.
    rm -rf "${workdir}"
}
trap cleanup EXIT

fail() {
    echo "error: $*" >&2
    exit 1
}

echo "    starting a throwaway Postgres"
password="$(head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n')"
docker run --detach --name "${container}" \
    --env "POSTGRES_PASSWORD=${password}" --publish 127.0.0.1::5432 "${image}" > /dev/null
port="$(docker port "${container}" 5432/tcp | head -n 1 | sed 's/.*://')"

# The server restarts once while it initializes; only the final one listens
# on TCP, so a TCP probe waits for the right one.
ready=""
for _ in $(seq 1 60); do
    if docker exec "${container}" pg_isready --host 127.0.0.1 --username postgres > /dev/null 2>&1; then
        ready=1
        break
    fi
    sleep 1
done
[ -n "${ready}" ] || fail "Postgres did not become ready"

export KAGE__DATABASE__URL="postgres://postgres:${password}@127.0.0.1:${port}/postgres"
# No sidecar takes part, but kage does not start without the token.
export KAGE__SIDECAR__TOKEN="${KAGE__SIDECAR__TOKEN:-not-used-by-this-check}"
export KAGE__PROXIES__PROVIDER=webshare
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

echo "    syncing the proxy list and probing every proxy once"
kage proxy health --json > "${workdir}/health.json" \
    || fail "kage proxy health failed; its log is above"

node tests/live/check-proxy-health.mjs "${workdir}/health.json" "${expected}"
