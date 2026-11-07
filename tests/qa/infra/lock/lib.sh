#!/usr/bin/env bash
# QA round three, lock: helpers for the single-server lock (F174).
#   source /scripts/qa/infra/lock/lib.sh NAME
# Gives: S (stack.sh), NAME, LOCK_KEY, lock_holder (pid, client addr, app),
# kage_cid (container id of the stack's kage), kage_pid, pg_ip,
# cut_on / cut_off (drop every packet between kage and postgres in kage's
# network namespace: a partition that keeps every address), ts (UTC time).
S=/scripts/qa/stack.sh
NAME="${1:?a stack name}"
LOCK_KEY=7737579320224608886   # 0x6b61676573657276, 'kageserv'
ts() { date -u +%H:%M:%S.%3N; }
sqlq() { bash $S sql "${NAME}" "$1"; }
lock_holder() {
    sqlq "SELECT l.pid || ' ' || coalesce(host(a.client_addr),'local') || ' ' || coalesce(a.application_name,'') FROM pg_locks l JOIN pg_stat_activity a USING (pid) WHERE l.locktype='advisory' AND l.objsubid=1 AND l.granted AND ((l.classid::bigint << 32) | l.objid::bigint) = ${LOCK_KEY}"
}
kage_cid() { bash $S dc "${NAME}" ps -q "${2:-kage}"; }
kage_pid() { docker inspect -f '{{.State.Pid}}' "$(kage_cid "$@")"; }
pg_ip() {
    local c; c="$(bash $S dc "${NAME}" ps -q postgres)"
    docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}} {{end}}' "$c" | awk '{print $1}'
}
cut_on() {   # [service]
    local pid ip; pid="$(kage_pid "${NAME}" "${1:-kage}")"; ip="$(pg_ip)"
    nsenter -t "$pid" -n iptables -I OUTPUT -d "$ip" -j DROP
    nsenter -t "$pid" -n iptables -I INPUT -s "$ip" -j DROP
}
cut_off() {  # [service]
    local pid ip; pid="$(kage_pid "${NAME}" "${1:-kage}")"; ip="$(pg_ip)"
    nsenter -t "$pid" -n iptables -D OUTPUT -d "$ip" -j DROP
    nsenter -t "$pid" -n iptables -D INPUT -s "$ip" -j DROP
}
klog() { bash $S dc "${NAME}" logs --no-color --since "${2:-10m}" "${1:-kage}" 2>&1; }
