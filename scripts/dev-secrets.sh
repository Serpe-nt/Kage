#!/usr/bin/env bash
# Creates deploy/.env with random local secrets. Existing values are kept, so
# the script is safe to run again; it only adds keys that are missing.
#
# deploy/.env is ignored by git. Production secrets are provisioned by the
# operator, never by this script.
set -euo pipefail
cd "$(dirname "$0")/.."

env_file="deploy/.env"
umask 077
touch "${env_file}"

# Hex keeps the value safe inside URLs and shell words.
random_hex() {
    head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'
}

ensure() {
    local key="$1" bytes="$2"
    if ! grep -q "^${key}=" "${env_file}"; then
        printf '%s=%s\n' "${key}" "$(random_hex "${bytes}")" >> "${env_file}"
        echo "generated ${key} in ${env_file}"
    fi
}

ensure KAGE_POSTGRES_PASSWORD 24
# Shared secret between kage and the browser sidecars (X-Kage-Sidecar-Token).
ensure KAGE_SIDECAR_TOKEN 32
# Key that encrypts proxy credentials at rest: 32 random bytes, hex-encoded.
ensure KAGE_MASTER_KEY 32
# The Grafana admin of the observability profile (deploy/compose.observability.yaml).
ensure KAGE_GRAFANA_ADMIN_PASSWORD 16
