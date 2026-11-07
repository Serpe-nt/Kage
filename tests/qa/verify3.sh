#!/usr/bin/env bash
# Round three: the session's own reproduction of each finding. Own-stack
# repros bring their own; the rest run on a fresh stack each (up + fast).
# Usage: verify3.sh ID...   -> /out/qa3/verify/<id>.log, summary.txt
# (VERIFY_ENV="ENGINE=firefox" runs the repro with that variable set.)
set -uo pipefail
export SWEEP_OUT="${SWEEP_OUT:-/out/qa3/verify}" SWEEP_FINDINGS=/out/qa3/findings
for id in "$@"; do
    if sed -n 2p "/out/qa3/findings/${id}/repro.sh" | grep -q 'own-stack'; then
        bash /scripts/qa/sweep.sh own "${id}"
    else
        bash /scripts/qa/sweep.sh fresh "${id}"
    fi
done
