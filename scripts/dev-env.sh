#!/usr/bin/env bash
# Exports the configuration that `kage` needs to run natively against the
# development stack (deploy/compose.dev.yaml publishes Postgres on loopback).
# Source it; do not execute it:
#
#   source scripts/dev-env.sh && cargo run --package kage -- serve
#
# The values come from deploy/.env, which `just setup` creates.

_kage_env_file="$(dirname "${BASH_SOURCE[0]}")/../deploy/.env"
if [ ! -f "${_kage_env_file}" ]; then
    echo "error: ${_kage_env_file} is missing; run 'just setup-env'" >&2
    return 1 2> /dev/null || exit 1
fi

set -a
# shellcheck disable=SC1090
. "${_kage_env_file}"
set +a
unset _kage_env_file

export KAGE__DATABASE__URL="postgres://kage:${KAGE_POSTGRES_PASSWORD}@127.0.0.1:5432/kage"
export KAGE__SIDECAR__TOKEN="${KAGE_SIDECAR_TOKEN}"
export KAGE__SECRETS__MASTER_KEY="${KAGE_MASTER_KEY}"
# Optional (docs/runbooks/proxies.md): set only when deploy/.env has them, so
# a key exported in the shell stays in force.
if [ -n "${KAGE_WEBSHARE_API_KEY:-}" ]; then
    export KAGE__WEBSHARE__API_KEY="${KAGE_WEBSHARE_API_KEY}"
fi
if [ -n "${KAGE_ABUSEIPDB_KEY:-}" ]; then
    export KAGE__REPUTATION__ABUSEIPDB_KEY="${KAGE_ABUSEIPDB_KEY}"
fi
# A development server does not send new personas to real sites unless it
# is told to (docs/runbooks/personas.md): KAGE_PERSONAS_WARMUP=true.
export KAGE__PERSONAS__WARMUP__ENABLED="${KAGE_PERSONAS_WARMUP:-false}"
