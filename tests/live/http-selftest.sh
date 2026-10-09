#!/usr/bin/env bash
# Live check of the http path's fingerprint (docs/02-plan.md, M3): through
# two real proxies, one request each, to a fingerprint echo, comparing JA4,
# the HTTP/2 fingerprint and the header order with the recorded values. One
# proxy presents the newest Chrome profile, the other the newest Firefox
# profile.
#
# The echo is the operator's own when KAGE__SELFTEST__HTTP__ECHO_URL is set,
# else the third-party echo of `selftest.http.third_party_url`, which sees
# the exit address of the two proxies. That is why it is two proxies per run
# and not the pool: the pair moves through the pool by date, so with twenty
# proxies each address is shown about once in ten runs.
#
# It contacts the Webshare API (one read of the list and of the account) and
# the echo, and nothing else: never a target site. It requests no
# replacement and changes no account setting. Run it through
# tests/live/run.sh, which checks the gate and the secrets.
#
#   KAGE_LIVE_SELFTEST_DAY   overrides the day number that picks the pair
#
# Needs Docker, the Rust toolchain and Node.
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "${KAGE_LIVE:-}" != "1" ]; then
    echo "error: this is a live check; run it through tests/live/run.sh" >&2
    exit 1
fi

# The same image, by digest, as deploy/compose.yaml (docs/versions.md).
image="postgres:17.11-trixie@sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3"
container="kage-live-selftest-$$"
workdir="$(mktemp -d)"

cleanup() {
    docker rm --force "${container}" > /dev/null 2>&1 || true
    # The results name real proxy addresses; they do not outlive the run.
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

echo "    reading the proxy list and the profiles this build serves"
kage proxy list --sync --json > "${workdir}/proxies.json" \
    || fail "kage proxy list --sync failed; its log is above"
kage config check > "${workdir}/config.json" \
    || fail "kage config check failed; its log is above"

day="${KAGE_LIVE_SELFTEST_DAY:-$(($(date -u +%s) / 86400))}"
node tests/live/pick-selftest-pair.mjs "${workdir}/proxies.json" "${workdir}/config.json" "${day}" \
    > "${workdir}/pair.txt" || fail "no pair of proxies and profiles could be picked"

status=0
while read -r proxy profile; do
    echo "    ${profile} through one proxy"
    # A failing self-test still prints its report; the judge reads it.
    kage selftest http --proxy "${proxy}" --profile "${profile}" --json \
        > "${workdir}/${profile}.json" || status=1
done < "${workdir}/pair.txt"

node tests/live/check-http-selftest.mjs "${workdir}"/chrome_*.json "${workdir}"/firefox_*.json
[ "${status}" -eq 0 ] || fail "a self-test command failed; its log is above"
