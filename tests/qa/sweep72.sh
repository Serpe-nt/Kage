#!/usr/bin/env bash
# Round three's addendum: round one's crawl, policy (classifier) and #72's
# other scripts (api-08, extract-07, jobs-02) on main after #72.
# Usage: sweep72.sh shared | own
set -uo pipefail
part="${1:?part}"
ids="api-08 crawl-01 crawl-02 crawl-03 crawl-04 crawl-05 crawl-06 crawl-07 crawl-08 extract-07 jobs-02
policy-01 policy-02 policy-03 policy-04 policy-05 policy-06 policy-07 policy-08 policy-09 policy-10 policy-11"
pick() {
    local want="$1" id
    for id in ${ids}; do
        if sed -n 2p "/out/qa1/findings/${id}/repro.sh" | grep -q own-stack; then
            [ "${want}" = own ] && echo "${id}"
        else
            [ "${want}" = other ] && echo "${id}"
        fi
    done
}
export SWEEP_OUT="${SWEEP_OUT:-/out/qa3/sweep-72}" SWEEP_FINDINGS=/out/qa1/findings
case "${part}" in
shared) bash /scripts/qa/sweep.sh shared $(pick other) ;;
own) bash /scripts/qa/sweep.sh own $(pick own) ;;
*)
    echo "unknown part ${part}" >&2
    exit 2
    ;;
esac
