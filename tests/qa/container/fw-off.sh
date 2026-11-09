#!/usr/bin/env bash
# Lifts the firewall of fw-on.sh (for a build only, with no stack running)
# and stops its capture. Matches tcpdump by its exact name so that it never
# kills the shell that runs it. QA_FW_DIR and QA_FW_TAG as for fw-on.sh.
set -uo pipefail
dir="${QA_FW_DIR:-/out/qa3}"
tag="${QA_FW_TAG:-QA3}"
running="$(docker ps --format '{{.Names}}' | wc -l)"
if [ "${running}" != "0" ]; then
    echo "refusing: ${running} containers running"
    docker ps --format '{{.Names}}'
    exit 1
fi
iptables -L DOCKER-USER -v -n
iptables -L OUTPUT -v -n
while iptables -D DOCKER-USER -o eth0 -j DROP 2> /dev/null; do :; done
while iptables -D DOCKER-USER -o eth0 -j NFLOG --nflog-group 7 --nflog-prefix "${tag}-DU" 2> /dev/null; do :; done
while iptables -D OUTPUT -o eth0 -j DROP 2> /dev/null; do :; done
while iptables -D OUTPUT -o eth0 -j NFLOG --nflog-group 7 --nflog-prefix "${tag}-OUT" 2> /dev/null; do :; done
pkill -x tcpdump || true
rm -f "${dir}/FW-ON"
date -u
echo "firewall off"
