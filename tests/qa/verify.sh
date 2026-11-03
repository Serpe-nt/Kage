#!/usr/bin/env bash
# Reproduces findings on a fresh stack of the session's own. Usage:
#   verify.sh ID...      (a topic prefix such as auth- takes every finding of it)
# Writes /out/qa/verify/<id>.log and appends `<id> <verdict> <seconds>` to
# /out/qa/verify/summary.txt.
set -uo pipefail
mkdir -p /out/qa/verify
stack=verify
stack_sh=/scripts/qa/stack.sh
ids=()
for arg in "$@"; do
    for dir in /out/qa/findings/${arg}*; do
        ids+=("$(basename "${dir}")")
    done
done
if [ ! -f "/target/qa/runs/${stack}/env.sh" ]; then
    bash "${stack_sh}" up "${stack}" > /out/qa/verify/stack-up.log 2>&1 || { echo "stack failed"; exit 1; }
    bash "${stack_sh}" fast "${stack}" >> /out/qa/verify/stack-up.log 2>&1
fi
for id in "${ids[@]}"; do
    repro="/out/qa/findings/${id}/repro.sh"
    if [ ! -f "${repro}" ]; then
        echo "${id} NO-REPRO 0" >> /out/qa/verify/summary.txt
        continue
    fi
    start=$(date +%s)
    timeout 2400 bash "${repro}" "${stack}" > "/out/qa/verify/${id}.log" 2>&1
    code=$?
    end=$(date +%s)
    if [ "${code}" = 0 ] && tail -n 3 "/out/qa/verify/${id}.log" | grep --quiet '^REPRODUCED'; then
        verdict=REPRODUCED
    elif tail -n 3 "/out/qa/verify/${id}.log" | grep --quiet '^NOT-REPRODUCED'; then
        verdict=NOT-REPRODUCED
    else
        verdict="ERROR(${code})"
    fi
    echo "${id} ${verdict} $((end - start))" | tee -a /out/qa/verify/summary.txt
done
echo done
