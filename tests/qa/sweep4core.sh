#!/usr/bin/env bash
# Round four, part A (the core): the regression sweep of rounds one to
# three's repro.sh in the core's area only, as filed, through sweep.sh.
# Part B (browsers, sidecar, typing, extraction, crawl, classifier) sweeps
# the rest. Usage: sweep4core.sh r1-shared | r1-own | r2-fresh | r2-own | r3 | list
# Round one's non-own scripts share one stack; round two's and three's each
# get a fresh stack (or the stack of their own their second line names).
set -uo pipefail
part="${1:?part}"
out=/out/qa4/sweep
# The core's ids, by round (findings dirs /out/qaN/findings/<id>).
# Agreed with part B (2026-11-08): it sweeps round one's api-08 and jobs-02
# (F116) and round two's flows-02, left-02, left-03, tls-02, tls-03,
# http-03..08, ssrf-02, ssrf-04; those are left out here.
r1_core() { ls /out/qa1/findings | grep -E '^(api|auth|cli|jobs|sched|secrecy|ssrf|docs)-' | grep -vxE 'api-08|jobs-02'; }
r2_core() { ls /out/qa2/findings | grep -E '^(flows|http|pool|ssrf|timers)-|^tls-01$|^left-01$' | grep -vxE 'flows-02|http-0[3-8]|ssrf-0[24]'; }
r3_core() { ls /out/qa3/findings | grep -E '^(api|flows|lock|level0|ops)-'; }
own_or_other() {
    local dir="$1" want="$2" id
    while read -r id; do
        [ -f "${dir}/${id}/repro.sh" ] || continue
        if sed -n 2p "${dir}/${id}/repro.sh" | grep -q own-stack; then
            [ "${want}" = own ] && echo "${id}"
        else
            [ "${want}" = other ] && echo "${id}"
        fi
    done
}
case "${part}" in
list)
    echo "r1: $(r1_core | wc -l)  r2: $(r2_core | wc -l)  r3: $(r3_core | wc -l)"
    r1_core | tr '\n' ' '; echo; r2_core | tr '\n' ' '; echo; r3_core | tr '\n' ' '; echo
    ;;
r1-shared)
    SWEEP_OUT="${out}/r1" SWEEP_FINDINGS=/out/qa1/findings bash /scripts/qa/sweep.sh shared $(r1_core | own_or_other /out/qa1/findings other)
    ;;
r1-own)
    # secrecy-01 runs as round two's corrected copy (as in round three).
    ids="$(r1_core | own_or_other /out/qa1/findings own | grep -v '^secrecy-01$')"
    SWEEP_OUT="${out}/r1" SWEEP_FINDINGS=/out/qa1/findings bash /scripts/qa/sweep.sh own ${ids}
    SWEEP_OUT="${out}/r1" SWEEP_FINDINGS=/out/qa1/findings bash /scripts/qa/sweep.sh alt secrecy-01 /out/qa1/support/alt/secrecy-01.sh
    ;;
r2-fresh)
    SWEEP_OUT="${out}/r2" SWEEP_FINDINGS=/out/qa2/findings bash /scripts/qa/sweep.sh fresh $(r2_core | own_or_other /out/qa2/findings other)
    ;;
r2-own)
    SWEEP_OUT="${out}/r2" SWEEP_FINDINGS=/out/qa2/findings bash /scripts/qa/sweep.sh own $(r2_core | own_or_other /out/qa2/findings own)
    ;;
r3)
    # flows-04 runs as round F's Chromium-pinned copy (f-core hand-over: the
    # stock script on Firefox ends its flows ok and says NOT-REPRODUCED for
    # the wrong reason; that disparity is F235).
    SWEEP_OUT="${out}/r3" SWEEP_FINDINGS=/out/qa3/findings bash /scripts/qa/sweep.sh fresh $(r3_core | own_or_other /out/qa3/findings other | grep -vx flows-04)
    SWEEP_OUT="${out}/r3" SWEEP_FINDINGS=/out/qa3/findings bash /scripts/qa/sweep.sh alt flows-04 /out/qa3/support/alt/flows-04.sh
    SWEEP_OUT="${out}/r3" SWEEP_FINDINGS=/out/qa3/findings bash /scripts/qa/sweep.sh own $(r3_core | own_or_other /out/qa3/findings own)
    ;;
*)
    echo "unknown part ${part}" >&2
    exit 2
    ;;
esac
