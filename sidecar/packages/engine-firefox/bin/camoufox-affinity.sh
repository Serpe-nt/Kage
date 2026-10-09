#!/bin/sh
# Starts the Camoufox of the image under a CPU affinity mask.
#
# The browser counts the processors it may run on; a persona that reports
# eight of them runs on eight, so what a page can time and what it is told
# do not differ more than the host forces. The engine sets both variables;
# without them this is the browser itself.
set -eu
camoufox="${KAGE_CAMOUFOX:-/opt/camoufox/camoufox-bin}"
if [ -n "${KAGE_CPU_LIST:-}" ]; then
    exec taskset --cpu-list "${KAGE_CPU_LIST}" "${camoufox}" "$@"
fi
exec "${camoufox}" "$@"
