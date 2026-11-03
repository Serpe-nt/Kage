#!/usr/bin/env bash
# Round two's regression sweep: round one's 111 repro.sh as filed, against
# the stacks of this round. Usage: sweep.sh LANE ID...
#   LANE shared: one stack `sw-<lane>` with `fast`, every id on it in turn.
#   LANE own:    each id brings its own stack (their second line says how).
#   LANE fresh:  each id on a fresh stack of its own (stack.sh up + fast).
# Writes /out/qa2/sweep/<id>[-fresh].log and appends `<id> <verdict> <seconds>`
# to /out/qa2/sweep/summary.txt.
set -uo pipefail
lane="${1:?lane}"
shift
out="${SWEEP_OUT:-/out/qa2/sweep}"
mkdir -p "${out}"
stack_sh=/scripts/qa/stack.sh
findings="${SWEEP_FINDINGS:-/out/qa1/findings}"
verdict_of() {
    local log="$1" code="$2"
    if [ "${code}" = 0 ] && tail -n 3 "${log}" | grep --quiet '^REPRODUCED'; then
        echo REPRODUCED
    elif tail -n 3 "${log}" | grep --quiet '^NOT-REPRODUCED'; then
        echo NOT-REPRODUCED
    else
        echo "ERROR(${code})"
    fi
}
run_one() {
    local id="$1" stack="$2" suffix="$3" script="${4:-${findings}/${1}/repro.sh}"
    local log="${out}/${id}${suffix}.log" start end code
    start=$(date +%s)
    (cd "${findings}/${id}" && timeout 2400 bash "${script}" "${stack}") > "${log}" 2>&1
    code=$?
    end=$(date +%s)
    local verdict
    verdict="$(verdict_of "${log}" "${code}")"
    { echo "--- firewall after ${id}"; bash "${stack_sh}" fw; } > "${out}/${id}${suffix}.fw" 2>&1
    echo "${id}${suffix} ${verdict} $((end - start)) $(date -u +%H:%M:%SZ)" | tee -a "${out}/summary.txt"
}
case "${lane}" in
shared)
    stack="sw-shared"
    if [ ! -f "/target/qa/runs/${stack}/env.sh" ]; then
        bash "${stack_sh}" up "${stack}" > "${out}/${stack}-up.log" 2>&1 || { echo "stack ${stack} failed" | tee -a "${out}/summary.txt"; exit 1; }
        bash "${stack_sh}" fast "${stack}" >> "${out}/${stack}-up.log" 2>&1
    fi
    for id in "$@"; do run_one "${id}" "${stack}" ""; done
    bash "${stack_sh}" down "${stack}" > /dev/null 2>&1
    ;;
own)
    for id in "$@"; do run_one "${id}" "sw-own-${id}" ""; done
    ;;
fresh)
    for id in "$@"; do
        stack="sw-f-${id}"
        bash "${stack_sh}" up "${stack}" > "${out}/${id}-fresh-up.log" 2>&1
        bash "${stack_sh}" fast "${stack}" >> "${out}/${id}-fresh-up.log" 2>&1
        run_one "${id}" "${stack}" "-fresh"
        bash "${stack_sh}" down "${stack}" > /dev/null 2>&1
    done
    ;;
alt)
    # A corrected copy of a repro (round B's), on its own stack: alt ID FILE
    run_one "$1" "sw-alt-$1" "-alt" "$2"
    ;;
esac
echo "lane ${lane} done $(date -u +%H:%M:%SZ)" | tee -a "${out}/summary.txt"
