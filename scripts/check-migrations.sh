#!/usr/bin/env bash
# Checks the migrations and the committed sqlx query data against a throwaway
# Postgres (spec §18):
#
#   1. the migrations apply from an empty database;
#   2. `sqlx migrate info` is clean: every migration installed, none modified;
#   3. applying them again changes nothing;
#   4. .sqlx/ matches the queries in the code.
#
#   scripts/check-migrations.sh             check everything (CI)
#   scripts/check-migrations.sh --prepare   regenerate .sqlx/ instead of step 4
#
# Needs Docker and sqlx-cli (`just setup` installs it).
set -euo pipefail
cd "$(dirname "$0")/.."

mode="${1:-check}"
migrations="crates/kage-store/migrations"
# The same image, by digest, as deploy/compose.yaml (docs/versions.md).
image="postgres:17.11-trixie@sha256:2d2b8998d31037bf721cfdf764d76ba74171b4fab3431b7f72c27c56ddbdf9e3"
container="kage-sqlx-$$"

cleanup() {
    docker rm --force "${container}" > /dev/null 2>&1 || true
}
trap cleanup EXIT

fail() {
    echo "error: $*" >&2
    exit 1
}

command -v sqlx > /dev/null || fail "sqlx-cli is not installed; run 'just setup-sqlx'"

echo "==> starting a throwaway Postgres"
docker run --detach --name "${container}" \
    --env POSTGRES_PASSWORD=sqlx --publish 127.0.0.1::5432 "${image}" > /dev/null
port="$(docker port "${container}" 5432/tcp | head -n 1 | sed 's/.*://')"
export DATABASE_URL="postgres://postgres:sqlx@127.0.0.1:${port}/postgres"

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

# Prints `sqlx migrate info` and fails unless every migration is installed
# with the checksum of its file.
assert_clean() {
    local info expected installed
    info="$(sqlx migrate info --source "${migrations}")"
    echo "${info}" | sed 's/^/    /'
    expected="$(find "${migrations}" -maxdepth 1 -name '*.sql' | wc -l)"
    installed="$(grep -c '^[0-9]\+/installed ' <<< "${info}" || true)"
    [ "${installed}" -eq "${expected}" ] \
        || fail "${installed} of ${expected} migrations are installed"
    if grep -v '^[0-9]\+/installed ' <<< "${info}" | grep -q .; then
        fail "sqlx migrate info reports a migration that is not installed"
    fi
    if grep -qi 'different checksum' <<< "${info}"; then
        fail "an installed migration differs from its file"
    fi
}

echo "==> migrations apply from an empty database"
sqlx migrate run --source "${migrations}"
assert_clean

echo "==> applying them again changes nothing"
again="$(sqlx migrate run --source "${migrations}")"
if grep -q '^Applied ' <<< "${again}"; then
    echo "${again}" >&2
    fail "the second run applied migrations"
fi
assert_clean

if [ "${mode}" = "--prepare" ]; then
    echo "==> regenerating .sqlx/"
    cargo sqlx prepare --workspace -- --all-targets --all-features
else
    echo "==> .sqlx/ matches the queries in the code"
    cargo sqlx prepare --workspace --check -- --all-targets --all-features
fi

echo "ok: migrations and query data are consistent"
