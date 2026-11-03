#!/usr/bin/env bash
# The image is a snapshot of a running container: it carries the runtime
# state of that container's Docker daemon (pid files, sockets, network
# namespaces). Nothing of it is alive here; the daemon refuses to start
# while the stale containerd pid file names a live pid. Clears the runtime
# directories (never /var/lib/docker, which is the data volume) and starts
# the daemon.
set -uo pipefail
pkill dockerd 2> /dev/null
pkill containerd 2> /dev/null
sleep 1
rm -rf /var/run/docker /run/containerd /var/run/docker.pid /var/run/docker.sock
bash /scripts/start-dockerd.sh
chmod 666 /var/run/docker.sock 2> /dev/null
docker ps --format '{{.Names}} {{.Status}}' | head
