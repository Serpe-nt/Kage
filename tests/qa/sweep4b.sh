#!/usr/bin/env bash
# Round four, part B (the browsers): the regression sweep of rounds one to
# three's repro.sh in its area (the browsers and the sidecar, typing,
# extraction and the cache, the crawl and the classifier, the browsers' own
# calls), as filed, through sweep.sh. Part A sweeps the rest.
#   sweep4b.sh r1-shared | r1-own | r2-fresh | r2-own | r3-shared | r3-own
#   sweep4b.sh confirm ID...   a row that reproduced on a shared stack, again
#                              on a fresh stack of its own (round three's way)
# Round one's findings are at /out/qa1/findings, round two's at
# /out/qa2/findings, round three's (report and addendum) at /out/qa3/findings.
# A repro whose second line says `# own-stack` brings its own stack.
# SKIP3: round three ids left out of r3-* (default: what #85 fixes, which was
# not merged at the commit under test).
set -uo pipefail
part="${1:?part}"
shift
R1="crawl-01 crawl-02 crawl-03 crawl-04 crawl-05 crawl-06 crawl-07 crawl-08
    extract-01 extract-02 extract-03 extract-04 extract-05 extract-06 extract-07
    extract-08 extract-09 extract-10 extract-11
    policy-01 policy-02 policy-03 policy-04 policy-05 policy-06 policy-07
    policy-08 policy-09 policy-10 policy-11 api-08 jobs-02"
R2="flows-02 left-02 left-03 tls-02 tls-03 http-03 http-04 http-05 http-06
    http-07 http-08 ssrf-02 ssrf-04"
R3="browser-01 browser-02 browser-03 browser-04 browser-05 browser-06 browser-07
    extract-01 extract-02 extract-03 extract-04 extract-05 extract-06 extract-07
    extract-08 crawl-01 crawl-02 crawl-03 crawl-04 class-01 class-02
    bgcalls-01 bgcalls-02"
SKIP3="${SKIP3-browser-01 browser-02 browser-03 browser-04 browser-05 class-02 crawl-04 extract-04 extract-06}"
skip3() { # IDS... -> those not in SKIP3
    local id
    for id in "$@"; do
        case " ${SKIP3} " in *" ${id} "*) ;; *) echo "${id}" ;; esac
    done
}
pick() { # DIR WANT(own|other) IDS... -> the ids of that lane
    local dir="$1" want="$2" id
    shift 2
    for id in "$@"; do
        [ -f "${dir}/${id}/repro.sh" ] || { echo "missing ${dir}/${id}/repro.sh" >&2; continue; }
        if sed -n 2p "${dir}/${id}/repro.sh" | grep -q own-stack; then
            [ "${want}" = own ] && echo "${id}"
        else
            [ "${want}" = other ] && echo "${id}"
        fi
    done
}
lane() { # ROUND DIR LANE WANT IDS...
    local round="$1" dir="$2" sweep_lane="$3" want="$4"
    shift 4
    # shellcheck disable=SC2046
    SWEEP_OUT="/out/qa4b/sweep/${round}" SWEEP_FINDINGS="${dir}" \
        bash /scripts/qa/sweep.sh "${sweep_lane}" $(pick "${dir}" "${want}" "$@")
}
# shellcheck disable=SC2086
case "${part}" in
r1-shared) lane r1 /out/qa1/findings shared other ${R1} ;;
r1-own) lane r1 /out/qa1/findings own own ${R1} ;;
r2-fresh) lane r2 /out/qa2/findings fresh other ${R2} ;;
r2-own) lane r2 /out/qa2/findings own own ${R2} ;;
r3-shared) lane r3 /out/qa3/findings shared other $(skip3 ${R3}) ;;
r3-own) lane r3 /out/qa3/findings own own $(skip3 ${R3}) ;;
confirm)
    for id in "$@"; do
        round="${id%%:*}"
        name="${id#*:}"
        SWEEP_OUT="/out/qa4b/sweep/${round}" SWEEP_FINDINGS="/out/qa${round#r}/findings" \
            bash /scripts/qa/sweep.sh fresh "${name}"
    done
    ;;
*)
    echo "unknown part ${part}" >&2
    exit 2
    ;;
esac
