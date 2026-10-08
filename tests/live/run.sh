#!/usr/bin/env bash
# Entry point of the gated live suite (spec §21). Live tests use real proxies
# and only ever hit the echo and detection test pages listed in configuration,
# never a real target site.
set -euo pipefail

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

echo "error: no live tests exist yet. The first ones land in M2 (proxy health) and M3 (HTTP fingerprint self-test)." >&2
echo "       This run fails on purpose so that an empty suite is never reported as green." >&2
exit 1
