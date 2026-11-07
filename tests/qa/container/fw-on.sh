#!/usr/bin/env bash
# Round three's firewall: DROP everything the nested Docker and this container
# send out of eth0, with every dropped packet copied to NFLOG group 7 first
# and captured to /out/qa3/fw-drops-N.pcap (the LOG target writes nothing
# from a nested network namespace).
set -uo pipefail
mkdir -p /out/qa3
if ! iptables -C DOCKER-USER -o eth0 -j DROP 2> /dev/null; then
    iptables -I DOCKER-USER -o eth0 -j DROP
    iptables -I DOCKER-USER -o eth0 -j NFLOG --nflog-group 7 --nflog-prefix QA3-DU
fi
if ! iptables -C OUTPUT -o eth0 -j DROP 2> /dev/null; then
    iptables -I OUTPUT -o eth0 -j DROP
    iptables -I OUTPUT -o eth0 -j NFLOG --nflog-group 7 --nflog-prefix QA3-OUT
fi
if ! pgrep -f 'tcpdump.*nflog:7' > /dev/null; then
    n=1
    while [ -e "/out/qa3/fw-drops-${n}.pcap" ]; do n=$((n + 1)); done
    nohup tcpdump -Z root -i nflog:7 -U -w "/out/qa3/fw-drops-${n}.pcap" > "/out/qa3/fw-drops-${n}.log" 2>&1 &
    sleep 1
    echo "capture: /out/qa3/fw-drops-${n}.pcap"
fi
iptables -L DOCKER-USER -v -n
iptables -L OUTPUT -v -n
