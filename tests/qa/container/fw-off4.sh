#!/usr/bin/env bash
# Lifts round four's firewall (for a build only, with no container
# running) and stops its capture. Usage: fw-off4.sh [ROUND_DIR, default qa4]
set -uo pipefail
round="${1:-qa4}"
tag="$(echo "${round}" | tr '[:lower:]' '[:upper:]')"
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
rm -f "/out/${round}/FW-ON"
date -u
echo "firewall off"
