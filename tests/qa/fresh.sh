#!/usr/bin/env bash
# Reproduces findings each on a stack of its own, for a repro that depends on
# a fresh world. Usage: fresh.sh ID...
set -uo pipefail
stack_sh=/scripts/qa/stack.sh
for id in "$@"; do
    stack="f-${id}"
    bash "${stack_sh}" up "${stack}" > "/out/qa/verify/${id}-fresh-up.log" 2>&1
    bash "${stack_sh}" fast "${stack}" >> "/out/qa/verify/${id}-fresh-up.log" 2>&1
    start=$(date +%s)
    timeout 2400 bash "/out/qa/findings/${id}/repro.sh" "${stack}" > "/out/qa/verify/${id}-fresh.log" 2>&1
    code=$?
    end=$(date +%s)
    if [ "${code}" = 0 ] && tail -n 3 "/out/qa/verify/${id}-fresh.log" | grep --quiet '^REPRODUCED'; then
        verdict=REPRODUCED-FRESH
    elif tail -n 3 "/out/qa/verify/${id}-fresh.log" | grep --quiet '^NOT-REPRODUCED'; then
        verdict=NOT-REPRODUCED-FRESH
    else
        verdict="ERROR-FRESH(${code})"
    fi
    echo "${id} ${verdict} $((end - start))" | tee -a /out/qa/verify/summary.txt
    bash "${stack_sh}" down "${stack}" > /dev/null 2>&1
    rm -rf "/out/qa/logs/${stack}"
done
echo done
