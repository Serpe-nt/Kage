#!/usr/bin/env bash
# Moves the clone to a newer main from a bundle and rebuilds the images.
# The firewall must be off for the build (packages are fetched) and on again
# after: run fw-off first, fw-on.sh after. Usage: rebuild-sidecars.sh BUNDLE [SERVICES...]
set -uo pipefail
bundle="${1:?bundle}"
shift
repo=/target/qa/kage
su dev -c "cd ${repo} && git fetch -q ${bundle} 'refs/remotes/origin/main:refs/remotes/origin/main' && git checkout -q --detach origin/main && git log --oneline -1"
env_setup='export PATH=/opt/cargo/bin:/opt/node/bin:$PATH HOME=/home/dev'
su dev -c "${env_setup}; cd ${repo} && docker compose --file deploy/compose.yaml build $*" > /out/rebuild.log 2>&1
echo "rebuild=$?" | tee /out/rebuild.status
