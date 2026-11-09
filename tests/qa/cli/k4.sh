#!/usr/bin/env bash
# Round four: the CLI of the commit under test, from its own image (kage:dev),
# with no network. Installed by container/prepare4.sh as
# /target/devtarget/debug/kage, so that round one's scripts (K=that path) and
# cli/k.sh run it unchanged without a cargo build on this side. Every KAGE__*
# and KAGE_CONFIG variable of the caller passes through; /tmp, /scripts, /out
# and the clone are mounted read-only at their own paths, and the working
# directory is kept when it lies under one of them.
# Used alone (no k.sh), it supplies throwaway secrets as k.sh does:
#   KAGE__SESSIONS__MAX_LIFETIME=3651d bash k4.sh config check
set -uo pipefail
secret() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }
: "${KAGE__DATABASE__URL:=postgres://qa@db.invalid/qa}"
: "${KAGE__SIDECAR__TOKEN:=$(secret 24)}"
: "${KAGE__SECRETS__MASTER_KEY:=$(secret 32)}"
: "${KAGE__PROXIES__STATIC_LIST:=proxy.invalid:3128:u:p:DE}"
export KAGE__DATABASE__URL KAGE__SIDECAR__TOKEN KAGE__SECRETS__MASTER_KEY KAGE__PROXIES__STATIC_LIST
args=(--rm -i --network none)
while IFS='=' read -r key _; do
    case "${key}" in
    KAGE__* | KAGE_CONFIG) args+=(-e "${key}") ;;
    esac
done < <(env)
for dir in /tmp /scripts /out /target/qa/kage; do
    [ -d "${dir}" ] && args+=(-v "${dir}:${dir}:ro")
done
case "${PWD}" in
/tmp* | /scripts* | /out* | /target/qa/kage*) args+=(-w "${PWD}") ;;
esac
exec docker run "${args[@]}" "${KAGE_IMAGE:-kage:dev}" "$@"
