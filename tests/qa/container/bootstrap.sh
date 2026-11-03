#!/usr/bin/env bash
# Prepares the verification container: the non-root user the browsers run
# as, ownership of the volumes, and the nested Docker daemon.
set -uo pipefail
id dev > /dev/null 2>&1 || useradd --create-home --shell /bin/bash --uid 1001 dev
mkdir -p /target/dev /target/dl /out
chown dev:dev /home/dev /target /target/dev /target/dl
chmod 777 /out
# The cargo home is root's in the image; the registry and git volumes are
# written by the user that builds.
chown -R dev:dev /opt/cargo/registry /opt/cargo/git 2> /dev/null
chmod -R a+rwX /opt/cargo 2> /dev/null
echo "dev ALL=(ALL) NOPASSWD:ALL" > /etc/sudoers.d/dev; chmod 440 /etc/sudoers.d/dev
echo "dev: $(id dev)"
bash /scripts/fix-dockerd.sh
chmod 666 /var/run/docker.sock 2> /dev/null
