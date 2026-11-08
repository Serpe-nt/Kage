#!/usr/bin/env bash
# Lifts round three's firewall (for a build only, with no stack running) and
# stops its capture. Matches tcpdump by its exact name so that it never kills
# the shell that runs it.
set -uo pipefail
running="$(docker ps --format '{{.Names}}' | wc -l)"
if [ "${running}" != "0" ]; then
    echo "refusing: ${running} containers running"
    docker ps --format '{{.Names}}'
    exit 1
fi
iptables -L DOCKER-USER -v -n
iptables -L OUTPUT -v -n
while iptables -D DOCKER-USER -o eth0 -j DROP 2> /dev/null; do :; done
while iptables -D DOCKER-USER -o eth0 -j NFLOG --nflog-group 7 --nflog-prefix QA3-DU 2> /dev/null; do :; done
while iptables -D OUTPUT -o eth0 -j DROP 2> /dev/null; do :; done
while iptables -D OUTPUT -o eth0 -j NFLOG --nflog-group 7 --nflog-prefix QA3-OUT 2> /dev/null; do :; done
pkill -x tcpdump || true
rm -f /out/qa3/FW-ON
date -u
echo "firewall off"
