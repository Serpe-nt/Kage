#!/usr/bin/env bash
# Round three: do the browsers' background names (google) leave as DNS under
# the pool, or only under `direct`? Counts the dropped lookups in the NFLOG
# capture before and after level 2 and 3 fetches on a pool stack and on a
# direct stack, nothing else running.
set -uo pipefail
H=/scripts/qa/stack.sh
cap="$(ls -t /out/qa3/fw-drops-*.pcap | head -n 1)"
count() { tcpdump -nn -r "${cap}" 2>/dev/null | grep -c -E 'google|gstatic|mozilla|firefox' ; }
run_fetches() {
    local stack="$1" base t e
    base="$(bash $H url "${stack}")"
    t="$(bash $H key "${stack}" "qa3-dns-$RANDOM" fetch,jobs '"requests_per_minute":600')"
    for e in chromium firefox chromium firefox; do
        for lvl in 2 3; do
            curl -s --max-time 180 -H "Authorization: Bearer ${t}" -H 'Content-Type: application/json' \
                --data "{\"url\":\"http://fixture.kage.test:8088/?dns=$RANDOM\",\"level\":${lvl},\"persona\":{\"engines\":[\"${e}\"]}}" \
                "${base}/v1/fetch" | jq -c "{engine: \"${e}\", level: ${lvl}, outcome}"
        done
    done
    sleep 60
}
for mode in pool direct; do
    stack="dns-${mode}"
    if [ "${mode}" = direct ]; then
        bash $H up "${stack}" KAGE_E2E_EGRESS_MODE=direct > "/out/qa3/dns-${mode}-up.log" 2>&1
    else
        bash $H up "${stack}" > "/out/qa3/dns-${mode}-up.log" 2>&1
    fi
    bash $H fast "${stack}" > /dev/null 2>&1
    before="$(count)"; t0="$(date -u +%T)"
    run_fetches "${stack}"
    after="$(count)"
    echo "${mode}: background-name lookups dropped from ${t0} to $(date -u +%T): $((after - before))"
    bash $H down "${stack}" > /dev/null 2>&1
done
