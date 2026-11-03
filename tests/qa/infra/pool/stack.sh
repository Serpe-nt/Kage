#!/usr/bin/env bash
# QA round two, pool: the harness, under the name the pool scripts use; pass
# QA_PERSONAS=5 to `up` for a world of five proxies (five-proxies.yaml).
exec bash /scripts/qa/stack.sh "$@"
