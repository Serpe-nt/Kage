#!/usr/bin/env bash
# One measurement: capture DNS and outbound SYNs in kage's and the sidecar's
# network namespaces, optionally fetch a page, wait, then list what the
# forwarder relayed (hosts.sh) and what was looked up / connected to.
#   exp.sh NAME ENGINE LEVEL WAIT_SECONDS [URL|none]
set -uo pipefail
name="$1"; engine="$2"; level="$3"; wait_s="$4"; url="${5:-}"
s=/scripts/qa/stack.sh
here=/scripts/qa/infra/bgcalls
out=/out/qa3/bgcalls; mkdir -p "${out}"
tag="${name}-${engine}-L${level}-$(date -u +%H%M%S)"
t0="$(date -u +%Y-%m-%dT%H:%M:%S)"
kpid="$(docker inspect -f '{{.State.Pid}}' "qa-${name}-kage-1")"
spid="$(docker inspect -f '{{.State.Pid}}' "qa-${name}-browser-${engine}-1")"
filter='udp or (tcp[tcpflags] & tcp-syn != 0 and tcp[tcpflags] & tcp-ack == 0)'
nsenter -t "${kpid}" -n tcpdump -i any -nn -U -w "${out}/${tag}-kage.pcap" "${filter}" > /dev/null 2>&1 &
p1=$!
nsenter -t "${spid}" -n tcpdump -i any -nn -U -w "${out}/${tag}-sidecar.pcap" "${filter}" > /dev/null 2>&1 &
p2=$!
sleep 1
if [ -n "${url}" ] && [ "${url}" != none ]; then
    tok="$(bash $s key "${name}" "qa3-bg-${tag}" fetch,jobs)"
    body="$(jq -nc --arg u "${url}" --argjson l "${level}" --arg e "${engine}" '{url:$u, level:$l, timeout_ms:90000, persona:{engines:[$e]}}')"
    r="$(bash $s call "${name}" POST /v1/fetch "${tok}" "${body}")"
    echo "fetch: $(printf '%s' "${r}" | head -n1) outcome=$(printf '%s' "${r}" | sed '1,/^\r\{0,1\}$/d' | jq -r '.outcome // .code' 2> /dev/null) at $(date -u +%T)"
fi
sleep "${wait_s}"
kill "${p1}" "${p2}" 2> /dev/null; sleep 1
echo "== window ${t0} .. $(date -u +%T) (${tag})"
echo "== forwarder relayed/refused, outside the world's names: count host port"
bash "${here}/hosts.sh" "${name}" "${t0}" | awk -F'\t' '{print $3"\t"$4"\t"$5"\t"$2}' | sort | uniq -c
echo "== forwarder lines by time (first 40)"
bash "${here}/hosts.sh" "${name}" "${t0}" | head -n 40
echo "== DNS questions seen in kage's namespace (non-world names)"
tcpdump -nn -r "${out}/${tag}-kage.pcap" 2> /dev/null | grep -oE '(A|AAAA)\? [^ ]+' | grep -v -E '\.(test|invalid)\.$|kage-|qa-' | sort | uniq -c
echo "== SYNs from kage's namespace to public addresses"
tcpdump -nn -r "${out}/${tag}-kage.pcap" 'tcp and not dst net 10.0.0.0/8 and not dst net 172.16.0.0/12 and not dst net 192.168.0.0/16 and not dst net 127.0.0.0/8' 2> /dev/null | awk '{print $5}' | sed 's/:$//' | sort | uniq -c
echo "== SYNs/UDP from the sidecar's namespace (destinations)"
tcpdump -nn -r "${out}/${tag}-sidecar.pcap" 2> /dev/null | awk '{print $2, $3, $5}' | awk '{print $1, $3}' | sort | uniq -c | sort -rn | head
echo "tag ${tag}"
