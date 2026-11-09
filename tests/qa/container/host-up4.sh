#!/usr/bin/env bash
# Run on the host (Git Bash, MSYS_NO_PATHCONV=1): creates round four's core
# container from kage-dev:m11 with its named volumes, the worktree read-only
# at /work, SCRATCH/scripts at /scripts (the harness at /scripts/qa) and
# SCRATCH/out at /out. Usage: host-up4.sh WORKTREE SCRATCH [NAME]
set -euo pipefail
work="${1:?worktree}"
scratch="${2:?scratch}"
name="${3:-kage-qa4-core}"
docker run -d --name "${name}" --privileged --init \
    --hostname "${name}" \
    -v "${name}-target:/target" \
    -v "${name}-home:/home/dev" \
    -v "${name}-git:/opt/cargo/git" \
    -v "${name}-registry:/opt/cargo/registry" \
    -v "${name}-docker:/var/lib/docker" \
    -v "${work}:/work:ro" \
    -v "${scratch}/scripts:/scripts" \
    -v "${scratch}/out:/out" \
    kage-dev:m11 sleep infinity
docker exec "${name}" bash /scripts/bootstrap.sh
docker exec "${name}" bash /scripts/start-dockerd.sh
