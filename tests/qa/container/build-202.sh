#!/usr/bin/env bash
# The 2.0.2 tree beside main, and its Firefox sidecar image (never published,
# ADR 0008), for the upgrade path: kage and Chromium 2.0.2 come from ghcr.
set -uo pipefail
repo=/target/qa/kage
old=/target/qa/kage-202
if [ ! -d "${old}" ]; then
    su dev -c "cd ${repo} && git worktree add -q --detach ${old} v2.0.2 2>&1 || git worktree add -q --detach ${old} 2e9cb40"
fi
su dev -c "cd ${old} && git log --oneline -1"
env_setup='export PATH=/opt/cargo/bin:/opt/node/bin:$PATH HOME=/home/dev'
su dev -c "${env_setup}; cd ${old} && cp -n ${repo}/deploy/.env deploy/.env 2>/dev/null; cp -rn ${repo}/deploy/secrets deploy/ 2>/dev/null; KAGE_BROWSER_FIREFOX_IMAGE=kage-browser-firefox:2.0.2 docker compose --file deploy/compose.yaml build browser-firefox" > /out/build-202.log 2>&1
echo "firefox-202=$?" > /out/build-202.status
docker images --format '{{.Repository}}:{{.Tag}}' | grep firefox
