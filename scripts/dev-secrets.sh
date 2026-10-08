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
