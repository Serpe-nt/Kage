#!/usr/bin/env bash
# The policies an end-to-end world starts from: the shipped files, copied
# into the directory given, with one change. The defaults' `active_hours`
# is `ignore` instead of `prefer`, so that no persona of a world sleeps.
#
# The worlds' personas are Germans (their proxies are DE): under `prefer`
# a persona is awake from 06:30 to 08:30 until 22:30 to 00:30 Berlin time
# (spec §11, "A persona's day"), a job never wakes one, and a job or a flow
# started at night waits for the morning (tests/e2e/guard.sh's flow was
# `queued` until 05:56 UTC on a run at 01:33 UTC). The suite has to pass at
# any hour, and what it tests is not the hour: `ignore` is the policy that
# says so (spec §11: "`ignore` ignores the hour"), and the planner's
# handling of the hours is tested under a clock of its own one layer down
# (crates/kage-core/src/schedule/planner/tests.rs, schedule/day.rs).
#
# Usage: tests/e2e/policies.sh <directory>
set -euo pipefail
cd "$(dirname "$0")/../.."

dir="${1:?the directory to write the policies into}"
cp policies/*.yaml "${dir}/"
file="${dir}/domains.yaml"
[ "$(grep --count '^  active_hours: prefer$' "${file}")" = 1 ] \
    || { echo "FAIL: policies/domains.yaml has no one defaults' active_hours: prefer to change" >&2; exit 1; }
sed --in-place 's/^  active_hours: prefer$/  active_hours: ignore/' "${file}"
