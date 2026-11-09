#!/usr/bin/env bash
# Round four: a race-dependent finding, up to N tries, each on a fresh stack
# (or its own). Usage: retry4.sh N ID...   -> /out/qa4/verify/tryK/, retries.txt
set -uo pipefail
n="${1:?tries}"
shift
for id in "$@"; do
    for k in $(seq 1 "${n}"); do
        SWEEP_OUT="/out/qa4/verify/try${k}" bash /scripts/qa/verify4.sh "${id}" > /dev/null 2>&1
        line="$(grep "^${id}" "/out/qa4/verify/try${k}/summary.txt" 2>/dev/null | tail -n 1)"
        echo "${id} try ${k}: ${line}" | tee -a /out/qa4/verify/retries.txt
        case "${line}" in *" REPRODUCED "*) break ;; esac
    done
done
