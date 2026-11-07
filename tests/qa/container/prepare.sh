#!/usr/bin/env bash
# Clones the commit under test from the bundle into a volume, builds the
# three images (compose) and the host-side CLI, installs the node projects.
# Writes its status to /out/prepare.status.
set -uo pipefail
rm -f /out/prepare.status
repo=/target/qa/kage
mkdir -p /target/qa
chown dev:dev /target/qa
if [ ! -d "${repo}/.git" ]; then
    git config --global --add safe.directory '*'
    su dev -c "git config --global --add safe.directory '*'; git init -q ${repo} && cd ${repo} && git remote add origin /nonexistent && git fetch -q /out/qa.bundle 'refs/remotes/origin/main:refs/remotes/origin/main' 'HEAD:refs/heads/qa' && git checkout -q qa && git log --oneline -1"
fi
chmod 666 /var/run/docker.sock
env_setup='export PATH=/opt/cargo/bin:/opt/node/bin:/usr/local/go/bin:$PATH RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo CARGO_TARGET_DIR=/target/devtarget SQLX_OFFLINE=true CI=true HOME=/home/dev'
# The images.
su dev -c "${env_setup}; cd ${repo} && scripts/dev-secrets.sh && docker compose --file deploy/compose.yaml build" > /out/build-images.log 2>&1
images=$?
# The CLI on this side (for just openapi and config checks), and the node projects.
su dev -c "${env_setup}; cd ${repo} && cargo build --package kage --locked" > /out/build-cli.log 2>&1
cli=$?
su dev -c "${env_setup}; cd ${repo}/sidecar && pnpm install --frozen-lockfile && pnpm --filter @kage/contract run build; cd ${repo}/tests/api-client && pnpm install --frozen-lockfile && pnpm run build" > /out/build-node.log 2>&1
node=$?
echo "images=${images} cli=${cli} node=${node}" > /out/prepare.status
