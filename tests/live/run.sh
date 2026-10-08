#!/usr/bin/env bash
# Entry point of the gated live suite (spec §21). Live tests use real proxies
# and only ever hit the echo and detection test pages listed in configuration,
# never a real target site.
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "${KAGE_LIVE:-}" != "1" ]; then
    echo "error: the live suite is gated. Set KAGE_LIVE=1 to run it." >&2
    exit 1
fi

missing=()
for name in KAGE__WEBSHARE__API_KEY KAGE__SECRETS__MASTER_KEY; do
    if [ -z "${!name:-}" ]; then
        missing+=("${name}")
    fi
done
if [ "${#missing[@]}" -gt 0 ]; then
    echo "error: the live suite needs these secrets in the environment: ${missing[*]}" >&2
    echo "       See docs/runbooks/repo.md, section 'Nightly live suite'." >&2
    exit 1
fi

# One script per check. M3 adds the HTTP fingerprint self-test.
checks=(
    tests/live/proxy-health.sh
)

for check in "${checks[@]}"; do
    echo "==> ${check}"
    "${check}"
done

echo "live suite passed (${#checks[@]} check(s))"
