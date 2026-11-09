#!/usr/bin/env bash
# Round four: the session's own reproduction of each finding of a part.
# Own-stack repros bring their own; the rest run on a fresh stack each
# (up + fast). Usage: verify4.sh ID...   (VERIFY_FINDINGS, default
# /out/qa4/findings) -> /out/qa4/verify/<id>[-fresh].log, summary.txt
set -uo pipefail
findings="${VERIFY_FINDINGS:-/out/qa4/findings}"
export SWEEP_OUT="${SWEEP_OUT:-/out/qa4/verify}" SWEEP_FINDINGS="${findings}"
for id in "$@"; do
    if sed -n 2p "${findings}/${id}/repro.sh" | grep -q 'own-stack'; then
        bash /scripts/qa/sweep.sh own "${id}"
    else
        bash /scripts/qa/sweep.sh fresh "${id}"
    fi
done
