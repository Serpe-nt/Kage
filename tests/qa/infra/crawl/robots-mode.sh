#!/usr/bin/env bash
# Sets `robots: MODE` on one world name's rule in a stack's domains.yaml
# (the stack's own copy under /target/qa/runs/NAME/policies, never the
# repository) and reloads the policies. The rule must exist (`stack.sh fast`).
# usage: robots-mode.sh STACK HOST MODE   (MODE: enforce, report, ignore)
set -euo pipefail
stack="$1" host="$2" mode="$3"
file="/target/qa/runs/${stack}/policies/domains.yaml"
# Drop a robots line this script set before, then add the new one under the host.
awk -v host="  ${host}:" -v mode="${mode}" '
  $0 == host { print; print "    robots: " mode; skip = 1; next }
  skip && /^    robots: / { next }
  /^  [^ ]/ { skip = 0 }
  { print }
' "${file}" > "${file}.tmp"
cat "${file}.tmp" > "${file}"
rm -f "${file}.tmp"
token="$(cat "/target/qa/runs/${stack}/admin.token")"
QA_MAX_TIME=30 bash /scripts/qa/stack.sh call "${stack}" POST /v1/admin/policies/reload "${token}" | head -n 1
