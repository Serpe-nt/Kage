#!/usr/bin/env bash
# udpwatch.sh STACK start|stop: tcpdump of every UDP packet on the stack's
# browsers bridge (the sidecars' only network) and of non-DNS UDP on its
# default bridge, to see a browser send UDP around the proxy. Runs in the
# container kage-qa2, where the nested Docker's bridges are visible.
set -uo pipefail
stack="$1"; what="$2"
dir="/out/qa/logs/udpwatch-${stack}"
mkdir -p "${dir}"
bridge() { echo "br-$(docker network inspect -f '{{.Id}}' "qa-${stack}_$1" | cut -c1-12)"; }
case "${what}" in
start)
    for net in browsers default pubnet; do
        b="$(bridge "${net}" 2> /dev/null)" || continue
        nohup tcpdump -i "${b}" -n -l -U udp > "${dir}/${net}.txt" 2>&1 &
        echo $! >> "${dir}/pids"
    done
    sleep 1
    echo "watching"
    ;;
stop)
    sleep 1
    [ -f "${dir}/pids" ] && xargs -r kill < "${dir}/pids" 2> /dev/null
    rm -f "${dir}/pids"
    for f in "${dir}"/*.txt; do
        echo "== $(basename "${f}"): $(grep -c ' UDP\|\.53: \|> ' "${f}") lines"
        grep -v 'listening on\|packets captured\|packets received\|packets dropped' "${f}" | grep -v '\.53: \|\.53 >' | head -n 20
    done
    ;;
esac
