#!/bin/sh
# Starts Chrome with a CPU affinity mask, so that the browser itself counts
# the processors of its persona: navigator.hardwareConcurrency is then true
# in the window, in every frame and in every worker, with nothing injected
# (spec §6.2; ADR 0009).
#
#   KAGE_CHROME    the Chrome binary
#   KAGE_CPU_LIST  the CPUs of this persona, as `taskset --cpu-list` reads
#                  them (for example 0-3 or 2,3,6,7); empty for no mask
set -eu
: "${KAGE_CHROME:?KAGE_CHROME names the Chrome binary}"
if [ -n "${KAGE_CPU_LIST:-}" ]; then
    exec taskset --cpu-list "${KAGE_CPU_LIST}" "${KAGE_CHROME}" "$@"
fi
exec "${KAGE_CHROME}" "$@"
