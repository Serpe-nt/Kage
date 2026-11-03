#!/usr/bin/env bash
# Round two stand-in for round one's missing cli/k.sh: the CLI built from the
# commit under test, on this side, with the shipped defaults (no KAGE__ env),
# from the clone (its policies/). Usage: k.sh -- ARGS
[ "${1:-}" = "--" ] && shift
cd /target/qa/kage && exec env -u KAGE_CONFIG KAGE__DATABASE__URL=postgres://qa@db.invalid/qa KAGE__SIDECAR__TOKEN=$(head -c 24 /dev/urandom | od -An -tx1 | tr -d " \n") KAGE__SECRETS__MASTER_KEY=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d " 
") KAGE__PROXIES__STATIC_LIST=proxy.invalid:3128:u:p:DE /target/devtarget/debug/kage "$@"
