#!/usr/bin/env bash
# Round four's firewall (round three's fw-on.sh with the round in a
# variable): DROP everything the nested Docker and this container send out
# of eth0, every dropped packet copied to NFLOG group 7 first and captured to
# /out/<round>/fw-drops-N.pcap (the LOG target writes nothing from a nested
# network namespace). Usage: fw-on4.sh [ROUND_DIR, default qa4]
set -uo pipefail
round="${1:-qa4}"
dir="/out/${round}"
mkdir -p "${dir}"
tag="$(echo "${round}" | tr '[:lower:]' '[:upper:]')"
if ! iptables -C DOCKER-USER -o eth0 -j DROP 2> /dev/null; then
    iptables -I DOCKER-USER -o eth0 -j DROP
    iptables -I DOCKER-USER -o eth0 -j NFLOG --nflog-group 7 --nflog-prefix "${tag}-DU"
fi
if ! iptables -C OUTPUT -o eth0 -j DROP 2> /dev/null; then
    iptables -I OUTPUT -o eth0 -j DROP
    iptables -I OUTPUT -o eth0 -j NFLOG --nflog-group 7 --nflog-prefix "${tag}-OUT"
fi
if ! pgrep -x tcpdump > /dev/null; then
    n=1
    while [ -e "${dir}/fw-drops-${n}.pcap" ]; do n=$((n + 1)); done
    nohup tcpdump -Z root -i nflog:7 -U -w "${dir}/fw-drops-${n}.pcap" > "${dir}/fw-drops-${n}.log" 2>&1 &
    sleep 1
    echo "capture: ${dir}/fw-drops-${n}.pcap"
fi
date -u > "${dir}/FW-ON"
iptables -L DOCKER-USER -v -n
iptables -L OUTPUT -v -n
