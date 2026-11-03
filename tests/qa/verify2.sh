#!/usr/bin/env bash
# Round two: the session's own reproduction of each finding. Own-stack
# repros bring their own; the rest run on a fresh stack each (up + fast).
# Usage: verify2.sh ID...   -> /out/qa2/verify/<id>.log, summary.txt
set -uo pipefail
export SWEEP_OUT=/out/qa2/verify SWEEP_FINDINGS=/out/qa/findings
for id in "$@"; do
    if sed -n 2p "/out/qa/findings/${id}/repro.sh" | grep -q 'own-stack'; then
        bash /scripts/qa/sweep.sh own "${id}"
    else
        bash /scripts/qa/sweep.sh fresh "${id}"
    fi
done
