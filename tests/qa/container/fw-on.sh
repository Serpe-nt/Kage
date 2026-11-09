#!/usr/bin/env bash
# The rounds' firewall: DROP everything the nested Docker and this container
# send out of eth0, with every dropped packet copied to NFLOG group 7 first
# and captured to ${QA_FW_DIR}/fw-drops-N.pcap (the LOG target writes nothing
# from a nested network namespace). QA_FW_DIR (default /out/qa3) and
# QA_FW_TAG (the NFLOG prefix, default QA3) name a round's own capture; round
# four's browsers ran with QA_FW_DIR=/out/qa4b QA_FW_TAG=QA4B. tcpdump must
# be installed first (`apt-get install tcpdump`; kage-dev has none).
set -uo pipefail
dir="${QA_FW_DIR:-/out/qa3}"
tag="${QA_FW_TAG:-QA3}"
command -v tcpdump > /dev/null || { echo "no tcpdump: install it before the rule goes on" >&2; exit 1; }
mkdir -p "${dir}"
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
