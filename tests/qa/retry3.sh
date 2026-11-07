#!/usr/bin/env bash
# Round three: a race-dependent finding, up to N tries, each on a fresh stack
# (or its own). Usage: retry3.sh N ID...   -> /out/qa3/verify/<id>-tryK.log
set -uo pipefail
n="${1:?tries}"
shift
for id in "$@"; do
    for k in $(seq 1 "${n}"); do
        SWEEP_OUT="/out/qa3/verify/try${k}" bash /scripts/qa/verify3.sh "${id}" > /dev/null 2>&1
        line="$(grep "^${id}" "/out/qa3/verify/try${k}/summary.txt" 2>/dev/null | tail -n 1)"
        echo "${id} try ${k}: ${line}" | tee -a /out/qa3/verify/retries.txt
        case "${line}" in *" REPRODUCED "*) break ;; esac
    done
done
