#!/usr/bin/env bash
# Round three's regression sweep: round one's 111 and round two's 33 repro.sh
# as filed, through sweep.sh. Usage: sweep3.sh r1-shared | r1-own | r2-fresh | r2-own
# A repro whose second line says `# own-stack` brings its own stack; round
# one's others share one stack, round two's others each get a fresh one (as
# round two verified them).
set -uo pipefail
part="${1:?part}"
lane_ids() {
    local dir="$1" want="$2" d id
    for d in "${dir}"/*/; do
        id="$(basename "${d}")"
        [ -f "${d}/repro.sh" ] || continue
        if sed -n 2p "${d}/repro.sh" | grep -q own-stack; then
            [ "${want}" = own ] && echo "${id}"
        else
            [ "${want}" = other ] && echo "${id}"
        fi
    done
}
case "${part}" in
r1-shared)
    SWEEP_OUT=/out/qa3/sweep/r1 SWEEP_FINDINGS=/out/qa1/findings bash /scripts/qa/sweep.sh shared $(lane_ids /out/qa1/findings other)
    ;;
r1-own)
    SWEEP_OUT=/out/qa3/sweep/r1 SWEEP_FINDINGS=/out/qa1/findings bash /scripts/qa/sweep.sh own $(lane_ids /out/qa1/findings own)
    ;;
r1-alt)
    SWEEP_OUT=/out/qa3/sweep/r1 SWEEP_FINDINGS=/out/qa1/findings bash /scripts/qa/sweep.sh alt secrecy-01 /out/qa1/support/alt/secrecy-01.sh
    ;;
r2-fresh)
    SWEEP_OUT=/out/qa3/sweep/r2 SWEEP_FINDINGS=/out/qa2/findings bash /scripts/qa/sweep.sh fresh $(lane_ids /out/qa2/findings other)
    ;;
r2-own)
    SWEEP_OUT=/out/qa3/sweep/r2 SWEEP_FINDINGS=/out/qa2/findings bash /scripts/qa/sweep.sh own $(lane_ids /out/qa2/findings own)
    ;;
*)
    echo "unknown part ${part}" >&2
    exit 2
    ;;
esac
