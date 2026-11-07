#!/usr/bin/env bash
# Hosts the forwarder relayed (trace "relaying"/"relaying with no upstream")
# and refused, outside the world's names, in kage's log of a stack.
#   hosts.sh NAME [SINCE (docker logs --since, e.g. 2026-11-07T22:00:00)] [--all]
# Prints: time, message, caller, host, port (tab separated).
# Needs the stack started with RUST_LOG=info,kage_scheduler::forwarder=trace.
set -uo pipefail
name="$1"; since="${2:-}"; all="${3:-}"
stack=/scripts/qa/stack.sh
args=(logs --no-color --no-log-prefix)
[ -n "${since}" ] && args+=(--since "${since}")
bash "${stack}" dc "${name}" "${args[@]}" kage 2> /dev/null \
  | grep -E '"relaying|refused' \
  | jq -rc '
      (.fields.reason // "") as $r
      | (if .fields.host != null then .fields.host
         else ($r | capture("Address\\(\"(?<h>[^ ]+) ").h? // null) end) as $h
      | select($h != null)
      | [.timestamp[0:19],
         (if .fields.host != null then .fields.message else "forwarder refused: " + ($r | sub("^Address\\(\"[^ ]+ "; "") | .[0:60]) end),
         (.fields.caller // "-"), $h, ((.fields.port // "") | tostring)] | @tsv' 2> /dev/null \
  | { if [ "${all}" = --all ]; then cat; else grep -v -E $'\t[^\t]*\\.(test|invalid)\t|\t(postgres|hooks|world|fixture|kage)\t'; fi; }
