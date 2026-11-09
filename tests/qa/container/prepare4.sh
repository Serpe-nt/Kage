#!/usr/bin/env bash
# Round four: clones the commit under test from the bundle at /out/qa.bundle
# into /target/qa/kage (the one fixed path) and builds the three images with
# compose. No cargo build on this side: the CLI is the kage image's own
# (tests/qa/cli/k4.sh), which keeps the cargo target empty. Writes its status
# to /out/prepare.status.
set -uo pipefail
rm -f /out/prepare.status
repo=/target/qa/kage
mkdir -p /target/qa
chown dev:dev /target/qa
git config --global --add safe.directory '*'
if [ ! -d "${repo}/.git" ]; then
    su dev -c "git config --global --add safe.directory '*'; git init -q ${repo} && cd ${repo} && git remote add origin /nonexistent"
fi
su dev -c "cd ${repo} && git fetch -q /out/qa.bundle 'refs/remotes/origin/main:refs/remotes/origin/main' && git checkout -q --detach origin/main && git log --oneline -1" | tee /out/prepare.commit
chmod 666 /var/run/docker.sock
env_setup='export PATH=/opt/cargo/bin:/opt/node/bin:$PATH HOME=/home/dev'
su dev -c "${env_setup}; cd ${repo} && scripts/dev-secrets.sh && docker compose --file deploy/compose.yaml build" > /out/build-images.log 2>&1
images=$?
# The CLI: a shim that runs the image's own kage, where round one's scripts
# and cli/k.sh look for a debug build.
install -D --mode 0755 /scripts/qa/cli/k4.sh /target/devtarget/debug/kage
echo "images=${images} commit=$(cut -d' ' -f1 /out/prepare.commit)" > /out/prepare.status
