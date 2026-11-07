#!/usr/bin/env bash
# Starts the nested Docker daemon inside the verification container.
set -uo pipefail

# cgroup v2 nesting: move every process out of the container's root cgroup,
# then delegate every controller, so containers with mem_limit/cpus can start.
if [ -f /sys/fs/cgroup/cgroup.controllers ] && [ -z "$(cat /sys/fs/cgroup/cgroup.subtree_control)" ]; then
    pkill dockerd 2> /dev/null
    pkill containerd 2> /dev/null
    sleep 2
    mkdir -p /sys/fs/cgroup/init
    for _ in 1 2 3 4 5; do
        for pid in $(cat /sys/fs/cgroup/cgroup.procs); do
            echo "${pid}" > /sys/fs/cgroup/init/cgroup.procs 2> /dev/null
        done
        controllers="$(sed -e 's/ / +/g' -e 's/^/+/' /sys/fs/cgroup/cgroup.controllers)"
        if echo "${controllers}" > /sys/fs/cgroup/cgroup.subtree_control 2> /dev/null; then
            break
        fi
        sleep 1
    done
    echo "subtree_control: $(cat /sys/fs/cgroup/cgroup.subtree_control)"
fi

if docker info > /dev/null 2>&1; then
    echo "dockerd already running"
    exit 0
fi

rm -f /var/run/docker.pid
nohup dockerd > /var/log/dockerd.log 2>&1 &

for _ in $(seq 1 60); do
    if docker info > /dev/null 2>&1; then
        echo "dockerd is up: $(docker version --format '{{.Server.Version}}')"
        exit 0
    fi
    sleep 1
done

echo "dockerd did not start" >&2
tail -n 50 /var/log/dockerd.log >&2
exit 1
