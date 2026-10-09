#!/usr/bin/env bash
# Installs what the sidecar's integration tests need on a Debian or Ubuntu
# machine (a CI runner, WSL2, a devcontainer): the pinned Chrome, Xvfb, Mesa's
# software OpenGL, and certutil for the browser capture.
#
# The Chrome version and the checksum of Google's package are read from
# deploy/docker/sidecar.Dockerfile, so tests and image run the same build.
# A machine that already has that version is left alone.
set -euo pipefail
cd "$(dirname "$0")/.."

dockerfile="deploy/docker/sidecar.Dockerfile"
version="$(sed -n 's/^ARG CHROME_VERSION=//p' "${dockerfile}")"
checksum="$(sed -n 's/^ARG CHROME_SHA256=//p' "${dockerfile}")"
if [ -z "${version}" ] || [ -z "${checksum}" ]; then
    echo "error: ${dockerfile} does not name CHROME_VERSION and CHROME_SHA256" >&2
    exit 1
fi

chrome="/opt/google/chrome/chrome"
installed=""
if [ -x "${chrome}" ]; then
    installed="$("${chrome}" --version | sed -E 's/^[^0-9]*([0-9.]+).*$/\1/')"
fi
if [ "${installed}" = "${version}" ] && command -v Xvfb > /dev/null && command -v certutil > /dev/null; then
    echo "Chrome ${version}, Xvfb and certutil are already installed"
    exit 0
fi

if [ "$(uname -s)" != "Linux" ] || ! command -v apt-get > /dev/null; then
    echo "error: the sidecar's browser tests need Linux with apt (a CI runner, WSL2 or a devcontainer)" >&2
    exit 1
fi
sudo=""
if [ "$(id -u)" -ne 0 ]; then
    sudo="sudo"
fi

deb="$(mktemp --suffix=.deb)"
trap 'rm -f "${deb}"' EXIT
echo "==> downloading Chrome ${version}"
curl --fail --silent --show-error --location --retry 3 --output "${deb}" \
    "https://dl.google.com/linux/chrome/deb/pool/main/g/google-chrome-stable/google-chrome-stable_${version}-1_amd64.deb"
echo "${checksum}  ${deb}" | sha256sum --check --strict

echo "==> installing Chrome ${version}, Xvfb, Mesa and certutil"
${sudo} apt-get update --quiet
${sudo} env DEBIAN_FRONTEND=noninteractive apt-get install --yes --no-install-recommends --allow-downgrades \
    "${deb}" xvfb libgl1-mesa-dri libegl1 libnss3-tools fonts-liberation
# The package adds Google's apt source so that the browser updates itself
# with the system. The pin is the point: a bump is a deliberate change.
${sudo} rm -f /etc/apt/sources.list.d/google-chrome.list /etc/cron.daily/google-chrome

"${chrome}" --version
