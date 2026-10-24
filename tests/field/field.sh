#!/usr/bin/env bash
# The field campaign's runner (docs/runbooks/field-campaign.md, "A session
# runs it"): `just field <site-index>` reads that site's batch of today
# through the API of the running campaign stack and records it. It reads a
# real site, so it runs only on the owner's approval of the round's list,
# never in CI and never from a test:
#
#   KAGE_FIELD=1                 the guard
#   KAGE_FIELD_SITES             the private sites file (tests/field/sites.example.json)
#   KAGE_FIELD_DIR               the private directory for the records
#   KAGE_FIELD_ENV               a private file that sets KAGE_FIELD_KEY (the
#                                round's key); read here, never printed
#   KAGE_FIELD_PROJECT           the stack's compose project (default kage-m13)
#   KAGE_FIELD_PORT              the API's port on the loopback (default 18180)
#
# `just field <index>` runs the batch; `just field <index> check` the
# preflight alone; `just field - <command> ...` any other command of
# tests/field/field.mjs (assign, policies, report <file>).
set -euo pipefail
cd "$(dirname "$0")/../.."

if [ "${KAGE_FIELD:-}" != "1" ] || [ -n "${CI:-}" ]; then
    echo "error: the field runner reads real sites: it runs only with KAGE_FIELD=1, outside CI, on the owner's approval" >&2
    exit 2
fi
for name in KAGE_FIELD_SITES KAGE_FIELD_DIR KAGE_FIELD_ENV; do
    if [ -z "${!name:-}" ]; then
        echo "error: ${name} is not set" >&2
        exit 2
    fi
done
[ -f "${KAGE_FIELD_SITES}" ] || { echo "error: no sites file at ${KAGE_FIELD_SITES}" >&2; exit 2; }
[ -f "${KAGE_FIELD_ENV}" ] || { echo "error: no private environment at ${KAGE_FIELD_ENV}" >&2; exit 2; }

project="${KAGE_FIELD_PROJECT:-kage-m13}"
export KAGE_API_URL="http://127.0.0.1:${KAGE_FIELD_PORT:-18180}"
export KAGE_FIELD_POSTGRES="${KAGE_FIELD_POSTGRES:-${project}-postgres-1}"
curl --silent --fail --max-time 5 --output /dev/null "${KAGE_API_URL}/healthz" \
    || { echo "error: the stack does not answer at ${KAGE_API_URL}" >&2; exit 2; }

# The key, from the private file, into the runner's environment only.
set -a
# shellcheck disable=SC1090
. "${KAGE_FIELD_ENV}"
set +a
[ -n "${KAGE_FIELD_KEY:-}" ] || { echo "error: ${KAGE_FIELD_ENV} does not set KAGE_FIELD_KEY" >&2; exit 2; }

index="${1:?usage: field.sh <site-index> [check] | field.sh - <command> ...}"
shift
if [ "${index}" = "-" ]; then
    exec node tests/field/field.mjs "$@"
elif [ "${1:-}" = "check" ]; then
    shift
    exec node tests/field/field.mjs check "${index}" "$@"
else
    exec node tests/field/field.mjs run "${index}" "$@"
fi
