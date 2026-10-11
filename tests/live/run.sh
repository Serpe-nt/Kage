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

# One script per check.
checks=(
    tests/live/proxy-health.sh
    tests/live/http-selftest.sh
    tests/live/browser-selftest.sh
)

# The persona check generates the pool's personas and shows two more exit
# addresses to the third-party detection pages: on request only.
if [ "${KAGE_LIVE_PERSONAS:-}" = "1" ]; then
    checks+=(tests/live/personas.sh)
fi

# The jobs check runs `kage serve` against the real pool and reads the two
# third-party detection pages about a dozen times through the proxies of a
# few personas: on request only.
if [ "${KAGE_LIVE_JOBS:-}" = "1" ]; then
    checks+=(tests/live/jobs.sh)
fi

for check in "${checks[@]}"; do
    echo "==> ${check}"
    "${check}"
done

echo "live suite passed (${#checks[@]} check(s))"
